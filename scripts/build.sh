#!/usr/bin/env bash
# ModularClipboard 构建环境引导脚本。
#
# 为什么需要它：Visual Studio 安装在 D:\Program Files 下（非常规路径），
# 且系统PATH 中没有 cl.exe。cc-rs（编译 sqlite3.c 等 C 代码）需要能找到
# 真正的 MSVC 工具链与 Windows SDK，因此必须显式注入 INCLUDE/LIB。
#
# 注意：Git Bash 会把形如 /d/Program Files/... 的 POSIX 路径转换后再传给
# 子进程，导致 cl.exe 无法定位头文件。本脚本统一使用原生 Windows 路径，
# 并通过 `MSYS_NO_PATHCONV=1` 阻止自动转换。
#
# 用法：
#   ./scripts/build.sh                 # debug 构建
#   ./scripts/build.sh --release       # release 构建
#   ./scripts/build.sh --run           # 构建并运行
#   ./scripts/build.sh --test          # 运行测试

set -euo pipefail

MSVC_ROOT='D:\Program Files\Microsoft Visual Studio\18\Insiders\VC\Tools\MSVC\14.52.36629'
SDK_ROOT='C:\Program Files (x86)\Windows Kits\10'

# 自动挑选最新的 Windows SDK 版本，避免硬编码小版本号后失效。
if [ ! -d "/c/Program Files (x86)/Windows Kits/10/Include" ]; then
    echo "错误：未找到 Windows SDK。" >&2
    exit 1
fi
SDK_VER=$(ls -1 "/c/Program Files (x86)/Windows Kits/10/Include" | sort -V | tail -1)

if [ ! -f "$MSVC_ROOT/bin/Hostx64/x64/cl.exe" ]; then
    echo "错误：未找到 cl.exe（$MSVC_ROOT）。" >&2
    exit 1
fi

export MSVC_NO_PATHCONV=1
export MSYS_NO_PATHCONV=1
export CC=cl.exe
export CXX=cl.exe
export AR=lib.exe

export INCLUDE="${MSVC_ROOT}\\include;${SDK_ROOT}\\Include\\${SDK_VER}\\ucrt;${SDK_ROOT}\\Include\\${SDK_VER}\\um;${SDK_ROOT}\\Include\\${SDK_VER}\\shared;${SDK_ROOT}\\Include\\${SDK_VER}\\winrt"
export LIB="${MSVC_ROOT}\\lib\\x64;${SDK_ROOT}\\Lib\\${SDK_VER}\\ucrt\\x64;${SDK_ROOT}\\Lib\\${SDK_VER}\\um\\x64"

# 让 cargo 在 PATH 中找到 cl.exe / link.exe。
export PATH="/d/Program Files/Microsoft Visual Studio/18/Insiders/VC/Tools/MSVC/14.52.36629/bin/Hostx64/x64:${PATH}"

export TARGET=x86_64-pc-windows-msvc

MODE=debug
ACTION=build
# 探针（examples/）不在默认构建目标里，需显式 `--example <name>`。
# 不加这个开关时 `cargo build` 只出库与主程序，探针 exe 不会生成，
# 而排查渲染问题恰恰只能靠探针——这个缺口曾导致「探针不存在」的误判。
EXAMPLE=""
# 除下面几个自有开关外，其余参数原样透传给 cargo（如 --nocapture、-p <crate>）。
EXTRA=()
while [ $# -gt 0 ]; do
    case "$1" in
        --release) MODE=release; shift ;;
        --run)     ACTION=run; shift ;;
        --test)    ACTION=test; shift ;;
        --example) EXAMPLE="$2"; ACTION=example; shift 2 ;;
        --example=*) EXAMPLE="${1#--example=}"; ACTION=example; shift ;;
        *) EXTRA+=("$1"); shift ;;
    esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

case "$ACTION" in
    build)
        cargo build --target "$TARGET" ${MODE:+$( [ "$MODE" = release ] && echo --release )}
        ;;
    run)
        cargo run --target "$TARGET" $( [ "$MODE" = release ] && echo --release )
        ;;
    test)
        # 透传额外参数（如 --nocapture、icons::），便于单跑某个测试看诊断输出。
        cargo test --target "$TARGET" --workspace ${EXTRA[@]+"${EXTRA[@]}"}
        ;;
    example)
        cargo build --target "$TARGET" ${MODE:+$( [ "$MODE" = release ] && echo --release )} \
            --example "$EXAMPLE" ${EXTRA[@]+"${EXTRA[@]}"}
        ;;
esac