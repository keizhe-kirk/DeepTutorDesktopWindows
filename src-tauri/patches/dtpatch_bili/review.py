"""视频学习标记 → 复习卡闭环（任务 #1）。

# 这个模块补上断掉的那一环

``video_learning`` 的骨架早就齐了：播放、时间戳字幕、marks、notes、进度。
``practice`` / ``question_notebook`` 也早就有了完整的间隔复习体系
（``notebook_entries`` + ``PracticeStore``）。**但两者从来没接通过** ——
用户在视频里打的标记（``key_point`` / ``question`` / ``review``）只躺在
``material["learning"]["marks"]`` 里，复习系统完全看不到。

这个模块把标记自动转成复习卡：

    mark ──► notebook_entry（题库）──► PracticeStore 队列 ──► /api/practice/queue

# 为什么写进 ``notebook_entries`` 而不是自己存一套

- ``PracticeStore.queue`` 的 SQL 是 ``LEFT JOIN practice_review_state``，
  没有复习状态行的条目 ``COALESCE(r.due_at, n.created_at)`` —— **建条目即
  自动进入待复习队列**，不用额外登记。
- 去重键是 ``(origin_type, origin_ref, turn_id, question_id)``，
  ``upsert`` 是幂等的：重复同步只会更新，不会产生重复卡。
- 前端「练习 / 题库」页面读的就是这些条目，卡片自动出现在那里，
  **不需要动任何前端产物**。

# 关键取值（都是上游契约，别改）

- ``origin_type="external_import"`` + ``origin_ref=material_id``
  （external_import 强制要求 origin_ref 非空；session_id 留空，
  ``is_visible`` 允许 ``session_id IS NULL``）。
- ``question_id="vmark:{mark_id}"`` —— 稳定 ID，重同步走更新分支。
- ``source="immersive_reading"`` / ``assessment_type="review"`` /
  ``result="ungraded"`` / ``is_correct=0`` → ``resolved=0``，
  卡片保持「待复习」而不是被判定为已答对。
- 删除标记时**必须显式删卡**：upsert 永不删除，否则删了标记卡还在。

# 失败策略

同步失败只打一行 stderr，**绝不影响标记接口本身** —— 标记是主功能，
复习卡是增强。自动同步可在设置里关掉（默认开）。
"""

from __future__ import annotations

import asyncio
import json
import os
import time
from pathlib import Path
from typing import Any

#: 条目来源标识前缀，同时用于列出卡片时过滤（见 ``list_cards``）。
CARD_PREFIX = "vmark:"

#: 与上游 ``QuestionOriginType`` 对齐。
ORIGIN_TYPE = "external_import"

#: 与上游 ``AssessmentSource`` 对齐。视频标记是「沉浸模式」的产物，
#: 取最接近的沉浸式来源（纯展示分组，无功能影响）。
CARD_SOURCE = "immersive_reading"

#: 标记类型 → 卡片正面的问法。key_point/review 没有明确问题，
#: 用「回忆某时刻的内容」这种克劳泽式（cloze）正面。
_KIND_HINT = {
    "key_point": "回忆这个知识点",
    "question": "回答当时的问题",
    "review": "复习这个片段",
}

_INSTALLED = False


def _log(msg: str) -> None:
    import sys

    print(f"[dtpatch.review] {msg}", file=sys.stderr, flush=True)


# ── 设置（自动同步开关） ────────────────────────────────────────


def settings_path() -> Path:
    """``%LOCALAPPDATA%/DeepTutor/patches/review_settings.json``。

    与 ``credentials.py`` 同目录 —— 补丁数据都归这儿，Rust 侧播种时
    也会覆盖到同一个根。
    """

    local = os.environ.get("LOCALAPPDATA") or os.path.expanduser("~")
    return Path(local) / "DeepTutor" / "patches" / "review_settings.json"


def load_settings() -> dict[str, Any]:
    try:
        raw = settings_path().read_text(encoding="utf-8")
        data = json.loads(raw)
        return data if isinstance(data, dict) else {}
    except Exception:  # noqa: BLE001 - 没有设置文件就是默认值
        return {}


def save_settings(payload: dict[str, Any]) -> dict[str, Any]:
    merged = {"auto_sync": True}
    merged.update({k: v for k, v in (payload or {}).items() if k in merged})
    path = settings_path()
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            json.dumps(merged, ensure_ascii=False, indent=2), encoding="utf-8"
        )
    except Exception as exc:  # noqa: BLE001 - 设置写不进去不该炸接口
        _log(f"设置写入失败: {type(exc).__name__}: {exc}")
    return merged


# ── 卡片构造 ────────────────────────────────────────────────────


def _mmss(seconds: float) -> str:
    total = max(0, int(seconds or 0))
    hours, remainder = divmod(total, 3600)
    minutes, secs = divmod(remainder, 60)
    if hours:
        return f"{hours}:{minutes:02d}:{secs:02d}"
    return f"{minutes:02d}:{secs:02d}"


def _clip(value: Any, limit: int = 600) -> str:
    text = " ".join(str(value or "").split())
    return text[:limit]


def _material_title(material: dict[str, Any]) -> str:
    metadata = material.get("metadata") if isinstance(material.get("metadata"), dict) else {}
    return _clip(metadata.get("title") or "视频", 120) or "视频"


def card_for_mark(material: dict[str, Any], mark: dict[str, Any]) -> dict[str, Any] | None:
    """把一个标记转成 ``notebook_entries`` 条目；没有可复习内容时返回 None。

    正面（question）= 用户自己的笔记，没有就退化成「回忆某时刻」；
    背面（explanation）= 视频出处 + 当时的字幕原文。
    这样「我记的要点 → 原话」正好构成一张回忆卡。
    """

    quote = _clip(mark.get("quote"))
    note = _clip(mark.get("note"))
    if not quote and not note:
        return None
    mark_id = str(mark.get("mark_id") or "").strip()
    if not mark_id:
        return None

    material_id = str(material.get("material_id") or "").strip()
    if not material_id:
        return None

    start = float(mark.get("start_seconds") or 0)
    stamp = _mmss(start)
    title = _material_title(material)
    kind = str(mark.get("kind") or "review")

    if note:
        question = note
    else:
        hint = _KIND_HINT.get(kind, "回忆这个片段")
        question = f"{hint}（{stamp}）"

    explanation = f"来自《{title}》{stamp}\n{quote}" if quote else f"来自《{title}》{stamp}"

    return {
        "session_id": None,
        "origin_type": ORIGIN_TYPE,
        "origin_ref": material_id,
        "turn_id": "",
        "question_id": f"{CARD_PREFIX}{mark_id}",
        "question": question,
        "question_type": "",
        "options": {},
        "correct_answer": "",
        "explanation": explanation,
        "difficulty": "",
        "user_answer": "",
        "source": CARD_SOURCE,
        "material_id": material_id,
        "material_title": title,
        "section_id": "",
        "section_title": stamp,
        "assessment_type": "review",
        "result": "ungraded",
        "attempt_count": 1,
        "hints_used": 0,
        "is_correct": False,
    }


def _store():
    from deeptutor.services.session.sqlite_store import get_sqlite_session_store

    return get_sqlite_session_store()


def _load_material(material_id: str) -> dict[str, Any]:
    from deeptutor.video_learning import service as vl

    return vl.get_timed_media_store().get(material_id)


def _marks_of(material: dict[str, Any]) -> list[dict[str, Any]]:
    learning = material.get("learning") if isinstance(material.get("learning"), dict) else {}
    rows = learning.get("marks")
    return [row for row in rows if isinstance(row, dict)] if isinstance(rows, list) else []


async def sync_material(material_id: str) -> dict[str, Any]:
    """把一个材料的全部标记同步成复习卡（幂等）。"""

    return await _sync_material_obj(_load_material(material_id))


async def _sync_material_obj(material: dict[str, Any]) -> dict[str, Any]:
    material_id = str(material.get("material_id") or "").strip()
    marks = _marks_of(material)
    items = [
        card
        for card in (card_for_mark(material, mark) for mark in marks)
        if card is not None
    ]
    synced = 0
    if items:
        synced = await _store().upsert_notebook_entries(None, items)
    return {"material_id": material_id, "marks": len(marks), "synced": synced}


def _store_root():
    """``TimedMediaStore`` 的材料目录（补建时枚举用）。"""

    from deeptutor.video_learning import service as vl

    return vl.get_timed_media_store().root


async def backfill_all(limit: int = 500) -> dict[str, Any]:
    """把**历史**材料里的标记一次性补建成复习卡。

    自动同步只对「本次会话里新建/修改的标记」生效；升级前打的老标记要靠
    这个补一次。幂等，重复跑只会 update 不会重复建卡。
    """

    root = _store_root()
    result = {"materials": 0, "synced": 0, "failed": 0}
    for path in sorted(root.glob("*.json"))[:limit]:
        try:
            material = json.loads(path.read_text(encoding="utf-8"))
            if not isinstance(material, dict) or material.get("type") != "timed_media":
                continue
            outcome = await _sync_material_obj(material)
            result["materials"] += 1
            result["synced"] += int(outcome.get("synced") or 0)
        except Exception as exc:  # noqa: BLE001 - 单个材料坏掉不拖垮整批
            result["failed"] += 1
            _log(f"补建跳过 {path.name}: {type(exc).__name__}: {exc}")
    return result


#: 每个进程只补一次；失败不置位（下次触发再试）。
_BACKFILL_TRIED = False


def _kick_backfill_once() -> None:
    """在后台任务里跑一次历史补建（绝不阻塞当前请求）。"""

    global _BACKFILL_TRIED
    if _BACKFILL_TRIED:
        return
    try:
        loop = asyncio.get_running_loop()
    except RuntimeError:
        return  # 没有运行中的事件循环（导入期），交给下次请求
    _BACKFILL_TRIED = True
    loop.create_task(_backfill_task())


async def _backfill_task() -> None:
    global _BACKFILL_TRIED
    try:
        result = await backfill_all()
        if result["materials"] or result["failed"]:
            _log(f"历史标记补建完成: {result}")
    except Exception as exc:  # noqa: BLE001
        _BACKFILL_TRIED = False  # 失败了下次再试
        _log(f"历史标记补建失败（不影响使用，可手动调 review-backfill）: {exc}")


async def drop_card(material_id: str, mark_id: str) -> bool:
    """删掉某个标记对应的复习卡（标记被删时调用）。"""

    entry = await _store().find_notebook_entry_by_origin(
        ORIGIN_TYPE, material_id, f"{CARD_PREFIX}{mark_id}"
    )
    if not entry:
        return False
    return bool(await _store().delete_notebook_entry(int(entry["id"])))


async def list_cards(material_id: str) -> list[dict[str, Any]]:
    """列出这个材料生成的复习卡（题库页面的同一批数据）。"""

    rows = await _store().list_notebook_entries(material_id=material_id, limit=200)
    items = rows.get("items") if isinstance(rows, dict) else rows
    items = items or []
    return [
        row
        for row in items
        if isinstance(row, dict)
        and str(row.get("origin_type") or "") == ORIGIN_TYPE
        and str(row.get("question_id") or "").startswith(CARD_PREFIX)
    ]


async def _maybe_sync(material_id: str) -> None:
    """自动同步入口：设置关了就跳过，失败只记录不抛出。"""

    try:
        if not load_settings().get("auto_sync", True):
            return
        result = await sync_material(material_id)
        _log(f"复习卡已同步: {result}")
    except Exception as exc:  # noqa: BLE001 - 卡片是增强，不能拖垮标记
        _log(f"复习卡同步失败（不影响标记本身）: {type(exc).__name__}: {exc}")


# ── 路由侧挂载 ──────────────────────────────────────────────────


def _find_route(router, suffix: str, method: str):
    """按路径后缀 + HTTP 方法找路由（GET/PATCH/DELETE 同路径要靠方法区分）。"""

    for route in getattr(router, "routes", []) or []:
        path = str(getattr(route, "path", "") or "")
        methods = getattr(route, "methods", None) or set()
        if path.endswith(suffix) and method in methods and hasattr(route, "endpoint"):
            return route
    return None


def _wrap_mark_routes(router_module, router) -> int:
    """给标记的增/改/删接口包一层「事后同步复习卡」。

    ★ 为什么必须换 ``route.endpoint``：``@router.post`` 在模块 import 时就把
    函数对象存进 ``APIRoute`` 了，之后改模块属性不生效（与 search 挂载同一
    踩坑结论）。body 参数必须**带类型注解**重新声明，否则 FastAPI 不知道该
    解析 JSON body。

    ★ 为什么用工厂函数而不是三个并列闭包：``original`` 是外层局部变量，
    并列闭包会**共享同一个单元格**（迟绑定）—— 后包的 DELETE 会把前面
    POST/PATCH 捕获的 original 一起覆盖掉，三个路由全去调最后一个处理函数
    （回归测试抓到过，别改回去）。
    """

    wrapped = 0

    def _make_create(original, model):
        async def create_endpoint(material_id: str, payload: model, *args: Any, **kwargs: Any):
            result = await original(material_id, payload, *args, **kwargs)
            await _maybe_sync(material_id)
            return result

        return create_endpoint

    def _make_patch(original, model):
        async def patch_endpoint(
            material_id: str, mark_id: str, payload: model, *args: Any, **kwargs: Any
        ):
            result = await original(material_id, mark_id, payload, *args, **kwargs)
            await _maybe_sync(material_id)
            return result

        return patch_endpoint

    def _make_delete(original):
        async def delete_endpoint(material_id: str, mark_id: str, *args: Any, **kwargs: Any):
            result = await original(material_id, mark_id, *args, **kwargs)
            try:
                await drop_card(material_id, mark_id)
            except Exception as exc:  # noqa: BLE001
                _log(f"删除复习卡失败（标记已删）: {type(exc).__name__}: {exc}")
            await _maybe_sync(material_id)
            return result

        return delete_endpoint

    # ---- POST /materials/{material_id}/marks --------------------------------
    route = _find_route(router, "/materials/{material_id}/marks", "POST")
    if route is not None:
        route.endpoint = _make_create(
            route.endpoint, getattr(router_module, "MarkCreateRequest", None)
        )
        wrapped += 1
    else:
        _log("没找到 POST /marks 路由，创建不同步卡片")

    # ---- PATCH /materials/{material_id}/marks/{mark_id} ---------------------
    route = _find_route(router, "/materials/{material_id}/marks/{mark_id}", "PATCH")
    if route is not None:
        route.endpoint = _make_patch(
            route.endpoint, getattr(router_module, "MarkPatchRequest", None)
        )
        wrapped += 1
    else:
        _log("没找到 PATCH /marks/{mark_id} 路由，更新不同步卡片")

    # ---- DELETE /materials/{material_id}/marks/{mark_id} --------------------
    # upsert 永不删除，所以标记删了必须显式删卡，再全量同步兜底。
    route = _find_route(router, "/materials/{material_id}/marks/{mark_id}", "DELETE")
    if route is not None:
        route.endpoint = _make_delete(route.endpoint)
        wrapped += 1
    else:
        _log("没找到 DELETE /marks/{mark_id} 路由，删除不联动删卡")

    return wrapped


def _wrap_material_get(router) -> bool:
    """给 ``GET /materials/{material_id}`` 包一层「首次打开触发历史补建」。

    打开任意一个已看过的视频都会走这条路由 —— 是「用户回来了」最自然的
    信号。补建在**后台任务**里跑，不拖慢本次响应；每进程只试一次。
    """

    route = _find_route(router, "/materials/{material_id}", "GET")
    if route is None:
        _log("没找到 GET /materials/{id} 路由，历史补建改由手动端点触发")
        return False

    original = route.endpoint

    async def get_endpoint(material_id: str, *args: Any, **kwargs: Any):
        result = await original(material_id, *args, **kwargs)
        try:
            _kick_backfill_once()
        except Exception as exc:  # noqa: BLE001 - 补建是增强，不能拖垮打开视频
            _log(f"触发历史补建失败: {type(exc).__name__}: {exc}")
        return result

    route.endpoint = get_endpoint
    return True


def install(router_module) -> bool:
    """Wrap mark routes and register review endpoints. Idempotent."""

    global _INSTALLED
    if _INSTALLED:
        return False

    router = getattr(router_module, "router", None)
    if router is None:
        return False

    from fastapi import Query
    from fastapi.responses import JSONResponse
    from pydantic import BaseModel

    class ReviewSettingsRequest(BaseModel):
        auto_sync: bool = True

    wrapped = _wrap_mark_routes(router_module, router)
    wrapped_get = _wrap_material_get(router)

    @router.get("/bilibili/materials/{material_id}/review-cards")
    async def bilibili_review_cards(material_id: str) -> JSONResponse:
        """列出这个视频的标记生成的复习卡。"""

        try:
            cards = await list_cards(material_id)
            return JSONResponse(
                {"material_id": material_id, "cards": cards, "total": len(cards)},
                headers={"Cache-Control": "no-store"},
            )
        except Exception as exc:  # noqa: BLE001
            return JSONResponse(
                {"material_id": material_id, "cards": [], "total": 0, "error": str(exc)},
                headers={"Cache-Control": "no-store"},
            )

    @router.post("/bilibili/materials/{material_id}/review-sync")
    async def bilibili_review_sync(material_id: str) -> JSONResponse:
        """手动全量同步（自动同步关闭时的前台按钮也能用）。"""

        try:
            result = await sync_material(material_id)
            return JSONResponse(result, headers={"Cache-Control": "no-store"})
        except Exception as exc:  # noqa: BLE001
            return JSONResponse(
                {"material_id": material_id, "error": str(exc)},
                status_code=400,
                headers={"Cache-Control": "no-store"},
            )

    @router.post("/bilibili/review-backfill")
    async def bilibili_review_backfill() -> JSONResponse:
        """手动补建：把升级前打的老标记一次性转成复习卡（幂等）。"""

        try:
            result = await backfill_all()
            return JSONResponse(result, headers={"Cache-Control": "no-store"})
        except Exception as exc:  # noqa: BLE001
            return JSONResponse(
                {"error": str(exc)}, status_code=400, headers={"Cache-Control": "no-store"}
            )

    @router.get("/bilibili/review-settings")
    async def bilibili_review_settings_get() -> JSONResponse:
        return JSONResponse(load_settings(), headers={"Cache-Control": "no-store"})

    @router.put("/bilibili/review-settings")
    async def bilibili_review_settings_put(payload: ReviewSettingsRequest) -> JSONResponse:
        return JSONResponse(
            save_settings(payload.model_dump()), headers={"Cache-Control": "no-store"}
        )

    _INSTALLED = True
    _log(
        f"复习卡闭环已挂载：包了 {wrapped} 个标记路由"
        f"（材料详情路由{'已' if wrapped_get else '未'}包，用于触发历史补建）；"
        "新增 /bilibili/materials/{id}/review-cards、/review-sync、"
        "/review-backfill、/review-settings"
    )
    return True


def try_install(module_name: str = "deeptutor.api.routers.video_learning") -> bool:
    """Import ``module_name`` and install. Never raises."""

    try:
        import importlib

        module = importlib.import_module(module_name)
    except Exception as exc:  # noqa: BLE001
        _log(f"未找到 {module_name}，复习卡本次不挂载（不影响其他功能）: {exc}")
        return False
    try:
        return install(module)
    except Exception as exc:  # noqa: BLE001
        _log(f"挂载复习卡失败（不影响其他功能）: {type(exc).__name__}: {exc}")
        return False
