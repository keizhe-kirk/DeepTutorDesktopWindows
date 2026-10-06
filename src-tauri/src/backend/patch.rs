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
//!   patch.json          补丁清单(见 PatchManifest)
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
//! 补丁通过 [`PatchManifest::verified_deeptutor`] 声明**自己在哪些 deeptutor
//! 版本上验证过**。生效版不在清单里就不打,只记一行日志。
//!
//! ★ 清单必须**随补丁一起发布**,不能靠运行时 stamping。之前这里记的是
//! 「播种那一刻碰巧在跑的版本」(`for_bundled`,由 [`Patches::seed_into`] 写入
//! `state.json`),于是后端热更新一激活新版,基线就永久陈旧 —— 而且
//! `if state.for_bundled.is_none()` 意味着它**再也不会被刷新**。用户表现是
//! 「文件都在、后端正常起、菜单却报补丁未就绪,而且重启多少次都没用」。
//! 那是运行期状态被当成静态契约用,必然烂尾。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 补丁根目录名(挂在 `DEEPTUTOR_HOME` 之下)。
const PATCH_DIRNAME: &str = "patches";
/// 状态文件名。
const STATE_FILENAME: &str = "state.json";
/// 补丁清单文件名。**随补丁一起发布**,声明这份补丁验证过哪些 deeptutor 版本。
const MANIFEST_FILENAME: &str = "patch.json";
/// 启动钩子文件名 —— Python 会在启动时自动 import 它。
pub const SITECUSTOMIZE_FILENAME: &str = "sitecustomize.py";
/// 补丁包（真正的 Python 实现）所在的目录名。
pub const PATCH_PACKAGE_DIRNAME: &str = "dtpatch_bili";

/// 播种时从安装目录带过去的条目。
///
/// ★ **比对与拷贝都吃这一份清单** —— 加新补丁文件时只改这里，
/// 两边不会漂。之前比对只看 `sitecustomize.py` + `patch.json`，于是
/// 「只改了补丁包里某个 `.py`」的更新会被判为「没变」而跳过拷贝，
/// 用户目录里的补丁永远停在旧版，且没有任何症状。
const SEEDED_ENTRIES: [&str; 3] = [MANIFEST_FILENAME, SITECUSTOMIZE_FILENAME, PATCH_PACKAGE_DIRNAME];

/// 补丁清单(`patch.json`)。
///
/// ★ 这是「补丁支持哪些 deeptutor 版本」的**唯一真相来源**,随补丁内容一起
/// 从安装目录播种过来。它必须由**补丁作者**在发版时写死,而不是运行时推测:
/// 补丁改的是 deeptutor 内部模块的函数引用,能不能用只有写补丁的人知道。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchManifest {
    /// 补丁标识(当前只有 `"bilibili"`)。
    #[serde(default)]
    pub id: String,
    /// 验证过的 deeptutor 版本号。
    ///
    /// 比对时按字符串精确匹配(与旧的等值校验同口径)。**空清单 = 不拦任何
    /// 版本**,补丁一律打上去 —— 与叠加层「读不到就放行」同一取舍。
    #[serde(default)]
    pub verified_deeptutor: Vec<String>,
}

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
    /// 这份补丁播种时,生效的 deeptutor 版本是多少。
    ///
    /// ★ **纯诊断信息,不参与任何判定。**
    ///
    /// 它曾经是适用性判据的基线,但那是错的:播种发生的那一刻"碰巧在跑什么
    /// 版本"与"这份补丁代码在什么版本上验证过"是两件事。后端热更新一激活
    /// 新版,这个字段就永久陈旧,而它又只在 `None` 时才被写 —— 于是补丁再也
    /// 不会生效,用户重启多少次都没用。判定已改由 [`PatchManifest`] 承担。
    ///
    /// 保留它只是为了排查时能一眼看出「播种那一刻是什么环境」。
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

    /// 读补丁清单。文件不存在 / 解析失败 / 内容为空 → `None`(视为不拦版本)。
    pub fn manifest(&self) -> Option<PatchManifest> {
        let text = std::fs::read_to_string(self.root.join(MANIFEST_FILENAME)).ok()?;
        match serde_json::from_str::<PatchManifest>(&text) {
            Ok(m) if !m.verified_deeptutor.is_empty() => Some(m),
            Ok(_) => None,
            Err(e) => {
                log::warn!(
                    "补丁清单解析失败,本次不拦版本: {e}({})",
                    self.root.join(MANIFEST_FILENAME).display()
                );
                None
            }
        }
    }

    /// 这份补丁对当前**生效版** deeptutor 是否适用。
    ///
    /// - 补丁没启用 → 不适用(不算错误)。
    /// - 没有清单(老补丁 / 读不到)→ 适用,交给 Python 侧兜底。
    /// - 生效版在 [`PatchManifest::verified_deeptutor`] 里 → 适用。
    /// - 不在 → **不适用**,补丁自动停用并记一行日志。
    ///
    /// `effective` 传 `None`(读不到生效版)时跳过校验,与叠加层同一取舍。
    ///
    /// ★ 收 `None` 的语义是「不知道」而不是「任何版本」,所以这里不能反过来说
    /// 「清单存在就必须命中」—— 读不到版本时放行,与叠加层保持一致。
    pub fn is_applicable(&self, effective: Option<&str>) -> bool {
        self.inapplicable_reason(effective).is_none()
    }

    /// [`Patches::is_applicable`] 的可诊断版本:不适用时给出人话原因。
    ///
    /// 单独抽出来是因为「为什么 B 站不可用」必须有答案 —— 补丁静默失效过一次,
    /// 现象是「后端正常起、菜单报未就绪」,没有任何线索指向真实原因。
    pub fn inapplicable_reason(&self, effective: Option<&str>) -> Option<String> {
        let state = self.state();
        if !state.is_enabled() {
            return Some("补丁未启用".to_string());
        }
        let Some(manifest) = self.manifest() else {
            // 没有清单 -> 不拦。与叠加层「读不到就放行」同一取舍。
            return None;
        };
        let Some(effective) = effective else {
            return None;
        };
        if manifest.verified_deeptutor.iter().any(|v| v == effective) {
            return None;
        }
        Some(format!(
            "补丁 {} 验证过的 deeptutor 版本为 {:?},当前生效的是 {}",
            if manifest.id.is_empty() { "(未命名)" } else { &manifest.id },
            manifest.verified_deeptutor,
            effective
        ))
    }

    /// 补丁生效时要前置进 `PYTHONPATH` 的目录。
    ///
    /// 除了目录本身存在,还必须真的有 `sitecustomize.py` —— 否则 Python 不会
    /// 加载任何东西,把一个空目录塞进 `PYTHONPATH` 只会污染 `sys.path`。
    pub fn effective_dir(&self, effective: Option<&str>) -> Option<PathBuf> {
        if !self.is_applicable(effective) {
            log::info!(
                "本地补丁未生效: {}",
                self.inapplicable_reason(effective).unwrap_or_default()
            );
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

        // ---- 内容比对与拷贝 ----
        //
        // ★ 参与比对的**必须和拷贝名单是同一份清单**（[`SEEDED_ENTRIES`]）。
        // 之前只比对 sitecustomize + 清单，于是「只改了补丁包里某个 .py」
        // 这类更新**永远落不到用户目录** —— 比对说「没变」，跳过拷贝。
        // 症状是新装用户功能残缺，而机器上明明有那份新补丁，最难查。
        let contents_match = SEEDED_ENTRIES
            .iter()
            .all(|name| entry_matches(&source.join(name), &target_root.join(name)));
        if contents_match {
            if state_dirty {
                let _ = write_state_at(target_root, &state);
            }
            return None;
        }

        let copy_result = (|| -> Result<()> {
            std::fs::create_dir_all(target_root)
                .with_context(|| format!("创建补丁目录失败: {}", target_root.display()))?;
            // ★ 只拷 [`SEEDED_ENTRIES`] 里这几个名字。
            // 绝不能整目录搬 —— 用户目录里还放着 `bili_credentials.json`,
            // 那是扫码产生的凭据,覆盖式播种会把它连同旧内容一起抹掉。
            for name in SEEDED_ENTRIES {
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
            "已播种本地补丁(id={}, 播种时生效版={}, 验证版本={:?}): {}",
            state.id.as_deref().unwrap_or("-"),
            state.for_bundled.as_deref().unwrap_or("-"),
            Patches { root: target_root.to_path_buf() }
                .manifest()
                .map(|m| m.verified_deeptutor)
                .unwrap_or_default(),
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
            dir.join(SITECUSTOMIZE_FILENAME).is_file()
                && dir.join(PATCH_PACKAGE_DIRNAME).is_dir()
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

/// 源与目标里的某个条目（文件或目录）内容是否一致。
///
/// 两边都没有算一致（老补丁没有清单时的兼容路径，与播种时跳过缺失源同语义）。
fn entry_matches(from: &Path, to: &Path) -> bool {
    if from.is_dir() || to.is_dir() {
        return dir_contents_match(from, to);
    }
    match (std::fs::read(from), std::fs::read(to)) {
        (Ok(a), Ok(b)) => a == b,
        (Err(_), Err(_)) => true,
        _ => false,
    }
}

/// 递归比对目录内容。跳过 `__pycache__` —— 必须与 [`copy_dir`] 同规则，
/// 否则目标里跑出来的编译缓存会让比对**永不相等**，于是每次启动都重播种。
///
/// 两边都没有该目录算一致（见 [`entry_matches`]）。
fn dir_contents_match(from: &Path, to: &Path) -> bool {
    if !from.is_dir() && !to.is_dir() {
        return true;
    }
    match (collect_tree(from), collect_tree(to)) {
        (Some(a), Some(b)) => a == b,
        // 任意一边读不动就当作不一致：宁可多拷一次，也不要漏掉更新。
        _ => false,
    }
}

/// 把目录收成「排序后的 (相对路径, 内容)」列表，便于直接比等。
///
/// 补丁包总共几十 KB，读进内存比对是零成本；换来的是**逐字节确定性**，
/// 不依赖任何哈希实现（`DefaultHasher` 的取值会随 Rust 版本变，
/// 那会造成无谓的重播种）。
fn collect_tree(root: &Path) -> Option<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    collect_into(root, root, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Some(out)
}

fn collect_into(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Option<()> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        // 与 `copy_dir` 同规则：编译缓存不参与比对。
        if entry.file_name() == "__pycache__" {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            collect_into(root, &path, out)?;
        } else {
            out.push((rel, std::fs::read(&path).ok()?));
        }
    }
    Some(())
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

    /// 写一份补丁清单 —— 白名单校验的输入。
    fn install_manifest(p: &Patches, verified: &[&str]) {
        std::fs::create_dir_all(p.root()).unwrap();
        let m = PatchManifest {
            id: "bilibili".into(),
            verified_deeptutor: verified.iter().map(|s| s.to_string()).collect(),
        };
        std::fs::write(
            p.root().join(MANIFEST_FILENAME),
            serde_json::to_string_pretty(&m).unwrap(),
        )
        .unwrap();
    }

    fn enabled_state() -> PatchState {
        PatchState {
            initialized: true,
            enabled: true,
            for_bundled: Some("1.6.12".into()),
            id: Some("bilibili".into()),
            enabled_at: None,
        }
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
        install_manifest(&p, &["1.6.12"]);
        p.write_state(&enabled_state()).unwrap();
        // ★ 这正是与叠加层的核心差异：同版本不被作废
        assert!(p.is_applicable(Some("1.6.12")));
        assert_eq!(
            p.effective_dir(Some("1.6.12")),
            Some(p.root().to_path_buf())
        );
    }

    #[test]
    fn unverified_version_disables_patch() {
        let p = tmp_patches("drift");
        install_hook(&p);
        install_manifest(&p, &["1.6.12"]);
        p.write_state(&enabled_state()).unwrap();
        // 生效版不在清单里 -> 补丁停用而不是硬打
        assert!(!p.is_applicable(Some("1.6.13")));
        assert_eq!(p.effective_dir(Some("1.6.13")), None);
        // 且必须能说清为什么,否则用户只看到「未就绪」而无处排查
        let why = p.inapplicable_reason(Some("1.6.13")).unwrap();
        assert!(why.contains("1.6.13"), "原因里应含当前生效版: {why}");
    }

    /// ★ 本次真机故障的回归测试。
    ///
    /// 用户已热更新到 1.6.13（生效版），而补丁播种时记下的基线是内置版
    /// 1.6.12 —— 旧设计拿"基线 == 生效版"做等值校验，于是补丁被判为不适用，
    /// 菜单报"未就绪"，而且**重启多少次都不会好**（基线只在 None 时才写）。
    ///
    /// 现在判据是「清单白名单」，热更新到已验证的版本必须照常生效。
    #[test]
    fn hot_updated_within_verified_set_still_applies() {
        let p = tmp_patches("hotupdate");
        install_hook(&p);
        // 补丁作者声明 1.6.12 与 1.6.13 都验证过
        install_manifest(&p, &["1.6.12", "1.6.13"]);
        // 播种时生效版是 1.6.12（基线照旧记成 1.6.12，纯诊断用）
        p.write_state(&enabled_state()).unwrap();
        // 用户随后热更新到 1.6.13 -> 生效版变了，但仍在白名单内
        assert!(
            p.is_applicable(Some("1.6.13")),
            "已验证的版本必须在热更新后继续生效"
        );
        assert_eq!(p.effective_dir(Some("1.6.13")), Some(p.root().to_path_buf()));
    }

    /// 反面：白名单必须真的拦人，不能因为"基线字段还在"就放行。
    #[test]
    fn stale_baseline_does_not_grant_applicability() {
        let p = tmp_patches("stalebase");
        install_hook(&p);
        install_manifest(&p, &["1.6.12"]);
        // 基线恰好等于当前生效版 —— 旧设计在这里会放行
        p.write_state(&enabled_state()).unwrap();
        assert_eq!(p.state().for_bundled.as_deref(), Some("1.6.12"));
        assert!(
            !p.is_applicable(Some("1.6.13")),
            "基线字段不得再作为放行依据"
        );
    }

    #[test]
    fn missing_hook_is_not_injected() {
        let p = tmp_patches("nohook");
        // 状态启用但没放 sitecustomize.py -> 不注入,别污染 sys.path
        install_manifest(&p, &["1.6.12"]);
        p.write_state(&enabled_state()).unwrap();
        assert_eq!(p.effective_dir(Some("1.6.12")), None);
    }

    #[test]
    fn unknown_effective_skips_check() {
        let p = tmp_patches("nobundled");
        install_hook(&p);
        install_manifest(&p, &["1.6.12"]);
        p.write_state(&enabled_state()).unwrap();
        // 读不到生效版 -> 交给 Python 侧兜底,这里放行
        assert!(p.is_applicable(None));
    }

    /// 老补丁没有清单文件 -> 不拦版本，交给 Python 侧能力探测。
    #[test]
    fn missing_manifest_is_permissive() {
        let p = tmp_patches("nomanifest");
        install_hook(&p);
        p.write_state(&enabled_state()).unwrap();
        assert_eq!(p.manifest(), None);
        assert!(p.is_applicable(Some("1.9.9")));
    }

    /// 空清单等同于没清单 -> 放行（不能因为写了空数组就把补丁全禁了）。
    #[test]
    fn empty_manifest_is_permissive() {
        let p = tmp_patches("emptymanifest");
        install_hook(&p);
        install_manifest(&p, &[]);
        p.write_state(&enabled_state()).unwrap();
        assert_eq!(p.manifest(), None);
        assert!(p.is_applicable(Some("1.9.9")));
    }

    /// 清单损坏时必须放行而不是禁掉 —— 宁可不拦，不可拦住。
    #[test]
    fn corrupted_manifest_is_permissive() {
        let p = tmp_patches("badmanifest");
        install_hook(&p);
        std::fs::create_dir_all(p.root()).unwrap();
        std::fs::write(p.root().join(MANIFEST_FILENAME), "{ not json").unwrap();
        p.write_state(&enabled_state()).unwrap();
        assert_eq!(p.manifest(), None);
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
        install_manifest(&p, &["1.6.12"]);
        p.write_state(&enabled_state()).unwrap();
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
        // 清单也必须出现在安装目录里 —— 播种会把它拷到用户目录,
        // 不放的话用户目录永远没有清单，判据会退化成永久放行。
        std::fs::write(
            dir.join(MANIFEST_FILENAME),
            serde_json::to_string_pretty(&PatchManifest {
                id: "bilibili".into(),
                verified_deeptutor: vec!["1.6.12".into(), "1.6.13".into()],
            })
            .unwrap(),
        )
        .unwrap();
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

        // 模拟一次全量播种
        for name in SEEDED_ENTRIES {
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
        // ★ 清单必须一起落盘，否则用户目录永远没有判据来源
        assert!(
            user_root.join(MANIFEST_FILENAME).is_file(),
            "补丁清单必须随内容一起播种"
        );
        assert_eq!(
            p.manifest().map(|m| m.verified_deeptutor),
            Some(vec!["1.6.12".to_string(), "1.6.13".to_string()])
        );
        assert_eq!(
            p.effective_dir(Some("1.6.12")),
            Some(user_root.clone()),
            "播种完必须立刻可注入 PYTHONPATH"
        );
        // 播种时生效版 1.6.12，但清单覆盖 1.6.13 —— 热更新后仍要生效
        assert_eq!(
            p.effective_dir(Some("1.6.13")),
            Some(user_root.clone()),
            "已验证的版本在热更新后必须照常生效"
        );
    }

    /// ★ 只改清单（补丁代码没动）时也必须重新播种。
    ///
    /// 「补丁发版新增了一个已验证版本」是最典型的场景：sitecustomize.py
    /// 一字节没变，若比对时不看清单，用户目录就会永远停在旧清单，
    /// 新装用户功能残缺而机器上明明有那个版本的补丁。
    #[test]
    fn manifest_only_change_propagates() {
        let res = fake_bundle("manifest-v1", "# v1\n");
        let source = res.join("patches");
        let user_root = std::env::temp_dir()
            .join(format!("dt-patch-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&user_root);

        Patches::seed_into(&source, &user_root, Some("1.6.13"));
        let p = Patches { root: user_root.clone() };
        assert_eq!(p.effective_dir(Some("1.6.14")), None, "1.6.14 尚未验证");

        // 新壳版本：补丁代码没动，只是把 1.6.14 加进了已验证清单
        std::fs::write(
            source.join(MANIFEST_FILENAME),
            serde_json::to_string_pretty(&PatchManifest {
                id: "bilibili".into(),
                verified_deeptutor: vec!["1.6.12".into(), "1.6.13".into(), "1.6.14".into()],
            })
            .unwrap(),
        )
        .unwrap();

        assert!(
            Patches::seed_into(&source, &user_root, Some("1.6.14")).is_some(),
            "只有清单变化时也必须重新播种"
        );
        assert_eq!(
            p.effective_dir(Some("1.6.14")),
            Some(user_root.clone()),
            "新增的已验证版本必须立刻生效"
        );
    }

    /// ★ 只改补丁包里的 `.py`，`sitecustomize.py` 与 `patch.json` 一个字节没动。
    ///
    /// 这是本项目真实发生过的漏洞：比对只看那两个文件，于是补丁包的更新
    /// **永远落不到用户目录** —— 而播种日志还显示「已是最新」。
    /// 症状是「明明发了新版补丁，用户那边行为还是老样子」。
    #[test]
    fn package_only_change_propagates() {
        let res = fake_bundle("pkg-v1", "# v1\n");
        let source = res.join("patches");
        let user_root = std::env::temp_dir()
            .join(format!("dt-patch-pkg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&user_root);

        Patches::seed_into(&source, &user_root, Some("1.6.13"));
        assert_eq!(
            std::fs::read_to_string(user_root.join(PATCH_PACKAGE_DIRNAME).join("search.py")).unwrap(),
            "# search\n"
        );

        // 只动补丁包
        std::fs::write(
            source.join(PATCH_PACKAGE_DIRNAME).join("search.py"),
            "# search v2\n",
        )
        .unwrap();

        assert!(
            Patches::seed_into(&source, &user_root, Some("1.6.13")).is_some(),
            "补丁包内容变化必须触发重新播种"
        );
        assert_eq!(
            std::fs::read_to_string(user_root.join(PATCH_PACKAGE_DIRNAME).join("search.py")).unwrap(),
            "# search v2\n",
            "新内容必须真的落到用户目录"
        );
    }

    /// 补丁包里新增一个文件也必须算变化（不只是已有文件被改）。
    #[test]
    fn package_new_file_propagates() {
        let res = fake_bundle("pkg-new", "# v1\n");
        let source = res.join("patches");
        let user_root = std::env::temp_dir()
            .join(format!("dt-patch-pkgnew-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&user_root);

        Patches::seed_into(&source, &user_root, Some("1.6.13"));
        std::fs::write(
            source.join(PATCH_PACKAGE_DIRNAME).join("extra.py"),
            "# new module\n",
        )
        .unwrap();

        assert!(
            Patches::seed_into(&source, &user_root, Some("1.6.13")).is_some(),
            "补丁包新增文件必须触发重新播种"
        );
        assert!(user_root.join(PATCH_PACKAGE_DIRNAME).join("extra.py").is_file());
    }

    /// ★ 目标目录里的 `__pycache__` 不得让比对永不相等。
    ///
    /// Python 一跑就会在补丁包下产出编译缓存。若它参与比对，结果是
    /// **每次启动都重播种**（无害但吵，且掩盖真正的更新信号）。
    /// 比对必须和 `copy_dir` 一样跳过它。
    #[test]
    fn pycache_does_not_trigger_reseed() {
        let res = fake_bundle("pycache", "# v1\n");
        let source = res.join("patches");
        let user_root = std::env::temp_dir()
            .join(format!("dt-patch-pycache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&user_root);

        Patches::seed_into(&source, &user_root, Some("1.6.13"));
        // 模拟 Python 在用户目录里跑过
        let cache = user_root.join(PATCH_PACKAGE_DIRNAME).join("__pycache__");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("search.cpython-313.pyc"), b"\x00compiled").unwrap();

        assert!(
            Patches::seed_into(&source, &user_root, Some("1.6.13")).is_none(),
            "只有编译缓存不同,不该重新播种"
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