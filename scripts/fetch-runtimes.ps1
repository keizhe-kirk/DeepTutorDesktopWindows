# 组装"自包含"运行时 —— 让安装包不再依赖用户机器上的 Python / Node.js
#
# 产物(均位于 src-tauri/runtimes/,已被 .gitignore 忽略,不会进 git):
#   runtimes/python/    relocatable CPython(python-build-standalone)
#                       + deeptutor 及其全部依赖(含 deeptutor_web 前端产物)
#   runtimes/node/      node.exe(官方单文件可执行程序)
#
# 这两个目录会被 tauri.conf.json 的 bundle.resources 打进 NSIS 安装包。
# 壳层启动时会:
#   1. 优先使用 runtimes/python/python.exe 跑 `python -m deeptutor start`
#   2. 把 runtimes/node 前置到子进程 PATH,让 deeptutor 的 shutil.which("node") 命中内置 Node
#
# Usage:
#   pwsh -ExecutionPolicy Bypass -File scripts\fetch-runtimes.ps1
#   pwsh ... -IndexUrl https://pypi.tuna.tsinghua.edu.cn/simple    # 国内加速
#   pwsh ... -DeepTutorSpec "deeptutor==1.6.9"                     # 升级内置版本
#   pwsh ... -Force -SkipNode                                      # 只重建 Python 侧

[CmdletBinding()]
param(
    # python-build-standalone 的 CPython 版本与发行批次
    [string]$PythonVersion = "3.13.15",
    [string]$PbsRelease    = "20260901",
    # Node.js 版本(官方 LTS)
    [string]$NodeVersion   = "v22.23.2",
    # 内置的 deeptutor 版本
    [string]$DeepTutorSpec = "deeptutor==1.6.8",
    # pip 索引源,留空则用官方 PyPI
    [string]$IndexUrl      = "",
    # 强制重新下载/重装
    [switch]$Force,
    [switch]$SkipPython,
    [switch]$SkipNode
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
# 关闭进度条渲染,否则 Invoke-WebRequest 会慢到不可用
$ProgressPreference = "SilentlyContinue"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$Runtimes = Join-Path $RepoRoot "src-tauri\runtimes"
$PyDir    = Join-Path $Runtimes "python"
$NodeDir  = Join-Path $Runtimes "node"
$CacheDir = Join-Path $RepoRoot ".cache\runtimes"

function Write-Step { param([string]$Message) Write-Host "`n==> $Message" -ForegroundColor Cyan }
function Write-Ok   { param([string]$Message) Write-Host "    $Message" -ForegroundColor Green }
function Write-Note { param([string]$Message) Write-Host "    $Message" -ForegroundColor Yellow }
function Write-Dim  { param([string]$Message) Write-Host "    $Message" -ForegroundColor DarkGray }

function Format-MB {
    param([long]$Bytes)
    return "{0:N1} MB" -f ($Bytes / 1MB)
}

# 带缓存与断点重试的下载。优先用 curl.exe(Windows 10+ 自带),比 Invoke-WebRequest 快很多。
function Get-RemoteFile {
    param([string]$Url, [string]$Dest)

    if ((Test-Path $Dest) -and -not $Force) {
        Write-Ok "缓存命中 $(Split-Path -Leaf $Dest) ($(Format-MB (Get-Item $Dest).Length))"
        return
    }

    $parent = Split-Path -Parent $Dest
    if (-not (Test-Path $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }

    Write-Dim "GET $Url"
    $partial = "$Dest.partial"
    if (Test-Path $partial) { Remove-Item $partial -Force }

    & curl.exe -L --fail --retry 3 --retry-delay 2 --connect-timeout 30 -o $partial $Url
    if ($LASTEXITCODE -ne 0) { throw "下载失败: $Url (curl exit $LASTEXITCODE)" }

    Move-Item -Force $partial $Dest
    Write-Ok "已下载 $(Split-Path -Leaf $Dest) ($(Format-MB (Get-Item $Dest).Length))"
}

function Expand-Tar {
    param([string]$Archive, [string]$Dest, [int]$StripComponents = 0)

    if (-not (Test-Path $Dest)) { New-Item -ItemType Directory -Path $Dest -Force | Out-Null }
    $tarArgs = @("-xf", $Archive, "-C", $Dest)
    if ($StripComponents -gt 0) { $tarArgs += "--strip-components=$StripComponents" }
    & tar.exe @tarArgs
    if ($LASTEXITCODE -ne 0) { throw "解压失败: $Archive (tar exit $LASTEXITCODE)" }
}

# ---------------------------------------------------------------------------
# 1) 内置 Python + deeptutor
# ---------------------------------------------------------------------------

if (-not $SkipPython) {
    Write-Step "组装内置 Python $PythonVersion + $DeepTutorSpec"

    $PyExe = Join-Path $PyDir "python.exe"
    $PyArchiveName = "cpython-$PythonVersion+$PbsRelease-x86_64-pc-windows-msvc-install_only.tar.gz"
    $PyArchive = Join-Path $CacheDir $PyArchiveName
    $PyUrl = "https://github.com/astral-sh/python-build-standalone/releases/download/$PbsRelease/$PyArchiveName"

    if ($Force -or -not (Test-Path $PyExe)) {
        Get-RemoteFile -Url $PyUrl -Dest $PyArchive
        Write-Dim "解压到 $PyDir"
        if (Test-Path $PyDir) { Remove-Item $PyDir -Recurse -Force }
        # 归档内顶层是 python/,直接剥掉,让 python.exe 落在 runtimes/python/ 下
        Expand-Tar -Archive $PyArchive -Dest $PyDir -StripComponents 1
        Write-Ok "解释器就位 $PyExe"
    } else {
        Write-Ok "复用现有解释器 $PyExe"
    }

    if (-not (Test-Path $PyExe)) { throw "内置解释器缺失: $PyExe" }

    $reported = & $PyExe -c "import sys;print('%d.%d.%d' % sys.version_info[:3])"
    Write-Dim "解释器自报版本: $reported"

    Write-Step "安装 $DeepTutorSpec 到内置解释器"

    $pipArgs = @("-m", "pip", "install", "--upgrade", "--no-warn-script-location", $DeepTutorSpec)
    if ($IndexUrl -ne "") { $pipArgs += @("-i", $IndexUrl) }

    & $PyExe @pipArgs
    if ($LASTEXITCODE -ne 0) { throw "pip install 失败 (exit $LASTEXITCODE)" }

    # 关键校验:deeptutor 可导入 + 前端产物(server.js)确实随 wheel 一起来了
    $probe = & $PyExe -c "import importlib.metadata as m; import deeptutor_web, os; print(m.version('deeptutor')); print(os.path.dirname(deeptutor_web.__file__))"
    if ($LASTEXITCODE -ne 0) { throw "内置环境校验失败: deeptutor / deeptutor_web 不可导入" }

    $installedVersion = $probe[0]
    $webDir = $probe[1]
    $webServer = Join-Path $webDir "server.js"
    if (-not (Test-Path $webServer)) {
        throw "前端产物缺失: $webServer(deepTutor 需要它才能起 :3782)"
    }
    Write-Ok "deeptutor $installedVersion 已安装,前端产物在 $webDir"
}

# ---------------------------------------------------------------------------
# 2) 内置 Node.js
# ---------------------------------------------------------------------------

if (-not $SkipNode) {
    Write-Step "组装内置 Node.js $NodeVersion"

    $NodeExe = Join-Path $NodeDir "node.exe"
    $NodeCache = Join-Path $CacheDir "node-$NodeVersion.exe"

    if ($Force -or -not (Test-Path $NodeExe)) {
        # 官方单文件发行版,无需解压整个 zip
        Get-RemoteFile -Url "https://nodejs.org/dist/$NodeVersion/win-x64/node.exe" -Dest $NodeCache
        if (-not (Test-Path $NodeDir)) { New-Item -ItemType Directory -Path $NodeDir -Force | Out-Null }
        Copy-Item -Force $NodeCache $NodeExe
    }

    if (-not (Test-Path $NodeExe)) { throw "内置 node.exe 缺失: $NodeExe" }

    $nodeReported = & $NodeExe --version
    Write-Ok "node.exe 就位,自报版本 $nodeReported"
}

# ---------------------------------------------------------------------------
# 3) 汇总
# ---------------------------------------------------------------------------

Write-Step "运行时汇总"

if (Test-Path $PyDir) {
    $pySize = (Get-ChildItem $PyDir -Recurse -File -ErrorAction SilentlyContinue |
               Measure-Object -Property Length -Sum).Sum
    $pyFiles = (Get-ChildItem $PyDir -Recurse -File -ErrorAction SilentlyContinue).Count
    Write-Ok "python/  $(Format-MB $pySize)  ($pyFiles 个文件)"
}
if (Test-Path $NodeDir) {
    $nodeSize = (Get-ChildItem $NodeDir -Recurse -File -ErrorAction SilentlyContinue |
                 Measure-Object -Property Length -Sum).Sum
    Write-Ok "node/    $(Format-MB $nodeSize)"
}

$total = 0
if (Test-Path $Runtimes) {
    $total = (Get-ChildItem $Runtimes -Recurse -File -ErrorAction SilentlyContinue |
              Measure-Object -Property Length -Sum).Sum
}
Write-Host ""
Write-Host "    合计 $(Format-MB $total) -> $Runtimes" -ForegroundColor Green
Write-Host "    接下来: pnpm tauri build --bundles nsis" -ForegroundColor Green
