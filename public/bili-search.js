/* B 站搜索窗口的全部前端逻辑。
 *
 * ★ 三个真机踩出来的坑，改这文件前先读：
 *
 * 1. **本文件必须外链**。`tauri.conf.json` 的 CSP 是 `script-src 'self'`，
 *    内联 `<script>` 会被 WebView2 直接拦掉 —— 症状是「窗口开了、按钮没反应」，
 *    控制台还没几条有用的报错。同理样式也外链。
 *
 * 2. **缩略图是协议相对 URL**（`//i0.hdslb.com/bfs/archive/...`）。
 *    直接丢进 `img.src` 会被当成页面相对路径；而 `http://` 封面会被 WebView2
 *    当混合内容拦掉。两种都要归一到 `https://`。
 *
 * 3. **用户数据一律 `textContent`**。B 站标题里可以有 `<` `>` `&`（还有
 *    `<em class="keyword">` 高亮残留），拼进 `innerHTML` 既会破版也是注入面。
 *
 * 通信走 `window.__TAURI_INTERNALS__.invoke` —— 本项目 `withGlobalTauri` 没开，
 * 便捷的 `window.__TAURI__` 不存在。
 */
;(function () {
  'use strict'

  // ---- 与 Rust 侧的唯一通道 --------------------------------------------
  var internals = window.__TAURI_INTERNALS__
  if (!internals || typeof internals.invoke !== 'function') {
    document.getElementById('status').textContent =
      '无法与主程序通信（Tauri IPC 不可用）。请完全退出后重新启动一次。'
    return
  }
  var invoke = function (cmd, args) { return internals.invoke(cmd, args) }

  var $ = function (id) { return document.getElementById(id) }
  var input = $('q')
  var btnGo = $('go')
  var btnPrev = $('prev')
  var btnNext = $('next')
  var pageinfo = $('pageinfo')
  var status = $('status')
  var list = $('list')

  // ---- 状态 -------------------------------------------------------------
  var state = {
    keyword: '',   // 当前已搜索（或正在搜索）的关键词
    page: 1,
    busy: false,
    hasMore: false,
    total: 0
  }

  // 单页条数上限，和补丁包 search.py 的 MAX_RESULTS 一致。
  // 只有在后端没给出 `has_more` 时才用它兜底判断「还有下一页」。
  var PAGE_SIZE = 20

  // ---- 小工具 -----------------------------------------------------------
  function errMsg(e) {
    if (e == null) return '未知错误'
    if (typeof e === 'string') return e
    if (e.message) return String(e.message)
    try { return JSON.stringify(e) } catch (_) { return String(e) }
  }

  function fmtDuration(sec) {
    sec = Math.max(0, Math.round(Number(sec) || 0))
    if (!sec) return ''
    var h = Math.floor(sec / 3600)
    var m = Math.floor((sec % 3600) / 60)
    var s = sec % 60
    var pad = function (n) { return n < 10 ? '0' + n : String(n) }
    return h > 0 ? h + ':' + pad(m) + ':' + pad(s) : m + ':' + pad(s)
  }

  function fmtCount(n) {
    n = Number(n) || 0
    if (n <= 0) return ''
    if (n >= 1e8) return (n / 1e8).toFixed(1).replace(/\.0$/, '') + '亿'
    if (n >= 1e4) return (n / 1e4).toFixed(1).replace(/\.0$/, '') + '万'
    return String(Math.round(n))
  }

  function fmtDate(sec) {
    var t = Number(sec) || 0
    if (t <= 0) return ''
    var d = new Date(t * 1000)
    if (isNaN(d.getTime())) return ''
    return d.getFullYear() + '-' + ('0' + (d.getMonth() + 1)).slice(-2) + '-' + ('0' + d.getDate()).slice(-2)
  }

  // ★ 见文件头注释 2：协议相对 / http 封面都要归一到 https。
  function normalizeThumb(raw) {
    var url = String(raw || '').trim()
    if (!url) return ''
    if (url.indexOf('//') === 0) return 'https:' + url
    if (url.indexOf('http://') === 0) return 'https://' + url.slice(7)
    if (url.indexOf('https://') === 0) return url
    return ''
  }

  function el(tag, className, text) {
    var node = document.createElement(tag)
    if (className) node.className = className
    if (text) node.textContent = text
    return node
  }

  function setStatus(text, opts) {
    status.textContent = ''
    if (!text) return
    if (opts && opts.retry) {
      status.appendChild(document.createTextNode(text + ' '))
      var again = el('button', 'retry', '重试')
      again.type = 'button'
      again.onclick = function () { run(true) }
      status.appendChild(again)
    } else {
      status.textContent = text
    }
  }

  function syncPager() {
    btnPrev.disabled = state.busy || state.page <= 1
    btnNext.disabled = state.busy || !state.hasMore
    if (!state.keyword) {
      pageinfo.textContent = ''
      return
    }
    var bits = ['第 ' + state.page + ' 页']
    if (state.total > 0) bits.push('约 ' + fmtCount(state.total) + ' 条结果')
    pageinfo.textContent = bits.join(' · ')
  }

  // ---- 渲染 -------------------------------------------------------------
  function renderItem(item) {
    var card = el('div', 'item')
    card.setAttribute('role', 'button')
    card.tabIndex = 0

    // 缩略图
    var box = el('div', 'thumb')
    var thumb = normalizeThumb(item.thumbnail_url)
    if (thumb) {
      var img = document.createElement('img')
      img.alt = ''
      img.loading = 'lazy'
      // 不带 referer：图床偶尔按 referer 判防盗链，no-referrer 最稳。
      img.setAttribute('referrerpolicy', 'no-referrer')
      img.onerror = function () {
        box.classList.add('missing')
        if (img.parentNode) img.parentNode.removeChild(img)
      }
      img.src = thumb
      box.appendChild(img)
    } else {
      box.classList.add('missing')
    }
    card.appendChild(box)

    var meta = el('div', 'meta')
    meta.appendChild(el('div', 'title', item.title || item.bvid || '(无标题)'))

    var sub = el('div', 'sub')
    if (item.author) sub.appendChild(el('span', 'author', item.author))
    var dur = fmtDuration(item.duration_seconds)
    if (dur) sub.appendChild(el('span', 'dur', dur))
    var plays = fmtCount(item.play_count)
    if (plays) sub.appendChild(el('span', 'play', plays + '播放'))
    var when = fmtDate(item.published_at)
    if (when) sub.appendChild(el('span', 'date', when))
    if (sub.childNodes.length) meta.appendChild(sub)

    var desc = String(item.description || '').trim()
    if (desc) meta.appendChild(el('div', 'desc', desc))

    card.appendChild(meta)

    var open = function () { openVideo(item) }
    card.addEventListener('click', open)
    card.addEventListener('keydown', function (e) {
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); open() }
    })
    return card
  }

  // ---- 动作 -------------------------------------------------------------
  function openVideo(item) {
    var url = String(item.url || '')
    if (!url && item.bvid) url = 'https://www.bilibili.com/video/' + item.bvid + '/'
    if (!url) return
    setStatus('正在用默认浏览器打开…')
    invoke('bili_open', { url: url }).then(
      function () {
        setStatus('已在浏览器打开。把那个链接粘到 DeepTutor 的「边看边学」里即可。')
      },
      function (e) { setStatus('打开失败：' + errMsg(e)) }
    )
  }

  function run(force) {
    if (state.busy) return
    var q = input.value.trim()
    if (!q) {
      input.focus()
      setStatus('请输入关键词，比如「线性代数」。')
      return
    }
    if (!force && q !== state.keyword) {
      state.keyword = q
      state.page = 1
    } else {
      state.keyword = q
    }

    state.busy = true
    state.hasMore = false
    btnGo.disabled = true
    syncPager()
    list.textContent = ''
    setStatus('正在搜索「' + state.keyword + '」第 ' + state.page + ' 页…')

    invoke('bili_search', { keyword: state.keyword, page: state.page }).then(
      function (r) {
        var items = (r && r.results) || []
        var err = (r && r.error) || ''
        if (!items.length) {
          state.hasMore = false
          setStatus(err ? ('搜索失败：' + err) : ('没有找到与「' + state.keyword + '」相关的视频。'),
            { retry: !!err })
          return
        }
        state.total = Number(r.total_results) || 0
        // 后端给了就用后端的；没给（老补丁）就按「整页即可能还有下一页」兜底。
        state.hasMore = (typeof r.has_more === 'boolean')
          ? r.has_more
          : items.length >= PAGE_SIZE

        var frag = document.createDocumentFragment()
        for (var i = 0; i < items.length; i++) frag.appendChild(renderItem(items[i]))
        list.appendChild(frag)

        var msg = '第 ' + state.page + ' 页 · 本页 ' + items.length + ' 条'
        setStatus(err ? (msg + '（' + err + '）') : msg)
      },
      function (e) {
        state.hasMore = false
        setStatus('搜索出错：' + errMsg(e), { retry: true })
      }
    ).then(function () {
      state.busy = false
      btnGo.disabled = false
      syncPager()
      try { input.focus() } catch (_) {}
    })
  }

  // ---- 事件绑定 ---------------------------------------------------------
  btnGo.addEventListener('click', function () { run(false) })
  input.addEventListener('keydown', function (e) {
    if (e.key === 'Enter') { e.preventDefault(); run(false) }
  })
  btnPrev.addEventListener('click', function () {
    if (state.page > 1 && !state.busy) { state.page--; run(true) }
  })
  btnNext.addEventListener('click', function () {
    if (!state.busy && state.hasMore) { state.page++; run(true) }
  })

  // Esc 关窗。Tauri 的窗口插件命令名是 `plugin:window|close`（权限
  // `core:window:allow-close`，已在 capabilities 里给本窗口开了）。
  // 万一 IPC 不给关，退化成 window.close()，再不行还有系统标题栏的 ✕。
  document.addEventListener('keydown', function (e) {
    if (e.key !== 'Escape') return
    e.preventDefault()
    invoke('plugin:window|close').catch(function () {
      try { window.close() } catch (_) {}
    })
  })

  syncPager()
  input.focus()
})()
