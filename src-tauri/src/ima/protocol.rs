//! 自定义协议路由(占位)。
//! 计划:
//! - 在 tauri.conf.json 注册 `deeptutor://` scheme
//! - 在 setup hook 里通过 Builder::register_uri_scheme_protocol 处理
//! - WebView 收到 deeptutor://workspace?session=xxx 时,将其重写为
//!   `http://127.0.0.1:3782/?deeplink=...` 或直接 emit 事件给前端
//!
//! M0 仅暴露 register 入口。

use tauri::{AppHandle, Runtime};

pub fn register<R: Runtime>(_app: &AppHandle<R>) -> tauri::Result<()> {
    // M1:接入 tauri::Builder::register_uri_scheme_protocol("deeptutor", ...)
    // M0:no-op
    Ok(())
}
