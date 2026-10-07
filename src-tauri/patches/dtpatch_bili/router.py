"""给上游 API router 挂 B 站搜索端点。

# 为什么要单独一个模块

``apply_patch``只改``deeptutor.video_learning.service`` —— 那是**纯函数层**,
没有 FastAPI 依赖。而搜索要能被前端/脚本调用,必须落到 HTTP 上,所以这里额外
往 ``deeptutor.api.routers.video_learning.router`` 上注册一个路由。

# 为什么挂在已有 router 上而不是新建 app

新起一个 ``FastAPI()`` 是没用的 —— Uvicorn 只跑``deeptutor.api.app`` 里那一个
app,后注册的 app 根本不会被serve。必须挂到已经被 ``include_router`` 的那个
router 上（顺带自动继承前缀与鉴权依赖）。

★ 时序：钩子必须赶在 ``app.include_router(...)`` **之前**改写。router 模块是
懒加载的,而 ``include_router`` 会遍历 ``router.routes`` 建路由表 —— 我们在
模块 import 完成后立刻注册，就一定早于它被 include。

# 接入沉浸式观看（2026-10-07 重做）

原先只挂了一个 ``/bilibili/search`` 端点，前端没有入口，等于「能力就绪、界面未接」。
现在接入方式改了：**不另开窗口**，而是让 B 站走``reading`` 目录 + 沉浸式观看页
**已有的** B 站播放器（chunk 4057 里``"bilibili"===e.source_kind`` 那个分支）。

因此这个模块除了挂搜索端点，还要改两处已有路由：

* ``POST /materials/resolve`` —— 上游把 ``provider_override`` 硬编码成
  ``{None, "youtube", "invidious"}``，B 站会被拒；
* ``POST /materials/{id}/transcript/refresh`` —— 上游用
  ``VIDEO_ID_RE``（YouTube 的 11 位 id）校验，BV 号匹配不上就抛 not found。

★ 改法都是替换 ``APIRoute.endpoint``，不是改模块属性 —— ``@router.post`` 在
模块 import 时就把函数存进路由表了，之后改模块名下的属性不生效（踩过）。
"""

from __future__ import annotations

import asyncio
from typing import Any

#: 已挂载过就返回 False（幂等）。
_ROUTED = False


def _log(msg: str) -> None:
    import sys

    print(f"[dtpatch.bili] {msg}", file=sys.stderr, flush=True)


def _wrap_search(func):
    """把 ``search()`` 包成端点：不让风控异常冒到 5xx。"""

    async def endpoint(term: str, page: int) -> dict[str, Any]:
        if not term:
            return {"results": [], "error": "", "keyword": "", "page": page, "total": 0}
        try:
            page = max(1, min(int(page or 1), 50))
        except (TypeError, ValueError):
            page = 1
        try:
            return await func(term, page=page)
        except Exception as exc:  # noqa: BLE001 - 搜索是可选增强
            _log(f"搜索端点异常: {type(exc).__name__}: {exc}")
            return {
                "results": [],
                "error": f"搜索失败：{type(exc).__name__}",
                "keyword": term,
                "page": page,
                "total": 0,
            }

    return endpoint


def _find_route(router, suffix: str):
    for route in getattr(router, "routes", []) or []:
        path = str(getattr(route, "path", "") or "")
        if path.endswith(suffix) and hasattr(route, "endpoint"):
            return route
    return None


def _relax_resolve_route(router) -> bool:
    """让 ``POST /materials/resolve`` 接受 ``provider_override="bilibili"``。

    ★ 为什么必须换 ``route.endpoint``，而不是给 ``router_module.resolve_video``
    重新赋值：``@router.post(...)`` 在**模块 import 时**就把函数对象存进
    ``APIRoute`` 了，之后改模块属性不会影响已注册的路由表（踩过）。

    上游那里是硬编码白名单 ``{None, "youtube", "invidious"}``，直接抛
    ``Unsupported provider override``。我们不放开白名单本身，只在值是
    ``bilibili`` 时把它抹成 ``None`` —— provider 的实际选择由
    ``service.resolve_material`` 按 URL 嗅探决定，本来就不看这个参数。
    """

    route = _find_route(router, "/materials/resolve")
    if route is None:
        _log("没找到 /materials/resolve 路由，跳过白名单放开")
        return False

    original = route.endpoint

    async def endpoint(payload, *args, **kwargs):
        override = getattr(payload, "provider_override", None)
        if override == "bilibili":
            copier = getattr(payload, "model_copy", None)
            if callable(copier):
                payload = copier(update={"provider_override": None})
            else:  # 老 pydantic 兜底
                try:
                    payload.provider_override = None
                except Exception:  # noqa: BLE001
                    pass
        return await original(payload, *args, **kwargs)

    route.endpoint = endpoint
    _log("已放开 resolve 的 provider 白名单（bilibili 交给 URL 嗅探）")
    return True


def _patch_transcript_refresh(router) -> bool:
    """让 ``POST /materials/{id}/transcript/refresh`` 对 B 站材料重新抓字幕。

    上游那个 ``refresh_invidious_transcript`` 第一步就
    ``VIDEO_ID_RE.fullmatch(video_id)`` —— BV 号匹配不上，直接抛
    ``TimedMediaNotFound``。这里在进原逻辑之前先拦一道：B 站材料重新走一遍
    我们的解析（字幕可能刚补上登录态），其余交给原函数。
    """

    route = _find_route(router, "/transcript/refresh")
    if route is None:
        _log("没找到 /transcript/refresh 路由，跳过字幕刷新补丁")
        return False

    original = route.endpoint

    async def endpoint(material_id: str, *args, **kwargs):
        from deeptutor.video_learning import service as vl

        try:
            stored = vl.get_timed_media_store().get(material_id)
        except Exception:  # noqa: BLE001 - 交给原函数报原本的错
            stored = {}
        source = stored.get("source") if isinstance(stored.get("source"), dict) else {}
        if str(source.get("provider") or "") == "bilibili":
            from . import resolve_bilibili_url

            return await resolve_bilibili_url(material_id, str(source.get("url") or ""))
        return await original(material_id, *args, **kwargs)

    route.endpoint = endpoint
    _log("已接管 /transcript/refresh 的 B 站分支")
    return True


def install(router_module) -> bool:
    """Register the search route on ``router_module``. Idempotent.

    Returns ``True`` when the route was added by this call.
    """

    global _ROUTED
    if _ROUTED:
        return False

    router = getattr(router_module, "router", None)
    if router is None:
        return False

    from fastapi import Query
    from fastapi.responses import JSONResponse

    from .search import search

    endpoint = _wrap_search(search)

    @router.get("/bilibili/search")
    async def bilibili_search(
        q: str = Query(default="", max_length=100),
        page: int = Query(default=1, ge=1, le=50),
    ) -> JSONResponse:
        # 端点只做「收参 + 兜错」,真正的逻辑在 search.py —— 与登录流程一样,
        # Rust 侧将来也能直接复用同一个函数。
        term = (q or "").strip()[:100]
        payload = await endpoint(term, page)
        return JSONResponse(payload, headers={"Cache-Control": "no-store"})

    # 登录状态一并返回：前端可以据此提示「登录后有字幕」。
    @router.get("/bilibili/login-status")
    async def bilibili_login_status() -> JSONResponse:
        from . import credentials as cred

        try:
            check = await cred.check_valid()
            payload = {
                "logged_in": bool(cred.is_logged_in()),
                "valid": str(check.get("ok", "unknown")),
                "detail": str(check.get("detail", "")),
            }
        except Exception as exc:  # noqa: BLE001
            payload = {"logged_in": False, "valid": "error", "detail": type(exc).__name__}
        return JSONResponse(payload, headers={"Cache-Control": "no-store"})

    # ---- B 站进入「沉浸式观看」原生链路所需的路由侧改动 -------------------
    # 顺序有讲究：先换 endpoint（要在路由表建好之后），再挂新端点。
    _relax_resolve_route(router)
    _patch_transcript_refresh(router)

    _ROUTED = True
    _log(
        "已挂载 B 站端点：/bilibili/search、/bilibili/login-status；"
        "并已放开 resolve 白名单 + transcript/refresh 的 B 站分支"
    )
    return True


def try_install(module_name: str = "deeptutor.api.routers.video_learning") -> bool:
    """Import ``module_name`` and install the route. Never raises."""

    try:
        import importlib

        module = importlib.import_module(module_name)
    except Exception as exc:  # noqa: BLE001
        _log(f"未找到 {module_name}，搜索端点本次不挂载（不影响其他功能）: {exc}")
        return False
    try:
        return install(module)
    except Exception as exc:  # noqa: BLE001
        _log(f"挂载搜索端点失败（不影响其他功能）: {type(exc).__name__}: {exc}")
        return False