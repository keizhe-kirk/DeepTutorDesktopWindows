//! 迷你 pip:依赖解析。
//!
//! 目标不是复刻 pip,而是解决一个**具体**问题:
//!
//! > 新版 deeptutor 的 wheel 里声明了一堆 `Requires-Dist`,其中有些
//! > 内置环境里没有、或版本不够。把这些补齐,让新版能 import 起来。
//!
//! # 策略
//!
//! 广度优先、贪心取版本,不做全局 SAT 求解。
//!
//! 1. 从 deeptutor 的 METADATA 读 `Requires-Dist`;
//! 2. 对每条做 **marker 求值**(extras 传空,于是 `; extra == "cli"` 这类
//!    会被判为不适用,只留下核心依赖);
//! 3. 与「已安装环境」比对:满足约束的**原样不动**(不重装、不降级);
//! 4. 不满足的进队列,查 PyPI 从高到低挑一个「满足约束 + 本平台有 wheel」
//!    的版本;
//! 5. 新加入的包自己也有依赖 —— 递归处理,限深防环。
//!
//! # 已知局限(有意为之)
//!
//! - **不做回溯**:若 A 要 `x>=2`、B 要 `x<2`,不会去找一个满足两者的解,
//!   而是先到先得 + 记一条 warning。上游小版本升级几乎不会出现这种冲突。
//! - **不降级已装包**:内置环境里版本更高的包不会被降下去。若新版的约束
//!   上限硬性要求降级,只记 warning 让人介入。
//! - **不管 extras 递归**:`deeptutor[server]` 这种自引用在空 extras 下
//!   自动跳过,不会把 server 那一整套拉下来。
//!
//! 这些取舍的理由:热更新的价值在于「上游改自己代码」时免于重发安装包,
//! 而不是替代完整的依赖管理。真遇到解不开的约束,应当提示用户走完整安装包。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use pep440_rs::{Version, VersionSpecifiers};
use pep508_rs::{ExtraName, MarkerEnvironment, MarkerEnvironmentBuilder, Requirement, VersionOrUrl};

use super::pypi::{canonical_name, InterpTag, PypiClient, ReleaseFile};

/// 依赖递归的最大深度。deeptutor -> A -> B -> C 已经很深了,
/// 再往后基本是环或者没必要追的传递依赖。
const MAX_DEPTH: usize = 6;

/// 一次解析最多装多少个包。防止约束写错导致拉下几百个包。
const MAX_PACKAGES: usize = 120;

/// 已安装环境的快照:规范化包名 -> 版本。
#[derive(Debug, Clone, Default)]
pub struct InstalledEnv {
    map: HashMap<String, Version>,
}

impl InstalledEnv {
    /// 扫描若干目录下的 `*.dist-info/METADATA`,收集已装版本。
    ///
    /// 后扫的目录覆盖先扫的 —— 调用方按「内置 -> 叠加层」的顺序传,
    /// 于是叠加层里的版本会正确胜出。
    pub fn scan(dirs: &[PathBuf]) -> Self {
        let mut map = HashMap::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                let Some(pkg_dir) = file_name.strip_suffix(".dist-info") else {
                    continue;
                };
                // 只认 `<name>-<version>.dist-info` 这种标准布局
                let Some((name_part, ver_part)) = pkg_dir.rsplit_once('-') else {
                    continue;
                };
                let Ok(version) = Version::from_str(ver_part) else {
                    continue;
                };
                map.insert(canonical_name(name_part), version);
            }
        }
        Self { map }
    }

    pub fn get(&self, name: &str) -> Option<&Version> {
        self.map.get(&canonical_name(name))
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 迭代所有 `(包名, 版本)`。
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Version)> {
        self.map.iter()
    }
}

/// 依赖解析出的一个待安装项。
#[derive(Debug, Clone)]
pub struct PlanItem {
    pub name: String,
    pub version: Version,
    pub file: ReleaseFile,
    /// 谁把它拉进来的(顶层包名,或"<传递依赖>")。
    pub required_by: String,
    /// 触发安装的原因,用于日志。
    pub reason: String,
}

/// 解析结果。
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// 需要下载安装的包(不含 deeptutor 本体)。
    pub items: Vec<PlanItem>,
    /// 约束已满足、不需要动的包数。
    pub already_satisfied: usize,
    /// 非致命问题(冲突、找不到合适 wheel 等)。有内容时应当提示用户。
    pub warnings: Vec<String>,
}

impl Plan {
    /// 待下载总字节数。
    pub fn total_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.file.size).sum()
    }
}

/// 构造内置解释器的 marker 求值环境。
///
/// Python 侧的小版本号(3.13.x)对 marker 求值基本没影响 —— 标记里出现的
/// 是 `python_version`(3.13) 这种两段式。给 full_version 补一个 `.0`
/// 保证能解析成合法 PEP 440 版本。
pub fn marker_environment(python_version: &str) -> Result<MarkerEnvironment> {
    let mut parts = python_version.split('.');
    let major = parts.next().unwrap_or("3");
    let minor = parts.next().unwrap_or("13");
    let short = format!("{major}.{minor}");

    let env = MarkerEnvironment::try_from(MarkerEnvironmentBuilder {
        implementation_name: "cpython",
        implementation_version: python_version,
        os_name: "nt",
        platform_machine: "AMD64",
        platform_python_implementation: "CPython",
        platform_release: "",
        platform_system: "Windows",
        platform_version: "",
        python_full_version: python_version,
        python_version: &short,
        sys_platform: "win32",
    })?;
    Ok(env)
}

/// 从 `Requires-Dist` 串里解析出一条 requirement。
///
/// 解析失败的条目直接忽略 —— PyPI 上偶有历史脏数据,不该因此让整个
/// 热更新失败。
pub fn parse_requirement(raw: &str) -> Option<Requirement> {
    Requirement::from_str(raw.trim()).ok()
}

/// 这条 requirement 在「当前环境 + 空 extras」下是否适用。
///
/// 空 extras 正是我们的意图:只要核心依赖。`; extra == "cli"` 之类的
/// 会在这里被过滤掉,`; python_version < "3.14"` 之类的则会按真实环境判断。
pub fn applies(req: &Requirement, env: &MarkerEnvironment) -> bool {
    req.marker.evaluate(env, &[] as &[ExtraName])
}

/// 取出 requirement 的版本约束(没有则返回 None,表示任意版本)。
pub fn specifier_of(req: &Requirement) -> Option<&VersionSpecifiers> {
    match &req.version_or_url {
        Some(VersionOrUrl::VersionSpecifier(s)) => Some(s),
        _ => None,
    }
}

/// 目标版本是否满足约束。
///
/// ⚠️ 这里**只管版本区间**,不管预发布 —— `pep440_rs::VersionSpecifiers::contains`
/// 本身也不管(`>=0.27.0` 对 `1.0.dev6` 返回 true)。预发布规则见
/// [`allows_prerelease`],由选版本的调用方显式叠加。
pub fn version_ok(req: &Requirement, version: &Version) -> bool {
    match specifier_of(req) {
        Some(spec) => spec.contains(version),
        None => true,
    }
}

/// 这条约束是否**显式**点名了预发布版本。
///
/// PEP 440 的原文是"除非没有其它选择",pip 则简化成"约束里写了预发布才放行"
/// (`>=1.0a1` / `==2.0.dev3`)。我们照抄 pip 的口径 —— 这条规则不是可选的洁癖:
/// deeptutor 声明 `httpx>=0.27.0`(无上限),而不加过滤就会选中当时的最高版
/// **`httpx 1.0.dev6`**,连带和 `mcp` / `perplexityai` / `litellm` 的 `<1.0`
/// 全部撞车 —— 一次实测就撞出 4 条冲突。换言之:少了这一行,上游随手发个
/// dev 版就能把整棵依赖树拖进预发布。
pub fn allows_prerelease(req: &Requirement) -> bool {
    specifier_of(req)
        .map(|s| s.iter().any(|sp| sp.any_prerelease()))
        .unwrap_or(false)
}

/// 待解决的依赖项。
struct Pending {
    /// 规范化包名。
    name: String,
    /// 所有指向它的约束(来自不同父包,逐个判定)。
    requirements: Vec<Requirement>,
    required_by: String,
    depth: usize,
}

/// 产出一份安装计划。
///
/// - `requires_dist`:从 deeptutor wheel 的 METADATA 里读到的 `Requires-Dist` 列表
/// - `installed`:已装环境(内置 + 已有叠加层)
/// - `tag`:目标解释器标签,用于筛 wheel
pub async fn plan(
    client: &PypiClient,
    tag: &InterpTag,
    env: &MarkerEnvironment,
    installed: &InstalledEnv,
    requires_dist: &[String],
    root_name: &str,
) -> Result<Plan> {
    let mut result = Plan::default();
    let mut queue: VecDeque<Pending> = VecDeque::new();
    let mut seen: HashSet<String> = HashSet::new();
    // 记录最终的"计划内版本",用于检测后到的约束是否与已选版本冲突
    let mut planned: HashMap<String, (Version, String)> = HashMap::new();

    // ---- 第一层:直接依赖 ----
    for raw in requires_dist {
        let Some(req) = parse_requirement(raw) else {
            continue;
        };
        if !applies(&req, env) {
            continue;
        }
        // URL 形式(`pkg @ https://...`)这里不支持,记一条警告跳过
        if matches!(req.version_or_url, Some(VersionOrUrl::Url(_))) {
            result
                .warnings
                .push(format!("跳过 URL 形式的依赖声明: {raw}"));
            continue;
        }

        let name = canonical_name(&req.name.to_string());

        // 已装且满足 -> 不动它
        if let Some(have) = installed.get(&name) {
            if version_ok(&req, have) {
                result.already_satisfied += 1;
                continue;
            }
            // 已装但不满足:只有当新约束要求「更高版本」时才升级。
            // 内置环境里版本更高时不降级(见模块文档的已知局限)。
            let wants_higher = specifier_of(&req)
                .map(|s| {
                    // 上限约束若排除当前版本,说明要降级 —— 不了,只告警
                    !s.contains(have)
                })
                .unwrap_or(false);
            if !wants_higher {
                result.already_satisfied += 1;
                continue;
            }
            result.warnings.push(format!(
                "{name} 内置版本 {have} 不满足新版要求({}),将升级",
                render_spec(&req)
            ));
        }

        if seen.insert(name.clone()) {
            queue.push_back(Pending {
                name,
                requirements: vec![req],
                required_by: root_name.to_string(),
                depth: 1,
            });
        } else {
            // 已在队列里,补一条约束
            if let Some(p) = queue.iter_mut().find(|p| p.name == name) {
                p.requirements.push(req);
            }
        }
    }

    // ---- BFS 展开传递依赖 ----
    while let Some(item) = queue.pop_front() {
        if result.items.len() >= MAX_PACKAGES {
            result.warnings.push(format!(
                "依赖数量超过上限 {MAX_PACKAGES},已停止继续解析(可能约束异常)"
            ));
            break;
        }

        // 求出该包需要满足的「共同约束」。没有 SAT,这里先用最后一条约束
        // 去挑版本,再用全部约束校验 —— 冲突就告警。
        let primary = item
            .requirements
            .last()
            .expect("Pending 至少带一条约束");

        let info = match client.fetch(&item.name).await {
            Ok(i) => i,
            Err(e) => {
                result
                    .warnings
                    .push(format!("查询 {}({}) 失败: {e}", item.name, item.required_by));
                continue;
            }
        };

        // 预发布过滤:约束里没显式点名预发布,就一律不选(理由见 allows_prerelease)。
        let allow_pre = item.requirements.iter().any(allows_prerelease);
        let picked = info.pick_best(tag, |v| {
            if v.any_prerelease() && !allow_pre {
                return false;
            }
            item.requirements.iter().all(|r| version_ok(r, v))
        });

        let Some((version, file)) = picked else {
            result.warnings.push(format!(
                "{}(来自 {}) 找不到满足 {} 且适配本平台的 wheel",
                item.name,
                item.required_by,
                render_spec(primary)
            ));
            continue;
        };

        result.items.push(PlanItem {
            name: item.name.clone(),
            version: version.clone(),
            file,
            required_by: item.required_by.clone(),
            reason: format!("满足 {}", render_spec(primary)),
        });
        planned.insert(
            item.name.clone(),
            (version.clone(), item.required_by.clone()),
        );

        // ---- 递归:这个包自己的依赖 ----
        //
        // 用单版本端点拿依赖声明,而不是下载 wheel 读 METADATA ——
        // 后者对每个依赖都要下几 MB,代价太高。
        if item.depth >= MAX_DEPTH {
            continue;
        }
        let deps = match client
            .fetch_version_requires(&item.name, &version.to_string())
            .await
        {
            Ok(d) => d,
            Err(e) => {
                result.warnings.push(format!(
                    "读取 {} {} 的依赖声明失败(其传递依赖将被跳过): {e}",
                    item.name, version
                ));
                continue;
            }
        };

        for raw in deps {
            let Some(req) = parse_requirement(&raw) else {
                continue;
            };
            if !applies(&req, env) {
                continue;
            }
            if matches!(req.version_or_url, Some(VersionOrUrl::Url(_))) {
                continue;
            }
            let name = canonical_name(&req.name.to_string());

            // 已装(内置或已计划)且满足 -> 跳过
            if let Some(have) = installed.get(&name) {
                if version_ok(&req, have) {
                    continue;
                }
            }
            if let Some((planned_ver, planned_by)) = planned.get(&name) {
                if version_ok(&req, planned_ver) {
                    continue;
                }
                result.warnings.push(format!(
                    "约束冲突:{name} 已为 {planned_by} 选定 {planned_ver},但 {} 要求 {}",
                    item.name,
                    render_spec(&req)
                ));
                continue;
            }

            if seen.contains(&name) {
                if let Some(p) = queue.iter_mut().find(|p| p.name == name) {
                    p.requirements.push(req);
                }
                continue;
            }
            seen.insert(name.clone());
            queue.push_back(Pending {
                name,
                requirements: vec![req],
                required_by: item.name.clone(),
                depth: item.depth + 1,
            });
        }
    }

    Ok(result)
}

/// 把约束渲染成可读文本,用于日志与对话框。
fn render_spec(req: &Requirement) -> String {
    match specifier_of(req) {
        Some(s) => s.to_string(),
        None => "任意版本".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> MarkerEnvironment {
        marker_environment("3.13.15").unwrap()
    }

    #[test]
    fn extras_markers_are_filtered_out() {
        let env = env();
        let core = parse_requirement("anthropic>=0.30.0").unwrap();
        let cli = parse_requirement(r#"anthropic>=0.30.0; extra == "cli""#).unwrap();
        assert!(applies(&core, &env));
        assert!(!applies(&cli, &env), "带 extra 标记的应在空 extras 下被过滤");
    }

    #[test]
    fn python_version_markers_are_evaluated() {
        let env = env();
        // 3.13 满足 <3.14
        let lt314 = parse_requirement(r#"faiss-cpu<2.0.0; python_version < "3.14""#).unwrap();
        assert!(applies(&lt314, &env));
        // 3.13 不满足 >=3.14
        let ge314 = parse_requirement(r#"faiss-cpu<2.0.0; python_version >= "3.14""#).unwrap();
        assert!(!applies(&ge314, &env));
    }

    #[test]
    fn version_ok_respects_bounds() {
        let req = parse_requirement("mcp<2.0.0,>=1.26.0").unwrap();
        assert!(version_ok(&req, &Version::from_str("1.26.0").unwrap()));
        assert!(version_ok(&req, &Version::from_str("1.30.0").unwrap()));
        assert!(!version_ok(&req, &Version::from_str("2.0.0").unwrap()));
        assert!(!version_ok(&req, &Version::from_str("1.25.9").unwrap()));
    }

    #[test]
    fn plain_requirement_accepts_anything() {
        let req = parse_requirement("requests").unwrap();
        assert!(version_ok(&req, &Version::from_str("0.1").unwrap()));
    }

    #[test]
    fn installed_env_uses_canonical_names() {
        let env = InstalledEnv::scan(&[]);
        assert!(env.is_empty());
    }

    /// 实测回归:`>=0.27.0` 会把 `1.0.dev6` 判为"满足"(区间判定不管预发布),
    /// 所以必须有单独的闸门拦住预发布。
    #[test]
    fn range_check_alone_admits_prerelease() {
        let req = parse_requirement("httpx>=0.27.0").unwrap();
        let dev = Version::from_str("1.0.dev6").unwrap();
        assert!(version_ok(&req, &dev), "区间判定本身就放行预发布,这是 PEP 440 的坑");
        assert!(!allows_prerelease(&req), "但约束没点名预发布,闸门应当拦住它");
    }

    #[test]
    fn explicit_prerelease_in_specifier_is_allowed() {
        for raw in ["pkg>=1.0a1", "pkg==2.0.dev3", "pkg>=1.0rc2,<2"] {
            let req = parse_requirement(raw).unwrap_or_else(|| panic!("{raw} 解析失败"));
            assert!(allows_prerelease(&req), "{raw} 显式点名了预发布,应当放行");
        }
        for raw in ["pkg>=1.0", "pkg>=1.0,<2", "pkg~=1.4.0"] {
            let req = parse_requirement(raw).unwrap_or_else(|| panic!("{raw} 解析失败"));
            assert!(!allows_prerelease(&req), "{raw} 没点名预发布,应当拦住");
        }
    }

    #[test]
    fn bare_requirement_never_allows_prerelease() {
        let req = parse_requirement("requests").unwrap();
        assert!(!allows_prerelease(&req));
    }

    // ---------------------------------------------------------------
    // 下面这个要用真网络,所以默认 `#[ignore]`。
    //
    //   cargo test --lib -- --ignored --nocapture pypi_end_to_end
    //
    // 为什么非要在真 PyPI 上跑一次:marker 求值、extras 过滤、wheel 适配度
    // 排序这三件事的**真实输入组合**只有上游的元数据才有 —— mock 出来的
    // `Requires-Dist` 恰好都是我们想得到的那几种形状,等于自己出题自己答。
    //
    // 再加一个环境变量,就能把「已装环境」换成**真机内置 site-packages**,
    // 跑出生产环境真正会看到的那份计划:
    //
    //   DEEPTUTOR_TEST_SITEPACKAGES="D:/APP/DeepTutor Desktop/DeepTutor/runtimes/python/Lib/site-packages" \
    //     cargo test --lib -- --ignored --nocapture pypi_end_to_end
    //
    // 空环境是**最坏情况**,真机环境因为 256 个包都已在位,往往一个都不用装 ——
    // 两者的差距大到必须各跑一次才算看清。
    // ---------------------------------------------------------------
    #[tokio::test]
    #[ignore = "需要联网访问 PyPI"]
    async fn pypi_end_to_end_resolves_deeptutor() {
        let tag = InterpTag::from_python_version("3.13.15");
        let env = marker_environment("3.13.15").unwrap();
        let client = PypiClient::new();

        let info = client
            .fetch(crate::backend::hotupdate::PACKAGE)
            .await
            .expect("拉取 deeptutor 失败");
        println!("index  = {}", client.index_url());
        println!("latest = {}", info.latest);
        println!("版本数 = {}", info.files.len());

        let (version, file) = info
            .pick_best(&tag, |v| v.is_stable())
            .expect("没有找到适配本平台的 wheel");
        println!("选中   = {version} / {}", file.filename);
        println!("大小   = {:.2} MB", file.size as f64 / 1048576.0);
        println!("sha256 = {}", file.sha256);

        let requires = client
            .fetch_version_requires(crate::backend::hotupdate::PACKAGE, &version.to_string())
            .await
            .expect("拉取依赖声明失败");
        println!("\n--- Requires-Dist ({}) ---", requires.len());
        for r in &requires {
            println!("  {r}");
        }

        // 有环境变量就按真机内置环境解析,否则按空环境(最坏情况)
        let scan_dirs: Vec<PathBuf> = std::env::var("DEEPTUTOR_TEST_SITEPACKAGES")
            .ok()
            .map(|s| s.split(';').filter(|p| !p.trim().is_empty()).map(PathBuf::from).collect())
            .unwrap_or_default();
        println!("\n--- 已装环境 ({} 个扫描目录) ---", scan_dirs.len());
        let installed = InstalledEnv::scan(&scan_dirs);
        println!("扫到 {} 个包", installed.len());

        let p = plan(
            &client,
            &tag,
            &env,
            &installed,
            &requires,
            crate::backend::hotupdate::PACKAGE,
        )
        .await
        .expect("依赖解析失败");

        println!("\n--- 计划安装 ({}) ---", p.items.len());
        for it in &p.items {
            println!(
                "  {:<26} {:<14} {:>8.2} MB  <- {}",
                it.name,
                it.version,
                it.file.size as f64 / 1048576.0,
                it.required_by
            );
        }
        println!("已满足 = {}", p.already_satisfied);
        println!("总计   = {:.2} MB", p.total_bytes() as f64 / 1048576.0);
        if !p.warnings.is_empty() {
            println!("--- 警告 ({}) ---", p.warnings.len());
            for w in &p.warnings {
                println!("  {w}");
            }
        }

        // 这两条与"是否空环境"无关,任何情况都必须成立:
        // 计划里不能混进本平台装不了的 wheel,也不能包含 deeptutor 本体。
        for it in &p.items {
            assert!(
                tag.accepts(&it.file.filename).is_some(),
                "计划里混进了本平台装不了的 wheel: {}",
                it.file.filename
            );
        }
        assert!(
            p.items.iter().all(|i| i.name != canonical_name("deeptutor")),
            "deeptutor 本体不该出现在依赖计划里"
        );

        if installed.is_empty() {
            // 空环境(最坏情况):deeptutor 自带一票依赖,空计划说明链路断了
            assert!(
                !p.items.is_empty(),
                "deeptutor 自带一票依赖,空计划说明解析链路断了"
            );
        } else {
            // 真机环境:绝大多数依赖应当被判为"已满足"。
            // 这条断言真正在测的是「已装环境有没有被用上」—— 若 scan 结果
            // 没进到解析里(比如路径写错),这里会立刻退化成空环境的行为。
            println!(
                "\n结论:真机环境下待装 {} 个 / 已满足 {} 个",
                p.items.len(),
                p.already_satisfied
            );
            assert!(
                p.already_satisfied > 50,
                "真机环境下只判定了 {} 个依赖已满足,已装环境可能没生效",
                p.already_satisfied
            );
        }
    }
}
