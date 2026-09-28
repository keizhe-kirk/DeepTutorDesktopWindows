//! 后端热更新的「用户级叠加层」(user overlay)。
//!
//! # 为什么不能直接改安装目录
//!
//! `tauri.conf.json` 里 NSIS 是 `installMode: "perMachine"`,默认装到
//! `C:\Program Files\...` 这类需要管理员权限的位置。往那里的
//! `runtimes\python\Lib\site-packages` 写文件,普通用户会直接失败 ——
//! 所以"下载新版 -> 覆盖安装目录"这条路走不通。
//!
//! 改成「叠加」思路:新版后端解到**用户自己可写**的目录里,启动后端时
//! 把它前置进子进程的 `PYTHONPATH`。Python 的 `sys.path` 顺序是
//!
//! ```text
//! 脚本目录 / cwd  ->  PYTHONPATH  ->  标准库  ->  site-packages
//! ```
//!
//! `PYTHONPATH` 排在 `site-packages` **之前**,所以叠加层里的新版本会
//! 自动盖住安装目录里的内置版,不需要动安装目录一个字节。
//!
//! # 目录布局
//!
//! ```text
//! %LOCALAPPDATA%\DeepTutor\backend\
//!   state.json      当前激活状态(见 OverlayState)
//!   versions\
//!     1.6.13\       wheel 解开后的内容,顶层直接是包目录:
//!                   deeptutor\  deeptutor_web\  deeptutor_cli\  requests\ ...
//!   wheels\         下载过的 wheel 缓存(便于离线重装 / 事后取证)
//! ```
//!
//! 安装目录里的内置版**永远原地不动** —— 它是出厂兜底:叠加层坏了、
//! 或者用户想退回去,删掉 state.json 就立刻恢复内置版。
//!
//! # 「内置反超」保护
//!
//! 用户热更新到 1.6.13 之后,某天桌面壳升级、内置版变成了 1.6.14。
//! 此时叠加层反而是**旧的**,必须让它自动失效 —— 见
//! [`Overlay::effective_version`] 里的版本比较。

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use pep440_rs::Version;
use serde::{Deserialize, Serialize};

/// 叠加层根目录名(挂在 `DEEPTUTOR_HOME` 之下)。
const OVERLAY_DIRNAME: &str = "backend";
/// 状态文件名。
const STATE_FILENAME: &str = "state.json";
/// 各版本解包内容的目录名。
const VERSIONS_DIRNAME: &str = "versions";
/// wheel 缓存目录名。
const WHEELS_DIRNAME: &str = "wheels";

/// 叠加层的持久状态。存 `state.json`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OverlayState {
    /// 当前激活的叠加层版本。`None` / 缺失 = 使用内置版。
    pub active: Option<String>,
    /// 激活时间(RFC3339)。
    pub activated_at: Option<String>,
    /// 这次叠加实际写入的包(展示 + 事后排查用)。
    #[serde(default)]
    pub packages: Vec<String>,
    /// 安装叠加层时,内置版是哪个版本。
    ///
    /// 作用:桌面壳升级后内置版可能反超叠加层,靠这个字段能判断出
    /// "这层是相对哪个基线做的",日志里说清楚。
    pub base_bundled: Option<String>,
}

impl OverlayState {
    /// 是否是"没有叠加,正在用内置版"。
    pub fn is_builtin(&self) -> bool {
        self.active.is_none()
    }
}

/// 用户级后端叠加层。
#[derive(Debug, Clone)]
pub struct Overlay {
    root: PathBuf,
}

impl Overlay {
    /// 按 `DEEPTUTOR_HOME` 的约定定位叠加层。
    ///
    /// `home` 传 `None` 时退回 `%LOCALAPPDATA%\DeepTutor`,与
    /// [`super::runtime::runtime_home_override`] 的默认值保持一致。
    pub fn detect(home: Option<PathBuf>) -> Self {
        let base = home.unwrap_or_else(|| {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("DeepTutor")
        });
        Self {
            root: base.join(OVERLAY_DIRNAME),
        }
    }

    /// 叠加层根目录(`...\DeepTutor\backend`)。**不保证已存在**。
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn state_path(&self) -> PathBuf {
        self.root.join(STATE_FILENAME)
    }

    /// 各版本解包目录的父目录(`...\backend\versions`)。
    pub fn versions_dir(&self) -> PathBuf {
        self.root.join(VERSIONS_DIRNAME)
    }

    /// 某个版本的解包目录。
    pub fn version_dir(&self, version: &str) -> PathBuf {
        self.versions_dir().join(version)
    }

    /// wheel 缓存目录。
    pub fn wheels_dir(&self) -> PathBuf {
        self.root.join(WHEELS_DIRNAME)
    }

    /// 读状态。文件不存在 / 解析失败一律当作"没有叠加",不报错 ——
    /// 状态文件损坏时应当静默退回内置版,而不是让应用起不来。
    pub fn state(&self) -> OverlayState {
        let Ok(text) = std::fs::read_to_string(self.state_path()) else {
            return OverlayState::default();
        };
        match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("叠加层状态文件损坏,按「使用内置版」处理: {e}");
                OverlayState::default()
            }
        }
    }

    /// 写状态。目录不存在会自动创建。
    pub fn write_state(&self, state: &OverlayState) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("创建叠加层目录失败: {}", self.root.display()))?;
        let text = serde_json::to_string_pretty(state)?;
        std::fs::write(self.state_path(), text)
            .with_context(|| format!("写入叠加层状态失败: {}", self.state_path().display()))?;
        Ok(())
    }

    /// 叠加层当前**实际应当生效**的版本目录。
    ///
    /// 三重校验,任一不过就返回 `None`(表示"用内置版"):
    /// 1. 状态里记了激活版本;
    /// 2. 该版本目录真实存在;
    /// 3. 它比内置版**更新** —— 内置反超时叠加层自动作废。
    ///
    /// `bundled` 是安装目录里内置的 deeptutor 版本(读不到就传 `None`,
    /// 此时跳过第 3 条校验)。
    pub fn effective_version(&self, bundled: Option<&str>) -> Option<String> {
        let state = self.state();
        let active = state.active.as_deref()?;

        let dir = self.version_dir(active);
        if !dir.is_dir() {
            log::warn!(
                "叠加层状态指向 {} 但目录不存在,退回内置版: {}",
                active,
                dir.display()
            );
            return None;
        }

        // 内置反超 -> 叠加层作废。
        if let Some(bundled) = bundled {
            match (Version::from_str(active), Version::from_str(bundled)) {
                (Ok(a), Ok(b)) if a <= b => {
                    log::info!(
                        "叠加层 {a} 已不高于内置版 {b},自动退回内置版(桌面壳升级后属正常现象)"
                    );
                    return None;
                }
                (Err(e), _) => {
                    log::warn!("叠加层版本号 {active:?} 不是合法 PEP 440 版本: {e}");
                    return None;
                }
                _ => {}
            }
        }

        Some(active.to_string())
    }

    /// 叠加层生效时的解包目录(要前置进 `PYTHONPATH` 的那个)。
    pub fn effective_dir(&self, bundled: Option<&str>) -> Option<PathBuf> {
        let v = self.effective_version(bundled)?;
        Some(self.version_dir(&v))
    }

    /// 清掉激活状态(退回到内置版)。不清任何文件,只改状态 ——
    /// 所以这一步几乎不可能失败,是可靠的"一键回滚"。
    pub fn deactivate(&self) -> Result<()> {
        let mut state = self.state();
        state.active = None;
        state.activated_at = None;
        state.packages.clear();
        self.write_state(&state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用**独立**的临时目录。
    ///
    /// 不能只按 pid 区分:cargo 默认并行跑测试,同一个 pid 下的多个用例
    /// 会共用目录,互相看到对方建的 `versions/<ver>/`,断言就会随机失败
    /// (`missing_version_dir_falls_back` 就栽在这上面)。
    fn tmp_overlay(tag: &str) -> Overlay {
        let dir = std::env::temp_dir().join(format!("dt-overlay-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Overlay { root: dir }
    }

    #[test]
    fn no_state_means_builtin() {
        let o = tmp_overlay("no-state");
        assert!(o.state().is_builtin());
        assert_eq!(o.effective_version(None), None);
    }

    #[test]
    fn missing_version_dir_falls_back() {
        let o = tmp_overlay("missing-dir");
        o.write_state(&OverlayState {
            active: Some("1.6.13".into()),
            ..Default::default()
        })
        .unwrap();
        // 目录没建,应当退回 None
        assert_eq!(o.effective_version(Some("1.6.12")), None);
    }

    #[test]
    fn activate_and_deactivate() {
        let o = tmp_overlay("activate");
        std::fs::create_dir_all(o.version_dir("1.6.13")).unwrap();
        o.write_state(&OverlayState {
            active: Some("1.6.13".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(o.effective_version(Some("1.6.12")), Some("1.6.13".into()));

        o.deactivate().unwrap();
        assert_eq!(o.effective_version(Some("1.6.12")), None);
    }

    #[test]
    fn bundled_overtaking_invalidates_overlay() {
        let o = tmp_overlay("overtake");
        std::fs::create_dir_all(o.version_dir("1.6.13")).unwrap();
        o.write_state(&OverlayState {
            active: Some("1.6.13".into()),
            ..Default::default()
        })
        .unwrap();
        // 内置升到 1.6.14 -> 叠加层作废
        assert_eq!(o.effective_version(Some("1.6.14")), None);
        // 内置持平 -> 也作废(用内置更干净)
        assert_eq!(o.effective_version(Some("1.6.13")), None);
    }

    #[test]
    fn corrupted_state_falls_back_to_builtin() {
        let o = tmp_overlay("corrupt");
        std::fs::create_dir_all(&o.root).unwrap();
        std::fs::write(o.root.join("state.json"), "{ this is not json").unwrap();
        // 坏状态文件不该让应用起不来,应当静默当作"用内置"
        assert!(o.state().is_builtin());
    }

    #[test]
    fn invalid_version_in_state_is_rejected() {
        let o = tmp_overlay("bad-version");
        std::fs::create_dir_all(o.version_dir("not-a-version")).unwrap();
        o.write_state(&OverlayState {
            active: Some("not-a-version".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(o.effective_version(Some("1.6.12")), None);
    }
}
