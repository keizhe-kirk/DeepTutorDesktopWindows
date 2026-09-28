//! 后端(deeptutor)热更新。
//!
//! # 要解决的问题
//!
//! 后端 deeptutor 原本是**硬编码打进安装包**的(`runtimes\python\Lib\site-packages`),
//! 于是上游每发一个后端版本,就得重发一次 230 MB 的桌面壳安装包 —— 哪怕
//! 壳代码一个字没改。1.6.9 -> 1.6.12 几天之内三个版本,就得发三次。
//!
//! # 做法
//!
//! 不去动安装目录(那是 `perMachine` 装的,普通用户无权写),而是把新版后端
//! 解到用户自己的目录里,启动时用 `PYTHONPATH` 把它盖在内置版上面。
//! 详见 [`super::overlay`] 的模块文档。
//!
//! 完整流程:
//!
//! ```text
//! 查 PyPI 最新版
//!   -> 下载 deeptutor wheel(sha256 校验)
//!   -> 读它的 METADATA 拿到 Requires-Dist
//!   -> 与已装环境比对,缺的/版本不够的走「迷你 pip」解析(见 resolve)
//!   -> 下载这些依赖 wheel
//!   -> 全部解压到 <home>\backend\versions\<版本>\
//!   -> 写 state.json 激活
//!   -> 清前端运行时缓存标记
//!   -> 重启后端
//! ```
//!
//! 内置版**一个字节都不动**,所以随时可以一键退回([`rollback`])。
//!
//! # 与 `crate::updater` 的区别
//!
//! - `crate::updater`      —— **桌面壳**的更新,走 GitHub Releases + minisign 签名,
//!                            装完要重启整个应用,托盘菜单「检查更新…」。
//! - `crate::backend::hotupdate`(本模块) —— **后端**的更新,走 PyPI + sha256,
//!                            只重启 Python 子进程,托盘菜单「检查后端更新…」。

pub mod pypi;
pub mod resolve;
pub mod wheel;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use pep440_rs::Version;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::backend::config::BackendConfig;
use crate::backend::overlay::{Overlay, OverlayState};
use crate::backend::runner::Runner;
use crate::backend::{boot, python, runtime};

use pypi::{InterpTag, PypiClient, ReleaseFile};
use resolve::InstalledEnv;

/// PyPI 上的包名。
pub const PACKAGE: &str = "deeptutor";

/// 进度事件名。前端启动页与托盘 tooltip 都监听它。
pub const EVENT_PROGRESS: &str = "backend-update://progress";

/// 热更新流程防重入。
///
/// 托盘菜单点不出「禁用态」,两次点击会起两个下载任务、并发写同一个
/// 版本目录 —— 用原子标志挡住。
static RUNNING: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------- 数据模型

/// 当前后端版本分布。
#[derive(Debug, Clone, Serialize)]
pub struct Versions {
    /// 实际生效的版本(叠加层优先)。
    pub effective: Option<String>,
    /// 安装包内置的版本。
    pub bundled: Option<String>,
    /// 生效的是否是热更新的叠加层(`false` = 正在用内置版)。
    pub using_overlay: bool,
    /// 叠加层目录(便于排障时告知用户去哪看)。
    pub overlay_dir: String,
}

/// 「检查后端更新」的结果。字段与前端类型一一对应。
#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub available: bool,
    pub current: Option<String>,
    pub bundled: Option<String>,
    pub latest: Option<String>,
    pub using_overlay: bool,
    /// 生效版本来自哪里,用于拼提示文案。
    pub source: String,
    pub index_url: String,
    /// 主 wheel 体积(依赖的体积要解析后才知道)。
    pub wheel_size: Option<u64>,
}

impl UpdateInfo {
    /// 供对话框/托盘用的一行摘要。
    pub fn summary(&self) -> String {
        let cur = self.current.as_deref().unwrap_or("未知");
        if !self.available {
            return format!("后端已是最新版本(当前 {cur},{})", self.source);
        }
        let latest = self.latest.as_deref().unwrap_or("未知");
        match self.wheel_size {
            Some(size) => format!(
                "发现新后端 {latest}(当前 {cur},{})\n主程序包约 {:.1} MB,依赖按需下载。",
                self.source,
                size as f64 / 1_048_576.0
            ),
            None => format!("发现新后端 {latest}(当前 {cur},{})", self.source),
        }
    }
}

/// 热更新的阶段。
///
/// 用 enum 而不是 `&'static str`:托盘侧要**反序列化**这个载荷来改 tooltip,
/// 而 `&'static str` 的 `Deserialize` 要求输入缓冲活到 `'static`,
/// 从事件载荷里解析根本做不到。枚举同时还能限定合法取值集合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// 正在停后端。
    Stopping,
    /// 正在查 PyPI / 解析依赖。
    Resolving,
    /// 正在下载 wheel。
    Downloading,
    /// 正在解压 wheel。
    Extracting,
    /// 正在写激活状态。
    Activating,
    /// 正在重启后端。
    Restarting,
}

/// 进度载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub phase: Phase,
    /// 给人看的一句话。
    pub message: String,
    /// 当前是第几个包(从 1 开始)。
    pub index: usize,
    /// 共几个包。
    pub total: usize,
    pub downloaded: u64,
    pub bytes_total: Option<u64>,
}

impl Progress {
    /// 不带包计数的阶段提示。
    fn simple(phase: Phase, message: impl Into<String>) -> Self {
        Self {
            phase,
            message: message.into(),
            index: 0,
            total: 0,
            downloaded: 0,
            bytes_total: None,
        }
    }
}

/// 安装完成报告。
#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub version: String,
    /// 一并装上的依赖(`name==version`)。
    pub dependencies: Vec<String>,
    /// 非致命问题。
    pub warnings: Vec<String>,
    /// 实际下载字节数。
    pub downloaded_bytes: u64,
    /// 写入的文件数。
    pub files: usize,
}

// ---------------------------------------------------------------- 查询

/// 定位叠加层。挂在**实际生效的** home 下。
pub fn overlay_of() -> Overlay {
    Overlay::detect(Some(runtime::effective_home()))
}

/// 读当前版本分布。
pub fn versions(runner: &Runner) -> Versions {
    let bundled = runner.bundled();
    let bundled_ver = bundled.deeptutor_version();
    let ov = overlay_of();
    let overlay_ver = ov.effective_version(bundled_ver.as_deref());

    let (effective, using_overlay) = match &overlay_ver {
        Some(v) => (Some(v.clone()), true),
        None => (bundled_ver.clone(), false),
    };

    Versions {
        effective,
        bundled: bundled_ver,
        using_overlay,
        overlay_dir: ov.root().display().to_string(),
    }
}

/// 查 PyPI 上有没有比当前更新的稳定版后端。
pub async fn check(runner: &Runner) -> Result<UpdateInfo, String> {
    let v = versions(runner);
    let client = PypiClient::new();
    let info = client
        .fetch(PACKAGE)
        .await
        .map_err(|e| format!("查询 PyPI 失败: {e}"))?;

    // 只看稳定版:预发布/开发版不该被推荐给普通用户
    let latest: Option<(Version, ReleaseFile)> = info
        .files
        .iter()
        .filter_map(|(s, files)| Some((Version::from_str(s).ok()?, files)))
        .filter(|(ver, _)| ver.is_stable())
        .max_by(|a, b| a.0.cmp(&b.0))
        .and_then(|(ver, files)| files.first().map(|f| (ver, f.clone())));

    let current_parsed = v.effective.as_deref().and_then(|s| Version::from_str(s).ok());
    let (available, latest_str, wheel_size) = match &latest {
        Some((lver, file)) => (
            match &current_parsed {
                Some(cur) => lver > cur,
                None => true,
            },
            Some(lver.to_string()),
            Some(file.size),
        ),
        None => (false, None, None),
    };

    Ok(UpdateInfo {
        available,
        current: v.effective,
        bundled: v.bundled,
        latest: latest_str,
        using_overlay: v.using_overlay,
        source: if v.using_overlay {
            "来自热更新".to_string()
        } else {
            "来自安装包内置".to_string()
        },
        index_url: client.index_url().to_string(),
        wheel_size,
    })
}

// ---------------------------------------------------------------- 安装

/// 一次热更新过程中反复要用到的上下文。
///
/// 打成结构体而不是一串函数参数 —— 流程里要传七八样东西,平铺出来
/// 既难看也容易在调用点错位。
struct Ctx {
    app: AppHandle,
    runner: Arc<Runner>,
    ov: Overlay,
    /// 内置版(用于「内置反超」判定与状态记录)。
    bundled_ver: Option<String>,
    /// 实际生效的 deeptutor 版本(用于「无需更新」判定)。
    current_ver: Option<String>,
    client: PypiClient,
    tag: InterpTag,
    /// 内置解释器的完整版本,如 `3.13.15`。
    py_version: String,
    /// deeptutor 数据根目录(实际生效的那个)。
    home: PathBuf,
    http: reqwest::Client,
}

/// 执行一次后端热更新。
///
/// 中途失败也会重启后端(否则应用会停在"后端未就绪"),所以拿到的
/// `Err` 不意味着环境被破坏;叠加层状态只在全部校验通过后才写入。
pub async fn install(app: &AppHandle) -> Result<InstallReport, String> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("已有后端更新任务在执行,请稍候".to_string());
    }
    let result = install_inner(app).await;
    RUNNING.store(false, Ordering::SeqCst);
    result
}

async fn install_inner(app: &AppHandle) -> Result<InstallReport, String> {
    let runner = app
        .try_state::<Arc<Runner>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| "后端管理器未就绪".to_string())?;

    let ov = overlay_of();
    let bundled_rt = runner.bundled();
    let bundled_ver = bundled_rt.deeptutor_version();
    let home = runtime::effective_home();

    log::info!(
        "开始后端热更新:内置版 {:?},叠加层 {}",
        bundled_ver,
        ov.root().display()
    );

    // ---- 1) 定位解释器(拿版本用于选 wheel / marker 求值) ----
    let cfg = BackendConfig::from_env();
    let py = python::locate(cfg.min_python, Some(&bundled_rt))
        .await
        .map_err(|e| format!("定位 Python 解释器失败: {e}"))?;
    let py_version = py.display_version();
    let tag = InterpTag::from_python_version(&py_version);
    log::info!("目标解释器 {py_version}(wheel ABI 标签 {})", tag.abi_tag);

    let http = reqwest::Client::builder()
        .user_agent(concat!("DeepTutorDesktop/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(600))
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| format!("初始化 HTTP 客户端失败: {e}"))?;

    let client = PypiClient::new();
    let current_ver = ov
        .effective_version(bundled_ver.as_deref())
        .or_else(|| bundled_ver.clone());

    let ctx = Ctx {
        app: app.clone(),
        runner: runner.clone(),
        ov,
        bundled_ver,
        current_ver,
        client,
        tag,
        py_version,
        home,
        http,
    };

    // ---- 2) 查 PyPI 定目标版本 ----
    emit(
        app,
        Progress::simple(Phase::Resolving, "正在查询 PyPI…"),
    );

    let info = ctx
        .client
        .fetch(PACKAGE)
        .await
        .map_err(|e| format!("查询 PyPI 失败: {e}"))?;

    let (target, main_file) = info
        .pick_best(&ctx.tag, |v| v.is_stable())
        .ok_or_else(|| {
            format!(
                "PyPI 上找不到适配 {} / {} 的稳定版 {PACKAGE}",
                ctx.py_version, ctx.tag.platform
            )
        })?;
    let target_str = target.to_string();

    if ctx
        .current_ver
        .as_deref()
        .and_then(|s| Version::from_str(s).ok())
        .as_ref()
        == Some(&target)
    {
        return Err(format!("当前已是 {target_str},无需更新"));
    }

    log::info!("目标版本 {target_str},主包 {}", main_file.filename);

    // ---- 3) 停后端 ----
    emit(&ctx.app, Progress::simple(Phase::Stopping, "正在停止后端…"));
    if let Err(e) = ctx.runner.stop().await {
        log::warn!("停止后端失败,继续升级(新版本在重启后生效): {e}");
    }

    // ---- 4) 安装主体 ----
    let outcome = do_install(&ctx, &target_str, &main_file).await;

    // ---- 5) 无论成败都重启后端 ----
    emit(&ctx.app, Progress::simple(Phase::Restarting, "正在重启后端…"));
    restart_and_refresh(&ctx.app, &ctx.runner);

    outcome
}

/// 重启后端,并在就绪后刷新 WebView。
///
/// 刷新那一步是必须的:热更新把后端停掉时,WebView 里那个 DeepTutor 界面
/// 是在**旧后端**上加载的 —— 连接一断界面就满屏报错,而它自己不会恢复
/// (它早就加载完了,收不到启动页那套 `backend://state` 事件)。
fn restart_and_refresh(app: &AppHandle, runner: &Arc<Runner>) {
    let handle = app.clone();
    let runner = runner.clone();
    tauri::async_runtime::spawn(async move {
        boot::run(handle.clone(), runner).await;
        reload_web_ui_if_on_it(&handle);
    });
}

/// 若 WebView 正停在 DeepTutor Web UI 上,刷新它。
///
/// 只刷「端口对得上」的页面:如果用户此刻看的是启动页(还没被导航走),
/// 那就什么都不做 —— 启动页有自己的状态机,刷它会打断流程。
fn reload_web_ui_if_on_it(app: &AppHandle) {
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    let Ok(url) = win.url() else {
        return;
    };
    if url.port() != Some(BackendConfig::from_env().web_port) {
        return;
    }
    log::info!("后端已重启,刷新停在 {url} 的界面");
    let _ = win.eval("window.location.reload()");
}

/// 安装主体(下载 -> 解析依赖 -> 解压 -> 激活)。
async fn do_install(ctx: &Ctx, target_str: &str, main_file: &ReleaseFile) -> Result<InstallReport, String> {
    let app = &ctx.app;
    let mut downloaded_bytes: u64 = 0;

    // ---- 下载主包 ----
    let main_path = fetch_wheel(app, &ctx.http, &ctx.ov, main_file, 1, 1, PACKAGE).await?;
    downloaded_bytes += main_file.size;

    // ---- 读依赖声明 ----
    let md = wheel::read_metadata(&main_path)
        .map_err(|e| format!("读取 {PACKAGE} 元数据失败: {e}"))?;
    log::info!(
        "{} {} 声明了 {} 条依赖声明(含 extra 标记)",
        md.name,
        md.version,
        md.requires_dist.len()
    );

    // ---- 依赖解析 ----
    emit(
        app,
        Progress::simple(Phase::Resolving, format!("正在解析 {} 的依赖…", md.name)),
    );

    let marker_env = resolve::marker_environment(&ctx.py_version)
        .map_err(|e| format!("构造环境标记求值器失败: {e}"))?;

    let mut scan_dirs: Vec<PathBuf> = Vec::new();
    if let Some(sp) = ctx.runner.bundled().site_packages_dir() {
        scan_dirs.push(sp);
    }
    if let Some(dir) = ctx.ov.effective_dir(ctx.bundled_ver.as_deref()) {
        scan_dirs.push(dir);
    }
    let installed = InstalledEnv::scan(&scan_dirs);
    log::info!("已装环境扫描到 {} 个包", installed.len());

    let plan = resolve::plan(
        &ctx.client,
        &ctx.tag,
        &marker_env,
        &installed,
        &md.requires_dist,
        PACKAGE,
    )
    .await
    .map_err(|e| format!("依赖解析失败: {e}"))?;

    log::info!(
        "依赖解析完成:需装 {} 个,已满足 {} 个,警告 {} 条",
        plan.items.len(),
        plan.already_satisfied,
        plan.warnings.len()
    );
    for w in &plan.warnings {
        log::warn!("依赖警告: {w}");
    }

    // ---- 下载依赖 ----
    let total = plan.items.len();
    let mut dep_paths: Vec<(PathBuf, String)> = Vec::with_capacity(total);
    for (i, item) in plan.items.iter().enumerate() {
        let dest = fetch_wheel(
            app,
            &ctx.http,
            &ctx.ov,
            &item.file,
            i + 1,
            total,
            &format!("{} {}", item.name, item.version),
        )
        .await?;
        downloaded_bytes += item.file.size;
        dep_paths.push((dest, item.name.clone()));
    }

    // ---- 解压 ----
    emit(
        app,
        Progress {
            phase: Phase::Extracting,
            message: "正在解压…".into(),
            index: 0,
            total: total + 1,
            downloaded: downloaded_bytes,
            bytes_total: None,
        },
    );

    let dest_dir = ctx.ov.version_dir(target_str);
    let mut files = 0usize;

    // 依赖先解、本体后解:万一出现同名文件,本体应当胜出
    for (path, name) in &dep_paths {
        let stats = wheel::extract_to(path, &dest_dir, |_| {})
            .map_err(|e| format!("解压 {name} 失败: {e}"))?;
        files += stats.files;
    }
    let main_stats = wheel::extract_to(&main_path, &dest_dir, |_| {})
        .map_err(|e| format!("解压 {PACKAGE} 失败: {e}"))?;
    files += main_stats.files;
    log::info!("解压完成:{files} 个文件 -> {}", dest_dir.display());

    // ---- 校验解压结果(激活前的最后一道闸) ----
    match runtime::read_deeptutor_version(&dest_dir) {
        Some(v) if v == target_str => {}
        Some(v) => {
            return Err(format!(
                "解压后的版本号是 {v},与目标 {target_str} 不一致,已放弃激活"
            ))
        }
        None => {
            return Err(format!(
                "解压目录里找不到 deeptutor/__version__.py,包结构异常: {}",
                dest_dir.display()
            ))
        }
    }

    // ---- 激活 ----
    emit(
        app,
        Progress {
            phase: Phase::Activating,
            message: "正在激活新版本…".into(),
            index: total + 1,
            total: total + 1,
            downloaded: downloaded_bytes,
            bytes_total: None,
        },
    );

    let dependencies: Vec<String> = plan
        .items
        .iter()
        .map(|i| format!("{}=={}", i.name, i.version))
        .collect();

    ctx.ov
        .write_state(&OverlayState {
            active: Some(target_str.to_string()),
            activated_at: Some(chrono::Utc::now().to_rfc3339()),
            packages: dependencies.clone(),
            base_bundled: ctx.bundled_ver.clone(),
        })
        .map_err(|e| format!("写入激活状态失败: {e}"))?;

    // ---- 前端运行时缓存失效 ----
    invalidate_web_cache(&ctx.home);

    log::info!("后端已切换到 {target_str}");

    Ok(InstallReport {
        version: target_str.to_string(),
        dependencies,
        warnings: plan.warnings,
        downloaded_bytes,
        files,
    })
}

/// 回退到安装包内置版本。
///
/// 只改状态文件、不动任何包 —— 所以这一步几乎不可能失败。
pub async fn rollback(app: &AppHandle) -> Result<String, String> {
    let runner = app
        .try_state::<Arc<Runner>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| "后端管理器未就绪".to_string())?;

    let ov = overlay_of();
    let Some(prev) = ov.state().active.clone() else {
        return Err("当前本就在使用安装包内置版本,无需回退".to_string());
    };

    if let Err(e) = runner.stop().await {
        log::warn!("回退前停止后端失败(继续): {e}");
    }

    ov.deactivate()
        .map_err(|e| format!("写入回退状态失败: {e}"))?;
    invalidate_web_cache(&runtime::effective_home());

    restart_and_refresh(app, &runner);

    let back = runner.bundled().deeptutor_version().unwrap_or_default();
    log::info!("已从 {prev} 回退到内置版 {back}");
    Ok(format!("已回退到安装包内置版本 {back}"))
}

// ---------------------------------------------------------------- 辅助

/// 下载一个 wheel(带缓存命中)。返回本地路径。
async fn fetch_wheel(
    app: &AppHandle,
    http: &reqwest::Client,
    ov: &Overlay,
    file: &ReleaseFile,
    index: usize,
    total: usize,
    label: &str,
) -> Result<PathBuf, String> {
    let dest = ov.wheels_dir().join(wheel::cache_filename(file));

    // 缓存命中:文件在、且 sha256 对得上
    if dest.is_file() {
        if let Ok(h) = wheel::sha256_file(&dest) {
            if h.eq_ignore_ascii_case(&file.sha256) {
                log::info!("{label} 命中 wheel 缓存");
                emit(
                    app,
                    Progress {
                        phase: Phase::Downloading,
                        message: format!("{label} 已在本地缓存"),
                        index,
                        total,
                        downloaded: file.size,
                        bytes_total: Some(file.size),
                    },
                );
                return Ok(dest);
            }
        }
        log::warn!("{label} 缓存校验不过,重新下载");
    }

    let mut last_pct: i64 = -1;
    let label_owned = label.to_string();
    wheel::download_verified(http, &file.url, &file.sha256, &dest, |done, bytes_total| {
        let pct = bytes_total
            .filter(|t| *t > 0)
            .map(|t| (done.saturating_mul(100) / t) as i64)
            .unwrap_or(-1);
        // 按整数百分比节流:一次下载会回调上千次
        if pct == last_pct {
            return;
        }
        last_pct = pct;
        let text = match (pct, bytes_total) {
            (p, Some(t)) if p >= 0 => format!(
                "{label_owned}  {p}%  ({:.1}/{:.1} MB)",
                done as f64 / 1_048_576.0,
                t as f64 / 1_048_576.0
            ),
            _ => format!("{label_owned}  {:.1} MB", done as f64 / 1_048_576.0),
        };
        emit(
            app,
            Progress {
                phase: Phase::Downloading,
                message: text,
                index,
                total,
                downloaded: done,
                bytes_total,
            },
        );
    })
    .await
    .map_err(|e| format!("{label} 下载失败: {e}"))?;

    Ok(dest)
}

/// 让 deeptutor 的前端运行时缓存失效。
///
/// launcher(`deeptutor/runtime/launcher.py`)用
/// `<home>/data/user/runtime/web/.deeptutor-web-runtime.json` 里的
/// `source_mtime_ns` 与打包源比对,一致就复用,否则删掉整个缓存重新拷贝。
/// 热更新后 `deeptutor_web` 的物理路径与 mtime 都变了,**理论上**会自动重建
/// —— 但这里主动删掉 marker,把"理论上"变成"必然",代价只是一个小 JSON。
fn invalidate_web_cache(home: &Path) {
    let marker = home
        .join("data")
        .join("user")
        .join("runtime")
        .join("web")
        .join(".deeptutor-web-runtime.json");
    match std::fs::remove_file(&marker) {
        Ok(()) => log::info!("已清除前端运行时缓存标记,重启后将重建"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("清除前端缓存标记失败(通常会自行重建): {e}"),
    }
}

fn emit(app: &AppHandle, p: Progress) {
    let _ = app.emit(EVENT_PROGRESS, p);
}

// ---------------------------------------------------------------- 静默检查

/// 有可用后端更新时广播的事件。前端据此显示提示(不自动安装)。
pub const EVENT_AVAILABLE: &str = "backend-update://available";

/// 启动后延迟一段时间,静默检查一次后端更新。
///
/// 有新版时只做两件事:**改托盘 tooltip** + **广播事件**。不弹模态框、
/// 不自动下载 —— 用户明确选择的是「自动查 + 手动装」。
///
/// 为什么要延迟:应用刚起来时 CPU 与网络都在为拉起后端服务,
/// 这时候再叠一层 PyPI 请求会拖慢真正重要的启动流程。
pub fn spawn_auto_check(app: &AppHandle) {
    const DELAY: std::time::Duration = std::time::Duration::from_secs(30);

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(DELAY).await;

        let Some(runner) = handle.try_state::<Arc<Runner>>().map(|s| s.inner().clone()) else {
            return;
        };

        // 后端自己都没起来,就别跟着凑热闹了(它可能正在失败重试)
        if !matches!(runner.state(), crate::backend::runner::State::Ready) {
            log::info!("后端未就绪,跳过自动检查后端更新");
            return;
        }

        match check(&runner).await {
            Ok(info) if info.available => {
                let latest = info.latest.as_deref().unwrap_or("未知");
                log::info!(
                    "发现新后端版本 {latest}(当前 {})",
                    info.current.as_deref().unwrap_or("未知")
                );
                crate::tray::set_tooltip(
                    &handle,
                    &format!("DeepTutor — 有新后端 v{latest}(右键托盘可更新)"),
                );
                let _ = handle.emit(EVENT_AVAILABLE, &info);
            }
            Ok(_) => log::info!("后端已是最新版本"),
            Err(e) => log::warn!("自动检查后端更新失败(忽略): {e}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(available: bool, current: &str, latest: Option<&str>) -> UpdateInfo {
        UpdateInfo {
            available,
            current: Some(current.into()),
            bundled: Some("1.6.12".into()),
            latest: latest.map(String::from),
            using_overlay: false,
            source: "来自安装包内置".into(),
            index_url: "https://pypi.org".into(),
            wheel_size: Some(38_000_000),
        }
    }

    #[test]
    fn summary_when_up_to_date() {
        let s = info(false, "1.6.12", None).summary();
        assert!(s.contains("1.6.12"));
        assert!(s.contains("最新"));
    }

    #[test]
    fn summary_when_update_available() {
        let s = info(true, "1.6.12", Some("1.6.13")).summary();
        assert!(s.contains("1.6.13"));
        assert!(s.contains("36.2 MB"));
    }

    #[test]
    fn progress_simple_has_no_counts() {
        let p = Progress::simple(Phase::Resolving, "hello");
        assert_eq!(p.index, 0);
        assert_eq!(p.total, 0);
        assert_eq!(p.bytes_total, None);
    }

    #[test]
    fn phase_serializes_lowercase() {
        let p = Progress::simple(Phase::Downloading, "x");
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains(r#""phase":"downloading""#), "got {json}");
        // 能原样反序列化回来(托盘侧就是这么用的)
        let back: Progress = serde_json::from_str(&json).unwrap();
        assert_eq!(back.phase, Phase::Downloading);
    }
}
