//! 系统托盘菜单。
//!
//! 使用 Tauri 2 内置的 TrayIconBuilder(不再依赖 tauri-plugin-system-tray,
//! 该插件在 Tauri 2 已移除)。
//!
//! 菜单项:
//! - 打开主窗口 / 隐藏主窗口
//! - 重启后端
//! - 检查更新…
//! - 退出
//!
//! # 为什么「检查更新」放在托盘而不只放界面上
//!
//! 后端就绪后启动页会被 DeepTutor Web UI 整个顶掉
//! (`window.location.replace(WEB_URL)`),启动页上那个「检查更新」按钮
//! 随之消失 —— 正常使用软件的路径上根本点不到它。
//! 托盘菜单在任何时候都在,是唯一可靠的入口。
//!
//! # 关窗 = 隐藏到托盘
//!
//! 见 lib.rs 的 `on_window_event`。所以这里的「退出」是唯一真正
//! 结束应用的路径,必须显式清理后端进程树(窗口隐藏时后端一直在跑)。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Listener, Manager};

use crate::backend::runner::Runner;

/// 托盘图标 id。更新进度要靠它反查托盘对象改 tooltip。
pub const TRAY_ID: &str = "main-tray";

/// 默认 tooltip(无更新任务时显示)。
const TOOLTIP_IDLE: &str = "DeepTutor";

/// 更新流程防重入。
///
/// 托盘菜单没有可靠的 disable 机制(拿不到菜单项的强类型句柄),
/// 用原子标志挡住重复点击 —— 否则连点两下会弹两个确认框、起两个下载任务。
static UPDATE_FLOW_RUNNING: AtomicBool = AtomicBool::new(false);

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let menu = Menu::with_items(
        app,
        &[
            &PredefinedMenuItem::about(app, Some("关于 DeepTutor"), None)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "toggle", "打开/隐藏主窗口", true, None::<&str>)?,
            &MenuItem::with_id(app, "restart", "重启后端", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "update", "检查更新…", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            // 不用 PredefinedMenuItem::quit:它直接退出,不给我们清理后端的机会。
            // 自定义项可以显式杀进程树再退。
            &MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?,
        ],
    )?;

    let _tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(app.default_window_icon().cloned().unwrap())
        .tooltip(TOOLTIP_IDLE)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "toggle" => {
                if let Some(win) = app.get_webview_window("main") {
                    if win.is_visible().unwrap_or(false) {
                        let _ = win.hide();
                    } else {
                        let _ = win.show();
                        let _ = win.set_focus();
                    }
                }
            }
            "restart" => {
                if let Some(runner) = app.try_state::<Arc<Runner>>() {
                    let runner = runner.inner().clone();
                    let handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let _ = runner.stop().await;
                        crate::backend::boot::run(handle, runner).await;
                    });
                }
            }
            "update" => {
                // 菜单回调跑在 UI 线程上,而更新流程要走网络、弹模态框,
                // 必须挪到后台任务,否则会卡住托盘与窗口的消息循环。
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    run_update_flow(handle).await;
                });
            }
            "quit" => {
                // 关窗只隐藏,后端一直活着 —— 真退出必须显式清理,
                // 否则会留下占着 8001/3782 的孤儿 python 进程。
                if let Some(runner) = app.try_state::<Arc<Runner>>() {
                    runner.kill_tree_sync();
                }
                log::info!("从托盘退出,后端进程树已清理");
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 左键单击唤起主窗口(Windows 常见交互)
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                if let Some(win) = tray.app_handle().get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
        })
        .build(app)?;

    // 把下载进度写进 tooltip。
    //
    // 更新可能是从托盘发起的,而那时前端早已跳到 DeepTutor Web UI ——
    // 界面上的进度条它收不到。tooltip 是唯一在所有情况下都可见的反馈面。
    let handle = app.clone();
    app.listen(crate::updater::EVENT_PROGRESS, move |event| {
        let Ok(p) = serde_json::from_str::<crate::updater::Progress>(event.payload()) else {
            return;
        };
        let text = match p.phase {
            crate::updater::Phase::Downloading => match p.percent {
                Some(pct) => format!("DeepTutor — 正在下载更新 {pct:.0}%"),
                None => "DeepTutor — 正在下载更新…".to_string(),
            },
            crate::updater::Phase::Installing => "DeepTutor — 正在安装更新…".to_string(),
        };
        set_tooltip(&handle, &text);
    });

    Ok(())
}

/// 改托盘 tooltip;托盘不存在时静默忽略。
fn set_tooltip(app: &AppHandle, text: &str) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(text));
    }
}

/// 托盘「检查更新…」的完整流程。
async fn run_update_flow(app: AppHandle) {
    // 防重入:已经在跑就直接忽略这次点击。
    if UPDATE_FLOW_RUNNING.swap(true, Ordering::SeqCst) {
        log::info!("已有更新流程在执行,忽略本次点击");
        return;
    }

    set_tooltip(&app, "DeepTutor — 正在检查更新…");

    match crate::updater::check(&app).await {
        Err(e) => {
            log::warn!("检查更新失败: {e}");
            info_dialog(&app, "检查更新失败", &e).await;
        }
        Ok(info) if !info.available => {
            info_dialog(&app, "检查更新", &info.summary()).await;
        }
        Ok(info) => {
            // 把 release notes 带上,但别让它把对话框撑爆。
            let mut msg = format!(
                "{}\n\n是否现在下载并安装?\n\n安装期间 DeepTutor 会关闭,完成后自动重新打开。",
                info.summary()
            );
            if let Some(body) = info.body.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
                let brief: String = body.chars().take(400).collect();
                msg.push_str("\n\n── 更新说明 ──\n");
                msg.push_str(&brief);
                if body.chars().count() > 400 {
                    msg.push_str("…");
                }
            }

            if confirm_dialog(&app, "发现新版本", &msg).await {
                log::info!("用户确认更新,开始下载");
                set_tooltip(&app, "DeepTutor — 正在下载更新…");
                if let Err(e) = crate::updater::install(&app).await {
                    log::error!("安装更新失败: {e}");
                    info_dialog(&app, "更新失败", &e).await;
                }
                // 成功路径不会走到这里 —— Windows 上安装器拉起后进程即退出。
            } else {
                log::info!("用户取消了本次更新");
            }
        }
    }

    set_tooltip(&app, TOOLTIP_IDLE);
    UPDATE_FLOW_RUNNING.store(false, Ordering::SeqCst);
}

/// 提示型对话框(仅确定按钮)。
async fn info_dialog(app: &AppHandle, title: &str, message: &str) {
    let app = app.clone();
    let title = title.to_string();
    let message = message.to_string();
    // blocking_show 会阻塞调用线程 —— 必须放到阻塞线程池,
    // 不能在 async runtime 的 worker 上直接跑。
    let _ = tauri::async_runtime::spawn_blocking(move || {
        use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
        app.dialog()
            .message(message)
            .title(title)
            .kind(MessageDialogKind::Info)
            .blocking_show();
    })
    .await;
}

/// 确认型对话框。返回 true 表示用户点了「立即更新」。
async fn confirm_dialog(app: &AppHandle, title: &str, message: &str) -> bool {
    let app = app.clone();
    let title = title.to_string();
    let message = message.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
        app.dialog()
            .message(message)
            .title(title)
            .kind(MessageDialogKind::Info)
            // 按钮文案必须自定义:默认的 OK/Cancel 在这里读起来不知所云。
            .buttons(MessageDialogButtons::OkCancelCustom(
                "立即更新".to_string(),
                "稍后".to_string(),
            ))
            .blocking_show()
    })
    .await
    .unwrap_or(false)
}
