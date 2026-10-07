//!把「B 站搜索」注入到沉浸式观看页里。
//!
//! # 为什么不另开窗口
//!
//! 用户明确要求「完美接入个性化学习里的沉浸式观看板块」。上游前端
//! (`MediaReadingStage`, chunk 4057) 本来就内置了完整的 B 站播放器:
//!
//! ```text
//! "bilibili" === e.source_kind && V ? <B站 iframe> : <YouTube iframe>
//! ```
//!
//! 所以真正缺的只是**在那个页面里能搜到视频**。页面自带一个输入框,
//! i18n key是 `Search videos or paste a video link`(中文:"搜索视频或粘贴视频链接"),
//! 粘贴链接会走 `POST /api/video-learning/materials/resolve` —— 那条路我们已经
//! 打过补丁, B 站链接能正常落进 reading 目录并被原生播放器接管。
//!
//! # 为什么用「注入」而不是改前端 chunk
//!
//! 前端是 `.next/standalone` 的压缩产物, 改它等于跟上游每次更新赛跑。
//! 注入只依赖两样东西: **i18n 文案** 和 **输入框 DOM**, 比压缩后的符号稳定得多。
//!
//! # 为什么用 MutationObserver 而不是 on_page_load
//!
//! Next.js App Router 是**客户端路由** —— 从别的页面切到沉浸式观看不会触发
//! `on_page_load`。所以注入的脚本自己盯 DOM: 输入框一出现就装按钮。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::Manager;

static INJECTED: AtomicBool = AtomicBool::new(false);

/// 注入脚本。
///
/// ★ 全程只用 `element.style.*`（CSSOM），不插 `<style>` 元素、不写
///   `style="..."` 属性 —— 页面 CSP 的 `style-src` 只拦后两者。
const PANEL_JS: &str = r##"
(function () {
  if (window.__dtBiliPanel) { return; }
  window.__dtBiliPanel = true;

  var LOG = '[dtbili] ';
  function log() {
    try { console.log.apply(console, [LOG].concat([].slice.call(arguments))); } catch (e) {}
  }

  // ---- 文案锚点：英文 i18n key + 中文译文，两边都认----
  var ANCHOR = /视频链接|粘贴\s*视频|Search videos or paste a video link|paste a video link/i;

  function findAnchorInput() {
    var nodes = document.querySelectorAll('input, textarea');
    for (var i = 0; i < nodes.length; i++) {
      var el = nodes[i];
      var hint = [el.placeholder || '', el.getAttribute('aria-label') || '',
                  el.getAttribute('data-testid') || ''].join(' ');
      if (ANCHOR.test(hint)) { return el; }
    }
    return null;
  }

  function invoke(cmd, args) {
    var internals = window.__TAURI_INTERNALS__;
    if (!internals || typeof internals.invoke !== 'function') {
      return Promise.reject(new Error('Tauri IPC 不可用'));
    }
    return internals.invoke(cmd, args || {});
  }

  // ---- React 受控组件的 value 写入法----
  function setReactValue(el, value) {
    var proto = (el.tagName === 'TEXTAREA') ? HTMLTextAreaElement.prototype
                                           : HTMLInputElement.prototype;
    var desc = Object.getOwnPropertyDescriptor(proto, 'value');
    if (desc && desc.set) { desc.set.call(el, value); } else { el.value = value; }
    el.dispatchEvent(new Event('input',  { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  }

  function submitVia(input) {
    // 优先找旁边的提交按钮；找不到就用 Enter 键事件（React 委托在 root 上）。
    var box = input.parentElement;
    if (box) {
      var btns = box.querySelectorAll('button');
      for (var i = 0; i < btns.length; i++) {
        var t = (btns[i].textContent || '').trim();
        if (t && /^(搜索|Search|添加|Add|打开|Open|Go)/i.test(t)) { btns[i].click(); return true; }
      }
    }
    ['keydown', 'keypress', 'keyup'].forEach(function (t) {
      input.dispatchEvent(new KeyboardEvent(t, {
        key: 'Enter', code: 'Enter', keyCode: 13, which: 13, bubbles: true, cancelable: true
      }));
    });
    return true;
  }

  // ---- 面板 ----
  var overlay = null;

  function ensureOverlay() {
    if (overlay && document.body.contains(overlay)) { return overlay; }
    overlay = document.createElement('div');
    overlay.style.cssText = 'position:fixed;inset:0;z-index:2147483000;display:none;'
      + 'align-items:center;justify-content:center;background:rgba(0,0,0,.45)';

    var panel = document.createElement('div');
    panel.style.cssText = 'width:min(680px,92vw);max-height:82vh;display:flex;flex-direction:column;'
      + 'background:var(--card,#fff);color:var(--foreground,#111);border-radius:12px;'
      + 'box-shadow:0 18px 50px rgba(0,0,0,.28);overflow:hidden';

    // 标题栏
    var head = document.createElement('div');
    head.style.cssText = 'display:flex;align-items:center;gap:8px;padding:12px 14px;'
      + 'border-bottom:1px solid var(--border,#e5e5e5)';
    var title = document.createElement('div');
    title.textContent = '搜索哔哩哔哩';
    title.style.cssText = 'font-size:14px;font-weight:500;flex:1';
    var close = document.createElement('button');
    close.type = 'button';
    close.textContent = '关闭';
    close.style.cssText = 'font-size:12px;padding:4px 10px;border-radius:6px;cursor:pointer;'
      + 'border:1px solid var(--border,#e5e5e5);background:transparent;color:inherit';
    close.onclick = function () { overlay.style.display = 'none'; };
    head.appendChild(title); head.appendChild(close);

    // 搜索行
    var row = document.createElement('div');
    row.style.cssText = 'display:flex;gap:8px;padding:12px 14px';
    var q = document.createElement('input');
    q.type = 'text';
    q.placeholder = '输入关键词，如「线性代数」';
    q.style.cssText = 'flex:1;min-width:0;height:34px;padding:0 10px;font-size:13px;border-radius:8px;'
      + 'border:1px solid var(--border,#e5e5e5);background:var(--background,#fff);color:inherit';
    var go = document.createElement('button');
    go.type = 'button';
    go.textContent = '搜索';
    go.style.cssText = 'height:34px;padding:0 16px;font-size:13px;border-radius:8px;cursor:pointer;'
      + 'border:0;background:var(--primary,#e9544b);color:var(--primary-foreground,#fff)';
    row.appendChild(q); row.appendChild(go);

    // 状态 + 结果
    var status = document.createElement('div');
    status.style.cssText = 'padding:0 14px 8px;font-size:12px;color:var(--muted-foreground,#888);min-height:18px';
    var list = document.createElement('div');
    list.style.cssText = 'flex:1;overflow:auto;padding:0 8px 12px';

    panel.appendChild(head); panel.appendChild(row); panel.appendChild(status); panel.appendChild(list);
    overlay.appendChild(panel);
    document.body.appendChild(overlay);
    overlay.addEventListener('click', function (e) { if (e.target === overlay) { overlay.style.display = 'none'; } });

    async function run() {
      var term = (q.value || '').trim();
      if (!term) { status.textContent = '请输入关键词'; return; }
      status.textContent = '正在搜索「' + term + '」…';
      list.textContent = '';
      go.disabled = true;
      try {
        var r = await invoke('bili_search', { keyword: term, page: 1 });
        go.disabled = false;
        var items = (r && r.results) || [];
        if (!items.length) {
          status.textContent = (r && r.error) ? ('搜索失败：' + r.error) : '没有找到相关视频';
          return;
        }
        status.textContent = '第 1 页 · ' + items.length + ' 条';
        items.forEach(function (it) {
          var card = document.createElement('button');
          card.type = 'button';
          card.style.cssText = 'display:flex;gap:10px;width:100%;text-align:left;padding:8px;'
            + 'border-radius:8px;border:0;background:transparent;cursor:pointer;align-items:flex-start';
          card.onmouseenter = function () { card.style.background = 'var(--muted,#f3f3f3)'; };
          card.onmouseleave = function () { card.style.background = 'transparent'; };

          var th = document.createElement('div');
          th.style.cssText = 'width:112px;height:63px;flex:none;border-radius:6px;object-fit:cover;'
            + 'background:var(--muted,#eee);overflow:hidden';
          var raw = String(it.thumbnail_url || '');
          var url = raw.indexOf('//') === 0 ? 'https:' + raw
                 : (raw.indexOf('http://') === 0 ? 'https://' + raw.slice(7) : raw);
          if (url) {
            var img = document.createElement('img');
            img.src = url; img.referrerPolicy = 'no-referrer'; img.alt = '';
            img.style.cssText = 'width:100%;height:100%;object-fit:cover';
            // ★ 封面 404/防盗链时会把浏览器破图图标露出来，很难看。
            //   加载失败就把 img 摘掉，只留纯色占位块。
            img.addEventListener('error', function () {
              if (img.parentNode) { img.parentNode.removeChild(img); }
            });
            th.appendChild(img);
          }

          var meta = document.createElement('div');
          meta.style.cssText = 'min-width:0;flex:1';
          var t1 = document.createElement('div');
          // ★ 用户数据一律 textContent：B 站标题含 < > 和 <em class="keyword"> 残留。
          t1.textContent = it.title || it.bvid || '(无标题)';
          t1.style.cssText = 'font-size:13px;line-height:1.45;display:-webkit-box;-webkit-line-clamp:2;'
            + '-webkit-box-orient:vertical;overflow:hidden';
          var t2 = document.createElement('div');
          var bits = [];
          if (it.author) { bits.push(it.author); }
          if (it.duration_seconds) { bits.push(fmtDur(it.duration_seconds)); }
          if (it.play_count) { bits.push(fmtCount(it.play_count) + '播放'); }
          t2.textContent = bits.join(' · ');
          t2.style.cssText = 'font-size:11px;color:var(--muted-foreground,#888);margin-top:4px';
          meta.appendChild(t1); meta.appendChild(t2);

          card.appendChild(th); card.appendChild(meta);
          card.onclick = function () { pick(it, url); };
          list.appendChild(card);
        });
      } catch (e) {
        go.disabled = false;
        status.textContent = '搜索出错：' + ((e && e.message) || e);
      }
    }

    function fmtDur(sec) {
      sec = Math.max(0, Math.round(Number(sec) || 0));
      if (!sec) { return ''; }
      var h = Math.floor(sec / 3600), m = Math.floor((sec % 3600) / 60), s = sec % 60;
      var pad = function (n) { return n < 10 ? '0' + n : String(n); };
      return h > 0 ? (h + ':' + pad(m) + ':' + pad(s)) : (m + ':' + pad(s));
    }
    function fmtCount(n) {
      n = Number(n) || 0;
      if (n <= 0) { return ''; }
      if (n >= 1e8) { return (n / 1e8).toFixed(1).replace(/\.0$/, '') + '亿'; }
      if (n >= 1e4) { return (n / 1e4).toFixed(1).replace(/\.0$/, '') + '万'; }
      return String(Math.round(n));
    }

    function pick(item, thumbUrl) {
      var target = findAnchorInput();
      var url = String(item.url || '');
      if (!url && item.bvid) { url = 'https://www.bilibili.com/video/' + item.bvid + '/'; }
      if (!target || !url) {
        status.textContent = '没能定位到输入框，请刷新页面后重试';
        return;
      }
      setReactValue(target, url);
      overlay.style.display = 'none';
      log('picked', url);
      setTimeout(function () { submitVia(target); }, 120);
    }

    go.onclick = run;
    q.addEventListener('keydown', function (e) { if (e.key === 'Enter') { e.preventDefault(); run(); } });
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape' && overlay && overlay.style.display !== 'none') {
        overlay.style.display = 'none';
      }
    });
    return overlay;
  }

  // ---- 装按钮 ----
  function install() {
    var input = findAnchorInput();
    if (!input) { return false; }
    if (input.dataset && input.dataset.dtBiliBtn === '1') { return true; }
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.textContent = 'B站搜索';
    btn.style.cssText = 'margin-left:8px;height:34px;padding:0 12px;font-size:13px;cursor:pointer;'
      + 'border-radius:8px;border:1px solid var(--border,#e5e5e5);'
      + 'background:transparent;color:var(--foreground,inherit)';
    btn.onclick = function () {
      var ov = ensureOverlay();
      ov.style.display = 'flex';
      var q = ov.querySelector('input');
      if (q) { q.focus(); }
    };
    if (input.dataset) { input.dataset.dtBiliBtn = '1'; }
    (input.parentElement || input).insertBefore(btn, input.nextSibling);
    log('button installed');
    return true;
  }

  // ---- 盯 DOM：SPA 切页不会触发 on_page_load，只能自己观察 ----
  function boot() {
    if (install()) { return; }
    var mo = new MutationObserver(function () {
      if (install()) { mo.disconnect(); }
    });
    mo.observe(document.documentElement, { childList: true, subtree: true });
    log('observing');
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }
})();
"##;

/// 真机验证用的后门：把要执行的 JS 写进 `%LOCALAPPDATA%\DeepTutor\bili_eval.js`，
/// 应用启动后会在主窗口执行一次（然后删掉文件）。
///
/// ★ 为什么要有这个后门：WebView2 只有在**进程能派生子进程**时才起得来。
///   从终端启动的应用在工具的 Job 对象里，`msedgewebview2.exe` 常常起不来
///   （症状：应用活着、窗口在、客户区纯白/纯黑、wry 一条错都不报）。
///   而任务计划程序被安全策略拉黑、`explorer.exe` 又不继承环境变量 ——
///   文件标记是唯一不依赖启动方式的通道。
///
/// ★★ 只在 debug 构建里存在。这是「往本地目录丢个文件就能让应用执行任意
///   JS」的后门，**绝不能随发布版出去** —— release 构建下这段代码根本不存在。
#[cfg(debug_assertions)]
fn eval_flag_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(std::path::PathBuf::from(base).join("DeepTutor").join("bili_eval.js"))
}

#[cfg(debug_assertions)]
fn spawn_eval_flag_watcher(app: tauri::AppHandle) {
    let Some(path) = eval_flag_path() else { return };
    std::thread::spawn(move || {
        // ★ 必须**重复**执行而不是一次：脚本里很可能要 location.assign 到沉浸式观看页，
        //   而导航会把 JS 上下文整个换掉，一次性 eval 的脚本随页面一起消失。
        //   脚本自己用 sessionStorage 做幂等（见 .tmp/bili_demo.js）。
        //   文件不自动删 —— 验证完手动删，否则会一直重复。
        let mut executed = 0usize;
        let mut seen = false;
        for _round in 0..60 {
            std::thread::sleep(Duration::from_millis(2500));
            match std::fs::read_to_string(&path) {
                Ok(script) => {
                    if script.trim().is_empty() {
                        continue;
                    }
                    // ★ 第一次读到才算「开始」；之后读不到 = 验证结束，退出。
                    if !seen {
                        seen = true;
                        log::info!("检测到 bili_eval.js，开始重复执行");
                    }
                    let Some(win) = app.get_webview_window("main") else {
                        continue;
                    };
                    if win.eval(&script).is_ok() {
                        executed += 1;
                        if executed == 1 {
                            log::info!("bili_eval.js 已执行（{} 字节）", script.len());
                        }
                    }
                }
                Err(_) => {
                    // ★ 没出现过就继续等；出现过又消失才收工。
                    //   写成"读不到就 return"会在文件还没创建时直接退出——
                    //   验证脚本往往启动后才放标记，一探测扑空就再也不会执行（踩过）。
                    if seen {
                        log::info!("bili_eval.js 已移除，监听结束（共执行 {executed} 次）");
                        return;
                    }
                }
            }
        }
        log::info!("bili_eval.js 监听轮次用尽，共执行 {executed} 次");
    });
}

/// 把面板脚本注入主窗口。
///
/// ★★ 必须**周期性重复**注入，绝不能只打一次。
///
/// 主窗口一开始加载的不是 Web UI，而是壳自己的启动页（`dist/index.html`）。
/// 注入脚本装好 `MutationObserver` 之后一直待在**那份文档**里；等后端就绪、
/// 前端 `location.replace(http://127.0.0.1:<web_port>)` 跳到 Web UI 时，整个
/// JS 上下文被整体替换 —— 脚本和它的观察者一起消失。
///
/// 症状极具迷惑性：注入日志明明打了「已注入（第 0 次尝试）」，Rust 侧一切正常，
/// 但沉浸式观看页上就是没有那个按钮（实测 v0.2.12 就是这样）。
///
/// 脚本自带 `window.__dtBiliPanel` 幂等标记，所以重复注入是免费的；
/// 顺带还能自愈「用户手动刷新页面」这类硬导航。
///
/// 主窗口来自 `tauri.conf.json` 的 `app.windows`，不是Rust 建的，拿不到
/// builder 上的 `on_page_load`，所以用「延时 + 周期 eval」。
pub fn inject(app: &tauri::AppHandle) {
    if INJECTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        // 真机验证后门（仅 debug 构建存在）必须独立起线程：
        // 它挂在periodic 注入循环里，而那个循环是`loop`，后面的代码不可达。
        #[cfg(debug_assertions)]
        spawn_eval_flag_watcher(handle.clone());

        let mut ok = 0u32;
        loop {
            std::thread::sleep(Duration::from_millis(3000));
            let Some(win) = handle.get_webview_window("main") else {
                continue;
            };
            if win.eval(PANEL_JS).is_ok() {
                ok += 1;
                if ok == 1 {
                    log::info!("B 站搜索面板已注入主窗口（周期性注入，每 3s 一次，脚本自带幂等）");
                }
            }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 注入脚本必须幂等：eval 可能被重试多次，重复执行会插出多个按钮。
    #[test]
    fn panel_script_is_idempotent() {
        assert!(
            PANEL_JS.contains("window.__dtBiliPanel"),
            "缺少幂等标记，重复注入会插出多个按钮"
        );
    }

    /// 定位锚点要同时认英文 i18n key 和中文译文 —— 界面语言由用户设置决定，
    /// 只认一种就有一半用户看不到按钮。
    #[test]
    fn anchor_covers_both_locales() {
        assert!(PANEL_JS.contains("Search videos or paste a video link"));
        assert!(PANEL_JS.contains("视频链接"));
    }

    /// ★ 用户数据一律 textContent。B 站标题里会有 `<` `>` 和
    /// `<em class="keyword">` 残留，拼 innerHTML 既破版也是注入面。
    #[test]
    fn user_data_never_uses_inner_html() {
        assert!(
            !PANEL_JS.contains("innerHTML"),
            "注入脚本里出现 innerHTML —— 用户数据必须走 textContent"
        );
        assert!(PANEL_JS.contains("textContent"));
    }

    /// 缩略图是协议相对 URL（`//i0.hdslb.com/...`），`http://` 封面会被当混合内容拦。
    #[test]
    fn thumbnails_are_normalised_to_https() {
        assert!(PANEL_JS.contains("indexOf('//') === 0 ? 'https:'"));
        assert!(PANEL_JS.contains("'https://' + raw.slice(7)"));
    }

    /// ★ 页面 CSP 的 `style-src` 会拦 `<style>` 元素和 `style="..."` 属性，
    /// 但**不拦** CSSOM（`el.style.x = ...`）。面板必须只用后者。
    #[test]
    fn styling_avoids_inline_style_element() {
        assert!(
            !PANEL_JS.contains("createElement('style')") && !PANEL_JS.contains("createElement(\"style\")"),
            "不要建 <style> 元素，会被页面 CSP 拦掉"
        );
        assert!(PANEL_JS.contains(".style.cssText"));
    }

    /// 受控组件写入 value 必须走原生 setter，否则 React 收不到 onChange。
    #[test]
    fn react_controlled_value_uses_native_setter() {
        assert!(PANEL_JS.contains("Object.getOwnPropertyDescriptor"));
        assert!(PANEL_JS.contains("new Event('input'"));
    }

    /// 封面 404 / 防盗链时不能露出浏览器破图图标。
    #[test]
    fn thumbnail_has_error_fallback() {
        assert!(
            PANEL_JS.contains("addEventListener('error'"),
            "缩略图缺 onerror 兜底：加载失败会显示浏览器破图图标"
        );
    }

    /// ★ 回归守卫：注入必须**周期性**执行，不能只打一次。
///
/// v0.2.12 就是这里翻的车：一次性 eval 把脚本打在壳的**启动页**上，
/// 后端就绪后前端 `location.replace` 换掉整个文档，脚本跟着消失，
/// 于是「注入日志一切正常，但沉浸式观看页上没有那个按钮」。
#[test]
fn injection_is_periodic_not_one_shot() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bili_panel.rs"),
    )
    .expect("应能读到自己的源码");
    let body = src
        .split_once("pub fn inject")
        .map(|(_, b)| b)
        .expect("应有 inject 函数");
    // ★ 必须在 `mod tests` 之前截断 —— 本测试的断言文案里就含
    // `for attempt in 0..` 这个字符串，不截断会自己匹配自己，恒失败。
    let body = body.split("#[cfg(test)]").next().unwrap_or(body);
    assert!(
        body.contains("loop {"),
        "inject 里必须有无限循环的周期注入，否则导航一次就丢"
    );
    assert!(
        !body.contains("for attempt in 0.."),
        "不要退回「有限次重试」—— 那正是 v0.2.12 按钮不出现的根因"
    );
}

/// 面板要能调 `bili_search`，所以主窗口必须在 capability 的 windows 里。
    #[test]
    fn main_window_may_invoke_bili_search() {
        let caps = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities/main.json");
        let text = std::fs::read_to_string(&caps).expect("capabilities/main.json 应存在");
        assert!(
            text.contains("\"main\""),
            "主窗口不在 capability 的 windows 里 → 注入脚本调不到 bili_search"
        );
    }
}
