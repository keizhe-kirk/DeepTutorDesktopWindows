//! 后端启动配置。
//!
//! 所有字段都可以用环境变量覆盖,目的有三个:
//! 1. 用户可以在不重新打包的情况下指向自定义解释器 / 自定义启动命令
//! 2. CI 可以用 mock 脚本跑端到端验证(见 `scripts/mock-backend.py`)
//! 3. DeepTutor 主项目改命令时,壳层不需要重新编译

use std::path::PathBuf;
use std::time::Duration;

use super::health::Ports;

/// 环境变量读取辅助。
fn env_str(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn flag_value(raw: Option<String>) -> bool {
    matches!(
        raw.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

fn env_flag(key: &str) -> bool {
    flag_value(std::env::var(key).ok())
}

fn env_u16(key: &str, default: u16) -> u16 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

#[derive(Debug, Clone)]
pub struct BackendConfig {
    /// 显式指定 Python 解释器(覆盖自动探测)。
    pub python: Option<PathBuf>,
    /// Python 模块名,默认 `deeptutor`(即 `python -m deeptutor`)。
    pub module: String,
    /// 传给模块的参数,默认 `start --child`。
    pub args: Vec<String>,
    /// 直接运行某个脚本而不是 `-m module`(mock / 排障用)。
    pub script: Option<PathBuf>,
    /// FastAPI 端口。
    pub api_port: u16,
    /// Next.js standalone 端口。
    pub web_port: u16,
    /// 从子进程拉起到两个端口都就绪的最长等待时间。
    pub startup_timeout: Duration,
    /// Python 最低版本要求。
    pub min_python: (u32, u32),
    /// 跳过 deeptutor 导入检查(开发调试 / CI mock 用)。
    pub skip_dep_check: bool,
    /// 透传给子进程的额外环境变量。
    pub extra_env: Vec<(String, String)>,
}

impl BackendConfig {
    pub fn from_env() -> Self {
        Self {
            python: env_path("DEEPTUTOR_PYTHON"),
            module: env_str("DEEPTUTOR_MODULE", "deeptutor"),
            args: env_str("DEEPTUTOR_ARGS", "start --child")
                .split_whitespace()
                .map(|s| s.to_string())
                .collect(),
            script: env_path("DEEPTUTOR_BACKEND_SCRIPT"),
            api_port: env_u16("DEEPTUTOR_API_PORT", 8001),
            web_port: env_u16("DEEPTUTOR_WEB_PORT", 3782),
            startup_timeout: Duration::from_millis(env_u64(
                "DEEPTUTOR_STARTUP_TIMEOUT_MS",
                120_000,
            )),
            min_python: (3, 11),
            skip_dep_check: env_flag("DEEPTUTOR_SKIP_DEP_CHECK"),
            extra_env: Vec::new(),
        }
    }

    pub fn ports(&self) -> Ports {
        Ports {
            api: self.api_port,
            web: self.web_port,
        }
    }

    /// 拼出实际要执行的命令行(仅用于日志展示与错误提示)。
    pub fn describe(&self, python: &std::path::Path) -> String {
        let mut parts = vec![python.display().to_string()];
        match &self.script {
            Some(s) => parts.push(s.display().to_string()),
            None => {
                parts.push("-m".to_string());
                parts.push(self.module.clone());
            }
        }
        parts.extend(self.args.iter().cloned());
        parts.join(" ")
    }
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self::from_env()
    }
}
