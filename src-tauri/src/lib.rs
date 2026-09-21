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
            // 注册 IMA 自定义协议路由
            ima::protocol::register(app.handle())?;
            // 启动后端引导流程(异步,不阻塞窗口显示)
            backend::boot::spawn(app.handle());
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
}
