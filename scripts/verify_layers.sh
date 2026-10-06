#!/usr/bin/env bash
# 带Khornos 验证层跑探针，统计 Validation Error 次数。
#
# 关键纪律（MEMORY.md 第 67 条）：「0 个 Validation Error」**不能自证层生效**。
# 把 VK_LAYER_PATH 指向不存在的目录，同样得到 0 VE + 退出码 0。
# 因此每次都必须用 --loader-log 抓取 loader 加载层的证据行。
#
# 用法：verify_layers.sh <exe名> <次数>
set -uo pipefail

EXE="${1:?用法: verify_layers.sh <exe名> <次数>}"
RUNS="${2:-10}"
EXE_DIR="D:/WorkBuddy/Tiez/target-verify/x86_64-pc-windows-msvc/debug/examples"

export VK_LAYER_PATH="C:\\Users\\Cookies\\VulkanSDK\\Bin"
export VK_INSTANCE_LAYERS="VK_LAYER_KHRONOS_validation"
export VK_DEBUG_UTILS_MESSAGE_SEVERITY="error"
export VK_DEBUG_UTILS_MESSAGE_TYPE="ERROR|WARNING"
export VK_LAYER_VALIDATE_SYNC="1"

LOGDIR="D:/WorkBuddy/Tiez/.verify-logs"
mkdir -p "$LOGDIR"

# ---- 层生效证据：单独跑一次并抓 loader 日志 ----
LOADER_LOG="$LOGDIR/${EXE}_loader.log"
VK_LOADER_DEBUG="layer" "$EXE_DIR/$EXE.exe" > "$LOADER_LOG" 2>&1
if grep -q "Loading layer library.*VkLayer_khronos_validation.dll" "$LOADER_LOG"; then
    echo "LAYER_ACTIVE: $(grep -m1 'Loading layer library.*VkLayer_khronos_validation.dll' "$LOADER_LOG" | sed 's/^ *//')"
elif ! grep -qiE "vkCreateInstance|Vulkan" "$LOADER_LOG"; then
    #纯 Win32 探针（window_probe / tray_probe）根本不创建 Vulkan 实例，
    # 层自然不会被加载——这不是「层没装」，而是「没有 Vulkan 可校验」。
    # 此时VE 恒为 0，计数无意义，直接跳过而不是误报 FAIL。
    echo "SKIP_NO_VULKAN: $EXE 不创建 Vulkan 实例（无 VkInstance），验证层不适用"
    exit 3
else
    echo "LAYER_INACTIVE: 该探针确实用了 Vulkan，但层未加载 —— 计数无意义"
    exit 2
fi

# ---- 正式计数 ----
total=0
fail=0
for i in $(seq 1 "$RUNS"); do
    out="$LOGDIR/${EXE}_run${i}.log"
    "$EXE_DIR/$EXE.exe" > "$out" 2>&1
    rc=$?
    n=$(grep -c "Validation Error" "$out")
    total=$((total + n))
    if [ "$rc" -ne 0 ]; then
        fail=$((fail + 1))
        echo "  run $i: EXIT=$rc  VE=$n"
    else
        echo "  run $i: EXIT=0VE=$n"
    fi
    if [ "$n" -gt 0 ]; then
        echo "--- 首条违规（run $i）---"
        grep -m1 -A6 "Validation Error" "$out"
    fi
done

echo "=============================="
echo "RESULT $EXE : runs=$RUNS  ValidationError总数=$total  非零退出=$fail"
if [ "$total" -eq 0 ]; then echo "VERDICT: PASS"; else echo "VERDICT: FAIL"; fi
echo "日志目录: $LOGDIR"
