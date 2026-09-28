//! wheel 的下载、完整性校验与解压。
//!
//! wheel 本质就是个 zip:顶层直接是包目录(无 root 前缀),
//! `<name>-<ver>.dist-info/` 里放 METADATA / WHEEL / RECORD。
//!
//! # 完整性
//!
//! 下载后**必须**按 PyPI 给出的 sha256 校验。这一步不能省:
//! 热更新的产物会被 Python 直接 import,等于执行任意代码 ——
//! 断流截断或中间人替换都会造成实际危害。
//!
//! # zip-slip
//!
//! 用 `ZipArchive::enclosed_name()` 而不是 `entry.name()` 拼路径。
//! 前者会拒绝 `../` 和绝对路径的条目,从根上挡掉「解压写穿目标目录」。

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// 从 wheel 的 METADATA 里读出的关键字段。
#[derive(Debug, Clone, Default)]
pub struct WheelMetadata {
    pub name: String,
    pub version: String,
    /// `Requires-Dist` 原始串(未做 marker 求值)。
    pub requires_dist: Vec<String>,
    pub requires_python: Option<String>,
}

/// 解压统计。
#[derive(Debug, Clone, Default)]
pub struct ExtractStats {
    pub files: usize,
    pub bytes: u64,
    /// 被跳过的条目(如 `.data/` 下的脚本目录),用于日志。
    pub skipped: usize,
}

/// 下载一个文件并按 `expect_sha256` 校验,落到 `dest`。
///
/// `on_progress(已下载, 总大小)` 每收到一块就调一次 —— 调用方自己
/// 决定怎么节流(不要在这里限制,否则上传进度会不均匀)。
///
/// 校验不通过时**删掉**已下载的文件并返回错误:半个 wheel 留在缓存里
/// 比没有更危险。
pub async fn download_verified(
    http: &reqwest::Client,
    url: &str,
    expect_sha256: &str,
    dest: &Path,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<u64> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("创建下载目录失败: {}", parent.display()))?;
    }

    let mut resp = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("下载失败: {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("下载失败 HTTP {}: {url}", resp.status()));
    }
    let total = resp.content_length();

    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("创建文件失败: {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;

    while let Some(chunk) = resp
        .chunk()
        .await
        .with_context(|| format!("读取响应流出错: {url}"))?
    {
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .with_context(|| format!("写入文件失败: {}", dest.display()))?;
        written += chunk.len() as u64;
        on_progress(written, total);
    }

    file.flush().await.ok();
    drop(file);

    let got = hex::encode(hasher.finalize());
    if !expect_sha256.is_empty() && !got.eq_ignore_ascii_case(expect_sha256) {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(anyhow!(
            "sha256 校验失败,文件已删除\n  期望: {expect_sha256}\n  实际: {got}"
        ));
    }

    Ok(written)
}

/// 读 wheel 里的 METADATA。
///
/// 只解出 METADATA 一个条目,不落盘、不解全包 —— 所以对
/// 「先看依赖再决定要不要装」这种场景非常廉价。
pub fn read_metadata(wheel: &Path) -> Result<WheelMetadata> {
    let file = std::fs::File::open(wheel)
        .with_context(|| format!("打开 wheel 失败: {}", wheel.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("解析 wheel(zip)失败: {}", wheel.display()))?;

    let meta_name = (0..archive.len()).find_map(|i| {
        let e = archive.by_index(i).ok()?;
        let n = e.name().to_string();
        (n.ends_with(".dist-info/METADATA") || n.ends_with(".dist-info\\METADATA")).then_some(n)
    });
    let Some(meta_name) = meta_name else {
        return Err(anyhow!("wheel 里找不到 dist-info/METADATA: {}", wheel.display()));
    };

    let mut text = String::new();
    archive
        .by_name(&meta_name)
        .with_context(|| format!("读取 {meta_name} 失败"))?
        .read_to_string(&mut text)
        .context("METADATA 不是合法 UTF-8")?;

    Ok(parse_metadata(&text))
}

/// 解析 METADATA 文本。RFC 822 风格的头部,`字段: 值`,空行后是长描述。
fn parse_metadata(text: &str) -> WheelMetadata {
    let mut out = WheelMetadata::default();
    for line in text.lines() {
        if line.is_empty() {
            break; // 头部结束,后面是 Description
        }
        // 续行(以空白开头)不属于我们要的字段
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "name" => out.name = value.to_string(),
            "version" => out.version = value.to_string(),
            "requires-dist" => out.requires_dist.push(value.to_string()),
            "requires-python" => out.requires_python = Some(value.to_string()),
            _ => {}
        }
    }
    out
}

/// 把 wheel 解压到 `dest`(会创建目录)。
///
/// 覆盖语义:同名文件直接覆盖。因为目标是**全新的版本目录**,正常情况下
/// 里面本来就是空的;覆盖只会在「同一版本重装」时发生,此时覆盖正是想要的。
pub fn extract_to(wheel: &Path, dest: &Path, mut on_progress: impl FnMut(usize)) -> Result<ExtractStats> {
    let file = std::fs::File::open(wheel)
        .with_context(|| format!("打开 wheel 失败: {}", wheel.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("解析 wheel(zip)失败: {}", wheel.display()))?;

    std::fs::create_dir_all(dest)
        .with_context(|| format!("创建目标目录失败: {}", dest.display()))?;

    let mut stats = ExtractStats::default();
    let total = archive.len();

    for i in 0..total {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("读取 wheel 第 {i} 个条目失败"))?;

        // zip-slip 防护:enclosed_name 会拒绝 `../` 与绝对路径
        let Some(rel) = entry.enclosed_name() else {
            stats.skipped += 1;
            continue;
        };

        // `.data/` 里的内容是 wheel 的「安装脚本/数据」区,按规范要装到
        // sys.prefix 下的不同子目录(scripts、data、headers...)。我们的
        // 叠加层只承担 site-packages 的角色,跳过它们比装错位置安全。
        let is_data = rel
            .components()
            .next()
            .and_then(|c| c.as_os_str().to_str().map(|s| s.contains(".data")))
            .unwrap_or(false);
        if is_data {
            stats.skipped += 1;
            continue;
        }

        let out_path = dest.join(&rel);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .with_context(|| format!("创建目录失败: {}", out_path.display()))?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败: {}", parent.display()))?;
        }

        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut buf)
            .with_context(|| format!("读取 wheel 条目失败: {}", rel.display()))?;
        std::fs::write(&out_path, &buf)
            .with_context(|| format!("写入失败: {}", out_path.display()))?;

        stats.files += 1;
        stats.bytes += buf.len() as u64;
        on_progress(stats.files);
    }

    Ok(stats)
}

/// wheel 缓存文件名(把 URL 里的文件名原样用上,便于人工检查)。
pub fn cache_filename(file: &super::pypi::ReleaseFile) -> String {
    // 文件名里可能有路径分隔符?PyPI 的不会,但保险起见取末段
    file.filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("package.whl")
        .to_string()
}

/// 计算本地文件的 sha256(用于校验缓存命中)。
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("打开文件失败: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 规范化版本目录名(防目录穿越,顺便统一展示形式)。
pub fn safe_version_dir_name(version: &str) -> Option<PathBuf> {
    let v = version.trim();
    if v.is_empty() || v.contains('/') || v.contains('\\') || v.contains("..") {
        return None;
    }
    Some(PathBuf::from(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metadata_headers() {
        let text = "Metadata-Version: 2.1\nName: deeptutor\nVersion: 1.6.13\n\
                    Requires-Python: <3.15,>=3.11\n\
                    Requires-Dist: PyYAML>=6.0\n\
                    Requires-Dist: anthropic>=0.30.0; extra == \"cli\"\n\
                    \n\
                    Long description here.\nRequires-Dist: not-a-real-field\n";
        let md = parse_metadata(text);
        assert_eq!(md.name, "deeptutor");
        assert_eq!(md.version, "1.6.13");
        assert_eq!(md.requires_python.as_deref(), Some("<3.15,>=3.11"));
        // 空行之后的内容不再计入 —— 长描述里的同名字串不该被误收
        assert_eq!(md.requires_dist.len(), 2);
        assert!(md.requires_dist[0].starts_with("PyYAML"));
    }

    #[test]
    fn handles_continuation_lines() {
        let text = "Name: foo\nVersion: 1.0\nRequires-Dist: bar>=1.0,\n     <2.0\n\n";
        let md = parse_metadata(text);
        assert_eq!(md.name, "foo");
        // 续行被忽略,只保留首行的部分
        assert_eq!(md.requires_dist, vec!["bar>=1.0,".to_string()]);
    }

    #[test]
    fn safe_version_dir_rejects_traversal() {
        assert!(safe_version_dir_name("1.6.13").is_some());
        assert!(safe_version_dir_name("../evil").is_none());
        assert!(safe_version_dir_name("a\\b").is_none());
        assert!(safe_version_dir_name("").is_none());
    }

    #[test]
    fn cache_filename_takes_last_segment() {
        let f = super::super::pypi::ReleaseFile {
            filename: "foo-1.0-py3-none-any.whl".into(),
            url: "https://example.com/x/foo-1.0-py3-none-any.whl".into(),
            sha256: String::new(),
            size: 0,
            is_wheel: true,
        };
        assert_eq!(cache_filename(&f), "foo-1.0-py3-none-any.whl");
    }
}
