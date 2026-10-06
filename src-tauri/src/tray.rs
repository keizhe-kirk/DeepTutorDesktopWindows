//! 系统托盘菜单。
//!
//! 使用 Tauri 2 内置的 TrayIconBuilder(不再依赖 tauri-plugin-system-tray,
//! 该插件在 Tauri 2 已移除)。
//!
//! 菜单项:
//! - 打开主窗口 / 隐藏主窗口
//! - 重启后端
//! - 检查桌面壳更新…      (走 GitHub Releases,装完重启整个应用)
//! - 检查后端更新…        (走 PyPI,只重启 Python 子进程)
//! - 回退到内置后端        (撤销后端热更新)
//! - 退出
//!
//! # 为什么「检查更新」放在托盘而不只放界面上
//!
//! 后端就绪后启动页会被 DeepTutor Web UI 整个顶掉
//! (`window.location.replace(WEB_URL)`),启动页上那个「检查更新」按钮
//! 随之消失 —— 正常使用软件的路径上根本点不到它。
//! 托盘菜单在任何时候都在,是唯一可靠的入口。
//!
//! # 两种更新要分清
//!
//! | 菜单项 | 更新对象 | 来源 | 代价 |
//! |---|---|---|---|
//! | 检查桌面壳更新… | Tauri 壳 + 内置运行时 | GitHub Releases | 重装 230 MB,应用重启 |
//! | 检查后端更新…   | deeptutor(pip 包) | PyPI | 增量几十 MB,只重启子进程 |
//!
//! 后端小版本迭代频繁(1.6.9 -> 1.6.12 只用几天),每次都发壳安装包毫无必要。
//! 后端热更新就是为这条路径准备的,详见 `crate::backend::hotupdate`。
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

use crate::backend::hotupdate;
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
            &MenuItem::with_id(app, "update", "检查桌面壳更新…", true, None::<&str>)?,
            &MenuItem::with_id(app, "backend-update", "检查后端更新…", true, None::<&str>)?,
            &MenuItem::with_id(app, "backend-rollback", "回退到内置后端", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            // 设置组:先把「找视频」的入口放在最前面 —— 没有入口的功能等于没有。
            &MenuItem::with_id(app, "bili-search", "搜索哔哩哔哩…", true, None::<&str>)?,
            // B 站字幕需要登录态,没登录态时「边看边学」只剩播放。
            &MenuItem::with_id(app, "bili-login", "登录哔哩哔哩（获取字幕）…", true, None::<&str>)?,
            &MenuItem::with_id(app, "bili-logout", "清除哔哩哔哩登录", true, None::<&str>)?,
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
            "backend-update" => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    run_backend_update_flow(handle).await;
                });
            }
            "backend-rollback" => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    run_backend_rollback_flow(handle).await;
                });
            }
            "bili-login" => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    run_bilibili_login_flow(handle).await;
                });
            }
            "bili-search" => {
                // 开窗是同步动作（要在 UI 线程拿Webview），失败弹说明框。
                if let Err(e) = crate::bili_search_window::open(app) {
                    let handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        info_dialog(&handle, "无法打开搜索窗口", &e.to_string()).await;
                    });
                }
            }
            "bili-logout" => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    run_bilibili_logout_flow(handle).await;
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

    // 后端热更新的进度同样写 tooltip(它的阶段比桌面壳多)。
    let handle = app.clone();
    app.listen(hotupdate::EVENT_PROGRESS, move |event| {
        let Ok(p) = serde_json::from_str::<hotupdate::Progress>(event.payload()) else {
            return;
        };
        let prefix = match p.phase {
            hotupdate::Phase::Stopping => "正在停止后端",
            hotupdate::Phase::Resolving => "正在解析依赖",
            hotupdate::Phase::Downloading => "正在下载后端",
            hotupdate::Phase::Extracting => "正在解压",
            hotupdate::Phase::Activating => "正在激活",
            hotupdate::Phase::Restarting => "正在重启后端",
        };
        set_tooltip(&handle, &format!("DeepTutor — {prefix}…"));
    });

    Ok(())
}

/// 改托盘 tooltip;托盘不存在时静默忽略。
pub(crate) fn set_tooltip(app: &AppHandle, text: &str) {
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

/// 托盘「检查后端更新…」的完整流程。
///
/// 与桌面壳更新(「检查桌面壳更新…」)的区别:这里只重启 Python 子进程,
/// 应用本身不退出,所以没有「安装期间应用会关闭」那套说法。
async fn run_backend_update_flow(app: AppHandle) {
    if UPDATE_FLOW_RUNNING.swap(true, Ordering::SeqCst) {
        log::info!("已有更新流程在执行,忽略本次点击");
        return;
    }

    let Some(runner) = app.try_state::<Arc<Runner>>().map(|s| s.inner().clone()) else {
        info_dialog(&app, "后端更新", "后端管理器尚未就绪,请稍后重试。").await;
        UPDATE_FLOW_RUNNING.store(false, Ordering::SeqCst);
        return;
    };

    set_tooltip(&app, "DeepTutor — 正在检查后端更新…");

    let info = match hotupdate::check(&runner).await {
        Ok(i) => i,
        Err(e) => {
            log::warn!("检查后端更新失败: {e}");
            info_dialog(&app, "检查后端更新失败", &e).await;
            set_tooltip(&app, TOOLTIP_IDLE);
            UPDATE_FLOW_RUNNING.store(false, Ordering::SeqCst);
            return;
        }
    };

    if !info.available {
        info_dialog(&app, "检查后端更新", &info.summary()).await;
        set_tooltip(&app, TOOLTIP_IDLE);
        UPDATE_FLOW_RUNNING.store(false, Ordering::SeqCst);
        return;
    }

    let msg = format!(
        "{}\n\n是否现在下载并安装?\n\n\
         安装期间后端会重启,界面短暂断开后自动恢复;应用本身不会关闭。\n\n\
         下载源:{}",
        info.summary(),
        info.index_url
    );

    if !confirm_dialog_with(&app, "发现新后端版本", &msg, "立即更新", "稍后").await {
        log::info!("用户取消了后端更新");
        set_tooltip(&app, TOOLTIP_IDLE);
        UPDATE_FLOW_RUNNING.store(false, Ordering::SeqCst);
        return;
    }

    set_tooltip(&app, "DeepTutor — 正在更新后端…");
    match hotupdate::install(&app).await {
        Err(e) => {
            log::error!("后端更新失败: {e}");
            info_dialog(&app, "后端更新失败", &e).await;
        }
        Ok(report) => {
            let mut text = format!(
                "后端已更新到 {}\n共写入 {} 个文件,下载 {:.1} MB。",
                report.version,
                report.files,
                report.downloaded_bytes as f64 / 1_048_576.0
            );

            if !report.dependencies.is_empty() {
                // 依赖可能有几十个,别把对话框撑爆
                let shown = report.dependencies.iter().take(15).cloned().collect::<Vec<_>>();
                text.push_str(&format!(
                    "\n\n一并更新了 {} 个依赖:\n{}",
                    report.dependencies.len(),
                    shown.join("\n")
                ));
                if report.dependencies.len() > shown.len() {
                    text.push_str(&format!(
                        "\n…另有 {} 个未列出",
                        report.dependencies.len() - shown.len()
                    ));
                }
            }

            if !report.warnings.is_empty() {
                let shown = report.warnings.iter().take(6).cloned().collect::<Vec<_>>();
                text.push_str(&format!("\n\n需要注意:\n{}", shown.join("\n")));
                if report.warnings.len() > shown.len() {
                    text.push_str(&format!("\n…另有 {} 条未列出(详见日志)", report.warnings.len() - shown.len()));
                }
            }

            text.push_str("\n\n后端正在重启,界面稍后会自动恢复。");
            info_dialog(&app, "后端更新完成", &text).await;
        }
    }

    set_tooltip(&app, TOOLTIP_IDLE);
    UPDATE_FLOW_RUNNING.store(false, Ordering::SeqCst);
}

/// 托盘「回退到内置后端」的流程。
///
/// 只改状态文件、不删任何包,所以是安全的撤销操作 ——
/// 用户对新版后端不满意(或新版起不来)时的一键后路。
async fn run_backend_rollback_flow(app: AppHandle) {
    let Some(runner) = app.try_state::<Arc<Runner>>().map(|s| s.inner().clone()) else {
        return;
    };

    let v = hotupdate::versions(&runner);
    if !v.using_overlay {
        info_dialog(
            &app,
            "回退后端",
            &format!(
                "当前正在使用安装包内置版本 {},无需回退。",
                v.bundled.as_deref().unwrap_or("未知")
            ),
        )
        .await;
        return;
    }

    let msg = format!(
        "将把后端从 {} 回退到安装包内置的 {}。\n\n\
         已下载的新版本文件不会被删除,之后仍可再次升级。\n\n\
         是否继续?",
        v.effective.as_deref().unwrap_or("未知"),
        v.bundled.as_deref().unwrap_or("未知")
    );

    if !confirm_dialog_with(&app, "回退后端", &msg, "回退", "取消").await {
        return;
    }

    set_tooltip(&app, "DeepTutor — 正在回退后端…");
    match hotupdate::rollback(&app).await {
        Ok(m) => info_dialog(&app, "回退完成", &m).await,
        Err(e) => info_dialog(&app, "回退失败", &e).await,
    }
    set_tooltip(&app, TOOLTIP_IDLE);
}

// 登录流程防重入:扫码要几分钟,期间连点会起多个浏览器实例。
static BILI_LOGIN_RUNNING: AtomicBool = AtomicBool::new(false);

/// 托盘「登录哔哩哔哩」的流程。
///
/// B 站的字幕/章节接口需要登录态（实测多数教学视频`need_login_subtitle=true`），
/// 没登录态时沉浸式观看只剩播放、没有字幕可跟读。
///
/// 这里用系统浏览器扫码，**不接触账号密码**：凭据只落
/// `%LOCALAPPDATA%\DeepTutor\patches\bili_credentials.json`，且只保留
/// `SESSDATA` / `buvid3` 两项。
async fn run_bilibili_login_flow(app: AppHandle) {
    use crate::backend::bilibili;

    if BILI_LOGIN_RUNNING.swap(true, Ordering::SeqCst) {
        info_dialog(&app, "哔哩哔哩登录", "已有一个登录流程在进行中，请先完成或等待超时。").await;
        return;
    }

    if let Err(e) = bilibili::patch_ready_check(&app) {
        info_dialog(&app, "哔哩哔哩登录", &e).await;
        BILI_LOGIN_RUNNING.store(false, Ordering::SeqCst);
        return;
    }

    let result = bilibili::login(&app, |notice| set_tooltip(&app, notice)).await;
    set_tooltip(&app, TOOLTIP_IDLE);
    BILI_LOGIN_RUNNING.store(false, Ordering::SeqCst);

    match result {
        Ok(outcome) if outcome.ok => {
            //登录后立刻校验一次：SESSDATA 写下了不等于服务端认。
            let detail = match bilibili::status(&app).await {
                Ok(s) if s.valid == "true" => {
                    let who = if s.detail.is_empty() { String::new() } else { format!("（{}）", s.detail) };
                    format!("已保存登录态{who}。\n\n现在把B 站链接粘进沉浸式观看，字幕与章节会随视频一起加载。")
                }
                Ok(s) => format!(
                    "登录态已保存，但 B 站暂未接受它：{}\n\n可以稍后重试，或换一个账号再试。",
                    s.detail
                ),
                Err(_) => "已保存登录态。".to_string(),
            };
            info_dialog(&app, "哔哩哔哩登录成功", &detail).await;
        }
        Ok(_) => {
            info_dialog(
                &app,
                "未完成登录",
                "没有取到登录态。\n\n请在打开的浏览器里完成扫码后重试；如果浏览器没有自动打开，\
                 也可以手动打开 https://passport.bilibili.com/login 登录后，\
                 再回到这里点一次「登录哔哩哔哩」。",
            )
            .await;
        }
        Err(e) => info_dialog(&app, "哔哩哔哩登录失败", &e).await,
    }
}

/// 托盘「清除哔哩哔哩登录」的流程。
async fn run_bilibili_logout_flow(app: AppHandle) {
    use crate::backend::bilibili;

    let current = match bilibili::status(&app).await {
        Ok(s) => s,
        Err(e) => {
            info_dialog(&app, "清除哔哩哔哩登录", &e).await;
            return;
        }
    };

    if current.logged_in != "true" {
        info_dialog(&app, "清除哔哩哔哩登录", "当前没有保存任何哔哩哔哩登录态。").await;
        return;
    }

    let msg = match current.detail.as_str() {
        "" => "将删除本机保存的哔哩哔哩登录态。\n\n删除后沉浸式观看将只剩播放、没有字幕。".to_string(),
        d => format!(
            "将删除本机保存的哔哩哔哩登录态（{d}）。\n\n删除后沉浸式观看将只剩播放、没有字幕。"
        ),
    };
    if !confirm_dialog_with(&app, "清除哔哩哔哩登录", &msg, "清除", "取消").await {
        return;
    }

    match bilibili::clear(&app).await {
        Ok(o) if o.ok => info_dialog(&app, "已清除", "本机保存的哔哩哔哩登录态已删除。").await,
        Ok(_) => info_dialog(&app, "清除失败", "没有找到可清除的登录态（可能已经清过了）。").await,
        Err(e) => info_dialog(&app, "清除失败", &e).await,
    }
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

/// 确认型对话框。返回 true 表示用户点了确认按钮。
async fn confirm_dialog(app: &AppHandle, title: &str, message: &str) -> bool {
    confirm_dialog_with(app, title, message, "立即更新", "稍后").await
}

/// 确认型对话框(自定义按钮文案)。
///
/// 默认的 OK/Cancel 在这类场景下读起来不知所云,必须换成动词。
async fn confirm_dialog_with(
    app: &AppHandle,
    title: &str,
    message: &str,
    ok_text: &str,
    cancel_text: &str,
) -> bool {
    let app = app.clone();
    let title = title.to_string();
    let message = message.to_string();
    let ok_text = ok_text.to_string();
    let cancel_text = cancel_text.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
        app.dialog()
            .message(message)
            .title(title)
            .kind(MessageDialogKind::Info)
            .buttons(MessageDialogButtons::OkCancelCustom(ok_text, cancel_text))
            .blocking_show()
    })
    .await
    .unwrap_or(false)
}
