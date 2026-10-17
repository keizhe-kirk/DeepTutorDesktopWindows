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

  // ---- 搜索：同源 fetch ----
  //
  // ★ 为什么不用 Tauri IPC：主窗口一开始是本地页，后端就绪后前端
  //   location.replace 到 http://127.0.0.1:<web_port> —— 这时它对 Tauri 是
  //   **远程来源**。Tauri 2 的 capability 若没有 `remote.urls` 声明，
  //   对远程来源一律不生效，于是每条命令都被 ACL 拒：
  //   `Command bili_search not allowed by ACL`（v0.2.13 实测）。
  //   同源 fetch 走 HTTP，完全不经过 IPC 层，因此不受 capability 影响。
  //
  // ★★ 不要再加「fetch 失败就回落 IPC」。v0.2.14 犯过这个错：端点 404 时
  //   fetch 抛错被 catch 吞掉，回落 IPC 又报 ACL ——用户看到的仍是
  //   `not allowed by ACL`，而真因是**后端补丁没启用**（后端热更新到了
  //   白名单之外的版本）。错误被替换成误导性文案，排查成本极高。
  //   现在直接把真实原因摊开，并对 404 给出可执行的判断依据。
  var SEARCH_API = '/api/video-learning/bilibili/search';
  function search(term, page) {
    var url = SEARCH_API + '?q=' + encodeURIComponent(term)
                       + '&page=' + encodeURIComponent(page);
    return fetch(url, { credentials: 'same-origin', cache: 'no-store' })
      .then(function (r) {
        if (r.status === 404) {
          throw new Error('HTTP 404 —— 后端没有挂上 B 站搜索端点，通常是'
            + '补丁对当前后端版本未启用（版本不在白名单）。实际后端版本请看'
            + '托盘「检查后端更新…」');
        }
        if (!r.ok) { throw new Error('HTTP ' + r.status); }
        return r.json();
      });
  }

  // ---- 字幕检索（#3）：把页面已拉取的材料数据留下来 ----
  //
  // ★ 为什么不新加后端端点：沉浸式观看页打开时，前端已经把整份材料
  //   （含 transcript.cues / segments）拉回来了，只是藏在 React 状态里，
  //   注入脚本够不着。包一层 fetch 把这些响应存下来，检索纯本地做 ——
  //   零后端依赖、零额外请求、离线也能搜。
  // ★ 为什么装在 fetch 上而不是轮询 DOM：字幕列表是虚拟滚动的，DOM 里
  //   只有可见那几条；而材料 JSON 是全量。
  // ★ 为什么早点装：SPA 路由切换不换文档，补丁装一次就全程有效；面板
  //   每 3s 周期注入 + 幂等标记，用户打开视频时它早已装好。
  var SUB_RECORDER = '__dtBiliSubRecorder';

  function looksLikeMaterial(j) {
    if (!j || typeof j !== 'object') { return false; }
    var t = j.transcript;
    var cues = t && Array.isArray(t.cues) ? t.cues : null;
    var segs = Array.isArray(j.segments) ? j.segments
             : (t && Array.isArray(t.segments) ? t.segments : null);
    return !!j.material_id && !!((cues && cues.length) || (segs && segs.length));
  }

  function installRecorder() {
    if (window[SUB_RECORDER]) { return; }
    window[SUB_RECORDER] = { material: null, lastSeek: 0 };
    var orig = window.fetch;
    if (typeof orig !== 'function') { return; }
    window.fetch = function () {
      var out = orig.apply(this, arguments);
      try {
        var input = arguments[0];
        var href = typeof input === 'string' ? input
                 : (input && input.url) ? String(input.url) : '';
        // 只克隆可能带材料的响应，避免给每个请求都加 JSON 解析开销
        if (/material|reading|video-learning|watching/i.test(href)) {
          out.then(function (resp) {
            try {
              resp.clone().json().then(function (j) {
                if (looksLikeMaterial(j)) { window[SUB_RECORDER].material = j; }
              }, function () {});
            } catch (e) {}
          }, function () {});
        }
      } catch (e) {}
      return out;
    };
    log('subtitle recorder installed');
  }

  function recordedCues() {
    var rec = window[SUB_RECORDER];
    var m = rec && rec.material;
    if (!m) { return []; }
    var t = m.transcript || {};
    var rows = Array.isArray(t.cues) && t.cues.length ? t.cues
             : (Array.isArray(t.segments) && t.segments.length ? t.segments
             : (Array.isArray(m.segments) && m.segments.length ? m.segments : []));
    return rows.filter(function (r) {
      return r && typeof r === 'object' && String(r.text || '').trim();
    });
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
        var r = await search(term, 1);
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

  // ---- 字幕检索面板 ----
  var subOverlay = null;

  // ★ 跳转机制与前端自己的 seek 完全一致：B 站 iframe 的 src 带 `t=` 参数，
  //   改参数即重载并从该秒开始（chunk 1241 里 `t:String(Math.floor(c))`）。
  //   React 只在 src 相对自己上次渲染值变化时才写 DOM，所以我们从外部改的
  //   属性能一直活到下一次前端主动 seek。
  //   高能进度条（#4）也复用这个查找器，别各写各的。
  function findBilibiliIframe() {
    var nodes = document.querySelectorAll('iframe');
    for (var i = 0; i < nodes.length; i++) {
      var src = nodes[i].src || '';
      if (src.indexOf('player.bilibili.com/player.html') !== -1) { return nodes[i]; }
    }
    return null;
  }

  function seekBilibili(seconds) {
    var frame = findBilibiliIframe();
    if (!frame) { return false; }
    try {
      var url = new URL(frame.src);
      url.searchParams.set('t', String(Math.max(0, Math.floor(seconds))));
      frame.src = url.toString();
      if (window[SUB_RECORDER]) { window[SUB_RECORDER].lastSeek = seconds; }
      return true;
    } catch (e) { return false; }
  }

  function fmtClock(sec) {
    sec = Math.max(0, Math.round(Number(sec) || 0));
    var h = Math.floor(sec / 3600), m = Math.floor((sec % 3600) / 60), s = sec % 60;
    var pad = function (n) { return n < 10 ? '0' + n : String(n); };
    return h > 0 ? (h + ':' + pad(m) + ':' + pad(s)) : (m + ':' + pad(s));
  }

  // 命中高亮：三段全是 textContent / createTextNode，绝不拼 HTML 字符串。
  function appendHighlighted(container, text, needle) {
    var idx = text.toLowerCase().indexOf(needle);
    if (idx === -1) { container.appendChild(document.createTextNode(text)); return; }
    container.appendChild(document.createTextNode(text.slice(0, idx)));
    var mark = document.createElement('span');
    mark.textContent = text.slice(idx, idx + needle.length);
    mark.style.cssText = 'background:var(--primary,#e9544b);'
      + 'color:var(--primary-foreground,#fff);border-radius:3px;padding:0 2px';
    container.appendChild(mark);
    container.appendChild(document.createTextNode(text.slice(idx + needle.length)));
  }

  function ensureSubOverlay() {
    if (subOverlay && document.body.contains(subOverlay)) { return subOverlay; }
    subOverlay = document.createElement('div');
    subOverlay.style.cssText = 'position:fixed;inset:0;z-index:2147483000;display:none;'
      + 'align-items:center;justify-content:center;background:rgba(0,0,0,.45)';

    var panel = document.createElement('div');
    panel.style.cssText = 'width:min(680px,92vw);max-height:82vh;display:flex;flex-direction:column;'
      + 'background:var(--card,#fff);color:var(--foreground,#111);border-radius:12px;'
      + 'box-shadow:0 18px 50px rgba(0,0,0,.28);overflow:hidden';

    var head = document.createElement('div');
    head.style.cssText = 'display:flex;align-items:center;gap:8px;padding:12px 14px;'
      + 'border-bottom:1px solid var(--border,#e5e5e5)';
    var title = document.createElement('div');
    title.textContent = '字幕检索';
    title.style.cssText = 'font-size:14px;font-weight:500;flex:1';
    var close = document.createElement('button');
    close.type = 'button';
    close.textContent = '关闭';
    close.style.cssText = 'font-size:12px;padding:4px 10px;border-radius:6px;cursor:pointer;'
      + 'border:1px solid var(--border,#e5e5e5);background:transparent;color:inherit';
    close.onclick = function () { subOverlay.style.display = 'none'; };
    head.appendChild(title); head.appendChild(close);

    var row = document.createElement('div');
    row.style.cssText = 'display:flex;gap:8px;padding:12px 14px';
    var q = document.createElement('input');
    q.type = 'text';
    q.placeholder = '搜本视频字幕，如「特征值」';
    q.style.cssText = 'flex:1;min-width:0;height:34px;padding:0 10px;font-size:13px;border-radius:8px;'
      + 'border:1px solid var(--border,#e5e5e5);background:var(--background,#fff);color:inherit';
    var go = document.createElement('button');
    go.type = 'button';
    go.textContent = '检索';
    go.style.cssText = 'height:34px;padding:0 16px;font-size:13px;border-radius:8px;cursor:pointer;'
      + 'border:0;background:var(--primary,#e9544b);color:var(--primary-foreground,#fff)';
    row.appendChild(q); row.appendChild(go);

    var status = document.createElement('div');
    status.style.cssText = 'padding:0 14px 8px;font-size:12px;color:var(--muted-foreground,#888);min-height:18px';
    var list = document.createElement('div');
    list.style.cssText = 'flex:1;overflow:auto;padding:0 8px 12px';

    panel.appendChild(head); panel.appendChild(row); panel.appendChild(status); panel.appendChild(list);
    subOverlay.appendChild(panel);
    document.body.appendChild(subOverlay);
    subOverlay.addEventListener('click', function (e) {
      if (e.target === subOverlay) { subOverlay.style.display = 'none'; }
    });

    function run() {
      var term = (q.value || '').trim();
      if (!term) { status.textContent = '请输入关键词'; return; }
      var cues = recordedCues();
      if (!cues.length) {
        status.textContent = '还没载入字幕数据 —— 请先打开这个视频的播放页，'
          + '等字幕加载完（或点一下字幕列表）再试';
        list.textContent = '';
        return;
      }
      var needle = term.toLowerCase();
      var hits = [];
      for (var i = 0; i < cues.length && hits.length < 100; i++) {
        var text = String(cues[i].text || '');
        if (text.toLowerCase().indexOf(needle) !== -1) {
          hits.push({ start: Number(cues[i].start) || 0, text: text });
        }
      }
      if (!hits.length) {
        status.textContent = '字幕里没有「' + term + '」';
        list.textContent = '';
        return;
      }
      status.textContent = '命中 ' + hits.length + ' 条 · 点一条跳到那一秒';
      list.textContent = '';
      hits.forEach(function (hit) {
        var card = document.createElement('button');
        card.type = 'button';
        card.style.cssText = 'display:flex;gap:10px;width:100%;text-align:left;padding:8px;'
          + 'border-radius:8px;border:0;background:transparent;cursor:pointer;align-items:flex-start';
        card.onmouseenter = function () { card.style.background = 'var(--muted,#f3f3f3)'; };
        card.onmouseleave = function () { card.style.background = 'transparent'; };

        var stamp = document.createElement('div');
        stamp.textContent = fmtClock(hit.start);
        stamp.style.cssText = 'flex:none;font-size:11px;font-variant-numeric:tabular-nums;'
          + 'padding:2px 6px;border-radius:5px;background:var(--muted,#f3f3f3);'
          + 'color:var(--muted-foreground,#888)';

        var body = document.createElement('div');
        body.style.cssText = 'min-width:0;flex:1;font-size:13px;line-height:1.5';
        appendHighlighted(body, hit.text, needle);

        card.appendChild(stamp); card.appendChild(body);
        card.onclick = function () {
          if (seekBilibili(hit.start)) {
            subOverlay.style.display = 'none';
            log('seek to', hit.start);
          } else {
            status.textContent = '没找到 B 站播放器（iframe），无法跳转';
          }
        };
        list.appendChild(card);
      });
    }

    go.onclick = run;
    q.addEventListener('keydown', function (e) { if (e.key === 'Enter') { e.preventDefault(); run(); } });
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape' && subOverlay && subOverlay.style.display !== 'none') {
        subOverlay.style.display = 'none';
      }
    });
    return subOverlay;
  }

  // ---- AI 陪看（#2）----
  //
  // ★ 为什么走后端端点而不是直连 LM Studio：补丁跑在后端进程里，
  //   deeptutor.services.llm 就是前端所有 AI 功能的同一条链路 —— 用户在
  //   设置里配的模型/base_url/key 自动生效，换模型不用改补丁。
  // ★ 为什么同源 fetch 不用 IPC：与搜索同理（远端来源 ACL 全拒），
  //   详见上面 SEARCH_API 的注释。
  // ★ 位置哪来的：B 站 iframe 不往父窗口 postMessage 播放进度（2026-10-17
  //   headless 探针实测只发一条 REQUEST_PARENT_TITLE），前端自己也不追
  //   （tracksPosition:!1）—— 所以用 iframe src 的 t= 参数当「最近一次
  //   定位」的近似，并允许用户手填秒数。没有位置就走全文上下文。
  var AI_API = '/api/video-learning/bilibili';
  var AI_MATERIALS_API = '/api/video-learning/bilibili/materials/';
  var aiOverlay = null;
  var aiMode = 'ask';

  function recordedMaterialId() {
    var rec = window[SUB_RECORDER];
    var m = rec && rec.material;
    return (m && m.material_id) ? String(m.material_id) : '';
  }

  // B 站 iframe 的 t= 参数：React 只在 seek 时改它，我们从外部改的值也能
  // 活着 —— 这是不跨域拿到「最近定位」的最佳近似。
  function currentPlayerSeconds() {
    var frame = findBilibiliIframe();
    if (!frame) { return 0; }
    try {
      var n = Number(new URL(frame.src).searchParams.get('t'));
      return Number.isFinite(n) && n > 0 ? n : 0;
    } catch (e) { return 0; }
  }

  function aiPost(mid, payload) {
    return window.fetch(
      AI_MATERIALS_API + encodeURIComponent(mid) + '/companion',
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify(payload)
      }
    ).then(function (r) {
      return r.json().then(function (j) { return { ok: r.ok, body: j || {} }; },
                           function () { return { ok: false, body: { error: '响应不是 JSON' } }; });
    });
  }

  function aiStyleButton(text) {
    var b = document.createElement('button');
    b.type = 'button';
    b.textContent = text;
    b.style.cssText = 'height:34px;padding:0 14px;font-size:13px;border-radius:8px;cursor:pointer;'
      + 'border:1px solid var(--border,#e5e5e5);background:transparent;color:inherit';
    return b;
  }

  function ensureAiOverlay() {
    if (aiOverlay && document.body.contains(aiOverlay)) { return aiOverlay; }
    aiOverlay = document.createElement('div');
    aiOverlay.style.cssText = 'position:fixed;inset:0;z-index:2147483000;display:none;'
      + 'align-items:center;justify-content:center;background:rgba(0,0,0,.45)';

    var panel = document.createElement('div');
    panel.style.cssText = 'width:min(680px,92vw);max-height:82vh;display:flex;flex-direction:column;'
      + 'background:var(--card,#fff);color:var(--foreground,#111);border-radius:12px;'
      + 'box-shadow:0 18px 50px rgba(0,0,0,.28);overflow:hidden';

    var head = document.createElement('div');
    head.style.cssText = 'display:flex;align-items:center;gap:8px;padding:12px 14px;'
      + 'border-bottom:1px solid var(--border,#e5e5e5)';
    var title = document.createElement('div');
    title.textContent = 'AI 陪看';
    title.style.cssText = 'font-size:14px;font-weight:500;flex:1';
    var close = document.createElement('button');
    close.type = 'button';
    close.textContent = '关闭';
    close.style.cssText = 'font-size:12px;padding:4px 10px;border-radius:6px;cursor:pointer;'
      + 'border:1px solid var(--border,#e5e5e5);background:transparent;color:inherit';
    close.onclick = function () { aiOverlay.style.display = 'none'; };
    head.appendChild(title); head.appendChild(close);

    // 模式切换
    var tabs = document.createElement('div');
    tabs.style.cssText = 'display:flex;gap:8px;padding:10px 14px 0';
    var tabAsk = aiStyleButton('问一问');
    var tabSum = aiStyleButton('章节摘要');
    var tabExp = aiStyleButton('解释标记');
    tabs.appendChild(tabAsk); tabs.appendChild(tabSum); tabs.appendChild(tabExp);

    // 输入行
    var row = document.createElement('div');
    row.style.cssText = 'display:flex;gap:8px;padding:10px 14px 0';
    var q = document.createElement('input');
    q.type = 'text';
    q.placeholder = '问一个关于这个视频的问题，如「刚才这句什么意思」';
    q.style.cssText = 'flex:1;min-width:0;height:34px;padding:0 10px;font-size:13px;border-radius:8px;'
      + 'border:1px solid var(--border,#e5e5e5);background:var(--background,#fff);color:inherit';
    var go = document.createElement('button');
    go.type = 'button';
    go.textContent = '提问';
    go.style.cssText = 'height:34px;padding:0 16px;font-size:13px;border-radius:8px;cursor:pointer;'
      + 'border:0;background:var(--primary,#e9544b);color:var(--primary-foreground,#fff)';
    row.appendChild(q); row.appendChild(go);

    // 定位行：ask/explain 用；summary 不需要
    var posRow = document.createElement('div');
    posRow.style.cssText = 'display:flex;gap:8px;align-items:center;padding:8px 14px 0;font-size:12px;'
      + 'color:var(--muted-foreground,#888)';
    var posLabel = document.createElement('span');
    posLabel.textContent = '定位（秒）';
    var pos = document.createElement('input');
    pos.type = 'number';
    pos.min = '0';
    pos.placeholder = '0 = 全文';
    pos.style.cssText = 'width:110px;height:30px;padding:0 8px;font-size:12px;border-radius:8px;'
      + 'border:1px solid var(--border,#e5e5e5);background:var(--background,#fff);color:inherit';
    var usePos = aiStyleButton('用播放器位置');
    posRow.appendChild(posLabel); posRow.appendChild(pos); posRow.appendChild(usePos);

    var status = document.createElement('div');
    status.style.cssText = 'padding:10px 14px 4px;font-size:12px;color:var(--muted-foreground,#888);min-height:18px';

    // 标记列表（explain 模式）
    var marks = document.createElement('div');
    marks.style.cssText = 'display:none;flex:0 0 auto;max-height:22vh;overflow:auto;padding:4px 8px 0';

    var answer = document.createElement('div');
    answer.style.cssText = 'flex:1;overflow:auto;margin:8px 14px 14px;padding:12px;font-size:13px;'
      + 'line-height:1.65;white-space:pre-wrap;word-break:break-word;border-radius:8px;'
      + 'background:var(--muted,#f6f6f6);color:inherit';

    panel.appendChild(head); panel.appendChild(tabs); panel.appendChild(row);
    panel.appendChild(posRow); panel.appendChild(status); panel.appendChild(marks);
    panel.appendChild(answer);
    aiOverlay.appendChild(panel);
    document.body.appendChild(aiOverlay);
    aiOverlay.addEventListener('click', function (e) {
      if (e.target === aiOverlay) { aiOverlay.style.display = 'none'; }
    });

    function tabStyle(active) {
      return 'height:34px;padding:0 14px;font-size:13px;border-radius:8px;cursor:pointer;'
        + 'border:1px solid var(--border,#e5e5e5);background:transparent;color:inherit'
        + (active ? ';border-color:var(--primary,#e9544b);color:var(--primary,#e9544b)' : '');
    }

    function paintTabs() {
      tabAsk.style.cssText = tabStyle(aiMode === 'ask');
      tabSum.style.cssText = tabStyle(aiMode === 'summary');
      tabExp.style.cssText = tabStyle(aiMode === 'explain');
      q.style.display = aiMode === 'summary' ? 'none' : '';
      q.placeholder = aiMode === 'explain'
        ? '想重点解释什么（可留空，直接解释这一处）'
        : '问一个关于这个视频的问题，如「刚才这句什么意思」';
      go.textContent = aiMode === 'summary' ? '生成摘要' : (aiMode === 'explain' ? '解释这一处' : '提问');
      posRow.style.display = aiMode === 'summary' ? 'none' : 'flex';
      marks.style.display = aiMode === 'explain' ? 'block' : 'none';
    }

    function refreshStatus() {
      var cues = recordedCues().length;
      status.textContent = '字幕 ' + cues + ' 条 · 正在确认模型…';
      window.fetch(AI_API + '/companion/status', { credentials: 'same-origin', cache: 'no-store' })
        .then(function (r) { return r.json(); })
        .then(function (j) {
          if (j && j.available) {
            status.textContent = '字幕 ' + cues + ' 条 · 模型 ' + (j.model || '?');
          } else {
            status.textContent = '字幕 ' + cues + ' 条 · ' + ((j && j.error) || '模型不可用');
          }
        })
        .catch(function () { status.textContent = '字幕 ' + cues + ' 条 · 模型状态获取失败'; });
    }

    function loadMarks() {
      var mid = recordedMaterialId();
      marks.textContent = '';
      if (!mid) { return; }
      status.textContent = '正在加载标记…';
      window.fetch('/api/reading/materials/' + encodeURIComponent(mid) + '/annotations',
                   { credentials: 'same-origin', cache: 'no-store' })
        .then(function (r) { return r.json(); })
        .then(function (rows) {
          var list = Array.isArray(rows) ? rows : [];
          if (!list.length) {
            status.textContent = '这个视频还没有标记 —— 先在播放页打几条，再来解释';
            return;
          }
          status.textContent = '选一条标记来解释（共 ' + list.length + ' 条）';
          list.slice(0, 20).forEach(function (row) {
            var card = document.createElement('button');
            card.type = 'button';
            card.style.cssText = 'display:block;width:100%;text-align:left;padding:8px;margin-bottom:6px;'
              + 'border-radius:8px;border:1px solid var(--border,#e5e5e5);background:transparent;'
              + 'color:inherit;cursor:pointer';
            card.onmouseenter = function () { card.style.background = 'var(--muted,#f3f3f3)'; };
            card.onmouseleave = function () { card.style.background = 'transparent'; };
            var head1 = document.createElement('div');
            head1.style.cssText = 'font-size:12px;color:var(--muted-foreground,#888)';
            head1.textContent = (row.kind || 'highlight') + ' · 片段 ' + (row.locator || '?');
            var body = document.createElement('div');
            body.style.cssText = 'font-size:13px;line-height:1.5;margin-top:2px';
            body.textContent = String(row.note || row.quote || '').slice(0, 120);
            card.appendChild(head1); card.appendChild(body);
            card.onclick = function () {
              pos.value = '';
              ask('explain', String(row.note || ''), Number(row.locator) || 0);
            };
            marks.appendChild(card);
          });
        })
        .catch(function () { status.textContent = '标记加载失败，可改用「问一问」'; });
    }

    // context：额外语料（#4「AI 总结热评」把高赞评论拼进来）。留空时
    // 与以前完全一致 —— companion 的 ask 模式只在有 context 时才改 prompt。
    function ask(mode, question, locator, context) {
      var mid = recordedMaterialId();
      if (!mid) {
        status.textContent = '还没拿到视频材料 —— 请先打开这个视频的播放页，'
          + '等材料加载完（字幕检索能搜到内容时就够了）再试';
        return;
      }
      var payload = { mode: mode, question: question || '' };
      if (mode !== 'summary') {
        var seconds = Number(pos.value);
        payload.position = Number.isFinite(seconds) && seconds > 0 ? seconds : 0;
      }
      if (locator) { payload.locator = locator; }
      if (context) { payload.context = context; }
      go.disabled = true;
      status.textContent = mode === 'summary' ? '正在通读全片生成摘要…' : '正在思考…';
      answer.textContent = '';
      aiPost(mid, payload).then(function (res) {
        go.disabled = false;
        if (res.ok && res.body && res.body.answer) {
          answer.textContent = res.body.answer;
          var used = res.body.cues_used;
          status.textContent = '完成 · 参考了 ' + (used || 0) + ' 条字幕'
            + (res.body.elapsed_ms ? (' · ' + (res.body.elapsed_ms / 1000).toFixed(1) + 's') : '');
        } else {
          status.textContent = (res.body && res.body.error) || '请求失败';
        }
      }).catch(function (e) {
        go.disabled = false;
        status.textContent = '网络错误：' + (e && e.message ? e.message : 'unknown');
      });
    }

    go.onclick = function () {
      if (aiMode === 'summary') { ask('summary', '', 0); return; }
      if (aiMode === 'explain') { ask('explain', (q.value || '').trim(), 0); return; }
      var text = (q.value || '').trim();
      if (!text) { status.textContent = '先输入问题；或切到「章节摘要」一键看全片结构'; return; }
      ask('ask', text, 0);
    };
    q.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') { e.preventDefault(); go.onclick(); }
    });
    tabAsk.onclick = function () { aiMode = 'ask'; paintTabs(); refreshStatus(); };
    tabSum.onclick = function () { aiMode = 'summary'; paintTabs(); refreshStatus(); };
    tabExp.onclick = function () { aiMode = 'explain'; paintTabs(); refreshStatus(); loadMarks(); };
    usePos.onclick = function () {
      var s = currentPlayerSeconds();
      pos.value = s > 0 ? String(s) : '';
      status.textContent = s > 0
        ? ('已填入播放器当前定位 ' + fmtClock(s) + '（B 站不上报实时进度，这是最近一次定位）')
        : '没读到播放器定位，请手动填秒数';
    };
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape' && aiOverlay && aiOverlay.style.display !== 'none') {
        aiOverlay.style.display = 'none';
      }
    });

    paintTabs();
    aiOverlay.__dtAiRefresh = refreshStatus;
    // 热评面板用：带语料提问（答案仍显示在 AI 面板里）。
    aiOverlay.__dtAiAsk = ask;
    return aiOverlay;
  }

  // ---- 高能进度条 + 热评（#4）----
  //
  // ★ 高能条为什么贴 iframe 上沿：B 站 iframe 不上报进度、也不接受外部
  //   定位（只能改 src 的 t= 参数重载），进度条画进播放器内部不可能；
  //   贴在播放器上方既能表达「全片弹幕密度」，又不遮播放器任何控件
  //   （B 站控件条在底部，顶部 6px 盖的是画面，可接受）。
  //   点击跳转复用 seekBilibili（与前端自己的 seek 同机制）。
  // ★ 为什么要自己起 interval：本脚本每个页面上下文只跑一次
  //   （window.__dtBiliPanel 幂等标记），boot 里那一次不够 —— 用户从别的
  //   页面切回来、滚动页面都会让 iframe 移位。1s 轮询 + scroll/resize
  //   监听足够，开销只是一次 querySelectorAll + getBoundingClientRect。
  // ★ 失败必须**安静降级**：风控/网络失败时条置灰并在 title 里说明原因
  //   （后端文案带倒计时），30s 内不重打；真没弹幕直接收掉，绝不挡画面。
  var heatBar = null;
  var heatFill = null;
  var heatFor = '';
  var heatState = '';       // '' = 没试过 / 'ok' = 有数据 / 'empty' = 确认没弹幕（终端态）
  var heatFailedAt = -1e9;
  var HEAT_COOLDOWN = 30000;
  var hotOverlay = null;
  var hotRows = [];

  // 只收 DOM，不动 heatFor/heatState —— 状态机要能记住「这个视频试过」
  function removeHeatBar() {
    if (heatBar && heatBar.parentNode) { heatBar.parentNode.removeChild(heatBar); }
    heatBar = null;
    heatFill = null;
  }

  function ensureHeatBar() {
    var frame = findBilibiliIframe();
    if (!frame) { return; }
    var mid = recordedMaterialId();
    if (!mid) {
      // 换了视频但材料还没到：旧条留着会拿旧时长跳新视频，先收掉
      if (heatFor || heatBar) { removeHeatBar(); heatFor = ''; heatState = ''; }
      return;
    }
    if (heatFor === mid && heatState === 'empty') { return; }  // 确认没弹幕：不建条
    if (!heatBar || !document.body.contains(heatBar)) {
      heatBar = document.createElement('div');
      heatBar.title = '高能进度条 · 弹幕密度（点击跳转）';
      heatBar.style.cssText = 'position:fixed;z-index:2147482000;height:6px;'
        + 'border-radius:3px;background:var(--border,#e5e5e5);cursor:pointer;'
        + 'overflow:hidden;box-shadow:0 0 0 1px rgba(0,0,0,.06)';
      heatFill = document.createElement('div');
      heatFill.style.cssText = 'height:100%;width:100%';
      heatBar.appendChild(heatFill);
      heatBar.onclick = function (e) {
        var dur = Number(heatBar.getAttribute('data-duration')) || 0;
        if (dur <= 0) { return; }
        var rect = heatBar.getBoundingClientRect();
        var ratio = (e.clientX - rect.left) / Math.max(1, rect.width);
        var secs = Math.floor(Math.min(1, Math.max(0, ratio)) * dur);
        if (seekBilibili(secs)) {
          heatBar.title = '高能进度条 · 已跳到 ' + fmtClock(secs);
        }
      };
      document.body.appendChild(heatBar);
    }
    var rect = frame.getBoundingClientRect();
    if (rect.width < 40 || rect.height < 40 || rect.bottom < 0 || rect.top > window.innerHeight) {
      heatBar.style.display = 'none';  // 播放器滚出视口：不收条（位置还会回来），只隐藏
      return;
    }
    heatBar.style.display = 'block';
    heatBar.style.left = Math.round(rect.left) + 'px';
    heatBar.style.top = Math.round(Math.max(2, rect.top - 9)) + 'px';
    heatBar.style.width = Math.round(rect.width) + 'px';
    // 状态机：ok/empty 是终端态不再打接口；失败（state='' 且刚失败过）
    // 走 30s 冷却 —— 防每轮轮询都重打，风控期间重试只会延长封禁
    if (heatFor === mid) {
      if (heatState === 'ok' || heatState === 'empty') { return; }
      if (Date.now() - heatFailedAt < HEAT_COOLDOWN) { return; }
    }
    heatFor = mid;
    loadHeat(mid);
  }

  function loadHeat(mid) {
    if (!heatBar) { return; }
    heatState = '';
    heatFill.style.background = '';
    heatBar.removeAttribute('data-duration');
    heatBar.title = '高能进度条 · 正在读取弹幕密度…';
    window.fetch(AI_MATERIALS_API + encodeURIComponent(mid) + '/heat',
                 { credentials: 'same-origin', cache: 'no-store' })
      .then(function (r) { return r.json(); })
      .then(function (j) {
        if (!heatBar || heatFor !== mid) { return; }
        var pts = (j && j.points) || [];
        var max = Number(j && j.max) || 0;
        var dur = Number(j && j.duration) || 0;
        if (j && j.error) {
          // 风控/网络失败：条留着但置灰，title 说清原因（后端文案带倒计时）
          heatState = '';
          heatFailedAt = Date.now();
          heatBar.title = '高能进度条 · ' + j.error;
          return;
        }
        if (!pts.length || max <= 0 || dur <= 0) {
          heatState = 'empty';  // 终端态：确认没弹幕，不再打接口
          removeHeatBar();
          return;
        }
        heatState = 'ok';
        heatBar.setAttribute('data-duration', String(dur));
        // 一条 linear-gradient 画完整条密度（240 格 = 480 个 color stop，
        // 单节点、无 240 个子 div，DOM 干净）
        var stops = [];
        for (var i = 0; i < pts.length; i++) {
          var a = (0.22 + 0.78 * (pts[i] / max)).toFixed(2);
          var p0 = (i / pts.length * 100).toFixed(2);
          var p1 = ((i + 1) / pts.length * 100).toFixed(2);
          stops.push('rgba(251,114,153,' + a + ') ' + p0 + '%');
          stops.push('rgba(251,114,153,' + a + ') ' + p1 + '%');
        }
        heatFill.style.background = 'linear-gradient(to right,' + stops.join(',') + ')';
        heatBar.title = '高能进度条 · 共 ' + (j.total || 0) + ' 条弹幕（点击跳转）';
      })
      .catch(function () {
        if (heatFor === mid) {
          heatState = '';
          heatFailedAt = Date.now();
        }
        removeHeatBar();
      });
  }

  // ---- 热评面板（#4）----
  function ensureHotOverlay() {
    if (hotOverlay && document.body.contains(hotOverlay)) { return hotOverlay; }
    hotOverlay = document.createElement('div');
    hotOverlay.style.cssText = 'position:fixed;inset:0;z-index:2147483000;display:none;'
      + 'align-items:center;justify-content:center;background:rgba(0,0,0,.45)';

    var panel = document.createElement('div');
    panel.style.cssText = 'width:min(680px,92vw);max-height:82vh;display:flex;flex-direction:column;'
      + 'background:var(--card,#fff);color:var(--foreground,#111);border-radius:12px;'
      + 'box-shadow:0 18px 50px rgba(0,0,0,.28);overflow:hidden';

    var head = document.createElement('div');
    head.style.cssText = 'display:flex;align-items:center;gap:8px;padding:12px 14px;'
      + 'border-bottom:1px solid var(--border,#e5e5e5)';
    var title = document.createElement('div');
    title.textContent = '热评';
    title.style.cssText = 'font-size:14px;font-weight:500;flex:1';
    var close = document.createElement('button');
    close.type = 'button';
    close.textContent = '关闭';
    close.style.cssText = 'font-size:12px;padding:4px 10px;border-radius:6px;cursor:pointer;'
      + 'border:1px solid var(--border,#e5e5e5);background:transparent;color:inherit';
    close.onclick = function () { hotOverlay.style.display = 'none'; };
    head.appendChild(title); head.appendChild(close);

    // 顶栏：把高赞评论当语料喂给 AI（#4「评论区高赞作语料」）
    var row = document.createElement('div');
    row.style.cssText = 'display:flex;gap:8px;align-items:center;padding:12px 14px 0';
    var askAi = aiStyleButton('AI 总结热评');
    askAi.onclick = aiSummarizeHot;
    var hint = document.createElement('span');
    hint.style.cssText = 'font-size:12px;color:var(--muted-foreground,#888)';
    hint.textContent = '让 AI 结合字幕和高赞评论，总结观众在讨论什么';
    row.appendChild(askAi); row.appendChild(hint);

    var status = document.createElement('div');
    status.style.cssText = 'padding:10px 14px 4px;font-size:12px;color:var(--muted-foreground,#888);min-height:18px';
    var list = document.createElement('div');
    list.style.cssText = 'flex:1;overflow:auto;padding:0 8px 12px';

    panel.appendChild(head); panel.appendChild(row); panel.appendChild(status); panel.appendChild(list);
    hotOverlay.appendChild(panel);
    document.body.appendChild(hotOverlay);
    hotOverlay.addEventListener('click', function (e) {
      if (e.target === hotOverlay) { hotOverlay.style.display = 'none'; }
    });

    function refresh() {
      var mid = recordedMaterialId();
      hotRows = [];
      list.textContent = '';
      if (!mid) {
        status.textContent = '还没拿到视频材料 —— 先打开这个视频的播放页再试';
        return;
      }
      status.textContent = '正在读取热评…';
      window.fetch(AI_MATERIALS_API + encodeURIComponent(mid) + '/hot-comments',
                   { credentials: 'same-origin', cache: 'no-store' })
        .then(function (r) { return r.json(); })
        .then(function (j) {
          var rows = (j && j.comments) || [];
          if (!rows.length) {
            status.textContent = (j && j.error) || '这个视频还没有热门评论';
            return;
          }
          hotRows = rows;
          status.textContent = '高赞 ' + rows.length + ' 条 · 按点赞排序';
          list.textContent = '';
          rows.forEach(function (item) {
            var card = document.createElement('div');
            card.style.cssText = 'padding:8px;margin-bottom:6px;border-radius:8px;'
              + 'border:1px solid var(--border,#e5e5e5)';
            var meta = document.createElement('div');
            meta.style.cssText = 'font-size:12px;color:var(--muted-foreground,#888)';
            meta.textContent = item.user + ' · ' + (item.like || 0) + ' 赞'
              + (item.replies ? (' · ' + item.replies + ' 条回复') : '');
            var body = document.createElement('div');
            body.style.cssText = 'font-size:13px;line-height:1.5;margin-top:2px;word-break:break-word';
            body.textContent = String(item.text || '');
            card.appendChild(meta); card.appendChild(body);
            list.appendChild(card);
          });
        })
        .catch(function () { status.textContent = '热评读取失败（不影响播放）'; });
    }

    hotOverlay.__dtHotRefresh = refresh;
    return hotOverlay;
  }

  // 把已加载的热评拼成语料，借 AI 面板提问（答案显示在 AI 面板里）
  function aiSummarizeHot() {
    if (!hotRows.length) { return; }
    var lines = [];
    for (var i = 0; i < hotRows.length; i++) {
      lines.push(hotRows[i].user + '（' + (hotRows[i].like || 0) + ' 赞）：' + hotRows[i].text);
    }
    var ov = ensureAiOverlay();
    ov.style.display = 'flex';
    if (ov.__dtAiAsk) {
      ov.__dtAiAsk('ask', '结合下面的热门评论，总结观众在讨论什么、提到哪些知识点', 0,
                   lines.join('\n'));
    }
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

    // 字幕检索按钮（#3）：只在有字幕数据的页面才有意义，但装不装都无害 ——
    // 没数据时面板会给明确提示，不会静默失败。
    var sub = document.createElement('button');
    sub.type = 'button';
    sub.textContent = '字幕检索';
    sub.style.cssText = 'margin-left:8px;height:34px;padding:0 12px;font-size:13px;cursor:pointer;'
      + 'border-radius:8px;border:1px solid var(--border,#e5e5e5);'
      + 'background:transparent;color:var(--foreground,inherit)';
    sub.onclick = function () {
      var ov = ensureSubOverlay();
      ov.style.display = 'flex';
      var q = ov.querySelector('input');
      if (q) { q.focus(); }
    };
    (btn.parentElement || btn).insertBefore(sub, btn.nextSibling);

    // AI 陪看按钮（#2）：问一问 / 章节摘要 / 解释标记。没字幕数据时面板
    // 会给明确提示，不会静默失败。
    var ai = document.createElement('button');
    ai.type = 'button';
    ai.textContent = 'AI 陪看';
    ai.style.cssText = 'margin-left:8px;height:34px;padding:0 12px;font-size:13px;cursor:pointer;'
      + 'border-radius:8px;border:1px solid var(--border,#e5e5e5);'
      + 'background:transparent;color:var(--foreground,inherit)';
    ai.onclick = function () {
      var ov = ensureAiOverlay();
      ov.style.display = 'flex';
      if (ov.__dtAiRefresh) { ov.__dtAiRefresh(); }
      var box = ov.querySelector('input');
      if (box) { box.focus(); }
    };
    (sub.parentElement || sub).insertBefore(ai, sub.nextSibling);

    // 热评按钮（#4）：评论区高赞 + 「AI 总结热评」语料入口。取不到时
    // 面板给明确提示，不影响播放。
    var hot = document.createElement('button');
    hot.type = 'button';
    hot.textContent = '热评';
    hot.style.cssText = 'margin-left:8px;height:34px;padding:0 12px;font-size:13px;cursor:pointer;'
      + 'border-radius:8px;border:1px solid var(--border,#e5e5e5);'
      + 'background:transparent;color:var(--foreground,inherit)';
    hot.onclick = function () {
      var ov = ensureHotOverlay();
      ov.style.display = 'flex';
      if (ov.__dtHotRefresh) { ov.__dtHotRefresh(); }
    };
    (ai.parentElement || ai).insertBefore(hot, ai.nextSibling);

    log('button installed');
    return true;
  }

  // ---- 盯 DOM：SPA 切页不会触发 on_page_load，只能自己观察 ----
  function boot() {
    installRecorder();
    // 高能进度条（#4）：立即跑一次 + 1s 轮询 + 滚动/缩放重定位。
    // 本脚本每页面上下文只跑一次，轮询是唯一的「持续跟踪」手段。
    try {
      ensureHeatBar();
      window.setInterval(ensureHeatBar, 1000);
      window.addEventListener('scroll', ensureHeatBar, true);
      window.addEventListener('resize', ensureHeatBar);
    } catch (e) { log('heat bar boot failed: ' + e); }
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

/// ★ 搜索必须**优先走同源 fetch**，不能把 Tauri IPC 当主路。
///
/// 主窗口在后端就绪后会 `location.replace` 到 `http://127.0.0.1:<web_port>`，
/// 对 Tauri 而言变成**远程来源**；而 `capabilities/main.json` 没有 `remote.urls`
/// 声明，于是每条 IPC 命令都被 ACL 拒 —— v0.2.13 实测报
/// `Command bili_search not allowed by ACL`（面板能弹、搜索必失败）。
#[test]
    fn search_uses_same_origin_fetch_first() {
        assert!(
            PANEL_JS.contains("/api/video-learning/bilibili/search"),
            "面板应直接打后端已挂的同源端点"
        );
        assert!(
            PANEL_JS.contains("credentials: 'same-origin'"),
            "同源 fetch 要显式带 cookie，否则鉴权失败"
        );
        let primary = PANEL_JS
            .find("return fetch(url")
            .expect("search() 应以 fetch 为返回主路");
        assert!(
            !PANEL_JS.contains("invokeTauri"),
            "★ 不要在fetch 失败时回落 Tauri IPC —— 主窗口是远程来源，IPC 必然被\
             ACL 拒，回落只会把真实错误（如 404 补丁未启用）替换成 \
             `not allowed by ACL`，排查成本极高（v0.2.14 实测踩过）"
        );
        assert!(
            PANEL_JS[primary..].contains("r.status === 404"),
            "404 要单独处理并给出可执行提示，而不是笼统的 HTTP 错误"
        );
    }

    /// 面板已完全不走 IPC，但仍要记录一条事实：主窗口的 windows 列表里
    /// 有 `"main"`（托盘的搜索窗口复用同一份 capability）。
    #[test]
    fn main_window_is_listed_in_capability() {
        let caps = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities/main.json");
        let text = std::fs::read_to_string(&caps).expect("capabilities/main.json 应存在");
        assert!(
            text.contains("\"main\""),
            "主窗口不在 capability 的 windows 里"
        );
    }

    /// ★ #3 字幕检索：数据必须来自「包装 fetch 记录页面自己拉的材料 JSON」。
    ///
    /// 不猜后端端点（1.6.14 起前端走 reading 动态拼路径，猜不齐）；也不轮询
    /// DOM（字幕列表虚拟滚动，DOM 里只有可见几条）。
    #[test]
    fn subtitle_search_records_page_material() {
        assert!(
            PANEL_JS.contains("__dtBiliSubRecorder"),
            "缺字幕数据记录标记，重复注入会重复包装 fetch"
        );
        assert!(
            PANEL_JS.contains("window.fetch = function"),
            "必须包装 fetch 抓页面已拉取的材料 JSON"
        );
        assert!(
            PANEL_JS.contains("resp.clone().json()"),
            "要克隆响应读 JSON，不能消费原响应流"
        );
    }

    /// 跳转必须走「改 iframe src 的 t 参数」—— 与前端自己的 seek 同机制
    /// （chunk 1241 里 `t:String(Math.floor(c))`），不要去找播放器 SDK。
    #[test]
    fn subtitle_seek_rewrites_t_param() {
        assert!(
            PANEL_JS.contains("player.bilibili.com/player.html"),
            "要定位 B 站播放器 iframe"
        );
        assert!(
            PANEL_JS.contains("searchParams.set('t'"),
            "跳转靠改 t 参数让 iframe 重载到那一秒"
        );
    }

    /// 字幕检索入口按钮要装上；没字幕数据时面板给明确提示，不静默失败。
    #[test]
    fn subtitle_entry_button_and_hint_exist() {
        assert!(PANEL_JS.contains("字幕检索"), "缺字幕检索入口按钮");
        assert!(
            PANEL_JS.contains("还没载入字幕数据"),
            "没有数据时必须给可执行的提示，不能静默无结果"
        );
    }

    /// 命中高亮分段必须走 textContent / createTextNode —— 字幕文本来自
    /// B 站，等同用户数据，拼 innerHTML 既是注入面也违反 CSP 惯例。
    #[test]
    fn subtitle_highlight_avoids_inner_html() {
        assert!(PANEL_JS.contains("appendHighlighted"), "缺高亮分段函数");
        assert!(PANEL_JS.contains("createTextNode"), "高亮要用文本节点拼接");
    }

    /// ★ #2 AI 陪看：必须打后端已挂的 companion 端点（同源 fetch），复用
    /// 用户配置的 LLM（LM Studio 等）—— 不自己直连模型服务，换模型不用改补丁。
    #[test]
    fn ai_companion_uses_backend_endpoint() {
        assert!(
            PANEL_JS.contains("/api/video-learning/bilibili/materials/"),
            "陪看必须打后端 companion 端点"
        );
        assert!(
            PANEL_JS.contains("/companion/status"),
            "打开面板要先确认模型是否可用"
        );
        assert!(
            PANEL_JS.contains("credentials: 'same-origin'"),
            "同源 fetch 要显式带 cookie，否则鉴权失败"
        );
    }

    /// 三种模式都要有：问一问 / 章节摘要 / 解释标记。
    #[test]
    fn ai_companion_has_three_modes() {
        for token in ["问一问", "章节摘要", "解释标记", "'ask'", "'summary'", "'explain'"] {
            assert!(PANEL_JS.contains(token), "陪看缺少模式 {token}");
        }
    }

    /// 位置来源：B 站 iframe 不上报实时进度（2026-10-17 headless 探针实测
    /// 只发一条 REQUEST_PARENT_TITLE），只能用 t= 参数近似 + 允许手填；
    /// 没有位置走后端全文上下文。
    #[test]
    fn ai_companion_position_is_best_effort() {
        assert!(
            PANEL_JS.contains("currentPlayerSeconds"),
            "要能从 iframe t= 参数读最近定位"
        );
        assert!(
            PANEL_JS.contains("用播放器位置"),
            "要有一键填入播放器位置的按钮"
        );
        assert!(
            PANEL_JS.contains("Number.isFinite(seconds)"),
            "position 要校验，非数字不能发给后端"
        );
    }

    /// 解释标记模式要读 reading annotations（1.6.14 播放页标记的真身，
    /// 不再是 video_learning 的 marks）。
    #[test]
    fn ai_companion_explain_reads_annotations() {
        assert!(
            PANEL_JS.contains("/api/reading/materials/"),
            "解释标记要从 reading annotations 取列表"
        );
        assert!(
            PANEL_JS.contains("/annotations"),
            "annotations 端点路径要对"
        );
    }

    /// 陪看入口按钮 + 无材料/无模型时的可执行提示。
    #[test]
    fn ai_companion_entry_and_hints_exist() {
        assert!(PANEL_JS.contains("AI 陪看"), "缺 AI 陪看入口按钮");
        assert!(
            PANEL_JS.contains("还没拿到视频材料"),
            "没材料时要引导先打开播放页"
        );
        assert!(
            PANEL_JS.contains("模型不可用") || PANEL_JS.contains("模型状态获取失败"),
            "模型不可用时要明说"
        );
    }

    /// 回答渲染必须走 textContent（模型输出等同不可信内容），并保留换行。
    #[test]
    fn ai_companion_answer_uses_text_content() {
        assert!(
            PANEL_JS.contains("answer.textContent = res.body.answer"),
            "回答必须走 textContent"
        );
        assert!(
            PANEL_JS.contains("white-space:pre-wrap"),
            "回答区要保留换行"
        );
    }

    /// ★ #4 高能进度条：必须打后端 /heat 端点（同源 fetch），弹幕密度由
    /// 后端算好（list.so XML → 每秒密度点串），前端只画条。
    #[test]
    fn heat_bar_uses_backend_endpoint() {
        assert!(
            PANEL_JS.contains("'/heat'"),
            "高能条要打后端 /heat 端点"
        );
        assert!(
            PANEL_JS.contains("'/hot-comments'"),
            "热评要打后端 /hot-comments 端点"
        );
        assert!(
            PANEL_JS.contains("credentials: 'same-origin'"),
            "同源 fetch 要显式带 cookie，否则鉴权失败"
        );
    }

    /// 高能条是装饰性功能：失败必须安静降级 —— 风控失败置灰并在 title
    /// 说明原因（后端文案带倒计时），真没弹幕直接收掉，绝不挡画面。
    #[test]
    fn heat_bar_degrades_silently() {
        assert!(
            PANEL_JS.contains("function removeHeatBar"),
            "要能把条收掉（降级路径）"
        );
        assert!(
            PANEL_JS.contains("if (j && j.error)"),
            "后端报错（风控/网络）要识别并置灰说明，不能当没弹幕"
        );
        assert!(
            PANEL_JS.contains("heatState = 'empty'"),
            "零弹幕要是终端态，不能每轮轮询都重打接口"
        );
        assert!(
            PANEL_JS.contains("heatFailedAt"),
            "失败后要冷却，不能每轮轮询都重打接口"
        );
    }

    /// 点条跳转必须换算全片时长并复用 seekBilibili（改 t= 参数），
    /// 与前端自己的 seek 同机制。
    #[test]
    fn heat_bar_click_seeks() {
        assert!(
            PANEL_JS.contains("if (seekBilibili(secs))"),
            "点高能条要复用 seekBilibili 跳转"
        );
        assert!(
            PANEL_JS.contains("getAttribute('data-duration')"),
            "跳转要按全片时长把点击位置换算成秒数"
        );
        assert!(
            PANEL_JS.contains("function findBilibiliIframe"),
            "iframe 查找要收敛成共用函数（seek/位置/高能条复用）"
        );
    }

    /// 高能条贴在 iframe 上沿（不遮播放器控件），且要持续跟踪 iframe
    /// 位置 —— 本脚本每页面上下文只跑一次，必须轮询。
    #[test]
    fn heat_bar_tracks_iframe_position() {
        assert!(
            PANEL_JS.contains("rect.top - 9"),
            "条要贴在 iframe 上沿而不是盖住播放器"
        );
        assert!(
            PANEL_JS.contains("window.setInterval(ensureHeatBar, 1000)"),
            "要 1s 轮询重定位（一次性注入不够：SPA 切页/滚动都会移位）"
        );
        assert!(
            PANEL_JS.contains("addEventListener('scroll', ensureHeatBar, true)"),
            "滚动要即时重定位（捕获阶段才能听到容器滚动）"
        );
    }

    /// 热评面板 + 「AI 总结热评」：高赞评论当语料进 companion 的 context
    /// 字段，答案仍显示在 AI 面板（textContent 渲染）。
    #[test]
    fn hot_comments_feed_ai_as_context() {
        assert!(PANEL_JS.contains("热评"), "缺热评入口按钮");
        assert!(PANEL_JS.contains("AI 总结热评"), "缺热评喂 AI 的入口");
        assert!(
            PANEL_JS.contains("aiOverlay.__dtAiAsk = ask"),
            "AI 面板要暴露带语料提问的入口"
        );
        assert!(
            PANEL_JS.contains("payload.context = context"),
            "热评语料要进 payload.context 字段"
        );
    }
}
