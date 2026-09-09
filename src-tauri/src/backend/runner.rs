//! Python 子进程拉起、日志回流与生命周期管理。
//!
//! 关键设计:
//! - `Sink` 是可以被 clone 进 tokio task 的状态句柄(绕过 `&self` 的生命周期限制)
//! - stdout/stderr 各起一个 reader task,逐行 emit `backend://log`
//! - supervisor task 用 `select!` 同时等"停止信号"和"子进程退出"
//! - 停止时先 `child.kill()`,再用 `taskkill /PID /T /F` 清掉 Next.js/uvicorn 的孙进程

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

use super::config::BackendConfig;
use super::health::Health;
use super::python::PythonLocator;

pub const EVENT_STATE: &str = "backend://state";
pub const EVENT_LOG: &str = "backend://log";

/// 日志环形缓冲上限(前端晚了也能拉到最近的日志)。
const LOG_CAP: usize = 500;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Idle,
    Booting,
    Ready,
    Crashed,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Idle,
    LocatingPython,
    CheckingDeps,
    Starting,
    Probing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    pub ts: String,
    pub stream: String,
    pub line: String,
}

/// 推给前端的完整状态快照。
#[derive(Debug, Clone, Serialize)]
pub struct StatusSnapshot {
    pub state: State,
    pub stage: Stage,
    pub message: String,
    pub health: Health,
    pub python: Option<String>,
    pub pid: Option<u32>,
}

/// 可被 clone 进异步任务的状态句柄。
#[derive(Clone)]
struct Sink {
    app: Option<AppHandle>,
    state: Arc<RwLock<State>>,
    stage: Arc<RwLock<Stage>>,
    message: Arc<RwLock<String>>,
    health: Arc<RwLock<Health>>,
    python: Arc<RwLock<Option<String>>>,
    logs: Arc<RwLock<VecDeque<LogLine>>>,
    pid: Arc<RwLock<Option<u32>>>,
}

impl Sink {
    fn now() -> String {
        chrono::Local::now().format("%H:%M:%S%.3f").to_string()
    }

    fn push_log(&self, stream: &str, line: impl Into<String>) {
        let entry = LogLine {
            ts: Self::now(),
            stream: stream.to_string(),
            line: line.into(),
        };
        {
            let mut buf = self.logs.write();
            if buf.len() >= LOG_CAP {
                buf.pop_front();
            }
            buf.push_back(entry.clone());
        }
        if let Some(app) = &self.app {
            let _ = app.emit(EVENT_LOG, entry);
        }
    }

    fn set(&self, stage: Stage, state: State, message: impl Into<String>) {
        *self.stage.write() = stage;
        *self.state.write() = state;
        *self.message.write() = message.into();
        self.emit_status();
    }

    fn set_message(&self, message: impl Into<String>) {
        *self.message.write() = message.into();
        self.emit_status();
    }

    fn set_health(&self, health: Health) {
        *self.health.write() = health;
    }

    fn emit_status(&self) {
        let Some(app) = &self.app else { return };
        let snapshot = StatusSnapshot {
            state: *self.state.read(),
            stage: *self.stage.read(),
            message: self.message.read().clone(),
            health: *self.health.read(),
            python: self.python.read().clone(),
            pid: None,
        };
        let _ = app.emit(EVENT_STATE, snapshot);
    }
}

/// 子进程句柄(只保留 pid 与停止信号通道,Child 本体移入 supervisor task)。
struct Supervisor {
    pid: Option<u32>,
    stop_tx: Option<oneshot::Sender<()>>,
}

/// 后端子进程管理器。
pub struct Runner {
    sink: Sink,
    sup: Mutex<Option<Supervisor>>,
}

impl Runner {
    pub fn new() -> Self {
        Self {
            sink: Sink {
                app: None,
                state: Arc::new(RwLock::new(State::Idle)),
                stage: Arc::new(RwLock::new(Stage::Idle)),
                message: Arc::new(RwLock::new(String::new())),
                health: Arc::new(RwLock::new(Health::default())),
                python: Arc::new(RwLock::new(None)),
                logs: Arc::new(RwLock::new(VecDeque::new())),
                pid: Arc::new(RwLock::new(None)),
            },
            sup: Mutex::new(None),
        }
    }

    /// 绑定 AppHandle(用于 emit 事件)。必须在 setup 阶段调用。
    pub fn attach(&mut self, app: AppHandle) {
        self.sink.app = Some(app);
    }

    // ---- 只读访问 ----

    pub fn state(&self) -> State {
        *self.sink.state.read()
    }

    pub fn pid(&self) -> Option<u32> {
        self.sup.lock().as_ref().and_then(|s| s.pid)
    }

    pub fn logs(&self) -> Vec<LogLine> {
        self.sink.logs.read().iter().cloned().collect()
    }

    pub fn status(&self) -> StatusSnapshot {
        StatusSnapshot {
            state: *self.sink.state.read(),
            stage: *self.sink.stage.read(),
            message: self.sink.message.read().clone(),
            health: *self.sink.health.read(),
            python: self.sink.python.read().clone(),
            pid: self.pid(),
        }
    }

    // ---- 状态写入 ----

    pub fn set(&self, stage: Stage, state: State, message: impl Into<String>) {
        self.sink.set(stage, state, message);
    }

    pub fn set_message(&self, message: impl Into<String>) {
        self.sink.set_message(message);
    }

    pub fn set_health(&self, health: Health) {
        self.sink.set_health(health);
        self.sink.emit_status();
    }

    pub fn push_log(&self, stream: &str, line: impl Into<String>) {
        self.sink.push_log(stream, line.into());
    }

    // ---- 生命周期 ----

    /// 拉起后端子进程。
    pub async fn start(&self, cfg: &BackendConfig, py: &PythonLocator) -> anyhow::Result<u32> {
        // 幂等:如果已有子进程,先停掉
        if self.pid().is_some() {
            let _ = self.stop().await;
        }

        *self.sink.python.write() = Some(format!(
            "{} ({})",
            py.executable.display(),
            py.display_version()
        ));

        let mut cmd = Command::new(&py.executable);
        cmd.args(&py.prefix);
        match &cfg.script {
            Some(script) => {
                cmd.arg(script);
            }
            None => {
                cmd.arg("-m").arg(&cfg.module);
            }
        }
        for arg in &cfg.args {
            cmd.arg(arg);
        }

        // Python 侧编码与缓冲:保证日志能实时刷出来、中文不乱码
        cmd.env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUTF8", "1")
            .env("PYTHONUNBUFFERED", "1")
            .env("DEEPTUTOR_DESKTOP", "1")
            .env("DEEPTUTOR_API_PORT", cfg.api_port.to_string())
            .env("DEEPTUTOR_WEB_PORT", cfg.web_port.to_string());
        for (k, v) in &cfg.extra_env {
            cmd.env(k, v);
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            // 不弹控制台窗口
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "无法启动后端进程:{}\n命令:{}",
                e,
                cfg.describe(&py.executable)
            )
        })?;

        let pid = child.id().unwrap_or(0);
        let command_line = cfg.describe(&py.executable);

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        self.sink
            .set(Stage::Starting, State::Booting, format!("已拉起后端进程 (pid {})", pid));
        self.sink
            .push_log("shell", format!("$ {}", command_line));

        // 日志回流
        if let Some(out) = stdout {
            spawn_reader(self.sink.clone(), "stdout", out);
        }
        if let Some(err) = stderr {
            spawn_reader(self.sink.clone(), "stderr", err);
        }

        // supervisor
        let (stop_tx, stop_rx) = oneshot::channel();
        *self.sup.lock() = Some(Supervisor {
            pid: Some(pid),
            stop_tx: Some(stop_tx),
        });

        let sink = self.sink.clone();
        tokio::spawn(async move {
            let mut child: Child = child;
            let exit = tokio::select! {
                _ = stop_rx => {
                    sink.push_log("shell", "收到停止信号,正在终止后端进程...");
                    let _ = child.start_kill();
                    None
                }
                status = child.wait() => Some(status),
            };

            if let Some(status) = exit {
                match status {
                    Ok(s) => {
                        let msg = format!("后端进程退出,退出码:{}", s);
                        sink.push_log("shell", &msg);
                        if !s.success() && *sink.state.read() == State::Booting {
                            sink.set(Stage::Failed, State::Crashed, msg);
                        }
                    }
                    Err(e) => {
                        let msg = format!("等待后端进程失败:{}", e);
                        sink.push_log("shell", &msg);
                        sink.set(Stage::Failed, State::Crashed, msg);
                    }
                }
            }

            // 兜底:杀掉进程树(Next.js / uvicorn 常有孙进程)
            kill_tree(pid, true);
        });

        Ok(pid)
    }

    /// 停止后端子进程。
    pub async fn stop(&self) -> anyhow::Result<()> {
        let sup = self.sup.lock().take();
        let Some(mut sup) = sup else {
            *self.sink.state.write() = State::Stopped;
            return Ok(());
        };

        let pid = sup.pid.take();
        if let Some(tx) = sup.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(pid) = pid {
            kill_tree(pid, true);
        }
        *self.sink.pid.write() = None;

        self.sink
            .set(Stage::Idle, State::Stopped, "后端已停止".to_string());
        Ok(())
    }

    /// 同步强杀进程树(用于 RunEvent::Exit,此时 async runtime 可能已经不响应)。
    pub fn kill_tree_sync(&self) {
        if let Some(pid) = self.pid() {
            kill_tree(pid, true);
        }
    }
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

/// 逐行读取子进程输出并推给前端。
fn spawn_reader<R: AsyncRead + Unpin + Send + 'static>(
    sink: Sink,
    stream: &'static str,
    reader: R,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let line = line.trim_end().to_string();
                    if !line.is_empty() {
                        sink.push_log(stream, line);
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });
}

/// 杀掉进程树。
///
/// Windows 上 DeepTutor 会派生 uvicorn / Next.js 等孙进程,只杀父进程会留下
/// 占端口的孤儿进程,所以统一走 `taskkill /T`。先尝试优雅结束,再强杀。
fn kill_tree(pid: u32, force: bool) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        let mut graceful = std::process::Command::new("taskkill");
        graceful.args(["/PID", &pid.to_string(), "/T"]);
        graceful.creation_flags(CREATE_NO_WINDOW);
        let _ = graceful.output();

        if force {
            let mut hard = std::process::Command::new("taskkill");
            hard.args(["/PID", &pid.to_string(), "/T", "/F"]);
            hard.creation_flags(CREATE_NO_WINDOW);
            let _ = hard.output();
        }
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .output();
        if force {
            let _ = std::process::Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .output();
        }
    }
}
