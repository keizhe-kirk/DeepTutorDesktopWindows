# 同步 HKUDS/DeepTutor 的 web/ 资源到本地(可选)
# 默认策略下 WebView 直接回环加载 127.0.0.1:3782,无需此脚本
# 但在"内嵌构建产物"策略下需要把 web/.next/standalone 同步到 src-tauri 资源目录
#
# Usage: pwsh -ExecutionPolicy Bypass -File scripts\fetch-web.ps1 -Ref v1.6.6
[CmdletBinding()]
param(
    [string]$Ref = "main",
    [string]$OutDir = "$(Split-Path -Parent $PSScriptRoot)\web-cache"
)

$ErrorActionPreference = "Stop"
$Repo = "https://github.com/HKUDS/DeepTutor.git"

Write-Host "== 拉取 DeepTutor web 资源 ==" -ForegroundColor Cyan
Write-Host "  Ref: $Ref"
Write-Host "  Out: $OutDir"

if (-not (Test-Path $OutDir)) {
    New-Item -ItemType Directory -Path $OutDir | Out-Null
}

# 浅克隆
$TmpClone = Join-Path $OutDir "_tmp_clone"
if (Test-Path $TmpClone) {
    Write-Host "  清理旧临时克隆..." -ForegroundColor Yellow
    Remove-Item -Recurse -Force $TmpClone
}

Write-Host "`n[1/3] 浅克隆主仓库 (depth=1)..." -ForegroundColor Yellow
git clone --depth 1 --branch $Ref $Repo $TmpClone 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) { Write-Error "克隆失败"; exit 1 }

Write-Host "[2/3] 拷贝 web/ ..." -ForegroundColor Yellow
robocopy "$TmpClone\web" "$OutDir\web" /MIR /NDL /NFL /NJH /NJS | Out-Null

Write-Host "[3/3] 清理临时克隆..." -ForegroundColor Yellow
Remove-Item -Recurse -Force $TmpClone

Write-Host "`n== 完成。后续 pnpm --dir $OutDir\web install --legacy-peer-deps ==" -ForegroundColor Green
