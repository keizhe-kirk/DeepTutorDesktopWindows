# 引导 Python 环境与安装 DeepTutor 主项目
# 在 PowerShell 5.1+ (Windows 10/11) 环境下运行
# Usage: pwsh -ExecutionPolicy Bypass -File scripts\bootstrap-python.ps1
[CmdletBinding()]
param(
    [string]$PythonExe = "python",
    [switch]$Force = $false
)

$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$VenvDir = Join-Path $ProjectRoot ".venv"
$ReqFile = Join-Path $ProjectRoot "requirements-shell.txt"

Write-Host "== DeepTutor Desktop Python Bootstrap ==" -ForegroundColor Cyan

# 1. 探测 Python
Write-Host "`n[1/4] 探测 Python 解释器 ($PythonExe)..." -ForegroundColor Yellow
try {
    $pyVer = & $PythonExe --version 2>&1
    Write-Host "  检测到: $pyVer"
} catch {
    Write-Error "  未找到 $PythonExe。请先安装 Python 3.11+ 后重试。https://www.python.org/downloads/"
    exit 1
}

$pyVerTuple = (& $PythonExe -c "import sys; print(tuple(sys.version_info[:2]))").Trim("()").Split(",")
if ([int]$pyVerTuple[0] -lt 3 -or ([int]$pyVerTuple[0] -eq 3 -and [int]$pyVerTuple[1] -lt 11)) {
    Write-Error "  需要 Python 3.11+,当前是 $($pyVerTuple -join '.')。"
    exit 2
}

# 2. 创建 venv
if (Test-Path $VenvDir) {
    if ($Force) {
        Write-Host "`n[2/4] 已存在 .venv,因 -Force 删除重建..." -ForegroundColor Yellow
        Remove-Item -Recurse -Force $VenvDir
    } else {
        Write-Host "`n[2/4] .venv 已存在,跳过创建(使用 -Force 重建)" -ForegroundColor Yellow
    }
}
if (-not (Test-Path $VenvDir)) {
    Write-Host "`n[2/4] 创建虚拟环境 $VenvDir ..." -ForegroundColor Yellow
    & $PythonExe -m venv $VenvDir
    if ($LASTEXITCODE -ne 0) { Write-Error "venv 创建失败"; exit 3 }
}

$VenvPython = Join-Path $VenvDir "Scripts\python.exe"
Write-Host "  venv Python: $VenvPython"

# 3. 升级 pip + 安装 deeptutor
Write-Host "`n[3/4] 升级 pip..." -ForegroundColor Yellow
& $VenvPython -m pip install --upgrade pip setuptools wheel | Out-Null

if (Test-Path $ReqFile) {
    Write-Host "  安装 requirements-shell.txt ..." -ForegroundColor Yellow
    & $VenvPython -m pip install -r $ReqFile
} else {
    Write-Host "  安装 DeepTutor 主包(deeptutor,来自 PyPI)..." -ForegroundColor Yellow
    & $VenvPython -m pip install -U deeptutor
}

if ($LASTEXITCODE -ne 0) { Write-Error "pip install 失败"; exit 4 }

# 4. 验证
Write-Host "`n[4/4] 验证安装..." -ForegroundColor Yellow
& $VenvPython -c "import deeptutor; print('  deeptutor module: OK')"

Write-Host "`n== 完成 ==" -ForegroundColor Green
Write-Host "安装完成。桌面壳会自动拉起后端 (python -m deeptutor.api.run_server :8001)。" -ForegroundColor Green
Write-Host "如需手动启动后端: $VenvPython -m deeptutor.api.run_server" -ForegroundColor Cyan
