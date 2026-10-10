#!/usr/bin/env bash
# 生成**规范化**的发布产物。
#
# 与直接 `cargo build --release` 后手工拷贝的区别：
#
# 1. 产物目录**先清空再写入** —— 不残留上一次构建的东西。
#    只覆盖同名文件是不够的：一旦产物改名、或上次留下了校验和/截图/
#    临时文件，它们会一直躺在 dist/ 里，最后被当成"本次产物"分发出去。
# 2. 产物**位置与内容固定**（见 `dist` 布局），不随构建方式变化。
# 3. 同时产出校验和与构建信息，便于核对"拿到的到底是哪一个构建"。
#
# dist 布局（唯一约定）：
#   dist/modular-clipboard.exe   主程序，单文件、静态链 CRT
#   dist/SHA256SUMS.txt          sha256sum -c 可直接校验
#   dist/BUILD_INFO.txt          版本 / 提交 / 构建时间 / 目标 / rustc
#
# 用法：
#   ./scripts/package.sh              # release 构建 + 生成产物
#   ./scripts/package.sh --debug      # 用 debug 构建（排查用，产物很大）
#   ./scripts/package.sh --no-clean   # 不清空 dist（一般不要用）
#
# dist/ 已在 .gitignore 中：二进制不入库，分发走 GitHub Release。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# shellcheck source=env.sh
source "$ROOT/scripts/env.sh"

MODE=release
CLEAN=1
while [ $# -gt 0 ]; do
    case "$1" in
        --debug)    MODE=debug; shift ;;
        --release)  MODE=release; shift ;;
        --no-clean) CLEAN=0; shift ;;
        *) echo "ERROR: 未知参数 $1" >&2; exit 2 ;;
    esac
done

PROFILE_FLAG=$([ "$MODE" = release ] && echo --release)
BIN_NAME="modular-clipboard.exe"
SRC="$ROOT/target/$TARGET/$MODE/$BIN_NAME"
DIST="$ROOT/dist"

echo "==> 构建（$MODE）"
cargo build --target "$TARGET" $PROFILE_FLAG

if [ ! -f "$SRC" ]; then
    echo "ERROR: 未找到构建产物 $SRC" >&2
    exit 1
fi

# ---- 清空产物目录 --------------------------------------------------
#
# 只删 dist 里的内容而不删目录本身：目录被 shell/资源管理器占用时
# 删目录会失败，删内容不会。
if [ "$CLEAN" = 1 ]; then
    echo "==> 清空 $DIST（避免残留上一次构建的产物）"
    mkdir -p "$DIST"
    find "$DIST" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
fi
mkdir -p "$DIST"

echo "==> 安装主程序"
cp -f "$SRC" "$DIST/$BIN_NAME"

# ---- 构建信息 ------------------------------------------------------
#
# 用 `git rev-parse --short` 而不是读 Cargo.toml：后者不含提交信息，
# 而"这个 exe 是哪次提交编出来的"恰恰是最需要回答的问题。
# 非 git 环境（例如从 tarball 构建）降级为 unknown，不中断。
COMMIT=$(git rev-parse --short=12 HEAD 2>/dev/null || echo "unknown")
# dirty 只看**已跟踪文件**是否与 HEAD 不同。未跟踪文件（原型草稿、
# 本地笔记）不参与编译，不该让产物被标成 dirty —— 那会把这个标记
# 稀释成一个几乎恒为真的噪音，用户也就懒得看了。
if git diff --quiet HEAD -- 2>/dev/null; then
    DIRTY=""
else
    DIRTY=" (有未提交改动)"
fi
VERSION=$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed -E 's/.*"(.*)".*/\1/')
BUILD_UTC=$(date -u '+%Y-%m-%dT%H:%M:%SZ')

cat > "$DIST/BUILD_INFO.txt" <<EOF
name        modular-clipboard
version     $VERSION
commit      $COMMIT$DIRTY
built_at    $BUILD_UTC (UTC)
profile     $MODE
target      $TARGET
rustc       $(rustc -vV | sed -n '1p')
cargo       $(cargo --version)
size_bytes  $(stat -c%s "$DIST/$BIN_NAME")
EOF

echo "==> 计算校验和"
# 用相对路径写进清单，这样 `cd dist && sha256sum -c SHA256SUMS.txt` 能直接用。
( cd "$DIST" && sha256sum "$BIN_NAME" > SHA256SUMS.txt )

echo
echo "==> 产物清单"
ls -l "$DIST"
echo
cat "$DIST/BUILD_INFO.txt"
echo
cat "$DIST/SHA256SUMS.txt"
