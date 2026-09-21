//! 自动更新:检查 / 下载 / 安装。
//!
//! 三个入口共用这一份实现:
//! - 启动页的「检查更新」按钮   -> `commands::check_for_update`
//! - 启动页的「立即更新」按钮   -> `commands::install_update`
//! - 系统托盘菜单「检查更新…」  -> `tray.rs`
//!
//! 托盘入口是主力。后端就绪后启动页会被 DeepTutor Web UI 整个顶掉
//! (`window.location.replace(WEB_URL)`),前端那套按钮就再也点不到了 ——
//! 只有托盘菜单在任何时候都在。
//!
//! # 安装前必须停后端
//!
//! NSIS 安装器要覆盖 `runtimes\python\python.exe`,而后端 python 进程
//! 正持有它。不停进程的话,安装阶段会因文件被占用而失败。
//! 所以下载完成、启动安装器之前先 `runner.stop()`。
//!
//! # 为什么不能只依赖 RunEvent::Exit
//!
//! `tauri-plugin-updater` 在 Windows 上启动安装器后直接
//! `std::process::exit(0)`(见插件 `updater.rs` 的 `install_inner`),
//! 它**绕过** Tauri 的 `RunEvent::Exit` —— 我们挂在退出事件上的后端清理
//! 不会执行。所以这里堆了三层保险:
//!
//! 1. 安装前显式 `stop()`,不依赖任何退出事件;
//! 2. `on_before_exit` 钩子做同步强杀兜底(它在 ShellExecuteW 之前触发);
//! 3. Job Object 的 `KILL_ON_JOB_CLOSE` —— 内核级最终兜底。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::UpdaterExt;

use crate::backend::runner::Runner;

/// 下载进度事件名。前端启动页监听它渲染进度条。
pub const EVENT_PROGRESS: &str = "update://progress";

/// 检查更新结果,字段与前端 `UpdateCheck` 类型一一对应。
#[derive(Debug, Clone, Serialize)]
pub struct UpdateCheck {
    pub available: bool,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub body: Option<String>,
    pub date: Option<String>,
}

impl UpdateCheck {
    /// 供托盘菜单拼提示文案用的一行摘要。
    pub fn summary(&self) -> String {
        if !self.available {
            return format!("当前已是最新版本 (v{})", self.current_version);
        }
        let latest = self.latest_version.as_deref().unwrap_or("未知");
        format!("发现新版本 v{latest} (当前 v{})", self.current_version)
    }
}

/// 下载 / 安装阶段。
///
/// 用 enum 而非裸字符串:托盘侧要反序列化这个载荷来更新 tooltip,
/// 枚举能同时表达「合法的取值集合」和「JSON 里长什么样」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Downloading,
    Installing,
}

/// 下载 / 安装进度,发给前端的进度载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub phase: Phase,
    /// 已下载字节数。
    pub downloaded: u64,
    /// 总字节数;服务端未给 Content-Length 时为 None。
    pub total: Option<u64>,
    /// 0-100;总长未知时为 None(前端应显示不确定进度)。
    pub percent: Option<f64>,
}

/// 检查是否有新版本(读 github releases 的 latest.json)。
pub async fn check(app: &AppHandle) -> Result<UpdateCheck, String> {
    let current = app.package_info().version.to_string();
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

/// 下载并安装更新。
///
/// Windows 上安装器被拉起后本进程即退出(插件内部 `process::exit`),
/// 由 NSIS 以 passive 模式完成安装并重启应用 —— 所以正常路径下
/// 本函数的 `Ok(())` 分支在 Windows 上几乎不会返回。
pub async fn install(app: &AppHandle) -> Result<(), String> {
    // 用 builder 而不是 `app.updater()`:只有 builder 能挂 on_before_exit 钩子。
    // 钩子在插件启动安装器之前触发,此时异步 runtime 可能已经不可用,
    // 所以里面只能用同步的 kill_tree_sync。
    let guard = app.clone();
    let updater = app
        .updater_builder()
        .on_before_exit(move || {
            if let Some(runner) = guard.try_state::<Arc<Runner>>() {
                runner.kill_tree_sync();
            }
        })
        .build()
        .map_err(|e| format!("初始化更新器失败: {e}"))?;
    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "当前已是最新版本".to_string())?;

    // ── 1) 下载(此阶段后端仍可正常服务,只在安装前才停) ──
    let mut downloaded: u64 = 0;
    let mut last_pct: i64 = -1;
    let progress_app = app.clone();
    let bytes = update
        .download(
            move |chunk: usize, total: Option<u64>| {
                downloaded += chunk as u64;
                // 按整数百分比节流:一次下载会回调上千次,
                // 每次都 emit 会把 IPC 打爆。
                let pct = total
                    .filter(|t| *t > 0)
                    .map(|t| (downloaded.saturating_mul(100) / t).min(100) as i64)
                    .unwrap_or(-1);
                if pct == last_pct {
                    return;
                }
                last_pct = pct;
                let _ = progress_app.emit(
                    EVENT_PROGRESS,
                    Progress {
                        phase: Phase::Downloading,
                        downloaded,
                        total,
                        percent: total
                            .filter(|t| *t > 0)
                            .map(|t| downloaded as f64 / t as f64 * 100.0),
                    },
                );
            },
            || {},
        )
        .await
        .map_err(|e| format!("下载更新包失败: {e}"))?;

    log::info!("更新包下载完成,共 {} 字节", bytes.len());

    // ── 2) 停后端,释放 runtimes\python\python.exe 的占用 ──
    stop_backend(app).await;

    let _ = app.emit(
        EVENT_PROGRESS,
        Progress {
            phase: Phase::Installing,
            downloaded: bytes.len() as u64,
            total: Some(bytes.len() as u64),
            percent: Some(100.0),
        },
    );

    // ── 3) 启动 NSIS 安装器 ──
    // 插件内部顺序:先跑 on_before_exit 钩子(第 2 层保险),
    // 再 ShellExecuteW 拉起安装器,最后 std::process::exit(0)。
    update
        .install(bytes)
        .map_err(|e| format!("启动安装程序失败: {e}"))
}

/// 停掉后端进程树。更新安装前调用,失败不阻断安装流程。
async fn stop_backend(app: &AppHandle) {
    let Some(runner) = app.try_state::<Arc<Runner>>() else {
        return;
    };
    let runner = runner.inner().clone();
    log::info!("安装更新前停止后端,释放内置运行时文件占用");
    if let Err(e) = runner.stop().await {
        log::warn!("停止后端失败(继续安装): {e}");
    }
}
