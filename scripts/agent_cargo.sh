#!/usr/bin/env bash
# 仅供 Agent 使用：在build.sh 的同一套MSVC 环境下跑任意 cargo 子命令。
#
# 为什么不直接改 build.sh：它是全组共用的构建入口，加自定义参数会
# 造成「Agent 改了主控脚本」的并发编辑冲突（MEMORY坑38）。
# 本文件是**新增**文件，不改 build.sh 任何一行。
#
# 用法：
#   ./scripts/agent_cargo.sh test -p modular-clipboard-ui --lib
#   ./scripts/agent_cargo.sh build -p modular-clipboard-ui
set -euo pipefail

MSVC_ROOT='D:\Program Files\Microsoft Visual Studio\18\Insiders\VC\Tools\MSVC\14.52.36629'
SDK_ROOT='C:\Program Files (x86)\Windows Kits\10'

SDK_VER=$(ls -1 "/c/Program Files (x86)/Windows Kits/10/Include" | sort -V | tail -1)

if [ ! -f "${MSVC_ROOT}/bin/Hostx64/x64/cl.exe" ]; then
    echo "错误：未找到 cl.exe（${MSVC_ROOT}）。" >&2
    exit 1
fi

export MSVC_NO_PATHCONV=1
export MSYS_NO_PATHCONV=1
export CC=cl.exe
export CXX=cl.exe
export AR=lib.exe

export INCLUDE="${MSVC_ROOT}\\include;${SDK_ROOT}\\Include\\${SDK_VER}\\ucrt;${SDK_ROOT}\\Include\\${SDK_VER}\\um;${SDK_ROOT}\\Include\\${SDK_VER}\\shared;${SDK_ROOT}\\Include\\${SDK_VER}\\winrt"
export LIB="${MSVC_ROOT}\\lib\\x64;${SDK_ROOT}\\Lib\\${SDK_VER}\\ucrt\\x64;${SDK_ROOT}\\Lib\\${SDK_VER}\\um\\x64"

export PATH="/d/Program Files/Microsoft Visual Studio/18/Insiders/VC/Tools/MSVC/14.52.36629/bin/Hostx64/x64:${PATH}"

export TARGET=x86_64-pc-windows-msvc
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-D:/WorkBuddy/Tiez/target-verify}"

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "$#" -eq 0 ]; then
    echo "用法：$0 <cargo 子命令> [参数...]" >&2
    exit 2
fi

SUB="$1"
shift

# `--target` 必须放在子命令之后、其余参数之前。
# 追加到末尾会撞上 `-- --list` 这类原样传给测试二进制的参数
# （`cargo test -- --list --target ...` 会被测试程序当成自己的选项）。
cargo "$SUB" --target "$TARGET" "$@"