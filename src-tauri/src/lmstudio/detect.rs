//! LM Studio 自动探测。
//!
//! LM Studio 本地服务器默认在 http://127.0.0.1:1234 暴露 OpenAI 兼容 API。
//! 探测策略:GET /v1/models 返回 2xx 即认为 LM Studio 已启动且可用。

use std::time::Duration;

use reqwest::Client;

/// 默认探测端口。
pub const DEFAULT_PORT: u16 = 1234;

/// 探测超时(连接被拒时快速返回,不卡启动流程)。
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Debug, Clone)]
pub struct Detector {
    client: Client,
}

impl Detector {
    pub fn new() -> Self {
        // no_proxy:避免系统代理劫持 127.0.0.1
        let client = Client::builder()
            .timeout(PROBE_TIMEOUT)
            .no_proxy()
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { client }
    }

    /// 探测 LM Studio 可用性,返回 Base URL(不含 /v1)。
    ///
    /// 支持 DEEPTUTOR_LMSTUDIO_URL 环境变量覆盖默认端口(便于用户自定义端口)。
    pub async fn probe(&self) -> Option<String> {
        let host = std::env::var("DEEPTUTOR_LMSTUDIO_URL")
            .ok()
            .unwrap_or_else(|| format!("http://127.0.0.1:{}", DEFAULT_PORT));
        let base = host.trim_end_matches('/').to_string();

        for path in ["/v1/models", "/api/v0/models"] {
            let url = format!("{}{}", base, path);
            if let Ok(resp) = self.client.get(&url).send().await {
                if resp.status().is_success() {
                    return Some(base);
                }
            }
        }
        None
    }
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}
