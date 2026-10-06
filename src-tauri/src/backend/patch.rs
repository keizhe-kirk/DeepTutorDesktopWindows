//! 本地补丁层（local patch layer）—— 与后端热更新叠加层**刻意不同**的机制。
//!
//! # 为什么需要它
//!
//! 后端热更新([`super::overlay`])解决的是「装更新的 deeptutor」,靠的是把新版
//! wheel 解到用户目录、前置进 `PYTHONPATH`。但有一类改动它天生表达不了:
//! **同版本代码修正** —— 比如给 `video_learning` 加一个 `bilibili` provider。
//! 这类补丁:
//!
//! 1. **不能走叠加层**。叠加层靠 `Version` 比较决定生死
//!    (`effective_version` 里`a <= b` 就作废),而补丁的版本号和内置版**相同**,
//!    一比较就等于自己把自己作废。
//! 2. **不能用 PYTHONPATH 整体遮蔽**。前置一个目录到 `PYTHONPATH` 会让
//!    Python 从那里找`deeptutor` 包,而只放一个 `service.py` 是不够的 ——
//!    同目录没有 `__init__.py` 的话包就不完整,补齐则等于复制整个 29 MB 的包。
//!
//! # 采用的机制：`sitecustomize.py` + import 钩子
//!
//! Python 启动时,若 `sys.path` 上存在 `sitecustomize`,会自动 import 它。
//! 于是把补丁目录放进 `PYTHONPATH`,再在`sitecustomize.py` 里装一个
//! [`sys.meta_path`] 钩子,在 `deeptutor.video_learning.service`
//! **被业务代码真正 import 的那一刻**改写它的模块全局 —— 此时原模块已经完整
//! 加载,我们只替换其中几个函数引用,不动一个字节的原始文件。
//!
//! 实测(内置 CPython 3.13.15):钩子能如期触发,改写生效且 YouTube 路径不受影响。
//!
//! # 目录布局
//!
//! ```text
//! %LOCALAPPDATA%\DeepTutor\patches\
//!   state.json          当前补丁状态(见 PatchState)
//!   sitecustomize.py    import 钩子(由补丁内容提供)
//!   dtpatch_bili\       补丁实现包
//!   bili_credentials.json  B 站登录凭据(用户扫码产生;绝不入库)
//! ```
//!
//! # 与叠加层的共存顺序
//!
//! 两者都在 `PYTHONPATH` 上,顺序是**叠加层在前、补丁层在后** —— 补丁必须看到
//! 叠加层里的 deeptutor 才能改写它。
//!
//! # 失效策略：宁可不打,不可打坏
//!
//! 补丁记录它是为哪个内置版写的(`for_bundled`)。桌面壳升级让内置版变了之后,
//! 补丁**自动停用**并记一行日志,而不是硬打上去把后端搞崩。
//! 停用只需改 `state.json`,与 [`Overlay::deactivate`] 一样可靠。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 补丁根目录名(挂在 `DEEPTUTOR_HOME` 之下)。
const PATCH_DIRNAME: &str = "patches";
/// 状态文件名。
const STATE_FILENAME: &str = "state.json";
/// 启动钩子文件名 —— Python 会在启动时自动 import 它。
pub const SITECUSTOMIZE_FILENAME: &str = "sitecustomize.py";

/// 补丁的持久状态。存 `state.json`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PatchState {
    /// 是否启用。`false` / 缺失 = 不打补丁。
    #[serde(default)]
    pub enabled: bool,
    /// 状态是否已初始化过。
    ///
    /// ★ 必须显式区分「从没播种过」与「用户主动停用过」：
    /// 两者在 [`enabled`] 上都是 `false`，但前者应当在播种时自动启用，
    /// 后者必须尊重用户选择。没有这个字段的话，停用功能在下次启动
    /// 播种时会被悄悄推翻 —— 而用户不会知道。
    #[serde(default)]
    pub initialized: bool,
    /// 这份补丁是为哪个内置版写的。
    ///
    /// 内置版一旦不同、而补丁内容又没跟着更新，补丁自动停用 ——
    /// 见 [`Patches::is_applicable`]。
    /// 上游一旦真的实现了 bilibili provider，用户就该用回内置版。
    #[serde(default)]
    pub for_bundled: Option<String>,
    /// 补丁标识(当前只有 `"bilibili"`)。
    #[serde(default)]
    pub id: Option<String>,
    /// 启用时间(RFC3339,展示用)。
    #[serde(default)]
    pub enabled_at: Option<String>,
}

impl PatchState {
    /// 是否处于「应当打补丁」的状态。
    ///
    /// 注意这里**故意不含版本比较** —— 与
    /// [`Overlay::effective_version`](super::overlay::Overlay::effective_version)
    /// 的关键区别。同版本补丁正是补丁层存在的理由。
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// 本地补丁层。
#[derive(Debug, Clone)]
pub struct Patches {
    root: PathBuf,
}

impl Patches {
    /// 按 `DEEPTUTOR_HOME` 的约定定位补丁目录。
    pub fn detect(home: Option<PathBuf>) -> Self {
        let base = home.unwrap_or_else(|| {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("DeepTutor")
        });
        Self {
            root: base.join(PATCH_DIRNAME),
        }
    }

    /// 补丁根目录(`...\DeepTutor\patches`)。**不保证已存在**。
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn state_path(&self) -> PathBuf {
        self.root.join(STATE_FILENAME)
    }

    /// 读状态。文件不存在 / 解析失败一律当作「不启用」——
    /// 状态损坏时必须静默退回无补丁,绝不能让应用起不来。
    pub fn state(&self) -> PatchState {
        let Ok(text) = std::fs::read_to_string(self.state_path()) else {
            return PatchState::default();
        };
        match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("补丁状态文件损坏,按「不启用」处理: {e}");
                PatchState::default()
            }
        }
    }

    /// 写状态。目录不存在会自动创建。
    pub fn write_state(&self, state: &PatchState) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("创建补丁目录失败: {}", self.root.display()))?;
        std::fs::write(self.state_path(), serde_json::to_string_pretty(state)?)
            .with_context(|| format!("写入补丁状态失败: {}", self.state_path().display()))?;
        Ok(())
    }

    /// 这份补丁对当前内置版**是否适用**。
    ///
    /// -补丁没启用 → 不适用(不算错误)。
    /// - 没记 `for_bundled`(老状态/手改)→ 适用,交给 Python 侧兜底。
    /// - 内置版与记录一致 → 适用。
    /// - 内置版已变→ **不适用**,补丁自动停用。
    ///
    /// `bundled` 传 `None`(读不到内置版)时跳过校验,与叠加层同一取舍。
    pub fn is_applicable(&self, bundled: Option<&str>) -> bool {
        let state = self.state();
        if !state.is_enabled() {
            return false;
        }
        let (Some(for_bundled), Some(bundled)) = (state.for_bundled.as_deref(), bundled) else {
            return true;
        };
        if for_bundled == bundled {
            return true;
        }
        log::info!(
            "补丁 {} 是为内置版 {for_bundled} 写的,当前内置版是 {bundled},自动停用补丁",
            state.id.as_deref().unwrap_or("(未命名)")
        );
        false
    }

    /// 补丁生效时要前置进 `PYTHONPATH` 的目录。
    ///
    /// 除了目录本身存在,还必须真的有 `sitecustomize.py` —— 否则 Python 不会
    /// 加载任何东西,把一个空目录塞进 `PYTHONPATH` 只会污染 `sys.path`。
    pub fn effective_dir(&self, bundled: Option<&str>) -> Option<PathBuf> {
        if !self.is_applicable(bundled) {
            return None;
        }
        let hook = self.root.join(SITECUSTOMIZE_FILENAME);
        if !hook.is_file() {
            log::warn!("补丁已启用但缺少 {SITECUSTOMIZE_FILENAME},本次不注入: {}", self.root.display());
            return None;
        }
        Some(self.root.clone())
    }

    /// 停用补丁。不删任何文件,只改状态 —— 所以这一步几乎不可能失败。
    pub fn deactivate(&self) -> Result<()> {
        let mut state = self.state();
        state.enabled = false;
        self.write_state(&state)
    }

    /// 首次运行时把补丁从安装目录播种到用户目录。
    ///
    /// 安装目录是只读的（`perMachine` 装在 Program Files),而且**绝不能**把
    /// 用户凭据写进去 —— 所以用户目录是补丁的唯一落点,安装目录只当只读源。
    ///
    /// 播种是**覆盖式**的:安装目录里带了新版补丁就同步过去,但
    /// `bili_credentials.json` 永远不碰（它不在安装目录里,也不该被覆盖）。
    ///
    /// `resource_dir` 来自 `app.path().resource_dir()`;不同打包后端落点有
    /// 差异,所以按顺序试几个,与 [`super::runtime::BundledRuntimes::detect`]
    /// 同一套策略。
    pub fn seed_from_bundle(resource_dir: Option<&Path>, bundled: Option<&str>) -> Option<PathBuf> {
        let source = Self::find_bundled_patches(resource_dir)?;
        // 目标目录必须与 `effective_dir` 用同一套定位规则(都尊重
        // DEEPTUTOR_HOME),否则自检改了 HOME 会两边指向不同地方。
        let target_root = Self::detect(Some(super::runtime::effective_home()))
            .root
            .to_path_buf();
        Self::seed_into(&source, &target_root, bundled)
    }

    /// 播种本体。目标目录显式传入,便于测试隔离(不碰真实用户目录)。
    ///
    /// 返回 `Some(root)` 表示本次真的写了东西(可用于打日志),
    /// `None` 表示已是最新、无需改动。
    fn seed_into(source: &Path, target_root: &Path, bundled: Option<&str>) -> Option<PathBuf> {
        // ---- 状态:先确保已初始化,再谈内容 ----
        //
        // 顺序很重要。★ 播种必须写 `enabled: true`,否则 `effective_dir` 的
        // `is_enabled()` 永远为假 —— 文件都在、钩子也在,却一份补丁都不打,
        // 而且没有任何症状(后端正常启动,只是 B 站链接照旧报不支持),
        // 是最难查的一类故障。
        //
        // 同时只在**首次**播种时启用:`initialized` 之后一律保留用户的选择,
        // 否则「停用补丁」会被下次启动的播种悄悄推翻。
        let mut state = self_state(target_root);
        let mut state_dirty = false;
        if !state.initialized {
            state.initialized = true;
            state.enabled = true;
            state.id = Some("bilibili".to_string());
            state.enabled_at = Some(chrono::Utc::now().to_rfc3339());
            state_dirty = true;
        }
        if state.for_bundled.is_none() {
            state.for_bundled = bundled.map(str::to_string);
            state_dirty = true;
        }

        // 已播种过同一份内容 -> 无需拷贝。但状态可能还没落盘(例如上一次写盘失败),
        // 所以这里仍要把状态补写一次再收工。
        let installed_hook = target_root.join(SITECUSTOMIZE_FILENAME);
        let same = match (
            std::fs::read(source.join(SITECUSTOMIZE_FILENAME)),
            std::fs::read(&installed_hook),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if same {
            if state_dirty {
                let _ = write_state_at(target_root, &state);
            }
            return None;
        }

        let copy_result = (|| -> Result<()> {
            std::fs::create_dir_all(target_root)
                .with_context(|| format!("创建补丁目录失败: {}", target_root.display()))?;
            // ★ 只拷这三个名字:sitecustomize + 补丁包。
            // 绝不能整目录搬 —— 用户目录里还放着 `bili_credentials.json`,
            // 那是扫码产生的凭据,覆盖式播种会把它连同旧内容一起抹掉。
            // 新增补丁文件时必须同步改这里(测试 `credentials_survive_reseeding`
            // 只钉住了「凭据不被碰」,不钉住「该拷的都拷了」)。
            for name in ["sitecustomize.py", "dtpatch_bili"] {
                let from = source.join(name);
                let to = target_root.join(name);
                if from.is_dir() {
                    copy_dir(&from, &to)?;
                } else if from.is_file() {
                    std::fs::copy(&from, &to).with_context(|| {
                        format!("复制补丁文件失败: {} -> {}", from.display(), to.display())
                    })?;
                }
            }
            Ok(())
        })();

        if let Err(e) = copy_result {
            log::warn!("播种本地补丁失败,B 站支持本次不可用: {e:#}");
            return None;
        }

        let _ = write_state_at(target_root, &state);
        log::info!(
            "已播种本地补丁(id={}, for_bundled={}): {}",
            state.id.as_deref().unwrap_or("-"),
            state.for_bundled.as_deref().unwrap_or("-"),
            target_root.display()
        );
        Some(target_root.to_path_buf())
    }

    /// 补丁内容在安装目录里的只读源。
    ///
    /// 候选来源两处:显式传入的资源目录,以及**内置运行时根目录的父目录**
    /// —— 后者与 [`super::runtime::BundledRuntimes::detect`] 的兜底探测同源,
    /// 所以不需要 Tauri 的 `app.path().resource_dir()`,后端启动阶段也能用。
    fn find_bundled_patches(resource_dir: Option<&Path>) -> Option<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(dir) = resource_dir {
            roots.push(dir.join("patches"));
            roots.push(dir.join("resources").join("patches"));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                roots.push(exe_dir.join("patches"));
                if let Some(parent) = exe_dir.parent() {
                    roots.push(parent.join("patches"));
                }
            }
        }
        roots.into_iter().find(|dir| {
            dir.join(SITECUSTOMIZE_FILENAME).is_file() && dir.join("dtpatch_bili").is_dir()
        })
    }

    /// 从内置运行时的落点推导资源目录,再播种补丁。
    ///
    /// `bundled_root` 是 `<资源根>/runtimes`,补丁在 `<资源根>/patches`。
    pub fn seed_beside_runtimes(bundled_root: Option<&Path>, bundled: Option<&str>) -> Option<PathBuf> {
        let parent = bundled_root?.parent()?;
        Self::seed_from_bundle(Some(parent), bundled)
    }
}

/// 读某个补丁根目录的状态。播种逻辑只拿到了根目录（不是 [`Patches`] 实例），
/// 所以走自由函数而不是 `Patches::state`。
fn self_state(root: &Path) -> PatchState {
    Patches { root: root.to_path_buf() }.state()
}

/// 写状态（自由函数版）。
fn write_state_at(root: &Path, state: &PatchState) -> Result<()> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("创建补丁目录失败: {}", root.display()))?;
    std::fs::write(root.join(STATE_FILENAME), serde_json::to_string_pretty(state)?)
        .with_context(|| format!("写入补丁状态失败: {}", root.join(STATE_FILENAME).display()))?;
    Ok(())
}

/// 递归复制目录。**覆盖式**写入,不复用删除 API（删目录在本机会被安全策略拦）。
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)
        .with_context(|| format!("创建目录失败: {}", to.display()))?;
    for entry in std::fs::read_dir(from)
        .with_context(|| format!("读取目录失败: {}", from.display()))?
    {
        let entry = entry?;
        // 跳过 __pycache__（目录也要跳）:源目录里若有编译缓存,拷过去只会带
        // stale 字节码。判断必须在 is_dir 分支**之前**,否则会递归进去。
        if entry.file_name() == "__pycache__" {
            continue;
        }
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst).with_context(|| {
                format!("复制补丁文件失败: {} -> {}", src.display(), dst.display())
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_patches(tag: &str) -> Patches {
        let dir = std::env::temp_dir().join(format!("dt-patch-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Patches { root: dir }
    }

    fn install_hook(p: &Patches) {
        std::fs::create_dir_all(p.root()).unwrap();
        std::fs::write(
            p.root().join(SITECUSTOMIZE_FILENAME),
            "# test hook\n",
        )
        .unwrap();
    }

    #[test]
    fn disabled_by_default() {
        let p = tmp_patches("default");
        assert!(!p.state().is_enabled());
        assert_eq!(p.effective_dir(Some("1.6.12")), None);
    }

    #[test]
    fn same_version_patch_survives() {
        let p = tmp_patches("same");
        install_hook(&p);
        p.write_state(&PatchState {
            initialized: true,
            enabled: true,
            for_bundled: Some("1.6.12".into()),
            id: Some("bilibili".into()),
            enabled_at: None,
        })
        .unwrap();
        // ★ 这正是与叠加层的核心差异：同版本不被作废
        assert!(p.is_applicable(Some("1.6.12")));
        assert_eq!(
            p.effective_dir(Some("1.6.12")),
            Some(p.root().to_path_buf())
        );
    }

    #[test]
    fn bundled_change_disables_patch() {
        let p = tmp_patches("drift");
        install_hook(&p);
        p.write_state(&PatchState {
            initialized: true,
            enabled: true,
            for_bundled: Some("1.6.12".into()),
            id: Some("bilibili".into()),
            enabled_at: None,
        })
        .unwrap();
        // 桌面壳升级带来自带新版 -> 补丁停用而不是硬打
        assert!(!p.is_applicable(Some("1.6.13")));
        assert_eq!(p.effective_dir(Some("1.6.13")), None);
    }

    #[test]
    fn missing_hook_is_not_injected() {
        let p = tmp_patches("nohook");
        // 状态启用但没放 sitecustomize.py -> 不注入,别污染 sys.path
        p.write_state(&PatchState {
            initialized: true,
            enabled: true,
            for_bundled: Some("1.6.12".into()),
            id: Some("bilibili".into()),
            enabled_at: None,
        })
        .unwrap();
        assert_eq!(p.effective_dir(Some("1.6.12")), None);
    }

    #[test]
    fn unknown_bundled_skips_check() {
        let p = tmp_patches("nobundled");
        install_hook(&p);
        p.write_state(&PatchState {
            initialized: true,
            enabled: true,
            for_bundled: Some("1.6.12".into()),
            id: Some("bilibili".into()),
            enabled_at: None,
        })
        .unwrap();
        // 读不到内置版 -> 交给 Python 侧兜底,这里放行
        assert!(p.is_applicable(None));
    }

    #[test]
    fn missing_for_bundled_is_permissive() {
        let p = tmp_patches("noforbundled");
        install_hook(&p);
        p.write_state(&PatchState {
            initialized: true,
            enabled: true,
            for_bundled: None,
            id: Some("bilibili".into()),
            enabled_at: None,
        })
        .unwrap();
        // 没有基线记录 -> 放行,Python 侧按能力探测
        assert!(p.is_applicable(Some("1.9.9")));
    }

    #[test]
    fn corrupted_state_falls_back_to_disabled() {
        let p = tmp_patches("corrupt");
        std::fs::create_dir_all(p.root()).unwrap();
        std::fs::write(p.state_path(), "{ not json").unwrap();
        assert!(!p.state().is_enabled());
        assert_eq!(p.effective_dir(Some("1.6.12")), None);
    }

    #[test]
    fn deactivate_keeps_files() {
        let p = tmp_patches("deact");
        install_hook(&p);
        p.write_state(&PatchState {
            initialized: true,
            enabled: true,
            for_bundled: Some("1.6.12".into()),
            id: Some("bilibili".into()),
            enabled_at: None,
        })
        .unwrap();
        p.deactivate().unwrap();
        assert!(!p.state().is_enabled());
        // 文件还在 —— 可随时重新启用,且回滚不依赖删除
        assert!(p.root().join(SITECUSTOMIZE_FILENAME).is_file());
    }

    /// 建一个假的「安装目录」资源布局：``<res>/patches/{sitecustomize.py,dtpatch_bili/}``
    fn fake_bundle(tag: &str, hook_body: &str) -> PathBuf {
        let res = std::env::temp_dir()
            .join(format!("dt-patch-bundle-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&res);
        let dir = res.join("patches");
        let pkg = dir.join("dtpatch_bili");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(dir.join(SITECUSTOMIZE_FILENAME), hook_body).unwrap();
        std::fs::write(pkg.join("__init__.py"), "# bili patch\n").unwrap();
        std::fs::write(pkg.join("search.py"), "# search\n").unwrap();
        // 模拟误入安装目录的编译缓存 —— 播种必须跳过它
        let cache = pkg.join("__pycache__");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("x.pyc"), b"\x00stale").unwrap();
        res
    }

    #[test]
    fn seeding_finds_bundle_under_resource_dir() {
        let res = fake_bundle("seed", "# v1\n");
        let found = Patches::find_bundled_patches(Some(&res));
        assert_eq!(
            found.as_deref(),
            Some(res.join("patches").as_path()),
            "应当找到 <resource_dir>/patches"
        );
    }

    #[test]
    fn seeding_requires_both_hook_and_package() {
        // 只有 hook 没有包 -> 该目录不算合法的补丁源。
        //
        // 注意断言的是「这个目录没被选中」,而不是「什么都没找到」——
        // 函数还有 current_exe 兜底探测,而仓库里就躺着真的 src-tauri/patches,
        // 所以它可能合法地命中别处。用 contains 而非 is_none 才能真正守住
        // 「半套补丁会被认出来」这个不变量。
        let res = std::env::temp_dir()
            .join(format!("dt-patch-bundle-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&res);
        let dir = res.join("patches");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SITECUSTOMIZE_FILENAME), "# only hook").unwrap();
        let found = Patches::find_bundled_patches(Some(&res));
        assert!(
            !found.as_deref().map(|p| p.starts_with(&res)).unwrap_or(false),
            "不完整的补丁目录不应被选中,却选中了 {found:?}"
        );
    }

    #[test]
    fn copy_dir_skips_pycache() {
        let res = fake_bundle("copy", "# v1\n");
        let src = res.join("patches").join("dtpatch_bili");
        let dest = std::env::temp_dir().join(format!("dt-patch-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        copy_dir(&src, &dest).unwrap();
        assert!(dest.join("__init__.py").is_file());
        assert!(dest.join("search.py").is_file());
        // stale 字节码不该被带过去
        assert!(!dest.join("__pycache__").exists());
    }

    #[test]
    fn credentials_survive_reseeding() {
        // 关键性质:重新播种补丁**绝不能**动用户凭据。
        // 凭据不在安装目录里,播种只拷贝 sitecustomize.py 与 dtpatch_bili/,
        // 这个测试把该不变量钉死。
        let res = fake_bundle("creds", "# v1\n");
        let bundle = res.join("patches");
        let user_root = std::env::temp_dir()
            .join(format!("dt-patch-creds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&user_root);
        std::fs::create_dir_all(&user_root).unwrap();
        let cred_file = user_root.join("bili_credentials.json");
        std::fs::write(&cred_file, r#"{"SESSDATA":"secret"}"#).unwrap();

        // 模拟一次全量播种(只碰这两个名字)
        for name in [SITECUSTOMIZE_FILENAME, "dtpatch_bili"] {
            let from = bundle.join(name);
            let to = user_root.join(name);
            if from.is_dir() {
                copy_dir(&from, &to).unwrap();
            } else {
                std::fs::copy(&from, &to).unwrap();
            }
        }

        assert!(cred_file.is_file(), "凭据文件必须原样保留");
        let text = std::fs::read_to_string(&cred_file).unwrap();
        assert!(text.contains("secret"), "凭据内容必须未被覆盖");
    }

    /// ★ 播种后必须真的生效。
    ///
    /// 这是本文件最要紧的不变量:`effective_dir` 要求 `enabled`,而播种如果
    /// 不写它,就会出现「文件都在、后端正常起、B 站就是不支持」—— 且没有任何
    /// 报错提示。之前正是踩了这个坑。
    #[test]
    fn seeding_enables_the_patch() {
        let res = fake_bundle("enable", "# v1\n");
        let source = res.join("patches");
        let user_root = std::env::temp_dir()
            .join(format!("dt-patch-enable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&user_root);

        assert!(Patches::seed_into(&source, &user_root, Some("1.6.12")).is_some());

        let p = Patches { root: user_root.clone() };
        let state = p.state();
        assert!(state.initialized, "状态必须标记为已初始化");
        assert!(state.is_enabled(), "首次播种必须自动启用补丁");
        assert_eq!(state.for_bundled.as_deref(), Some("1.6.12"));
        assert!(user_root.join(SITECUSTOMIZE_FILENAME).is_file());
        assert!(user_root.join("dtpatch_bili").is_dir());
        assert_eq!(
            p.effective_dir(Some("1.6.12")),
            Some(user_root.clone()),
            "播种完必须立刻可注入 PYTHONPATH"
        );
    }

    /// 停用之后,后续播种不得偷偷把用户的选择推翻。
    #[test]
    fn reseeding_respects_a_user_disable() {
        let res = fake_bundle("respect", "# v1\n");
        let source = res.join("patches");
        let user_root = std::env::temp_dir()
            .join(format!("dt-patch-respect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&user_root);

        Patches::seed_into(&source, &user_root, Some("1.6.12"));
        let p = Patches { root: user_root.clone() };
        p.deactivate().unwrap();
        assert!(!p.state().is_enabled());

        // 内容没变 -> 不该拷贝,但状态也必须保持停用。
        assert!(Patches::seed_into(&source, &user_root, Some("1.6.12")).is_none());
        assert!(
            !p.state().is_enabled(),
            "用户停用后,播种不得重新启用"
        );
        assert_eq!(p.effective_dir(Some("1.6.12")), None);
    }

    /// 停用 + 壳升级带来新补丁内容:内容要同步,但仍尊重用户的停用。
    #[test]
    fn update_propagates_content_but_keeps_disabled() {
        let res = fake_bundle("update-v1", "# v1\n");
        let source = res.join("patches");
        let user_root = std::env::temp_dir()
            .join(format!("dt-patch-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&user_root);

        Patches::seed_into(&source, &user_root, Some("1.6.12"));
        let p = Patches { root: user_root.clone() };
        p.deactivate().unwrap();

        // 模拟新壳版本带了 v2 补丁。
        std::fs::write(source.join(SITECUSTOMIZE_FILENAME), "# v2\n").unwrap();
        assert!(
            Patches::seed_into(&source, &user_root, Some("1.6.13")).is_some(),
            "内容变了就该重新拷贝"
        );

        let text = std::fs::read_to_string(user_root.join(SITECUSTOMIZE_FILENAME)).unwrap();
        assert_eq!(text, "# v2\n", "新补丁内容必须落到用户目录");
        assert!(
            !p.state().is_enabled(),
            "内容更新不等于用户同意启用"
        );
    }
}