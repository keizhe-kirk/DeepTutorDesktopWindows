"""B 站登录：用系统 Edge/Chrome 的 DevTools 协议读取登录态。

# 为什么是这条路（换掉的前一版方案不可行，已实测否决）

先试过"本地起 HTTP 服务 + 让浏览器把 cookie POST 回来"，**技术上不成立**，两条都堵死：

1. ``SESSDATA`` 是 **HttpOnly** cookie，页面 ``document.cookie`` 读不到。
2. ``http://127.0.0.1`` 的服务拿不到 ``.bilibili.com`` 的 cookie（不同站）。
3. ``passport.bilibili.com/login`` **不支持** ``callback_url`` 参数（实测 404）。

用 Playwright 倒是能解决，但要给自包含的内置Python 加浏览器二进制（+150MB 量级），
与安装包体积直接冲突 —— 不可接受。

# 实际方案

系统本来就装了 Edge/Chrome。用它的**用户数据目录**起一个实例，
扫码登录后通过 DevTools 协议（CDP）读 cookie：

- 启动浏览器时带一个**独立端口**的 ``--remote-debugging-port``；
- 用 WebSocket 调 ``Network.getAllCookies`` 拿全部 cookie；
- 只取 ``SESSDATA`` / ``buvid3``，其余立刻丢弃、不落盘。

用户看到的体验与普通登录一致：浏览器打开 → 扫码 → 自动保存。

# 依赖

仅标准库 + WebSocket。WebSocket 客户端是手写的最小实现（CDP 只需文本帧，
不需扩展协商），避免为几十行代码引入第三方包。
"""

from __future__ import annotations

import base64
import json
import os
import secrets
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

from . import credentials as cred

WANTED = ("SESSDATA", "buvid3")

#: 已安装的浏览器候选，按优先级。
BROWSERS = (
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
)

_LOGIN_PAGE = "https://passport.bilibili.com/login"


def log(message: str) -> None:
    """写到 stderr。

    ★ 必须走 stderr 且即时 flush：Rust 侧把 stdout 当作**结果通道**
    （只解析最后一行 JSON），任何往 stdout 打的调试信息都会污染它。
    """
    try:
        print(f"[dtpatch.bili.login] {message}", file=sys.stderr, flush=True)
    except Exception:  # noqa: BLE001 - 日志绝不能影响登录本身
        pass


def find_browser() -> str | None:
    for path in BROWSERS:
        if Path(path).is_file():
            return path
    found = shutil.which("msedge") or shutil.which("chrome")
    return found


# --------------------------------------------------------------------------
# 极简 WebSocket 客户端（只支持 CDP 需要的文本帧）
# --------------------------------------------------------------------------


class _WS:
    def __init__(self, url: str, timeout: float = 15.0) -> None:
        import urllib.parse

        parsed = urllib.parse.urlparse(url)
        if parsed.scheme != "ws":
            raise ValueError(f"not a ws url: {url}")
        self._sock = socket.create_connection(
            (parsed.hostname or "127.0.0.1", parsed.port or 80), timeout=timeout
        )
        self._sock.settimeout(timeout)
        key = base64.b64encode(secrets.token_bytes(16)).decode()
        path = parsed.path or "/"
        if parsed.query:
            path += "?" + parsed.query
        handshake = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {parsed.hostname}:{parsed.port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        )
        self._sock.sendall(handshake.encode())
        self._buf = b""
        header = self._read_until(b"\r\n\r\n")
        if b" 101" not in header.split(b"\r\n")[0]:
            raise ValueError(f"websocket handshake failed: {header[:80]!r}")

    def _read_until(self, sep: bytes) -> bytes:
        while sep not in self._buf:
            chunk = self._sock.recv(65536)
            if not chunk:
                raise ConnectionError("socket closed during handshake")
            self._buf += chunk
        head, self._buf = self._buf.split(sep, 1)
        return head

    def _recv_exact(self, n: int) -> bytes:
        while len(self._buf) < n:
            chunk = self._sock.recv(65536)
            if not chunk:
                raise ConnectionError("socket closed")
            self._buf += chunk
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

    def send(self, text: str) -> None:
        payload = text.encode()
        header = bytearray([0x81])  # FIN + text
        mask = secrets.token_bytes(4)
        n = len(payload)
        if n < 126:
            header.append(0x80 | n)
        elif n < 1 << 16:
            header.append(0x80 | 126)
            header += struct.pack(">H", n)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", n)
        header += mask
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self._sock.sendall(bytes(header) + masked)

    def recv(self) -> str:
        while True:
            first = self._recv_exact(2)
            opcode = first[0] & 0x0F
            length = first[1] & 0x7F
            if length == 126:
                length = struct.unpack(">H", self._recv_exact(2))[0]
            elif length == 127:
                length = struct.unpack(">Q", self._recv_exact(8))[0]
            data = self._recv_exact(length)
            if opcode == 0x8:
                raise ConnectionError("websocket closed by peer")
            if opcode == 0x9:  # ping -> pong
                continue
            if opcode in (0x1, 0x2):
                return data.decode("utf-8", "replace")

    def close(self) -> None:
        try:
            self._sock.close()
        except OSError:
            pass


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def _http_json(url: str, timeout: float = 2.0):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except Exception:  # noqa: BLE001
        return None


def _pick_target(info: object) -> str:
    """从 `/json/list` 里挑一个该问的 target，返回其 ws url（空串=没有）。"""
    if not isinstance(info, list):
        return ""
    # ① 优先 bilibili 页面 —— 只有它的 cookie jar 里才有我们要的东西。
    for target in info:
        if not isinstance(target, dict):
            continue
        if "bilibili.com" in str(target.get("url") or "") and target.get("webSocketDebuggerUrl"):
            return str(target["webSocketDebuggerUrl"])
    # ② 退而求其次：任意普通页面。★ 必须排除 background_page ——
    # Edge/Chrome 启动时会带一票扩展的 background_page，它们的 cookie jar
    # 里绝不会有 .bilibili.com 的东西，握上去必然空手而归。
    for target in info:
        if not isinstance(target, dict):
            continue
        if target.get("type") == "page" and target.get("webSocketDebuggerUrl"):
            return str(target["webSocketDebuggerUrl"])
    return ""


def _dump_wanted_cookies(ws_url: str) -> dict[str, str]:
    """在给定 target 上问一次 cookie，只保留 WANTED 里的。"""
    ws = _WS(ws_url)
    try:
        ws.send(json.dumps({"id": 1, "method": "Network.getAllCookies"}))
        for _ in range(12):
            message = json.loads(ws.recv())
            if message.get("id") != 1:
                continue
            if "error" in message:
                log(f"CDP 返回错误: {message['error']}")
                return {}
            found: dict[str, str] = {}
            for cookie in message.get("result", {}).get("cookies", []):
                name = str(cookie.get("name") or "")
                if name in WANTED:
                    value = str(cookie.get("value") or "").strip()
                    if value:
                        found[name] = value
            return found
    finally:
        ws.close()
    return {}


def _read_cookies_via_cdp(port: int, timeout: float) -> dict[str, str]:
    """轮询 CDP，直到拿到 SESSDATA 或超时。

    ★ 关键：**必须反复轮询到超时为止**，不能只问一次就返回。
    登录页刚打开时用户还没扫码，此刻 cookie jar 里必然是空的 —— 问一次
    就返回等于「用户永远来不及扫码」。之前正是这么写的，于是无论用户扫
    多快，脚本都在扫码前就结束，界面只显示「没有取到登录态」。
    """
    deadline = time.monotonic() + timeout
    # 每次查询都重取 target：登录后页面会跳转（passport -> 主页），
    # 拿旧的 target 句柄会一直读到那个已经消失的页面。
    while time.monotonic() < deadline:
        ws_url = _pick_target(_http_json(f"http://127.0.0.1:{port}/json/list"))
        if ws_url:
            try:
                found = _dump_wanted_cookies(ws_url)
            except Exception as exc:  # noqa: BLE001 - 页面可能正在导航
                log(f"读取 cookie 失败（页面可能正在跳转）: {exc!r}")
                found = {}
            if "SESSDATA" in found:
                return found
        time.sleep(1.0)
    return {}


def _cleanup(profile: Path, proc: subprocess.Popen | None) -> None:
    """关浏览器、删临时 profile。**每一步都不许抛**。

    ★ 删不掉是常态而非异常：浏览器退出有延迟，profile 里的文件可能仍被
    占用；而本机的删除还会被安全策略拦（safe-delete shim 改走回收站）。
    所以这里一律 `ignore_errors` + 记日志 —— 清理失败绝不能影响登录结果，
    只是会在临时目录里留下一个可以下次再清的目录。
    """
    if proc is not None:
        try:
            proc.terminate()
            proc.wait(timeout=8)
        except Exception:  # noqa: BLE001
            try:
                proc.kill()
            except Exception:  # noqa: BLE001
                pass
    try:
        shutil.rmtree(profile, ignore_errors=True)
    except Exception:  # noqa: BLE001
        pass
    if profile.exists():
        log(f"临时 profile 未完全清理（可忽略，下次启动会复用）: {profile}")


def login(timeout: float = 300.0) -> dict[str, str]:
    """Open a browser, wait for the user to scan, persist the cookies.

    Never raises; returns the collected cookies (``{}`` on failure/timeout).
    """

    browser = find_browser()
    if not browser:
        log("未找到 Edge/Chrome，无法引导登录")
        return {}

    port = _free_port()
    # 独立 profile：不污染用户日常浏览数据，退出时整个目录可丢。
    profile = Path(tempfile.gettempdir()) / f"dt-bili-login-{os.getpid()}"
    profile.mkdir(parents=True, exist_ok=True)

    log(f"启动 {browser}（调试端口 {port}）")
    try:
        proc = subprocess.Popen(
            [
                browser,
                f"--remote-debugging-port={port}",
                f"--user-data-dir={profile}",
                "--no-first-run",
                "--no-default-browser-check",
                "--new-window",
                _LOGIN_PAGE,
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except Exception as exc:  # noqa: BLE001
        log(f"启动浏览器失败: {exc!r}")
        _cleanup(profile, None)
        return {}

    try:
        found = _read_cookies_via_cdp(port, timeout)
        if "SESSDATA" in found:
            cred.save_cookies(found)
            log("已取到 SESSDATA，登录成功")
            return found
        # ★ 超时也要说清原因。前端据此区分「用户没扫」与「扫了但没成」，
        # 而笼统的「没有取到登录态」会让用户以为是自己操作错了。
        log("等待超时：始终没有取到 SESSDATA（未扫码，或扫码未成功）")
        return {}
    except Exception as exc:  # noqa: BLE001 - login is optional, never fatal
        log(f"登录过程异常: {exc!r}")
        return {}
    finally:
        _cleanup(profile, proc)


async def status() -> dict[str, str]:
    """Report whether a Bilibili login exists and whether it still works."""

    if not cred.is_logged_in():
        return {"logged_in": "false", "valid": "false", "detail": "未登录"}
    check = await cred.check_valid()
    return {
        "logged_in": "true",
        "valid": check.get("ok", "unknown"),
        "detail": check.get("detail", ""),
    }


def clear_login() -> bool:
    return cred.clear_cookies()