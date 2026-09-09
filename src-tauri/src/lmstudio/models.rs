//! LM Studio 模型管理(列表/加载/卸载)。
//!
//! 使用 LM Studio 原生 REST API(`/api/v0/*`),因为只有它能返回 `state`
//! 字段来区分"已下载但未加载"与"已加载到内存",这正是 macOS 端
//! 模型切换界面的核心数据。
//!
//! - `GET  /api/v0/models`       列出所有已下载模型(含 state)
//! - `POST /api/v0/models/load`  加载模型到内存,body {"model": "..."}
//! - `POST /api/v0/models/unload` 从内存卸载,body {"model": "..."}

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};

/// 单次请求超时(加载模型可能较慢)。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// LM Studio 返回的单个模型信息(原生 /api/v0/models 的字段)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    /// "loaded" | "not-loaded" | 其他
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub max_context_length: Option<u64>,
}

impl ModelInfo {
    pub fn is_loaded(&self) -> bool {
        self.state == "loaded"
    }
}

/// 原生 API 返回的模型列表包裹结构。
#[derive(Debug, Deserialize)]
struct NativeList {
    data: Vec<ModelInfo>,
}

/// load/unload 请求体。
#[derive(Debug, Serialize)]
struct LoadBody<'a> {
    model: &'a str,
}

/// LM Studio 原生 /api/v0 客户端。
pub struct ModelsClient {
    base_url: String,
    client: Client,
}

impl ModelsClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .no_proxy()
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
        }
    }

    /// 列出所有已下载模型(含 state 区分是否已加载)。
    ///
    /// 优先走原生 `/api/v0/models`;若 LM Studio 版本较老不支持,
    /// 回退到 OpenAI 兼容 `/v1/models`(只有 id,state 置空)。
    pub async fn list(&self) -> anyhow::Result<Vec<ModelInfo>> {
        // 1) 原生 API
        let native = format!("{}/api/v0/models", self.base_url);
        if let Ok(resp) = self.client.get(&native).send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.json::<NativeList>().await {
                    return Ok(body.data);
                }
            }
        }

        // 2) 回退:OpenAI 兼容 /v1/models
        let compat = format!("{}/v1/models", self.base_url);
        let resp = self.client.get(&compat).send().await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;

        let mut out = Vec::new();
        if let Some(arr) = body.get("data").and_then(|d| d.as_array()) {
            for item in arr {
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    out.push(ModelInfo {
                        id: id.to_string(),
                        state: String::new(),
                        publisher: None,
                        arch: None,
                        size_bytes: None,
                        max_context_length: None,
                    });
                }
            }
        }
        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "LM Studio 返回非 2xx ({}),请确认本地服务器已启动",
                status
            ));
        }
        Ok(out)
    }

    /// 加载模型到内存。
    pub async fn load(&self, model_id: &str) -> anyhow::Result<()> {
        let url = format!("{}/api/v0/models/load", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&LoadBody { model: model_id })
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "加载模型失败 (HTTP {}): {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ))
        }
    }

    /// 从内存卸载模型。
    pub async fn unload(&self, model_id: &str) -> anyhow::Result<()> {
        let url = format!("{}/api/v0/models/unload", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&LoadBody { model: model_id })
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "卸载模型失败 (HTTP {}): {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ))
        }
    }
}
