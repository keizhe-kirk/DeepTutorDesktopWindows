"""启动钩子:由 PYTHONPATH 上的 ``sitecustomize.py`` 在解释器启动时 import。

职责只有一件:把「B 站搜索」的能力注入
``deeptutor.video_learning.service`` 与相关路由。

# 2026-10-18 拆除记录

曾在此挂载字幕检索 / AI 陪看 / 热评 / 课程化进度 / ASR 兜底 / 标记同步
等「沉浸式观看增强」（review / companion / heat / series / asr 五个模块），
用户真机试用后判定为无用的设计测试，已全部拆除，只留 B站搜索。
保留的两处注入是**搜索的地基**，删了搜索→导入→播放会断:

* ``video_learning.service`` —— B 站材料解析（apply_patch）;
* ``api.routers.reading`` —— 摄入流水线换我们带 cookie 的 loader
  （上游 loader 接口选错 + 不带 cookie + UA 被当爬虫，三重失效）。

# 为什么用import 钩子而不是改文件

安装目录是 ``perMachine`` 的,普通用户写不进去;而且直接改 site-packages
会被后端热更新 / 桌面壳升级冲掉。这里改的是**内存里的模块对象** —— 原文件
一个字节都不动,补丁随进程消亡。

# 为什么钩子要等模块被 import 时才动手

业务代码是懒加载 ``deeptutor.video_learning.service`` 的(在请求处理路径上)。
钩子装在 ``sys.meta_path``,等真要 import 时才把加载器包一层:先让原模块
正常加载完,再改写它 —— 这样我们看到的是**完整的**原模块,而不是半成品。

# 失败策略

任何一步失败都只打印一行 stderr,**绝不让后端起不来**。后端起不来的代价
远大于「B 站没接上」。
"""

from __future__ import annotations

import sys
import traceback

_SERVICE = "deeptutor.video_learning.service"
_ROUTER = "deeptutor.api.routers.video_learning"
# ★ 1.6.14 起前端加视频走的是 reading 的导入端点，不再调 video_learning 的
#   resolve，所以字幕注入必须也钩 reading 路由 —— 否则补丁只对老路径有效。
_READING = "deeptutor.api.routers.reading"
_TARGETS = (_SERVICE, _ROUTER, _READING)


def _log(msg: str) -> None:
    print(f"[dtpatch] {msg}", file=sys.stderr, flush=True)


class _VideoLearningPatch:
    """Wrap the target module's loader, then rewrite it once fully loaded."""

    def find_module(self, fullname, path=None):  # legacy API, unused on 3.12+
        return None

    def load_module(self, fullname):
        return None

    def find_spec(self, fullname, path=None, target=None):
        if fullname not in _TARGETS:
            return None
        import importlib.util

        # Temporarily remove ourselves so find_spec does not recurse.
        try:
            sys.meta_path.remove(self)
        except ValueError:
            return None
        try:
            spec = importlib.util.find_spec(fullname)
        finally:
            sys.meta_path.insert(0, self)

        if spec is None or spec.loader is None:
            return None

        original_exec = spec.loader.exec_module

        def exec_module(module):
            original_exec(module)
            try:
                if fullname == _SERVICE:
                    from dtpatch_bili import apply_patch

                    apply_patch(module)
                elif fullname == _ROUTER:
                    from dtpatch_bili import router as bili_router

                    # ★ 必须在 include_router 之前完成 —— router 模块 import 完
                    # 就立刻注册,那时app 还没遍历它的 routes。
                    bili_router.install(module)
                else:
                    # reading 路由：换 _ingestion() 注入我们的 loader。这是
                    # B 站导入的地基 —— 上游 loader 三重失效，不注入它
                    # 「搜索→导入→播放」整条链会断。不新增端点，所以没有
                    # include_router 那条时序约束。
                    from dtpatch_bili import reading_patch

                    reading_patch.install(module)
            except Exception:  # noqa: BLE001 - never break the backend
                _log(f"注入 {fullname} 失败，相关功能本次不可用:")
                traceback.print_exc(file=sys.stderr)

        spec.loader.exec_module = exec_module
        return spec


def _install() -> None:
    try:
        sys.meta_path.insert(0, _VideoLearningPatch())
        _log("已装载 import 钩子,等待 deeptutor.video_learning.service 加载")
    except Exception:  # noqa: BLE001
        _log("无法装载钩子(不影响后端启动):")
        traceback.print_exc(file=sys.stderr)


_install()