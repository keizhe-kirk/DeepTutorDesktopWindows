//! Windows 原生进程工具:作业对象(Job Object)与进程映像查询。
//!
//! ## 为什么需要作业对象
//!
//! 壳进程拉起的是一条进程链:
//!
//! ```text
//! DeepTutor.exe
//!   └─ python.exe -m deeptutor start --no-browser
//!        ├─ python.exe -m uvicorn ... --port 8001
//!        └─ node.exe ...\runtime\web\server.js  (:3782)
//! ```
//!
//! 只靠"退出回调里杀进程树"只能覆盖**正常退出**这一条路径。以下情况回调根本不会执行:
//! 任务管理器强杀、`profile.release` 的 `panic = "abort"`、自动更新时安装器接管后
//! 直接 `process::exit()`,以及开发者按 Ctrl+C 结束 `tauri dev`。
//! 一旦发生,后端进程就变成孤儿,**死占 8001 / 3782**,下次启动会静默连到旧服务上。
//!
//! 正解是 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`:作业对象里最后一个句柄被关闭时,
//! 内核会强制结束作业内的所有进程。壳进程持有这个句柄,进程一死句柄就被回收,
//! 于是**内核替我们完成清理** —— 不依赖任何用户态回调,因此无法被绕过。
//!
//! 子进程加入作业后,它后续派生的孙进程会自动继承作业成员身份,无需逐个登记。
//!
//! ## 依赖的 windows crate feature
//!
//! `Win32_System_JobObjects`(作业对象本体)、`Win32_Security`
//! (`CreateJobObjectW` 的 `SECURITY_ATTRIBUTES` 参数)、`Win32_System_Threading`
//! (`JOBOBJECT_EXTENDED_LIMIT_INFORMATION` 与 `OpenProcess`)。
//! 少一个都会导致"符号找不到"的编译错误 —— 报错信息不会提示缺哪个 feature。
//!
//! ## 非 Windows 平台
//!
//! 提供同签名空实现,保证跨平台编译通过(本项目只发 Windows 包)。

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::io;
    use std::path::PathBuf;

    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// 「句柄关闭即杀死全部成员进程」的作业对象。
    ///
    /// 必须活到壳进程结束 —— 一旦被 Drop(句柄关闭),作业内所有进程立即被内核杀死。
    pub struct JobHandle(HANDLE);

    // 内核对象句柄本身是进程级资源,可安全跨线程共享与传递。
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    impl JobHandle {
        /// 创建作业对象并启用 `KILL_ON_JOB_CLOSE`。
        pub fn new_kill_on_close() -> io::Result<Self> {
            unsafe {
                let job =
                    CreateJobObjectW(None, None).map_err(|e| io::Error::other(e.to_string()))?;

                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

                if let Err(e) = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) {
                    let _ = CloseHandle(job);
                    return Err(io::Error::other(e.to_string()));
                }

                Ok(Self(job))
            }
        }

        /// 把指定 pid 的进程加入作业。
        ///
        /// Windows 8 起支持嵌套作业,因此即使壳进程自己已在某个作业里也能成功。
        /// 该进程之后派生的子进程会自动成为作业成员。
        pub fn assign_pid(&self, pid: u32) -> io::Result<()> {
            if pid == 0 {
                return Err(io::Error::other("pid 为 0,无法加入作业"));
            }
            unsafe {
                // 注意:windows 0.61 起 OpenProcess 的 binherithandle 参数是 bool 而非 BOOL
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)
                    .map_err(|e| io::Error::other(e.to_string()))?;
                let result = AssignProcessToJobObject(self.0, process);
                let _ = CloseHandle(process);
                result.map_err(|e| io::Error::other(e.to_string()))?;
            }
            Ok(())
        }
    }

    impl Drop for JobHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// 查询进程的可执行文件完整路径。进程不存在或无权限时返回 `None`。
    ///
    /// 用于「陈旧进程回收」时校验 pid 身份 —— 只比对映像路径,
    /// 避免 PID 复用后误杀一个毫不相干的进程。
    pub fn image_path(pid: u32) -> Option<PathBuf> {
        if pid == 0 {
            return None;
        }
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 4096];
            let mut len = buf.len() as u32;
            let result = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            );
            let _ = CloseHandle(process);
            result.ok()?;
            let text = String::from_utf16_lossy(&buf[..len as usize]);
            if text.is_empty() {
                None
            } else {
                Some(PathBuf::from(text))
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;
    use std::path::PathBuf;

    /// 非 Windows 平台的占位实现。
    pub struct JobHandle;

    impl JobHandle {
        pub fn new_kill_on_close() -> io::Result<Self> {
            Ok(Self)
        }

        pub fn assign_pid(&self, _pid: u32) -> io::Result<()> {
            Ok(())
        }
    }

    pub fn image_path(_pid: u32) -> Option<PathBuf> {
        None
    }
}

pub use imp::{image_path, JobHandle};

/// 判断两个路径是否指向同一个可执行文件(大小写不敏感、忽略尾部斜杠)。
pub fn same_executable(a: &std::path::Path, b: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .replace('/', "\\")
            .to_ascii_lowercase()
    };
    let (x, y) = (norm(a), norm(b));
    !x.is_empty() && x == y
}
