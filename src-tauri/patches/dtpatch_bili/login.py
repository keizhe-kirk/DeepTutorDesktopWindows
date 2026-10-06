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


def _read_cookies_via_cdp(port: int, timeout: float) -> dict[str, str]:
    """Open a CDP target on bilibili and dump its cookies."""

    deadline = time.monotonic() + timeout
    ws_url = ""
    while time.monotonic() < deadline:
        info = _http_json(f"http://127.0.0.1:{port}/json/list")
        if isinstance(info, list):
            for target in info:
                if not isinstance(target, dict):
                    continue
                url = str(target.get("url") or "")
                if "bilibili.com" in url and target.get("webSocketDebuggerUrl"):
                    ws_url = str(target["webSocketDebuggerUrl"])
                    break
                if not ws_url and target.get("webSocketDebuggerUrl") and target.get("type") == "page":
                    ws_url = str(target["webSocketDebuggerUrl"])
            if ws_url:
                break
        time.sleep(0.6)
    if not ws_url:
        return {}

    ws = _WS(ws_url)
    try:
        ws.send(json.dumps({"id": 1, "method": "Network.getAllCookies"}))
        for _ in range(12):
            message = json.loads(ws.recv())
            if message.get("id") == 1:
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


def login(timeout: float = 300.0) -> dict[str, str]:
    """Open a browser, wait for the user to scan, persist the cookies.

    Never raises; returns the collected cookies (``{}`` on failure/timeout).
    """

    browser = find_browser()
    if not browser:
        return {}

    port = _free_port()
    # 独立 profile：不污染用户日常浏览数据，退出时整个目录可丢。
    profile = Path(tempfile.gettempdir()) / f"dt-bili-login-{os.getpid()}"
    profile.mkdir(parents=True, exist_ok=True)

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

    try:
        found = _read_cookies_via_cdp(port, timeout)
        if "SESSDATA" in found:
            cred.save_cookies(found)
            return found
        return {}
    except Exception:  # noqa: BLE001 - login is optional, never fatal
        return {}
    finally:
        try:
            proc.terminate()
            proc.wait(timeout=8)
        except Exception:  # noqa: BLE001
            try:
                proc.kill()
            except Exception:  # noqa: BLE001
                pass
        shutil.rmtree(profile, ignore_errors=True)


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