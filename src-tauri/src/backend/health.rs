//! 健康探测:轮询 FastAPI :8001 与 Next.js :3782。
//!
//! 判定策略:只要端口上有进程接受连接并返回了 HTTP 响应(不论状态码),
//! 就认为该服务已监听。原因:DeepTutor 的 health 路径可能随版本变化,
//! 但"能连上"这件事本身就是我们等的那件事(在此之前是 ECONNREFUSED)。

use std::time::Duration;

use serde::Serialize;
use tokio::time::sleep;

/// 后端端口集合。
#[derive(Debug, Clone, Copy)]
pub struct Ports {
    pub api: u16,
    pub web: u16,
}

impl Ports {
    pub fn new(api: u16, web: u16) -> Self {
        Self { api, web }
    }
}

impl Default for Ports {
    fn default() -> Self {
        Self { api: 8001, web: 3782 }
    }
}

/// 单次探测结果。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Health {
    pub api: bool,
    pub web: bool,
}

impl Health {
    pub fn ready(&self) -> bool {
        self.api && self.web
    }
}

/// API 侧按顺序尝试的路径(FastAPI 默认有 /docs,也可用 /api/health 或根路径)。
const API_PATHS: [&str; 4] = ["/docs", "/api/health", "/health", "/"];

/// 健康轮询器。
pub struct Prober {
    ports: Ports,
    client: reqwest::Client,
    interval: Duration,
}

impl Prober {
    pub fn new(ports: Ports) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(1200))
            .no_proxy()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            ports,
            client,
            interval: Duration::from_millis(500),
        }
    }

    /// 对单个端口做一次探活(任一路径返回 HTTP 响应即算就绪)。
    async fn probe(&self, port: u16, paths: &[&str]) -> bool {
        for path in paths {
            let url = format!("http://127.0.0.1:{}{}", port, path);
            if self.client.get(&url).send().await.is_ok() {
                return true;
            }
        }
        false
    }

    /// 做一次完整探测。
    pub async fn check_once(&self) -> Health {
        let (api, web) = tokio::join!(
            self.probe(self.ports.api, &API_PATHS),
            self.probe(self.ports.web, &["/"])
        );
        Health { api, web }
    }

    /// 轮询直到两个服务都就绪,或超时。
    ///
    /// `on_tick` 每轮都会被调用(用于把进度推给 UI),包括最后一轮。
    pub async fn wait_ready(
        &self,
        timeout: Duration,
        mut on_tick: impl FnMut(Health),
    ) -> anyhow::Result<Health> {
        let start = std::time::Instant::now();
        let mut last;

        loop {
            last = self.check_once().await;
            on_tick(last);

            if last.ready() {
                return Ok(last);
            }
            if start.elapsed() >= timeout {
                return Err(anyhow::anyhow!(
                    "等待后端就绪超时({:?}):api(:{})={} web(:{})={}",
                    timeout,
                    self.ports.api,
                    last.api,
                    self.ports.web,
                    last.web
                ));
            }
            sleep(self.interval).await;
        }
    }
}

impl Default for Prober {
    fn default() -> Self {
        Self::new(Ports::default())
    }
}
