"""给上游 reading 摄入流水线注入我们带登录态的 B 站 loader。

# 为什么需要这个（1.6.14 起）

上游``ReadingIngestionService`` 构造时会选loader::

    self._bilibili_loader = bilibili_loader or _load_bilibili_media

而 ``_load_bilibili_media`` 实测**三重失效**（接口选错 + 不带 cookie +
UA 被当爬虫），1.6.13 与 1.6.14 **逐字节一致**（4438 字符，md5 相同）——
上游从没修过。所以字幕必须由我们带cookie 的实现产出。

# 注入点选_ingestion() 而不是 __init__

``ReadingIngestionService.__init__`` 本身已经支持 ``bilibili_loader=``，
但那是上游的代码，我们改不得（安装目录只读，且会被热更新冲掉）。
``api/routers/reading.py`` 里有个只属于它的工厂函数::

    def _ingestion() -> ReadingIngestionService:
        catalog = _catalog()
        return ReadingIngestionService(ReadingStore(catalog.root), catalog)

把模块里的 ``_ingestion`` 换成我们的版本，就能让**所有**走这条工厂的摄入
路径都用上我们的 loader —— 包括 1.6.14 新增的
``POST /api/reading/library/import-urls``（前端现在就是走它加视频）。

★ 模块全局在**调用时**才解析（``service = _ingestion()`` 写在函数体内），
  所以直接替换模块属性即可，路由装饰器早绑定的那个引用不受影响。
"""

from __future__ import annotations

import sys
from typing import Any

_INSTALLED = False


def _log(msg: str) -> None:
    print(f"[dtpatch.bili] {msg}", file=sys.stderr, flush=True)


def _loader(url_value: str, languages: Any):
    """适配成上游 ``BilibiliLoader`` 签名。

    上游传的是**规范化后的 URL 字符串**，我们内部要的是 ``BilibiliRequest``。
    """

    from . import _fetch_bilibili_media, _parse

    return _fetch_bilibili_media(_parse(url_value), languages)


def install(module) -> bool:
    """把 ``_ingestion()`` 换成注入我们 loader 的版本。幂等。"""

    global _INSTALLED
    if _INSTALLED:
        return False

    original = getattr(module, "_ingestion", None)
    if not callable(original):
        _log("reading 路由里没有 _ingestion()，B 站字幕注入本次跳过")
        return False

    def _ingestion():
        service = original()
        # 只替换 loader，其余配置（catalog / reading_store / transcriber）保持原样。
        try:
            service._bilibili_loader = _loader
        except Exception as exc:  # noqa: BLE001 - 绝不让后端起不来
            _log(f"注入 bilibili_loader 失败，字幕将退回上游实现: {exc}")
            return service
        return service

    module._ingestion = _ingestion
    _INSTALLED = True
    _log("已注入 reading 摄入流水线的 bilibili_loader（字幕将带登录态抓取）")
    return True


def try_install(module_name: str = "deeptutor.api.routers.reading") -> bool:
    """导入并安装。Never raises."""

    try:
        import importlib

        module = importlib.import_module(module_name)
    except Exception as exc:  # noqa: BLE001
        _log(f"未找到 {module_name}，字幕注入本次跳过（不影响其他功能）: {exc}")
        return False
    try:
        return install(module)
    except Exception as exc:  # noqa: BLE001
        _log(f"字幕注入失败（不影响其他功能）: {type(exc).__name__}: {exc}")
        return False