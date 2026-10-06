"""从命令行跑一次 B 站搜索，把结果以 JSON 打到 stdout。

# 为什么需要它

搜索能力本来只挂在 FastAPI 端点上（``router.py`` 的 ``/bilibili/search``），
**而前端没有任何 B 站搜索 UI** —— 能力就绪但用户没有入口，等于没有。
这个模块给「入口」提供后端：Rust 侧托盘菜单收集到关键词后跑它，
拿 JSON 渲染成可点的列表，不必依赖前端改动。

契约（**必须严格遵守**，Rust 侧按这个解析）：

- 结果只打到 **stdout**，且是**唯一一个 JSON 对象**（换行结尾）。
- 任何日志/警告只走 **stderr**。往 stdout 打杂信息会污染解析。
- 永不抛异常：失败以 ``{"error": "..."}`` 表达。

用法::

    python -m dtpatch_bili.search_cli 线性代数
    python -m dtpatch_bili.search_cli 线性代数 2
"""

from __future__ import annotations

import asyncio
import json
import sys

from . import search as _search


def _emit(payload: dict) -> None:
    # ensure_ascii=False 让中文标题在 Rust 侧直接可读，省一次转义。
    sys.stdout.write(json.dumps(payload, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if not args or args[0] in {"-h", "--help"}:
        print(__doc__, file=sys.stderr)
        return 0 if args else 2

    keyword = args[0]
    page = 1
    if len(args) > 1:
        try:
            page = max(1, int(args[1]))
        except ValueError:
            page = 1

    try:
        payload = asyncio.run(_search.search(keyword, page=page))
    except Exception as exc:  # noqa: BLE001 - 入口永不因异常而空手
        _emit({"error": f"{type(exc).__name__}: {exc}", "results": [], "keyword": keyword})
        return 1

    _emit(payload)
    return 0 if not payload.get("error") else 1


if __name__ == "__main__":
    raise SystemExit(main())
