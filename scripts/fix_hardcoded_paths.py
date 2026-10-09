"""把脚本里的硬编码仓库路径改为按脚本位置自动推导。

# 为什么必须做

项目目录搬过家（`D:\\WorkBuddy\\Tiez` → `D:\\modular-clipboard`），
但 10 个验证脚本里的路径**全是硬编码的绝对路径**，于是**全部失效**：

    找不到 exe：D:\\WorkBuddy\\Tiez\\target\\...\\modular-clipboard.exe

验证手段全废 ⇒ 无法实机确认改动，只能靠推断——这正是最危险的处境。

# 改法

`$PSScriptRoot` 是 PowerShell 自动提供的变量，指向脚本所在目录。
`scripts\\xxx.ps1` 的 `$PSScriptRoot` 就是 `<repo>\\scripts`，
所以 `<repo>` = `$PSScriptRoot\\..`。

替换规则（按出现位置分类）：

  D:\\WorkBuddy\\Tiez\\scripts\\   →  $PSScriptRoot\\          （同目录脚本）
  D:\\WorkBuddy\\Tiez\\           →  $RepoRoot\\               （仓库其它位置）

用法：
    python scripts/fix_hardcoded_paths.py --check   # 只报告，不改
    python scripts/fix_hardcoded_paths.py           # 执行替换
"""

from __future__ import annotations

import io
import re
import sys
from pathlib import Path

# 旧目录前缀。出现次数最多的那个。
OLD_PREFIX = r"D:\WorkBuddy\Tiez"

# 逐个脚本要替换的目标形式：(旧串, 新串)
# ⚠️ 这里**不能用 rf-string**：`rf"...\\"` 里的 `\\` 是**两个**反斜杠，
# 而文件里只有一个（`D:\WorkBuddy\Tiez\`）。raw string 不会处理 `\\` 转义，
# 第一版就因此「改了个寂寞」—— 文件原封不动，只多了段引导代码。
#
# 用普通字符串 + 双写反斜杠：`OLD_PREFIX + "\\"` 才是「前缀 + 一个反斜杠」。
_BACKSLASH = "\\"
REPLACEMENTS = [
    # ⚠️ 顺序要紧：具体的必须在通配的前面，否则 `...\Tiez\scripts\xxx`
    # 会被通配规则先吃掉、变成 `$RepoRoot\scripts\xxx`（语义仍对，
    # 但之后再想单独调整 scripts 基准就没法匹配了）。
    #
    # scripts/ 目录下的文件
    (OLD_PREFIX + "\\scripts" + _BACKSLASH, "$PSScriptRoot" + _BACKSLASH),
    # 仓库根下的文件（target/、*.log、*.err 等）
    (OLD_PREFIX + _BACKSLASH, "$RepoRoot" + _BACKSLASH),
    # 末尾没有反斜杠的裸路径（如 `[string]$OutDir = 'D:\WorkBuddy\Tiez'`）
    (OLD_PREFIX, "$RepoRoot"),
]

# 需要在每个改过的脚本里插入的引导代码。
BOILERPLATE = """
# ⚠️ 由 scripts/fix_hardcoded_paths.py 插入：按**脚本自身位置**推导仓库根，
# 不再硬编码绝对路径。项目目录改名 / 搬走后，验证脚本依然能用。
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
"""


def needs_fix(text: str) -> bool:
    return OLD_PREFIX in text


def apply(text: str) -> str:
    # 顺序要紧：先替换更具体的 `...\\scripts\\`，否则通配的 `...\\`
    # 会把它先吃掉、留下 `$RepoRoot\\scripts\\`（其实也对，但不如前者干净）。
    for old, new in REPLACEMENTS:
        text = text.replace(old, new)

    # 插引导代码：放在 param 块之后、正文第一个语句之前。
    #
    # ⚠️ 判据是「**还没有**插过引导代码」，而不是「文本里出现过 $RepoRoot」——
    # 后者会在刚做完替换时就成立，导致这里误判成「已处理过」而
    # **把替换结果整个丢掉**（第一版就踩了这个：替换后文件原封不动）。
    if "$RepoRoot =" not in text:
        # 找 param(...) 块的结束位置
        m = re.search(r"^param\s*\(", text, re.M)
        if m:
            depth = 0
            for i in range(m.end() - 1, len(text)):
                if text[i] == "(":
                    depth += 1
                elif text[i] == ")":
                    depth -= 1
                    if depth == 0:
                        return text[: i + 1] + "\n" + BOILERPLATE + text[i + 1 :]
        # 没有 param 块：插在开头的文档注释之后
        lines = text.split("\n")
        idx = 0
        for i, line in enumerate(lines):
            if not line.startswith("#"):
                idx = i
                break
        lines.insert(idx, BOILERPLATE)
        return "\n".join(lines)
    return text


def main() -> int:
    mode = sys.argv[1] if len(sys.argv) > 1 else "--check"
    root = Path(__file__).resolve().parent.parent
    scripts = sorted(
        [p for p in (root / "scripts").iterdir() if p.suffix in (".ps1", ".sh")]
    )

    dirty: list[tuple[Path, int]] = []
    for p in scripts:
        try:
            raw = p.read_bytes()
        except OSError:
            continue
        # .ps1 可能是 UTF-8；.sh 一定是 UTF-8
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError:
            continue
        if needs_fix(text):
            dirty.append((p, text.count(OLD_PREFIX)))

    if not dirty:
        print("[OK] 所有脚本都不含硬编码仓库路径")
        return 0

    for p, n in dirty:
        print(f"[{'需修' if mode == '--check' else '修复'}] {p.relative_to(root)}：{n} 处")

    if mode == "--check":
        print("\n加 --check 之外的无参数运行即可执行替换")
        return 1

    for p, _ in dirty:
        text = p.read_text(encoding="utf-8")
        fixed = apply(text)
        p.write_text(fixed, encoding="utf-8")
        print(f"[已改] {p.relative_to(root)}")

    # 回读校验
    print("\n=== 回读校验 ===")
    still = [
        p for p in scripts if p.suffix == ".ps1" and needs_fix(p.read_text(encoding="utf-8"))
    ]
    if still:
        print("[失败] 仍有残留：", [str(s.relative_to(root)) for s in still])
        return 1
    print("[OK] 已无硬编码仓库路径")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())