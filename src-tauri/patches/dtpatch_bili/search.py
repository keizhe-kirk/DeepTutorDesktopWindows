"""B 站内搜索。

# 为什么必须节流（实测结论，别删）

``x/web-interface/search/type`` 对匿名请求有**速率风控**。实测：
连续快速调用立刻返回 ``code=-412 request was banned``，而同一客户端在
``Referer`` + 约 1.6 秒间隔下连续 4 组请求全部 ``code=0``。

所以这里用 :class:`_Throttle` 做全局节流，并复用登录态里的 ``buvid3``
（第一方设备指纹，能进一步降低风控概率）。

# 与 Invidious 搜索的关系

上游的 ``/invidious/browse/{kind}`` 是给 Invidious 实例用的，依赖用户自建实例。
B 站用自己的官方接口，不依赖任何自建服务，因此这里独立实现，不复用那条链路。

# 优雅降级

搜索失败（风控 / 网络 / 接口变更）一律返回 ``{"results": [], "error": ...}``，
**绝不影响**粘贴链接解析与播放 —— 搜索只是锦上添花。
"""

from __future__ import annotations

import asyncio
import time
from typing import Any

#: 两次搜索之间的最小间隔（秒）。
#:
#: 实测校准：1.6s 在单次调用时够用，但**连续调用仍会被 -412**。B 站对搜索的
#: 匿名配额比单个请求严格得多，所以这里取 3.0s 并在命中 -412 时指数退避。
MIN_INTERVAL = 3.0

#: 命中 -412 时的退避序列（秒）。逐级变长，最后一次放弃并如实报错。
BACKOFF = (4.0, 8.0, 16.0)

#: 单次搜索最多取多少条 —— 够填一屏，又不至于把风控额度一次耗光。
MAX_RESULTS = 20

_SEARCH_URL = "https://api.bilibili.com/x/web-interface/search/type"
_REFERER = "https://search.bilibili.com/"
_UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"
)


def _warn(msg: str) -> None:
    import sys

    print(f"[dtpatch.bili.search] {msg}", file=sys.stderr, flush=True)


class _Throttle:
    """Process-wide minimum interval between search calls.

    ``widen`` lets a-412 response stretch the interval for every later call:
    a client that just got banned should not keep hammering.
    """

    def __init__(self, interval: float = MIN_INTERVAL) -> None:
        self._interval = interval
        self._base = interval
        self._last = 0.0
        self._lock = asyncio.Lock()

    def widen(self, seconds: float) -> None:
        self._interval = max(self._interval, seconds, self._base)

    async def wait(self) -> None:
        async with self._lock:
            now = time.monotonic()
            gap = self._interval - (now - self._last)
            if gap > 0:
                await asyncio.sleep(gap)
            self._last = time.monotonic()


_throttle = _Throttle()


def _strip_html(value: Any) -> str:
    """Bilibili search results embed <em class="keyword"> highlights."""

    import re

    text = str(value or "")
    text = re.sub(r"<[^>]+>", "", text)
    return (
        text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", '"')
        .replace("&#39;", "'")
        .strip()
    )


def _duration_seconds(value: Any) -> int:
    """Search returns ``"12:34"`` / ``"1:02:03"``."""

    raw = str(value or "").strip()
    if not raw:
        return 0
    parts = raw.split(":")
    try:
        numbers = [int(p) for p in parts]
    except ValueError:
        return 0
    total = 0
    for n in numbers:
        total = total * 60 + n
    return total


def _row_to_item(row: dict[str, Any]) -> dict[str, Any] | None:
    bvid = str(row.get("bvid") or "").strip()
    if not bvid:
        return None
    return {
        "bvid": bvid,
        "title": _strip_html(row.get("title")),
        "author": _strip_html(row.get("author")),
        "duration_seconds": _duration_seconds(row.get("duration")),
        "thumbnail_url": str(row.get("pic") or "").strip(),
        "url": f"https://www.bilibili.com/video/{bvid}/",
        "play_count": int(row.get("play") or 0) if str(row.get("play") or "").isdigit() else 0,
        "published_at": int(row.get("pubdate") or 0),
        "description": _strip_html(row.get("description"))[:200],
    }


async def _once(
    httpx_mod,
    term: str,
    page: int,
    jar: dict[str, str],
) -> tuple[int, dict[str, Any]]:
    """One throttled attempt. Returns ``(code, payload)``; never raises."""

    async with httpx_mod.AsyncClient(
        timeout=12.0,
        follow_redirects=False,
        headers={"User-Agent": _UA, "Referer": _REFERER, "Accept": "application/json"},
        cookies=jar or None,
    ) as client:
        response = await client.get(
            _SEARCH_URL,
            params={"search_type": "video", "keyword": term, "page": page},
        )
        try:
            payload = response.json()
        except ValueError:
            return -1, {"message": f"响应不是 JSON（HTTP {response.status_code}）"}
        return int(payload.get("code") or 0), payload


async def search(
    keyword: str,
    *,
    page: int = 1,
    cookies: dict[str, str] | None = None,
) -> dict[str, Any]:
    """Search Bilibili videos. Never raises — failures come back as ``error``."""

    term = (keyword or "").strip()
    if not term:
        return {"results": [], "error": "", "keyword": term, "page": page}

    try:
        import httpx
    except ImportError:
        return {"results": [], "error": "httpx 不可用", "keyword": term, "page": page}

    from . import credentials as cred

    jar = dict(cookies or {})
    if not jar:
        jar = cred.load_cookies()

    page = max(1, int(page or 1))
    code, payload = -1, {"message": ""}

    # Attempt 0 is the normal path; extra attempts only exist for -412.
    for attempt in range(len(BACKOFF) + 1):
        await _throttle.wait()
        try:
            code, payload = await _once(httpx, term, page, jar)
        except Exception as exc:  # noqa: BLE001
            return {
                "results": [],
                "error": f"搜索请求失败：{type(exc).__name__}",
                "keyword": term,
                "page": page,
            }
        if code != -412:
            break
        if attempt < len(BACKOFF):
            delay = BACKOFF[attempt]
            _warn(f"命中 -412 风控，{delay:.0f}s 后重试（第 {attempt + 1} 次）")
            # Widen the global interval too: a hot client should slow down for
            # everyone, not just this call.
            _throttle.widen(delay)

    if code == -412:
        return {
            "results": [],
            "error": "触发了 B 站风控（-412），请稍后再试",
            "keyword": term,
            "page": page,
        }
    if code != 0:
        return {
            "results": [],
            "error": str(payload.get("message") or f"搜索失败（code={code}）"),
            "keyword": term,
            "page": page,
        }

    rows = ((payload.get("data") or {}).get("result")) or []
    items: list[dict[str, Any]] = []
    for row in rows:
        if not isinstance(row, dict):
            continue
        item = _row_to_item(row)
        if item:
            items.append(item)
        if len(items) >= MAX_RESULTS:
            break

    return {
        "results": items,
        "error": "",
        "keyword": term,
        "page": page,
        "total": len(items),
    }