"""B 站登录凭据的读写。

# 为什么要登录态

实测：知识/教学区视频12 个里有 9 个 ``need_login_subtitle=true``，无cookie 时
``x/player/v2`` 返回的 ``subtitles`` 是空数组，``view_points``（章节）同样为空。
也就是说**没有登录态就没有字幕**，"边看边学"退化为纯播放。

需要的是 ``SESSDATA``（登录态）与 ``buvid3``（设备指纹，缺了会触发 -412 风控），
**不需要**账号密码。

# 存放

``%LOCALAPPDATA%\\DeepTutor\\patches\\bili_credentials.json``。
文件权限尽力设为仅当前用户可读（Windows 上 ``icacls``），且**绝不入库**。

# 绝不抛异常

凭据是可选增强。任何读取/解析失败都返回空 dict，让 B 站退回纯播放，
绝不让后端起不来。
"""

from __future__ import annotations

import json
import os
from pathlib import Path

#: B 站真正需要的两个 cookie。其余（SESSDATA 之外的）不存。
NEEDED = ("SESSDATA", "buvid3")

_FILENAME = "bili_credentials.json"


def _patch_root() -> Path:
    """``%LOCALAPPDATA%/DeepTutor/patches``,与 Rust 侧的 patch 目录一致。"""

    override = os.environ.get("DEEPTUTOR_PATCH_HOME")
    if override:
        return Path(override)
    local = os.environ.get("LOCALAPPDATA") or os.path.expanduser("~")
    return Path(local) / "DeepTutor" / "patches"


def credentials_path() -> Path:
    return _patch_root() / _FILENAME


def load_cookies() -> dict[str, str]:
    """Return the stored cookies, or ``{}`` when absent/invalid."""

    path = credentials_path()
    try:
        raw = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return {}
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        return {}
    if not isinstance(data, dict):
        return {}
    out: dict[str, str] = {}
    for key in NEEDED:
        value = data.get(key)
        if isinstance(value, str) and value.strip():
            out[key] = value.strip()
    return out


def is_logged_in() -> bool:
    """``SESSDATA`` present = logged in. ``buvid3`` alone is not enough."""

    return "SESSDATA" in load_cookies()


def save_cookies(cookies: dict[str, str]) -> None:
    """Persist the given cookies, keeping only :data:`NEEDED`. Never raises."""

    path = credentials_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        key: cookies[key].strip()
        for key in NEEDED
        if isinstance(cookies.get(key), str) and cookies[key].strip()
    }
    path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
    _restrict(path)


def clear_cookies() -> bool:
    """Remove the credential file. Returns True if a file was deleted."""

    path = credentials_path()
    try:
        path.unlink()
        return True
    except FileNotFoundError:
        return False
    except OSError:
        return False


def _restrict(path: Path) -> None:
    """Best-effort: make the file readable only by the current user.

    Silently skipped when ``icacls`` is unavailable — the desktop app runs in
    the user's own profile directory anyway.
    """

    import subprocess

    try:
        user = os.environ.get("USERNAME")
        if not user:
            return
        subprocess.run(
            ["icacls", str(path), "/inheritance:r", "/grant:r", f"{user}:(R,W)"],
            capture_output=True,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return


# --------------------------------------------------------------------------
# 有效性检查
# --------------------------------------------------------------------------


async def check_valid() -> dict[str, str]:
    """Ask Bilibili whether the stored ``SESSDATA`` still works.

    Uses ``x/web-interface/nav``: it is cheap, needs no signature, and returns
    ``code=0`` plus a ``mid`` when the cookie is accepted.
    """

    cookies = load_cookies()
    if not cookies:
        return {"ok": "false", "detail": "未配置登录态"}

    try:
        import httpx
    except ImportError:
        return {"ok": "unknown", "detail": "httpx 不可用"}

    try:
        async with httpx.AsyncClient(
            timeout=10.0,
            follow_redirects=False,
            cookies=cookies,
            headers={
                "User-Agent": (
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
                    "(KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"
                ),
                "Referer": "https://www.bilibili.com/",
            },
        ) as client:
            response = await client.get("https://api.bilibili.com/x/web-interface/nav")
            payload = response.json()
    except Exception as exc:  # noqa: BLE001
        return {"ok": "unknown", "detail": f"{type(exc).__name__}: {exc}"}

    if payload.get("code") == 0:
        data = payload.get("data") or {}
        uname = (data.get("uname") or "")[:40]
        mid = data.get("mid")
        detail = f"已登录{f'（{uname}, mid={mid}）' if uname else ''}"
        return {"ok": "true", "detail": detail}
    return {"ok": "false", "detail": str(payload.get("message") or "登录态已失效")}