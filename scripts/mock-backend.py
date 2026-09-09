#!/usr/bin/env python
"""DeepTutor Desktop 壳层的假后端,仅用于端到端验证启动链路。

它会:
  1. 先睡 2 秒,模拟 uv / Next.js 的冷启动
  2. 在 :8001 提供 /api/health(FastAPI 角色)
  3. 在 :3782 提供一个简单页面(Next.js standalone 角色)
  4. 持续往 stdout/stderr 打日志,验证壳层的日志回流

用法:
    set DEEPTUTOR_BACKEND_SCRIPT=D:\\code\\DeepTutorDesktopWin\\scripts\\mock-backend.py
    pnpm tauri dev
"""

import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

API_PORT = int(os.environ.get("DEEPTUTOR_API_PORT", "8001"))
WEB_PORT = int(os.environ.get("DEEPTUTOR_WEB_PORT", "3782"))

# 访问日志写到文件:壳层的 WebView 一旦导航过来,这里就会出现记录,
# 这是"启动链路跑通"的客观证据(子进程的 stdout 被壳层接管,外部看不到)。
ACCESS_LOG = os.path.join(
    os.environ.get("TEMP") or os.path.expanduser("~"), "deeptutor-mock-access.log"
)
_LOG_LOCK = threading.Lock()

PAGE = """<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8">
<title>DeepTutor (mock)</title>
<style>
body{margin:0;height:100vh;display:flex;align-items:center;justify-content:center;
font-family:'Segoe UI',sans-serif;background:#0f1f1a;color:#c8f5e4}
.card{text-align:center}
h1{font-weight:500;font-size:22px;margin:0 0 8px}
p{margin:0;color:#6fbf9f;font-size:13px}
.tag{display:inline-block;margin-top:18px;padding:4px 10px;border:1px solid #2f6b57;
border-radius:999px;font-size:11px;color:#5dcaa5}
</style></head>
<body><div class="card">
<h1>DeepTutor Web UI</h1>
<p>由 mock-backend.py 提供,用于验证壳层启动链路</p>
<div class="tag">API :%d &nbsp;·&nbsp; Web :%d</div>
</div></body></html>""" % (API_PORT, WEB_PORT)


class Handler(BaseHTTPRequestHandler):
    def _send(self, code: int, body: bytes, ctype: str) -> None:
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):  # noqa: N802
        # 用 User-Agent 区分请求来源:
        #   reqwest(壳层探活)不自带 UA -> "-"
        #   WebView2 导航过来的请求 -> "Mozilla/5.0 ... Edg/..."
        ua = self.headers.get("User-Agent", "-")
        with _LOG_LOCK:
            try:
                with open(ACCESS_LOG, "a", encoding="utf-8") as f:
                    f.write(
                        "%s GET :%d %s | ua=%s\n"
                        % (
                            time.strftime("%H:%M:%S"),
                            self.server.server_address[1],
                            self.path,
                            ua,
                        )
                    )
            except OSError:
                pass
        if self.server.server_address[1] == API_PORT:
            payload = json.dumps(
                {"status": "ok", "service": "deeptutor-api", "path": self.path}
            ).encode()
            self._send(200, payload, "application/json")
        else:
            self._send(200, PAGE.encode("utf-8"), "text/html; charset=utf-8")

    def log_message(self, fmt, *args):  # 访问日志走 stderr,模拟真实后端
        sys.stderr.write("[mock] %s\n" % (fmt % args))
        sys.stderr.flush()


def serve(port: int) -> None:
    httpd = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print("[mock] listening on 127.0.0.1:%d" % port, flush=True)
    httpd.serve_forever()


def main() -> int:
    print("[mock] booting, simulating cold start (2s)...", flush=True)
    time.sleep(2)

    for port in (API_PORT, WEB_PORT):
        t = threading.Thread(target=serve, args=(port,), daemon=True)
        t.start()
        time.sleep(0.2)

    print("[mock] ready. api=%d web=%d" % (API_PORT, WEB_PORT), flush=True)
    n = 0
    while True:
        n += 1
        print("[mock] heartbeat %d" % n, flush=True)
        time.sleep(5)


if __name__ == "__main__":
    sys.exit(main())
