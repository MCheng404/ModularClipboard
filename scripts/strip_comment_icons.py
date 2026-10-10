#!/usr/bin/env python3
"""把代码注释里的图标标记换成专业术语。

覆盖的图标（用码位写出，避免本文件自己再引入它们）：

    U+26A0 (+U+FE0F)  旧写法里的"注意"  -> NOTE / IMPORTANT / CAUTION
    U+2705            -> DONE:
    U+274C            -> ERROR:
    U+2B1C            -> TODO:
    U+1F527           -> TODO:

为什么做这件事：

1. 这些字符不在 GBK 里。本仓库的启动脚本（.cmd/.vbs）与若干验证脚本
   是 GBK 编码，注释一旦被搬进去（或文件被工具存成 GBK）就会变成乱码
   甚至语法错误 —— 而这类问题只在"换台机器/换个编辑器"时才暴露。
2. 图标在 diff、日志、终端里的宽度不一致，对齐会错乱。
3. 旧写法只有一个语义层级，"必须这样做"和"千万别这样做"长得一样，
   读的人分不出轻重。

术语表（也在 BUILD.md 里定义）：

    NOTE:       说明、背景、为什么这么写
    IMPORTANT:  必须满足的约束（不这么做就不对）
    CAUTION:    禁止/有坑（这么做会出问题）
    ERROR:      错误写法（用于两段对照的前一段）
    CORRECT:    正确写法（用于两段对照的后一段）
    TODO:       待办
    DONE:       已完成

用法：
    python scripts/strip_comment_icons.py          # 实际改写
    python scripts/strip_comment_icons.py --dry-run # 只报告
"""

from __future__ import annotations

import io
import os
import sys

# 判断"轻重"的关键词。顺序重要：先判禁止，再判必须，最后兜底。
PROHIBIT = ("不能", "不要", "不该", "不可", "切勿", "禁止", "绝不", "不许",
            "否则", "会崩", "会 panic", "会出错", "会漏", "会串", "会丢失")
REQUIRE = ("必须", "一定要", "务必", "关键", "不可或缺", "才能", "否则不")


def classify(tail: str) -> str:
    """按注释正文判断该用哪个标记。"""
    window = tail[:120]
    if any(w in window for w in PROHIBIT):
        return "CAUTION:"
    if any(w in window for w in REQUIRE):
        return "IMPORTANT:"
    return "NOTE:"


# 图标用码位给出：本文件自身不引入这些字符，免得它自己又变成
# 「需要被清理的文件」，也让本文件可以安全地按任意编码搬运。
WARN = "\u26a0\ufe0f"       # 旧写法里的“注意”（含变体选择符）
WARN_BARE = "\u26a0"        # 不带变体选择符的写法
ICONS = (
    ("\u2705", "DONE:"),
    ("\u274c", "ERROR:"),
    ("\u2b1c", "TODO:"),
    ("\U0001f527", "TODO:"),
)


def convert(text: str) -> tuple[str, int]:
    n = 0
    out = []
    for line in text.split("\n"):
        # 先按带变体选择符的形式处理，再兜底裸字符。
        for mark in (WARN, WARN_BARE):
            while mark in line:
                n += 1
                i = line.index(mark)
                tail = line[i + len(mark):]
                line = line[:i] + classify(tail) + " " + tail.lstrip(" ")
        for icon, word in ICONS:
            if icon in line:
                n += line.count(icon)
                line = line.replace(icon, word)
        out.append(line)
    return "\n".join(out), n


def main() -> int:
    dry = "--dry-run" in sys.argv
    root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
    total = 0
    files = 0
    for base, dirs, names in os.walk(os.path.join(root, "crates")):
        dirs[:] = [d for d in dirs if d != "target"]
        for name in names:
            if not name.endswith((".rs", ".py", ".wgsl")):
                continue
            path = os.path.join(base, name)
            src = io.open(path, encoding="utf-8").read()
            new, n = convert(src)
            if n:
                files += 1
                total += n
                rel = os.path.relpath(path, root)
                print(f"{'would' if dry else 'did  '} {rel}: {n}")
                if not dry:
                    io.open(path, "w", encoding="utf-8", newline="").write(new)
    print(f"---- 合计 {total} 处 / {files} 个文件")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
