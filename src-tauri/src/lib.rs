//! DeepTutor Windows shell - library entrypoint.
//!
//! 与 macOS 端 DeepTutorDesktop 平行设计:
//! - SwiftUI + WKWebView (macOS)
//! - Tauri 2 + WebView2 + React 启动页 (Windows)
//!
//! 业务分工:
//! - main.rs        : crate 入口(windows_subsystem = "windows")
//! - backend/*      : Python 子进程管理(deeptutor start --child)
//! - lmstudio/*     : LM Studio 桥接(模型列表/加载/卸载)
//! - ima/*          : 腾讯 IMA 自定义协议与桥接
//! - tray.rs        : 系统托盘菜单
//! - autostart.rs   : HKCU\...\Run 自启动注册

pub mod backend;
pub mod lmstudio;
pub mod ima;
pub mod tray;
pub mod autostart;

use std::sync::Arc;

use tauri::Manager;

use backend::runner::Runner;

/// 启动器:构建并运行 Tauri 应用。
pub fn run() {
    let app = tauri::Builder::default()
        // 单一实例:第二次启动时聚焦已有窗口
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
        }))
        // 窗口位置/尺寸记忆
        .plugin(tauri_plugin_window_state::Builder::default().build())
        // 自启动
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        // 自动更新(github releases)
        .plugin(tauri_plugin_updater::Builder::new().build())
        // 日志
        .plugin(tauri_plugin_log::Builder::default().build())
        // shell(打开外部链接用)
        .plugin(tauri_plugin_shell::init())
        // OS 信息
        .plugin(tauri_plugin_os::init())
        .setup(|app| {
            let mut runner = Runner::new();
            runner.attach(app.handle().clone());
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
    use tauri_plugin_updater::UpdaterExt;

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

    /// 检查更新结果。
    #[derive(Serialize)]
    pub struct UpdateCheck {
        pub available: bool,
        pub current_version: String,
        pub latest_version: Option<String>,
        pub body: Option<String>,
        pub date: Option<String>,
    }

    /// 检查是否有新版本(github releases latest.json)。
    #[tauri::command]
    pub async fn check_for_update(app: AppHandle) -> Result<UpdateCheck, String> {
        let current = app
            .package_info()
            .version
            .to_string();
        let updater = app
            .updater()
            .map_err(|e| format!("初始化更新器失败: {e}"))?;
        match updater.check().await.map_err(|e| e.to_string())? {
            Some(update) => Ok(UpdateCheck {
                available: true,
                current_version: current,
                latest_version: Some(update.version),
                body: update.body,
                date: update.date.map(|d| d.to_string()),
            }),
            None => Ok(UpdateCheck {
                available: false,
                current_version: current,
                latest_version: None,
                body: None,
                date: None,
            }),
        }
    }

    /// 下载并安装更新(Windows 下安装完成后会自动退出并拉起新版本)。
    #[tauri::command]
    pub async fn install_update(app: AppHandle) -> Result<(), String> {
        let updater = app
            .updater()
            .map_err(|e| format!("初始化更新器失败: {e}"))?;
        let update = updater
            .check()
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "当前已是最新版本".to_string())?;
        update
            .download_and_install(|_chunk, _total| {}, || {})
            .await
            .map_err(|e| e.to_string())
    }
}
