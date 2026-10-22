# -*- coding: utf-8 -*-
"""通用 rs 拆分器：foo.rs -> foo.rs(root) + foo/<name>.rs。

用法：改下面 SPEC 后运行。行界须先人工核验（带断言）。
特性：
- 分区可为多个不相邻区间；
- 按顶层 use 块 + 名称出现精确计算每个子文件的导入；
- 跨子模块引用自动生成 use super::other::item;
- 可见性：fn/struct/enum/const/static 默认 pub(super)，KEEP_PUB 保持 pub；
- recording 式「方法段」可用 METHOD_VIS 包进 impl super::RootType；
- root 追加 mod 声明 / 再导出 / 自用导入；tests 补子模块 glob。
"""
import io
import os
import re
import sys

# ---------------------------------------------------------------------- SPEC
SPEC = {
    "path": "crates/modular-clipboard-ui/src/renderer.rs",
    "subdir": "crates/modular-clipboard-ui/src/renderer",
    "sections": {
        # name: {"ranges": [(s,e)...], "doc": "...", "mode": "free"|"methods",
        #        "method_vis": {...}, "keep_pub": {...}}
        "painter": {
            "ranges": [(397, 1961)],
            "doc": "//! `Painter`：一帧的绘制上下文。\n"
                   "//!\n"
                   "//! 持有字体图集、用户纹理表、离屏模糊链与磨砂背景资源；\n"
                   "//! `Drop` 按「描述符引用者 -> 被引用者」逆序释放。\n"
                   "//! 本文件只放 Painter 本体；批次与纯函数在根，槽位缓冲在根。",
            "mode": "free",
        },
    },
    "root_parts": [(1, 396), (1962, 3145)],
    "root_extra": [],
    "root_reexport": ["pub(crate) use painter::Painter;"],
    "tests_marker": "mod tests {",
    "asserts": [
        (398, "// Painter"),
        (1964, "顶点/索引缓冲"),
        (106, "pub struct Batch"),
        (2134, "mod tests {"),
    ],
}

# ---------------------------------------------------------------------- 引擎
path = SPEC["path"]
src = io.open(path, encoding="utf-8", newline="").read()
lines = src.split("\n")


def L(n):
    return lines[n - 1]


for n, needle in SPEC["asserts"]:
    assert needle in L(n), "assert failed at %d: %r not in %r" % (n, needle, L(n)[:60])

os.makedirs(SPEC["subdir"], exist_ok=True)

moved = {}
for name, spec in SPEC["sections"].items():
    body = []
    for i, (s, e) in enumerate(spec["ranges"]):
        if i:
            body.append("")
        body.extend(lines[s - 1:e])
    moved[name] = body

root = []
for s, e in SPEC["root_parts"]:
    root.extend(lines[s - 1:e])

# ---- 顶层导入清单（支持多行 use 块）----
def parse_imports(body_lines):
    """把一段源码里的 use 语句解析成 [(name, stmt)]，支持多行花括号组。"""
    out = []
    i = 0
    n = len(body_lines)
    while i < n:
        ln = body_lines[i]
        if not re.match(r"^use\s", ln):
            i += 1
            continue
        stmt_lines = [ln]
        while not stmt_lines[-1].rstrip().endswith(";"):
            i += 1
            if i >= n:
                break
            stmt_lines.append(body_lines[i])
        stmt = "\n".join(stmt_lines)
        m = re.match(r"^use\s+([\w:]+)::\{([^}]*)\};\s*$", stmt, re.S)
        if m:
            base, group = m.group(1), m.group(2)
            for part in group.split(","):
                part = part.strip()
                if part:
                    out.append((part.split(" as ")[0].strip(),
                                "use %s::%s;" % (base, part)))
        else:
            m2 = re.match(r"^use\s+([\w:]+);\s*$", stmt, re.S)
            if m2:
                base = m2.group(1)
                out.append((base.split("::")[-1], "use %s;" % base))
            else:
                # use a::{b as c, d}; 的别名等形态：整条保留，按别名词登记
                m3 = re.match(r"^use\s+([\w:]+)::\{([^}]*)\};\s*$", stmt, re.S)
                if m3:
                    base = m3.group(1)
                    for part in m3.group(2).split(","):
                        part = part.strip()
                        if part:
                            nm = part.split(" as ")[-1].strip()
                            out.append((nm, "use %s::%s;" % (base, part)))
        i += 1
    return out


top_imports = parse_imports(lines[:120])

ITEM_DEF = re.compile(r"^(?:pub(?:\(crate\))?\s+)?(?:pub\(super\)\s+)?(const fn|fn|struct|enum|const|static|type)\s+([A-Za-z_][A-Za-z0-9_]*)")


def code_text(body):
    out = []
    for ln in body:
        if ln.strip().startswith("//"):
            continue
        out.append(re.sub(r'"(?:[^"\\]|\\.)*"', '""', ln))
    return "\n".join(out)


def uses(text, name):
    return re.search(r"\b%s\b" % re.escape(name), text) is not None


def top_items(body):
    names = set()
    for ln in body:
        m = ITEM_DEF.match(ln)
        if m:
            names.add(m.group(2))
    return names


module_items = {n: top_items(b) for n, b in moved.items()}

# root 段里的顶层项名（子模块可能引用 root 的私有项——通过 super:: 不行，
# 子模块本就能访问父模块私有项，无需导入；只有跨子模块才要 import）
root_item_names = set()
for s, e in SPEC["root_parts"]:
    root_item_names |= top_items(lines[s - 1:e])


def transform(name, body, spec):
    keep_pub = spec.get("keep_pub", set())
    method_vis = spec.get("method_vis", {})
    mode = spec.get("mode", "free")
    out = []
    for ln in body:
        if mode == "methods":
            m = re.match(r"^(\s*)fn ([A-Za-z_][A-Za-z0-9_]*)\(", ln)
            if m and m.group(2) in method_vis:
                vis = method_vis[m.group(2)]
                out.append(("%s%s %s" % (m.group(1), vis, ln.lstrip())) if vis else ln)
                continue
            out.append(ln)
            continue
        if ln.startswith("pub"):
            out.append(ln)
            continue
        m = re.match(r"^(?:const fn|fn|struct|enum|const|static|type)\s+([A-Za-z_][A-Za-z0-9_]*)", ln)
        nm = m.group(1) if m else None
        vis = "pub" if (nm and nm in keep_pub) else "pub(super)"
        if re.match(r"^const fn ", ln) or re.match(r"^fn ", ln) or re.match(r"^struct ", ln) \
                or re.match(r"^enum ", ln) or re.match(r"^const ", ln) \
                or re.match(r"^static ", ln) or re.match(r"^type ", ln):
            out.append(vis + " " + ln)
        else:
            out.append(ln)
    return out


written = []
for name, spec in SPEC["sections"].items():
    body = transform(name, moved[name], spec)
    if spec.get("mode") == "methods":
        body = ["impl super::%s {" % spec["impl_target"]] + body + ["}"]
    while body and body[0].strip() == "":
        body.pop(0)
    while body and body[-1].strip() == "":
        body.pop()
    text = code_text(body)
    imps = [stmt for nm, stmt in top_imports if uses(text, nm)]
    imps.sort()
    cross = []
    for other, items in module_items.items():
        if other == name:
            continue
        for it in sorted(items):
            if uses(text, it):
                cross.append((other, it))
    # root 私有项：子模块经 use super::X 引用
    supers = sorted(n for n in root_item_names if uses(text, n))
    header = [spec["doc"], ""]
    if imps:
        header.extend(imps)
        header.append("")
    if supers:
        header.append("use super::{%s};" % ", ".join(supers))
        header.append("")
    for other, it in cross:
        header.append("use super::%s::%s;" % (other, it))
    if cross:
        header.append("")
    out = "\n".join(header + body) + "\n"
    io.open("%s/%s.rs" % (SPEC["subdir"], name), "w", encoding="utf-8", newline="").write(out)
    written.append((name, len(body), len(imps), len(cross)))

# ---- root ----
insert_at = None
for i, ln in enumerate(root):
    if re.match(r"^use ", ln):
        insert_at = i + 1
assert insert_at is not None, "root use block not found"
block = [""]
for name in SPEC["sections"]:
    block.append("mod %s;" % name)
block.append("")
block.extend(SPEC["root_reexport"])
block.extend(SPEC["root_extra"])
if len(block) > 2:
    block.append("")
root[insert_at:insert_at] = block

# tests glob
if SPEC.get("tests_marker"):
    for i, ln in enumerate(root):
        if ln.strip() == SPEC["tests_marker"]:
            globs = ["    use super::{"]
            for name in SPEC["sections"]:
                globs.append("        %s::*," % name)
            globs.append("    };")
            root[i + 1:i + 1] = globs
            break

io.open(path, "w", encoding="utf-8", newline="").write("\n".join(root))
print("root: %d lines" % len(root))
for name, n, ni, nc in written:
    print("  %-14s %5d lines, imports=%d cross=%d" % (name, n, ni, nc))
