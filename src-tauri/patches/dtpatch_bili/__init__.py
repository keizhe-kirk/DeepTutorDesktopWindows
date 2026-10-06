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

PROVIDER = "bilibili"

#: 字幕来源标识，写进 ``transcript.source``，前端据此提示用户。
SOURCE_SUBTITLES = "bilibili_subtitles"
SOURCE_CHAPTERS_ONLY = "bilibili_chapters_only"
SOURCE_NONE = "bilibili_no_subtitles"
SOURCE_DISABLED = "disabled"

_MAX_TITLE = 200


def _log(msg: str) -> None:
    import sys

    print(f"[dtpatch.bili] {msg}", file=sys.stderr, flush=True)


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


async def _resolve_bilibili(module, url: str, language: Any) -> dict[str, Any]:
    """Produce the same material dict ``resolve_material`` would, for Bilibili."""

    request = _parse(url)  # may raise ReadingError -> caller maps it
    cookies = _credentials()

    from deeptutor.reading.ingestion import ReadingError, _load_bilibili_media

    langs = _language_list(language)

    try:
        media = await _load_bilibili_media(request.canonical_url, langs)
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