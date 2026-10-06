//! DeepTutor Windows shell - library entrypoint.
//!
//! 与 macOS 端 DeepTutorDesktop 平行设计:
//! - SwiftUI + WKWebView (macOS)
//! - Tauri 2 + WebView2 + React 启动页 (Windows)
//!
//! 业务分工:
//! - main.rs        : crate 入口(windows_subsystem = "windows")
//! - backend/*      : Python 子进程管理(deeptutor start --no-browser)
//!                    其中 runtime.rs 负责定位安装包内置的 Python / Node,
//!                    使正式安装包不依赖用户机器上的运行时
//! - lmstudio/*     : LM Studio 桥接(模型列表/加载/卸载)
//! - ima/*          : 腾讯 IMA 自定义协议与桥接
//! - tray.rs        : 系统托盘菜单(含「检查更新…」入口)
//! - updater.rs     : 自动更新(检查/下载/安装),托盘与前端共用
//! - autostart.rs   : HKCU\...\Run 自启动注册

pub mod backend;
pub mod lmstudio;
pub mod ima;
pub mod tray;
pub mod updater;
pub mod autostart;
pub mod bili_search_window;

use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};

use tauri::Manager;

use backend::runner::Runner;
use backend::BundledRuntimes;

/// 是否已经提示过「已最小化到托盘」。
///
/// 只在本次运行内提示一次:反复弹框会烦人,但第一次关窗如果不说明,
/// 用户会以为程序没关掉(其实在托盘里活着,后端也还在跑)。
static TRAY_HINT_SHOWN: AtomicBool = AtomicBool::new(false);

/// 启动器:构建并运行 Tauri 应用。
pub fn run() {
    let app = tauri::Builder::default()
        // 单一实例:第二次启动时聚焦已有窗口
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        // 窗口位置/尺寸记忆。
        //
        // 显式排除 VISIBLE:本应用「关窗 = 隐藏到托盘」,若把可见性也记下来,
        // 用户在隐藏状态下退出(托盘右键退出)后,下次启动窗口会是隐藏的 ——
        // 表现为「双击图标没反应」,极难排查。
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::all()
                        & !tauri_plugin_window_state::StateFlags::VISIBLE,
                )
                .build(),
        )
        // 自启动
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        // 自动更新(github releases);托盘菜单的更新流程也走它
        .plugin(tauri_plugin_updater::Builder::new().build())
        // 原生对话框:更新流程的确认/结果提示。
        // 只在 Rust 侧使用(托盘菜单),前端不经 IPC 调用,故无需 capabilities 授权。
        .plugin(tauri_plugin_dialog::init())
        // 日志
        .plugin(tauri_plugin_log::Builder::default().build())
        // shell(打开外部链接用)
        .plugin(tauri_plugin_shell::init())
        // OS 信息
        .plugin(tauri_plugin_os::init())
        .setup(|app| {
            let mut runner = Runner::new();
            runner.attach(app.handle().clone());

            // 解析安装包内置运行时:自包含安装包携带 Python + Node,
            // 用户机器上无需预装任何东西。解析不到时退回系统 Python(开发场景)。
            let bundled = BundledRuntimes::detect(app.path().resource_dir().ok());
            log::info!("{}", bundled.describe());
            runner.set_bundled(bundled);

            // 回收上一轮可能残留的后端进程(兼容从旧版本升级上来的用户:
            // 那些孤儿进程会死占 8001/3782,让新实例静默连到旧服务上)。
            runner.reap_stale_backend();

            let runner = Arc::new(runner);
            app.manage(runner);

            // 启动系统托盘(Tauri 2 内置 API)
            tray::setup(app.handle())?;
            // 诊断开关：设了 DEEPTUTOR_OPEN_BILI_SEARCH 就启动后把 B 站搜索
            // 窗口开出来。它的正式入口是托盘菜单，而托盘点不点得动没法自动化
            // —— 留这个开关是为了真机能一键验证那扇窗口(CSP/IPC/缩略图/关窗)。
            // 平时不设，等于没有这段代码。
            //
            // ★ 不能在 setup() 里同步开：窗口要等消息循环跑起来才能建，
            // 否则是竞态（实测会随机报 0x80070578「无效的窗口句柄」然后
            // 窗口静默消失）。所以推迟到主/UI 线程上再开。
            if std::env::var_os("DEEPTUTOR_OPEN_BILI_SEARCH").is_some() {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(900));
                    let inner = handle.clone();
                    if let Err(e) = handle.run_on_main_thread(move || {
                        if let Err(e) = bili_search_window::open(&inner) {
                            log::warn!("启动时打开 B 站搜索窗口失败: {e}");
                        }
                    }) {
                        log::warn!("调度搜索窗口打开失败: {e}");
                    }
                });
            }
            // 注册 IMA 自定义协议路由
            ima::protocol::register(app.handle())?;
            // 启动后端引导流程(异步,不阻塞窗口显示)
            backend::boot::spawn(app.handle());
            // 后端就绪后静默查一次有没有新版后端(只提示,不自动装)
            backend::hotupdate::spawn_auto_check(app.handle());
            Ok(())
        })
        // 关闭主窗口 = 隐藏到系统托盘(不退出)。
        //
        // 这样后端继续跑,托盘随时能唤回来 —— 也避免了「用户以为关了,
        // 其实 python 进程还占着 8001/3782」这类困惑。
        //
        // 真正退出走托盘菜单的「退出」,那里会显式清理后端进程树。
        //
        // 注意与历史行为的差异:0.2.1 及更早「关窗即退出」,是为了解决
        // 「窗口关了后端还在跑」的问题。现在改成关窗隐藏,那个问题由
        // 托盘退出路径 + Job Object 兜底解决,不再需要关窗就杀进程。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() != "main" {
                    return;
                }
                api.prevent_close();
                let _ = window.hide();
                log::info!("主窗口已隐藏到系统托盘(应用与后端继续运行)");

                // 首次关窗提示一次,否则用户会以为程序没关掉。
                if !TRAY_HINT_SHOWN.swap(true, Ordering::SeqCst) {
                    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
                    let app = window.app_handle().clone();
                    tauri::async_runtime::spawn(async move {
                        app.dialog()
                            .message("DeepTutor 仍在后台运行,可从系统托盘图标唤回。\n\n要彻底退出请右键托盘图标选择「退出」。")
                            .title("已最小化到托盘")
                            .kind(MessageDialogKind::Info)
                            .blocking_show();
                    });
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::ping,
            commands::get_app_info,
            commands::backend_status,
            commands::backend_logs,
            commands::backend_restart,
            commands::backend_stop,
            commands::lmstudio_status,
            commands::lmstudio_load,
            commands::lmstudio_unload,
            commands::check_for_update,
            commands::install_update,
            commands::backend_versions,
            commands::check_backend_update,
            commands::install_backend_update,
            commands::rollback_backend,
            commands::bili_search,
            commands::bili_open,
        ])
        .build(tauri::generate_context!())
        .expect("error while building DeepTutor shell");

    app.run(|handle, event| {
        // 退出时确保后端进程树被清理,否则端口会被孤儿进程占住
        if let tauri::RunEvent::Exit = event {
            if let Some(runner) = handle.try_state::<Arc<Runner>>() {
                runner.kill_tree_sync();
            }
        }
    });
}

/// Tauri 暴露给前端的 command 集合。
mod commands {
    use std::sync::Arc;

    use serde::Serialize;
    use tauri::{AppHandle, Manager};

    use crate::backend::boot;
    use crate::backend::hotupdate;
    use crate::backend::runner::{LogLine, Runner, StatusSnapshot};
    use crate::lmstudio::{Detector, ModelInfo, ModelsClient};

    #[derive(Serialize)]
    pub struct AppInfo {
        pub name: &'static str,
        pub version: &'static str,
        pub target: &'static str,
    }

    /// 心跳,用于前端确认 shell IPC 链路通畅。
    #[tauri::command]
    pub fn ping() -> &'static str {
        "pong"
    }

    /// 返回当前壳层元信息。
    #[tauri::command]
    pub fn get_app_info() -> AppInfo {
        AppInfo {
            name: "DeepTutor Desktop for Windows",
            version: env!("CARGO_PKG_VERSION"),
            target: std::env::consts::OS,
        }
    }

    /// 当前后端状态快照(前端首屏挂载时主动拉一次,避免错过事件)。
    #[tauri::command]
    pub fn backend_status(app: AppHandle) -> StatusSnapshot {
        let runner: Arc<Runner> = app.state::<Arc<Runner>>().inner().clone();
        runner.status()
    }

    /// 最近的后端日志(环形缓冲,最多 500 行)。
    #[tauri::command]
    pub fn backend_logs(app: AppHandle) -> Vec<LogLine> {
        let runner: Arc<Runner> = app.state::<Arc<Runner>>().inner().clone();
        runner.logs()
    }

    /// 重启后端引导流程。
    #[tauri::command]
    pub async fn backend_restart(app: AppHandle) -> Result<(), String> {
        let runner: Arc<Runner> = app.state::<Arc<Runner>>().inner().clone();
        runner.stop().await.map_err(|e| e.to_string())?;
        tauri::async_runtime::spawn(async move {
            boot::run(app, runner).await;
        });
        Ok(())
    }

    /// 停止后端。
    #[tauri::command]
    pub async fn backend_stop(app: AppHandle) -> Result<(), String> {
        let runner: Arc<Runner> = app.state::<Arc<Runner>>().inner().clone();
        runner.stop().await.map_err(|e| e.to_string())
    }

    /// LM Studio 状态:探测可用性 + 列出所有已下载模型(含加载状态)。
    #[derive(Serialize)]
    pub struct LmStudioStatus {
        pub detected: bool,
        pub base_url: Option<String>,
        pub models: Vec<ModelInfo>,
    }

    #[tauri::command]
    pub async fn lmstudio_status() -> LmStudioStatus {
        let detector = Detector::new();
        match detector.probe().await {
            Some(base_url) => {
                let models = ModelsClient::new(&base_url).list().await.unwrap_or_default();
                LmStudioStatus {
                    detected: true,
                    base_url: Some(base_url),
                    models,
                }
            }
            None => LmStudioStatus {
                detected: false,
                base_url: None,
                models: Vec::new(),
            },
        }
    }

    /// 加载模型到内存。
    #[tauri::command]
    pub async fn lmstudio_load(model_id: String) -> Result<(), String> {
        let detector = Detector::new();
        let base = detector
            .probe()
            .await
            .ok_or_else(|| "未检测到 LM Studio,请先启动并开启本地服务器".to_string())?;
        ModelsClient::new(base)
            .load(&model_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// 从内存卸载模型。
    #[tauri::command]
    pub async fn lmstudio_unload(model_id: String) -> Result<(), String> {
        let detector = Detector::new();
        let base = detector
            .probe()
            .await
            .ok_or_else(|| "未检测到 LM Studio,请先启动并开启本地服务器".to_string())?;
        ModelsClient::new(base)
            .unload(&model_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// 检查更新结果(定义在 updater 模块,前端类型与之对应)。
    pub use crate::updater::UpdateCheck;

    /// 检查是否有新版本。实际实现在 `updater::check`,与托盘菜单共用。
    #[tauri::command]
    pub async fn check_for_update(app: AppHandle) -> Result<UpdateCheck, String> {
        crate::updater::check(&app).await
    }

    /// 下载并安装更新。
    ///
    /// Windows 下安装器拉起后本进程会退出并由安装程序接管,故正常路径不会返回。
    /// 实现见 `updater::install` —— 那里在启动安装器前会先停掉后端进程树。
    #[tauri::command]
    pub async fn install_update(app: AppHandle) -> Result<(), String> {
        crate::updater::install(&app).await
    }

    // ---- 后端(deeptutor)热更新 ----
    //
    // 与上面两个命令的区别:那两个更新的是**桌面壳**(GitHub Releases),
    // 下面这几个更新的是**后端**(PyPI),只重启 Python 子进程。
    // 详见 `crate::backend::hotupdate` 的模块文档。

    /// 当前后端版本分布(叠加层优先于安装包内置版)。
    #[tauri::command]
    pub fn backend_versions(app: AppHandle) -> hotupdate::Versions {
        let runner: Arc<Runner> = app.state::<Arc<Runner>>().inner().clone();
        hotupdate::versions(&runner)
    }

    /// 检查 PyPI 上有没有新的后端版本。
    #[tauri::command]
    pub async fn check_backend_update(app: AppHandle) -> Result<hotupdate::UpdateInfo, String> {
        let runner: Arc<Runner> = app.state::<Arc<Runner>>().inner().clone();
        hotupdate::check(&runner).await
    }

    /// 下载并安装新版后端(含依赖解析)。后端会在完成后自动重启。
    #[tauri::command]
    pub async fn install_backend_update(app: AppHandle) -> Result<hotupdate::InstallReport, String> {
        hotupdate::install(&app).await
    }

    /// 回退到安装包内置的后端版本。
    #[tauri::command]
    pub async fn rollback_backend(app: AppHandle) -> Result<String, String> {
        hotupdate::rollback(&app).await
    }

    // ---- B 站搜索窗口（见 `crate::bili_search_window` 的模块文档）----
    //
    // 这两个 command 是**搜索窗口那页 HTML 唯一的出口**。前端是上游
    // Next.js 的编译产物、加不了 B 站 UI，所以入口只能自己搭；搜索逻辑
    // 仍在补丁包里，这里只做转发。

    /// 搜索 B 站视频。
    #[tauri::command]
    pub async fn bili_search(
        app: AppHandle,
        keyword: String,
        page: Option<u32>,
    ) -> Result<crate::backend::bilibili::SearchOutcome, String> {
        crate::backend::bilibili::search(&app, &keyword, page.unwrap_or(1)).await
    }

    /// 用系统默认浏览器打开一个 B 站视频。
    ///
    /// ★ 只允许 `bilibili.com` 域 —— 搜索结果里的 URL 来自网络响应，
    /// 直接丢给 shell 打开等于给了任意 URL 拉起外部程序的机会。
    #[allow(deprecated)]
    #[tauri::command]
    pub fn bili_open(app: AppHandle, url: String) -> Result<(), String> {
        use tauri_plugin_shell::ShellExt;
        let lower = url.to_ascii_lowercase();
        if !lower.starts_with("https://www.bilibili.com/")
            && !lower.starts_with("https://bilibili.com/")
        {
            return Err("只允许打开 bilibili.com 的链接。".into());
        }
        // 有意用 deprecated 的 `Shell::open`：它在本项目已注册的
        // `tauri-plugin-shell` 里，功能完好。换成 `tauri-plugin-opener`
        // 需要新增依赖并配 capability，属于无谓的连带改动 —— 等上游把
        // shell 插件整体迁走时再一并处理。
        app.shell()
            .open(&url, None)
            .map_err(|e| format!("打开失败：{e}"))
    }
}
