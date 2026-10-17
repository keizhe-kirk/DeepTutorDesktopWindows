"""无字幕视频 ASR 兜底（任务 #5）。

# 背景

沉浸式观看的视频，上游若有官方字幕就走字幕；没有的话 ``transcript`` 是空的，
字幕检索 / AI 陪看 / 标记同步全都用不上。本模块在「无字幕」时自动用
B 站音频流 + 本地 LM Studio 的 whisper 转写，把 cues 补回来。

# 链路

1. 从材料 ``source`` 取 ``bvid`` + ``page``；``cid`` 从 ``provider_cache`` 的
   ``bilibili_pages``（或合集 ``bilibili_season``）拿，拿不到就打一次 view 补取
   （复用 :mod:`dtpatch_bili.series` 的逻辑，不重复造）。
2. playurl（unsigned，``fnval=16``）→ ``dash.audio[0].baseUrl``（AAC，最低档）。
3. 流式下载音频（带体量上限，防长视频撑爆内存/磁盘）。
4. POST 到 LM Studio 的 ``/v1/audio/transcriptions``（``verbose_json``）→ segments。
5. segments → cues（``start`` / ``end`` / ``text``）。

# ★ 硬约束（踩过）

- bundled runtime **没有 ffmpeg**、也没有 AAC 解码器（只有 ``numpy`` / ``wave`` /
  ``pydub``，pydub 本身也依赖 ffmpeg）。所以**不能本地把 ``.m4s``(AAC) 转成 WAV**，
  只能把 AAC 字节直接喂 LM Studio，赌它的 whisper 构建带 ffmpeg 后端能解
  AAC/fMP4。若 LM Studio 解不动 → 静默降级（不当机、不报错惊动用户，
  只是这次没字幕）。
- LM Studio 没跑 / 没装 whisper 模型 → 端点 4xx/连不上 → 同样静默降级。

# 失败策略

ASR 是纯装饰性兜底：任何失败都写进 job.error，前端轮询到就静默跳过，
绝不 4xx/5xx、绝不影响播放。
"""

from __future__ import annotations

import asyncio
import os
import time
from typing import Any

#: LM Studio 默认地址（上游 vllm/lmstudio 适配器也用这个 base）。可用环境变量
#: ``DT_BILI_ASR_BASE_URL`` 覆盖（比如换了端口或走远程 whisper 服务）。
ASR_BASE_URL = os.environ.get("DT_BILI_ASR_BASE_URL", "http://localhost:1234/v1").rstrip("/")

#: LM Studio 的 /audio/transcriptions 用当前已加载的模型，这个字段按 OpenAI
#: 规范必填但常被忽略，随便给一个占位即可。
ASR_MODEL = os.environ.get("DT_BILI_ASR_MODEL", "whisper")

#: 音频体量上限（字节）。AAC 最低档约 100kbps ≈ 45MB/h，60MB 约 80 分钟；
#: 超过说明是超长视频，下载 + 转写都不划算，直接跳过。
ASR_MAX_BYTES = int(os.environ.get("DT_BILI_ASR_MAX_BYTES", "60000000"))

#: 转写请求超时（秒）。长视频 whisper 可能跑几分钟，给足。
ASR_REQUEST_TIMEOUT = float(os.environ.get("DT_BILI_ASR_REQUEST_TIMEOUT", "600"))

#: 音频下载超时（秒）。
ASR_DOWNLOAD_TIMEOUT = float(os.environ.get("DT_BILI_ASR_DOWNLOAD_TIMEOUT", "120"))

_INSTALLED = False

#: material_id -> job 状态。进程级（重启即丢，下次面板再触发一次）。
_JOBS: dict[str, dict[str, Any]] = {}

#: material_id -> asyncio.Task。与 ``_JOBS`` 分开存（Task 不可序列化）。
_TASKS: dict[str, "asyncio.Task[None]"] = {}


def _log(msg: str, *args: object) -> None:
    """记一行 stderr 日志（printf 风格，与包内其他模块一致）。"""

    import sys

    try:
        text = msg % args if args else str(msg)
    except Exception:  # noqa: BLE001 - 格式化失败不能连带打崩业务
        text = f"{msg} {args!r}"
    print(f"[dtpatch.asr] {text}", file=sys.stderr, flush=True)


# ── 客户端 ──────────────────────────────────────────────────────


def _build_client() -> Any:
    """建一个 B 站客户端：直连 + 浏览器 UA + Referer + 匿名 cookie。"""

    import httpx

    from . import _UA, credentials as cred

    return httpx.AsyncClient(
        timeout=ASR_DOWNLOAD_TIMEOUT,
        follow_redirects=True,
        headers={
            "Accept": "application/json, */*",
            "Referer": "https://www.bilibili.com/",
            "User-Agent": _UA,
        },
        cookies=dict(cred.load_cookies()) or None,
        trust_env=False,
    )


# ── 解析 ────────────────────────────────────────────────────────


def _int(value: Any) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


def _resolve(material: dict[str, Any]) -> tuple[str, int, int, float]:
    """从材料解析 (bvid, cid, page, duration)。cid 取不到时返回 0（调用方补 view）。"""

    from . import series

    source = material.get("source") if isinstance(material.get("source"), dict) else {}
    bvid = str(source.get("bvid") or source.get("video_id") or "")
    page = _int(source.get("page")) or 1

    cache = material.get("provider_cache") if isinstance(material.get("provider_cache"), dict) else {}
    kind, pages, season = series._course_from_cache(cache)

    duration = 0.0
    if pages:
        for row in pages:
            if _int(row.get("page")) == page:
                cid = _int(row.get("cid"))
                duration = float(row.get("duration") or 0)
                return bvid, cid, page, duration
        # 没匹配到当前 P（少见），退而用第一个
        first = pages[0]
        return bvid, _int(first.get("cid")), page, float(first.get("duration") or 0)
    if season:
        for ep in series._rows(season.get("episodes")):
            if str(ep.get("bvid") or "") == bvid:
                return bvid, _int(ep.get("cid")), page, float(ep.get("duration") or 0)
    return bvid, 0, page, duration


def _pick_audio_url(data: dict[str, Any]) -> str:
    """从 playurl 响应挑最低码率的 AAC 流 URL（体量最小、转写够用）。"""

    dash = data.get("dash") if isinstance(data.get("dash"), dict) else {}
    audio = dash.get("audio") if isinstance(dash.get("audio"), list) else []
    if not audio:
        raise RuntimeError("playurl 没返回音频流（可能需登录或区域限制）")
    audio.sort(key=lambda r: _int(r.get("bandwidth")))
    row = audio[0]
    url = str(row.get("baseUrl") or row.get("base_url") or "")
    if not url:
        raise RuntimeError("音频流 URL 为空")
    return url


def _parse_cues(data: dict[str, Any], duration: float) -> list[dict[str, Any]]:
    """verbose_json / 普通 json → cues。没有任何分段时退化为整段一条。"""

    segs = data.get("segments") if isinstance(data.get("segments"), list) else []
    cues: list[dict[str, Any]] = []
    for s in segs:
        if not isinstance(s, dict):
            continue
        text = str(s.get("text") or "").strip()
        if not text:
            continue
        cues.append(
            {
                "start": float(s.get("start") or 0),
                "end": float(s.get("end") or 0),
                "text": text,
            }
        )
    if cues:
        return cues
    text = str(data.get("text") or "").strip()
    if text:
        return [{"start": 0.0, "end": float(duration or 0), "text": text}]
    raise RuntimeError("LM Studio 返回空转写")


# ── 网络三步（各自独立、便于测试时替换）──────────────────────────


async def _fetch_audio_url(bvid: str, cid: int) -> str:
    """playurl unsigned 取音频 URL（走 _api_call：直连 + 节流 + 熔断）。"""

    from . import _api_call

    async with _build_client() as client:
        data = await _api_call(
            client,
            "/x/player/wbi/playurl",
            {"bvid": bvid, "cid": cid, "fnval": 16, "fourk": 1},
        )
    return _pick_audio_url(data)


async def _download_audio(url: str) -> bytes:
    """流式下载音频，超体量上限即抛（防长视频撑爆）。"""

    async with _build_client() as client:
        async with client.stream("GET", url) as resp:
            try:
                resp.raise_for_status()
            except Exception as exc:  # noqa: BLE001
                raise RuntimeError(f"音频下载失败（HTTP {getattr(resp, 'status_code', '?')}）") from exc
            buf = bytearray()
            async for chunk in resp.aiter_bytes(1 << 16):
                buf.extend(chunk)
                if len(buf) > ASR_MAX_BYTES:
                    mb = ASR_MAX_BYTES // 1_000_000
                    raise RuntimeError(f"音频过大（>{mb}MB），超过 ASR 兜底上限，跳过")
            return bytes(buf)


async def _transcribe(audio: bytes, duration: float) -> list[dict[str, Any]]:
    """把 AAC 字节喂给 LM Studio 的 whisper，拿 cues。"""

    import httpx

    url = ASR_BASE_URL + "/audio/transcriptions"
    async with httpx.AsyncClient(timeout=ASR_REQUEST_TIMEOUT, trust_env=False) as client:
        try:
            resp = await client.post(
                url,
                data={"model": ASR_MODEL, "response_format": "verbose_json"},
                files={"file": ("audio.m4a", audio, "audio/mp4")},
            )
        except Exception as exc:  # noqa: BLE001
            raise RuntimeError(f"连不上 LM Studio（{ASR_BASE_URL}）：{type(exc).__name__}") from exc
    if resp.status_code >= 400:
        raise RuntimeError(
            f"LM Studio 转写失败（HTTP {resp.status_code}）：{resp.text[:200]}"
        )
    try:
        data = resp.json()
    except ValueError:
        raise RuntimeError("LM Studio 返回非 JSON（可能不是 whisper 模型或未加载）")
    return _parse_cues(data, duration)


# ── 落盘（持久化 cues，刷新/重载后还在）──────────────────────────


def _persist_cues(material_id: str, cues: list[dict[str, Any]]) -> None:
    try:
        from deeptutor.video_learning import service as vl

        from . import series

        store = vl.get_timed_media_store()
        with store.lock(material_id):
            try:
                material = store.get(material_id, lock_held=True)
            except Exception:  # noqa: BLE001
                return
            cache = material.get("provider_cache")
            if not isinstance(cache, dict):
                cache = {}
                material["provider_cache"] = cache
            cache["bilibili_asr_cues"] = cues
            store.save(material)
        _log("已把 ASR cues 写回材料 %s（%d 条）", material_id, len(cues))
    except Exception as exc:  # noqa: BLE001
        _log("写回 ASR cues 失败 %s: %s: %s", material_id, type(exc).__name__, exc)


def _load_cues(material_id: str) -> list[dict[str, Any]]:
    try:
        from deeptutor.video_learning import service as vl

        store = vl.get_timed_media_store()
        material = store.get(material_id)
        cache = material.get("provider_cache") if isinstance(material.get("provider_cache"), dict) else {}
        cues = cache.get("bilibili_asr_cues")
        return cues if isinstance(cues, list) else []
    except Exception:  # noqa: BLE001
        return []


# ── 编排 ────────────────────────────────────────────────────────


async def transcribe_material(material_id: str) -> list[dict[str, Any]]:
    """无字幕兜底主流程。抛异常由调用方（job）接住降级。"""

    from . import series

    material = series._material(material_id)
    if not isinstance(material, dict):
        raise RuntimeError("材料不存在")
    bvid, cid, page, duration = _resolve(material)
    if not bvid:
        raise RuntimeError("这个材料不是 B 站视频")

    # cid 没在缓存里 → 打一次 view 补
    if cid == 0:
        pages, _ = await series._fetch_view_course(bvid)
        if pages:
            idx = min(page, len(pages)) - 1
            cid = _int(pages[idx].get("cid"))

    if cid == 0:
        raise RuntimeError("取不到音频 cid")

    audio_url = await _fetch_audio_url(bvid, cid)
    audio = await _download_audio(audio_url)
    cues = await _transcribe(audio, duration)
    _persist_cues(material_id, cues)
    return cues


# ── 任务机（后台跑，长视频不阻塞请求）────────────────────────────


async def _run_job(material_id: str) -> None:
    try:
        cues = await transcribe_material(material_id)
        _JOBS[material_id] = {
            "state": "done",
            "cues": cues,
            "error": "",
            "finished": time.time(),
        }
        _log("ASR 完成 %s（%d 条 cues）", material_id, len(cues))
    except Exception as exc:  # noqa: BLE001
        _JOBS[material_id] = {
            "state": "error",
            "cues": [],
            "error": _friendly(exc),
            "finished": time.time(),
        }
        _log("ASR 失败 %s: %s", material_id, _friendly(exc))


async def start_asr(material_id: str) -> dict[str, Any]:
    """触发一次 ASR（幂等：已在跑 / 已完成则不动）。"""

    existing = _JOBS.get(material_id)
    if existing and existing.get("state") in ("pending", "done"):
        return existing
    cached = _load_cues(material_id)
    if cached:
        _JOBS[material_id] = {
            "state": "done",
            "cues": cached,
            "error": "",
            "finished": 0,
        }
        return _JOBS[material_id]
    _JOBS[material_id] = {
        "state": "pending",
        "cues": [],
        "error": "",
        "started": time.time(),
    }
    _TASKS[material_id] = asyncio.create_task(_run_job(material_id))
    _log("ASR 已触发 %s", material_id)
    return _JOBS[material_id]


def asr_status(material_id: str) -> dict[str, Any]:
    job = _JOBS.get(material_id)
    if job:
        return job
    cached = _load_cues(material_id)
    if cached:
        return {"state": "done", "cues": cached, "error": "", "finished": 0}
    return {"state": "none", "cues": [], "error": ""}


def _friendly(exc: Exception) -> str:
    text = str(exc)
    name = type(exc).__name__
    if "LM Studio" in text or "localhost:1234" in text:
        return "本地 LM Studio 未运行或未加载 whisper 模型，无字幕兜底暂不可用"
    if "音频过大" in text:
        return text
    if "风控" in text or "-412" in text:
        return text
    lowered = text.lower()
    if "timeout" in lowered:
        return "B 站接口/音频下载超时，请稍后再试"
    if "connect" in lowered or "network" in lowered:
        return "连不上（已强制直连，请检查网络）"
    return f"ASR 兜底失败：{name}: {text[:160]}"


# ── 挂载 ────────────────────────────────────────────────────────


def install(router_module) -> bool:
    """在 video_learning router 上注册 ASR 端点（POST 触发 / GET 查状态）。"""

    global _INSTALLED
    if _INSTALLED:
        return False

    router = getattr(router_module, "router", None)
    if router is None:
        return False

    from fastapi.responses import JSONResponse

    @router.post("/bilibili/materials/{material_id}/asr")
    async def bilibili_asr_start(material_id: str) -> JSONResponse:
        """触发一次 ASR 兜底（幂等）。立即返回，转写在后台跑。"""
        payload = await start_asr(material_id)
        return JSONResponse(payload, headers={"Cache-Control": "no-store"})

    @router.get("/bilibili/materials/{material_id}/asr")
    async def bilibili_asr_status(material_id: str) -> JSONResponse:
        """查 ASR 状态：none / pending / done / error（done 带 cues）。"""
        payload = asr_status(material_id)
        return JSONResponse(payload, headers={"Cache-Control": "no-store"})

    _INSTALLED = True
    _log("ASR 端点已挂载：POST/GET /bilibili/materials/{id}/asr")
    return True


def try_install(module_name: str = "deeptutor.api.routers.video_learning") -> bool:
    try:
        import importlib

        module = importlib.import_module(module_name)
    except Exception as exc:  # noqa: BLE001
        _log(f"未找到 {module_name}，ASR 本次不挂载（不影响其他功能）: {exc}")
        return False
    try:
        return install(module)
    except Exception as exc:  # noqa: BLE001
        _log(f"挂载 ASR 失败（不影响其他功能）: {type(exc).__name__}: {exc}")
        return False
