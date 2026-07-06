# scripts/stop.ps1 — 停止 Orcha Gateway + Adapter 后台进程
#
# 用法：.\scripts\stop.ps1
#
# 行为：
#   1. 读 logs\orcha.pids（start.ps1 写的）
#   2. 优雅停止（先 SIGTERM，3s 后强杀）
#   3. 找不到 PID 文件则按进程名兜底查找

$OutputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok($msg)   { Write-Host "  [OK] $msg" -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "  [!]  $msg" -ForegroundColor Yellow }

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$pidFile = Join-Path $repoRoot 'logs\orcha.pids'

Write-Step "停止 Orcha 进程"

# 从 PID 文件读
$pids = @()
if (Test-Path $pidFile) {
    $pids = (Get-Content $pidFile -Encoding UTF8).Trim() -split '\s+' | Where-Object { $_ -match '^\d+$' }
}

# 兜底：按进程名找
if ($pids.Count -eq 0) {
    $gw = Get-Process -Name 'orcha-gateway' -ErrorAction SilentlyContinue
    if ($gw) { $pids += $gw.Id }
    # cmd.exe 包装的 adapter，找它的子 node 进程
    $node = Get-Process -Name 'node' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -like '*orcha-feishu-adapter*' -or $true }  # node 进程不好按路径筛，全停
    if ($node) {
        # 谨慎：可能停掉用户其他 node 进程，只在 PID 文件丢失时才做
        # 改为只停 orcha 相关的：靠命令行匹配（PS 5.1 用 WMI）
        $nodeProcs = Get-CimInstance Win32_Process -Filter "Name='node.exe'" |
            Where-Object { $_.CommandLine -like '*orcha-feishu-adapter*' }
        foreach ($np in $nodeProcs) {
            $pids += $np.ProcessId
        }
        # 同样停包装的 cmd.exe
        $cmdProcs = Get-CimInstance Win32_Process -Filter "Name='cmd.exe'" |
            Where-Object { $_.CommandLine -like '*orcha-feishu-adapter*' }
        foreach ($cp in $cmdProcs) {
            $pids += $cp.ProcessId
        }
    }
}

if ($pids.Count -eq 0) {
    Write-Warn "没有找到运行中的 Orcha 进程"
    if (Test-Path $pidFile) { Remove-Item $pidFile -ErrorAction SilentlyContinue }
    exit 0
}

# 去重
$pids = $pids | Sort-Object -Unique

foreach ($pid in $pids) {
    $proc = Get-Process -Id $pid -ErrorAction SilentlyContinue
    if (-not $proc) {
        Write-Warn "PID $pid 已不存在"
        continue
    }

    # 优雅停止：先 Stop-Process（Windows 没有 SIGTERM，直接 Stop-Process 等价强杀，
    # 但 Gateway/Adapter 有 SIGINT/SIGTERM handler，PS 5.1 没法发 SIGINT，
    # 这里用 CloseMainWindow 模拟，失败则 Stop-Process 强杀）
    $null = $proc.CloseMainWindow()
    Start-Sleep -Milliseconds 500

    if (-not $proc.HasExited) {
        try {
            Stop-Process -Id $pid -Force -ErrorAction Stop
        } catch {
            Write-Warn "停止 PID $pid 失败：$($_.Exception.Message)"
        }
    }
    Write-Ok "已停止 PID $pid ($($proc.ProcessName))"
}

if (Test-Path $pidFile) {
    Remove-Item $pidFile -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "Orcha 已停止" -ForegroundColor Green
