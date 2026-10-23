//! 窗口枚举 / 查找 / 调整 / 置前 / 布局读取（Win32）。
//!
//! 只依赖 Win32 API（`windows` crate），不依赖 egui。
//!
//! 布局（`read_layout`）用 `EnumWindows` + `EnumChildWindows` 收集
//! HWND 树，返回标题、类名、矩形、可见/可用状态。这只覆盖有 HWND 的
//! 控件（Win32/WinForms/部分 WPF），现代 UWP / 网页内容需要 UI
//! Automation，是后续增强。

#![cfg(windows)]

use anyhow::Result;
use serde_json::{json, Value};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GA_PARENT, GetAncestor, GetClassNameW, GetForegroundWindow,
    GetWindowRect, GetWindowTextW, IsIconic, IsWindowVisible, SetForegroundWindow, SetWindowPos,
    ShowWindow, SWP_NOZORDER, SW_RESTORE,
};

// ---------------------------------------------------------------------------
// 查找
// ---------------------------------------------------------------------------

/// 按标题模糊匹配（大小写不敏感）查找顶层窗口，返回第一个匹配的 HWND。
pub fn find_window(needle: &str) -> Option<HWND> {
    // 用裸结构体 + 指针传给 EnumWindows（模式同 egui crate 里的回调）。
    struct FindCtx {
        needle: String,
        found: Option<HWND>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        unsafe {
            let ctx = &mut *(lparam.0 as *mut FindCtx);
            let title = window_text(hwnd);
            let visible = IsWindowVisible(hwnd).as_bool();
            if visible
                && !title.is_empty()
                && title.to_lowercase().contains(&ctx.needle.to_lowercase())
            {
                ctx.found = Some(hwnd);
                return windows::core::BOOL(0); // 停止枚举
            }
            windows::core::BOOL(1)
        }
    }
    let mut ctx = FindCtx {
        needle: needle.to_string(),
        found: None,
    };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.found
}

/// 读取窗口标题（内部已处理 UTF-16 截断）。
unsafe fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n as usize])
}

/// 读取窗口类名。
unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n as usize])
}

// ---------------------------------------------------------------------------
// 调整大小 / 置前
// ---------------------------------------------------------------------------

/// 调整窗口大小；`pos` 为 Some 时同时移动，为 None 时只改尺寸。
pub fn resize(needle: &str, w: i32, h: i32, pos: Option<(i32, i32)>) -> Result<()> {
    let hwnd = find_window(needle)
        .ok_or_else(|| anyhow::anyhow!("未找到标题包含「{needle}」的窗口"))?;
    let (x, y, flags) = match pos {
        Some((x, y)) => (x, y, SWP_NOZORDER),
        None => (0, 0, SWP_NOZORDER | windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE),
    };
    unsafe { SetWindowPos(hwnd, None, x, y, w, h, flags)? };
    Ok(())
}

/// 将窗口置前（最小化则先还原）。
pub fn focus(needle: &str) -> Result<()> {
    let hwnd = find_window(needle)
        .ok_or_else(|| anyhow::anyhow!("未找到标题包含「{needle}」的窗口"))?;
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        // SetForegroundWindow 受前台锁定限制（非前台进程可能失败）。
        // 这里做一次尝试，失败时如实报错。
        if !SetForegroundWindow(hwnd).as_bool() {
            // 检查是否本来就是前台（那也算成功）。
            if GetForegroundWindow() != hwnd {
                anyhow::bail!("系统拒绝置前（前台锁定），可点击任务栏图标后重试");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 布局读取
// ---------------------------------------------------------------------------

/// 读取窗口布局为 JSON。
///
/// `title` 为 None 时枚举所有顶层窗口（虚拟桌面视角）；为 Some 时只
/// 展开该窗口及其子控件。`depth` 限制子树深度。
pub fn layout(title: Option<&str>, depth: u32) -> Value {
    match title {
        Some(t) => {
            let hwnd = find_window(t);
            match hwnd {
                Some(hwnd) => json!({
                    "window_title": unsafe { window_text(hwnd) },
                    "class": unsafe { class_name(hwnd) },
                    "rect": rect_json(hwnd),
                    "children": node_tree(hwnd, depth, 0),
                }),
                None => json!({ "error": format!("未找到标题包含「{t}」的窗口") }),
            }
        }
        None => {
            // 枚举所有顶层窗口。
            struct Ctx {
                nodes: Vec<Value>,
            }
            unsafe extern "system" fn top_cb(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
                unsafe {
                    let ctx = &mut *(lparam.0 as *mut Ctx);
                    let title = window_text(hwnd);
                    if IsWindowVisible(hwnd).as_bool() && !title.is_empty() {
                        ctx.nodes.push(json!({
                            "title": title,
                            "class": class_name(hwnd),
                            "rect": rect_json(hwnd),
                        }));
                    }
                    windows::core::BOOL(1)
                }
            }
            let mut ctx = Ctx { nodes: Vec::new() };
            unsafe {
                let _ = EnumWindows(Some(top_cb), LPARAM(&mut ctx as *mut _ as isize));
            }
            json!({ "windows": ctx.nodes })
        }
    }
}

/// 递归收集一个窗口的子控件树。
fn node_tree(hwnd: HWND, max_depth: u32, current: u32) -> Value {
    unsafe {
        let mut node = json!({
            "title": window_text(hwnd),
            "class": class_name(hwnd),
            "rect": rect_json(hwnd),
            "visible": IsWindowVisible(hwnd).as_bool(),
        });
        if current >= max_depth {
            node["children"] = json!([]);
            return node;
        }
        struct Ctx {
            parent: HWND,
            max_depth: u32,
            current: u32,
            children: Vec<Value>,
        }
        unsafe extern "system" fn child_cb(child: HWND, lparam: LPARAM) -> windows::core::BOOL {
            unsafe {
                let ctx = &mut *(lparam.0 as *mut Ctx);
                // EnumChildWindows 会枚举所有后代（不只是直接子）。用
                // GetAncestor(GA_PARENT) 判断直接父级，保证层级正确。
                let parent = GetAncestor(child, GA_PARENT);
                if parent == ctx.parent {
                    let mut child_node = json!({
                        "title": window_text(child),
                        "class": class_name(child),
                        "rect": rect_json(child),
                        "enabled": IsWindowEnabled(child).as_bool(),
                    });
                    child_node["children"] = node_tree(child, ctx.max_depth, ctx.current + 1);
                    ctx.children.push(child_node);
                }
                windows::core::BOOL(1)
            }
        }
        let mut ctx = Ctx {
            parent: hwnd,
            max_depth,
            current,
            children: Vec::new(),
        };
        let _ = EnumChildWindows(
            Some(hwnd),
            Some(child_cb),
            LPARAM(&mut ctx as *mut _ as isize),
        );
        node["children"] = json!(ctx.children);
        node
    }
}

/// 读取窗口外接矩形（屏幕坐标）并转 JSON。
fn rect_json(hwnd: HWND) -> Value {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut rc);
    }
    json!({
        "x": rc.left,
        "y": rc.top,
        "width": rc.right - rc.left,
        "height": rc.bottom - rc.top,
    })
}
