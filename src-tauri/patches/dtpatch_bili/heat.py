"""热评（任务 #4）：B 站视频评论区高赞前 N 条。

# 是什么

沉浸式观看 B 站视频时，「热评」面板列出评论区的高赞评论，并可一键
把高赞当语料喂给 AI 陪看（companion 的 context 字段）。

# 接口

``x/v2/reply/main``：``type=1``（视频评论）+ ``oid=aid`` +
``mode=3``（热门排序），一页拿齐。老材料没存 aid 时打一次
``/x/web-interface/view`` 补（走共享节流/熔断）。

# 风控（与搜索/解析同一套结论，勿重复造）

- 熔断器 / 节流器**共用** ``search.breaker`` / ``search._throttle``：
  B 站风控按客户端整体速率算，所有出口必须共享同一块表；
- ``trust_env=False`` 永远直连（2026-10-12 实测：走系统代理会撞失效
  端口 ConnectError，出口 IP 飘海外还加码 -412）；
- 命中 -412 **立即熔断不重试**（ban 窗口约 9 分钟，重试只是延长封禁）。

# 失败策略

热评是**装饰性**功能：任何失败（风控 / 网络 / 非 B 站材料）都返回
``{"error": ...}`` 由面板决定降级 —— 绝不 500、绝不影响播放。

> 注：v0.2.22 移除了同文件里的「高能进度条」（弹幕密度 /heat 端点）——
> 前端入口重锚后不再使用，弹幕相关代码（list.so XML 解析 / 密度分桶 /
> cid 缓存）一并删除，只留热评。
"""

from __future__ import annotations

from typing import Any

#: 热评最多列多少条（面板一屏放得下，又不至于把评论区搬空）。
MAX_COMMENTS = 12

#: 单次请求超时。
REQUEST_TIMEOUT = 25.0

#: 评论区接口路径（走 ``_api_call`` 拼 ``api.bilibili.com`` 前缀）。
#: ``type=1`` = 视频评论，``oid=aid``，``mode=3`` = 热门排序。
_REPLY_PATH = "/x/v2/reply/main"

_INSTALLED = False


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


async def _view_identifiers(client: Any, bvid: str, page: int) -> tuple[int, int]:
    """老材料没有 aid/cid 时打一次 view 补上。返回 ``(aid, cid)``。"""

    from . import _api_call

    view = await _api_call(client, "/x/web-interface/view", {"bvid": bvid})
    pages = view.get("pages") if isinstance(view.get("pages"), list) else []
    index = min(max(page, 1), len(pages)) - 1 if pages else 0
    row = pages[index] if pages and isinstance(pages[index], dict) else {}
    return int(view.get("aid") or 0), int(row.get("cid") or 0)


# ── 对外：热评 fetch ────────────────────────────────────────────


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
    """Register the hot-comments endpoint on the video_learning router.

    端点是 GET 且**永远 200**：失败走 body 里的 ``error`` 字段，由面板
    决定降级 —— 装饰性功能不配用 4xx/5xx 惊动用户。
    """

    global _INSTALLED
    if _INSTALLED:
        return False

    router = getattr(router_module, "router", None)
    if router is None:
        return False

    from fastapi.responses import JSONResponse

    @router.get("/bilibili/materials/{material_id}/hot-comments")
    async def bilibili_hot_comments(material_id: str) -> JSONResponse:
        """热评：评论区高赞前 N 条。"""

        payload = await fetch_comments(material_id)
        return JSONResponse(payload, headers={"Cache-Control": "no-store"})

    _INSTALLED = True
    _log("热评已挂载：/bilibili/materials/{id}/hot-comments")
    return True


def try_install(module_name: str = "deeptutor.api.routers.video_learning") -> bool:
    try:
        import importlib

        module = importlib.import_module(module_name)
    except Exception as exc:  # noqa: BLE001
        _log(f"未找到 {module_name}，热评本次不挂载（不影响其他功能）: {exc}")
        return False
    try:
        return install(module)
    except Exception as exc:  # noqa: BLE001
        _log(f"挂载热评失败（不影响其他功能）: {type(exc).__name__}: {exc}")
        return False
