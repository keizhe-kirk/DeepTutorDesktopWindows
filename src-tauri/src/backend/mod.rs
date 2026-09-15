//! Backend 模块:管理 DeepTutor Python 后端子进程。
//!
//! - `config`  : 启动配置(全部可用环境变量覆盖)
//! - `runtime` : 内置运行时(自包含安装包携带的 Python / Node)路径解析
//! - `python`  : 解释器探测与依赖检查
//! - `runner`  : 子进程拉起 / 日志回流 / 生命周期
//! - `health`  : HTTP 健康探测
//! - `boot`    : 把上述步骤串成状态机的启动编排

pub mod config;
pub mod runtime;
pub mod python;
pub mod runner;
pub mod health;
pub mod boot;

pub use runner::Runner;
pub use runtime::BundledRuntimes;
pub use python::{PythonLocator, VenvSpec};
pub use health::Prober;
