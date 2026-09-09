# DeepTutor Desktop for Windows — 架构说明

> 与 [`HKUDS/DeepTutorDesktop`](https://github.com/HKUDS/DeepTutorDesktop) (macOS Swift) 平行设计。

## 1. 设计目标

1. **零侵入主项目** —— DeepTutor 主仓库不修改一行代码。集成方式全部走子进程 + HTTP/WebSocket + 自定义协议。
2. **平行 macOS 端** —— Swift 端做"原生窗口 + 子进程拉起 + 本地推理/IMA 桥接",Windows 端做等价的 Tauri/Rust 实现。
3. **轻量分发** —— 安装包目标 25MB 以内(对比 Electron 150MB+),适合普通用户通过 GitHub Releases 下载。
4. **Python 环境隔离** —— 不污染用户全局 Python,通过 uv/venv 创建壳层专属环境。

## 2. 系统拓扑

```
┌─────────────────────────────────────────────────────────────┐
│                    Windows 用户桌面                          │
│                                                             │
│  ┌──────────────────────┐    HTTP/WebSocket                 │
│  │  DeepTutor Shell     │   ─────────────────────────────┐ │
│  │  (Tauri 2 + WebView2)│                                  │ │
│  │                      │                                  ▼ │
│  │  - main window       │   ┌─────────────────────────────────┐
│  │  - system tray       │   │  deeptutor serve (子进程)        │
│  │  - autostart toggle  │   │                                  │
│  │  - LM Studio manager │   │  - FastAPI :8001                 │
│  │  - IMA bridge        │   │  - Next.js 16 standalone :3782  │
│  │  - log panel         │   │  - Auth (JWT) + WebSocket        │
│  └──────────────────────┘   │  - LM Studio client (内部调用)   │
│           │                 │                                  │
│           ▼                 │  数据/记忆/PocketBase(可选)       │
│  ┌──────────────────────┐   └─────────────────────────────────┘
│  │  Python 3.11+ venv   │           ▲
│  │  - deeptutor (pip)   │           │ OpenAI-compatible
│  └──────────────────────┘   ┌───────┴──────────────────┐
│                             │  LM Studio / Ollama      │
│                             │  :1234 (HTTP)            │
│                             └──────────────────────────┘
└─────────────────────────────────────────────────────────────┘
```

## 3. 进程模型

启动时序(M1 阶段实现,M0 已留接口):

```
Shell start
  └─> python --version 探测
        └─< 缺失 -> 引导安装/选择解释器
  └─> pip show deeptutor
        └─< 缺失 -> `pip install -U deeptutor`
  └─> spawn: python -m deeptutor.api.run_server (默认只起后端:8001)
              (捕获 stdout/stderr → shell Tauri event "log")
              (健康轮询 :8001(/docs) 与 :3782(/) → state machine: Booting/Ready/Crashed)
  └─> WebView2 navigate http://127.0.0.1:3782
        └─< 200 -> 显示主界面
        └─< 持续 5xx -> 显示"启动失败"对话框,引导查看日志
```

退出时:

```
Shell quit
  └─> SIGTERM deeptutor process
  └─> 等待 5s,仍未退出 -> SIGKILL
  └─> 关闭 WebView2
```

## 4. 模块职责

| 模块 | 文件 | 职责 | 状态 |
|------|------|------|------|
| 入口 | `main.rs` | 创建 Tauri builder、注册 commands、启动托盘 | M0 ✅ stub |
| 后端 - Python | `backend/python.rs` | 解释器探测、venv 创建 | M0 ✅ stub |
| 后端 - Runner | `backend/runner.rs` | 子进程 spawn、日志回灌、健康状态机 | M0 ✅ stub |
| 后端 - Health | `backend/health.rs` | 端口轮询、健康事件 | M0 ✅ stub |
| LM Studio | `lmstudio/detect.rs` | 启动时探测 :1234 | M0 ✅ stub |
| LM Studio | `lmstudio/models.rs` | 模型列表/加载/卸载 API | M1 🚧 |
| IMA | `ima/protocol.rs` | 自定义协议注册 | M0 ✅ stub |
| IMA | `ima/com.rs` | COM 桥接(可选) | M1 🚧 |
| 托盘 | `tray.rs` | 系统托盘菜单 | M0 ✅ stub |
| 自启动 | `autostart.rs` | HKCU Run 注册表 | M0 ✅ stub |

## 5. 与 macOS 端平行映射

| 关注点 | macOS Swift | Windows Tauri |
|--------|-------------|---------------|
| 主窗口 | `SwiftUI App` + `NSWindow` | `tauri::WebviewWindow` |
| WebView | `WKWebView` 加载 `file://` 或 `http://127.0.0.1:3782` | `WebView2` 加载 `http://127.0.0.1:3782` |
| 托盘 | `NSStatusItem` + `LSUIElement` | `tauri_plugin_system_tray` |
| 自启动 | `LaunchAgents/*.plist` | HKCU\...\Run 注册表 |
| 子进程 | `Process` (Foundation) | `tokio::process` |
| 模型管理 | `URLSession` + JSON | `reqwest` + JSON |
| 日志回灌 | `Pipe` + Combine | Tauri Event + `tauri_plugin_log` |
| 单一实例 | `NSApplication.shared` 检查 | `tauri_plugin_single_instance` |
| 自动更新 | SwiftUpdater 或自定义 | `tauri_plugin_updater` |
| 安装包 | `.app` + `.dmg` | `.msi` + `.exe`(NSIS) |

## 6. 安全模型

- Tauri 2 capabilities 严格限制:`src-tauri/capabilities/main.json` 仅允许必要的 plugin + command
- CSP 严格:`default-src 'self'; connect-src 'self' http://127.0.0.1:* https://*`
- WebView 关闭右键菜单、禁止外部导航(`webview_request_navigation` 拦截非白名单 URL)
- 所有 invoke command 必须经过 Rust 校验

## 7. 升级路径

- 主项目升级:`pip install -U deeptutor`,壳无需修改
- 壳自身升级:通过 `tauri-plugin-updater` 从 GitHub Releases 拉 `latest.json`,增量替换
- Python 解释器升级:`scripts/bootstrap-python.ps1` 提供切换解释器入口

## 8. 已知约束

| 约束 | 缓解 |
|------|------|
| WebView2 在 Win10 1809 以下缺失 | 安装包内嵌 evergreen bootstrapper(可选 build feature) |
| 用户没装 Python | 壳首次启动引导;或计划 v2 提供内置嵌入版(Python embeddable + deeptutor wheel) |
| 腾讯 IMA 桌面端可能不响应 deep link | 提供剪贴板注入降级方案 |
| LM Studio 多实例共存(端口冲突) | 探测冲突并询问用户切换端口 |
