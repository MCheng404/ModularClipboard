#!/usr/bin/env bash
# ModularClipboard 构建环境引导（只导出环境变量，不执行任何构建）。
#
# 为什么单独拆出来：
#   `cargo install cargo-nextest`、`cargo clippy`、手写 `cargo build`
#   等都需要同一套 MSVC 环境变量，但以前它们只存在于 build.sh 内部，
#   于是每次都要「先跑一次 build.sh 再手动导出」，极易漏。
#   source 本文件即可拿到完整环境。
#
# 用法：
#   source scripts/env.sh          # 在当前 shell 里生效
#   source scripts/env.sh --print  # 只打印关键变量，便于排查
#
# 为什么需要它（而不是直接 cargo）：
#   Visual Studio 装在 D:\Program Files（非常规路径），系统 PATH 里
#   没有 cl.exe。cc-rs 要编译 sqlite3.c，必须显式注入 INCLUDE/LIB。
#
# 路径约定：
#   Git Bash 会把 POSIX 路径（/d/Program Files/...）转换后再传给子进程，
#   导致 cl.exe 定位不到头文件。这里统一用原生 Windows 路径，并通过
#   MSYS_NO_PATHCONV=1 阻止自动转换。

# 被 source 时 BASH_SOURCE[0] 仍指向本文件，cd 后会失效，先存下来。
_ENV_SH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

MSVC_ROOT='D:\Program Files\Microsoft Visual Studio\18\Insiders\VC\Tools\MSVC\14.52.36629'
SDK_ROOT='C:\Program Files (x86)\Windows Kits\10'

# 自动挑选最新的 Windows SDK，避免硬编码小版本号后失效。
if [ ! -d "/c/Program Files (x86)/Windows Kits/10/Include" ]; then
    echo "ERROR: 未找到 Windows SDK（$SDK_ROOT）。" >&2
    return 1 2>/dev/null || exit 1
fi
SDK_VER=$(ls -1 "/c/Program Files (x86)/Windows Kits/10/Include" | sort -V | tail -1)

if [ ! -f "$MSVC_ROOT/bin/Hostx64/x64/cl.exe" ]; then
    echo "ERROR: 未找到 cl.exe（$MSVC_ROOT）。" >&2
    return 1 2>/dev/null || exit 1
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

if [ "${1:-}" = "--print" ]; then
    echo "TARGET=$TARGET"
    echo "SDK_VER=$SDK_VER"
    echo "CC=$CC"
    echo "INCLUDE=$INCLUDE"
fi
