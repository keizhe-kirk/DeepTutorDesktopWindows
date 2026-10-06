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

# 前端目前没有入口（实测）

前端 chunk 里只有 ``invidious/browse/{kind}``（invidious 自己的搜索），**没有**
任何 B 站搜索 UI。所以这个端点当前是「能力已就绪、界面未接」的���态：
命令行/脚本可直接调,前端一旦加上搜索框就能直接用,不必再动后端。
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

    _ROUTED = True
    _log("已挂载 B 站搜索端点:/bilibili/search、/bilibili/login-status")
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