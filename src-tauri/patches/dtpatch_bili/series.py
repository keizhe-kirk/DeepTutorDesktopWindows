"""合集/分P 追更 + 课程进度（任务 #6）。

# 是什么

沉浸式观看页多一个「课程」面板：把当前视频所在的**分P清单**或**合集剧集**
列出来，每集带时长和「看到哪」的进度；点某一集即导入并切过去。分P 已支持
播放（loader 按 ``?p=N`` 选 cid），缺的是课程化结构 —— 这个模块补上。

# 数据从哪来

``view`` 接口一次返回两套结构（实测）：

- ``pages``：分P清单，``[{page, part, cid, duration}]``。教程类视频基本都是
  这个形态（实测搜索「Python入门教程」前 5 个结果 37~200 个分P）；
- ``ugc_season``：合集，``sections→episodes`` 两层。episode 顶层
  ``duration`` 实测是 ``None``，真实时长在 ``episode["page"]["duration"]``。

导入时 loader 已把两者（经 :data:`dtpatch_bili.__init__._extract_pages` /
``_extract_season`` 扁平化）写进 ``provider_cache`` 的
``bilibili_pages`` / ``bilibili_season``；更早导入的材料没有这两个字段，
本模块打一次 view 补取，并**顺手写回 provider_cache**（老材料自愈，只补一次）。

# 风控（与搜索/解析/弹幕同一套结论，勿重复造）

- view 走包内 ``_api_call``：``trust_env=False`` 强制直连 + 全局节流 +
  ``-412`` 熔断（按客户端整体速率算，所有出口共享同一块表）；
- 结果按 bvid 缓存 :data:`CACHE_TTL` 秒，来回切不重复请求。

# 失败策略

课程面板是**结构性**功能：任何失败（风控 / 网络 / 非 B 站材料）都返回
``{"kind": "none", "error": ...}``，永远 200 —— 面板显示「暂无课程信息」，
绝不 4xx/5xx 惊动用户，绝不影响播放。
"""

from __future__ import annotations

import time
from typing import Any

#: 课程结构缓存 TTL（秒）。分P/合集基本不会变（UP 主改结构是少数），
#: 缓存 10 分钟足够避免「切出去再回来」的重复请求。
CACHE_TTL = 600.0

#: 缓存条数上限，防长会话里无限增长（超了丢最旧的一条）。
CACHE_MAX_ENTRIES = 32

#: 单次请求超时。view 响应几百 KB，给足。
REQUEST_TIMEOUT = 20.0

_INSTALLED = False

#: bvid -> (写入时刻, (pages, season))。进程级缓存，重启即丢（多打一次
#: 而已）。**缓存的是结构不是 payload** —— payload 每帧都要按当前 P 重建
#: （高亮「看到哪」），结构才是不变量。
_CACHE: dict[str, tuple[float, tuple[list[dict[str, Any]], dict[str, Any] | None]]] = {}


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
    print(f"[dtpatch.series] {text}", file=sys.stderr, flush=True)


# ── 材料读取 ────────────────────────────────────────────────────


def _material(material_id: str) -> dict[str, Any]:
    """读 timed_media 材料（整份 dict）。"""

    from deeptutor.video_learning import service as vl

    return vl.get_timed_media_store().get(material_id)


def _int(value: Any) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


def _rows(value: Any) -> list[dict[str, Any]]:
    """过滤出 dict 行（坏行进垃圾出，与包内其他解析一致）。"""

    if not isinstance(value, list):
        return []
    return [row for row in value if isinstance(row, dict)]


def _course_from_cache(cache: dict[str, Any]) -> tuple[str, list[dict[str, Any]], dict[str, Any] | None]:
    """从 provider_cache 取分P/合集。返回 ``(kind, pages, season)``。"""

    pages = _rows(cache.get("bilibili_pages"))
    season = cache.get("bilibili_season")
    if not isinstance(season, dict) or not _rows(season.get("episodes")):
        season = None
    return ("pages" if pages else ("season" if season else "none")), pages, season


def _persist_course(material_id: str, pages: list[dict[str, Any]], season: dict[str, Any] | None) -> None:
    """把补取到的课程结构写回 provider_cache（老材料自愈，只补一次）。

    写失败无所谓（下次面板再打一次 view），所以整体 try 住。
    """

    try:
        from deeptutor.video_learning import service as vl

        store = vl.get_timed_media_store()
        with store.lock(material_id):
            try:
                material = store.get(material_id, lock_held=True)
            except Exception:  # noqa: BLE001 - 取不到就不写
                return
            cache = material.get("provider_cache")
            if not isinstance(cache, dict):
                cache = {}
                material["provider_cache"] = cache
            cache["bilibili_pages"] = pages
            cache["bilibili_season"] = season
            store.save(material)
        _log("已把课程结构写回材料 %s（pages=%d season=%s）", material_id, len(pages), bool(season))
    except Exception as exc:  # noqa: BLE001 - 自愈失败不能影响主流程
        _log("写回课程结构失败 %s: %s: %s", material_id, type(exc).__name__, exc)


# ── 网络补取 ────────────────────────────────────────────────────


async def _fetch_view_course(bvid: str) -> tuple[list[dict[str, Any]], dict[str, Any] | None]:
    """打一次 view 拿分P/合集（走 _api_call：直连 + 节流 + 熔断）。"""

    import httpx

    from . import _UA, _api_call
    from . import credentials as cred
    from .__init__ import _extract_pages, _extract_season

    async with httpx.AsyncClient(
        timeout=REQUEST_TIMEOUT,
        follow_redirects=True,
        headers={
            "Accept": "application/json, */*",
            "Referer": "https://www.bilibili.com/",
            "User-Agent": _UA,
        },
        cookies=dict(cred.load_cookies()) or None,
        trust_env=False,
    ) as client:
        view = await _api_call(client, "/x/web-interface/view", {"bvid": bvid})

    pages = _extract_pages(view.get("pages") if isinstance(view.get("pages"), list) else [])
    season = _extract_season(view.get("ugc_season"))
    return pages, season


# ── 对外主入口 ──────────────────────────────────────────────────


def build_payload(
    kind: str,
    title: str,
    bvid: str,
    current_page: int,
    pages: list[dict[str, Any]],
    season: dict[str, Any] | None,
    cached: bool,
) -> dict[str, Any]:
    """组装面板 payload。

    ``items`` 每项带 ``url``（切换时直接喂 ``/library/import-urls``）：
    分P 用 ``?p=N``（上游解析器认这个参数，实测），合集 episode 用它自己的
    bvid。``key`` 是前端认当前项的稳定标识（``p{N}`` / bvid）。
    """

    items: list[dict[str, Any]] = []
    if kind == "pages":
        for row in pages:
            page = _int(row.get("page"))
            if page <= 0:
                continue
            items.append(
                {
                    "key": f"p{page}",
                    "page": page,
                    "title": str(row.get("title") or f"P{page}"),
                    "duration": float(row.get("duration") or 0),
                    "url": f"https://www.bilibili.com/video/{bvid}?p={page}",
                }
            )
        current = f"p{max(current_page, 1)}"
    elif kind == "season" and season:
        for ep in _rows(season.get("episodes")):
            ep_bvid = str(ep.get("bvid") or "")
            if not ep_bvid:
                continue
            items.append(
                {
                    "key": ep_bvid,
                    "bvid": ep_bvid,
                    "title": str(ep.get("title") or ep_bvid),
                    "duration": float(ep.get("duration") or 0),
                    "url": f"https://www.bilibili.com/video/{ep_bvid}",
                }
            )
        current = bvid
    else:
        current = ""

    return {
        "kind": kind,
        "title": title,
        "bvid": bvid,
        "current": current,
        "items": items,
        "cached": cached,
        "error": "",
    }


def _empty(error: str) -> dict[str, Any]:
    return {"kind": "none", "title": "", "bvid": "", "current": "", "items": [], "error": error}


async def fetch_series(material_id: str) -> dict[str, Any]:
    """课程结构（分P/合集）。失败返回 ``_empty(error)``，绝不抛。"""

    try:
        material = _material(material_id)
    except Exception as exc:  # noqa: BLE001 - 材料不存在/损坏都是降级
        return _empty(f"材料读取失败：{type(exc).__name__}")

    source = material.get("source") if isinstance(material.get("source"), dict) else {}
    cache = material.get("provider_cache") if isinstance(material.get("provider_cache"), dict) else {}
    metadata = material.get("metadata") if isinstance(material.get("metadata"), dict) else {}

    bvid = str(source.get("bvid") or source.get("video_id") or "")
    if not bvid:
        return _empty("这个材料不是 B 站视频")
    page = _int(source.get("page")) or 1
    title = str(metadata.get("title") or "")

    kind, pages, season = _course_from_cache(cache)
    cached = kind != "none"
    if kind == "none":
        # 老材料：先看进程缓存（provider_cache 写回失败时的兜底，10 分钟内
        # 不重复打接口），没有才打 view 补取并写回（下次面板直接读本地）。
        row = _cache_get(bvid)
        if row is not None:
            pages, season = row
            kind = "pages" if pages else ("season" if season else "none")
            cached = kind != "none"
        else:
            try:
                pages, season = await _fetch_view_course(bvid)
            except Exception as exc:  # noqa: BLE001 - 装饰性功能，一律降级
                _log(f"课程结构补取失败 {material_id}: {type(exc).__name__}: {exc}")
                return _empty(_friendly(exc))
            kind = "pages" if pages else ("season" if season else "none")
            if kind != "none":
                _cache_put(bvid, (pages, season))
                _persist_course(material_id, pages, season)

    if kind == "none":
        return _empty("没有找到分P或合集信息")

    payload = build_payload(kind, title, bvid, page, pages, season, cached)

    # 合集形态标题用合集名更有信息量；分P 用视频标题。
    if kind == "season" and season and str(season.get("title") or ""):
        payload["title"] = str(season.get("title"))

    return payload


def _friendly(exc: Exception) -> str:
    """网络/风控异常 → 可读文案（带可执行倒计时，别引导用户狂点）。"""

    text = str(exc)
    name = type(exc).__name__
    if "风控" in text or "-412" in text:
        return text
    lowered = text.lower()
    if "timeout" in lowered or "timed out" in lowered:
        return "B 站接口响应超时，请稍后再试"
    if "connect" in lowered or "network" in lowered:
        return "连不上 B 站（已强制直连，请检查网络）"
    return f"课程信息获取失败：{name}: {text[:160]}"


# ── 缓存 ────────────────────────────────────────────────────────


def _cache_get(bvid: str) -> tuple[list[dict[str, Any]], dict[str, Any] | None] | None:
    row = _CACHE.get(bvid)
    if row is None:
        return None
    written, value = row
    if time.monotonic() - written > CACHE_TTL:
        _CACHE.pop(bvid, None)
        return None
    return value


def _cache_put(bvid: str, value: tuple[list[dict[str, Any]], dict[str, Any] | None]) -> None:
    if len(_CACHE) >= CACHE_MAX_ENTRIES:
        # 丢最旧的一条（插入序即写入序）
        oldest = next(iter(_CACHE))
        _CACHE.pop(oldest, None)
    _CACHE[bvid] = (time.monotonic(), value)


# ── 挂载 ────────────────────────────────────────────────────────


def install(router_module) -> bool:
    """在 video_learning router 上注册课程结构端点。

    端点 GET 且**永远 200**：失败走 body 里的 ``error``，由面板决定降级。
    """

    global _INSTALLED
    if _INSTALLED:
        return False

    router = getattr(router_module, "router", None)
    if router is None:
        return False

    from fastapi.responses import JSONResponse

    @router.get("/bilibili/materials/{material_id}/series")
    async def bilibili_series(material_id: str) -> JSONResponse:
        """课程结构：分P清单 / 合集剧集（带切换用 URL）。"""

        payload = await fetch_series(material_id)
        return JSONResponse(payload, headers={"Cache-Control": "no-store"})

    _INSTALLED = True
    _log("课程结构已挂载：/bilibili/materials/{id}/series")
    return True


def try_install(module_name: str = "deeptutor.api.routers.video_learning") -> bool:
    try:
        import importlib

        module = importlib.import_module(module_name)
    except Exception as exc:  # noqa: BLE001
        _log(f"未找到 {module_name}，课程结构本次不挂载（不影响其他功能）: {exc}")
        return False
    try:
        return install(module)
    except Exception as exc:  # noqa: BLE001
        _log(f"挂载课程结构失败（不影响其他功能）: {type(exc).__name__}: {exc}")
        return False
