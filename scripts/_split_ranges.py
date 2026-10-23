# -*- coding: utf-8 -*-
r"""按**显式行区间**拆分 rs：把若干连续段搬进 foo/<name>.rs，
root 保留其余部分，`pub` 项在 root 再导出以保持外部 API 不变。

与 _split_generic.py（锚点驱动）/ _split_impl.py（impl 方法）互补：
当文件里各特性是**交错**的（常量区混着多个特性的常量），
显式行区间比锚点更可控。

用法：改 SPEC["modules"] 的 1-based 闭区间后运行。
"""
import io
import os
import re
import sys

SPEC = {
    "path": "crates/modular-clipboard-ui/src/icons.rs",
    "subdir": "crates/modular-clipboard-ui/src/icons",
    # 1-based 闭区间，按源文件顺序
    "modules": {
        "font": [(63, 120), (429, 451)],
        "draw": [(334, 427), (453, 468)],
        "md": [(470, 573)],
    },
    "promote": [],
    "docs": {
        "font": "//! 图标字体数据与度量基线。\n"
               "//!\n"
               "//! 字体是离线烘焙的 TTF（`scripts/make_icons.py` 产出），\n"
               "//! 度量常量与 TTF 头里的 ascender/descender 必须一致。",
        "draw": "//! 图标的矢量绘制：线段/圆角笔触与内联排布。\n"
               "//!\n"
               "//! `vector_icon` 是各 `Icon` 变体的实际画法，\n"
               "//! `paint_inline` 负责在文本行里对齐。",
        "md": "//! 列表行的类型图标（文本/图片/文件等）。\n"
              "//!\n"
              "//! `md_icon_geom` 给出统一几何，`paint_kind_icon` 按\n"
              "//! `ClipKind` 选形状。",
    },
}

path = SPEC["path"]
lines = io.open(path, encoding="utf-8", newline="").read().split("\n")

# ---- 圈定被搬走的行 ----
moved = set()
for ranges in SPEC["modules"].values():
    for a, b in ranges:
        assert 1 <= a <= b <= len(lines), "range out of bounds: %r" % ((a, b),)
        for i in range(a - 1, b):
            moved.add(i)

module_body = {}
for name, ranges in SPEC["modules"].items():
    parts = []
    for a, b in ranges:
        chunk = lines[a - 1:b]
        while chunk and chunk[0].strip() == "":
            chunk.pop(0)
        while chunk and chunk[-1].strip() == "":
            chunk.pop()
        parts.append(chunk)
    body = []
    for k, chunk in enumerate(parts):
        if k:
            body.append("")
        body.extend(chunk)
    module_body[name] = body

root = [ln for i, ln in enumerate(lines) if i not in moved]


def parse_imports(body_lines):
    out = []
    i, n = 0, len(body_lines)
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
                if not part:
                    continue
                if " as " in part:
                    orig, alias = part.split(" as ")
                    out.append((alias.strip(), "use %s::%s as %s;" % (base, orig.strip(), alias.strip())))
                else:
                    out.append((part, "use %s::%s;" % (base, part)))
        else:
            m2 = re.match(r"^use\s+([\w:]+);\s*$", stmt, re.S)
            if m2:
                base = m2.group(1)
                out.append((base.split("::")[-1], "use %s;" % base))
        i += 1
    return out


ITEM_DEF = re.compile(
    r"^(pub(?:\(crate\))?\s+)?(const fn|fn|struct|enum|const|static|type)\s+([A-Za-z_][A-Za-z0-9_]*)"
)

# 顶层 use 块在文件开头（impl 之前）
# CAUTION: `use` 语句常跨多行（`use foo::{\n a, b,\n};`）。只取文件前 N 行
# 会在大括号中间截断，导致后面一半导入名字解析不出来、被误裁。
# 这里扫描整个 root 的 use 块（连续的 use 语句，允许中间空行）。
_use_lines = []
_i = 0
while _i < len(root):
    if re.match(r"^use\s", root[_i]):
        _j = _i
        while not root[_j].rstrip().endswith(";"):
            _j += 1
        _use_lines.extend(root[_i:_j + 1])
        _i = _j + 1
    else:
        _i += 1
top_imports = parse_imports(_use_lines)


def code_text(body):
    return "\n".join(ln for ln in body if not ln.strip().startswith("//"))


def import_name_used(text, name):
    """用量判定：排除 `::Name` 与 4 空格缩进的枚举变体定义行。"""
    stripped = "\n".join(
        ln for ln in text.split("\n")
        if not re.match(r"^ {4}[A-Z][A-Za-z0-9_]*\s*(?:\(|,)", ln)
    )
    return re.search(r"(?<!::)\b%s\b" % re.escape(name), stripped) is not None


def bare_used(text, name):
    return re.search(r"\b%s\b" % re.escape(name), text) is not None


def item_names(body):
    names, pubs = set(), set()
    for ln in body:
        m = ITEM_DEF.match(ln)
        if m:
            names.add(m.group(3))
            if m.group(1):
                pubs.add(m.group(3))
    return names, pubs


module_items, module_pubs = {}, {}
for n, b in module_body.items():
    module_items[n], module_pubs[n] = item_names(b)

root_all = set()
for ln in root:
    m = ITEM_DEF.match(ln)
    if m:
        root_all.add(m.group(3))

# root 将再导出的 pub 项，供 use super::{...} 解析
reexport_names = set()
for n in module_pubs.values():
    reexport_names |= n
root_all |= reexport_names

# root 引用到的**模块私有**项（如 match 模式里用的 WM_* 常量）：
# 必须提权为 pub(crate)，并在 root 侧 `use <mod>::<item>;`——否则
# 「cannot find value」或 match 里被当成新绑定（E0408）。
root_code = code_text(root)
root_private_used = {}
for name in SPEC["modules"]:
    for it in sorted(module_items[name]):
        if it in module_pubs[name]:
            continue
        if bare_used(root_code, it):
            root_private_used.setdefault(name, []).append(it)

promote_set = set(SPEC.get("promote", []))
for names in root_private_used.values():
    promote_set |= set(names)

def promote_vis(body, names):
    out = []
    for ln in body:
        m = re.match(r"^(\s*)(?:const\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)", ln)
        if m and m.group(2) in names:
            indent, rest = m.group(1), ln.lstrip()
            if not rest.startswith("pub"):
                out.append(indent + "pub(crate) " + rest)
                continue
        m2 = re.match(r"^(\s*)const\s+([A-Za-z_][A-Za-z0-9_]*)\s*:", ln)
        if m2 and m2.group(2) in names and not ln.lstrip().startswith("pub"):
            out.append(m2.group(1) + "pub(crate) " + ln.lstrip())
            continue
        out.append(ln)
    return out


os.makedirs(SPEC["subdir"], exist_ok=True)
written = []
for name in SPEC["modules"]:
    body = promote_vis(module_body[name], promote_set)
    text = code_text(body)
    names_in = module_items[name]
    imps = sorted(stmt for nm, stmt in top_imports if import_name_used(text, nm))
    supers = sorted(n for n in root_all if bare_used(text, n) and n not in names_in)
    header = [SPEC["docs"][name], ""]
    if imps:
        header.extend(imps)
        header.append("")
    if supers:
        header.append("use super::{%s};" % ", ".join(supers))
        header.append("")
    out = "\n".join(header + body) + "\n"
    io.open("%s/%s.rs" % (SPEC["subdir"], name), "w", encoding="utf-8", newline="").write(out)
    written.append((name, len(body)))

# ---- root 注入 mod / 再导出 ----
i = 0
insert_at = None
while i < len(root):
    if re.match(r"^use\s", root[i]):
        while not root[i].rstrip().endswith(";"):
            i += 1
        insert_at = i + 1
    i += 1
assert insert_at is not None, "root use block not found"

block = [""]
for name in SPEC["modules"]:
    block.append("mod %s;" % name)
block.append("")
# root 侧要用到的模块私有项（match 模式里的 WM_* 常量等）。
for name in SPEC["modules"]:
    for it in root_private_used.get(name, []):
        block.append("use %s::%s;" % (name, it))
for name in SPEC["modules"]:
    for it in sorted(module_pubs[name]):
        block.append("pub use %s::%s;" % (name, it))
block.append("")
root[insert_at:insert_at] = block

io.open(path, "w", encoding="utf-8", newline="").write("\n".join(root))
print("root: %d lines" % len(root))
for name, n in written:
    print("  %-10s %5d lines" % (name, n))
print("reexports:", ", ".join(sorted(reexport_names)))
