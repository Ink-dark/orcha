# Orcha v0.1.0-beta 安装脚本
# 用法：.\install.ps1
# 首次运行自动创建 .orcha/config.toml 模板 + 提示填入 key
# 后续运行 `.\start.ps1` 启动服务

[CmdletBinding()]
param(
    [switch]$SkipCheck # 跳过依赖检查（仅限 CI）
)

$ErrorActionPreference = 'Stop'
$repoRoot = $PSScriptRoot

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok($msg)   { Write-Host "  [OK] $msg" -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "  [!] $msg" -ForegroundColor Yellow }
function Write-Err($msg)  { Write-Host "  [X] $msg" -ForegroundColor Red }

Write-Host ""
Write-Host "  ╔══════════════════════════════════════╗"
Write-Host "  ║       Orcha v0.1.0-beta             ║"
Write-Host "  ║   AI-Agent 驱动的编码助手            ║"
Write-Host "  ╚══════════════════════════════════════╝"
Write-Host ""

# ---- 1. 检查依赖 ----
if (-not $SkipCheck) {
    Write-Step "1/4 检查依赖"

    # Git
    try {
        $gitVersion = & git --version 2>$null
        Write-Ok "Git: $gitVersion"
    } catch {
        Write-Err "未找到 Git，请安装: https://git-scm.com/download/win"
        exit 1
    }

    # Node.js（Adapter 需要）
    try {
        $nodeVersion = & node --version 2>$null
        Write-Ok "Node.js: $nodeVersion"
    } catch {
        Write-Warn "未找到 Node.js（Adapter 需要），跳过"
        Write-Warn "  Adapter 仅飞书模式需要；本地调试可用 smoke 端口"
    }
}

# ---- 2. 配置 ----
Write-Step "2/4 配置"

$orchaHome = Join-Path $repoRoot '.orcha'
if (-not (Test-Path $orchaHome)) {
    New-Item -ItemType Directory -Force -Path $orchaHome | Out-Null
}

$configFile = Join-Path $orchaHome 'config.toml'
if (-not (Test-Path $configFile)) {
    Write-Warn "生成默认 config.toml"
    $homeEscaped = $orchaHome -replace '\\', '/'
    @"
home = "$homeEscaped"
workers = 2

[ipc]
kind = "auto"
port = 7422

[cycle]
max_rounds = 30
max_retries = 3
cool_down_secs = 60

[llm]
# 从环境变量 ORCHA_LLM_API_KEY 读取，或直接填 api_key = "sk-..."
api_key_env = "ORCHA_LLM_API_KEY"
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"

[[auth.whitelist]]
platform = "feishu"
group = "*"

[[auth.whitelist]]
platform = "feishu"
user = "*"

[workspace]
# 飞书触发任务时在哪个 repo 里改代码？改成你的项目路径
repo = "."

[approval]
timeout_secs = 1800

[[approval.write]]
platform = "feishu"
group = "*"

[[approval.command]]
platform = "feishu"
group = "*"

[[approval.delete]]
platform = "feishu"
group = "*"
"@ | Out-File -FilePath $configFile -Encoding UTF8
    Write-Ok "config.toml 已生成: $configFile"
} else {
    Write-Ok "config.toml 已存在"
}

# ---- 3. API Key ----
Write-Step "3/4 配置 API Key"

$envFile = Join-Path $repoRoot 'scripts' 'dev-env.ps1'
if (-not (Test-Path $envFile)) {
    $envExample = Join-Path $repoRoot 'scripts' 'dev-env.ps1.example'
    if (Test-Path $envExample) {
        Copy-Item $envExample $envFile
    }
    @"
# Orcha 环境变量
# 至少填一个 LLM API Key（DeepSeek / OpenAI 兼容均可）
`$env:ORCHA_LLM_API_KEY = "sk-your-key-here"

# 飞书应用凭证（可选，不填则只能 smoke 测试）
# `$env:ORCHA_FEISHU_APP_ID = "cli_xxx"
# `$env:ORCHA_FEISHU_APP_SECRET = "..."
"@ | Out-File -FilePath $envFile -Encoding UTF8
}

if ($env:ORCHA_LLM_API_KEY -and $env:ORCHA_LLM_API_KEY -ne 'sk-your-key-here') {
    Write-Ok "LLM API Key 已设置"
} else {
    Write-Warn "请编辑 $envFile 填入 LLM API Key"
    Write-Warn "  ORCHA_LLM_API_KEY = `"sk-...`""
    Write-Warn ""
    Write-Warn "  免费获取 DeepSeek Key: https://platform.deepseek.com/api_keys"
    Write-Warn "  也支持 OpenAI / Ollama / vLLM 等兼容接口"
}

# ---- 4. 编译/就绪检查 ----
Write-Step "4/4 检查二进制"

$gatewayExe = Join-Path $repoRoot "orcha-gateway.exe"
if (-not (Test-Path $gatewayExe)) {
    $gatewayExe = Join-Path $repoRoot "target" "debug" "orcha-gateway.exe"
}
if (-not (Test-Path $gatewayExe)) {
    Write-Warn "未找到预编译二进制，请确认 orcha-gateway.exe 在本目录"
} else {
    Write-Ok "Gateway 二进制: $gatewayExe"
}

# ---- 完成 ----
Write-Host ""
Write-Host "  ╔══════════════════════════════════════╗"
Write-Host "  ║      安装完成！                     ║"
Write-Host "  ╚══════════════════════════════════════╝"
Write-Host ""
Write-Host "  快速开始:"
Write-Host ""
Write-Host "  1. 编辑 API Key:"
Write-Host "     notepad $envFile"
Write-Host ""
Write-Host "  2. 启动服务:"
Write-Host "     .\scripts\start.ps1"
Write-Host ""
Write-Host "  3. Smoke 测试（本地，不需要飞书）:"
Write-Host '     $c=New-Object System.Net.Sockets.TcpClient("127.0.0.1",7423)'
Write-Host '     $w=New-Object System.IO.StreamWriter($c.GetStream())'
Write-Host '     $w.WriteLine("创建一个 hello.py 输出 hello world")'
Write-Host '     $w.Flush();$c.Close()'
Write-Host ""
Write-Host "  或配好飞书凭证后，在群里 @Orcha 发任务。"
Write-Host ""
Write-Host "  详细文档: docs/LOCAL_RUN.md"
Write-Host ""
