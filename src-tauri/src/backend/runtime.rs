//! 内置运行时(自包含安装包)路径解析。
//!
//! 安装包携带的目录布局(Tauri 资源目录下):
//!
//! ```text
//! runtimes/
//!   python/python.exe     relocatable CPython + deeptutor 及其全部依赖
//!   node/node.exe         内置 Node.js(deeptutor 起前端时 `shutil.which("node")` 命中它)
//! ```
//!
//! 解析优先级:
//! 1. 环境变量 `DEEPTUTOR_RUNTIMES`(开发 / 排障 / 手工指定)
//! 2. Tauri 资源目录下的 `runtimes/`(正式安装后的位置)
//!
//! 两者都不存在时返回空值,壳层退回"使用系统 Python / Node"的旧行为 ——
//! 这样 `tauri dev` 和 CI 里的 mock 后端仍然可用。

use std::path::{Path, PathBuf};

/// 覆盖内置运行时目录的环境变量。
pub const ENV_RUNTIMES_DIR: &str = "DEEPTUTOR_RUNTIMES";

/// deeptutor 运行时数据根目录的环境变量。
///
/// 未设置时 deeptutor 会把数据写到"当前工作目录/data",在安装版里会把
/// `data/` 撒到用户主目录。壳层会把它显式指向 `%LOCALAPPDATA%\DeepTutor`。
pub const ENV_DEEPTUTOR_HOME: &str = "DEEPTUTOR_HOME";

#[cfg(windows)]
const PYTHON_RELATIVE: &str = "python/python.exe";
#[cfg(not(windows))]
const PYTHON_RELATIVE: &str = "python/bin/python3";

#[cfg(windows)]
const NODE_RELATIVE: &str = "node";
#[cfg(not(windows))]
const NODE_RELATIVE: &str = "node/bin";

/// 已解析到的内置运行时集合。
#[derive(Debug, Clone, Default)]
pub struct BundledRuntimes {
    root: Option<PathBuf>,
}

impl BundledRuntimes {
    /// 探测内置运行时。`resource_dir` 来自 `app.path().resource_dir()`。
    ///
    /// 因为不同打包目标对 `bundle.resources` 的落点略有差异,这里按顺序试几个
    /// 候选目录,命中即止。顺序上优先信任显式配置,最后才猜路径。
    pub fn detect(resource_dir: Option<PathBuf>) -> Self {
        if let Some(root) = dir_from_env() {
            return Self { root: Some(root) };
        }

        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(dir) = resource_dir.as_ref() {
            // tauri.conf.json 里 "runtimes/": "runtimes/" 的常规落点
            roots.push(dir.join("runtimes"));
            // 少数打包后端会把 resources/ 整体嵌一层
            roots.push(dir.join("resources").join("runtimes"));
        }
        // 兜底:与可执行文件同级(绿色版 / 手工解压场景)
        if let Ok(exe) = std::env::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                roots.push(exe_dir.join("runtimes"));
                if let Some(parent) = exe_dir.parent() {
                    roots.push(parent.join("runtimes"));
                }
            }
        }

        for candidate in roots {
            if candidate.is_dir() {
                return Self {
                    root: Some(candidate),
                };
            }
        }

        Self { root: None }
    }

    /// 内置运行时根目录(`runtimes/`)。
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// 内置 Python 解释器。不存在时返回 `None`。
    pub fn python_exe(&self) -> Option<PathBuf> {
        let candidate = self.root.as_ref()?.join(PYTHON_RELATIVE);
        candidate.is_file().then_some(candidate)
    }

    /// 内置 Node 所在目录(用于前置到子进程 PATH)。
    pub fn node_dir(&self) -> Option<PathBuf> {
        let candidate = self.root.as_ref()?.join(NODE_RELATIVE);
        candidate.is_dir().then_some(candidate)
    }

    /// 内置 Node 可执行文件。
    pub fn node_exe(&self) -> Option<PathBuf> {
        let dir = self.node_dir()?;
        let candidate = if cfg!(windows) {
            dir.join("node.exe")
        } else {
            dir.join("node")
        };
        candidate.is_file().then_some(candidate)
    }

    /// 是否 Python 与 Node 都齐备(齐备才算真正自包含)。
    pub fn is_complete(&self) -> bool {
        self.python_exe().is_some() && self.node_exe().is_some()
    }

    /// 供日志展示的一句话描述。
    pub fn describe(&self) -> String {
        match (self.python_exe(), self.node_exe()) {
            (Some(py), Some(node)) => format!(
                "内置运行时就绪 (python: {}, node: {})",
                py.display(),
                node.display()
            ),
            (Some(py), None) => format!(
                "内置 Python 就绪 ({}) 但缺少内置 Node,前端可能起不来",
                py.display()
            ),
            (None, Some(node)) => format!(
                "内置 Node 就绪 ({}) 但缺少内置 Python",
                node.display()
            ),
            (None, None) => "未检测到内置运行时,将回退到系统 Python / Node".to_string(),
        }
    }
}

fn dir_from_env() -> Option<PathBuf> {
    let raw = std::env::var(ENV_RUNTIMES_DIR).ok()?;
    let trimmed = raw.trim().trim_matches('"');
    if trimmed.is_empty() {
        return None;
    }
    let path = PathBuf::from(trimmed);
    path.is_dir().then_some(path)
}

/// deeptutor 运行时数据根目录(即 `DEEPTUTOR_HOME`)。
///
/// - 用户已显式设置 `DEEPTUTOR_HOME` 时尊重用户的选择,返回 `None`(表示不要覆盖)。
/// - 否则返回 `<LOCALAPPDATA>\DeepTutor`(`dirs::data_local_dir()`)。
pub fn runtime_home_override() -> Option<PathBuf> {
    if std::env::var(ENV_DEEPTUTOR_HOME)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        return None;
    }
    dirs::data_local_dir().map(|dir| dir.join("DeepTutor"))
}
