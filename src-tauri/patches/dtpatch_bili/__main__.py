"""``python -m dtpatch_bili`` 的入口，跑一次冒烟自检。

需要``DEEPTUTOR_HOME`` 或补丁目录已在 ``PYTHONPATH`` 上（由 sitecustomize
装载钩子）。自检失败只打印结果，不抛异常 —— 它是诊断工具，不是门禁。
"""

from __future__ import annotations

from . import _self_test

_self_test()