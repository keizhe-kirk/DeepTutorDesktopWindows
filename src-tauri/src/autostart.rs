//! 注册表自启动(M0 stub)。
//! M1 计划:通过 tauri_plugin_autostart 暴露的 enable/disable 命令,
//! 在用户首次勾选"开机启动"时写入 HKCU\Software\Microsoft\Windows\CurrentVersion\Run。
//! 这里预留 API 形态。

#![allow(dead_code)]

pub fn enable() -> anyhow::Result<()> {
    // M1 接入 tauri_plugin_autostart
    Err(anyhow::anyhow!("M1 待实现"))
}

pub fn disable() -> anyhow::Result<()> {
    Err(anyhow::anyhow!("M1 待实现"))
}
