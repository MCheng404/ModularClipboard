# -*- coding: utf-8 -*-
"""多行 use 组的未用导入清理：读 cargo check 告警，按语句改写（支持跨行组）。"""
import io
import re
import subprocess
import sys

CRATE = sys.argv[1] if len(sys.argv) > 1 else "modular-clipboard-gfx"

WARN = re.compile(r"^warning: unused imports?: (.+)$")
LOC = re.compile(r"^\s*-->\s*(crates[^:]+\.rs):(\d+):\d+\s*$")
NAME = re.compile(r"`([^`]+)`")


def run_check():
    p = subprocess.run(["cargo", "check", "-p", CRATE, "--all-targets"],
                       capture_output=True, text=True, shell=True)
    out = p.stdout + p.stderr
    hits = {}
    lines = out.splitlines()
    for i, ln in enumerate(lines):
        m = WARN.match(ln.strip())
        if not m:
            continue
        for j in range(i + 1, min(i + 4, len(lines))):
            ml = LOC.match(lines[j])
            if ml:
                path, line_no = ml.group(1), int(ml.group(2))
                hits.setdefault(path, {}).setdefault(line_no, set()).update(
                    NAME.findall(m.group(1)))
                break
    return hits


def local_name(entry):
    e = entry.strip()
    if " as " in e:
        return e.split(" as ")[-1].strip()
    return e.split("::")[-1]


def entry_matches(entry, bad, base):
    """告警名可能三种形态：裸名 / 完整路径 / `Orig as Alias` 全串。"""
    e = entry.strip()
    if e in bad:
        return True
    if local_name(e) in bad:
        return True
    orig = e.split(" as ")[0].strip()
    if "%s::%s" % (base, orig) in bad:
        return True
    return False


for round_no in range(10):
    hits = run_check()
    if not hits:
        print("clean")
        break
    changed = 0
    for path, lines_map in hits.items():
        src = io.open(path, encoding="utf-8", newline="").read()
        lines = src.split("\n")
        # 按语句归并：从告警行向上找 use 起始行，向下找分号
        stmt_groups = {}  # stmt_start_line(1-based) -> set(names)
        for line_no, bad in lines_map.items():
            i = line_no - 1
            while i > 0 and not lines[i].lstrip().startswith("use "):
                i -= 1
            stmt_groups.setdefault(i + 1, set()).update(bad)
        for start, bad in sorted(stmt_groups.items(), reverse=True):
            i = start - 1
            j = i
            while j < len(lines) and not lines[j].rstrip().endswith(";"):
                j += 1
            stmt = "\n".join(lines[i:j + 1])
            m = re.match(r"^(\s*)use\s+([\w:]+)::\{([^}]*)\};\s*$", stmt, re.S)
            if m:
                indent, base, inner = m.groups()
                entries = [e.strip() for e in inner.split(",") if e.strip()]
                keep = [e for e in entries if not entry_matches(e, bad, base)]
                if not keep:
                    del lines[i:j + 1]
                elif len(keep) == 1:
                    lines[i:j + 1] = ["%suse %s::%s;" % (indent, base, keep[0])]
                else:
                    lines[i:j + 1] = ["%suse %s::{%s};" % (indent, base, ", ".join(keep))]
                changed += 1
                continue
            m2 = re.match(r"^(\s*)use\s+([\w:]+);\s*$", stmt, re.S)
            if m2 and entry_matches(m2.group(2), bad, ""):
                del lines[i:j + 1]
                changed += 1
        io.open(path, "w", encoding="utf-8", newline="").write("\n".join(lines))
    print("round %d: %d statements rewritten" % (round_no, changed))
    if not changed:
        break
else:
    print("ERROR: not converged")
    sys.exit(1)