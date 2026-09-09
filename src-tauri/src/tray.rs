//! 系统托盘菜单。
//!
//! 使用 Tauri 2 内置的 TrayIconBuilder(不再依赖 tauri-plugin-system-tray,
//! 该插件在 Tauri 2 已移除)。
//!
//! 菜单项:
//! - 打开主窗口 / 隐藏主窗口
//! - 重启后端
//! - 退出

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let menu = Menu::with_items(
        app,
        &[
            &PredefinedMenuItem::about(app, Some("关于 DeepTutor"), None)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "toggle", "打开/隐藏主窗口", true, None::<&str>)?,
            &MenuItem::with_id(app, "restart", "重启后端", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::quit(app, Some("退出"))?,
        ],
    )?;

    let _tray = TrayIconBuilder::with_id("main-tray")
        .icon(app.default_window_icon().cloned().unwrap())
        .tooltip("DeepTutor")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "toggle" => {
                if let Some(win) = app.get_webview_window("main") {
                    if win.is_visible().unwrap_or(false) {
                        let _ = win.hide();
                    } else {
                        let _ = win.show();
                        let _ = win.set_focus();
                    }
                }
            }
            "restart" => {
                if let Some(runner) = app.try_state::<std::sync::Arc<crate::backend::runner::Runner>>()
                {
                    let runner = runner.inner().clone();
                    let handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let _ = runner.stop().await;
                        crate::backend::boot::run(handle, runner).await;
                    });
                }
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 左键单击唤起主窗口(Windows 常见交互)
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                if let Some(app) = tray.app_handle().get_webview_window("main") {
                    let _ = app.show();
                    let _ = app.set_focus();
                }
            }
        })
        .build(app)?;

    Ok(())
}
