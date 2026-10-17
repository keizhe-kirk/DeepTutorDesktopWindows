"""AI 陪看：基于当前视频字幕的本地 LLM 问答（任务 #2）。

# 是什么

沉浸式观看时，播放器旁边多一个「AI 陪看」面板：

- **问一问**：看着视频随时提问（「刚才这句什么意思」「这个博主在对比什么」），
  LLM 拿**当前视频的字幕片段**当上下文回答，不靠通用知识瞎编；
- **章节摘要**：一键生成每个片段/章节的一句话摘要，先睹全片结构；
- **解释标记**：对某一条标记（reading annotation）问「这处为什么重要」。

# 为什么复用后端 LLM 而不是自己直连 LM Studio

补丁层跑在后端进程里，``deeptutor.services.llm`` 就是前端所有 AI 功能
（对话、co-writer、quiz）的同一条链路：用户在设置里配的模型 / base_url /
API Key 自动生效，换模型不用改补丁。LM Studio 桥接（桌面壳那一侧）负责
把本地服务拉起来，这里只负责「提问」。

    ┌────────────┐   POST /bilibili/materials/{id}/companion   ┌──────────┐
    │ B站面板 JS │ ───────────────────────────────────────────► │ 本模块   │
    │ (同源fetch)│ ◄─────────────────────────────────────────── │ 读字幕   │
    └──────────── ┘        {answer, model, cues_used}           │ 调 LLM   │
                                                               └──────────┘

# 字幕从哪来（1.6.14 的两套存储都兼容）

- **reading store**（主路径）：``iter_units()`` 出文本，
  ``unit_references()`` 的 ``source_href="#t=123"`` 出秒数；
- **timed_media**（兜底）：``transcript.cues`` 的 ``{start,end,text}``。

# 失败策略

LLM 没配置 / 超时 / 返空，都给**可执行**的文案（「去设置里配模型」），
绝不给裸堆栈；任何失败都不影响播放与标记功能。
"""

from __future__ import annotations

import asyncio
import re
import time
from typing import Any

from pydantic import BaseModel

#: ask 模式全文上下文的字符预算（本地模型上下文有限，超了截断并告知）。
ASK_CONTEXT_BUDGET = 6000

#: summary 模式的字符预算（要覆盖全片，放宽一些）。
SUMMARY_CONTEXT_BUDGET = 12000

#: explain/定点问答时，取位置前后多少秒的字幕做上下文。
WINDOW_BEFORE = 90.0
WINDOW_AFTER = 30.0

#: LLM 调用超时（秒）。本地模型首 token 可能很慢，给足。
LLM_TIMEOUT = 180.0

#: 支持的陪看模式。
MODES = ("ask", "summary", "explain")

_INSTALLED = False

_SYSTEM_PROMPT = (
    "你是 DeepTutor 的本地学习助手，陪用户一起看视频。"
    "用简体中文回答，简洁、直接、面向学习场景。"
    "只根据下面给出的字幕片段回答；字幕里没有的信息就明说「字幕里没提到」，"
    "不要编造时间、人名、数字。回答控制在两三句话以内。"
)


class CompanionRequest(BaseModel):
    """陪看请求。``mode`` 见 :data:`MODES`。"""

    mode: str = "ask"
    question: str = ""
    #: 当前播放位置（秒）。0 表示面板不知道位置，退化为全文上下文。
    position: float = 0.0
    #: explain 模式可指定片段序号（reading locator）。
    locator: int = 0
    #: explain 模式可指定标记 ID（reading annotation）。
    annotation_id: str = ""


def _log(msg: str) -> None:
    import sys

    print(f"[dtpatch.companion] {msg}", file=sys.stderr, flush=True)


# ── 字幕装载 ──────────────────────────────────────────────────────


def _reading_store():
    from deeptutor.reading.store import ReadingStore

    return ReadingStore()


def _timed_media_store():
    from deeptutor.video_learning import service as vl

    return vl.get_timed_media_store()


def _mmss(seconds: float) -> str:
    total = max(0, int(seconds or 0))
    hours, remainder = divmod(total, 3600)
    minutes, secs = divmod(remainder, 60)
    if hours:
        return f"{hours}:{minutes:02d}:{secs:02d}"
    return f"{minutes:02d}:{secs:02d}"


def _cues_from_reading(store: Any, material_id: str) -> tuple[list[dict[str, Any]], str]:
    """从 reading store 取字幕。秒数藏在 ``unit_references`` 的 ``#t=`` 里。"""

    manifest = store.manifest(material_id)
    title = str(getattr(manifest, "title", "") or "").strip()
    refs = {
        int(getattr(ref, "locator", 0)): ref
        for ref in (store.unit_references(material_id) or [])
    }
    cues: list[dict[str, Any]] = []
    for locator, text in store.iter_units(material_id):
        body = " ".join(str(text or "").split())
        if not body:
            continue
        ref = refs.get(int(locator))
        start = 0.0
        if ref is not None:
            match = re.search(r"#\s*t\s*=\s*(\d+)", str(getattr(ref, "source_href", "") or ""))
            if match:
                start = float(match.group(1))
        cues.append({"start": start, "locator": int(locator), "text": body})
    return cues, title


def _cues_from_timed_media(store: Any, material_id: str) -> tuple[list[dict[str, Any]], str]:
    """兜底：video_learning 的 timed_media（cues 带 start/end/text）。"""

    material = store.get(material_id)
    metadata = material.get("metadata") if isinstance(material.get("metadata"), dict) else {}
    title = str(metadata.get("title") or "").strip()
    rows = (material.get("transcript") or {}).get("cues") or []
    cues: list[dict[str, Any]] = []
    for row in rows:
        if not isinstance(row, dict):
            continue
        body = " ".join(str(row.get("text") or "").split())
        if not body:
            continue
        try:
            start = max(0.0, float(row.get("start") or 0))
        except (TypeError, ValueError):
            continue  # 与上游 normalize_cues 一致：坏行跳过，不留 start=0 的垃圾
        cues.append({"start": start, "locator": 0, "text": body})
    return cues, title


def load_cues(material_id: str) -> tuple[list[dict[str, Any]], str]:
    """取一个视频材料的全部字幕 cue。读不出来返回 ``([], "")``。"""

    try:
        return _cues_from_reading(_reading_store(), material_id)
    except Exception as reading_exc:  # noqa: BLE001 - 换兜底存储再试
        try:
            return _cues_from_timed_media(_timed_media_store(), material_id)
        except Exception as timed_exc:  # noqa: BLE001
            _log(
                f"字幕装载失败 {material_id}: "
                f"reading={type(reading_exc).__name__} timed={type(timed_exc).__name__}"
            )
            return [], ""


# ── 上下文构造 ────────────────────────────────────────────────────


def _render_cues(cues: list[dict[str, Any]]) -> str:
    return "\n".join(f"[{_mmss(cue['start'])}] {cue['text']}" for cue in cues)


def _window_cues(cues: list[dict[str, Any]], position: float) -> list[dict[str, Any]]:
    """取 position 前后一小段（含压线覆盖的 cue）。"""

    low = max(0.0, position - WINDOW_BEFORE)
    high = position + WINDOW_AFTER
    picked = [cue for cue in cues if low <= cue["start"] <= high]
    if len(picked) < 3:
        # 窗口太空（分P跳转/位置不准）就退化到全文，比给太少上下文强
        return list(cues)
    return picked


def _truncate(text: str, budget: int) -> tuple[str, bool]:
    if len(text) <= budget:
        return text, False
    return text[:budget], True


def _group_for_summary(cues: list[dict[str, Any]], groups: int = 8) -> list[list[dict[str, Any]]]:
    """把字幕均分成若干组（没有章节信息时的近似）。"""

    if not cues:
        return []
    size = max(1, (len(cues) + groups - 1) // groups)
    return [cues[i : i + size] for i in range(0, len(cues), size)]


def build_prompt(mode: str, cues: list[dict[str, Any]], request: CompanionRequest) -> tuple[str, str]:
    """按模式构造 (system_prompt, user_prompt)。"""

    if mode == "summary":
        blocks = _group_for_summary(cues)
        rendered = "\n\n".join(
            f"第 {index} 段：\n{_render_cues(block)}" for index, block in enumerate(blocks, 1)
        )
        rendered, truncated = _truncate(rendered, SUMMARY_CONTEXT_BUDGET)
        note = "\n（字幕过长，已截断，只总结已给出的部分）" if truncated else ""
        return _SYSTEM_PROMPT, (
            "下面是视频的各段字幕（带时间戳）。请按顺序为每一段生成一句话摘要，"
            "格式为「[时间戳] 一句话」。只总结字幕里有的内容。"
            f"{note}\n\n{rendered}"
        )

    if mode == "explain":
        picked = cues
        if request.locator:
            index = next(
                (i for i, cue in enumerate(cues) if cue.get("locator") == request.locator), None
            )
            if index is not None:
                picked = cues[max(0, index - 2) : index + 3]
        rendered = _render_cues(picked)
        rendered, truncated = _truncate(rendered, ASK_CONTEXT_BUDGET)
        note = "\n（字幕过长，已截断）" if truncated else ""
        focus = request.question.strip() or "用户在看这一处时打了标记，请解释这一处的内容。"
        return _SYSTEM_PROMPT, (
            f"用户在视频的这一处打了标记/有疑问：{focus}\n\n"
            f"这一处前后的字幕：{note}\n{rendered}\n\n"
            "请解释这一处：它在讲什么、为什么重要。"
        )

    # ask（默认）：有位置就用窗口上下文，没有就全文
    picked = _window_cues(cues, request.position) if request.position > 0 else list(cues)
    rendered = _render_cues(picked)
    rendered, truncated = _truncate(rendered, ASK_CONTEXT_BUDGET)
    note = "\n（字幕过长，已截断，只基于已给出的部分回答）" if truncated else ""
    scope = "当前播放位置附近" if request.position > 0 else "整个视频"
    return _SYSTEM_PROMPT, (
        f"以下是{scope}的字幕（带时间戳）：{note}\n{rendered}\n\n"
        f"用户的问题：{request.question.strip() or '这个片段在讲什么？'}"
    )


# ── LLM 调用 ──────────────────────────────────────────────────────


async def _call_llm(system_prompt: str, prompt: str) -> str:
    """用后端配置的模型跑一次补全。测试会 monkeypatch 这个函数。"""

    from deeptutor.services.llm import complete as llm_complete
    from deeptutor.services.llm.config import get_llm_config, get_token_limit_kwargs

    config = get_llm_config()
    base_url = (config.base_url or "").rstrip("/")
    for suffix in ("/chat/completions", "/completions"):
        if base_url.endswith(suffix):
            base_url = base_url[: -len(suffix)]
    api_key = config.api_key or "sk-no-key-required"
    token_kwargs = get_token_limit_kwargs(config.model, max_tokens=800)
    return await asyncio.wait_for(
        llm_complete(
            model=config.model,
            prompt=prompt,
            system_prompt=system_prompt,
            binding=config.binding,
            api_key=api_key,
            base_url=base_url,
            temperature=0.3,
            **token_kwargs,
        ),
        timeout=LLM_TIMEOUT,
    )


def _friendly_error(exc: Exception) -> str:
    name = type(exc).__name__
    text = str(exc)
    if "Config" in name or "config" in text.lower() or "api_key" in text.lower():
        return "还没配置可用的模型：请到「设置 → 模型与服务」配好 LLM 后再试"
    if isinstance(exc, asyncio.TimeoutError):
        return f"模型响应超时（{LLM_TIMEOUT:.0f} 秒没返回）：可能正在加载或上下文过长，请稍后再试"
    if "Connect" in name or "Connect" in text:
        return "连不上模型服务：请确认 LM Studio 已启动且端口与设置里一致"
    return f"AI 陪看调用失败：{name}: {text[:160]}"


# ── 路由侧挂载 ────────────────────────────────────────────────────


def install(router_module) -> bool:
    """Register companion endpoints on the video_learning router."""

    global _INSTALLED
    if _INSTALLED:
        return False

    router = getattr(router_module, "router", None)
    if router is None:
        return False

    from fastapi.responses import JSONResponse

    @router.get("/bilibili/companion/status")
    async def bilibili_companion_status() -> JSONResponse:
        """模型是否可用（面板用来决定显示「去配置模型」还是输入框）。"""

        try:
            from deeptutor.services.llm.config import get_llm_config

            config = get_llm_config()
            return JSONResponse(
                {"available": True, "model": config.model},
                headers={"Cache-Control": "no-store"},
            )
        except Exception as exc:  # noqa: BLE001
            return JSONResponse(
                {"available": False, "error": _friendly_error(exc)},
                headers={"Cache-Control": "no-store"},
            )

    @router.post("/bilibili/materials/{material_id}/companion")
    async def bilibili_companion(material_id: str, payload: CompanionRequest) -> JSONResponse:
        """陪看主端点：ask / summary / explain 三种模式。"""

        started = time.time()
        mode = (payload.mode or "ask").strip().lower()
        if mode not in MODES:
            return JSONResponse(
                {"error": f"未知模式 {mode!r}，可用：{', '.join(MODES)}"},
                status_code=400,
                headers={"Cache-Control": "no-store"},
            )
        try:
            cues, title = load_cues(material_id)
            if not cues:
                return JSONResponse(
                    {
                        "error": "这个视频还没有字幕数据，等播放页字幕加载完再试",
                        "material_id": material_id,
                    },
                    status_code=400,
                    headers={"Cache-Control": "no-store"},
                )
            system_prompt, prompt = build_prompt(mode, cues, payload)
            answer = await _call_llm(system_prompt, prompt)
            answer = (answer or "").strip()
            if not answer:
                return JSONResponse(
                    {"error": "模型没有返回内容，换个问法或稍后再试"},
                    status_code=400,
                    headers={"Cache-Control": "no-store"},
                )
            return JSONResponse(
                {
                    "material_id": material_id,
                    "mode": mode,
                    "answer": answer,
                    "title": title,
                    "cues_used": len(cues),
                    "elapsed_ms": round((time.time() - started) * 1000),
                },
                headers={"Cache-Control": "no-store"},
            )
        except Exception as exc:  # noqa: BLE001 - 文案要可执行，不能给裸堆栈
            _log(f"陪看失败 {material_id} ({mode}): {type(exc).__name__}: {exc}")
            return JSONResponse(
                {"error": _friendly_error(exc), "material_id": material_id},
                status_code=400,
                headers={"Cache-Control": "no-store"},
            )

    _INSTALLED = True
    _log("AI 陪看已挂载：/bilibili/companion/status、/bilibili/materials/{id}/companion")
    return True


def try_install(module_name: str = "deeptutor.api.routers.video_learning") -> bool:
    try:
        import importlib

        module = importlib.import_module(module_name)
    except Exception as exc:  # noqa: BLE001
        _log(f"未找到 {module_name}，AI 陪看本次不挂载（不影响其他功能）: {exc}")
        return False
    try:
        return install(module)
    except Exception as exc:  # noqa: BLE001
        _log(f"挂载 AI 陪看失败（不影响其他功能）: {type(exc).__name__}: {exc}")
        return False
