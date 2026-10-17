"""高能进度条（弹幕密度）+ 热评（任务 #4）。

# 是什么

沉浸式观看 B 站视频时，播放器上方多一条「高能进度条」：把全片按
:data:`HEAT_BUCKETS` 格切分，每格填该秒区间的**弹幕密度**（颜色越深
弹幕越密），点哪跳哪。另有一个「热评」面板，列评论区高赞。

# 为什么用 list.so 而不是 seg.so

B 站有两套弹幕接口：

- ``x/v2/dm/web/seg.so``：protobuf 二进制，按 6 分钟分段，29 小时的视频
  要 296 个请求 —— 节流 3s 一个就是 15 分钟，根本不能用；
- ``x/v1/dm/list.so``：**一个请求拿全量 XML**（实测 29.6 小时视频
  273KB / 2900 条），stdlib ``ElementTree`` 直接解析，零依赖。

弹幕 XML 形状（实测）::

    <i><d p="485.02400,1,25,16777215,1773629670,0,c9466ea9,dmid,10">文本</d>...</i>

``p`` 属性第一个字段就是视频内秒数（浮点），密度只用它。

# 风控（与搜索/解析同一套结论，勿重复造）

- 熔断器 / 节流器**共用** ``search.breaker`` / ``search._throttle``：
  B 站风控按客户端整体速率算，所有出口必须共享同一块表；
- ``trust_env=False`` 永远直连（2026-10-12 实测：走系统代理会撞失效
  端口 ConnectError，出口 IP 飘海外还加码 -412）；
- 命中 -412 **立即熔断不重试**（ban 窗口约 9 分钟，重试只是延长封禁）；
- 弹幕结果按 cid 缓存 :data:`CACHE_TTL` 秒，切出去再回来不重复请求。

# 失败策略

高能条与热评都是**装饰性**功能：任何失败（风控 / 网络 / 非 B 站材料）
都返回 ``{"error": ...}`` 由面板决定降级 —— 绝不 500、绝不影响播放。
"""

from __future__ import annotations

import time
import xml.etree.ElementTree as ET
from typing import Any

#: 高能条分辨率：全片切多少格。240 格在 1080p 下每格约 8px，再细前端
#: 也渲染不出差别，只会让 gradient 字符串无谓膨胀。
HEAT_BUCKETS = 240

#: 热评最多列多少条（面板一屏放得下，又不至于把评论区搬空）。
MAX_COMMENTS = 12

#: 弹幕密度缓存 TTL（秒）。弹幕不会自己变（除非 UP 主清理），缓存
#: 10 分钟足够避免「切出去再回来」的重复请求。
CACHE_TTL = 600.0

#: 缓存条数上限，防长会话里无限增长（超了丢最旧的一条）。
CACHE_MAX_ENTRIES = 32

#: 单次请求超时。弹幕 XML 可能很大（实测 273KB），给足。
REQUEST_TIMEOUT = 25.0

#: 弹幕全量接口（XML）。参数只要 ``oid=cid``。
_DANMAKU_URL = "https://api.bilibili.com/x/v1/dm/list.so"

#: 评论区接口路径（走 ``_api_call`` 拼 ``api.bilibili.com`` 前缀）。
#: ``type=1`` = 视频评论，``oid=aid``，``mode=3`` = 热门排序。
_REPLY_PATH = "/x/v2/reply/main"

_INSTALLED = False

#: cid -> (写入时刻, 密度 payload)。进程级缓存，重启即丢（多打一次而已）。
_CACHE: dict[int, tuple[float, dict[str, Any]]] = {}


def _log(msg: str, *args: object) -> None:
    """记一行 stderr 日志。printf 风格（与包内其他模块一致）。

    ★ 必须走 stderr：Rust 侧把 stdout 当作**结果通道**（只解析最后一行
    JSON），任何往 stdout 打的调试信息都会污染结果。
    """

    import sys

    try:
        text = msg % args if args else str(msg)
    except Exception:  # noqa: BLE001 - 格式化失败不能连带打崩业务
        text = f"{msg} {args!r}"
    print(f"[dtpatch.heat] {text}", file=sys.stderr, flush=True)


# ── 弹幕 XML → 秒数 ──────────────────────────────────────────────


def parse_danmaku_seconds(raw: bytes) -> list[float]:
    """从 ``list.so`` 的 XML 里取出每条弹幕的视频内秒数。

    坏行（``p`` 缺失 / 不是数字）跳过，与上游 ``normalize_cues`` 的
    「坏行不留垃圾」原则一致。解析不了的整体报错。
    """

    if not raw:
        return []
    try:
        root = ET.fromstring(raw)
    except ET.ParseError as exc:
        raise RuntimeError(f"弹幕数据不是合法 XML（{exc}）") from exc
    seconds: list[float] = []
    for node in root.iter("d"):
        attr = node.get("p") or ""
        head = attr.split(",", 1)[0]
        try:
            value = float(head)
        except ValueError:
            continue
        if value >= 0:
            seconds.append(value)
    return seconds


def density(seconds: list[float], duration: float, buckets: int = HEAT_BUCKETS) -> list[int]:
    """把弹幕秒数按全片时长分桶计数。``duration<=0`` 时返回空列表。"""

    if duration <= 0 or buckets <= 0:
        return []
    counts = [0] * buckets
    span = duration / buckets
    for value in seconds:
        index = int(value / span)
        if index >= buckets:  # 弹幕时间戳可能略超时长（UP 主改过稿）
            index = buckets - 1
        counts[index] += 1
    return counts


# ── 热评 ────────────────────────────────────────────────────────


def comment_rows(payload: Any, limit: int = MAX_COMMENTS) -> list[dict[str, Any]]:
    """``x/v2/reply/main`` 的 data → 面板好渲染的行。

    只取前端要显示的字段：用户 / 赞 / 文本 / 楼中楼数。空消息跳过。
    """

    rows: list[dict[str, Any]] = []
    replies = payload.get("replies") if isinstance(payload, dict) else None
    for item in replies or []:
        if not isinstance(item, dict):
            continue
        content = item.get("content") if isinstance(item.get("content"), dict) else {}
        member = item.get("member") if isinstance(item.get("member"), dict) else {}
        message = " ".join(str(content.get("message") or "").split())
        if not message:
            continue
        try:
            like = int(item.get("like") or 0)
        except (TypeError, ValueError):
            like = 0
        try:
            nested = int(item.get("rcount") or 0)
        except (TypeError, ValueError):
            nested = 0
        rows.append(
            {
                "user": str(member.get("uname") or "匿名"),
                "like": like,
                "text": message[:300],
                "replies": nested,
                "avatar": str(member.get("avatar") or ""),
            }
        )
        if len(rows) >= limit:
            break
    return rows


# ── 材料信息 ────────────────────────────────────────────────────


def material_info(material_id: str) -> dict[str, Any]:
    """从 timed_media 材料里取 cid / aid / bvid / duration。

    导入时 loader 已把 ``bilibili_cid`` / ``bilibili_aid`` 写进
    ``provider_cache``；更早导入的材料没有 aid，靠 view 接口兜底补。
    """

    from deeptutor.video_learning import service as vl

    material = vl.get_timed_media_store().get(material_id)
    source = material.get("source") if isinstance(material.get("source"), dict) else {}
    cache = (
        material.get("provider_cache")
        if isinstance(material.get("provider_cache"), dict)
        else {}
    )
    metadata = material.get("metadata") if isinstance(material.get("metadata"), dict) else {}

    def _int(value: Any) -> int:
        try:
            return int(value or 0)
        except (TypeError, ValueError):
            return 0

    return {
        "cid": _int(cache.get("bilibili_cid")),
        "aid": _int(cache.get("bilibili_aid")),
        "bvid": str(source.get("bvid") or source.get("video_id") or ""),
        "page": _int(source.get("page")) or 1,
        "duration": float(metadata.get("duration_seconds") or 0),
    }


# ── 网络 ────────────────────────────────────────────────────────


def _client(httpx_mod: Any, cookies: dict[str, str]):
    """B 站客户端：直连 + 真实 UA + 登录态。三件套理由见模块文档。"""

    from . import _UA

    return httpx_mod.AsyncClient(
        timeout=REQUEST_TIMEOUT,
        follow_redirects=True,
        headers={
            "Accept": "application/json, text/xml, */*",
            "Referer": "https://www.bilibili.com/",
            "User-Agent": _UA,
        },
        cookies=cookies or None,
        trust_env=False,
    )


def _breaker_gate() -> None:
    """熔断窗口内 fail-fast（文案带剩余秒数，别引导用户狂点）。"""

    from .search import breaker

    left = breaker.remaining()
    if left > 0:
        raise RuntimeError(
            f"B 站风控拦截中，请等约 {int(left) + 1} 秒后再试"
            "（现在点重试只会延长拦截时间）"
        )


async def _danmaku_seconds(client: Any, cid: int) -> list[float]:
    """拉全量弹幕秒数。XML 接口没有 JSON ``code``，-412 靠 body 嗅探。"""

    from .search import _throttle, breaker

    _breaker_gate()
    await _throttle.wait()
    resp = await client.get(_DANMAKU_URL, params={"oid": cid})
    if resp.status_code != 200:
        raise RuntimeError(f"弹幕接口返回 HTTP {resp.status_code}")
    body = resp.content
    # -412 在 XML 接口上不回 JSON code，但风控错误体里能闻到这两个标记
    if b'"code":-412' in body or b"request was banned" in body:
        window = breaker.trip()
        _throttle.widen(5.0)
        _log("弹幕接口命中 -412 风控，熔断 %.0fs", window)
        raise RuntimeError(
            f"触发了 B 站风控（-412）：请求过于频繁。已暂停请求约 "
            f"{int(window)} 秒，请等倒计时结束再点重试"
        )
    seconds = parse_danmaku_seconds(body)
    breaker.reset()
    return seconds


async def _view_identifiers(client: Any, bvid: str, page: int) -> tuple[int, int]:
    """老材料没有 aid/cid 时打一次 view 补上。返回 ``(aid, cid)``。"""

    from . import _api_call

    view = await _api_call(client, "/x/web-interface/view", {"bvid": bvid})
    pages = view.get("pages") if isinstance(view.get("pages"), list) else []
    index = min(max(page, 1), len(pages)) - 1 if pages else 0
    row = pages[index] if pages and isinstance(pages[index], dict) else {}
    return int(view.get("aid") or 0), int(row.get("cid") or 0)


# ── 缓存 ────────────────────────────────────────────────────────


def _cache_get(cid: int) -> dict[str, Any] | None:
    row = _CACHE.get(cid)
    if row is None:
        return None
    stamp, payload = row
    if time.monotonic() - stamp > CACHE_TTL:
        _CACHE.pop(cid, None)
        return None
    return payload


def _cache_put(cid: int, payload: dict[str, Any]) -> None:
    if cid not in _CACHE and len(_CACHE) >= CACHE_MAX_ENTRIES:
        oldest = min(_CACHE, key=lambda key: _CACHE[key][0])
        _CACHE.pop(oldest, None)
    _CACHE[cid] = (time.monotonic(), payload)


# ── 对外：两个 fetch ────────────────────────────────────────────


def _empty_heat(error: str) -> dict[str, Any]:
    return {"points": [], "max": 0, "total": 0, "duration": 0.0, "error": error}


async def fetch_heat(material_id: str) -> dict[str, Any]:
    """弹幕密度点串。失败返回 ``_empty_heat(error)``，绝不抛。"""

    try:
        import httpx
    except ImportError:
        return _empty_heat("httpx 不可用")

    try:
        info = material_info(material_id)
    except Exception as exc:  # noqa: BLE001 - 材料不存在/损坏都是降级
        return _empty_heat(f"材料读取失败：{type(exc).__name__}")

    cid = info["cid"]
    if cid <= 0:
        return _empty_heat("这个材料没有 B 站 cid（不是 B 站视频？）")

    cached = _cache_get(cid)
    if cached is not None:
        payload = dict(cached)
        payload["cached"] = True
        return payload

    from . import credentials as cred

    try:
        async with _client(httpx, dict(cred.load_cookies())) as client:
            seconds = await _danmaku_seconds(client, cid)
    except Exception as exc:  # noqa: BLE001 - 装饰性功能，一律降级
        _log(f"弹幕密度获取失败 {material_id}: {type(exc).__name__}: {exc}")
        return _empty_heat(_friendly(exc))

    duration = info["duration"]
    if duration <= 0 and seconds:
        # 材料里没存时长（极老的导入）：用最后一条弹幕的时间兜底
        duration = max(seconds)
    points = density(seconds, duration)
    payload = {
        "points": points,
        "max": max(points) if points else 0,
        "total": len(seconds),
        "duration": round(duration, 3),
        "cached": False,
        "error": "",
    }
    _cache_put(cid, payload)
    return payload


def _empty_comments(error: str) -> dict[str, Any]:
    return {"comments": [], "error": error}


async def fetch_comments(material_id: str) -> dict[str, Any]:
    """高赞热评。失败返回 ``_empty_comments(error)``，绝不抛。"""

    try:
        import httpx
    except ImportError:
        return _empty_comments("httpx 不可用")

    try:
        info = material_info(material_id)
    except Exception as exc:  # noqa: BLE001
        return _empty_comments(f"材料读取失败：{type(exc).__name__}")

    from . import credentials as cred

    aid = info["aid"]
    try:
        async with _client(httpx, dict(cred.load_cookies())) as client:
            if aid <= 0:
                # 老材料没存 aid：打一次 view 补（走共享节流/熔断）
                if not info["bvid"]:
                    return _empty_comments("这个材料没有 B 站 aid（不是 B 站视频？）")
                aid, _cid = await _view_identifiers(client, info["bvid"], info["page"])
                if aid <= 0:
                    return _empty_comments("B 站没返回 aid，热评取不到")
            from . import _api_call

            data = await _api_call(
                client,
                _REPLY_PATH,
                {"type": 1, "oid": aid, "mode": 3, "next": 0},
            )
    except Exception as exc:  # noqa: BLE001
        _log(f"热评获取失败 {material_id}: {type(exc).__name__}: {exc}")
        return _empty_comments(_friendly(exc))

    rows = comment_rows(data)
    if not rows:
        return _empty_comments("这个视频还没有热门评论")
    return {"comments": rows, "error": ""}


def _friendly(exc: Exception) -> str:
    """错误文案要可执行：风控带倒计时，连接类说人话。"""

    text = str(exc)
    if "风控" in text or "-412" in text:
        return text  # 已经带倒计时，原样透出
    name = type(exc).__name__
    if "Connect" in name or "Timeout" in name or "timeout" in name:
        return "网络连接失败，请检查网络后重试（不影响播放）"
    return f"获取失败：{name}: {text[:120]}"


# ── 路由侧挂载 ──────────────────────────────────────────────────


def install(router_module) -> bool:
    """Register heat endpoints on the video_learning router.

    两个端点都是 GET 且**永远 200**：失败走 body 里的 ``error`` 字段，
    由面板决定降级 —— 装饰性功能不配用 4xx/5xx 惊动用户。
    """

    global _INSTALLED
    if _INSTALLED:
        return False

    router = getattr(router_module, "router", None)
    if router is None:
        return False

    from fastapi.responses import JSONResponse

    @router.get("/bilibili/materials/{material_id}/heat")
    async def bilibili_heat(material_id: str) -> JSONResponse:
        """高能进度条：弹幕密度点串（按 cid 缓存）。"""

        payload = await fetch_heat(material_id)
        return JSONResponse(payload, headers={"Cache-Control": "no-store"})

    @router.get("/bilibili/materials/{material_id}/hot-comments")
    async def bilibili_hot_comments(material_id: str) -> JSONResponse:
        """热评：评论区高赞前 N 条。"""

        payload = await fetch_comments(material_id)
        return JSONResponse(payload, headers={"Cache-Control": "no-store"})

    _INSTALLED = True
    _log("高能进度条/热评已挂载：/bilibili/materials/{id}/heat、/hot-comments")
    return True


def try_install(module_name: str = "deeptutor.api.routers.video_learning") -> bool:
    try:
        import importlib

        module = importlib.import_module(module_name)
    except Exception as exc:  # noqa: BLE001
        _log(f"未找到 {module_name}，高能条/热评本次不挂载（不影响其他功能）: {exc}")
        return False
    try:
        return install(module)
    except Exception as exc:  # noqa: BLE001
        _log(f"挂载高能条/热评失败（不影响其他功能）: {type(exc).__name__}: {exc}")
        return False
