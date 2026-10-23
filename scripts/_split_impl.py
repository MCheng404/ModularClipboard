# -*- coding: utf-8 -*-
r"""impl 方法级拆分器：把 `impl Type { ... }` 内的方法按模块分组搬到
foo/<name>.rs，每个子文件自带一个 `impl Type { ... }` 块。

与 _split_generic.py 的区别：那边切的是顶层条目（struct/fn/enum），
这边切的是**同一个 impl 块里的方法**——Rust 允许同一个类型的多个
`impl` 块散落在多个文件里，因此「删一个功能 = 删一个文件 + 一行 mod」
依然成立。

判定口径（踩过的坑）：
- 跨模块**方法**不生成 `use`（方法经 `self.` / `Self::` 调用，只需要
  可见性 pub(crate)）；只有自由函数才生成 `use super::mod::fn;`。
- 词边界匹配会把 `std::path::Path` 的 `path`、SQL 注释里的 `` `list()` ``
  当成引用。方法按 `\.name\b|Self::name\b|Type::name\b` 判定，
  自由函数按 `\bname\s*\(` 判定。

用法：改下面 SPEC 后运行。
"""
import io
import os
import re
import sys

SPEC = {
    "path": "crates/modular-clipboard-store/src/lib.rs",
    "subdir": "crates/modular-clipboard-store/src",
    "type_name": "Store",
    "impl_anchor": "impl Store {",
    "tests_anchor": "mod tests {",
    # 各子模块的方法名，按**源文件出现顺序**列出（生成时也按此顺序）。
    "modules": {
        "open": ["open_default", "open_in", "data_dir", "open_in_memory", "open", "migrate", "path"],
        "query": [
            "find_by_hash", "row_to_item", "SELECT_COLS", "get", "list", "search",
            "like_search", "escape_like", "load_payload", "stats",
        ],
        "write": [
            "insert", "insert_with_dims", "touch", "bump_to_top", "set_pinned",
            "move_to_group", "delete", "update_content", "delete_many",
        ],
        "retention": ["clear_all", "enforce_retention", "gc_orphan_blobs"],
        "groups": ["list_groups", "create_group", "rename_group", "delete_group"],
    },
    # 留在 root 的方法（不搬）。
    "keep_methods": ["current_payload_limit", "set_payload_limit", "apply_config"],
    # 顶层自由函数搬家：{模块名: [函数名]}。它们不进 impl 块。
    "free_fns": {
        "blobs": ["safe_blob_path", "write_blob_atomic", "compute_dims"],
    },
    # 需要提权为 pub(crate) 的名字（跨模块方法 + 被 root tests 调用的方法/函数）。
    # 引擎还会按调用点自动补提权，这里只是显式兜底。
    "promote": [
        "like_search", "escape_like", "delete_many",
        "safe_blob_path", "write_blob_atomic", "compute_dims",
    ],
    # trait 导入必须保留：它们只以 `.method()` 形式出现，名字从不出现在代码里，
    # 按「名字是否出现」裁剪会把 trait 一并删掉，然后 `.optional()` 就找不到方法。
    # root 的 tests 里需要显式补的导入（不放 root 顶层，避免 lib 目标误报未用）。
    "tests_imports": [
        "use super::blobs::safe_blob_path;",
    ],
    "docs": {
        "open": "//! 库的打开、建表迁移与位置访问。\n"
                "//!\n"
                "//! `open_in_memory` 的载荷目录必须每次独占（带 pid + 原子序号 +\n"
                "//! 随机分量，SEC-005），否则并行测试会互相删对方的载荷文件。",
        "query": "//! 条目查询：分页列表、全文搜索与 LIKE 回退、行反序列化。\n"
                 "//!\n"
                 "//! `SELECT_COLS` 与 `row_to_item` 是读路径的单一来源，\n"
                 "//! 列序改了必须两处同步（它们按**列下标**取值）。",
        "write": "//! 条目写入与变更：插入、就地更新、置顶、删除。\n"
                 "//!\n"
                 "//! 同哈希条目共享同一载荷文件（`{hash}.bin`），删除时必须按\n"
                 "//! 引用计数决定是否删文件（DF-01）；contentless FTS5 不支持\n"
                 "//! SQL DELETE，改索引一律「先 'delete' 命令 + 原文，再插入」。",
        "retention": "//! 保留策略与容量维护：数量/字节双上限淘汰、全量清空、孤儿回收。\n"
                     "//!\n"
                     "//! 淘汰走 `delete_many` 单事务（PF-7）；置顶条目永不淘汰。",
        "groups": "//! 分组定义的增删改查。\n"
                  "//!\n"
                  "//! 组内条目不随分组删除：外键 `ON DELETE SET NULL` 把\n"
                  "//! `group_id` 置空，条目本身留在历史里。",
        "blobs": "//! 载荷文件层：路径规范化、原子写、图片尺寸嗅探。\n"
                 "//!\n"
                 "//! `safe_blob_path` 是 SEC-006 的闸门：DB 行内容不可信，\n"
                 "//! 含目录分量的 `blob_file` 会把读/删历史变成任意文件\n"
                 "//! 原语。正常文件名恒为 `{hash}.bin`。",
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

METHOD_RE_TMPL = r"^(    )(?:pub(?:\(crate\))?\s+)?(?:const\s+)?fn %s\s*(?:\(|<)"
CONST_RE_TMPL = r"^(    )(?:pub(?:\(crate\))?\s+)?const %s\s*:"
FREE_FN_RE_TMPL = r"^(?:pub(?:\(crate\))?\s+)?(?:const\s+)?fn %s\s*(?:\(|<)"

ITEM_DEF = re.compile(
    r"^(pub(?:\(crate\))?\s+)?(const fn|fn|struct|enum|const|static|type)\s+([A-Za-z_][A-Za-z0-9_]*)"
)


def code_text(body):
    """去掉注释行后的代码文本。"""
    return "\n".join(ln for ln in body if not ln.strip().startswith("//"))


def method_used(text, name):
    r"""方法/关联常量的调用点判定。

    CAUTION: 不要用 `\.name|::name` —— 会把 `r.get(0)`（rusqlite 的
    `Row::get`）、`Connection::open` 也算成 `Store` 的方法，导致几乎所有
    方法都被误提权。只认 `self.` / `Self::` / `Store::` 三种真实调用点；
    其余跨模块调用（如 `store.migrate()`）在 `SPEC["promote"]` 里显式列出。
    """
    n = re.escape(name)
    return re.search(r"self\.%s\b|Self::%s\b|Store::%s\b" % (n, n, n), text) is not None


def free_used(text, name):
    """自由函数/常量/类型的引用判定：调用 `name(` 或路径 `::name`。"""
    n = re.escape(name)
    return re.search(r"\b%s\s*\(|::%s\b" % (n, n), text) is not None


def bare_used(text, name):
    """宽松判定（仅用于 root 自有项生成 use super::{...}）。"""
    return re.search(r"\b%s\b" % re.escape(name), text) is not None


# trait 名 -> 它提供、我们真正调用的方法。trait 只以 `.method()` 形式出现，
# 名字本身从不写在代码里，所以「名字是否出现」判不出它有没有被用到。
TRAIT_METHODS = {
    "OptionalExtension": ["optional"],
}


def import_name_used(text, name):
    """顶层 use 成员的用量判定。

    CAUTION 两处误判：
    1. `::Name` 形式——`GroupFilter::Group(gid)` 的 `Group` 是枚举变体，
       不是 `modular_clipboard_core::Group`；路径全限定（`std::path::Path`）
       本身就无需导入。
    2. **枚举变体定义行**——`    Group(i64),` 顶格大写名字 + 圆括号/逗号，
       词边界同样会命中。只在 4 空格缩进（枚举体）下剥离，不影响
       `            Some(f) => ...` 这类更深层缩进的调用。
    """
    if name in TRAIT_METHODS:
        return any(re.search(r"\.%s\s*\(" % re.escape(m), text) for m in TRAIT_METHODS[name])
    stripped = "\n".join(
        ln for ln in text.split("\n")
        if not re.match(r"^ {4}[A-Z][A-Za-z0-9_]*\s*(?:\(|,)", ln)
    )
    return re.search(r"(?<!::)\b%s\b" % re.escape(name), stripped) is not None


def body_names(body):
    s = set()
    for ln in body:
        m = re.match(r"^\s*(?:pub(?:\(crate\))?\s+)?(?:const\s+)?(?:fn|const)\s+([A-Za-z_][A-Za-z0-9_]*)", ln)
        if m:
            s.add(m.group(1))
    return s


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


def find_line(pred, what):
    for i, ln in enumerate(lines):
        if pred(i, ln):
            return i
    sys.exit("not found: %s" % what)


def method_index(name):
    mre = re.compile(METHOD_RE_TMPL % re.escape(name))
    cre = re.compile(CONST_RE_TMPL % re.escape(name))
    for i, ln in enumerate(lines):
        if mre.match(ln) or cre.match(ln):
            return i
    sys.exit("method not found: %r" % name)


def free_fn_index(name):
    fre = re.compile(FREE_FN_RE_TMPL % re.escape(name))
    for i, ln in enumerate(lines):
        if fre.match(ln):
            return i
    sys.exit("free fn not found: %r" % name)


def absorb_docs(i):
    while i > 0 and (lines[i - 1].lstrip().startswith("///")
                     or lines[i - 1].lstrip().startswith("#[")):
        i -= 1
    return i


# ---- 定位 impl 块 ----
impl_at = find_line(lambda i, ln: ln.rstrip() == SPEC["impl_anchor"], SPEC["impl_anchor"])
impl_end = None
for i in range(impl_at + 1, len(lines)):
    if lines[i] == "}":
        impl_end = i
        break
assert impl_end is not None, "impl closing brace not found"
top_imports = parse_imports(lines[:impl_at])

# ---- 切方法段 ----
all_method_names = []
for names in SPEC["modules"].values():
    all_method_names.extend(names)
all_method_names.extend(SPEC["keep_methods"])

seg = {}
starts = []
for name in all_method_names:
    idx = absorb_docs(method_index(name))
    starts.append((idx, name))
starts.sort()
for k, (s, name) in enumerate(starts):
    e = starts[k + 1][0] if k + 1 < len(starts) else impl_end
    seg[name] = (s, e)

# ---- 切自由函数段 ----
# CAUTION: 段的结束必须从**函数定义行**起做花括号配对。若从文档起点往下
# 找「下一个顶层条目」，会把函数自己的定义行当成下一节，只剩一截文档。
free_seg = {}
for names in SPEC["free_fns"].values():
    for name in names:
        fi = free_fn_index(name)
        s = absorb_docs(fi)
        depth = 0
        e = None
        for i in range(fi, len(lines)):
            depth += lines[i].count("{") - lines[i].count("}")
            if depth == 0 and "{" in "".join(lines[fi:i + 1]):
                e = i + 1
                break
        assert e is not None, "free fn body end not found: %r" % name
        free_seg[name] = (s, e)

moved_lines = set()
for name, (s, e) in seg.items():
    if name in SPEC["keep_methods"]:
        continue
    for i in range(s, e):
        moved_lines.add(i)
for name, (s, e) in free_seg.items():
    for i in range(s, e):
        moved_lines.add(i)

root_all = set()
for i, ln in enumerate(lines):
    if i in moved_lines:
        continue
    m = ITEM_DEF.match(ln)
    if m:
        root_all.add(m.group(3))

# ---- 各模块内容（按 SPEC 顺序拼接，段间空行） ----
module_body = {}
for mod in list(SPEC["modules"]) + list(SPEC["free_fns"]):
    parts = []
    for name in SPEC["modules"].get(mod, []) + SPEC["free_fns"].get(mod, []):
        s, e = (seg[name] if name in seg else free_seg[name])
        parts.append(lines[s:e])
    body = []
    for k, chunk in enumerate(parts):
        if k:
            body.append("")
        body.extend(chunk)
    module_body[mod] = body

# ---- 自动提权：跨模块调用的方法/函数统一 pub(crate) ----
owner = {}
for mod, names in SPEC["modules"].items():
    for n in names:
        owner[n] = (mod, "method")
for mod, names in SPEC["free_fns"].items():
    for n in names:
        owner[n] = (mod, "free")

promote_set = set(SPEC["promote"])
for mod, body in module_body.items():
    text = code_text(body)
    names_in = body_names(body)
    for n, (omod, kind) in owner.items():
        if omod == mod or n in names_in:
            continue
        hit = method_used(text, n) if kind == "method" else free_used(text, n)
        if hit:
            promote_set.add(n)

# root tests 的调用点也要求可见
tests_at = find_line(lambda i, ln: ln.strip() == SPEC["tests_anchor"], SPEC["tests_anchor"])
tests_text = code_text(lines[tests_at:])
for n, (omod, kind) in owner.items():
    hit = method_used(tests_text, n) if kind == "method" else free_used(tests_text, n)
    if hit:
        promote_set.add(n)


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
for mod in list(SPEC["modules"]) + list(SPEC["free_fns"]):
    body = promote_vis(module_body[mod], promote_set)
    text = code_text(body)
    names_in = body_names(body)
    imps = sorted(stmt for nm, stmt in top_imports if import_name_used(text, nm))
    supers = sorted(n for n in root_all if bare_used(text, n) and n not in names_in)
    # 跨模块只给**自由函数**生成 use；方法靠 pub(crate) + `self.`/`Self::` 调用。
    cross = []
    for other, names in SPEC["free_fns"].items():
        if other == mod:
            continue
        for it in names:
            if free_used(text, it) and it not in names_in:
                cross.append(it)
    header = [SPEC["docs"][mod], ""]
    if imps:
        header.extend(imps)
        header.append("")
    if mod in SPEC["modules"]:
        header.append("use super::%s;" % SPEC["type_name"])
    if supers:
        header.append("use super::{%s};" % ", ".join(supers))
    if mod in SPEC["modules"] or supers:
        header.append("")
    if cross:
        header.append("use super::blobs::{%s};" % ", ".join(sorted(set(cross))))
        header.append("")
    if mod in SPEC["modules"]:
        inner = ["impl %s {" % SPEC["type_name"]] + body + ["}"]
    else:
        inner = body
    out = "\n".join(header + inner) + "\n"
    io.open("%s/%s.rs" % (SPEC["subdir"], mod), "w", encoding="utf-8", newline="").write(out)
    written.append((mod, len(inner)))

# ---- 重建 root ----
root = []
for i, ln in enumerate(lines):
    if i in moved_lines:
        continue
    root.append(ln)

# root 顶层导入按**非测试**代码裁剪：`cargo build` 不编译 `mod tests`，
# 只被测试用到的导入留在顶层会被报 unused。测试专用导入挪进 `mod tests`。
i = 0
use_stmt_ranges = []
while i < len(root):
    if re.match(r"^use\s", root[i]):
        j = i
        while not root[j].rstrip().endswith(";"):
            j += 1
        use_stmt_ranges.append((i, j + 1))
        i = j + 1
    else:
        i += 1

tests_at_root = None
for i, ln in enumerate(root):
    if ln.strip() == SPEC["tests_anchor"]:
        tests_at_root = i
        break

kept_use_lines = []
tests_extra = []
if use_stmt_ranges:
    u0, u1 = use_stmt_ranges[0][0], use_stmt_ranges[-1][1]
    t_end = tests_at_root if tests_at_root is not None else len(root)
    body_wo_use = root[:u0] + root[u1:t_end]
    code_txt = code_text(body_wo_use)
    tests_txt = code_text(root[tests_at_root:]) if tests_at_root is not None else ""
    for a, b in use_stmt_ranges:
        stmt = "\n".join(root[a:b])
        m = re.match(r"^use\s+([\w:]+)::\{([^}]*)\};\s*$", stmt, re.S)
        if m:
            base, group = m.group(1), m.group(2)
            parts, tparts = [], []
            for part in group.split(","):
                part = part.strip()
                if not part:
                    continue
                nm = part.split(" as ")[-1].strip()
                if import_name_used(code_txt, nm):
                    parts.append(part)
                elif import_name_used(tests_txt, nm):
                    tparts.append(part)
            if parts:
                kept_use_lines.append("use %s::{%s};" % (base, ", ".join(parts)))
            if tparts:
                tests_extra.append("use %s::{%s};" % (base, ", ".join(tparts)))
        else:
            m2 = re.match(r"^use\s+([\w:]+);\s*$", stmt, re.S)
            if m2:
                base = m2.group(1)
                nm = base.split("::")[-1]
                if import_name_used(code_txt, nm):
                    kept_use_lines.append("use %s;" % base)
                elif import_name_used(tests_txt, nm):
                    tests_extra.append("use %s;" % base)
            else:
                kept_use_lines.extend(root[a:b])
    del root[u0:u1]
    insert_at = u0
    root[insert_at:insert_at] = kept_use_lines
else:
    insert_at = None

# CAUTION: `use` 语句可能跨行，mod 声明要插在整条 use 块之后。
i = 0
while i < len(root):
    if re.match(r"^use\s", root[i]):
        while not root[i].rstrip().endswith(";"):
            i += 1
        insert_at = i + 1
    i += 1
assert insert_at is not None, "root use block not found"

block = [""]
for mod in list(SPEC["modules"]) + list(SPEC["free_fns"]):
    block.append("mod %s;" % mod)
block.append("")
root[insert_at:insert_at] = block

for i, ln in enumerate(root):
    if ln.strip() == SPEC["tests_anchor"]:
        root[i + 1:i + 1] = ["    " + s for s in SPEC["tests_imports"] + tests_extra]
        break

io.open(path, "w", encoding="utf-8", newline="").write("\n".join(root))
print("root: %d lines" % len(root))
for mod, n in written:
    print("  %-12s %5d lines" % (mod, n))
print("promoted:", ", ".join(sorted(promote_set)))
