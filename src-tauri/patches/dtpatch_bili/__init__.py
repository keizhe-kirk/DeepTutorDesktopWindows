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
SOURCE_SUBTITLES = "bilibili_subtitles"
SOURCE_CHAPTERS_ONLY = "bilibili_chapters_only"
SOURCE_NONE = "bilibili_no_subtitles"
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


async def _fetch_bilibili_media(request: Any, languages: Any) -> BilibiliMedia:
    """抓 B 站元数据 + 字幕 + 章节。

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
    cookies = _credentials()
    headers = {
        "Accept": "application/json",
        "Referer": "https://www.bilibili.com/",
        "User-Agent": _UA,
    }
    if cookies:
        headers["Cookie"] = "; ".join(f"{k}={v}" for k, v in cookies.items())

    async with httpx.AsyncClient(
        timeout=15, follow_redirects=True, headers=headers
    ) as client:

        async def api(path: str, params: dict[str, Any]) -> dict[str, Any]:
            resp = await client.get(f"{_API}{path}", params=params)
            data = resp.json()
            if not isinstance(data, dict) or data.get("code") != 0:
                raise RuntimeError(
                    f"Bilibili 接口 {path} 返回 code="
                    f"{(data or {}).get('code') if isinstance(data, dict) else '?'}"
                )
            payload = data.get("data")
            return payload if isinstance(payload, dict) else {}

        view = await api("/x/web-interface/view", {"bvid": request.bvid})
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
            player = await api(
                "/x/player/wbi/v2", {"bvid": request.bvid, "cid": cid}
            )
        except Exception as exc:  # noqa: BLE001 - 字幕失败不该拖垮整条链路
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
    """Produce the same material dict ``resolve_material`` would, for Bilibili."""

    request = _parse(url)  # may raise ReadingError -> caller maps it
    cookies = _credentials()

    from deeptutor.reading.ingestion import ReadingError

    langs = _language_list(language)

    # ★ 走自己的实现（见 ``_fetch_bilibili_media`` 的文档：上游那条路
    # 接口选错 + 不带 cookie + UA 被当爬虫，三重失效）。
    try:
        media = await _fetch_bilibili_media(request, langs)
    except ReadingError:
        raise
    except Exception as exc:  # noqa: BLE001 - surface as a user-facing failure
        raise module.TimedMediaError(
            f"Bilibili 无法加载该视频({type(exc).__name__}: {exc})。"
        ) from exc

    cues = _segments_to_cues(media.segments)
    chapters = media.segments if not cues else []

    # Bilibili exposes chapters through view_points and subtitles separately;
    # when subtitles are missing we still want *some* timeline, so chapters
    # become the cue source (this is exactly what the reading area does with
    # its ``bilibili-chapters-only`` fallback).
    source_kind = SOURCE_SUBTITLES
    if not cues and chapters:
        cues = _segments_to_cues(chapters)
        source_kind = SOURCE_CHAPTERS_ONLY
    elif not cues:
        source_kind = SOURCE_NONE
    if not cookies and source_kind != SOURCE_NONE:
        _log("未配置 B 站登录态，字幕/章节可能不可用")

    store = module.get_timed_media_store()
    material_id = module.material_id_for(f"bili:{request.bvid}:p{media.page_number}")
    try:
        existing = store.get(material_id)
    except module.TimedMediaNotFound:
        existing = {}

    duration = int(float(media.duration_seconds or 0))
    learning = (
        existing["learning"]
        if isinstance(existing.get("learning"), dict)
        else {"last_position": request.entry_time_seconds}
    )
    learning.setdefault("last_position", request.entry_time_seconds)

    title = (media.title or request.bvid)[:_MAX_TITLE]
    canonical = request.canonical_url
    if media.page_number > 1:
        canonical = f"{canonical}?p={media.page_number}"

    material = {
        "version": 1,
        "type": "timed_media",
        "material_id": material_id,
        "created_at": existing.get("created_at")
        or module.datetime.now(module.timezone.utc).isoformat(),
        "source": {
            # ★ The two fields the frontend switches on: ``provider`` selects the
            # Bilibili iframe branch, ``video_id``/``url`` feed its parser.
            "provider": PROVIDER,
            "source_kind": PROVIDER,
            "video_id": request.bvid,
            "bvid": request.bvid,
            "page": media.page_number,
            "url": canonical,
            "entry_time_seconds": request.entry_time_seconds,
        },
        "metadata": {
            "title": title,
            "author": "",
            "duration_seconds": duration,
            "thumbnail_url": media.cover_url or "",
        },
        "transcript": {
            "status": "ready" if cues else "unavailable",
            "reason": "" if cues else source_kind,
            "language": langs[0] if langs else "",
            "source": source_kind,
            "cues": cues,
        },
        "segments": module.build_segments(cues),
        "learning": learning,
        "provider_cache": {
            "bilibili_page": media.page_number,
            "bilibili_cid": media.cid,
            "bilibili_chapters": len(chapters),
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
        f"已解析 {request.bvid} P{media.page_number}: "
        f"{len(cues)} 条字幕/章节, 来源={source_kind}, 时长={duration}s"
    )
    payload = module.public_material(material, provider=PROVIDER)
    # ★ public_material 的 else 分支会塞``kind: "youtube_iframe"``,而它的
    # video_id 取自 source["video_id"] —— 对 B 站来说那是个 BV 号,前端可能
    # 误判成 YouTube 视频。这里整个去掉:前端本来就会pop("playback") 自己拼
    # player.bilibili.com 的iframe,留着反而有害。
    payload.pop("playback", None)
    return payload


def _language_list(language: Any) -> list[str]:
    if isinstance(language, str):
        return [language] if language else ["zh-CN", "zh-Hans", "zh", "en"]
    if isinstance(language, (list, tuple)):
        return [str(v).strip() for v in language if str(v).strip()]
    return ["zh-CN", "zh-Hans", "zh", "en"]


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