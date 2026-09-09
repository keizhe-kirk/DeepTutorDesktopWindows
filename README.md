# DeepTutor Desktop for Windows

**DeepTutor 的 Windows 原生外壳**,与 HKUDS/DeepTutorDesktop (macOS, Swift) 平行设计。
零侵入 DeepTutor 主仓库——所有集成通过子进程 + HTTP/WebSocket + 自定义协议完成。

> 主项目:https://github.com/HKUDS/DeepTutor
> 平行参考(macOS):https://github.com/HKUDS/DeepTutorDesktop

---

## 架构一句话

```
WebView2 窗口 ──http://127.0.0.1:3782──> Next.js 16 standalone ──/api/* ws/*──> FastAPI :8001 (Python 3.11+)
   │                                       │
   │                                       └──> LM Studio :1234 / Ollama / vLLM (OpenAI 兼容)
   │
   └──>  Tauri 2 (Rust) ──> deeptutor start --child (子进程管理 + 日志回灌)
                      └──> LM Studio 模型管理 / 腾讯 IMA 自定义协议 / 系统托盘
```

详细架构与模块职责见 [ARCHITECTURE.md](./ARCHITECTURE.md)。

---

## 技术栈

| 层 | 技术 |
|----|------|
| 窗口与渲染 | Tauri 2 + WebView2(Win10/11 内置 Edge) |
| 系统层 | Rust 1.78+(stable) |
| 前端壳 UI | React 18 + TypeScript + Vite(仅 1 页启动/状态 UI,主界面用 WebView 回环加载) |
| Python 后端 | 来自 `HKUDS/DeepTutor`,通过 pip install 引导,**不修改主项目** |
| 推理侧 | LM Studio(Ollama / vLLM 同协议) |

---

## 开发前置

- Node.js 20+(推荐 22)
- pnpm 10+
- Rust stable(通过 `rustup` 安装,profile = default)
- Python 3.11+(DeepTutor 主项目要求)
- Windows 10 1903+(WebView2 Runtime 内置)或手动安装 evergreen runtime

### 推荐 Rust 安装命令

```powershell
# 在 C:\Windows\Temp\ 执行(或任意非工程目录)
Invoke-WebRequest -UseBasicParsing https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe -OutFile rustup-init.exe
.\rustup-init.exe -y --default-toolchain stable --default-host x86_64-pc-windows-msvc --profile default
# 安装完成后新开一个 shell,运行: cargo --version
```

---

## 快速开始

```bash
# 1. 克隆并进入
cd D:\code
git clone https://github.com/HKUDS/DeepTutorDesktopWin.git
cd DeepTutorDesktopWin

# 2. 安装前端壳 UI 依赖
pnpm install

# 3. 引导 DeepTutor 主项目(pip 安装 deeptutor)
pwsh -ExecutionPolicy Bypass -File scripts\bootstrap-python.ps1

# 4. 开发模式(自动启动 Tauri + Rust 热重载 + WebView 直连 DeepTutor 后端)
pnpm tauri dev

# 5. 生产构建(产出 .msi + .exe 安装包到 src-tauri\target\release\bundle\)
pnpm tauri build
```

---

## 启动流程(M1 已实现)

壳层启动后按这条状态机推进,每一步都会把 `backend://state` 事件推给启动页:

```
LocatingPython   探测 Python(DEEPTUTOR_PYTHON → PATH → py -3.xx → 常见安装路径),要求 >= 3.11
      │
CheckingDeps     用 importlib.util.find_spec('deeptutor') 检查后端包
      │
Starting         python -m deeptutor.api.run_server(Windows 下 CREATE_NO_WINDOW,UTF-8/无缓冲)
      │          stdout/stderr 逐行 emit `backend://log`,UI 实时滚动
Probing          每 500ms 探 :8001(/docs → /api/health → /health → /)与 :3782(/)
      │
Ready            WebView 导航到 http://127.0.0.1:<web_port>
   └─ 任一环节失败 → Failed,UI 给出可执行修复建议 + 一键重试
```

> **注意**:DeepTutor 主项目默认 `deeptutor start` 同时起后端(:8001)+ 前端(:3782),
> 但桌面壳**只起后端**(壳层自带 WebView2 负责前端显示),因此使用 `deeptutor.api.run_server` 模块。
> 如需同时起前端(调试/开发),可设 `DEEPTUTOR_MODULE=deeptutor` + `DEEPTUTOR_ARGS=start`。

### 启动配置(全部可用环境变量覆盖)

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `DEEPTUTOR_PYTHON` | 自动探测 | 直接指定解释器路径 |
| `DEEPTUTOR_MODULE` | `deeptutor.api.run_server` | `python -m <module>`(DeepTutor 真实后端入口) |
| `DEEPTUTOR_ARGS` | 空 | 传给模块的参数(run_server 不需要) |
| `DEEPTUTOR_BACKEND_SCRIPT` | 空 | 直接跑某个 .py(调试 / mock 用) |
| `DEEPTUTOR_API_PORT` | `8001` | FastAPI 端口 |
| `DEEPTUTOR_WEB_PORT` | `3782` | Next.js 端口 |
| `DEEPTUTOR_STARTUP_TIMEOUT_MS` | `120000` | 等待就绪的超时 |
| `DEEPTUTOR_SKIP_DEP_CHECK` | `0` | 跳过 deeptutor 导入检查 |

### 没有装 DeepTutor 也能验证链路

```powershell
$env:DEEPTUTOR_BACKEND_SCRIPT = "D:\code\DeepTutorDesktopWin\scripts\mock-backend.py"
$env:DEEPTUTOR_SKIP_DEP_CHECK = "1"
pnpm tauri dev
```

`scripts/mock-backend.py` 会在 :8001 / :3782 起两个假服务(并模拟 2 秒冷启动),
并把访问日志写到 `%TEMP%\deeptutor-mock-access.log` —— 出现 `GET :3782 /`
就说明"探测 → 拉起 → 探活 → 导航"整条链路真的跑通了。

> **注意**:真实 `deeptutor.api.run_server` 只起后端(:8001),前端(:3782)需由壳层 WebView 加载(或用 `deeptutor start` 同时起)。
> mock 模拟了两个端口以验证壳层完整的双端口探活逻辑。

---

## 目录结构

```
DeepTutorDesktopWin/
├─ src-tauri/                  # Rust 主程序
│  ├─ src/
│  │  ├─ main.rs               # 入口
│  │  ├─ backend/              # Python 子进程管理
│  │  ├─ lmstudio/             # LM Studio 客户端
│  │  ├─ ima/                  # 腾讯 IMA 桥接
│  │  ├─ tray.rs               # 系统托盘
│  │  └─ autostart.rs          # 注册表自启动
│  ├─ tauri.conf.json
│  ├─ capabilities/            # Tauri 2 capabilities 授权
│  └─ Cargo.toml
├─ src/                        # 壳层 React UI(启动页/状态面板)
├─ scripts/
│  ├─ bootstrap-python.ps1    # Python 环境引导 + deeptutor 安装
│  └─ fetch-web.ps1           # 拉取/同步 DeepTutor web 资源(可选)
├─ installer/                  # 安装包模板
│  ├─ wix/
│  └─ nsis/
├─ .github/workflows/
│  └─ release.yml             # tag 触发,出 MSI + NSIS + auto-update json
├─ ARCHITECTURE.md             # 架构详细说明
└─ README.md
```

---

## 与 DeepTutor 主项目的关系

| 资源 | 来源 | 集成方式 |
|------|------|----------|
| FastAPI 后端(`deeptutor/`) | `HKUDS/DeepTutor` | `pip install -U deeptutor`(PyPI)或 `pip install -e .`(源码) |
| Web 前端(`web/`) | `HKUDS/DeepTutor` | **不重新打包**,WebView 直连 `http://127.0.0.1:3782` |
| LM Studio 集成 | DeepTutor `Settings → Models` 已内置 | 壳层额外提供"启动/切换模型"快捷操作 |

升级主项目时无需修改本仓库——`pnpm update deeptutor` 然后重启壳即可。

---

## 里程碑(M0/M1/M2/M3)

- **M0** ✅ 脚手架就绪:Rust 工程 + Tauri 配置 + 模块占位
- **M1** ✅ 后端拉起 + 启动页:Python 引导、`deeptutor start --child`、健康探测、UI 启动动画
- **M2** ✅ LM Studio + 托盘:模型列表/加载/卸载(原生 `/api/v0/*`)、托盘菜单、启动页模型管理面板
- **M3** ✅ 打包 + 自动更新:NSIS 安装包已可产出;签名密钥 + `latest.json` + 前端检查更新已接入
- **M4** ✅ 正式图标:毕业帽 + 神经节点 + 青蓝渐变(`assets/logo-source.png` → `tauri icon` 多尺寸 RGBA + ICO);启动页与 splash 都已引用;MSI 待 CI 环境(本地 light.exe 被杀软锁)

当前进度:**M3 + M4 完成(MSI 在 CI 上跑)**

---

## 图标与品牌(M4)

| 文件 | 用途 |
|------|------|
| `assets/logo-source.png` | 1024×1024 主源图(用于 tauri icon 生成全套) |
| `src-tauri/icons/icon.ico` | Windows 安装包/资源管理器/任务栏图标 |
| `src-tauri/icons/{32,64,128,128@2x,256,512}.png` | 各平台图标尺寸 |
| `src-tauri/icons/icon.icns` | macOS(图标用,本项目不部署) |
| `public/logo.png` | 前端启动页 + splash 引用的 PNG |
| `public/favicon.png` | 浏览器/标签页 favicon |

如要替换为新图标:把新源图丢进 `assets/`(≥1024×1024 RGBA PNG),然后:

```bash
pnpm tauri icon assets/<新源图>.png    # 覆盖 src-tauri/icons/*
cp src-tauri/icons/128x128.png public/logo.png
cp src-tauri/icons/32x32.png   public/favicon.png
```

---

## 自动更新(M3)

壳层通过 `tauri-plugin-updater` 走 GitHub Releases 自动更新:

| 环节 | 说明 |
|------|------|
| 签名密钥 | `D:\deeptutor-setup\deeptutor-updater.key`(私钥,勿泄露/勿提交)与 `.key.pub`(公钥已写入 `tauri.conf.json` 的 `plugins.updater.pubkey`) |
| 更新源 | `https://github.com/HKUDS/DeepTutorDesktopWin/releases/latest/download/latest.json` |
| 签名环境变量 | CI 里用 `TAURI_SIGNING_PRIVATE_KEY` + `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`(密码:`deeptutor-release-2026`) |
| 前端入口 | 启动页右下"检查更新"按钮 → `check_for_update` / `install_update` 命令 |

**发布新版本步骤:**

1. 在 GitHub 仓库 Settings → Secrets → Actions 添加:
   - `TAURI_SIGNING_PRIVATE_KEY`:`D:\deeptutor-setup\deeptutor-updater.key` 文件内容
   - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`:`deeptutor-release-2026`
2. 改 `package.json` / `src-tauri/Cargo.toml` / `tauri.conf.json` 三处版本号一致
3. `git tag v0.1.1 && git push --tags` → 触发 CI 出包 + 签名 + 生成 `latest.json` 并上传 release
4. 已安装用户下次点"检查更新"即可收到提示并一键升级

> 本地想手动签名单个文件:`pnpm tauri signer sign -f <私钥路径> -p <密码> <file>`

---

## LM Studio 集成(M2)

壳层通过 LM Studio 本地服务器(默认 `http://127.0.0.1:1234`)管理模型:

| 端点 | 用途 |
|------|------|
| `GET /v1/models` / `GET /api/v0/models` | 探测可用性 |
| `GET /api/v0/models` | 列出所有已下载模型(含 `state` 区分已加载/未加载) |
| `POST /api/v0/models/load` | 加载模型到内存 |
| `POST /api/v0/models/unload` | 从内存卸载 |

- 探测/请求都强制 `.no_proxy()`,避免系统代理劫持 127.0.0.1
- 端口可用 `DEEPTUTOR_LMSTUDIO_URL` 覆盖(如 `http://127.0.0.1:1235`)
- 启动页会显示"LM Studio 已连接 / N 个模型已加载",并提供加载/卸载按钮
- 系统托盘:右键菜单含"打开/隐藏主窗口 / 重启后端 / 退出",左键单击唤起窗口

## 许可证

Apache-2.0(与 DeepTutor 主项目一致)
