//! 鼠标 / 键盘输入模拟：点击、输入文本、滚动、拖拽。
//!
//! 鼠标用 `SetCursorPos` + `mouse_event`（比 SendInput 构造 MOUSEINPUT
//! 简单，且 SetCursorPos 已处理绝对坐标）；键盘用 `SendInput` +
//! `KEYEVENTF_UNICODE`（逐 UTF-16 单元，支持中文）。

#![cfg(windows)]

use anyhow::Result;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS, SendInput, VIRTUAL_KEY, VK_RETURN, mouse_event,
};
use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

/// 鼠标按键选择。
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    /// 从工具参数（left/right/middle）解析，未知值默认左键。
    pub fn from_str(s: &str) -> Self {
        match s {
            "right" => Self::Right,
            "middle" => Self::Middle,
            _ => Self::Left,
        }
    }

    /// 该按键的 down / up 事件标志。
    fn flags(&self) -> (MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS) {
        match self {
            Button::Left => (MOUSE_EVENT_FLAGS(0x0002), MOUSE_EVENT_FLAGS(0x0004)),
            Button::Right => (MOUSE_EVENT_FLAGS(0x0008), MOUSE_EVENT_FLAGS(0x0010)),
            Button::Middle => (MOUSE_EVENT_FLAGS(0x0020), MOUSE_EVENT_FLAGS(0x0040)),
        }
    }
}

/// 在 (x,y) 点击（可选双击、可选按键）。
pub fn click(x: i32, y: i32, button: Button, double: bool) -> Result<()> {
    unsafe {
        SetCursorPos(x, y)?;
        // 给系统一点时间把光标移到位（尤其跨高刷屏/远程桌面）。
        std::thread::sleep(std::time::Duration::from_millis(20));
        let (down, up) = button.flags();
        let times = if double { 2 } else { 1 };
        for i in 0..times {
            mouse_event(down, 0, 0, 0, 0);
            mouse_event(up, 0, 0, 0, 0);
            if i == 0 && double {
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
        }
    }
    Ok(())
}

/// 输入文本（可选结尾回车）。
pub fn type_text(text: &str, enter: bool) -> Result<()> {
    unsafe {
        for ch in text.chars() {
            let mut buf = [0u16; 2];
            let units = ch.encode_utf16(&mut buf);
            for u in units {
                send_unicode_unit(*u);
            }
        }
        if enter {
            send_vk(VK_RETURN);
        }
    }
    Ok(())
}

/// 发送一个 UTF-16 单元的 down + up。
unsafe fn send_unicode_unit(u: u16) {
    let mk = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: u,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(
            &[mk(KEYEVENTF_UNICODE), mk(KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)],
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

/// 发送一个虚拟键的 down + up。
unsafe fn send_vk(vk: VIRTUAL_KEY) {
    let mk = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(
            &[mk(KEYBD_EVENT_FLAGS(0)), mk(KEYEVENTF_KEYUP)],
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

/// 在指定位置（缺省当前鼠标位置）滚动。
///
/// `dx`/`dy` 为滚轮格数（正：向下/向右）。
pub fn scroll(x: Option<i32>, y: Option<i32>, dx: i32, dy: i32) -> Result<()> {
    unsafe {
        if let (Some(x), Some(y)) = (x, y) {
            SetCursorPos(x, y)?;
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        // WHEEL_DELTA = 120（一格）。
        if dy != 0 {
            // MOUSEEVENTF_WHEEL = 0x0800
            mouse_event(MOUSE_EVENT_FLAGS(0x0800), 0, 0, dy * 120, 0);
        }
        if dx != 0 {
            // MOUSEEVENTF_HWHEEL = 0x1000
            mouse_event(MOUSE_EVENT_FLAGS(0x1000), 0, 0, dx * 120, 0);
        }
    }
    Ok(())
}

/// 从 (fx,fy) 拖拽到 (tx,ty)。
pub fn drag(fx: i32, fy: i32, tx: i32, ty: i32, button: Button) -> Result<()> {
    unsafe {
        SetCursorPos(fx, fy)?;
        std::thread::sleep(std::time::Duration::from_millis(20));
        let (down, up) = button.flags();
        mouse_event(down, 0, 0, 0, 0);
        std::thread::sleep(std::time::Duration::from_millis(60));
        // 分几步移动（模拟真实拖拽，避免一步跳变导致拖放不识别）。
        const STEPS: i32 = 12;
        for i in 1..=STEPS {
            let t = i as f32 / STEPS as f32;
            let x = fx as f32 + (tx - fx) as f32 * t;
            let y = fy as f32 + (ty - fy) as f32 * t;
            SetCursorPos(x as i32, y as i32)?;
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        SetCursorPos(tx, ty)?;
        std::thread::sleep(std::time::Duration::from_millis(40));
        mouse_event(up, 0, 0, 0, 0);
    }
    Ok(())
}

// ---------------------------------------------------------------- 组合键

/// Win32 虚拟键码（`VK_*`）。这些值是 Windows ABI 的一部分，不会变。
mod vk {
    pub const BACK: u16 = 0x08;
    pub const TAB: u16 = 0x09;
    pub const RETURN: u16 = 0x0D;
    pub const SHIFT: u16 = 0x10;
    pub const CONTROL: u16 = 0x11;
    pub const MENU: u16 = 0x12; // Alt
    pub const ESCAPE: u16 = 0x1B;
    pub const SPACE: u16 = 0x20;
    pub const PRIOR: u16 = 0x21; // PageUp
    pub const NEXT: u16 = 0x22; // PageDown
    pub const END: u16 = 0x23;
    pub const HOME: u16 = 0x24;
    pub const LEFT: u16 = 0x25;
    pub const UP: u16 = 0x26;
    pub const RIGHT: u16 = 0x27;
    pub const DOWN: u16 = 0x28;
    pub const DELETE: u16 = 0x2E;
    pub const LWIN: u16 = 0x5B;
    /// F1 的码位；F2..F24 依次 +1。
    pub const F1: u16 = 0x70;
    /// F24 的码位（含）。
    pub const F24: u16 = 0x87;
}

/// 把一个按键名映射到虚拟键码。
///
/// 接受：单个 ASCII 字母/数字、`f1`..`f24`、以及常见命名键
/// （`enter`/`esc`/`tab`/`space`/`backspace`/`delete`/方向键等）。
pub fn vk_for_key(name: &str) -> Option<u16> {
    let k = name.trim().to_ascii_lowercase();
    // 单个字母或数字：ASCII 码位恰好等于虚拟键码（A-Z、0-9）。
    let mut chars = k.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && c.is_ascii_alphanumeric() {
            return Some(c.to_ascii_uppercase() as u16);
        }
    if let Some(n) = k.strip_prefix('f').and_then(|s| s.parse::<u16>().ok()) {
        // 用 `saturating_sub` 而非 `n - 1`：`f0` 会让后者下溢 panic，
        // 而输入解析绝不该因为一个畸形字符串把进程带走。
        let code = vk::F1.saturating_add(n.saturating_sub(1));
        if n >= 1 && code <= vk::F24 {
            return Some(code);
        }
        // 越界必须返回 None，不能算出相邻键码——那会按下完全不相干的键。
        return None;
    }
    Some(match k.as_str() {
        "enter" | "return" => vk::RETURN,
        "esc" | "escape" => vk::ESCAPE,
        "tab" => vk::TAB,
        "space" | "spacebar" => vk::SPACE,
        "backspace" | "back" => vk::BACK,
        "delete" | "del" => vk::DELETE,
        "home" => vk::HOME,
        "end" => vk::END,
        "pageup" | "pgup" => vk::PRIOR,
        "pagedown" | "pgdn" => vk::NEXT,
        "up" => vk::UP,
        "down" => vk::DOWN,
        "left" => vk::LEFT,
        "right" => vk::RIGHT,
        _ => return None,
    })
}

/// 解析组合键字符串，返回 `(修饰键码列表, 主键码)`。
///
/// 形如 `"ctrl+shift+s"`、`"alt+f4"`、`"enter"`。修饰键可写
/// `ctrl`/`control`、`alt`、`shift`、`win`/`meta`/`super`，大小写不敏感。
///
/// 纯函数，便于单测：按键映射写错是「静默无效」类问题的典型来源——
/// 调用返回成功但按下的键完全不对，界面毫无反应。
pub fn parse_combo(combo: &str) -> Result<(Vec<u16>, u16)> {
    let mut mods: Vec<u16> = Vec::new();
    let mut main: Option<u16> = None;

    for part in combo.split('+') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods.push(vk::CONTROL),
            "alt" | "menu" => mods.push(vk::MENU),
            "shift" => mods.push(vk::SHIFT),
            "win" | "meta" | "super" | "cmd" => mods.push(vk::LWIN),
            _ => {
                anyhow::ensure!(
                    main.is_none(),
                    "组合键只能有一个主键，但 {combo:?} 里出现了第二个：{p}"
                );
                main = Some(vk_for_key(p).ok_or_else(|| {
                    anyhow::anyhow!("无法识别的按键 {p:?}（支持字母/数字/f1-f24/常见命名键）")
                })?);
            }
        }
    }

    let main = main.ok_or_else(|| anyhow::anyhow!("组合键 {combo:?} 缺少主键"))?;
    anyhow::ensure!(!mods.is_empty() || main != 0, "组合键解析结果非法");
    Ok((mods, main))
}

/// 按下并释放一个组合键（修饰键按住 → 主键按放 → 修饰键逆序释放）。
///
/// 释放顺序与按下顺序**相反**是 Windows 的惯例：某些程序在修饰键
/// 仍按住时会误判后续按键。
pub fn key_press(combo: &str) -> Result<()> {
    let (mods, main) = parse_combo(combo)?;
    unsafe {
        for m in &mods {
            send_vk_raw(*m, false);
        }
        send_vk_raw(main, false);
        send_vk_raw(main, true);
        for m in mods.iter().rev() {
            send_vk_raw(*m, true);
        }
    }
    Ok(())
}

/// 发送一个虚拟键的按下或释放。
unsafe fn send_vk_raw(vk: u16, up: bool) {
    let flags = if up {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // SAFETY: 调用方保证在 unsafe 上下文中；input 是 POD 且大小已给对。
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_letter_maps_to_ascii_code() {
        assert_eq!(vk_for_key("s"), Some(0x53));
        assert_eq!(vk_for_key("S"), Some(0x53));
        assert_eq!(vk_for_key("7"), Some(0x37));
    }

    #[test]
    fn function_keys_map_sequentially() {
        assert_eq!(vk_for_key("f1"), Some(vk::F1));
        assert_eq!(vk_for_key("f12"), Some(vk::F1 + 11));
        assert_eq!(vk_for_key("f24"), Some(vk::F24));
        // 越界必须是 None，不能算出相邻的键码（那会按下完全不相干的键）。
        assert_eq!(vk_for_key("f25"), None);
        assert_eq!(vk_for_key("f0"), None);
    }

    #[test]
    fn named_keys_are_recognized() {
        assert_eq!(vk_for_key("enter"), Some(vk::RETURN));
        assert_eq!(vk_for_key("Esc"), Some(vk::ESCAPE));
        assert_eq!(vk_for_key("pageDown"), Some(vk::NEXT));
        assert_eq!(vk_for_key("无此键"), None);
    }

    #[test]
    fn combo_splits_modifiers_and_main() {
        let (mods, main) = parse_combo("ctrl+shift+s").unwrap();
        assert_eq!(mods, vec![vk::CONTROL, vk::SHIFT]);
        assert_eq!(main, 0x53);

        let (mods, main) = parse_combo("Alt+F4").unwrap();
        assert_eq!(mods, vec![vk::MENU]);
        assert_eq!(main, vk::F1 + 3);

        // 无修饰键也合法
        let (mods, main) = parse_combo("enter").unwrap();
        assert!(mods.is_empty());
        assert_eq!(main, vk::RETURN);
    }

    #[test]
    fn combo_rejects_bad_input() {
        assert!(parse_combo("ctrl+").is_err(), "只有修饰键应报错");
        assert!(parse_combo("").is_err(), "空串应报错");
        assert!(parse_combo("ctrl+a+b").is_err(), "两个主键应报错");
        assert!(parse_combo("ctrl+nope").is_err(), "未知键应报错");
    }

    #[test]
    fn win_key_is_recognized_as_modifier() {
        let (mods, main) = parse_combo("win+r").unwrap();
        assert_eq!(mods, vec![vk::LWIN]);
        assert_eq!(main, 0x52);
    }
}
