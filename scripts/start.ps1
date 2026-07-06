# scripts/start.ps1 — Orcha 一键启动 Gateway + Adapter（后台）
#
# 用法：
#   .\scripts\start.ps1                  # 启动两个后台进程
#   .\scripts\start.ps1 -Smoke            # 启动后自动 curl 触发一次 smoke
#   .\scripts\start.ps1 -Stop             # 等价于 .\scripts\stop.ps1
#   .\scripts\start.ps1 -Restart          # 先 stop 再 start
#
# 首次运行：
#   1. 检查 scripts\dev-env.ps1 是否存在，不存在则从 .example 拷贝并提示编辑
#   2. 检查 $ORCHA_HOME\config.toml 是否存在，不存在则自动生成默认模板
#   3. 启动 Gateway 和 Adapter 两个后台进程，日志写到 logs\
#   4. 等待就绪后打印状态
#
# 详见 docs/LOCAL_RUN.md。

[CmdletBinding()]
param(
    [switch]$Smoke,
    [switch]$Stop,
    [switch]$Restart
)

# 强制 UTF-8（PS 5.1 默认 GBK 会乱码）
$OutputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok($msg)   { Write-Host "  [OK] $msg" -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "  [!]  $msg" -ForegroundColor Yellow }
function Write-Err($msg)  { Write-Host "  [X]  $msg" -ForegroundColor Red }

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$envFile = Join-Path $PSScriptRoot 'dev-env.ps1'
$envExample = Join-Path $PSScriptRoot 'dev-env.ps1.example'
$orchaHome = Join-Path $repoRoot '.orcha'
$configFile = Join-Path $orchaHome 'config.toml'
$logDir = Join-Path $repoRoot 'logs'
$pidFile = Join-Path $logDir 'orcha.pids'

# ---- 处理 -Stop / -Restart -------------------------------------------
if ($Stop -or $Restart) {
    Write-Step "停止现有 Orcha 进程"
    & (Join-Path $PSScriptRoot 'stop.ps1')
    if (-not $Restart) { exit 0 }
}

# ---- 阶段 0：检查 dev-env.ps1 ------------------------------------------
if (-not (Test-Path $envFile)) {
    if (Test-Path $envExample) {
        Copy-Item $envExample $envFile
        Write-Warn "已从模板创建 scripts\dev-env.ps1"
        Write-Err "请编辑 scripts\dev-env.ps1 填入你的 LLM API key，然后重新运行此脚本"
        Write-Host "  编辑示例：" -ForegroundColor Gray
        Write-Host '  $env:ORCHA_LLM_API_KEY = "sk-..."' -ForegroundColor Yellow
        notepad $envFile
        exit 1
    } else {
        Write-Err "找不到 scripts\dev-env.ps1.example，仓库不完整"
        exit 1
    }
}

# 加载环境变量（dev-env.ps1 里设置 $env:ORCHA_LLM_API_KEY 等）
. $envFile

if (-not $env:ORCHA_LLM_API_KEY -or $env:ORCHA_LLM_API_KEY -eq 'YOUR_LLM_API_KEY_HERE') {
    Write-Err "scripts\dev-env.ps1 里 ORCHA_LLM_API_KEY 未填写"
    Write-Host "  请编辑 $envFile" -ForegroundColor Yellow
    notepad $envFile
    exit 1
}
Write-Ok "LLM key 已加载"

# ---- 阶段 1：生成 config.toml（如果不存在） ---------------------------
if (-not (Test-Path $orchaHome)) {
    New-Item -ItemType Directory -Force -Path $orchaHome | Out-Null
}

if (-not (Test-Path $configFile)) {
    Write-Step "生成默认 config.toml（$configFile）"

    $llmBaseUrl  = if ($env:ORCHA_LLM_BASE_URL) { $env:ORCHA_LLM_BASE_URL } else { 'https://api.openai.com/v1' }
    $llmModel    = if ($env:ORCHA_LLM_MODEL)   { $env:ORCHA_LLM_MODEL }   else { 'gpt-4o' }
    $homeEscaped = $orchaHome -replace '\\', '/'

    $toml = @"
home = "$homeEscaped"
workers = 2

[ipc]
kind = "auto"
port = 7422

[cycle]
max_rounds = 10
max_retries = 3
cool_down_secs = 60

[llm]
api_key_env = "ORCHA_LLM_API_KEY"
base_url = "$llmBaseUrl"
model = "$llmModel"

# 开发调试用：允许任意飞书来源触发
# 生产环境请改成具体的 open_id / chat_id
[[auth.whitelist]]
platform = "feishu"
group = "*"
"@
    $toml | Out-File -FilePath $configFile -Encoding UTF8
    Write-Ok "config.toml 已生成"
} else {
    Write-Ok "config.toml 已存在"
}

# ---- 阶段 2：检查编译产物 ---------------------------------------------
$gatewayExe = Join-Path $repoRoot "target\debug\orcha-gateway.exe"
$adapterMain = Join-Path $repoRoot "packages\orcha-feishu-adapter\dist\main.js"

if (-not (Test-Path $gatewayExe)) {
    Write-Step "Gateway 未编译，开始 cargo build"
    Push-Location $repoRoot
    $prevEAP = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & cargo build -p orcha-gateway
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prevEAP
    Pop-Location
    if ($code -ne 0) {
        Write-Err "Gateway 编译失败"
        exit 1
    }
}
Write-Ok "Gateway 已编译"

if (-not (Test-Path $adapterMain)) {
    Write-Step "Adapter 未编译，开始 npm build"
    $adapterDir = Join-Path $repoRoot 'packages\orcha-feishu-adapter'
    if (-not (Test-Path (Join-Path $adapterDir 'node_modules'))) {
        Push-Location $adapterDir
        $prevEAP = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        & npm install
        $ErrorActionPreference = $prevEAP
        Pop-Location
    }
    Push-Location $adapterDir
    $prevEAP = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & npm run build
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prevEAP
    Pop-Location
    if ($code -ne 0) {
        Write-Err "Adapter 编译失败"
        exit 1
    }
}
Write-Ok "Adapter 已编译"

# ---- 阶段 3：启动 Gateway 后台进程 -------------------------------------
if (-not (Test-Path $logDir)) {
    New-Item -ItemType Directory -Force -Path $logDir | Out-Null
}

$gwLog = Join-Path $logDir 'gateway.log'
$adLog = Join-Path $logDir 'adapter.log'

# Gateway 用 eprintln!，日志全在 stderr（写到 .err 文件）
# Adapter 用 console.log/warn/error，stdout/.log + stderr/.err 都有
# 等待就绪时两个文件都 grep

Write-Step "启动 Gateway（后台）"
$gwProc = Start-Process -FilePath $gatewayExe `
    -ArgumentList @("--config", $configFile) `
    -WorkingDirectory $repoRoot `
    -RedirectStandardOutput $gwLog `
    -RedirectStandardError  "$gwLog.err" `
    -PassThru -WindowStyle Hidden

Start-Sleep -Seconds 1
if ($gwProc.HasExited) {
    Write-Err "Gateway 启动后立即退出"
    Write-Host "  ---- gateway.log ----" -ForegroundColor Gray
    Get-Content $gwLog -ErrorAction SilentlyContinue | Select-Object -First 30
    Write-Host "  ---- gateway.log.err ----" -ForegroundColor Gray
    Get-Content "$gwLog.err" -ErrorAction SilentlyContinue | Select-Object -First 30
    exit 1
}
Write-Ok "Gateway PID=$($gwProc.Id)"

# ---- 阶段 4：等待 Gateway 端口就绪 -------------------------------------
Write-Step "等待 Gateway IPC 就绪（最多 15 秒）"
$ready = $false
for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Milliseconds 500
    # Gateway 用 eprintln!，日志在 stderr（.err 文件）
    if (Get-Content "$gwLog.err" -Encoding UTF8 -ErrorAction SilentlyContinue | Select-String 'IPC listening on') {
        $ready = $true
        break
    }
    if (Get-Content $gwLog -Encoding UTF8 -ErrorAction SilentlyContinue | Select-String 'IPC listening on') {
        $ready = $true
        break
    }
    # 早期退出的情况
    if ($gwProc.HasExited) {
        Write-Err "Gateway 启动后退出，exit code=$($gwProc.ExitCode)"
        Write-Host "  ---- gateway.log ----" -ForegroundColor Gray
        Get-Content $gwLog -Encoding UTF8 -ErrorAction SilentlyContinue | Select-Object -First 30
        Write-Host "  ---- gateway.log.err ----" -ForegroundColor Gray
        Get-Content "$gwLog.err" -Encoding UTF8 -ErrorAction SilentlyContinue | Select-Object -First 30
        exit 1
    }
}
if (-not $ready) {
    Write-Err "Gateway 启动超时（15 秒内无 'IPC listening on' 日志）"
    Write-Host "  ---- gateway.log ----" -ForegroundColor Gray
    Get-Content $gwLog -Encoding UTF8 -ErrorAction SilentlyContinue | Select-Object -First 50
    Write-Host "  ---- gateway.log.err ----" -ForegroundColor Gray
    Get-Content "$gwLog.err" -Encoding UTF8 -ErrorAction SilentlyContinue | Select-Object -First 50
    Stop-Process -Id $gwProc.Id -Force -ErrorAction SilentlyContinue
    exit 1
}
Write-Ok "Gateway IPC 已就绪"

# ---- 阶段 5：启动 Adapter 后台进程 -------------------------------------
Write-Step "启动 Adapter（后台）"
$adapterDir = Join-Path $repoRoot 'packages\orcha-feishu-adapter'
$adEnv = @{
    ORCHA_GATEWAY_ENDPOINT = 'tcp://127.0.0.1:7422'
    ORCHA_ADAPTER_MOCK     = '1'
}
# 保留 dev-env 里的飞书凭证（如果设了）
if ($env:ORCHA_FEISHU_APP_ID)     { $adEnv.ORCHA_FEISHU_APP_ID = $env:ORCHA_FEISHU_APP_ID }
if ($env:ORCHA_FEISHU_APP_SECRET) { $adEnv.ORCHA_FEISHU_APP_SECRET = $env:ORCHA_FEISHU_APP_SECRET }
# 用户在 dev-env.ps1 里把 mock 设为 0 时切真实飞书模式（长连接）
if ($null -ne $env:ORCHA_ADAPTER_MOCK) { $adEnv.ORCHA_ADAPTER_MOCK = $env:ORCHA_ADAPTER_MOCK }

# PS 5.1 Start-Process 不支持 -Environment，用 cmd /c 注入环境变量
$envCmd = ($adEnv.GetEnumerator() | ForEach-Object { "set `"$($_.Key)=$($_.Value)`"" }) -join ' && '
$adCmd = "$envCmd && node `"$adapterMain`""
$adProc = Start-Process -FilePath 'cmd.exe' `
    -ArgumentList @('/c', $adCmd) `
    -WorkingDirectory $adapterDir `
    -RedirectStandardOutput $adLog `
    -RedirectStandardError  "$adLog.err" `
    -PassThru -WindowStyle Hidden

Start-Sleep -Seconds 1
if ($adProc.HasExited) {
    Write-Err "Adapter 启动后立即退出，看日志：$adLog"
    Get-Content $adLog -ErrorAction SilentlyContinue | Select-Object -First 30
    Get-Content "$adLog.err" -ErrorAction SilentlyContinue | Select-Object -First 30
    Stop-Process -Id $gwProc.Id -Force -ErrorAction SilentlyContinue
    exit 1
}
Write-Ok "Adapter PID=$($adProc.Id)，日志：$adLog"

# 等待 Adapter 连上 Gateway（最多 15 秒）
# Adapter 日志含中文，PS 5.1 按 GBK 读 UTF-8 文件会乱码，所以只 grep ASCII 部分
$adReady = $false
for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Milliseconds 500
    # 关键就绪标志（全是 ASCII）：
    #   "ws connect success" — 长连接已建立
    #   "ws client ready"    — SDK 客户端就绪
    #   "IPC" + "Gateway"    — IPC 已连上 Gateway
    $adLogContent = Get-Content $adLog -ErrorAction SilentlyContinue -Encoding UTF8
    $adErrContent = Get-Content "$adLog.err" -ErrorAction SilentlyContinue -Encoding UTF8
    $allContent = @($adLogContent) + @($adErrContent) -join "`n"
    if ($allContent -match 'ws connect success|ws client ready') {
        $adReady = $true
        break
    }
    if ($allContent -match 'IPC.+Gateway|Gateway.+IPC') {
        $adReady = $true
        break
    }
}
if ($adReady) {
    Write-Ok "Adapter 已就绪（飞书长连接 + IPC 都已连上）"
} else {
    Write-Warn "Adapter 启动超时（15 秒内无就绪日志），看日志确认：$adLog"
    Write-Host "  ---- adapter.log ----" -ForegroundColor Gray
    Get-Content $adLog -Encoding UTF8 -ErrorAction SilentlyContinue | Select-Object -First 30
    Write-Host "  ---- adapter.log.err ----" -ForegroundColor Gray
    Get-Content "$adLog.err" -Encoding UTF8 -ErrorAction SilentlyContinue | Select-Object -First 30
}

# ---- 阶段 6：保存 PID + 打印状态 --------------------------------------
"$($gwProc.Id) $($adProc.Id)" | Out-File -FilePath $pidFile -Encoding UTF8

Write-Host ""
Write-Host "==== Orcha 已启动 ====" -ForegroundColor Green
Write-Host "  Gateway PID：$($gwProc.Id)" -ForegroundColor Gray
Write-Host "  Adapter PID：$($adProc.Id)" -ForegroundColor Gray
Write-Host "  IPC 端点：   tcp://127.0.0.1:7422" -ForegroundColor Gray
if ($adEnv.ORCHA_ADAPTER_MOCK -eq '0') {
    Write-Host "  飞书模式：   长连接（主动连飞书服务器，无需公网 URL）" -ForegroundColor Gray
} else {
    Write-Host "  飞书模式：   mock（不连真实飞书，动作用 console.log）" -ForegroundColor Gray
}
Write-Host "  ORCHA_HOME： $orchaHome" -ForegroundColor Gray
Write-Host ""
Write-Host "  日志：" -ForegroundColor Gray
Write-Host "    Gateway:  $gwLog" -ForegroundColor Gray
Write-Host "    Adapter:  $adLog" -ForegroundColor Gray
Write-Host ""
Write-Host "  实时看日志：  Get-Content $gwLog -Wait -Tail 20 -Encoding UTF8" -ForegroundColor Gray
Write-Host "                Get-Content `"$gwLog.err`" -Wait -Tail 20 -Encoding UTF8" -ForegroundColor Gray
Write-Host "                Get-Content `"$adLog`" -Wait -Tail 20 -Encoding UTF8" -ForegroundColor Gray
Write-Host "  停止服务：    .\scripts\stop.ps1" -ForegroundColor Gray
Write-Host ""

# ---- 阶段 7：可选 smoke test（仅 mock 模式有意义）---------------------
if ($Smoke) {
    if ($adEnv.ORCHA_ADAPTER_MOCK -eq '0') {
        Write-Warn "非 mock 模式：smoke 跳过（长连接由飞书服务器推送事件，无法本地触发）"
        Write-Host "  真实联调请到飞书群里 @机器人 发消息，看 logs\adapter.log" -ForegroundColor Gray
    } else {
        Write-Step "smoke：检查 Adapter 是否在跑"
        $adReady = Get-Content $adLog -ErrorAction SilentlyContinue | Select-String '启动完成'
        if ($adReady) {
            Write-Ok "Adapter 已就绪（mock 模式无事件入口，靠真实飞书或单元测试验证接收链路）"
        } else {
            Write-Warn "Adapter 还在启动中，看日志：$adLog"
        }
    }
    Write-Host ""
    Write-Host "  看 Gateway 处理日志：" -ForegroundColor Gray
    Write-Host "    Get-Content $gwLog -Wait -Tail 30" -ForegroundColor Gray
}
