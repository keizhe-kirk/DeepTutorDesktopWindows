//! CA 信任合并:让后端 Python 认识 Windows 证书库里的额外根。
//!
//! 背景:网络加速器(SteamTools/Steam++ 等)与企业代理会对 github.com 等域名做
//! TLS 中间人 —— 用它们的自签根现场签一张假叶子证书。浏览器 / schannel 类工具
//! 走 **Windows 证书库**,认这些根;而 Python 的 httpx / requests / urllib 走
//! **certifi**(Mozilla 独立根清单),不认 → 后端一碰被劫持的域名就抛
//! `[SSL: CERTIFICATE_VERIFY_FAILED] ... unable to get local issuer certificate`,
//! 表现为软件里「检查更新」永远失败(2026-09-29 实锤排查)。
//!
//! 修法:启动后端前生成一份**合并信任库**(certifi 全量 + Windows 证书库中
//! certifi 没有的根),写到运行数据目录,再给子进程注入
//! `SSL_CERT_FILE` / `REQUESTS_CA_BUNDLE` / `CURL_CA_BUNDLE`。
//!
//! 两条原则:
//! - **只注入子进程**,不动用户环境变量,不改安装目录(certifi 是包文件,
//!   安装目录 perMachine 只读,且热更新叠加层也不该动它)。
//! - **失败即退回现状**(返回 None 就不注入),任何一步出错都不会比今天更糟。
//!
//! 去重:按证书 DER 的 base64 正文比对,已在 certifi 里的根不再追加,
//! 避免 bundle 越滚越大。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use base64::Engine;

/// 合并信任库的文件名(落在运行数据目录下)。
pub const MERGED_CA_FILE: &str = "ca-merged.pem";

/// 生成合并信任库并写盘,返回写盘路径。
///
/// - `python_exe`:后端解释器,用来定位它的 certifi(`cacert.pem`)。
/// - `out_dir`   :合并库的落盘目录(一般是 `DEEPTUTOR_HOME`)。
///
/// 返回 `None` 表示"没有需要补的根"或生成失败 —— 调用方维持默认信任即可。
pub fn build_merged_bundle(python_exe: &Path, out_dir: &Path) -> Option<PathBuf> {
    let certifi = certifi_path(python_exe)?;
    let base = std::fs::read_to_string(&certifi).ok()?;
    let extra = native_root_certs();
    let (merged, added) = merge(&base, &extra);
    if added == 0 {
        return None;
    }
    std::fs::create_dir_all(out_dir).ok()?;
    let out = out_dir.join(MERGED_CA_FILE);
    // 内容没变就不重写:避免每次启动都动磁盘
    if std::fs::read_to_string(&out).ok().as_deref() != Some(merged.as_str()) {
        std::fs::write(&out, &merged).ok()?;
    }
    Some(out)
}

/// 定位解释器对应的 certifi 根清单。
///
/// 同时兼容「解释器在根目录」(`python/Lib/site-packages`)与
/// 「解释器在 venv 的 Scripts 下」(`venv/Scripts/../Lib/site-packages`)两种布局。
fn certifi_path(python_exe: &Path) -> Option<PathBuf> {
    let dir = python_exe.parent()?;
    let candidates = [
        dir.join("Lib").join("site-packages").join("certifi").join("cacert.pem"),
        dir.join("..")
            .join("Lib")
            .join("site-packages")
            .join("certifi")
            .join("cacert.pem"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

/// 纯逻辑:把额外的 DER 证书并进 certifi 文本,返回 (合并文本, 实际追加张数)。
///
/// 按 base64 正文去重 —— certifi 里已有的根原样保留,不重复追加。
fn merge(certifi_text: &str, extra_ders: &[Vec<u8>]) -> (String, usize) {
    let mut seen = pem_bodies(certifi_text);
    let mut out = String::from(certifi_text);
    let mut added = 0usize;
    for der in extra_ders {
        let body = base64::engine::general_purpose::STANDARD.encode(der);
        if seen.insert(body) {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&pem_block(der));
            added += 1;
        }
    }
    (out, added)
}

/// 抽出 PEM 文本里所有证书块的 base64 正文(去掉换行)。
fn pem_bodies(text: &str) -> HashSet<String> {
    let mut bodies = HashSet::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("-----BEGIN CERTIFICATE") {
            current = Some(String::new());
        } else if trimmed.starts_with("-----END CERTIFICATE") {
            if let Some(body) = current.take() {
                if !body.is_empty() {
                    bodies.insert(body);
                }
            }
        } else if let Some(body) = current.as_mut() {
            body.push_str(trimmed);
        }
    }
    bodies
}

/// DER -> PEM(64 列换行,OpenSSL/Python/浏览器都认)。
fn pem_block(der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut s = String::with_capacity(b64.len() + 96);
    s.push_str("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        // base64 输出恒为 ASCII
        s.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        s.push('\n');
    }
    s.push_str("-----END CERTIFICATE-----\n");
    s
}

/// 枚举 Windows 证书库的根证书(LocalMachine\Root + CurrentUser\Root)。
#[cfg(windows)]
fn native_root_certs() -> Vec<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{
        CERT_SYSTEM_STORE_CURRENT_USER, CERT_SYSTEM_STORE_LOCAL_MACHINE,
    };

    let mut out = enum_store_roots(CERT_SYSTEM_STORE_LOCAL_MACHINE);
    out.extend(enum_store_roots(CERT_SYSTEM_STORE_CURRENT_USER));
    out
}

#[cfg(not(windows))]
fn native_root_certs() -> Vec<Vec<u8>> {
    Vec::new()
}

/// 枚举指定位置 `Root` 存储里的证书 DER。
///
/// 注意:Python 的 `ssl.enum_certificates("ROOT")` 只看 CurrentUser,
/// 会漏掉 LocalMachine(实测:加速器的 MITM 根就装在 LocalMachine\Root),
/// 所以这里必须走 Win32 API 显式枚举两处。
#[cfg(windows)]
fn enum_store_roots(location: u32) -> Vec<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Security::Cryptography::{
        CertCloseStore, CertEnumCertificatesInStore, CertOpenStore, CERT_CONTEXT,
        CERT_OPEN_STORE_FLAGS, CERT_STORE_OPEN_EXISTING_FLAG, CERT_STORE_PROV_SYSTEM_W,
        CERT_STORE_READONLY_FLAG, X509_ASN_ENCODING,
    };

    let mut out = Vec::new();
    let name: Vec<u16> = std::ffi::OsStr::new("Root")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let flags = CERT_STORE_OPEN_EXISTING_FLAG
        | CERT_STORE_READONLY_FLAG
        | CERT_OPEN_STORE_FLAGS(location);

    // SAFETY: 所有裸指针都来自系统 API;枚举产生的上下文由系统在下次调用时回收,
    // 不手动 free;存储句柄在函数末尾关闭。
    let store = unsafe {
        match CertOpenStore(
            CERT_STORE_PROV_SYSTEM_W,
            X509_ASN_ENCODING,
            None,
            flags,
            Some(name.as_ptr() as *const _),
        ) {
            Ok(s) if !s.is_invalid() => s,
            _ => return out,
        }
    };

    let mut prev: Option<*const CERT_CONTEXT> = None;
    loop {
        let ctx = unsafe { CertEnumCertificatesInStore(store, prev) };
        if ctx.is_null() {
            break;
        }
        unsafe {
            let cert = &*ctx;
            if !cert.pbCertEncoded.is_null() && cert.cbCertEncoded > 0 {
                out.push(std::slice::from_raw_parts(
                    cert.pbCertEncoded,
                    cert.cbCertEncoded as usize,
                )
                .to_vec());
            }
        }
        prev = Some(ctx as *const CERT_CONTEXT);
    }
    unsafe {
        let _ = CertCloseStore(Some(store), 0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_der(fill: u8, len: usize) -> Vec<u8> {
        vec![fill; len]
    }

    #[test]
    fn pem_block_body_is_plain_base64() {
        let der = fake_der(0xAB, 30);
        let block = pem_block(&der);
        assert!(block.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(block.ends_with("-----END CERTIFICATE-----\n"));
        let body: String = block
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(body, base64::engine::general_purpose::STANDARD.encode(&der));
    }

    #[test]
    fn merge_skips_roots_already_in_certifi() {
        let d1 = fake_der(1, 32);
        let d2 = fake_der(2, 48);
        let certifi = pem_block(&d1);
        let (merged, added) = merge(&certifi, &[d1.clone(), d2.clone()]);
        assert_eq!(added, 1, "d1 已在 certifi 里,只应追加 d2");
        assert_eq!(merged.matches("BEGIN CERTIFICATE").count(), 2);
        let b1 = base64::engine::general_purpose::STANDARD.encode(&d1);
        assert_eq!(merged.matches(b1.as_str()).count(), 1, "d1 不应重复");
    }

    #[test]
    fn merge_with_no_new_roots_is_identity() {
        let d1 = fake_der(1, 32);
        let certifi = pem_block(&d1);
        let (merged, added) = merge(&certifi, &[d1]);
        assert_eq!(added, 0);
        assert_eq!(merged, certifi);
    }

    #[test]
    fn pem_bodies_handles_wrapped_lines() {
        let der = fake_der(7, 100); // 100 字节 -> base64 136 字符,必然折行
        let text = pem_block(&der);
        let bodies = pem_bodies(&text);
        assert_eq!(
            bodies.into_iter().next().unwrap(),
            base64::engine::general_purpose::STANDARD.encode(&der)
        );
    }

    /// 手动行为验证用(默认不跑):
    /// `DEEPTUTOR_TEST_PYTHON=... cargo test --lib -- --ignored emit_real_bundle --nocapture`
    /// 生成真实合并库到项目 `.tmp/`,再拿它跑 `.tmp/ssl-probe.py` 复测直连 GitHub。
    #[test]
    #[ignore]
    fn emit_real_bundle() {
        let py = std::env::var("DEEPTUTOR_TEST_PYTHON").map(PathBuf::from).unwrap_or_else(|_| {
            PathBuf::from(r"D:\APP\DeepTutor Desktop\DeepTutor\runtimes\python\python.exe")
        });
        let out_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repo root")
            .join(".tmp");
        let path = build_merged_bundle(&py, &out_dir).expect("应生成合并库");
        println!("merged bundle -> {}", path.display());
    }
}
