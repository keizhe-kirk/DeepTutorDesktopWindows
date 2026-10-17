"""视频学习标记 → 复习卡闭环（任务 #1）。

# 这个模块补上断掉的那一环

``video_learning`` 的骨架早就齐了：播放、时间戳字幕、marks、notes、进度。
``practice`` / ``question_notebook`` 也早就有了完整的间隔复习体系
（``notebook_entries`` + ``PracticeStore``）。**但两者从来没接通过** ——
用户在视频里打的标记只躺在材料数据里，复习系统完全看不到。

这个模块把标记自动转成复习卡：

    标记 ──► notebook_entry（题库）──► PracticeStore 队列 ──► /api/practice/queue

# ★ 1.6.14 起标记走 reading annotations（2026-10-17 真机前端考古实锤）

播放页（chunk 1241 的 ``MediaReadingStage``）的 ``saveMark`` 来自
``ReadingProvider``（模块 15163），落点是
``PUT /api/reading/materials/{id}/annotations`` —— **不是** video_learning
的 ``/marks`` 路由（那套路由前端早就弃用了，``mark-suggestions`` 在前端
产物里 0 引用）。所以本模块同时包两套路由：

- **reading annotations**（主路径，``vann:`` 前缀）：``Annotation`` 的
  ``locator`` 就是字幕片段序号，秒数从 ``unit_references`` 的
  ``source_href="#t=123"`` 解析；
- **video_learning marks**（兜底路径，``vmark:`` 前缀）：万一别的客户端
  还在用老路由，行为不变。

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
- ``question_id="vann:{annotation_id}"`` / ``"vmark:{mark_id}"`` ——
  稳定 ID，重同步走更新分支。
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
import re
import time
from pathlib import Path
from typing import Any

#: video_learning marks 卡片前缀（兜底路径）。
CARD_PREFIX = "vmark:"

#: reading annotations 卡片前缀（1.6.14 主路径）。
ANNOTATION_PREFIX = "vann:"

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


# ── reading annotations（1.6.14 主路径） ──────────────────────────


def _field(obj: Any, name: str, default: Any = None) -> Any:
    """dataclass / dict 通吃取值（Annotation 是 dataclass，测试可能传 dict）。"""

    if isinstance(obj, dict):
        return obj.get(name, default)
    return getattr(obj, name, default)


def _reading_store():
    from deeptutor.reading.store import ReadingStore

    return ReadingStore()


def _locator_seconds(store: Any, material_id: str) -> dict[int, float]:
    """``locator → 起始秒数``。

    上游摄入 B 站视频时给每个片段写了
    ``UnitReference(source_href="#t={int(start_seconds)}")`` —— 秒数就藏在这儿。
    """

    out: dict[int, float] = {}
    try:
        for ref in store.unit_references(material_id) or []:
            href = str(_field(ref, "source_href", "") or "")
            match = re.search(r"#\s*t\s*=\s*(\d+)", href)
            if not match:
                continue
            try:
                out[int(_field(ref, "locator", 0))] = float(match.group(1))
            except (TypeError, ValueError):
                continue
    except Exception as exc:  # noqa: BLE001 - 秒数缺失只影响时间戳展示
        _log(f"读取 unit_references 失败（时间戳将显示 0:00）: {exc}")
    return out


def card_for_annotation(
    material_id: str,
    title: str,
    annotation: Any,
    seconds: float = 0.0,
) -> dict[str, Any] | None:
    """把一条 reading annotation 转成 ``notebook_entries`` 条目。

    ``quote`` 是标记覆盖的文本（视频里就是那句字幕），``note`` 是用户自己
    写的笔记。正面取 note（没有就退化成「回忆这个片段」），背面给出处 +
    原话 —— 「我记的要点 → 原话」正好构成一张回忆卡。
    """

    annotation_id = str(_field(annotation, "annotation_id", "") or "").strip()
    if not annotation_id:
        return None
    quote = _clip(_field(annotation, "quote", ""))
    note = _clip(_field(annotation, "note", ""))
    if not quote and not note:
        return None

    material_id = str(material_id or "").strip()
    if not material_id:
        return None

    stamp = _mmss(seconds)
    question = note or f"回忆这个片段（{stamp}）"
    explanation = (
        f"来自《{title}》{stamp}\n{quote}" if quote else f"来自《{title}》{stamp}"
    )
    return {
        "session_id": None,
        "origin_type": ORIGIN_TYPE,
        "origin_ref": material_id,
        "turn_id": "",
        "question_id": f"{ANNOTATION_PREFIX}{annotation_id}",
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


async def sync_annotations(material_id: str, store: Any = None) -> dict[str, Any]:
    """把一个材料的全部 reading annotations 同步成复习卡（幂等）。"""

    store = store if store is not None else _reading_store()
    manifest = store.manifest(material_id)
    title = _clip(_field(manifest, "title", "") or "视频", 120) or "视频"
    annotations = store.annotations(material_id) or []
    seconds = _locator_seconds(store, material_id)
    items = [
        card
        for card in (
            card_for_annotation(
                material_id, title, row, seconds.get(int(_field(row, "locator", 0) or 0), 0.0)
            )
            for row in annotations
        )
        if card is not None
    ]
    synced = 0
    if items:
        synced = await _store().upsert_notebook_entries(None, items)
    return {
        "material_id": material_id,
        "annotations": len(annotations),
        "synced": synced,
    }


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
    这个补一次。幂等，重复跑只会 update 不会重复建卡。两套存储都跑：
    timed_media（marks） + reading（annotations）。
    """

    outcome = await backfill_marks(limit)
    annotations = await backfill_annotations(limit)
    return {
        "materials": int(outcome.get("materials") or 0)
        + int(annotations.get("materials") or 0),
        "synced": int(outcome.get("synced") or 0) + int(annotations.get("synced") or 0),
        "failed": int(outcome.get("failed") or 0) + int(annotations.get("failed") or 0),
        "marks": outcome,
        "annotations": annotations,
    }


async def backfill_marks(limit: int = 500) -> dict[str, Any]:
    """timed_media 里的 marks 补建（video_learning 老路径）。"""

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


async def backfill_annotations(limit: int = 500) -> dict[str, Any]:
    """reading annotations 补建（1.6.14 主路径）。

    只扫 ``render_mode`` 是 video/audio 的材料 —— 文档的 annotate 是
    阅读批注，不是「边看边学」的标记，不该混进复习队列。
    """

    result = {"materials": 0, "synced": 0, "failed": 0}
    try:
        store = _reading_store()
        manifests = store.list_materials() or []
    except Exception as exc:  # noqa: BLE001 - 列不出材料就本次跳过
        _log(f"reading 材料枚举失败: {type(exc).__name__}: {exc}")
        return result
    for manifest in manifests[:limit]:
        material_id = str(_field(manifest, "material_id", "") or "")
        if not material_id:
            continue
        if str(_field(manifest, "render_mode", "text") or "text") not in {
            "video",
            "audio",
        }:
            continue
        try:
            outcome = await sync_annotations(material_id, store=store)
            result["materials"] += 1
            result["synced"] += int(outcome.get("synced") or 0)
        except Exception as exc:  # noqa: BLE001 - 单个材料坏掉不拖垮整批
            result["failed"] += 1
            _log(f"annotation 补建跳过 {material_id}: {type(exc).__name__}: {exc}")
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


async def drop_card(material_id: str, mark_id: str, prefix: str = CARD_PREFIX) -> bool:
    """删掉某个标记对应的复习卡（标记被删时调用）。"""

    entry = await _store().find_notebook_entry_by_origin(
        ORIGIN_TYPE, material_id, f"{prefix}{mark_id}"
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
        and str(row.get("question_id") or "").startswith((CARD_PREFIX, ANNOTATION_PREFIX))
    ]


async def _maybe_sync(material_id: str) -> None:
    """自动同步入口（marks 路径）：设置关了就跳过，失败只记录不抛出。"""

    try:
        if not load_settings().get("auto_sync", True):
            return
        result = await sync_material(material_id)
        _log(f"复习卡已同步: {result}")
    except Exception as exc:  # noqa: BLE001 - 卡片是增强，不能拖垮标记
        _log(f"复习卡同步失败（不影响标记本身）: {type(exc).__name__}: {exc}")


async def _maybe_sync_annotations(material_id: str) -> None:
    """自动同步入口（annotations 路径）：同上，失败只记录不抛出。"""

    try:
        if not load_settings().get("auto_sync", True):
            return
        result = await sync_annotations(material_id)
        _log(f"复习卡已同步（annotations）: {result}")
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


def _wrap_annotation_routes(router_module, router) -> int:
    """给 reading annotations 的增/删接口包一层「事后同步复习卡」。

    1.6.14 播放页的标记就走这两条路由（``ReadingProvider.saveMark`` →
    ``PUT /materials/{id}/annotations``；``removeMark`` → DELETE）。
    body 模型是路由模块里的 ``AnnotationPayload``。
    """

    wrapped = 0
    payload_model = getattr(router_module, "AnnotationPayload", None)

    def _make_put(original, model):
        # 模型在运行时才知道（getattr），所以用工厂把注解写进函数签名——
        # FastAPI 靠签名认 JSON body，运行后再补 __annotations__ 不可靠。
        async def put_endpoint(material_id: str, payload: model):
            result = await original(material_id, payload)
            await _maybe_sync_annotations(material_id)
            return result

        return put_endpoint

    def _make_delete(original):
        async def delete_endpoint(
            material_id: str, annotation_id: str, *args: Any, **kwargs: Any
        ):
            result = await original(material_id, annotation_id, *args, **kwargs)
            try:
                await drop_card(material_id, annotation_id, prefix=ANNOTATION_PREFIX)
            except Exception as exc:  # noqa: BLE001
                _log(f"删除复习卡失败（标记已删）: {type(exc).__name__}: {exc}")
            await _maybe_sync_annotations(material_id)
            return result

        return delete_endpoint

    # ---- PUT /materials/{material_id}/annotations ---------------------------
    route = _find_route(router, "/materials/{material_id}/annotations", "PUT")
    if route is not None and payload_model is not None:
        route.endpoint = _make_put(route.endpoint, payload_model)
        wrapped += 1
    else:
        _log("没找到 PUT /annotations 路由（或 AnnotationPayload 缺失），标记不同步卡片")

    # ---- DELETE /materials/{material_id}/annotations/{annotation_id} --------
    route = _find_route(router, "/materials/{material_id}/annotations/{annotation_id}", "DELETE")
    if route is not None:
        route.endpoint = _make_delete(route.endpoint)
        wrapped += 1
    else:
        _log("没找到 DELETE /annotations/{id} 路由，删除不联动删卡")

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


#: 按模块名记录已挂载的路由（两个 router 模块各挂一次，端点只注册一次）。
_INSTALLED_MODULES: set[str] = set()


def install(router_module, add_endpoints: bool = True) -> bool:
    """Wrap mark/annotation routes and register review endpoints.

    ``add_endpoints=False`` 给 reading 路由用：它只包路由、不新增端点
    （端点统一挂在 video_learning 路由下，面板只认那一份路径）。
    同一个模块重复调用是幂等的。
    """

    module_name = str(getattr(router_module, "__name__", "") or id(router_module))
    if module_name in _INSTALLED_MODULES:
        return False

    router = getattr(router_module, "router", None)
    if router is None:
        return False

    from fastapi.responses import JSONResponse
    from pydantic import BaseModel

    class ReviewSettingsRequest(BaseModel):
        auto_sync: bool = True

    wrapped = _wrap_mark_routes(router_module, router)
    wrapped += _wrap_annotation_routes(router_module, router)
    wrapped_get = _wrap_material_get(router)

    if add_endpoints:

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
            """手动全量同步（自动同步关闭时的前台按钮也能用）。

            marks 与 annotations 两套存储都同步 —— 材料可能来自任一路径。
            """

            try:
                marks = await sync_material(material_id)
            except Exception as exc:  # noqa: BLE001
                marks = {"error": str(exc)}
            try:
                annotations = await sync_annotations(material_id)
            except Exception as exc:  # noqa: BLE001
                annotations = {"error": str(exc)}
            return JSONResponse(
                {"material_id": material_id, "marks": marks, "annotations": annotations},
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

    _INSTALLED_MODULES.add(module_name)
    _log(
        f"复习卡闭环已挂载（{module_name}）：包了 {wrapped} 个标记路由"
        f"（材料详情路由{'已' if wrapped_get else '未'}包，用于触发历史补建）"
        + ("；新增 /bilibili review 端点" if add_endpoints else "；不新增端点")
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
