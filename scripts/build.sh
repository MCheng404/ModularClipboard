#!/usr/bin/env bash
# ModularClipboard 构建入口。
#
# 环境变量由 scripts/env.sh 统一注入（MSVC / Windows SDK），本脚本只负责
# 「决定跑哪个 cargo 命令」。两者分开，才好在 shell 里直接 source env.sh
# 跑 cargo install / clippy 之类的一次性命令。
#
# 用法：
#   ./scripts/build.sh                  # debug 构建
#   ./scripts/build.sh --release        # release 构建
#   ./scripts/build.sh --run            # 构建并运行
#   ./scripts/build.sh --test           # 跑测试（默认跳过 doctest，更快）
#   ./scripts/build.sh --doctest        # 只跑 doctest
#   ./scripts/build.sh --test icons::   # 只跑名字含 icons:: 的测试
#   ./scripts/build.sh --package        # 生成规范化产物（见 package.sh）
#   ./scripts/build.sh --example probe  # 构建探针（examples/）
#
# 除上面几个自有开关外，其余参数原样透传给 cargo（如 --nocapture、-p <crate>）。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# shellcheck source=env.sh
source "$ROOT/scripts/env.sh"

MODE=debug
ACTION=build
# 探针（examples/）不在默认构建目标里，需显式 `--example <name>`。
# 不加这个开关时 `cargo build` 只出库与主程序，探针 exe 不会生成，
# 而排查渲染问题恰恰只能靠探针——这个缺口曾导致「探针不存在」的误判。
EXAMPLE=""
EXTRA=()
while [ $# -gt 0 ]; do
    case "$1" in
        --release) MODE=release; shift ;;
        --run)     ACTION=run; shift ;;
        --test)    ACTION=test; shift ;;
        --doctest) ACTION=doctest; shift ;;
        --package) ACTION=package; shift ;;
        --example) EXAMPLE="$2"; ACTION=example; shift 2 ;;
        --example=*) EXAMPLE="${1#--example=}"; ACTION=example; shift ;;
        *) EXTRA+=("$1"); shift ;;
    esac
done

# cargo 只认 `--release` 这一个开关，debug 时不传任何 profile 参数。
profile_flag() {
    [ "$MODE" = release ] && echo --release
}

case "$ACTION" in
    package)
        exec "$ROOT/scripts/package.sh" ${EXTRA[@]+"${EXTRA[@]}"}
        ;;
    build)
        cargo build --target "$TARGET" $(profile_flag) ${EXTRA[@]+"${EXTRA[@]}"}
        ;;
    run)
        cargo run --target "$TARGET" $(profile_flag) ${EXTRA[@]+"${EXTRA[@]}"}
        ;;
    test)
        # NOTE: 默认**跳过 doctest**（`--lib --tests`）。
        #
        # doctest 是 7 个独立的编译+运行目标，而本项目一共只有 1 条
        # （还是 ignored）。实测热缓存下「含 doctest」3.7s、「跳过」1.6s
        # —— 每次跑测试多付 2 秒，换回 0 条有意义的用例。
        # 需要跑它们时用 `--doctest`。
        cargo test --target "$TARGET" --workspace --lib --tests ${EXTRA[@]+"${EXTRA[@]}"}
        ;;
    doctest)
        cargo test --target "$TARGET" --workspace --doc ${EXTRA[@]+"${EXTRA[@]}"}
        ;;
    example)
        cargo build --target "$TARGET" $(profile_flag) \
            --example "$EXAMPLE" ${EXTRA[@]+"${EXTRA[@]}"}
        ;;
esac
