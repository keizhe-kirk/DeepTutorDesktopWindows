//! Python 解释器探测与依赖检查。
//!
//! 探测顺序:`DEEPTUTOR_PYTHON` -> **安装包内置解释器(runtimes/python)**
//! -> PATH 中的 python/python3 -> Windows `py -3.xx` launcher ->
//! `%LOCALAPPDATA%\Programs\Python\Python3xx` -> 盘符根安装路径。
//! 每个候选都实际执行一次 `python -c "..."` 读取版本号,要求 >= 3.11。

use serde::Serialize;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::timeout;

use super::runtime::BundledRuntimes;

/// 单次探测(版本读取 / 依赖检查)的超时时间。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// 一个可用的 Python 解释器。
///
/// `prefix` 用于在 Windows py launcher 场景保留 `-3.13` 之类的选择参数,
/// 后续所有子进程调用都要带上它,否则版本可能对不上。
#[derive(Debug, Clone, Serialize)]
pub struct PythonLocator {
    pub executable: PathBuf,
    #[serde(default)]
    pub prefix: Vec<String>,
    pub version: (u32, u32, u32),
    /// 该解释器是否来自安装包内置的运行时(runtimes/python)。
    #[serde(default)]
    pub bundled: bool,
}

impl PythonLocator {
    pub fn display_version(&self) -> String {
        format!("{}.{}.{}", self.version.0, self.version.1, self.version.2)
    }

    /// 供日志展示的来源标签。
    pub fn source_label(&self) -> &'static str {
        if self.bundled {
            "安装包内置运行时"
        } else {
            "系统 Python"
        }
    }
}

/// venv 目录布局规格(供 bootstrap 脚本使用)。
#[derive(Debug, Clone)]
pub struct VenvSpec {
    pub root: PathBuf,
    pub python_exe: PathBuf,
}

impl VenvSpec {
    pub fn for_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let python_exe = if cfg!(windows) {
            root.join(".venv").join("Scripts").join("python.exe")
        } else {
            root.join(".venv").join("bin").join("python")
        };
        Self {
            root: root.join(".venv"),
            python_exe,
        }
    }
}

/// 一个候选解释器。
struct Candidate {
    executable: PathBuf,
    prefix: Vec<String>,
    /// 是否来自安装包内置运行时。
    bundled: bool,
}

impl Candidate {
    fn plain(executable: PathBuf) -> Self {
        Self {
            executable,
            prefix: Vec::new(),
            bundled: false,
        }
    }
}

/// 收集所有可能的解释器候选(带可选的 launcher 前缀参数)。
fn candidates(bundled: Option<&BundledRuntimes>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();

    // 1. 显式指定优先(排障逃生口,允许开发期指向任意解释器)
    if let Ok(p) = std::env::var("DEEPTUTOR_PYTHON") {
        let p = p.trim().trim_matches('"').to_string();
        if !p.is_empty() {
            out.push(Candidate::plain(PathBuf::from(p)));
        }
    }

    // 2. 安装包内置运行时 —— 自包含安装包的主路径,必须排在系统 Python 之前
    if let Some(py) = bundled.and_then(|b| b.python_exe()) {
        out.push(Candidate {
            executable: py,
            prefix: Vec::new(),
            bundled: true,
        });
    }

    // 3. PATH (跳过 WindowsApps stub)
    for name in ["python", "python3"] {
        if let Ok(p) = which::which(name) {
            let p_str = p.to_string_lossy();
            // 跳过 WindowsApps 下的 stub
            if p_str.contains("WindowsApps") {
                continue;
            }
            out.push(Candidate::plain(p));
        }
    }

    if cfg!(windows) {
        // 4. Windows py launcher(带版本选择参数)
        if let Ok(py) = which::which("py") {
            for ver in ["3.14", "3.13", "3.12", "3.11"] {
                out.push(Candidate {
                    executable: py.clone(),
                    prefix: vec![format!("-{}", ver)],
                    bundled: false,
                });
            }
            out.push(Candidate {
                executable: py,
                prefix: vec!["-3".to_string()],
                bundled: false,
            });
        }

        // 5. 用户级安装目录
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            for dir in ["Python314", "Python313", "Python312", "Python311"] {
                let p = PathBuf::from(&local)
                    .join("Programs")
                    .join("Python")
                    .join(dir)
                    .join("python.exe");
                if p.exists() {
                    out.push(Candidate::plain(p));
                }
            }
        }

        // 6. 盘符根安装
        for p in [
            r"C:\Python314\python.exe",
            r"C:\Python313\python.exe",
            r"C:\Python312\python.exe",
            r"C:\Python311\python.exe",
        ] {
            let p = PathBuf::from(p);
            if p.exists() {
                out.push(Candidate::plain(p));
            }
        }
    } else {
        for name in ["python3.14", "python3.13", "python3.12", "python3.11"] {
            if let Ok(p) = which::which(name) {
                out.push(Candidate::plain(p));
            }
        }
    }

    out
}

fn parse_version(raw: &str) -> Option<(u32, u32, u32)> {
    let s = raw.trim();
    let mut parts = s.split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next()?.trim().parse().ok()?;
    let patch = parts
        .next()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(0);
    Some((major, minor, patch))
}

/// 实际跑一次 `-c` 读取版本。
async fn probe_version(executable: &std::path::Path, prefix: &[String]) -> Option<(u32, u32, u32)> {
    let mut cmd = Command::new(executable);
    cmd.args(prefix);
    cmd.arg("-c")
        .arg("import sys;print('%d.%d.%d' % sys.version_info[:3])");
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let out = timeout(PROBE_TIMEOUT, cmd.output()).await.ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

/// 查找满足最低版本的 Python 解释器。
///
/// `bundled` 为安装包内置运行时;存在时其解释器优先于任何系统 Python。
pub async fn locate(
    min: (u32, u32),
    bundled: Option<&BundledRuntimes>,
) -> anyhow::Result<PythonLocator> {
    let mut tried: Vec<String> = Vec::new();

    for candidate in candidates(bundled) {
        let tag = if candidate.bundled {
            "[内置] "
        } else {
            ""
        };
        match probe_version(&candidate.executable, &candidate.prefix).await {
            Some(version) if (version.0, version.1) >= min => {
                return Ok(PythonLocator {
                    executable: candidate.executable,
                    prefix: candidate.prefix,
                    version,
                    bundled: candidate.bundled,
                });
            }
            Some(version) => tried.push(format!(
                "{}{} -> Python {}.{}.{} (低于要求 {}.{})",
                tag,
                candidate.executable.display(),
                version.0,
                version.1,
                version.2,
                min.0,
                min.1
            )),
            None => tried.push(format!(
                "{}{} -> 无法执行",
                tag,
                candidate.executable.display()
            )),
        }
    }

    Err(anyhow::anyhow!(
        "未找到 Python >= {}.{} 的解释器。已尝试:\n  {}",
        min.0,
        min.1,
        if tried.is_empty() {
            "(没有任何候选)".to_string()
        } else {
            tried.join("\n  ")
        }
    ))
}

/// 检查 deeptutor 包在当前解释器里是否可导入。
pub async fn is_deeptutor_installed(py: &PythonLocator) -> bool {
    let mut cmd = Command::new(&py.executable);
    cmd.args(&py.prefix);
    cmd.arg("-c").arg(
        "import importlib.util as u; raise SystemExit(0 if u.find_spec('deeptutor') else 1)",
    );
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    matches!(timeout(PROBE_TIMEOUT, cmd.status()).await, Ok(Ok(s)) if s.success())
}
