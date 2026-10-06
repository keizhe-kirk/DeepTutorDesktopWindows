//! B 站搜索窗口 —— 托盘菜单「搜索哔哩哔哩…」的实现。
//!
//! # 为什么必须自己做一个窗口
//!
//! 两个批评都指向同一个事实：**能力有了，用户却用不了**。
//!
//! - 搜索能力早就挂在后端 `/bilibili/search` 端点上，但**前端一个入口都没有**
//!   —— 前端是上游 Next.js 的编译产物（`.next/standalone`），改不了源码。
//! - 托盘菜单只能点、不能输入，所以拿不到关键词。
//!
//! 于是这里补上缺失的那一环：一个**独立的小窗口**，页面是自己的
//! （`public/bili-search.{html,css,js}`，由 Vite 原样拷进 `dist/`），
//! 通过 Tauri command 与 Rust 侧通信。搜索逻辑仍在补丁包里
//! （`dtpatch_bili.search_cli`），本模块只负责「开窗」。
//!
//! # ★ 页面必须用 `WebviewUrl::App` 加载（别再走临时文件）
//!
//! 先前的实现是把 HTML 内联成常量 → 写到 `%TEMP%` → `win.eval("location.replace
//! ('file://...')")` 跳过去。有两处硬伤，都是真机踩出来的：
//!
//! 1. **CSP 是 `script-src 'self'`**。内联 `<script>` 被 WebView2 直接拦掉，
//!    症状是「窗口开了、按钮全没反应」，控制台还没几条有用的报错。
//!    页面必须整体放在应用自己的 origin 下，脚本外链。
//! 2. **`file://` 的 origin 是 `null`**，且主窗口随时可能被导航到
//!    `http://127.0.0.1:3782`，IPC 在那两个 origin 上都不保证可用。
//!    `WebviewUrl::App(...)` 会走 Tauri 的自定义协议，origin 是 `tauri://`，
//!    `invoke` 一定在。
//!
//! # 为什么不复用主窗口
//!
//! 主窗口会被 `window.location.replace(WEB_URL)` 导航成 DeepTutor Web UI，
//! 在上面叠一层外来 DOM 迟早和上游样式/路由打架。独立窗口互不干扰，
//! 关掉即销毁。
//!
//! # ★ 窗口创建的时机：别在 `setup()` 里同步开
//!
//! 窗口必须在 **UI 线程、且消息循环已经跑起来之后** 创建。在 `.setup()`
//! 里同步调 [`open`] 是竞态的：实测同一次改动里两次成功、一次失败，
//! 报 `WebView2 error: 0x80070578 无效的窗口句柄`，然后窗口静默消失 ——
//! 没有报错弹窗，只有一行日志。所以 [`open`] 内部做了有界重试
//! （[`schedule_retry`]），托盘菜单与启动期调用都吃这一套。
//!
//! # 数据流
//!
//! ```text
//! public/bili-search.js
//!   │ invoke("bili_search", { keyword, page })
//!   ▼
//! commands::bili_search  ->  bilibili::search  ->  dtpatch_bili.search_cli
//!   │
//!   │ invoke("bili_open", { url })
//!   ▼
//! 用系统默认浏览器打开视频
//! ```

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// 搜索窗口的 label。用它防止重复开窗。
///
/// ★ 必须与 `capabilities/main.json` 的 `windows` 名单一致，否则这扇窗口
/// 不在任何 capability 里 —— `Esc` 关窗（`core:window:allow-close`）会失效。
pub const SEARCH_WINDOW: &str = "bili-search";

/// 窗口内页面，相对 `frontendDist`（`../dist`）。
///
/// 源文件在 `public/`，Vite 的 `publicDir` 会原样拷进 `dist/`，
/// dev 下也由 dev server 直接伺服，两条路都通。
const SEARCH_PAGE: &str = "bili-search.html";

/// 防重入：连点菜单两次不该开两个窗口。
static SEARCH_WINDOW_OPEN: AtomicBool = AtomicBool::new(false);

/// 创建失败后的重试次数。
///
/// ★ 为什么需要重试：窗口必须在 **UI 线程、且消息循环已经跑起来之后** 创建。
/// 在 `setup()` 里同步开窗是**竞态**的 —— 实测同一次改动里两次成功、一次
/// 失败，报 `WebView2 error: 0x80070578 无效的窗口句柄`，窗口静默消失。
/// 症状随机、极难复现，所以这里做有界重试兜底。
const MAX_CREATE_ATTEMPTS: u8 = 3;

/// 打开搜索窗口（已存在则聚焦）。
pub fn open(app: &AppHandle) -> tauri::Result<()> {
    open_with_retry(app, 0)
}

fn open_with_retry(app: &AppHandle, attempt: u8) -> tauri::Result<()> {
    if let Some(win) = app.get_webview_window(SEARCH_WINDOW) {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }

    // ★ 先占位再创建：WebviewWindowBuilder 是异步落地的，菜单连点会在窗口
    // 真正出现前重复进入这段代码，于是开出两个一模一样的窗口。
    if SEARCH_WINDOW_OPEN.swap(true, Ordering::SeqCst) {
        return Ok(());
    }

    let win = WebviewWindowBuilder::new(app, SEARCH_WINDOW, WebviewUrl::App(SEARCH_PAGE.into()))
        .title("搜索哔哩哔哩")
        .inner_size(620.0, 680.0)
        .min_inner_size(460.0, 420.0)
        .resizable(true)
        // 不用 decorations(false)：自定义标题栏在 Windows 上要额外处理拖拽、
        // 缩放、双击最大化，得不偿失。用系统标题栏但**不显示在任务栏**
        // （跳过Taskbar），这样它就是个「附属于托盘」的小窗。
        .skip_taskbar(true)
        .build();

    let win = match win {
        Ok(w) => w,
        Err(e) => {
            // 放掉占位，否则重试一进来就被自己挡掉。
            SEARCH_WINDOW_OPEN.store(false, Ordering::SeqCst);
            if attempt + 1 < MAX_CREATE_ATTEMPTS {
                let delay = 400 * u64::from(attempt + 1);
                log::warn!(
                    "创建 B 站搜索窗口失败(第 {} 次): {e}；{delay}ms 后重试",
                    attempt + 1
                );
                schedule_retry(app, attempt + 1, delay);
                return Err(e);
            }
            log::warn!("打开 B 站搜索窗口失败: {e}");
            return Err(e);
        }
    };

    // 窗口被关掉（用户点X、Alt+F4、托盘隐藏等）时清掉占位标记，
    // 否则这个窗口**永远开不了第二次** —— 这是最容易犯的等价。
    win.on_window_event(|event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            SEARCH_WINDOW_OPEN.store(false, Ordering::SeqCst);
            log::debug!("B 站搜索窗口已关闭");
        }
    });
    Ok(())
}

/// 把一次重试排到 UI 线程上执行。
///
/// ★ 必须 `run_on_main_thread`：窗口创建只能在主/UI 线程做，
/// 从后台线程调 `WebviewWindowBuilder::build` 会拿到无效句柄。
fn schedule_retry(app: &AppHandle, attempt: u8, delay_ms: u64) {
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        let inner = handle.clone();
        if let Err(e) = handle.run_on_main_thread(move || {
            let _ = open_with_retry(&inner, attempt);
        }) {
            log::warn!("调度搜索窗口重试失败: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// label 必须和 capability 里的窗口名单一致 —— 不一致时**编译期看不出来**，
    /// 表现为「Esc 关不掉窗口」，所以在这里钉死。
    #[test]
    fn search_window_label_matches_capability() {
        let cap = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("capabilities")
            .join("main.json");
        let text = std::fs::read_to_string(&cap)
            .unwrap_or_else(|e| panic!("读不到 {}: {e}", cap.display()));
        let value: serde_json::Value =
            serde_json::from_str(&text).expect("capabilities/main.json 必须是合法 JSON");
        let listed = value
            .get("windows")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            listed.iter().any(|w| w.as_str() == Some(SEARCH_WINDOW)),
            "capabilities/main.json 的 windows={listed:?} 里必须包含 {SEARCH_WINDOW},\
             否则搜索窗口拿不到 core:window:allow-close(Esc 关窗)等权限"
        );
    }

    /// 页面文件必须真的被 Vite 拷进产物里，否则窗口会开成一个空白页。
    #[test]
    fn search_page_is_copied_into_dist() {
        let public = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("public")
            .join(SEARCH_PAGE);
        assert!(
            public.is_file(),
            "缺少 {} —— 搜索窗口会开成空白页",
            public.display()
        );
    }
}
