//! PyPI JSON API 客户端 + wheel 选型。
//!
//! 只用两个端点:
//! - `GET {base}/pypi/{name}/json` —— 拿某个包的全部发行版与文件清单
//! - 文件本身走 `files.pythonhosted.org`(响应里给的绝对 URL)
//!
//! # 为什么一次拉全量
//!
//! 依赖解析要「从高到低挑一个满足约束、且在本平台有可用 wheel 的版本」,
//! 只看最新版不够 —— 最新版可能没有 win_amd64 的 wheel,得回退到更早的版本。
//! 全量 JSON 对绝大多数包只有几十 KB,一次拿完比逐版本探测省事得多。
//!
//! # 镜像源
//!
//! 默认走官方 `https://pypi.org`;可用环境变量 `DEEPTUTOR_PYPI_INDEX` 覆盖
//! (填站点根,如 `https://pypi.tuna.tsinghua.edu.cn`)。注意部分镜像不提供
//! 完全兼容的 JSON API —— 覆盖后能否工作取决于镜像实现。

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use pep440_rs::Version;
use pep508_rs::PackageName;
use serde::Deserialize;

/// 覆盖 PyPI 站点根的环境变量。
pub const ENV_PYPI_INDEX: &str = "DEEPTUTOR_PYPI_INDEX";

const DEFAULT_INDEX: &str = "https://pypi.org";

/// 目标解释器的 wheel 兼容标签。
///
/// 内置解释器固定是 CPython + win_amd64,所以不需要完整的 PEP 425
/// 标签集合推导 —— 只要「CPython 3.13 / win_amd64」这一组就够。
#[derive(Debug, Clone)]
pub struct InterpTag {
    /// 如 `cp313`。用于匹配 ABI 相关的 wheel 文件名。
    pub abi_tag: String,
    /// 如 `313`。用于匹配 `cp313` / `abi3` 判定。
    pub short: String,
    /// 如 `win_amd64`。
    pub platform: String,
}

impl InterpTag {
    /// 从 `3.13.15` 这样的完整版本串推导标签。
    pub fn from_python_version(full: &str) -> Self {
        let mut it = full.split('.');
        let major = it.next().unwrap_or("3");
        let minor = it.next().unwrap_or("13");
        Self {
            abi_tag: format!("cp{major}{minor}"),
            short: format!("{major}{minor}"),
            platform: "win_amd64".to_string(),
        }
    }

    /// 这个 wheel 文件名是否能在本平台安装。
    ///
    /// wheel 文件名形如 `<name>-<ver>-<pytag>-<abitag>-<platformtag>.whl`,
    /// 每个 tag 位可以是 `.` 分隔的多值。
    pub fn accepts(&self, filename: &str) -> Option<WheelRank> {
        let stem = filename.strip_suffix(".whl")?;
        let parts: Vec<&str> = stem.split('-').collect();
        if parts.len() < 5 {
            return None;
        }
        let (py_tag, abi, plat) = (
            parts[parts.len() - 3],
            parts[parts.len() - 2],
            parts[parts.len() - 1],
        );

        // 平台:纯 `none` 表示与平台无关;否则必须命中 win_amd64
        let plat_ok = plat.split('.').any(|p| p == self.platform);
        let plat_any = plat.split('.').any(|p| p == "any");

        // ABI
        let abi_ok = abi.split('.').any(|a| {
            a == "none"
                || a == "abi3"          // 稳定 ABI,向后兼容
                || a == self.abi_tag
        });
        if !abi_ok {
            return None;
        }

        // Python 版本 tag
        let mut py_ok = false;
        for p in py_tag.split('.') {
            if p == "py3" || p == "py2" || p == "py2.py3" {
                py_ok = true;
            } else if let Some(rest) = p.strip_prefix("cp") {
                // py 位本身是宽松的(cp312 <= cp313 就算过)。真正的守门人是
                // 下面的 abi 判定 —— 只有 abi3 才跨 minor 兼容,精确 ABI 必须
                // 同版本,所以 cp312-cp312 最终仍会被拒。
                if let (Ok(want), Ok(got)) = (self.short.parse::<u32>(), rest.parse::<u32>()) {
                    if got <= want {
                        py_ok = true;
                    }
                }
            }
        }
        if !py_ok {
            return None;
        }

        // 排序偏好:平台专用 > 纯 Python;CPython 专用 > abi3 > none
        let rank = if plat_any {
            WheelRank::PureAny
        } else if plat_ok {
            if abi.split('.').any(|a| a == self.abi_tag) {
                WheelRank::NativeExact
            } else if abi.split('.').any(|a| a == "abi3") {
                WheelRank::NativeAbi3
            } else {
                WheelRank::NativeOther
            }
        } else {
            return None;
        };
        Some(rank)
    }
}

/// wheel 适配度排序。数值越小越优先。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WheelRank {
    /// CPython 精确 ABI + 平台专用(最理想)
    NativeExact = 0,
    /// abi3 平台专用
    NativeAbi3 = 1,
    /// 其他平台专用
    NativeOther = 2,
    /// 纯 Python(`py3-none-any`)
    PureAny = 3,
}

/// 一个发行版文件。
#[derive(Debug, Clone)]
pub struct ReleaseFile {
    pub filename: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub is_wheel: bool,
}

/// 一个包在 PyPI 上的信息(已裁剪到我们需要的部分)。
#[derive(Debug, Clone)]
pub struct PackageInfo {
    /// 规范化后的包名(小写、`_`/`.` 归一为 `-`)。
    pub name: String,
    /// PyPI 上的最新版(含预发布,仅作展示)。
    pub latest: String,
    /// 各版本的文件清单。
    pub files: BTreeMap<String, Vec<ReleaseFile>>,
}

impl PackageInfo {
    /// 从高到低挑一个可用版本。
    ///
    /// `accept(version)` 决定这个版本是否满足依赖约束;
    /// `tag` 用来筛选本平台能装的 wheel。
    ///
    /// 返回 `(版本, wheel 文件)`。
    pub fn pick_best(
        &self,
        tag: &InterpTag,
        accept: impl Fn(&Version) -> bool,
    ) -> Option<(Version, ReleaseFile)> {
        let mut candidates: Vec<(Version, ReleaseFile, WheelRank)> = Vec::new();
        for (ver_str, files) in &self.files {
            let Ok(ver) = Version::from_str(ver_str) else {
                continue;
            };
            if !accept(&ver) {
                continue;
            }
            // 同一版本里挑适配度最好的那个 wheel
            let mut best: Option<(ReleaseFile, WheelRank)> = None;
            for f in files {
                if !f.is_wheel {
                    continue;
                }
                let Some(rank) = tag.accepts(&f.filename) else {
                    continue;
                };
                if best.as_ref().map(|(_, r)| rank < *r).unwrap_or(true) {
                    best = Some((f.clone(), rank));
                }
            }
            if let Some((file, rank)) = best {
                candidates.push((ver, file, rank));
            }
        }
        // 版本优先,其次 wheel 适配度
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.cmp(&b.2)));
        candidates
            .into_iter()
            .next()
            .map(|(v, f, _)| (v, f))
    }
}

/// PyPI 客户端。
#[derive(Debug, Clone)]
pub struct PypiClient {
    base: String,
    http: reqwest::Client,
    /// 单包 JSON 的响应缓存,避免同一轮解析里重复请求。
    cache: std::sync::Arc<parking_lot::Mutex<BTreeMap<String, PackageInfo>>>,
}

impl PypiClient {
    pub fn new() -> Self {
        let base = std::env::var(ENV_PYPI_INDEX)
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_INDEX.to_string());

        let http = reqwest::Client::builder()
            .user_agent(concat!("DeepTutorDesktop/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(20))
            .build()
            .expect("构建 reqwest 客户端失败");

        Self {
            base,
            http,
            cache: std::sync::Arc::new(parking_lot::Mutex::new(BTreeMap::new())),
        }
    }

    pub fn index_url(&self) -> &str {
        &self.base
    }

    /// 拉某个包的完整信息(带进程内缓存)。
    pub async fn fetch(&self, name: &str) -> Result<PackageInfo> {
        let key = normalize_name(name);
        if let Some(hit) = self.cache.lock().get(&key).cloned() {
            return Ok(hit);
        }

        let url = format!("{}/pypi/{}/json", self.base, key);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("请求 PyPI 失败: {url}"))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(anyhow!("PyPI 上没有名为 {key} 的包"));
        }
        let resp = resp
            .error_for_status()
            .with_context(|| format!("PyPI 返回错误状态: {url}"))?;

        let raw: RawPackage = resp
            .json()
            .await
            .with_context(|| format!("解析 PyPI 响应失败: {url}"))?;
        let info = raw.into_info();

        self.cache.lock().insert(key, info.clone());
        Ok(info)
    }

    /// 只取某个**指定版本**的 `Requires-Dist` 列表。
    ///
    /// 全量端点(`/pypi/{name}/json`)只给最新版的依赖声明,而依赖解析
    /// 需要的是「我们选中的那个版本」的声明。单版本端点正好提供它 ——
    /// 比把整个 wheel 下来读 METADATA 省得多(几十 KB vs 几 MB)。
    ///
    /// 该端点对老旧包可能没有 `requires_dist`(元数据里根本没写),
    /// 此时返回空列表,调用方当作"无传递依赖"处理即可。
    pub async fn fetch_version_requires(&self, name: &str, version: &str) -> Result<Vec<String>> {
        let key = normalize_name(name);
        let url = format!("{}/pypi/{}/{}/json", self.base, key, version);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("请求 PyPI 失败: {url}"))?;
        let resp = resp
            .error_for_status()
            .with_context(|| format!("PyPI 返回错误状态: {url}"))?;

        let raw: RawVersionPackage = resp
            .json()
            .await
            .with_context(|| format!("解析 PyPI 响应失败: {url}"))?;
        Ok(raw.info.requires_dist.unwrap_or_default())
    }
}

impl Default for PypiClient {
    fn default() -> Self {
        Self::new()
    }
}

/// 把包名规范化成 PyPI 的规范形式(PEP 503)。
pub fn normalize_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .replace(['_', '.'], "-")
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// 用 pep508_rs 的 `PackageName` 做一次权威规范化(失败时退回本地实现)。
pub fn canonical_name(name: &str) -> String {
    PackageName::from_str(name)
        .map(|p| p.to_string())
        .unwrap_or_else(|_| normalize_name(name))
}

// ---------- PyPI 响应反序列化 ----------

#[derive(Deserialize)]
struct RawPackage {
    info: RawInfo,
    #[serde(default)]
    releases: BTreeMap<String, Vec<RawFile>>,
}

#[derive(Deserialize)]
struct RawInfo {
    name: String,
    version: String,
}

#[derive(Deserialize)]
struct RawFile {
    filename: String,
    url: String,
    #[serde(default)]
    packagetype: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    yanked: bool,
    #[serde(default)]
    digests: RawDigests,
}

#[derive(Deserialize, Default)]
struct RawDigests {
    #[serde(default)]
    sha256: String,
}

/// `/pypi/{name}/{version}/json` 的响应(只取依赖声明)。
#[derive(Deserialize)]
struct RawVersionPackage {
    info: RawVersionInfo,
}

#[derive(Deserialize)]
struct RawVersionInfo {
    #[serde(default)]
    requires_dist: Option<Vec<String>>,
}

impl RawPackage {
    fn into_info(self) -> PackageInfo {
        let mut files = BTreeMap::new();
        for (ver, list) in self.releases {
            let kept: Vec<ReleaseFile> = list
                .into_iter()
                // 被 yank 的版本不参与选型
                .filter(|f| !f.yanked)
                .filter(|f| !f.digests.sha256.is_empty())
                .map(|f| ReleaseFile {
                    is_wheel: f.packagetype == "bdist_wheel" || f.filename.ends_with(".whl"),
                    filename: f.filename,
                    url: f.url,
                    sha256: f.digests.sha256,
                    size: f.size,
                })
                .collect();
            if !kept.is_empty() {
                files.insert(ver, kept);
            }
        }
        PackageInfo {
            name: normalize_name(&self.info.name),
            latest: self.info.version,
            files,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag() -> InterpTag {
        InterpTag::from_python_version("3.13.15")
    }

    #[test]
    fn accepts_pure_and_native() {
        let t = tag();
        assert_eq!(
            t.accepts("idna-3.20-py3-none-any.whl"),
            Some(WheelRank::PureAny)
        );
        assert_eq!(
            t.accepts("numpy-2.1.0-cp313-cp313-win_amd64.whl"),
            Some(WheelRank::NativeExact)
        );
    }

    #[test]
    fn abi_must_match_exactly_or_be_stable() {
        let t = tag();
        // ★ CPython 的 ABI **不跨 minor 版本兼容**:cp312 编译的扩展在 3.13 上
        //   加载会报 "Module use of Python312.dll conflicts"。所以 cp312-cp312
        //   的 wheel 必须被拒 —— 这正是 abi 位要求精确匹配的原因。
        assert_eq!(t.accepts("foo-1.0-cp312-cp312-win_amd64.whl"), None);
        assert_eq!(t.accepts("foo-1.0-cp311-cp311-win_amd64.whl"), None);
        // abi3(稳定 ABI)才向后兼容,低版本编的也能用
        assert_eq!(
            t.accepts("foo-1.0-cp38-abi3-win_amd64.whl"),
            Some(WheelRank::NativeAbi3)
        );
        // 同版本的精确 ABI 当然可用
        assert_eq!(
            t.accepts("foo-1.0-cp313-cp313-win_amd64.whl"),
            Some(WheelRank::NativeExact)
        );
    }

    #[test]
    fn accepts_cp313_abi3_combo() {
        let t = tag();
        // py 位写 cp39、abi 位写 abi3 是常见写法:py 位宽松、性质由 abi 决定
        assert_eq!(
            t.accepts("foo-1.0-cp39-abi3-win_amd64.whl"),
            Some(WheelRank::NativeAbi3)
        );
    }

    #[test]
    fn rejects_other_platform_and_newer_python() {
        let t = tag();
        assert_eq!(t.accepts("foo-1.0-cp313-cp313-manylinux_x86_64.whl"), None);
        assert_eq!(t.accepts("foo-1.0-cp313-cp313-macosx_11_0_arm64.whl"), None);
        // 比目标更新的 CPython 不能装
        assert_eq!(t.accepts("foo-1.0-cp314-cp314-win_amd64.whl"), None);
        // 源码包不是 wheel
        assert_eq!(t.accepts("foo-1.0.tar.gz"), None);
    }

    #[test]
    fn native_beats_pure() {
        let t = tag();
        let native = t.accepts("x-1-cp313-cp313-win_amd64.whl").unwrap();
        let pure = t.accepts("x-1-py3-none-any.whl").unwrap();
        assert!(native < pure);
    }

    #[test]
    fn name_normalization() {
        assert_eq!(normalize_name("PyYAML"), "pyyaml");
        assert_eq!(normalize_name("zope.interface"), "zope-interface");
        assert_eq!(normalize_name("typing_extensions"), "typing-extensions");
    }
}
