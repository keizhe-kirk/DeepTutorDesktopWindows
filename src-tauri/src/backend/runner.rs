//! Python 子进程拉起、日志回流与生命周期管理。
//!
//! 关键设计:
//! - `Sink` 是可以被 clone 进 tokio task 的状态句柄(绕过 `&self` 的生命周期限制)
//! - stdout/stderr 各起一个 reader task,逐行 emit `backend://log`
//! - supervisor task 用 `select!` 同时等"停止信号"和"子进程退出"
//! - 停止时先 `child.kill()`,再用 `taskkill /PID /T /F` 清掉 Next.js/uvicorn 的孙进程
//! - **退出兜底(关键)**:子进程会被加入一个 `KILL_ON_JOB_CLOSE` 作业对象。
//!   壳进程无论以何种方式消失(正常退出 / 崩溃 / 被强杀 / 更新器 exit),
//!   内核都会连带结束整棵后端进程树,不会留下死占端口的孤儿进程。
//! - 启动时还会回收"上一轮残留下来的"后端进程(见 `reap_stale_backend`),
//!   兼容旧版本遗留的孤儿进程。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

use super::config::BackendConfig;
use super::health::Health;
use super::python::PythonLocator;
use super::runtime::{self, BundledRuntimes};
use super::winproc::{self, JobHandle};

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
    /// 安装包内置运行时(自包含安装包携带的 Python / Node)。
    bundled: Mutex<BundledRuntimes>,
    /// 承载后端子进程树的作业对象。活到进程结束 —— Drop 即触发内核清理。
    job: Mutex<Option<JobHandle>>,
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
            bundled: Mutex::new(BundledRuntimes::default()),
            job: Mutex::new(None),
        }
    }

    /// 绑定 AppHandle(用于 emit 事件)。必须在 setup 阶段调用。
    pub fn attach(&mut self, app: AppHandle) {
        self.sink.app = Some(app);
    }

    /// 登记安装包内置运行时。必须在 setup 阶段调用(早于 boot::spawn)。
    pub fn set_bundled(&self, bundled: BundledRuntimes) {
        *self.bundled.lock() = bundled;
    }

    /// 取一份内置运行时快照。
    pub fn bundled(&self) -> BundledRuntimes {
        self.bundled.lock().clone()
    }

    /// 确保作业对象就绪,并把 `pid` 登记进去。返回一句给用户看的日志。
    ///
    /// 登记成功后,壳进程无论以何种方式消失,内核都会连带结束该进程及其所有后代。
    fn enroll_in_job(&self, pid: u32) -> String {
        if pid == 0 {
            return "警告:未拿到有效 pid,无法登记作业对象".to_string();
        }
        let mut guard = self.job.lock();
        if guard.is_none() {
            match JobHandle::new_kill_on_close() {
                Ok(job) => *guard = Some(job),
                Err(e) => {
                    return format!("警告:创建作业对象失败,异常退出时可能残留后端进程: {e}");
                }
            }
        }
        match guard.as_ref().map(|job| job.assign_pid(pid)) {
            Some(Ok(())) => "后端进程已纳入作业对象:壳退出时由内核连带回收".to_string(),
            Some(Err(e)) => format!("警告:无法把后端进程纳入作业对象: {e}"),
            None => "警告:作业对象不可用".to_string(),
        }
    }

    /// 记录本轮拉起的后端进程,供下次启动回收残留。
    fn write_pid_record(&self, pid: u32, exe: &std::path::Path) {
        let Some(path) = pid_record_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let record = PidRecord {
            pid,
            exe: exe.display().to_string(),
        };
        if let Ok(text) = serde_json::to_string(&record) {
            if let Err(e) = std::fs::write(&path, text) {
                self.push_log("shell", format!("警告:写入 pid 记录失败: {e}"));
            }
        }
    }

    fn clear_pid_record(&self) {
        if let Some(path) = pid_record_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    /// 启动时回收"上一轮残留"的后端进程。
    ///
    /// 0.2.0 起后端子进程被纳入作业对象,正常路径不会再残留;这个兜底主要面向
    /// 从旧版本(仅靠退出回调清理)升级上来的用户 —— 那些孤儿进程仍死占着
    /// 8001 / 3782,会让新实例静默连到旧服务上。
    ///
    /// 只回收"映像路径与上次记录完全一致"的进程,避免 PID 复用后误杀无关进程。
    pub fn reap_stale_backend(&self) {
        let Some(path) = pid_record_path() else { return };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let _ = std::fs::remove_file(&path);

        let Ok(record) = serde_json::from_str::<PidRecord>(&text) else {
            return;
        };
        if record.pid == 0 || record.pid == self.pid().unwrap_or(0) {
            return;
        }
        let Some(live) = winproc::image_path(record.pid) else {
            return;
        };
        if !winproc::same_executable(&live, std::path::Path::new(&record.exe)) {
            return;
        }

        self.push_log(
            "shell",
            format!(
                "检测到上一轮残留的后端进程 (pid {}),正在回收...",
                record.pid
            ),
        );
        kill_tree(record.pid, true);
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

        let bundled = self.bundled();

        *self.sink.python.write() = Some(format!(
            "{} ({}) [{}]",
            py.executable.display(),
            py.display_version(),
            py.source_label()
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

        // 内置 Node 必须前置到 PATH:deeptutor 用 `shutil.which("node")` 定位前端运行时,
        // 放在最前面才能盖过用户机器上可能存在的旧版 Node,保证自包含。
        if let Some(node_dir) = bundled.node_dir() {
            let mut paths = vec![node_dir.clone()];
            if let Some(existing) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&existing));
            }
            match std::env::join_paths(paths) {
                Ok(joined) => {
                    cmd.env("PATH", &joined);
                    self.sink.push_log(
                        "shell",
                        format!("内置 Node 已前置到子进程 PATH: {}", node_dir.display()),
                    );
                }
                Err(e) => self.sink.push_log(
                    "shell",
                    format!("警告:合并 PATH 失败,内置 Node 可能不生效: {}", e),
                ),
            }
        }

        // deeptutor 默认把运行数据写到 `<cwd>/data`(见 runtime/home.py)。
        // 显式指向用户级目录,避免安装版把 data/ 撒进用户主目录,
        // 同时避开 Program Files 的只读属性。
        let data_home = match runtime::runtime_home_override() {
            Some(dir) => match std::fs::create_dir_all(&dir) {
                Ok(()) => {
                    cmd.env(runtime::ENV_DEEPTUTOR_HOME, &dir);
                    self.sink
                        .push_log("shell", format!("运行数据目录: {}", dir.display()));
                    Some(dir)
                }
                Err(e) => {
                    self.sink.push_log(
                        "shell",
                        format!("警告:无法创建数据目录 {}: {}", dir.display(), e),
                    );
                    None
                }
            },
            None => None,
        };

        // 内置解释器装在 Program Files 下(普通用户不可写),Python 导入模块时
        // 尝试写 __pycache__ 会失败并退化为"每次启动都重新编译",拖慢启动。
        // 把字节码缓存重定向到用户目录,顺便避免污染安装目录。
        if py.bundled {
            if let Some(dir) = dirs::data_local_dir().map(|d| d.join("DeepTutor").join("pycache")) {
                cmd.env("PYTHONPYCACHEPREFIX", dir);
            }
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            // 不弹控制台窗口
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        // 设置工作目录:优先落在可写的运行数据目录(否则子进程的相对路径写入
        // 可能落到 System32 这类无权限位置),拿不到时退回用户主目录。
        if let Some(dir) = data_home.or_else(dirs::home_dir) {
            cmd.current_dir(dir);
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

        // 关键兜底:把子进程纳入作业对象,壳一旦消失由内核连带回收整棵树。
        let job_note = self.enroll_in_job(pid);
        self.sink.push_log("shell", job_note);
        self.write_pid_record(pid, &py.executable);

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
            self.clear_pid_record();
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
        self.clear_pid_record();

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

/// 上一轮拉起的后端进程记录。仅用于启动时回收残留。
#[derive(Debug, Serialize, Deserialize)]
struct PidRecord {
    pid: u32,
    /// 解释器完整路径。回收前会与存活进程的映像路径比对,防止 PID 复用误杀。
    exe: String,
}

/// pid 记录文件位置(与 `DEEPTUTOR_HOME` 同目录)。
fn pid_record_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|dir| dir.join("DeepTutor").join("backend.pid"))
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
