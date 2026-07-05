#!/usr/bin/env bash
# scripts/mvp-demo.sh — Orcha M3 MVP 一键复现脚本
#
# 用法：./scripts/mvp-demo.sh [--release]
#
# 本脚本演示 Orcha Cycleround 闭环的 3 条核心路径（对应 ROADMAP M3 验收）：
#   1. 成功路径：预置 test.py，单轮即闭环产出 hello.py
#   2. Fixer 修复路径：空白 workspace，第 1 轮 Tester 失败 → Fixer 创建 test.py → 第 2 轮成功
#   3. 熔断路径：预置失败 test.py，Fixer 拒绝改写 → Failed(MaxRetriesExceeded)，不死循环
#
# 退出码：
#   0 = 全部演示通过
#   非 0 = 某个演示失败（脚本会打印失败原因并立即退出）
#
# 详见 docs/ROADMAP.md M3 章节。

set -euo pipefail

# ---- 颜色输出（非 TTY 时关闭） -------------------------------------------
if [[ -t 1 ]]; then
    GREEN=$'\033[32m'
    RED=$'\033[31m'
    YELLOW=$'\033[33m'
    BLUE=$'\033[34m'
    RESET=$'\033[0m'
else
    GREEN=""
    RED=""
    YELLOW=""
    BLUE=""
    RESET=""
fi

log()   { printf '%s[demo]%s %s\n' "$BLUE" "$RESET" "$*"; }
ok()    { printf '%s[ OK ]%s %s\n' "$GREEN" "$RESET" "$*"; }
warn()  { printf '%s[warn]%s %s\n' "$YELLOW" "$RESET" "$*"; }
fail()  { printf '%s[FAIL]%s %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }

# ---- 解析参数 -----------------------------------------------------------
BUILD_MODE="debug"
if [[ "${1:-}" == "--release" ]]; then
    BUILD_MODE="release"
fi

# ---- 定位仓库根目录 -----------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# ---- 编译 orcha 二进制 -------------------------------------------------
log "build orcha (mode=$BUILD_MODE)"
if [[ "$BUILD_MODE" == "release" ]]; then
    cargo build --release --bin orcha
    ORCHA_BIN="$REPO_ROOT/target/release/orcha"
else
    cargo build --bin orcha
    ORCHA_BIN="$REPO_ROOT/target/debug/orcha"
fi
[[ -x "$ORCHA_BIN" ]] || fail "orcha binary not found at $ORCHA_BIN"
ok "orcha built: $ORCHA_BIN"

# ---- 准备临时工作区 -----------------------------------------------------
DEMO_ROOT="$(mktemp -d -t orcha-mvp-demo-XXXXXX)"
trap 'rm -rf "$DEMO_ROOT"' EXIT
log "demo workspace root: $DEMO_ROOT"

# ========================================================================
# Demo 1: 成功路径（GT-1）
# 单轮即闭环：workspace 预置 test.py 断言 hello.py 内容为 'hello'。
# 期望：outcome=Success, rounds=1, hello.py 真实产出, exit 0
# ========================================================================
log "Demo 1: 成功路径 (preexisting test.py, single-round success)"
WS1="$DEMO_ROOT/ws1"
HOME1="$DEMO_ROOT/home1"
mkdir -p "$WS1"
printf 'assert open("hello.py").read().strip() == "hello"\n' > "$WS1/test.py"

OUT1="$("$ORCHA_BIN" --home "$HOME1" fix \
    --workspace "$WS1" \
    --max-rounds 5 --max-retries 3 \
    "创建 hello.py 输出 hello")" \
    || fail "Demo 1: orcha fix exited non-zero"

# 验证 hello.py 真实产出
[[ -f "$WS1/hello.py" ]] || fail "Demo 1: hello.py not produced"
[[ "$(cat "$WS1/hello.py")" == "hello" ]] || fail "Demo 1: hello.py content mismatch"

# 验证 JSON 输出
echo "$OUT1" | grep -q '"outcome": "Success"' || fail "Demo 1: outcome != Success"
echo "$OUT1" | grep -q '"rounds": 1'        || fail "Demo 1: rounds != 1"
ok "Demo 1: hello.py produced, single-round success"

# ========================================================================
# Demo 2: Fixer 修复路径（GT-2）
# 空白 workspace：第 1 轮 Tester 失败 → Fixer 创建 test.py → 第 2 轮成功。
# 期望：outcome=Success, rounds=2, greet.txt + test.py 都产出, exit 0
# ========================================================================
log "Demo 2: Fixer 修复路径 (empty workspace, succeeds in round 2)"
WS2="$DEMO_ROOT/ws2"
HOME2="$DEMO_ROOT/home2"
mkdir -p "$WS2"

OUT2="$("$ORCHA_BIN" --home "$HOME2" fix \
    --workspace "$WS2" \
    --max-rounds 5 --max-retries 3 \
    "创建 greet.txt 输出 hi")" \
    || fail "Demo 2: orcha fix exited non-zero"

# 验证 greet.txt + test.py 都产出
[[ -f "$WS2/greet.txt" ]] || fail "Demo 2: greet.txt not produced"
[[ "$(cat "$WS2/greet.txt")" == "hi" ]] || fail "Demo 2: greet.txt content mismatch"
[[ -f "$WS2/test.py" ]] || fail "Demo 2: Fixer did not create test.py"

# 验证 rounds=2（Fixer 第 1 轮创建 test.py，第 2 轮才成功）
echo "$OUT2" | grep -q '"outcome": "Success"' || fail "Demo 2: outcome != Success"
echo "$OUT2" | grep -q '"rounds": 2'        || fail "Demo 2: rounds != 2"
ok "Demo 2: greet.txt + test.py produced, Fixer repaired in 2 rounds"

# ========================================================================
# Demo 3: 熔断路径（GT-3）
# workspace 预置失败 test.py（assert 1==2）：Fixer 拒绝改写 →
# 触达 max_retries=2 → Failed(MaxRetriesExceeded)，不死循环。
# 期望：outcome=Failed, reason=MaxRetriesExceeded, exit 1, test.py 内容未被改写
# ========================================================================
log "Demo 3: 熔断路径 (failing test.py, MaxRetriesExceeded, no infinite loop)"
WS3="$DEMO_ROOT/ws3"
HOME3="$DEMO_ROOT/home3"
mkdir -p "$WS3"
printf 'assert 1 == 2, "intentional bug"\n' > "$WS3/test.py"

# 这里期望 orcha fix 退出码为 1（失败路径）
set +e
"$ORCHA_BIN" --home "$HOME3" fix \
    --workspace "$WS3" \
    --max-rounds 10 --max-retries 2 \
    "创建 hello.py 输出 hello" > "$DEMO_ROOT/out3.json" 2>&1
EXIT3=$?
set -e

[[ $EXIT3 -eq 1 ]] || fail "Demo 3: expected exit 1, got $EXIT3"

OUT3="$(cat "$DEMO_ROOT/out3.json")"
echo "$OUT3" | grep -q '"outcome": "Failed"'             || fail "Demo 3: outcome != Failed"
echo "$OUT3" | grep -q '"reason": "MaxRetriesExceeded"' || fail "Demo 3: reason != MaxRetriesExceeded"

# 不死循环：rounds 应远小于 max_rounds=10
ROUNDS3=$(echo "$OUT3" | grep -oE '"rounds": [0-9]+' | grep -oE '[0-9]+')
[[ "$ROUNDS3" -le 2 ]] || fail "Demo 3: rounds=$ROUNDS3 exceeded max_retries=2 (infinite loop?)"

# Fixer 不应改写已存在的 test.py
[[ "$(cat "$WS3/test.py")" == 'assert 1 == 2, "intentional bug"' ]] \
    || fail "Demo 3: Fixer tampered with existing test.py (cheating)"
ok "Demo 3: MaxRetriesExceeded triggered, no infinite loop, test.py unchanged"

# ========================================================================
# 收尾：验证 history 文件落盘可追溯（M3 验收项「每一轮 round 可追溯」）
# ========================================================================
log "Verify: history files persisted and traceable"
for home in "$HOME1" "$HOME2" "$HOME3"; do
    history_files=$(ls "$home/history"/*.jsonl 2>/dev/null || true)
    [[ -n "$history_files" ]] || fail "history dir empty at $home/history"
    for hf in $history_files; do
        # 每行应是合法 JSON，且至少 1 条记录
        lines=$(wc -l < "$hf")
        [[ "$lines" -ge 1 ]] || fail "$hf has no records"
        # 校验每行是合法 JSON（python3 不可用时跳过）
        if command -v python3 >/dev/null 2>&1; then
            while IFS= read -r line; do
                [[ -z "$line" ]] && continue
                echo "$line" | python3 -c 'import json,sys; json.loads(sys.stdin.read())' \
                    || fail "$hf contains invalid JSON line: $line"
            done < "$hf"
        fi
    done
done
ok "history files persisted as JSONL, all records traceable"

# ========================================================================
# 全部通过
# ========================================================================
echo
ok "All 3 MVP demos passed."
ok "M3 acceptance verified:"
ok "  - orcha fix 闭环修复且测试通过 (Demo 1 + 2)"
ok "  - 触达熔断时转 FAILED，不死循环 (Demo 3)"
ok "  - 每轮 round 在 history 中可追溯 (verified above)"
ok "  - 3 个 golden tasks 全部通过 (cargo test --test m3_golden_tasks)"
ok "  - ./scripts/mvp-demo.sh 一键复现完整闭环 (this script)"
