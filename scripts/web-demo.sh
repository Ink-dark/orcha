#!/usr/bin/env bash
# scripts/web-demo.sh — Orcha Web UI 一键 demo（D3）
#
# 用法：./scripts/web-demo.sh [--release] [--port 7421]
#
# 本脚本：
#   1. 编译 orcha 二进制（默认 debug，可 --release）
#   2. 在临时 home 跑一遍 `orcha fix` 确定性闭环，种 1~2 个 task + history + artifacts
#   3. 启动 `orcha shell` Web UI 服务器，浏览器打开 http://127.0.0.1:{port}
#
# Ctrl-C 退出后会清理临时目录。
#
# LLM 路径（可选）：若设置 `ORCHA_LLM_API_KEY` 等环境变量并加 `--llm` 标志，
# 脚本会用 `--features llm` 编译并追加跑一次 `orcha fix --llm`，
# 此时 memory 标签页会显示 LLM 对话历史。
#
# 详见 README §9 Quick Start。

set -euo pipefail

# ---- 颜色输出（非 TTY 时关闭） -------------------------------------------
if [[ -t 1 ]]; then
    GREEN=$'\033[32m'
    RED=$'\033[31m'
    YELLOW=$'\033[33m'
    BLUE=$'\033[34m'
    CYAN=$'\033[36m'
    RESET=$'\033[0m'
else
    GREEN=""
    RED=""
    YELLOW=""
    BLUE=""
    CYAN=""
    RESET=""
fi

log()  { printf '%s[demo]%s %s\n' "$BLUE" "$RESET" "$*"; }
ok()   { printf '%s[ OK ]%s %s\n' "$GREEN" "$RESET" "$*"; }
warn() { printf '%s[warn]%s %s\n' "$YELLOW" "$RESET" "$*"; }
fail() { printf '%s[FAIL]%s %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }

# ---- 解析参数 -----------------------------------------------------------
BUILD_MODE="debug"
PORT=7421
USE_LLM=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --release) BUILD_MODE="release"; shift;;
        --port)    PORT="$2"; shift 2;;
        --llm)     USE_LLM=1; shift;;
        *)         fail "unknown arg: $1";;
    esac
done

# ---- 定位仓库根目录 -----------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# ---- 编译 orcha ---------------------------------------------------------
BUILD_FLAGS=("--bin" "orcha")
if [[ "$USE_LLM" -eq 1 ]]; then
    BUILD_FLAGS+=("--features" "orcha-cli/llm")
fi
if [[ "$BUILD_MODE" == "release" ]]; then
    BUILD_FLAGS+=("--release")
fi

log "cargo build ${BUILD_FLAGS[*]}"
cargo build "${BUILD_FLAGS[@]}"
ORCHA_BIN="$REPO_ROOT/target/$BUILD_MODE/orcha"
[[ -x "$ORCHA_BIN" ]] || fail "orcha binary not found at $ORCHA_BIN"
ok "orcha built: $ORCHA_BIN"

# ---- 临时 demo 目录（退出时清理） ---------------------------------------
DEMO_ROOT="$(mktemp -d -t orcha-web-demo-XXXXXX)"
HOME_DIR="$DEMO_ROOT/home"
mkdir -p "$HOME_DIR"
cleanup() {
    if [[ -n "${ORCHA_SHELL_PID:-}" ]]; then
        kill "$ORCHA_SHELL_PID" 2>/dev/null || true
        wait "$ORCHA_SHELL_PID" 2>/dev/null || true
    fi
    rm -rf "$DEMO_ROOT"
}
trap cleanup EXIT
log "demo root: $DEMO_ROOT"

# ---- Demo 1: 确定性闭环，单轮成功（种一个 DONE task） -------------------
WS1="$DEMO_ROOT/ws1"
mkdir -p "$WS1"
printf 'assert open("hello.py").read().strip() == "hello"\n' > "$WS1/test.py"

log "Demo 1: orcha fix (deterministic, single-round success)"
"$ORCHA_BIN" --home "$HOME_DIR" fix \
    --workspace "$WS1" \
    --max-rounds 5 --max-retries 3 \
    "创建 hello.py 输出 hello" > /dev/null
ok "Demo 1: hello.py produced, task DONE, history persisted"

# ---- Demo 2: 空白 workspace，第 2 轮 Fixer 修复（种一个 rounds=2 task） --
WS2="$DEMO_ROOT/ws2"
mkdir -p "$WS2"

log "Demo 2: orcha fix (empty workspace, Fixer repairs in round 2)"
"$ORCHA_BIN" --home "$HOME_DIR" fix \
    --workspace "$WS2" \
    --max-rounds 5 --max-retries 3 \
    "创建 greet.txt 输出 hi" > /dev/null
ok "Demo 2: greet.txt produced via Fixer, task DONE with 2 rounds"

# ---- Demo 3 (optional, --llm): LLM 路径，种 memory 数据 ----------------
if [[ "$USE_LLM" -eq 1 ]]; then
    if [[ -z "${ORCHA_LLM_API_KEY:-}" ]]; then
        warn "ORCHA_LLM_API_KEY not set; skipping LLM demo task"
    else
        WS3="$DEMO_ROOT/ws3"
        mkdir -p "$WS3"
        printf 'assert open("ping.py").read().strip() == "pong"\n' > "$WS3/test.py"
        log "Demo 3: orcha fix --llm (seeds memory entries for the Memory tab)"
        "$ORCHA_BIN" --home "$HOME_DIR" fix \
            --workspace "$WS3" --llm \
            --max-rounds 3 --max-retries 2 \
            "创建 ping.py 输出 pong" > /dev/null || warn "Demo 3 (LLM) did not succeed (continuing anyway)"
        ok "Demo 3: LLM task attempted; memory file may have entries"
    fi
fi

# ---- 启动 Web UI -------------------------------------------------------
echo
log "Starting Orcha Web UI at http://127.0.0.1:$PORT"
log "  home: $HOME_DIR"
log "  press Ctrl-C to stop and clean up"
echo
printf '%s  Open in browser: %shttp://127.0.0.1:%s/%s\n' "$CYAN" "$RESET" "$PORT" "$RESET"
echo

"$ORCHA_BIN" --home "$HOME_DIR" shell --port "$PORT" &
ORCHA_SHELL_PID=$!
wait "$ORCHA_SHELL_PID"
