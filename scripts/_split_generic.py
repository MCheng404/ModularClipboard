# -*- coding: utf-8 -*-
"""通用 rs 拆分器（锚点驱动版）：foo.rs -> foo.rs(root) + foo/<name>.rs。

用法：改下面 SPEC 后运行。特性：
- anchors 模式：按顺序给出各节锚点（结构体/函数定义行），自动上吸文档注释；
  节的结束 = 下一节锚点起点；root_after_anchor 之后到 tests_anchor 留 root。
- vis_mode: "pub_super"（ui crate 内部模块，默认降为 pub(super)）
            | "keep"（公共 API 不降级，配 mod_vis=pub + 自动再导出）
- 按顶层 use 块 + 名称出现精确计算每个子文件的导入；
- 跨子模块 / root 私有项引用自动生成 use super::...;
- root 追加 mod 声明 / 再导出 / 自用导入；tests 补子模块 glob。
"""
import io
import os
import re
import sys

SPEC = {
    "path": "crates/modular-clipboard-app/src/lib.rs",
    "subdir": "crates/modular-clipboard-app/src",
    "vis_mode": "pub_super",
    "mod_vis": "",
    "anchors": {
        "ingest": "fn set_notice",
        "paths": "const PAGE_LIMIT",
    },
    # ingest 段跨两段（paths 段之后还有 preview 助手），用显式 extra ranges 表达：
    # 引擎按 anchors 顺序切节；这里借助 anchors 的顺序 + 一个附加节实现。
    "root_after_anchor": "mod tests {",
    "section_extra_ranges": {
        "ingest": [("pub const PREVIEW_MAX_CHARS", "mod tests {")],
    },
    "root_extra": [
        "use ingest::*;",
        "use paths::*;",
    ],
    "root_reexport": [
        "pub use ingest::format_time;",
        "pub use paths::{CONFIG_FILE_NAME, PREVIEW_MAX_CHARS, config_path_in};",
    ],
    "tests_anchor": "mod tests {",
    "docs": {
        "ingest": "//! 捕获入库管线：自写抑制、序列号闸门、预览生成、图片尺寸。\n"
                  "//!\n"
                  "//! `capture_should_read` 是「这次剪贴板变更要不要读」的唯一\n"
                  "//! 判据；`build_preview` 截断到 [`PREVIEW_MAX_CHARS`]。",
        "paths": "//! 数据目录与配置路径推导、分页与事件队列容量。\n"
                 "//!\n"
                 "//! 路径推导历史上三处并存导致过「读 A 写 B」，收敛到此处。",
    },
    "asserts": [],
}

path = SPEC["path"]
src = io.open(path, encoding="utf-8", newline="").read()
lines = src.split("\n")


def L(n):
    return lines[n - 1]


for n, needle in SPEC.get("asserts", []):
    assert needle in L(n), "assert failed at %d: %r not in %r" % (n, needle, L(n)[:60])


def find_anchor(needle):
    """找锚点行并上吸连续的文档注释与属性行。返回 0-based 索引。"""
    for i, ln in enumerate(lines):
        if needle in ln:
            while i > 0 and (lines[i - 1].lstrip().startswith("//")
                             or lines[i - 1].lstrip().startswith("#[")):
                i -= 1
            return i
    sys.exit("anchor not found: %r" % needle[:60])


os.makedirs(SPEC["subdir"], exist_ok=True)

names = list(SPEC["anchors"].keys())
starts = [find_anchor(SPEC["anchors"][n]) for n in names]
tests_at = find_anchor(SPEC["tests_anchor"])
if SPEC.get("root_after_anchor"):
    root_after = find_anchor(SPEC["root_after_anchor"])
else:
    root_after = tests_at

moved = {}
primary = {}
for i, name in enumerate(names):
    s = starts[i]
    e = starts[i + 1] if i + 1 < len(starts) else root_after
    primary[name] = (s, e)

# 附加区间：同一模块的第二段（如 ingest 的 preview 助手在 paths 之后）。
# 先解析所有附加区间，再把被其起点切开的主区间截断，避免内容重复。
extras = []
for name, ranges in SPEC.get("section_extra_ranges", {}).items():
    for start_anchor, end_anchor in ranges:
        s = find_anchor(start_anchor)
        e = find_anchor(end_anchor)
        extras.append((name, s, e))
for name, s, e in extras:
    for other in names:
        if other == name:
            continue
        os_, oe_ = primary[other]
        if os_ < s < oe_:
            primary[other] = (os_, s)

for name in names:
    s, e = primary[name]
    moved[name] = lines[s:e]
for name, s, e in extras:
    moved[name] = moved[name] + [""] + lines[s:e]

root = lines[:starts[0]] + lines[root_after:]


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


top_imports = parse_imports(lines[:150])

ITEM_DEF = re.compile(r"^(pub(?:\(crate\))?\s+)?(const fn|fn|struct|enum|const|static|type)\s+([A-Za-z_][A-Za-z0-9_]*)")


def code_text(body):
    """去掉注释行后的代码文本。

    CAUTION: 不剥字符串字面量——`#[serde(default = "fn_name")]` 的名字
    只在字符串里出现，剥了就找不到跨模块引用。
    """
    return "\n".join(ln for ln in body if not ln.strip().startswith("//"))


def uses(text, name):
    return re.search(r"\b%s\b" % re.escape(name), text) is not None


def top_items(body):
    alln, pubs = set(), set()
    for ln in body:
        m = ITEM_DEF.match(ln)
        if m:
            alln.add(m.group(3))
            if m.group(1):
                pubs.add(m.group(3))
    return alln, pubs


module_items, module_pubs = {}, {}
for n, b in moved.items():
    module_items[n], module_pubs[n] = top_items(b)
root_all, _ = top_items(lines[root_after:])
root_all |= top_items(lines[:starts[0]])[0]
root_all |= set(SPEC.get("root_extra_items", []))

KEEP_PUB = set()


def transform(name, body, needs_vis):
    promote_items = {it for kind, it in needs_vis.get(name, ()) if kind == "item"}
    promote_methods = {it for kind, it in needs_vis.get(name, ()) if kind == "method"}
    promote_structs = set(SPEC.get("pub_super_fields", {}).get(name, []))
    out = []
    in_trait_impl = False
    in_promo_struct = None
    for ln in body:
        # trait impl（impl X for Y）内的方法不许带可见性限定
        if re.match(r"^impl\b.*\bfor\b", ln):
            in_trait_impl = True
        elif re.match(r"^impl\b", ln):
            in_trait_impl = False
        elif ln.startswith("}") and in_trait_impl:
            in_trait_impl = False
        if in_promo_struct is not None:
            if ln.startswith("}"):
                in_promo_struct = None
            elif re.match(r"^    (?:pub(?:\(super\))?\s+)?[A-Za-z_][A-Za-z0-9_]*\s*:", ln) \
                    and not ln.lstrip().startswith("pub"):
                out.append("    pub(super) " + ln[4:])
                continue
        mstruct = re.match(r"^(?:pub(?:\(super\))?\s+)?struct ([A-Za-z_][A-Za-z0-9_]*)", ln)
        if mstruct and mstruct.group(1) in promote_structs:
            in_promo_struct = mstruct.group(1)
        if SPEC.get("vis_mode") == "keep":
            m = ITEM_DEF.match(ln)
            if m and not m.group(1) and m.group(3) in promote_items:
                out.append("pub(super) " + ln)
                continue
            mm = re.match(r"^(\s*)(?:const\s+)?fn ([A-Za-z_][A-Za-z0-9_]*)\s*\(", ln)
            if (mm and not in_trait_impl and mm.group(2) in promote_methods
                    and not ln.lstrip().startswith("pub")):
                out.append(mm.group(1) + "pub(super) " + ln.lstrip())
                continue
            out.append(ln)
            continue
        if ln.startswith("pub"):
            out.append(ln)
            continue
        m = re.match(r"^(const fn|fn|struct|enum|const|static|type)\s+([A-Za-z_][A-Za-z0-9_]*)", ln)
        nm = m.group(2) if m else None
        vis = "pub" if (nm and nm in KEEP_PUB) else "pub(super)"
        if m:
            out.append(vis + " " + ln)
        else:
            out.append(ln)
    return out


# ---- 第一遍：计算跨模块引用，得出各模块需要提权的项 ----
# needs_vis[name] = {(kind, item)}，kind: "item" | "method"
ASSOC_CALL = re.compile(r"\b([A-Z][A-Za-z0-9_]*)::([a-z_][A-Za-z0-9_]*)\s*\(")
type_owner = {}
for n in names:
    for it in module_items[n]:
        type_owner[it] = n

needs_vis = {n: set() for n in names}
for name in names:
    text = code_text(moved[name])
    for other in names:
        if other == name:
            continue
        for it in sorted(module_items[other]):
            if uses(text, it) and it not in module_pubs[other]:
                needs_vis[other].add(("item", it))
    # 关联方法调用：Type::method( ，Type 属于别的模块
    for ty, meth in ASSOC_CALL.findall(text):
        owner = type_owner.get(ty)
        if owner and owner != name:
            needs_vis[owner].add(("method", meth))
# root 的 tests 也按同样规则提权（测试是 root 的后代，pub(super) 可见）
tests_text = code_text(lines[tests_at:])
for other in names:
    for it in sorted(module_items[other]):
        if it not in module_pubs[other] and uses(tests_text, it):
            needs_vis[other].add(("item", it))
    for meth in SPEC.get("extra_promote_methods", {}).get(other, []):
        needs_vis[other].add(("method", meth))

written = []
reexports = []
for name in names:
    body = transform(name, moved[name], needs_vis)
    while body and body[0].strip() == "":
        body.pop(0)
    while body and body[-1].strip() == "":
        body.pop()
    text = code_text(body)
    imps = sorted(stmt for nm, stmt in top_imports if uses(text, nm))
    supers = sorted(n for n in root_all if uses(text, n) and n not in module_items[name])
    cross = []
    for other in names:
        if other == name:
            continue
        for it in sorted(module_items[other]):
            if uses(text, it):
                cross.append((other, it))
    header = [SPEC["docs"][name], ""]
    imps.extend(SPEC.get("force_imports", {}).get(name, []))
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
    if SPEC.get("vis_mode") == "keep":
        for it in sorted(module_pubs[name]):
            reexports.append("pub use %s::%s;" % (name, it))

insert_at = None
for i, ln in enumerate(root):
    if re.match(r"^use ", ln):
        insert_at = i + 1
assert insert_at is not None, "root use block not found"
mod_vis = (SPEC.get("mod_vis", "") + " ") if SPEC.get("mod_vis") else ""
block = [""]
for name in names:
    block.append("%smod %s;" % (mod_vis, name))
block.append("")
block.extend(reexports)
block.extend(SPEC.get("root_reexport", []))
block.extend(SPEC.get("root_extra", []))
if len(block) > 2:
    block.append("")
root[insert_at:insert_at] = block

if SPEC.get("tests_anchor"):
    for i, ln in enumerate(root):
        if ln.strip() == SPEC["tests_anchor"]:
            globs = ["    use super::{"]
            for name in names:
                globs.append("        %s::*," % name)
            globs.append("    };")
            root[i + 1:i + 1] = globs
            break

io.open(path, "w", encoding="utf-8", newline="").write("\n".join(root))
print("root: %d lines, reexports: %d" % (len(root), len(reexports)))
for name, n, ni, nc in written:
    print("  %-14s %5d lines, imports=%d cross=%d" % (name, n, ni, nc))
