//! Python 解释器探测与依赖检查。
//!
//! 探测顺序:`DEEPTUTOR_PYTHON` -> PATH 中的 python/python3 -> Windows `py -3.xx`
//! launcher -> `%LOCALAPPDATA%\Programs\Python\Python3xx` -> 盘符根安装路径。
//! 每个候选都实际执行一次 `python -c "..."` 读取版本号,要求 >= 3.11。

use serde::Serialize;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::timeout;

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
}

impl PythonLocator {
    pub fn display_version(&self) -> String {
        format!("{}.{}.{}", self.version.0, self.version.1, self.version.2)
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

/// 收集所有可能的解释器候选(带可选的 launcher 前缀参数)。
fn candidates() -> Vec<(PathBuf, Vec<String>)> {
    let mut out: Vec<(PathBuf, Vec<String>)> = Vec::new();

    // 1. 显式指定优先
    if let Ok(p) = std::env::var("DEEPTUTOR_PYTHON") {
        let p = p.trim().trim_matches('"').to_string();
        if !p.is_empty() {
            out.push((PathBuf::from(p), Vec::new()));
        }
    }

    // 2. PATH (跳过 WindowsApps stub)
    for name in ["python", "python3"] {
        if let Ok(p) = which::which(name) {
            let p_str = p.to_string_lossy();
            // 跳过 WindowsApps 下的 stub
            if p_str.contains("WindowsApps") {
                continue;
            }
            out.push((p, Vec::new()));
        }
    }

    if cfg!(windows) {
        // 3. Windows py launcher(带版本选择参数)
        if let Ok(py) = which::which("py") {
            for ver in ["3.14", "3.13", "3.12", "3.11"] {
                out.push((py.clone(), vec![format!("-{}", ver)]));
            }
            out.push((py, vec!["-3".to_string()]));
        }

        // 4. 用户级安装目录
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            for dir in ["Python314", "Python313", "Python312", "Python311"] {
                let p = PathBuf::from(&local)
                    .join("Programs")
                    .join("Python")
                    .join(dir)
                    .join("python.exe");
                if p.exists() {
                    out.push((p, Vec::new()));
                }
            }
        }

        // 5. 盘符根安装
        for p in [
            r"C:\Python314\python.exe",
            r"C:\Python313\python.exe",
            r"C:\Python312\python.exe",
            r"C:\Python311\python.exe",
        ] {
            let p = PathBuf::from(p);
            if p.exists() {
                out.push((p, Vec::new()));
            }
        }
    } else {
        for name in ["python3.14", "python3.13", "python3.12", "python3.11"] {
            if let Ok(p) = which::which(name) {
                out.push((p, Vec::new()));
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
pub async fn locate(min: (u32, u32)) -> anyhow::Result<PythonLocator> {
    let mut tried: Vec<String> = Vec::new();

    for (executable, prefix) in candidates() {
        match probe_version(&executable, &prefix).await {
            Some(version) if (version.0, version.1) >= min => {
                return Ok(PythonLocator {
                    executable,
                    prefix,
                    version,
                });
            }
            Some(version) => tried.push(format!(
                "{} -> Python {}.{}.{} (低于要求 {}.{})",
                executable.display(),
                version.0,
                version.1,
                version.2,
                min.0,
                min.1
            )),
            None => tried.push(format!("{} -> 无法执行", executable.display())),
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
