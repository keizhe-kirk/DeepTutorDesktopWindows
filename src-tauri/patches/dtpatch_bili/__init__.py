"""B 站沉浸式观看支持 —— 注入 ``deeptutor.video_learning.service``。

# 前置结论（实测，勿轻易推翻）

- **前端已就绪**：``deeptutor_web`` 的 chunk 里已有完整 B 站渲染分支
  （BV 正则、``b23.tv`` 短链、分 P、``player.bilibili.com`` iframe、时间轴）。
  只要后端把 ``source.provider`` 标成 ``bilibili``、``source.url`` 给规范
  BV 链接，前端就会走那条分支 —— **不需要改任何前端产物**。
- **后端已就绪**：``reading.ingestion._load_bilibili_media`` 拿得到
  元数据 / 字幕 / 章节（纯 httpx，API 目的地固定，字幕只允许 ``*.hdslb.com``）。
- **唯一缺口**：``video_learning`` 的 provider 只有 youtube / invidious，
  ``parse_youtube_url`` 遇到 B 站链接直接抛错。

所以这里只做一件事：给 ``resolve_material`` 加一条B 站分支。

# 字幕需要登录态

实测12 个知识/教学区视频，9 个 ``need_login_subtitle=true``，无cookie 时
``subtitles`` 为空数组；``view_points``（章节）同样为空。也就是说**无登录态
时 B 站只能播放，不能「边看边学」**。

登录态从 ``bili_credentials.json`` 读（用户扫码产生，见 ``credentials.py``）。
读不到就退回「纯播放」并在 reason 里说明 —— 降级而非报错。

# 不做的事

- 不碰 ``youtube`` / ``invidious`` 任何路径（原函数保持引用，只在外层分流）。
- 不下发 ``playback``：前端 ``payload.pop("playback")`` 自己拼 B 站播放器。
"""

from __future__ import annotations

import asyncio
import os
from pathlib import Path
from typing import Any

# ★ 字幕是自己抓的，不用上游那条路 —— 原因见 `_fetch_bilibili_media` 的文档。
# 保留这两个名字是为了复用上游已经写好的「分段归一化」逻辑，避免重复实现。
from deeptutor.reading.ingestion import (  # noqa: E402
    MAX_TRANSCRIPT_BYTES,
    BilibiliMedia,
    build_transcript_segments,
    normalize_transcript_segments,
    parse_bilibili_url,
)

PROVIDER = "bilibili"

#: 字幕来源标识，写进 ``transcript.source``，前端据此提示用户。
#: 字幕来源标识，同时写进 ``transcript.source`` 和顶层 ``extractor``。
#: ★ 取值必须是**连字符**：前端 chunk 8771 就是这么判的
#:   ``["youtube-no-captions","bilibili-no-subtitles","bilibili-chapters-only"]``，
#:   写成下划线会让「没有字幕」提示永远不触发（实测踩过）。
SOURCE_SUBTITLES = "bilibili-subtitles"
SOURCE_CHAPTERS_ONLY = "bilibili-chapters-only"
SOURCE_NONE = "bilibili-no-subtitles"
SOURCE_DISABLED = "disabled"

_MAX_TITLE = 200


def _log(msg: str, *args: object) -> None:
    """记一行日志到 **stderr**。

    ★ 必须走 stderr：Rust 侧把 stdout 当作**结果通道**（只解析最后一行
    JSON），任何往 stdout 打的调试信息都会污染结果。

    支持 printf 风格（``_log("x=%s", v)``）—— 这样和模块里已有的
    ``_log("...%s", exc)`` 调用风格一致，不必逐处改成 f-string。
    """
    import sys

    try:
        text = msg % args if args else str(msg)
    except Exception:  # noqa: BLE001 - 格式化失败不能连带打崩业务
        text = f"{msg} {args!r}"
    print(f"[dtpatch.bili] {text}", file=sys.stderr, flush=True)


def _looks_like_bilibili(url: str) -> bool:
    """Cheap host sniff so we only claim URLs we can actually handle.

    Mirrors the frontend's host set (including ``b23.tv`` short links) but
    deliberately does *not* resolve them -- :func:`parse_bilibili_url` does
    that, and raising here would break the YouTube path.
    """

    from urllib.parse import urlparse

    try:
        parsed = urlparse((url or "").strip().strip("`\"'"))
    except ValueError:
        return False
    if parsed.scheme.lower() not in {"http", "https"}:
        return False
    host = (parsed.hostname or "").lower().rstrip(".")
    return host in {
        "bilibili.com",
        "www.bilibili.com",
        "m.bilibili.com",
        "player.bilibili.com",
        "b23.tv",
    }


# --------------------------------------------------------------------------
# 登录凭据
# --------------------------------------------------------------------------


def _credentials() -> dict[str, str]:
    """Read the Bilibili cookies the QR-login flow stored.

    Never raises: a missing/invalid file simply means "no login", which
    degrades Bilibili to plain playback.
    """

    from . import credentials as cred

    return cred.load_cookies()


def _cookies_into_client_kwargs(cookies: dict[str, str]) -> dict[str, Any]:
    return {"cookies": cookies} if cookies else {}


# --------------------------------------------------------------------------
# B 站解析
# --------------------------------------------------------------------------


def _parse(url: str):
    """Normalise a Bilibili URL via the upstream parser.

    Raises ``deeptutor.reading.ingestion.ReadingError`` for non-Bilibili input;
    callers convert that into the standard ``TimedMediaError``.
    """

    from deeptutor.reading.ingestion import parse_bilibili_url

    return parse_bilibili_url(url)


def _segments_to_cues(segments: list[Any]) -> list[dict[str, Any]]:
    """``TranscriptSegment`` -> the video_learning cue dict shape.

    video_learning stores cues as ``{"start","end","text"}`` (see
    ``normalize_cues`` upstream); reading stores richer dataclasses. Only the
    three fields the player/transcript UI needs are carried over.
    """

    cues: list[dict[str, Any]] = []
    for seg in segments or []:
        text = str(getattr(seg, "text", "") or "").strip()
        if not text:
            continue
        try:
            start = max(0.0, float(getattr(seg, "start", 0) or 0))
            end = float(getattr(seg, "end", 0) or 0)
        except (TypeError, ValueError):
            continue
        if end <= start:
            end = start
        cues.append({"start": start, "end": end, "text": text[:_MAX_TITLE * 10]})
    return cues


# --------------------------------------------------------------------------
# 字幕抓取：自己实现，不用上游的``_load_bilibili_media``
# --------------------------------------------------------------------------

#: B 站 Web API 的固定域名。字幕 CDN 在``*.hdslb.com``。
_API = "https://api.bilibili.com"

#: 真实浏览器 UA。用``DeepTutor/ImmersiveReading`` 这类自定义 UA 会被
#: B 站当成爬虫，字幕接口直接返回空列表（实测）。
_UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0"
)

#: 字幕语言偏好（按顺序）。``ai-zh`` 是 B 站自动生成的中文轴，
#: 覆盖率远高于人工上传，所以排第一 —— 否则很多视频一条字幕都匹配不上。
_SUBTITLE_PREFERENCE = ("zh-CN", "ai-zh", "zh-Hans", "zh_CN", "zh", "ai-en", "en")


def _pick_subtitle(rows: list[dict[str, Any]]) -> dict[str, Any] | None:
    """从字幕列表里挑一条：有 URL 的优先，其次按语言偏好。"""
    usable = [r for r in rows if str(r.get("subtitle_url") or "").strip()]
    if not usable:
        return None
    for wanted in _SUBTITLE_PREFERENCE:
        for row in usable:
            lan = str(row.get("lan") or "").strip()
            if lan.lower() == wanted.lower():
                return row
    return usable[0]


def _absolute(url: str) -> str:
    """把 B 站返回的协议相对/ http URL 统一成 https。

    ★ 必须强制 https：``view`` 接口的 ``pic`` 字段实测会返回 ``http://``，
    而 DeepTutor 的页面本身是 https，浏览器会把 http 封面拦成混合内容
    ——表现为「视频能播但封面空白」。
    """
    url = url.strip()
    if url.startswith("//"):
        return f"https:{url}"
    if url.lower().startswith("http://"):
        return f"https://{url[7:]}"
    return url


def _is_subtitle_cdn(url: str) -> bool:
    """只允许 B 站自己的字幕 CDN（与上游同样的安全约束）。"""
    from urllib.parse import urlparse

    parsed = urlparse(url)
    host = (parsed.hostname or "").lower().rstrip(".")
    return parsed.scheme == "https" and (
        host == "hdslb.com" or host.endswith(".hdslb.com")
    )


# --------------------------------------------------------------------------
# 风控防护：节流 / -412 退避 / 匿名设备指纹（与 search.py 同源结论）
#
# 2026-10-11 真机踩坑：导入 B 站视频时 ``/x/web-interface/view`` 连续 5 次
# ``code=-412``（13:36:37~13:37:43，用户在连点「重试」），ban 窗口约 9 分钟。
# 而同一时刻**匿名单发/6 连发/带登录态都复现不出来** —— 说明 -412 是
# **临时速率风控**：触发靠「请求密集」，解法是节流 + 指纹 + 串行锁。
#
# 同日二次踩坑（v0.2.16 → v0.2.17）：加了「4/8/16s 退避重试」后**照样
# -412**，且首发即被拦 —— 退避间隔远短于 9 分钟 ban 窗口，用户每点一次
# 「重试」就再打一轮（1+3 发），**只会延长封禁**。结论：命中 -412 必须
# **熔断**（与 :mod:`.search` 共享 ``breaker``），窗口内本地 fail-fast，
# 一个包都不发给 B 站；文案带剩余秒数，别再引导用户狂点。
# --------------------------------------------------------------------------

#: 匿名设备指纹缓存（``finger/spi`` 的 ``b_3``，即 ``buvid3``）。
#: 没有登录态时缺 ``buvid3`` 的请求更容易被风控（search.py 实测结论）。
_anon_buvid3: str | None = None

#: B 站解析的全局串行锁。用户连点「重试」会并发触发解析，并发打
#: ``api.bilibili.com`` 正是 -412 的直接诱因；串行化后同一时间只有一个
#: 解析在打接口，配合节流把速率压到风控线以下。
_ingest_gate: Any = None
_gate_loop: Any = None


def _gate() -> Any:
    """惰性创建 :class:`asyncio.Lock`。

    ★ 必须 loop-aware：``asyncio.Lock`` 一旦在某个事件循环里用过就绑死
    该循环，换循环再用直接 ``RuntimeError``（回归测试多次 ``asyncio.run``
    就是两个循环）。检测到换循环就重建。
    """

    global _ingest_gate, _gate_loop
    loop = asyncio.get_running_loop()
    if _ingest_gate is None or _gate_loop is not loop:
        _ingest_gate = asyncio.Lock()
        _gate_loop = loop
    return _ingest_gate


async def _api_call(client: Any, path: str, params: dict[str, Any]) -> dict[str, Any]:
    """打一个 B 站 Web API，带节流与 -412 熔断。

    ★ 节流器与熔断器与搜索**共享**（``search._throttle`` / ``search.breaker``）：
    B 站的风控按客户端整体速率算，搜索与解析互相让速才是正确姿势。
    ★★ 命中 -412 **立即熔断、不重试**：B 站 ban 窗口约 9 分钟，任何
    「几分钟内再试」的重试都只是延长封禁（v0.2.16 真机教训）。
    """

    from .search import _throttle, breaker

    left = breaker.remaining()
    if left > 0:
        raise RuntimeError(
            f"B 站风控拦截中，请等约 {int(left) + 1} 秒后再试"
            "（现在点重试只会延长拦截时间）"
        )

    await _throttle.wait()
    resp = await client.get(f"{_API}{path}", params=params)
    try:
        data = resp.json()
    except ValueError:
        raise RuntimeError(
            f"Bilibili 接口 {path} 返回了非 JSON 响应（HTTP {resp.status_code}）"
        ) from None
    code = data.get("code") if isinstance(data, dict) else None
    if code == 0:
        breaker.reset()
        payload = data.get("data")
        return payload if isinstance(payload, dict) else {}
    if code == -412:
        window = breaker.trip()
        _throttle.widen(5.0)
        _log("B 站风控 -412（%s），熔断 %.0fs（此间不再发任何请求）", path, window)
        raise RuntimeError(
            f"B 站风控拦截了请求（-412）：请求过于频繁。"
            f"已暂停请求约 {int(window)} 秒，请等倒计时结束再点重试，"
            "连续快速重试会延长拦截时间"
        )
    raise RuntimeError(f"Bilibili 接口 {path} 返回 code={code}")


async def _ensure_buvid3(client: Any, cookies: dict[str, str]) -> None:
    """没有 ``buvid3`` 时取一个匿名设备指纹塞进 cookie。

    ``x/frontend/finger/spi`` 是 B 站给匿名客户端发设备指纹的接口
    （实测可用，返回 ``b_3`` 即 ``buvid3``）。指纹按进程缓存，取一次就好。
    可选增强：失败绝不影响主流程。
    """

    global _anon_buvid3
    if cookies.get("buvid3"):
        return
    if _anon_buvid3:
        cookies["buvid3"] = _anon_buvid3
        return
    try:
        resp = await client.get(f"{_API}/x/frontend/finger/spi")
        data = resp.json()
        b3 = str(((data or {}).get("data") or {}).get("b_3") or "")
    except Exception as exc:  # noqa: BLE001
        _log("取匿名 buvid3 失败（不影响主流程）: %s", exc)
        return
    if b3:
        _anon_buvid3 = b3
        cookies["buvid3"] = b3
        _log("已取匿名 buvid3 设备指纹（无登录态）")


#: 分P/合集清单的条数上限。B 站真有 748 集「大全集」分P 视频（实测搜索见过），
#: 全量落 provider_cache 会把材料 JSON 撑爆；截断只影响面板列表长度。
_MAX_COURSE_ITEMS = 300


def _extract_pages(pages: list[Any]) -> list[dict[str, Any]]:
    """``view`` 接口的 ``pages`` → 分P 清单。

    只要 page/part/cid/duration 四个字段：面板画列表 + 按全片时长换算秒数
    够用，不带 dimension/first_frame 这些体积大的。坏行跳过，与上游
    ``normalize_cues`` 的宽容策略一致。
    """

    rows: list[dict[str, Any]] = []
    for item in pages or []:
        if not isinstance(item, dict):
            continue
        try:
            page = int(item.get("page") or 0)
            cid = int(item.get("cid") or 0)
            duration = float(item.get("duration") or 0)
        except (TypeError, ValueError):
            continue
        if page <= 0 or cid <= 0:
            continue
        title = str(item.get("part") or "").strip() or f"P{page}"
        rows.append(
            {"page": page, "title": title[:_MAX_TITLE], "cid": cid, "duration": duration}
        )
    return rows[:_MAX_COURSE_ITEMS]


def _extract_season(season: Any) -> dict[str, Any] | None:
    """``view`` 接口的 ``ugc_season`` → 合集结构。

    B 站合集是 sections→episodes 两层；episode 顶层 ``duration`` 实测是
    ``None``，真实时长在 ``episode["page"]["duration"]``。扁平化成
    ``[{bvid, cid, title, duration}]``；多 section 时把 section 标题并进
    episode 标题（单 section 不加前缀，避免「默认分类 |」噪音）。
    """

    if not isinstance(season, dict):
        return None
    sections = season.get("sections") if isinstance(season.get("sections"), list) else []
    multi = len(sections) > 1
    flat: list[dict[str, Any]] = []
    for section in sections:
        if not isinstance(section, dict):
            continue
        sec_title = str(section.get("title") or "").strip()
        episodes = (
            section.get("episodes") if isinstance(section.get("episodes"), list) else []
        )
        for ep in episodes:
            if not isinstance(ep, dict):
                continue
            bvid = str(ep.get("bvid") or "").strip()
            page = ep.get("page") if isinstance(ep.get("page"), dict) else {}
            try:
                cid = int(ep.get("cid") or page.get("cid") or 0)
                duration = float(page.get("duration") or 0)
            except (TypeError, ValueError):
                continue
            if not bvid.startswith("BV") or cid <= 0:
                continue
            title = str(ep.get("title") or bvid).strip() or bvid
            if multi and sec_title:
                title = f"{sec_title} | {title}"
            flat.append(
                {
                    "bvid": bvid,
                    "cid": cid,
                    "title": title[:_MAX_TITLE],
                    "duration": duration,
                }
            )
    if not flat:
        return None
    return {
        "title": str(season.get("title") or "").strip()[:_MAX_TITLE],
        "episodes": flat[:_MAX_COURSE_ITEMS],
    }


async def _fetch_bilibili_media(
    request: Any, languages: Any, capture: dict[str, Any] | None = None
) -> BilibiliMedia:
    """抓 B 站元数据 + 字幕 + 章节。

    ``capture``（可选）用来把 ``view`` 接口里的 ``aid`` / 分P / 合集结构
    截出来旁路保存。``BilibiliMedia`` 是上游 frozen+slots dataclass，塞不进
    额外字段，只能从旁路捞。

    ★ 为什么不能直接用上游 ``_load_bilibili_media``（实测三重失效）：

    1. **接口错了。** 上游打的是 ``/x/player/v2``，而 B 站现在只在
       ``/x/player/wbi/v2`` 里返回 ``subtitle_url``。同一个视频同一份cookie，
       前者 6 条字幕但每条 ``subtitle_url`` **都是空字符串**，后者才带真实
       CDN 地址 —— 上游那行 ``if subtitle_url:`` 于是直接跳过，字幕恒为空。
    2. **不带cookie。** 上游的 headers 只有 Accept/Referer/UA，没有 Cookie，
       所以用户扫码登录对它**毫无作用** —— 登录功能等于白做。
    3. **UA 被识别为爬虫。** ``Mozilla/5.0 DeepTutor/ImmersiveReading``
       不是浏览器 UA，字幕接口会返回空列表。

    三条都实测过（BV1d7wAzsE8V，登录态下）：``nav`` 确认
    ``isLogin=True``、``player/v2`` 与 ``player/wbi/v2`` 都是 6 条字幕，
    但只有后者带 URL。
    """

    try:
        import httpx
    except ImportError as exc:  # pragma: no cover
        raise RuntimeError("Bilibili import requires httpx") from exc

    langs = _language_list(languages)
    cookies = dict(_credentials())
    headers = {
        "Accept": "application/json",
        "Referer": "https://www.bilibili.com/",
        "User-Agent": _UA,
    }

    # ★ 串行锁：连点「重试」/并发导入时，同一时间只有一个解析在打 B 站
    #   —— 并发请求正是 -412 风控的直接诱因（2026-10-11 真机踩坑）。
    async with _gate():
        # ★★ 熔断窗口内连 client 都不建：发往 B 站的任何请求（含
        #   finger/spi）都会延长封禁，必须本地 fail-fast。
        from .search import breaker as _breaker

        _left = _breaker.remaining()
        if _left > 0:
            raise RuntimeError(
                f"B 站风控拦截中，请等约 {int(_left) + 1} 秒后再试"
                "（现在点重试只会延长拦截时间）"
            )
        async with httpx.AsyncClient(
            timeout=15,
            follow_redirects=True,
            headers=headers,
            # ★ trust_env=False：B 站 API/字幕 CDN 都是国内服务，永远直连。
            # 走系统代理会撞失效端口 ConnectError，出口 IP 飘海外还会加码
            # 风控（2026-10-12 实测：代理隧道 TLS 失败 + -412 交替出现）。
            trust_env=False,
        ) as client:
            # 匿名时补一个设备指纹，降低 -412 概率（登录态自带 buvid3）。
            await _ensure_buvid3(client, cookies)
            if cookies:
                client.headers["Cookie"] = "; ".join(
                    f"{k}={v}" for k, v in cookies.items()
                )

            view = await _api_call(client, "/x/web-interface/view", {"bvid": request.bvid})
            if capture is not None:
                capture["aid"] = int(view.get("aid") or 0)
                # 分P清单 + 合集结构旁路捞出来（BilibiliMedia 是 frozen+slots
                # 塞不进，跟 aid 一个路子）。
                capture["pages"] = _extract_pages(
                    view.get("pages") if isinstance(view.get("pages"), list) else []
                )
                capture["season"] = _extract_season(view.get("ugc_season"))
            pages = view.get("pages") if isinstance(view.get("pages"), list) else []
            if not pages:
                raise RuntimeError("Bilibili 未返回可播放分P")

            page_index = min(int(getattr(request, "page_number", 1) or 1), len(pages)) - 1
            page = pages[page_index] if isinstance(pages[page_index], dict) else {}
            cid = int(page.get("cid") or 0)
            if cid <= 0:
                raise RuntimeError("Bilibili 未返回 cid")

            duration = float(page.get("duration") or view.get("duration") or 0)
            title = str(page.get("part") or view.get("title") or request.bvid).strip()
            cover = _absolute(str(view.get("pic") or ""))

            # ---- 章节 ----
            chapters: list[Any] = []
            # ---- 字幕 ----
            segments: list[Any] = []
            try:
                # ★ 关键：必须用 wbi 版，且 cid 要用真实值（不是 1）。
                player = await _api_call(
                    client, "/x/player/wbi/v2", {"bvid": request.bvid, "cid": cid}
                )
            except Exception as exc:  # noqa: BLE001 - 字幕失败不该拖垮整条链路
                if _breaker.remaining() > 0:
                    # 风控熔断中：别把「无字幕的残次导入」当成功落库，
                    # 整体失败让用户等窗口过了重试，字幕才补得回来。
                    raise
                _log("播放器接口不可用（%s），仅返回元数据", exc)
                player = {}

            from deeptutor.reading.ingestion import normalize_transcript_segments as _norm

            chapters = build_transcript_segments(_norm(player.get("view_points") or []))

            subtitle_root = player.get("subtitle")
            rows = (
                subtitle_root.get("subtitles")
                if isinstance(subtitle_root, dict)
                and isinstance(subtitle_root.get("subtitles"), list)
                else []
            )
            chosen = _pick_subtitle(rows)
            if chosen:
                url = _absolute(str(chosen.get("subtitle_url") or ""))
                if _is_subtitle_cdn(url):
                    try:
                        resp = await client.get(url)
                        if len(resp.content) > MAX_TRANSCRIPT_BYTES * 2:
                            _log("字幕体积超限，忽略")
                        else:
                            resp.raise_for_status()
                            payload = resp.json()
                            body = (
                                payload.get("body")
                                if isinstance(payload, dict)
                                and isinstance(payload.get("body"), list)
                                else []
                            )
                            segments = build_transcript_segments(_norm(body))
                            _log(
                                "已取到字幕 lan=%s 条数=%d",
                                chosen.get("lan"),
                                len(segments),
                            )
                    except Exception as exc:  # noqa: BLE001
                        _log("字幕下载失败: %s", exc)
                else:
                    _log("字幕 URL 不在 B 站 CDN 上，拒绝: %s", url[:60])

    return BilibiliMedia(
        title=title,
        cover_url=cover,
        duration_seconds=duration,
        page_number=page_index + 1,
        cid=cid,
        segments=segments,
        chapters=chapters,
    )


async def _resolve_bilibili(module, url: str, language: Any) -> dict[str, Any]:
    """把 B 站链接做成沉浸式观看**原生**能播的 material。

    ★★★ 为什么必须委托上游 reading 摄入流水线，而不是自己往video_learning 写一条

    前端 ``MediaReadingStage``（chunk 4057）这样选播放器::

        "bilibili" === e.source_kind && V ? <B站 iframe> : <YouTube iframe>

    它读的是**顶层** ``material.source_kind`` / ``material.source_url``。
    而 ``video_learning.service.public_material()`` 只透传、**不摊平**::

        payload = {k: v for k, v in material.items() if k not in {...}}  # source 仍是嵌套

    所以把 ``source_kind`` 写进 ``material["source"]`` 等于**写了个没人读的字段**：
    BV 号会被当成 YouTube videoId 喂给 ``youtube-nocookie`` iframe，表现就是
    「播放器空白 / 报错」—— 看着像功能没做，其实是字段放错了层级。

    上游 reading 侧本来就完整支持 B 站（``SourceKind.BILIBILI`` +
    ``IngestionService._process_bilibili``），且 ``MaterialRecord.to_dict()`` 的
    字段与前端读的**完全一致**。于是：

    1. 委托 ``ReadingIngestionService`` 做摄入 —— 目录行、reading store 的
       outline / unit_refs / extractor 全由上游产出，契约天然对齐；
    2. 再用**同一个 material_id** 补写 TimedMediaStore，让 video-learning 的
       字幕 / 笔记 / 进度 / subtitles.vtt 端点继续可用；
    3. 返回体把顶层字段补齐（``public_material`` 不摊平，只能自己补）。
    """

    from deeptutor.reading.catalog_models import IngestionStatus
    from deeptutor.reading.catalog_store import ReadingCatalogStore
    from deeptutor.reading.ingestion import ReadingIngestionService, ReadingStore

    request = _parse(url)  # may raise ReadingError -> caller maps it
    langs = _language_list(language)

    # 让上游的摄入流水线用**我们**的抓取实现（带登录态 + 真实 UA），
    # 同时把结果截下来，避免为了拿 cues 再打一次网络。
    captured: dict[str, Any] = {}

    async def _loader(url_value: str, languages: Any):
        media = await _fetch_bilibili_media(_parse(url_value), languages, captured)
        captured["media"] = media
        return media

    catalog = ReadingCatalogStore()
    ingestion = ReadingIngestionService(
        ReadingStore(catalog.root), catalog, bilibili_loader=_loader
    )

    # ---- 1) 走上游摄入：产出目录行(顶层 source_kind=bilibili / render_mode=video)
    try:
        record = ingestion.queue_url(url)
    except Exception as exc:  # noqa: BLE001
        raise module.TimedMediaError(f"Bilibili 链接无法入队：{exc}") from exc

    try:
        record = await ingestion.process_url(
            record.material_id, preferred_languages=langs
        )
    except Exception as exc:  # noqa: BLE001
        raise module.TimedMediaError(f"Bilibili 解析失败：{exc}") from exc

    #process_url 内部会吞异常并把状态置成 FAILED，所以必须自己判一次。
    if getattr(record, "status", None) is not IngestionStatus.READY:
        detail = str(getattr(record, "error_detail", "") or "未知原因")
        raise module.TimedMediaError(f"Bilibili 解析失败：{detail}")

    media = captured.get("media")
    if media is None:  # 兜底：理论上 loader 一定被调过
        media = await _fetch_bilibili_media(_parse(record.source_url), langs, captured)

    # ---- 2) 组装 video_learning 侧的 cues
    cues = _segments_to_cues(media.segments)
    extractor = SOURCE_SUBTITLES
    if not cues:
        # 没有字幕就退回章节标记，至少留一条时间轴（上游也是这么兜的）。
        cues = _segments_to_cues(media.chapters)
        extractor = SOURCE_CHAPTERS_ONLY if cues else SOURCE_NONE
    if not _credentials() and extractor != SOURCE_NONE:
        _log("未配置 B 站登录态，字幕/章节可能不可用")

    material_id = str(record.material_id)
    duration = int(float(getattr(media, "duration_seconds", 0) or 0))
    title = str(getattr(media, "title", "") or material_id)[:_MAX_TITLE]
    cover = str(getattr(media, "cover_url", "") or "")
    source_url = str(getattr(record, "source_url", "") or url)

    # ---- 3) 用同一个 material_id 写 TimedMediaStore（字幕/笔记/进度端点靠它）
    store = module.get_timed_media_store()
    try:
        existing = store.get(material_id)
    except module.TimedMediaNotFound:
        existing = {}

    learning = (
        existing["learning"]
        if isinstance(existing.get("learning"), dict)
        else {"last_position": request.entry_time_seconds}
    )
    learning.setdefault("last_position", request.entry_time_seconds)

    material = {
        "version": 1,
        "type": "timed_media",
        "material_id": material_id,
        "created_at": existing.get("created_at")
        or module.datetime.now(module.timezone.utc).isoformat(),
        "source": {
            #嵌套那份保留：video-learning 自己读它，前端读下面摊平的顶层字段。
            "provider": PROVIDER,
            "source_kind": PROVIDER,
            "video_id": request.bvid,
            "bvid": request.bvid,
            "page": int(getattr(media, "page_number", 1) or 1),
            "url": source_url,
            "entry_time_seconds": request.entry_time_seconds,
        },
        "metadata": {
            "title": title,
            # BilibiliMedia 没有 author 字段（上游 dataclass 里就没有），
            # 原生页面的 meta 行也不显示 UP 主，留空即可。
            "author": "",
            "duration_seconds": duration,
            "thumbnail_url": cover,
        },
        "transcript": {
            "status": "ready" if cues else "unavailable",
            "reason": "" if cues else extractor,
            "language": langs[0] if langs else "",
            "source": extractor,
            "cues": cues,
        },
        "segments": module.build_segments(cues),
        "learning": learning,
        "provider_cache": {
            "bilibili_page": int(getattr(media, "page_number", 1) or 1),
            "bilibili_cid": int(getattr(media, "cid", 0) or 0),
            # aid / 分P / 合集结构旁路存根（老材料没有这些字段）。
            "bilibili_aid": int(captured.get("aid") or 0),
            "bilibili_pages": captured.get("pages") or [],
            "bilibili_season": captured.get("season"),
        },
        "_caption_text_version": 1,
    }

    with store.lock(material_id):
        try:
            latest = store.get(material_id, lock_held=True)
        except module.TimedMediaNotFound:
            latest = {}
        if isinstance(latest.get("learning"), dict):
            material["learning"] = latest["learning"]
        store.save(material)

    _log(
        "已解析 %s P%s：%d 条字幕/章节，extractor=%s，时长=%ds，material_id=%s",
        request.bvid,
        getattr(media, "page_number", 1),
        len(cues),
        extractor,
        duration,
        material_id,
    )

    # ---- 4) 返回体：顶层字段是前端唯一认的形状
    payload = module.public_material(material, provider=PROVIDER)
    # ★ public_material 的 else 分支会塞 kind:"youtube_iframe" 且 video_id 取自
    #   source["video_id"] —— 对 B 站来说那是 BV 号，前端可能误判成 YouTube 视频。
    #   整个去掉：前端本来就会自己拼 player.bilibili.com 的 iframe。
    payload.pop("playback", None)

    flat = record.to_dict() if hasattr(record, "to_dict") else {}
    if isinstance(flat, dict):
        payload.update(flat)

    payload.update(
        {
            "material_id": material_id,
            "title": title,
            "author": "",
            "source": material["source"],
            "source_kind": PROVIDER,
            "source_url": source_url,
            "render_mode": "video",
            "cover_url": cover,
            "thumbnail_url": cover,
            "duration_seconds": duration,
            "status": "ready",
            "progress": 100,
            "extractor": extractor,
            "transcript": material["transcript"],
            "segments": material["segments"],
            "learning": material["learning"],
            "metadata": material["metadata"],
        }
    )
    return payload


def _language_list(language: Any) -> list[str]:
    if isinstance(language, str):
        return [language] if language else ["zh-CN", "zh-Hans", "zh", "en"]
    if isinstance(language, (list, tuple)):
        return [str(v).strip() for v in language if str(v).strip()]
    return ["zh-CN", "zh-Hans", "zh", "en"]


async def resolve_bilibili_url(material_id: str, url: str) -> dict[str, Any]:
    """按 B 站链接重新解析并覆盖已有 material。

    给 ``transcript/refresh`` 用：字幕可能刚补上登录态，得重抓一遍。
    ``material_id`` 只用于日志——真正的 id 由上游 ``queue_url`` 从 URL 派生，
    所以同一个链接一定会命中同一条目录记录。
    """

    from deeptutor.video_learning import service as vl

    _log("按字幕刷新请求重新解析 B 站材料 material_id=%s", material_id)
    return await _resolve_bilibili(vl, url, "")


# --------------------------------------------------------------------------
# 注入入口
# --------------------------------------------------------------------------


def apply_patch(module) -> None:
    """Rewrite ``module`` in place to accept Bilibili. Idempotent."""

    if getattr(module, "_dtpatch_bili", False):
        _log("已注入过，跳过")
        return

    original_resolve = module.resolve_material
    original_normalize = module.normalize_video_learning_settings

    async def resolve_material(url, language="", provider_override=None):
        if _looks_like_bilibili(url):
            from deeptutor.reading.ingestion import ReadingError

            try:
                return await _resolve_bilibili(module, url, language)
            except ReadingError as exc:
                raise module.TimedMediaError(str(exc)) from exc
            except module.TimedMediaError:
                raise
            except Exception as exc:  # noqa: BLE001
                _log("B 站解析异常，退回内置行为: %s", exc)
                raise module.TimedMediaError(
                    f"Bilibili 链接解析失败: {exc}"
                ) from exc
        # YouTube / invidioustays on the untouched original.
        return await original_resolve(url, language, provider_override)

    def normalize_video_learning_settings(payload):
        raw = payload if isinstance(payload, dict) else {}
        provider = str(raw.get("default_provider") or "").strip().lower()
        if provider != PROVIDER:
            return original_normalize(payload)
        # Bilibili needs no base URL; keep the youtube block so the settings
        # round-trip does not lose it, and mark the provider.
        merged = dict(raw)
        merged["default_provider"] = PROVIDER
        youtube = raw.get("youtube") if isinstance(raw.get("youtube"), dict) else {}
        transcript_provider = str(
            youtube.get("transcript_provider") or "youtube_transcript_api"
        )
        merged["youtube"] = {"transcript_provider": transcript_provider}
        invidious = raw.get("invidious") if isinstance(raw.get("invidious"), dict) else {}
        merged["invidious"] = {
            "api_base_url": str(invidious.get("api_base_url") or ""),
            "public_base_url": str(invidious.get("public_base_url") or ""),
        }
        merged["bilibili"] = {"credentials_present": bool(_credentials())}
        merged["version"] = 1
        return merged

    module.resolve_material = resolve_material
    module.normalize_video_learning_settings = normalize_video_learning_settings

    # Keep the provider registry honest so anything iterating PROVIDER_RESOLVERS
    # does not KeyError on bilibili.
    resolvers = getattr(module, "PROVIDER_RESOLVERS", None)
    if isinstance(resolvers, dict):
        async def _unavailable(request, language):  # noqa: ARG001
            raise module.TimedMediaError(
                "Bilibili 不通过该provider 解析(immersive provider 由补丁直接处理)。"
            )

        resolvers.setdefault(PROVIDER, _unavailable)

    module._dtpatch_bili = True
    _log("注入完成：B 站链接将走沉浸式观看（字幕需登录态）")


def credentials_path() -> Path:
    """Where the QR-login flow writes cookies (exposed for the shell)."""

    from . import credentials as cred

    return cred.credentials_path()


def _self_test() -> None:
    """Tiny smoke check, runnable as ``python -m dtpatch_bili``."""

    from deeptutor.video_learning import service

    apply_patch(service)
    for url, expect in (
        ("https://www.bilibili.com/video/BV1GJ411x7h7/", True),
        ("https://b23.tv/abcdef", True),
        ("https://www.youtube.com/watch?v=dQw4w9WgXcQ", False),
        ("https://youtu.be/dQw4w9WgXcQ", False),
        ("not a url", False),
    ):
        got = _looks_like_bilibili(url)
        status = "ok " if got == expect else "FAIL"
        print(f"  {status} {url} -> bilibili={got} (expected {expect})")
    os.environ.setdefault("PYTHONHASHSEED", "0")


if __name__ == "__main__":
    _self_test()