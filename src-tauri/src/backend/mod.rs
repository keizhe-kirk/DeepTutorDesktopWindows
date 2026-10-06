//! Backend 模块:管理 DeepTutor Python 后端子进程。
//!
//! - `config`    : 启动配置(全部可用环境变量覆盖)
//! - `runtime`   : 内置运行时(自包含安装包携带的 Python / Node)路径解析
//! - `overlay`   : 后端热更新的用户级叠加层(新版解到用户目录,靠 PYTHONPATH 生效)
//! - `hotupdate` : 后端热更新流程(查 PyPI -> 下载 -> 依赖解析 -> 解压 -> 激活)
//! - `patch`     : 本地补丁层(sitecustomize 钩子打同版本修正,与 overlay 语义不同)
//! - `bilibili`  : B 站登录(调补丁包的 CDP 登录脚本;字幕/章节需要登录态)
//! - `winproc`   : Windows 作业对象与进程映像查询(退出时保证不留孤儿进程)
//! - `ca`        : CA 信任合并(certifi + Windows 证书库根),供子进程 HTTPS 校验
//! - `python`    : 解释器探测与依赖检查
//! - `runner`    : 子进程拉起 / 日志回流 / 生命周期
//! - `health`    : HTTP 健康探测
//! - `boot`      : 把上述步骤串成状态机的启动编排

pub mod config;
pub mod ca;
pub mod runtime;
pub mod overlay;
pub mod hotupdate;
pub mod patch;
pub mod bilibili;
pub mod winproc;
pub mod python;
pub mod runner;
pub mod health;
pub mod boot;

pub use runner::Runner;
pub use runtime::BundledRuntimes;
pub use python::{PythonLocator, VenvSpec};
pub use health::Prober;
