"""修正 param 默认值里的「变量不会被插值」问题。

# 问题

`fix_hardcoded_paths.py` 把

    [string]$OutDir = 'D:\\WorkBuddy\\Tiez'

改成了

    [string]$OutDir = '$RepoRoot'

但 **PowerShell 的 param 默认值不做变量插值**——它就是字面字符串
`$RepoRoot`。于是脚本会把一个名为 `$RepoRoot` 的目录当成输出目录，
`New-Item` 之类的调用要么失败、要么在 CWD 下建出一个奇怪的目录。

而且 `$RepoRoot` 的定义在 param 块**之后**，即使能插值也取不到值。

# 改法

默认值留空，脚本内在 `$RepoRoot` 定义之后回填。
"""

from __future__ import annotations

import glob
import io
import os

ANCHOR = "$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path"
BACKFILL = (
    "\n\n# param 默认值里PowerShell **不做变量插值**（那是字面串），"
    "\n# 所以默认值留空，在这里回填。\nif (-not $OutDir) { $OutDir = $RepoRoot }"
)

root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

for rel in sorted(glob.glob(os.path.join(root, "scripts", "*.ps1"))):
    text = io.open(rel, encoding="utf-8").read()
    original = text

    # 只改 param 块内的默认值
    text = text.replace("[string]$OutDir = '$RepoRoot',", "[string]$OutDir = '',")

    if text == original:
        continue

    if ANCHOR not in text:
        print(f"[跳过] {os.path.basename(rel)}：找不到 $RepoRoot 定义行")
        continue

    text = text.replace(ANCHOR, ANCHOR + BACKFILL, 1)
    io.open(rel, "w", encoding="utf-8").write(text)
    print(f"[已修] {os.path.basename(rel)}")