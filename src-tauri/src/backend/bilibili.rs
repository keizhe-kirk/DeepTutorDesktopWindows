//! B 站登录（托盘「设置」组的入口）。
//!
//! # 为什么跑子进程而不是把逻辑搬进 Rust
//!
//! 扫码登录要跟系统浏览器说 CDP、读HttpOnly cookie —— 逻辑在补丁包
//! `dtpatch_bili/login.py` 里（手写WebSocket 客户端）。这里**不重复实现**，
//! 而是用内置 Python 起一个短命子进程跑它，复用同一份实现。
//!
//! # 为什么用 spawn +轮询而不是 output()
//!
//! 用户扫码可能要几分钟，`Command::output()` 会一直阻塞等待子进程退出。
//! 这里改成 spawn + 每 500ms 轮询，这样：
//! - tooltip能实时反映「等待扫码」；
//! - 用户想放弃时可以直接杀进程，不留孤儿浏览器。
//!
//! # 失败一律降级
//!
//! 登录是可选增强。任何失败（没有浏览器、Python 起不来、用户超时）都只是
//! 弹个说明框，**绝不影响**后端与已登录状态。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};
use tokio::process::{Child, Command};

use super::patch::Patches;

/// 扫码等待上限。与Python 侧 `login(timeout=...)` 一致。
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// 运行登录脚本的 Python 脚本内容。
///
/// 用 `-c` 而不是临时文件：不需要在磁盘上留一个含用户数据的脚本文件。
const SCRIPT: &str = r#"
import asyncio, json, sys
sys.path.insert(0, sys.argv[1])
from dtpatch_bili import login

if sys.argv[2] == "login":
    found = login.login(timeout=float(sys.argv[3]))
    print(json.dumps({"ok": bool(found.get("SESSDATA"))}, ensure_ascii=False))
elif sys.argv[2] == "clear":
    print(json.dumps({"ok": login.clear_login()}, ensure_ascii=False))
else:
    st = asyncio.run(login.status())
    print(json.dumps(st, ensure_ascii=False))
"#;

/// 登录结果。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LoginOutcome {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub logged_in: String,
    #[serde(default)]
    pub valid: String,
}

/// 拼出跑登录脚本所需的 (python, patch_dir) 二元组。
///
/// 解释器用**内置运行时**（`BundledRuntimes::python_exe`）：登录脚本只需要标准
/// library，用内置 Python 就不必猜系统 Python 有没有、版本对不对。拿不到就退回
/// 系统解释器，再不行返回 `None` 让上层弹说明框。
fn command_parts(app: &AppHandle) -> Option<(PathBuf, PathBuf)> {
    let runner = app.try_state::<std::sync::Arc<super::runner::Runner>>()?;
    let patch_dir = Patches::detect(Some(super::runtime::effective_home()))
        .root()
        .to_path_buf();

    let bundled = runner.bundled().python_exe();
    let python = bundled.unwrap_or_else(|| PathBuf::from("python"));
    Some((python, patch_dir))
}

/// 起一个跑补丁脚本的子进程。
fn spawn_script(app: &AppHandle, mode: &str, extra: Option<&str>) -> Result<(Child, String), String> {
    let (python, patch_dir) = command_parts(app).ok_or("后端管理器尚未就绪，请稍后重试。")?;
    if !patch_dir.join("dtpatch_bili").is_dir() {
        return Err("本地补丁尚未就位，请先启动一次后端。".into());
    }

    let mut cmd = Command::new(&python);
    cmd.arg("-P")
        .arg("-c")
        .arg(SCRIPT)
        .arg(patch_dir.as_os_str())
        .arg(mode);
    if let Some(value) = extra {
        cmd.arg(value);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // 子进程只跑本地 stdlib 脚本，不碰后端的数据目录。
    cmd.env("DEEPTUTOR_PATCH_HOME", &patch_dir);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("无法启动 Python（{}）：{e}", python.display()))?;
    Ok((child, patch_dir.display().to_string()))
}

/// 跑一个「很快返回」的脚本（状态查询 / 清除凭据）。
async fn run_quick(app: &AppHandle, mode: &str) -> Result<LoginOutcome, String> {
    let (child, _) = spawn_script(app, mode, None)?;
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .map_err(|_| "操作超时。".to_string())?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let msg = err.trim();
        return Err(if msg.is_empty() {
            format!("操作失败（退出码 {:?}）", output.status.code())
        } else {
            msg.chars().take(200).collect()
        });
    }
    parse_last_json(&output.stdout)
        .ok_or_else(|| "无法解析脚本输出。".to_string())
}

/// 从 stdout 里取最后一个 JSON 对象。
///
/// 补丁会往 stderr 打日志、stdout 偶尔混进警告，所以不能假设第一行就是 JSON。
fn parse_last_json(bytes: &[u8]) -> Option<LoginOutcome> {
    let text = String::from_utf8_lossy(bytes);
    for line in text.lines().rev() {
        let trimmed = line.trim();
        if trimmed.starts_with('{') && trimmed.ends_with('}') {
            if let Ok(v) = serde_json::from_str::<LoginOutcome>(trimmed) {
                return Some(v);
            }
        }
    }
    None
}

/// 当前登录状态（用于菜单文案与「清除」前的判断）。
pub async fn status(app: &AppHandle) -> Result<LoginOutcome, String> {
    run_quick(app, "status").await
}

/// 清除已保存的登录凭据。
pub async fn clear(app: &AppHandle) -> Result<LoginOutcome, String> {
    run_quick(app, "clear").await
}

/// 引导用户扫码登录。
///
/// 会在浏览器里打开 B 站登录页，阻塞等待到用户完成或超时。
/// `on_wait` 用来更新 tooltip。
pub async fn login<F>(app: &AppHandle, mut on_wait: F) -> Result<LoginOutcome, String>
where
    F: FnMut(&str),
{
    let (mut child, _) = spawn_script(app, "login", Some(&LOGIN_TIMEOUT.as_secs().to_string()))?;

    let deadline = Instant::now() + LOGIN_TIMEOUT + Duration::from_secs(20);
    let mut last_notice = Instant::now() - Duration::from_secs(30);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // 正常退出：脚本应该已把 JSON 写到 stdout。
                let mut buf = Vec::new();
                if let Some(mut stdout) = child.stdout.take() {
                    use tokio::io::AsyncReadExt;
                    let _ = stdout.read_to_end(&mut buf).await;
                }
                if !status.success() {
                    let mut err = Vec::new();
                    if let Some(mut stderr) = child.stderr.take() {
                        use tokio::io::AsyncReadExt;
                        let _ = stderr.read_to_end(&mut err).await;
                    }
                    let msg = String::from_utf8_lossy(&err);
                    return Err(if msg.trim().is_empty() {
                        format!("登录失败（退出码 {:?}）", status.code())
                    } else {
                        msg.trim().lines().last().unwrap_or("登录失败").to_string()
                    });
                }
                return parse_last_json(&buf)
                    .ok_or_else(|| "无法解析登录结果。".to_string());
            }
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }

        if Instant::now() >= deadline {
            let _ = child.kill().await;
            return Err("登录超时，已取消。请确认在浏览器里完成了扫码。".into());
        }

        // 每 30 秒给一次提示，避免用户以为卡死了。
        if last_notice.elapsed() >= Duration::from_secs(30) {
            last_notice = Instant::now();
            on_wait("DeepTutor — 等待在浏览器中完成 B 站登录…");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// 补丁是否可用（没播种 / 没启用 / 版本不匹配时不显示登录入口）。
///
/// 返回 `Err(msg)` 而非 bool，是为了让调用方能把「为什么不支持」讲清楚 ——
/// 用户看到一句「本地补丁尚未就位」比看到一个灰掉的菜单项有用得多。
pub fn patch_ready_check(app: &AppHandle) -> Result<(), String> {
    let Some(runner) = app.try_state::<std::sync::Arc<super::runner::Runner>>() else {
        return Err("后端管理器尚未就绪，请稍后重试。".into());
    };
    // ★ 必须与 runner.rs 的注入判据同源（生效版：叠加层优先），否则菜单会说
    // 「已就绪」而实际没注入 —— 用户点了才发现，白跑一趟浏览器。
    let version = super::runner::effective_deeptutor_version(&runner.bundled());
    let patches = Patches::detect(Some(super::runtime::effective_home()));
    let effective = version.as_deref();
    if patches.effective_dir(effective).is_some() {
        return Ok(());
    }

    // ★ 报错必须说清**真实原因**。这里曾经只有一句「请完全退出后重新启动」——
    // 而版本判据不匹配时重启再多次也没用，用户会被这句话无限误导下去。
    // （真机踩过：生效版 1.6.13、旧基线 1.6.12，菜单报未就绪。）
    let Some(reason) = patches.inapplicable_reason(effective) else {
        // 判据放行但没注入 → 只剩钩子文件缺失这一种可能。
        return Err(
            "哔哩哔哩支持尚未启用（本地补丁未就绪）。\n\n\
             请完全退出 DeepTutor 后重新启动一次，让应用完成首次播种。"
                .into(),
        );
    };
    let hint = match reason.as_str() {
        "补丁未启用" => "请完全退出 DeepTutor 后重新启动一次，让应用完成首次播种。",
        _ => "请更新 DeepTutor 桌面端到最新版本（补丁需与该版本一同发布）。",
    };
    Err(format!(
        "哔哩哔哩支持尚未启用。\n\n原因：{reason}\n\n{hint}"
    ))
}

/// 补丁目录（供文案展示）。
pub fn patch_dir() -> Option<PathBuf> {
    let dir = Patches::detect(Some(super::runtime::effective_home()))
        .root()
        .to_path_buf();
    dir.is_dir().then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_last_json_only() {
        let input = b"warning: something\n{\"ok\":true,\"detail\":\"fine\"}\n";
        let got = parse_last_json(input).expect("should parse");
        assert!(got.ok);
        assert_eq!(got.detail, "fine");
    }

    #[test]
    fn ignores_non_json_noise() {
        assert!(parse_last_json(b"[dtpatch] log line\nno json here\n").is_none());
    }

    #[test]
    fn picks_the_last_object_when_several_are_printed() {
        let input = b"{\"ok\":false}\n{\"ok\":true,\"detail\":\"later\"}\n";
        let got = parse_last_json(input).expect("should parse");
        assert!(got.ok, "应当取最后一个 JSON");
        assert_eq!(got.detail, "later");
    }

    #[test]
    fn status_fields_default_to_empty_not_missing() {
        let got = parse_last_json(b"{\"ok\":true}").expect("should parse");
        assert!(got.ok);
        assert!(got.detail.is_empty());
        assert!(got.logged_in.is_empty());
    }
}