# scripts/build.ps1 — Orcha 一键编译 + 验证脚本（Windows PowerShell）
#
# 用法：
#   .\scripts\build.ps1                  # 编译 Rust + TS（默认）
#   .\scripts\build.ps1 -Test            # 加跑全量测试
#   .\scripts\build.ps1 -Lint            # 加跑 clippy + fmt check
#   .\scripts\build.ps1 -Test -Lint     # 全量验收（编译 + 测试 + lint）
#   .\scripts\build.ps1 -Release        # 用 release profile（更慢，产物在 target\release）
#   .\scripts\build.ps1 -SkipTs         # 跳过 Feishu Adapter（只编译 Rust）
#
# 退出码：
#   0 = 全部通过
#   1 = 某步失败（脚本会打印失败步骤并立即退出）
#
# 详见 docs/LOCAL_RUN.md。

[CmdletBinding()]
param(
    [switch]$Test,
    [switch]$Lint,
    [switch]$Release,
    [switch]$SkipTs
)

$ErrorActionPreference = 'Stop'
$PSDefaultParameterValues['*:Encoding'] = 'utf8'

# ---- 颜色输出 -----------------------------------------------------------
function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok($msg)   { Write-Host "  ✓ $msg" -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "  ! $msg" -ForegroundColor Yellow }
function Write-Err($msg)  { Write-Host "  ✗ $msg" -ForegroundColor Red }

# ---- 阶段 0：依赖检查 ----------------------------------------------------
Write-Step "检查依赖"

$missing = @()

$rustc = Get-Command rustc -ErrorAction SilentlyContinue
if (-not $rustc) {
    $missing += "rustc (Rust 工具链，MSRV 1.75) —— https://rustup.rs/"
} else {
    $rv = (rustc --version) -replace '^rustc ', ''
    $rvMajor = ([version]($rv -replace '(\d+\.\d+)\..*', '$1.0')).Major
    $rvMinor = ([version]($rv -replace '(\d+\.\d+)\..*', '$1.0')).Minor
    if ($rvMajor -lt 1 -or ($rvMajor -eq 1 -and $rvMinor -lt 75)) {
        Write-Err "Rust 版本 $rv 低于 MSRV 1.75，请升级：rustup update stable"
        exit 1
    }
    Write-Ok "Rust $rv"
}

$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) {
    $missing += "cargo (随 rustup 安装)"
} else {
    Write-Ok "cargo $((cargo --version) -replace '^cargo ', '')"
}

if (-not $SkipTs) {
    $node = Get-Command node -ErrorAction SilentlyContinue
    if (-not $node) {
        $missing += "node (Node.js 20+) —— https://nodejs.org/"
    } else {
        $nv = ((node --version) -replace '^v', '')
        $nvMajor = [int]($nv -replace '^(\d+)\..*', '$1')
        if ($nvMajor -lt 20) {
            Write-Err "Node.js 版本 $nv 低于 20，请升级"
            exit 1
        }
        Write-Ok "Node v$nv"
    }

    $npm = Get-Command npm -ErrorAction SilentlyContinue
    if (-not $npm) {
        $missing += "npm (随 Node.js 安装)"
    } else {
        Write-Ok "npm $((npm --version))"
    }
}

if ($missing.Count -gt 0) {
    Write-Err "缺少依赖："
    $missing | ForEach-Object { Write-Host "      - $_" -ForegroundColor Red }
    exit 1
}

# ---- 阶段 1：Rust 编译 --------------------------------------------------
Write-Step "编译 Rust workspace（--all-features）"

$cargoArgs = @('build', '--workspace', '--all-features')
if ($Release) { $cargoArgs += '--release' }

& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) {
    Write-Err "Rust 编译失败"
    exit 1
}
Write-Ok "Rust 编译通过"

# ---- 阶段 2：TS 编译 ----------------------------------------------------
if (-not $SkipTs) {
    Write-Step "编译 Feishu Adapter TS"

    $adapterDir = Join-Path $PSScriptRoot '..\packages\orcha-feishu-adapter'
    $adapterDir = (Resolve-Path $adapterDir).Path

    if (-not (Test-Path (Join-Path $adapterDir 'node_modules'))) {
        Write-Warn "node_modules 不存在，先 npm install"
        Push-Location $adapterDir
        & npm install
        if ($LASTEXITCODE -ne 0) {
            Write-Err "npm install 失败"
            Pop-Location
            exit 1
        }
        Pop-Location
        Write-Ok "npm install 完成"
    }

    Push-Location $adapterDir
    & npm run build
    if ($LASTEXITCODE -ne 0) {
        Write-Err "TS 编译失败"
        Pop-Location
        exit 1
    }
    Pop-Location
    Write-Ok "TS 编译通过（dist/）"
}

# ---- 阶段 3：测试（可选） ----------------------------------------------
if ($Test) {
    Write-Step "跑全量测试（cargo test --workspace --all-features）"

    & cargo test --workspace --all-features 2>&1 | Tee-Object -Variable testOutput
    if ($LASTEXITCODE -ne 0) {
        Write-Err "测试失败"
        $failed = $testOutput | Select-String 'FAILED'
        if ($failed) {
            Write-Host "失败测试：" -ForegroundColor Red
            $failed | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
        }
        exit 1
    }

    $okCount = ($testOutput | Select-String 'test result: ok\.').Count
    $failCount = ($testOutput | Select-String 'test result: FAILED').Count
    Write-Ok "测试全过：$okCount 个 test result ok，$failCount 个 FAILED"

    if (-not $SkipTs) {
        $adapterDir = (Resolve-Path (Join-Path $PSScriptRoot '..\packages\orcha-feishu-adapter')).Path
        Push-Location $adapterDir
        & npx tsc --noEmit
        if ($LASTEXITCODE -ne 0) {
            Write-Err "TS 类型检查失败"
            Pop-Location
            exit 1
        }
        Pop-Location
        Write-Ok "TS 类型检查通过"
    }
}

# ---- 阶段 4：lint（可选） ----------------------------------------------
if ($Lint) {
    Write-Step "clippy 检查（--all-targets --all-features）"

    & cargo clippy --workspace --all-targets --all-features -- -D warnings
    if ($LASTEXITCODE -ne 0) {
        Write-Err "clippy 有 warning 或 error"
        exit 1
    }
    Write-Ok "clippy 通过，无 warning"

    Write-Step "fmt 检查（--check）"

    & cargo fmt --all --check
    if ($LASTEXITCODE -ne 0) {
        Write-Err "fmt 不规范，运行：cargo fmt --all"
        exit 1
    }
    Write-Ok "fmt 通过"
}

# ---- 总结 ----------------------------------------------------------------
$targetDir = if ($Release) { 'release' } else { 'debug' }
$runFlag = if ($Release) { '--release' } else { '' }

Write-Host ""
Write-Host "==== 全部完成 ====" -ForegroundColor Green
Write-Host "  Rust 产物：target\$targetDir\" -ForegroundColor Gray
if (-not $SkipTs) {
    Write-Host "  TS 产物：  packages\orcha-feishu-adapter\dist\" -ForegroundColor Gray
}
Write-Host ""
Write-Host "  下一步：" -ForegroundColor Gray
Write-Host "    Gateway：  cargo run -p orcha-gateway $runFlag" -ForegroundColor Gray
Write-Host "    Adapter：  pushd packages\orcha-feishu-adapter; node dist\main.js; popd" -ForegroundColor Gray
Write-Host "    触发示例：见 docs\LOCAL_RUN.md 场景 A" -ForegroundColor Gray
