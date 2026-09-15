# DeepTutor Desktop for Windows

[![Release](https://img.shields.io/github/v/release/keizhe-kirk/DeepTutorDesktopWindows?label=release&color=2ea3a3)](https://github.com/keizhe-kirk/DeepTutorDesktopWindows/releases/latest)
[![Build](https://img.shields.io/github/actions/workflow/status/keizhe-kirk/DeepTutorDesktopWindows/release.yml?label=build)](https://github.com/keizhe-kirk/DeepTutorDesktopWindows/actions)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](./LICENSE)
[![Platform](https://img.shields.io/badge/platform-Windows%2010%2F11-0078D4?logo=windows)](#)

**DeepTutor 的 Windows 原生桌面端** —— 与 HKUDS/DeepTutorDesktop (macOS, Swift) 平行设计,
零侵入 DeepTutor 主仓库:所有集成通过子进程 + HTTP/WebSocket + 自定义协议完成。

> 主项目:https://github.com/HKUDS/DeepTutor
> 平行参考(macOS):https://github.com/HKUDS/DeepTutorDesktop

## ⬇️ 下载安装

到 **[Releases](https://github.com/keizhe-kirk/DeepTutorDesktopWindows/releases/latest)** 下载
`DeepTutor_<版本>_x64-setup.exe`,双击安装即可。

> ### 🎁 免依赖:安装包里已经装好一切
>
> **不需要预装 Python、pip、Node.js,也不需要装任何 VC++ 运行库。**
> 安装包约 225 MB —— 因为它真的把整套运行时搬了进去:
>
> | 内置组件 | 版本/体积 | 作用 |
> |---|---|---|
> | relocatable CPython | 3.13(约 45 MB) | 跑 DeepTutor 后端 |
> | `deeptutor` 及其全部依赖 | 含 Next.js 前端产物(约 400 MB) | FastAPI 后端 + Web UI |
> | Node.js | v22 LTS 单文件 `node.exe`(约 84 MB) | 跑 deeptutor 附带的 Next.js standalone |
>
> 壳层优先使用**内置**运行时,探测不到时才回退系统环境(方便 `tauri dev` 与排障,
> 可用 `DEEPTUTOR_RUNTIMES` 覆盖)。
>
> 安装后运行数据落在 `%LOCALAPPDATA%\DeepTutor`,不污染用户主目录;
> 应用内置自动更新,后续版本一键升级。

**系统要求**:Windows 10 1903+ / Windows 11,64 位。WebView2 运行时就绪(Win11 及新版 Win10 已内置)。

---

## 架构一句话

```
WebView2 窗口 ──http://127.0.0.1:3782──> Next.js 16 standalone ──/api/* ws/*──> FastAPI :8001 (内置 CPython 3.13)
   │                                       │
   │                                       └──> LM Studio :1234 / Ollama / vLLM (OpenAI 兼容)
   │
   └──>  Tauri 2 (Rust) ──> deeptutor start --child (子进程管理 + 日志回灌 + 作业对象兜底)
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
| Python 后端 | 来自 `HKUDS/DeepTutor`,**随安装包内置**(自带 CPython,不修改主项目) |
| 前端运行时 | **随安装包内置** Node.js,用于跑 deeptutor 附带的 Next.js standalone |
| 推理侧 | LM Studio(Ollama / vLLM 同协议) |

> **自包含设计**:正式安装包内含 `runtimes\python`(relocatable CPython + deeptutor 及其全部依赖)
> 与 `runtimes\node`(node.exe)。**终端用户无需预装 Python、pip 或 Node.js**,装完即用。
> 壳层优先使用内置运行时,找不到时才回退到系统 Python(便于 `tauri dev` 与排障)。

---

## 开发前置

> **只使用安装包的用户什么都不需要装。** 下面这些是"想自己编译"才需要的。

- Node.js 20+(推荐 22)
- pnpm 10+
- Rust stable(通过 `rustup` 安装,profile = default)
- Python 3.11+ —— **可选**,仅在开发模式下回退使用;打包时会自动下载内置解释器
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
git clone https://github.com/keizhe-kirk/DeepTutorDesktopWindows.git
cd DeepTutorDesktopWindows

# 2. 安装前端壳 UI 依赖
pnpm install

# 3. 开发模式(自动启动 Tauri + Rust 热重载 + WebView 直连 DeepTutor 后端)
#    这一步会回退到系统 Python;若系统没有 deeptutor,先跑 scripts\bootstrap-python.ps1
pnpm tauri dev

# 4. 生产构建:先把内置运行时组装到 src-tauri\runtimes\(约 600 MB,只需做一次)
pwsh -ExecutionPolicy Bypass -File scripts\fetch-runtimes.ps1
#    国内加速: 追加 -IndexUrl https://pypi.tuna.tsinghua.edu.cn/simple

# 5. 出安装包(NSIS .exe,落到 src-tauri\target\release\bundle\nsis\)
pnpm tauri build --bundles nsis
```

<details>
<summary><code>fetch-runtimes.ps1</code> 做什么</summary>

| 步骤 | 内容 |
|------|------|
| 下载 | python-build-standalone 的 relocatable CPython 3.13(win x64) |
| 解压 | 剥掉顶层目录,落到 `src-tauri/runtimes/python/` |
| 安装 | 用该解释器执行 `pip install deeptutor==X.Y.Z`,依赖与前端产物(`deeptutor_web`)一并装进它自己的 `Lib/site-packages` |
| 下载 | Node.js 官方单文件 `node.exe`,落到 `src-tauri/runtimes/node/` |

产物已被 `.gitignore` 忽略;`bundle.resources` 会把整个 `runtimes/` 打进安装包。
升级内置 deeptutor 只需 `pwsh ... -DeepTutorSpec "deeptutor==1.6.9" -Force`。

</details>

---

## 启动流程(M1 已实现)

壳层启动后按这条状态机推进,每一步都会把 `backend://state` 事件推给启动页:

```
BundledRuntimes  定位安装包内置运行时(DEEPTUTOR_RUNTIMES → 资源目录/runtimes → exe 同级/runtimes)
      │
LocatingPython   探测解释器(DEEPTUTOR_PYTHON → **内置 python.exe** → PATH → py -3.xx → 常见安装路径),要求 >= 3.11
      │
CheckingDeps     用 importlib.util.find_spec('deeptutor') 检查后端包
      │
Starting         python -m deeptutor start --no-browser(Windows 下 CREATE_NO_WINDOW,UTF-8/无缓冲)
      │          · 子进程 PATH 最前面插入 runtimes\node → deeptutor 的 shutil.which("node") 命中内置 Node
      │          · 注入 DEEPTUTOR_HOME=%LOCALAPPDATA%\DeepTutor,运行数据不再污染用户主目录
      │          · 注入 PYTHONPYCACHEPREFIX,避免向只读的 Program Files 写 __pycache__
      │          stdout/stderr 逐行 emit `backend://log`,UI 实时滚动
Probing          每 500ms 探 :8001(/docs → /api/health → /health → /)与 :3782(/)
      │
Ready            WebView 导航到 http://127.0.0.1:<web_port>
   └─ 任一环节失败 → Failed,UI 给出可执行修复建议 + 一键重试
```

> **注意**:`deeptutor start --no-browser` 会同时起后端(:8001)与前端(:3782),
> `--no-browser` 阻止它自己弹系统浏览器 —— 界面由壳层的 WebView2 显示。

### 窗口与后端的生命周期绑定

后端是一条进程链(`python -m deeptutor start` → `uvicorn` / `node server.js`),
清理不干净就会变成占着端口的孤儿进程,下次启动静默连到旧服务上。三道防线:

| 防线 | 机制 | 覆盖场景 |
|------|------|----------|
| 1. 关闭主窗口 | `CloseRequested` 上同步杀进程树后 `app.exit(0)` | 用户点 X |
| 2. 正常退出 | `RunEvent::Exit` → `kill_tree_sync()` | 托盘"退出"、`app.exit()` |
| 3. **作业对象**(根治) | 子进程加入 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 作业;壳进程一死,内核连带结束整棵树 | 任务管理器强杀、崩溃、`panic = "abort"`、更新器接管后 `process::exit` |

第 3 条是关键:**它不依赖任何用户态回调,因此无法被绕过**。
另外启动时会读取上次记录的 pid,回收旧版本遗留的孤儿进程(仅当存活进程的
映像路径与记录完全一致时才动手,避免 PID 复用误杀)。

> 语义约定:点 X = 退出应用(连带停后端);只想隐藏窗口请用托盘的"打开/隐藏主窗口"。

### 启动配置(全部可用环境变量覆盖)

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `DEEPTUTOR_RUNTIMES` | 自动探测 | 直接指定内置运行时目录(内含 `python/` 与 `node/`) |
| `DEEPTUTOR_PYTHON` | 自动探测 | 直接指定解释器路径(优先级高于内置运行时) |
| `DEEPTUTOR_HOME` | `%LOCALAPPDATA%\DeepTutor` | deeptutor 运行数据根目录;用户已设置时不覆盖 |
| `DEEPTUTOR_MODULE` | `deeptutor` | `python -m <module>` |
| `DEEPTUTOR_ARGS` | `start --no-browser` | 传给模块的参数 |
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
│  │  │  ├─ config.rs          #   启动配置(环境变量可覆盖)
│  │  │  ├─ runtime.rs         #   内置运行时(Python/Node)定位
│  │  │  ├─ winproc.rs         #   作业对象 + 进程映像查询(退出兜底)
│  │  │  ├─ python.rs          #   解释器探测
│  │  │  ├─ runner.rs          #   子进程生命周期
│  │  │  ├─ health.rs          #   双端口健康探测
│  │  │  └─ boot.rs            #   启动状态机
│  │  ├─ lmstudio/             # LM Studio 客户端
│  │  ├─ ima/                  # 腾讯 IMA 桥接
│  │  ├─ tray.rs               # 系统托盘
│  │  └─ autostart.rs          # 注册表自启动
│  ├─ runtimes/                # 内置运行时(由 fetch-runtimes.ps1 生成,git 忽略)
│  │  ├─ python/               #   CPython + deeptutor 及其依赖 + 前端产物
│  │  └─ node/                 #   node.exe
│  ├─ tauri.conf.json
│  ├─ capabilities/            # Tauri 2 capabilities 授权
│  └─ Cargo.toml
├─ src/                        # 壳层 React UI(启动页/状态面板)
├─ scripts/
│  ├─ fetch-runtimes.ps1      # 组装内置 Python + deeptutor + Node(打包必跑)
│  ├─ bootstrap-python.ps1    # 开发模式:引导系统 Python 环境(可选)
│  └─ fetch-web.ps1           # 拉取/同步 DeepTutor web 资源(可选)
├─ .github/workflows/
│  └─ release.yml             # tag 触发,组装运行时 → 出 NSIS + auto-update json
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

## 里程碑

| 里程碑 | 内容 | 状态 |
|--------|------|------|
| **M0** | 脚手架就绪:Rust 工程 + Tauri 配置 + 模块占位 | ✅ |
| **M1** | 后端拉起 + 启动页:Python 探测、`deeptutor start --no-browser`、双端口健康探测、UI 启动动画 | ✅ |
| **M2** | LM Studio + 托盘:模型列表/加载/卸载(原生 `/api/v0/*`)、托盘菜单、启动页模型管理面板 | ✅ |
| **M3** | 打包 + 自动更新:NSIS 安装包、`latest.json`、前端检查更新 | ✅ |
| **M4** | 正式图标:毕业帽 + 神经节点 + 青蓝渐变(`tauri icon` 多尺寸 RGBA + ICO) | ✅ |
| **M5** | 自包含安装包 + 进程生命周期根治(**当前版本 v0.2.1**) | ✅ |

**M5 做了两件大事:**

1. **真·自包含** —— 安装包内置 relocatable CPython 3.13 + deeptutor(含前端产物)+ Node.js v22,
   终端用户零前置依赖。安装包从 2.5 MB 涨到 225 MB,涨的这部分就是运行时。
   新增 `scripts/fetch-runtimes.ps1`(幂等、带缓存、会强校验 `deeptutor_web/server.js` 是否随 wheel 落地)
   与 `src-tauri/src/backend/runtime.rs`(`BundledRuntimes::detect`,多候选目录容错)。
2. **关窗口 = 关后端** —— 修复"关掉窗口后 python/node 仍占着 8001/3782 变成孤儿进程"。
   三道防线见下文[窗口与后端的生命周期绑定](#窗口与后端的生命周期绑定),其中
   Windows **作业对象**(`KILL_ON_JOB_CLOSE`)由内核保证:壳进程一消失,整棵子进程树连带回收,
   覆盖强杀/崩溃/panic 等所有用户态回调来不及执行的路径。

MSI 目标保留在配置中但暂不上——本地 `light.exe` 被杀软锁,NSIS 已满足分发需求。

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

## 自动更新

壳层通过 `tauri-plugin-updater` 走 GitHub Releases 自动更新。**三个条件缺一不可**,
任何一个没配好都会退化成"能打包但更新装不上":

| 环节 | 要求 | 位置 |
|------|------|------|
| ① 打包开关 | `bundle.createUpdaterArtifacts: true` —— 不开就**不会生成 `.sig`**,即使配了密钥也没用 | `src-tauri/tauri.conf.json` |
| ② 签名密钥 | `TAURI_SIGNING_PRIVATE_KEY` + `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 两个 CI secret,均**不可为空** | GitHub → Settings → Secrets → Actions |
| ③ 公钥 | 与私钥配对的公钥(base64) | `tauri.conf.json` → `plugins.updater.pubkey` |
| 更新源 | `https://github.com/keizhe-kirk/DeepTutorDesktopWindows/releases/latest/download/latest.json` | `tauri.conf.json` → `plugins.updater.endpoints` |
| 前端入口 | 启动页"检查更新"按钮 → `check_for_update` / `install_update` 两个自定义命令 | `src/App.tsx` → `lib.rs` |

> 密钥文件请放在仓库外(如 `~/.tauri/`),**绝不要提交**;密码只写在 GitHub Secrets 里,
> 不要出现在任何文档或聊天记录中。
>
> 本地生成密钥对:
> ```bash
> npx @tauri-apps/cli@^2 signer generate -w ~/.tauri/deeptutor.key
> ```
> 用 `-p ""`(空密码)会让 `signer sign` 等待 stdin 而挂起,而 GitHub Secrets 又不接受空值 ——
> 因此**务必设置非空密码**。

**发布新版本步骤:**

1. 三个版本号改齐:`package.json` / `src-tauri/Cargo.toml`(+ `Cargo.lock`)/ `src-tauri/tauri.conf.json`
2. 提交并推送到 `main`
3. `git tag -a vX.Y.Z -m "..." && git push origin vX.Y.Z` → 触发 CI:`fetch-runtimes.ps1` 组装运行时 → 构建签名 NSIS → 生成 `latest.json` → 建 Release
4. **验收**:Release 里必须能看到 `DeepTutor_X.Y.Z_x64-setup.exe`、`.exe.sig`、`latest.json`,
   且 `latest.json` 的 `signature` 字段**非空**。CI 已内置硬校验,缺签名会直接失败而不会静默发出坏包。

> 本地手动签名单个文件:`npx @tauri-apps/cli@^2 signer sign -f <私钥> -p <密码> <file>`
>
> ⚠️ 历史包袱:v0.1.x ~ v0.2.0 的更新包签名是空的(缺 `createUpdaterArtifacts` + 无密钥),
> 这些版本**无法自动更新**,需要手动装一次 v0.2.1 及以后版本,之后才能接力自动升级。

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
