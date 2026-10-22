# -*- coding: utf-8 -*-
"""把 ui/src/app.rs 拆成 app/ 模块家族。

分区（行界已人工核验，带断言；段可以是多个不相邻区间）：
- boot.rs        启动辅助 + 四个入口 + is_sane_window_pos
- frame_loop.rs  run_mode + drive_attach_windows + pick_surface_format + rebuild
- recording.rs   热键录制相关方法（包进 impl super::App）
- config_write.rs 四个 set_*_field 配置写入函数
- resize.rs      WidthRequest + resize_width_target + resize_tests
- app.rs (root)  App 结构体 + impl App 其余方法 + mod tests
"""
import io
import os
import re
import sys

os.makedirs("crates/modular-clipboard-ui/src/app", exist_ok=True)

P = "crates/modular-clipboard-ui/src/app.rs"
src = io.open(P, encoding="utf-8", newline="").read()
lines = src.split("\n")


def L(n):
    return lines[n - 1]


def check(cond, msg):
    if not cond:
        sys.exit("boundary check failed: " + msg)


check("fn backdrop_alpha" in L(57), "backdrop_alpha fn at 57")
check("pub fn run(" in L(127), "run fn at 127")
check("fn run_mode(" in L(198), "run_mode at 198")
check("pub(crate) fn pick_surface_format" in L(1721), "pick_surface_format at 1721")
check("fn rebuild" in L(1743), "rebuild at 1743")
check("fn is_modifier_key" in L(2175), "is_modifier_key at 2175")
check("fn capture_recording" in L(2197), "capture_recording at 2197")
check("fn apply_ops" in L(2340), "apply_ops at 2340")
check("fn set_bool_field" in L(3461), "set_bool_field at 3461")
check("pub enum WidthRequest" in L(3561), "WidthRequest at 3561")
check("mod resize_tests" in L(3624), "resize_tests at 3624")
check("mod tests" in L(3734), "tests at 3734")
check("pub struct App" in L(1797), "App struct at 1797")
check("use super::*;" in L(3849), "tests use super::* at 3849")

# name -> [ (start, end) inclusive 1-indexed, ... ] 按文件顺序
SECTIONS = {
    "boot": [(41, 124), (126, 192), (1766, 1790)],
    "frame_loop": [(193, 1760)],
    "recording": [(2167, 2327)],
    "config_write": [(3451, 3550)],
    "resize": [(3552, 3732)],
}
ROOT_PARTS = [(1, 40), (1761, 1765), (1791, 2166), (2328, 3450), (3733, len(lines))]

moved = {}
for name, ranges in SECTIONS.items():
    parts = [lines[s - 1:e] for s, e in ranges]
    body = []
    for i, part in enumerate(parts):
        if i:
            body.append("")
        body.extend(part)
    moved[name] = body

root = []
for s, e in ROOT_PARTS:
    root.extend(lines[s - 1:e])

# ---------------------------------------------------------------- 导入计算
IMPORTS = [
    ("WindowPos", "use modular_clipboard_core::WindowPos;"),
    ("Service", "use modular_clipboard_app::Service;"),
    ("FrameRenderer", "use modular_clipboard_gfx::frame::FrameRenderer;"),
    ("PipelineBundle", "use modular_clipboard_gfx::frame::PipelineBundle;"),
    ("PresentResult", "use modular_clipboard_gfx::frame::PresentResult;"),
    ("EventLoop", "use modular_clipboard_gfx::window::EventLoop;"),
    ("Window", "use modular_clipboard_gfx::window::Window;"),
    ("WindowEvent", "use modular_clipboard_gfx::window::WindowEvent;"),
    ("Gpu", "use modular_clipboard_gfx::Gpu;"),
    ("IsWindowVisible", "use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;"),
    ("CloseDecision", "use crate::presence::CloseDecision;"),
    ("Resident", "use crate::presence::Resident;"),
    ("Painter", "use crate::renderer::Painter;"),
    ("WINDOW_TITLE", "use crate::renderer::WINDOW_TITLE;"),
    ("theme", "use crate::theme;"),
]
PRESENCE_CALL = re.compile(r"\bpresence::")
ROOT_SUPER = ["App"]

ITEM_DEF = re.compile(r"^(?:pub(?:\(crate\))?\s+)?(?:pub\(super\)\s+)?(const fn|fn|struct|enum|const|static)\s+([A-Za-z_][A-Za-z0-9_]*)")


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

# ---------------------------------------------------------------- 可见性变换
KEEP_PUB = {"run", "run_with_capture_override", "run_with_options", "run_remote",
            "pick_surface_format", "default_window_width_px"}
METHOD_VIS = {
    "is_modifier_key": None,
    "capture_recording": "pub(super)",
    "apply_hotkey": "pub(super)",
    "hotkey_recording_open": "pub(crate)",
    "set_hotkeys_suspended": "pub(super)",
    "surface_hotkey_error": "pub(super)",
}


def transform(name, body):
    out = []
    for ln in body:
        if ln.startswith("pub"):
            out.append(ln)
            continue
        if name == "recording":
            m = re.match(r"^fn ([A-Za-z_][A-Za-z0-9_]*)\(", ln)
            if m and m.group(1) in METHOD_VIS:
                vis = METHOD_VIS[m.group(1)]
                out.append(("%s %s" % (vis, ln)) if vis else ln)
                continue
            out.append(ln)
            continue
        if re.match(r"^const fn ", ln):
            out.append("pub(super) " + ln)
        elif re.match(r"^const ", ln):
            out.append("pub(super) " + ln)
        elif re.match(r"^static ", ln):
            out.append("pub(super) " + ln)
        elif re.match(r"^fn ", ln):
            m = re.match(r"^fn ([A-Za-z_][A-Za-z0-9_]*)\(", ln)
            out.append("pub " + ln if (m and m.group(1) in KEEP_PUB) else "pub(super) " + ln)
        elif re.match(r"^struct ", ln):
            out.append("pub(super) " + ln)
        elif re.match(r"^enum ", ln):
            out.append("pub(super) " + ln)
        else:
            out.append(ln)
    return out


def build_imports(name, body):
    text = code_text(body)
    imps = [stmt for nm, stmt in IMPORTS if uses(text, nm)]
    if PRESENCE_CALL.search(text):
        imps.append("use crate::presence;")
    imps.sort(key=lambda s: (0 if s.startswith("use egui") else
                             (1 if s.startswith("use modular_clipboard") else
                              (2 if s.startswith("use windows") else 3)), s))
    supers = [n for n in ROOT_SUPER if uses(text, n)]
    cross = []
    for other, items in module_items.items():
        if other == name:
            continue
        for it in sorted(items):
            if uses(text, it):
                cross.append((other, it))
    return imps, supers, cross


DOCS = {
    "boot": "//! 启动：入口函数与启动期辅助。\n"
            "//!\n"
            "//! 四个 `run*` 是 crate 对外的界面入口；其余是启动顺序相关的\n"
            "//! 纯函数/小助手（背景实度、清屏色、env 开关、窗口坐标 sanity）。",
    "frame_loop": "//! 帧循环：`run_mode` 与附属窗口驱动。\n"
                  "//!\n"
                  "//! `run_mode` 组装窗口 + GPU + 管线 + 托盘后进入主循环；\n"
                  "//! 循环内的编号小节（0 事件 / 1 状态 / 2 业务 UI / 3 尺寸 /\n"
                  "//! 4 渲染）是既有的分段注释，改动前先读它们。",
    "recording": "//! 热键录制：`App` 的录制态方法。\n"
                 "//!\n"
                 "//! 录制必须在界面进程里做（按键来自 egui 输入），录完写配置，\n"
                 "//! 常驻进程收到配置保存后自行重注册（`app::daemon::rebind_hotkeys`）。",
    "config_write": "//! 配置字段写入：`apply_ops` 的字符串字段落地层。\n"
                    "//!\n"
                    "//! 分组 + 字段名的字符串通道把新增设置项的成本降到\n"
                    "//! 「paint 一行 + 这里一行」；拼错不 panic 由测试守。",
    "resize": "//! 窗口宽度调整：`WidthRequest` 两种语义与求解。\n"
              "//!\n"
              "//! 「切到独立分栏」只放大，「切回共用单栏」精确恢复默认宽——\n"
              "//! 无脑一个目标宽度会把两种情况都变成「总是设成它」。",
}

written = []
for name, body in moved.items():
    body = transform(name, body)
    while body and body[0].strip() == "":
        body.pop(0)
    while body and body[-1].strip() == "":
        body.pop()
    if name == "recording":
        body = ["impl super::App {"] + body + ["}"]
    imps, supers, cross = build_imports(name, body)
    header = [DOCS[name], ""]
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
    path = "crates/modular-clipboard-ui/src/app/%s.rs" % name
    io.open(path, "w", encoding="utf-8", newline="").write(out)
    written.append((name, len(body), len(imps), len(supers), len(cross)))

# ---------------------------------------------------------------- root
insert_at = None
for i, ln in enumerate(root):
    if ln.strip() == "use crate::theme;":
        insert_at = i + 1
        break
check(insert_at is not None, "root insert point")

block = [
    "",
    "mod boot;",
    "mod config_write;",
    "mod frame_loop;",
    "mod recording;",
    "mod resize;",
    "",
    "use boot::is_sane_window_pos;",
    "use config_write::{set_bool_field, set_number_field, set_opt_number_field, set_text_field};",
    "use resize::{WidthRequest, resize_width_target};",
    "",
]
root[insert_at:insert_at] = block

# tests：在既有的 use super::*; 之后补子模块 glob
for i, ln in enumerate(root):
    if ln.strip() == "use super::*;":
        root[i + 1:i + 1] = [
            "    use super::{",
            "        boot::*,",
            "        config_write::*,",
            "        frame_loop::*,",
            "        recording::*,",
            "        resize::*,",
            "    };",
        ]
        break
else:
    sys.exit("tests use super::* not found")

io.open(P, "w", encoding="utf-8", newline="").write("\n".join(root))

print("root: %d lines" % len(root))
for name, n, ni, ns, nc in written:
    print("  %-14s %5d lines, imports=%d super=%d cross=%d" % (name, n, ni, ns, nc))
