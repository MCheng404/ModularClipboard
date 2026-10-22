# -*- coding: utf-8 -*-
"""迭代清理未用导入：读 cargo check --message-format=short 的告警，
按行改写 use 语句（支持分组部分移除），重复直到无未用导入告警。"""
import io
import re
import subprocess
import sys

CRATE = "modular-clipboard-ui"

WARN = re.compile(r"^warning: unused imports?: (.+)$")
LOC = re.compile(r"^\s*-->\s*(crates[^:]+\.rs):(\d+):\d+\s*$")
NAME = re.compile(r"`([^`]+)`")


def run_check():
    p = subprocess.run(
        ["cargo", "check", "-p", CRATE],
        capture_output=True, text=True, shell=True,
    )
    out = p.stdout + p.stderr
    hits = {}
    lines = out.splitlines()
    for i, ln in enumerate(lines):
        m = WARN.match(ln.strip())
        if not m:
            continue
        # 下一非空行应是 --> file:line:col
        for j in range(i + 1, min(i + 4, len(lines))):
            ml = LOC.match(lines[j])
            if ml:
                path, line_no, names = ml.group(1), int(ml.group(2)), m.group(1)
                bad = NAME.findall(names)
                hits.setdefault(path, {}).setdefault(line_no, set()).update(bad)
                break
    return hits


def rewrite_line(line, bad):
    """从一条 use 语句里删掉未用名；全删则返回 None。"""
    m = re.match(r"^(\s*)use\s+([A-Za-z_][\w:]*)::\{([^}]*)\};\s*$", line)
    if m:
        indent, path, inner = m.groups()
        # 名字可能带 as 别名，按顶层逗号切分
        parts = [p.strip() for p in inner.split(",") if p.strip()]
        keep = []
        for p in parts:
            name = p.split(" as ")[0].strip()
            if name in bad or p in bad:
                continue
            keep.append(p)
        if not keep:
            return None
        if len(keep) == 1:
            return "%suse %s::%s;" % (indent, path, keep[0])
        return "%suse %s::{%s};" % (indent, path, ", ".join(keep))
    # 单名形式：use path::name;  或 use path::{name};
    m2 = re.match(r"^(\s*)use\s+([\w:]+);\s*$", line)
    if m2:
        full = m2.group(2)
        if full in bad or full.split("::")[-1] in bad:
            return None
    return line


for round_no in range(12):
    hits = run_check()
    if not hits:
        print("round %d: clean" % round_no)
        break
    changed = 0
    for path, lines_map in hits.items():
        src = io.open(path, encoding="utf-8", newline="").read()
        lines = src.split("\n")
        for line_no, bad in sorted(lines_map.items(), reverse=True):
            idx = line_no - 1
            if idx >= len(lines):
                continue
            new = rewrite_line(lines[idx], bad)
            if new is None:
                lines[idx] = None
                changed += 1
            elif new != lines[idx]:
                lines[idx] = new
                changed += 1
        lines = [l for l in lines if l is not None]
        if changed:
            io.open(path, "w", encoding="utf-8", newline="").write("\n".join(lines))
    print("round %d: %d lines rewritten" % (round_no, changed))
    if not changed:
        break
else:
    print("ERROR: 12 轮未收敛")
    sys.exit(1)
