//! 启动编排:把"探测 -> 检查 -> 拉起 -> 探活 -> 导航"串成一条状态机。
//!
//! 每一步都 emit `backend://state`,启动页据此渲染进度;失败时进入
//! `Stage::Failed` 并带上可执行的修复建议。

use std::sync::Arc;

use tauri::{AppHandle, Manager};

use super::config::BackendConfig;
use super::health::Prober;
use super::python;
use super::runner::{Runner, Stage, State};

const HINT_NO_PYTHON: &str =
    "请安装 Python 3.11+ 并勾选 \"Add Python to PATH\",或用环境变量 DEEPTUTOR_PYTHON 直接指定解释器路径。";
const HINT_NO_DEEPTUTOR: &str = "请先安装 DeepTutor 后端:在 DeepTutor 仓库根目录执行 pip install -e . ,或运行本项目 scripts\\bootstrap-python.ps1 完成一键引导。";
const HINT_TIMEOUT: &str =
    "后端进程已拉起但服务未就绪。请看下方日志定位;常见原因是端口被占用、依赖缺失或首次构建 Next.js 较慢。";

/// 在后台启动一次完整的后端引导流程。
pub fn spawn(app: &AppHandle) {
    let Some(runner) = app
        .try_state::<Arc<Runner>>()
        .map(|s| s.inner().clone())
    else {
        log::error!("boot::spawn 调用时 Runner 尚未注册到 managed state");
        return;
    };
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        run(handle, runner).await;
    });
}

/// 完整的后端引导流程。重复调用是安全的(会先停掉已有进程)。
pub async fn run(_app: AppHandle, runner: Arc<Runner>) {
    let cfg = BackendConfig::from_env();
    let _ = runner.stop().await;

    // 1) 探测 Python
    runner.set(
        Stage::LocatingPython,
        State::Booting,
        "正在探测 Python 解释器...",
    );
    let py = match python::locate(cfg.min_python).await {
        Ok(p) => p,
        Err(e) => {
            fail(&runner, e.to_string(), HINT_NO_PYTHON);
            return;
        }
    };
    runner.push_log(
        "shell",
        format!(
            "Python {} @ {}",
            py.display_version(),
            py.executable.display()
        ),
    );

    // 2) 检查 DeepTutor 是否可导入
    runner.set(
        Stage::CheckingDeps,
        State::Booting,
        "正在检查 DeepTutor 依赖...",
    );
    if cfg.skip_dep_check {
        runner.push_log("shell", "已跳过依赖检查 (DEEPTUTOR_SKIP_DEP_CHECK=1)");
    } else if !python::is_deeptutor_installed(&py).await {
        fail(
            &runner,
            "当前 Python 环境中未找到 deeptutor 包".to_string(),
            HINT_NO_DEEPTUTOR,
        );
        return;
    } else {
        runner.push_log("shell", "deeptutor 依赖检查通过");
    }

    // 3) 拉起子进程
    runner.set(Stage::Starting, State::Booting, "正在拉起后端子进程...");
    if let Err(e) = runner.start(&cfg, &py).await {
        fail(&runner, e.to_string(), "请确认 DeepTutor 安装完整后重试。");
        return;
    }

    // 4) 等待两个端口就绪
    runner.set(
        Stage::Probing,
        State::Booting,
        format!(
            "等待服务就绪 (API :{} / Web :{})...",
            cfg.api_port, cfg.web_port
        ),
    );
    let prober = Prober::new(cfg.ports());
    let progress_runner = runner.clone();
    match prober
        .wait_ready(cfg.startup_timeout, move |h| progress_runner.set_health(h))
        .await
    {
        Ok(_) => {}
        Err(e) => {
            fail(&runner, e.to_string(), HINT_TIMEOUT);
            return;
        }
    }

    // 5) 就绪 -> 通知前端跳转到 DeepTutor Web UI
    //
    // 不在 Rust 侧调 win.navigate(),原因:WebView2 上偶发 0x8007139F 错误风暴。
    // 改由前端监听 state==ready 后用 location.replace 跳转,稳定且与 dev 模式兼容。
    let url = format!("http://127.0.0.1:{}", cfg.web_port);
    runner.push_log("shell", format!("backend ready,前端将导航 -> {}", url));
    runner.set(Stage::Ready, State::Ready, "后端就绪,正在加载界面...");
}

fn fail(runner: &Arc<Runner>, message: String, hint: &str) {
    let full = if hint.is_empty() {
        message.clone()
    } else {
        format!("{}\n{}", message, hint)
    };
    runner.push_log("shell", format!("ERROR: {}", message));
    runner.set(Stage::Failed, State::Failed, full);
}
