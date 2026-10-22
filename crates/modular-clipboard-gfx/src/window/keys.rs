//! 键位翻译纯逻辑：VK -> egui::Key、修饰键、滚轮、鼠标键、
//! UTF-16 代理对解码。全部是无副作用函数，单测直接覆盖。

use egui::Key;
use egui::Modifiers;
use egui::PointerButton;
use egui::emath::Pos2;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_BACK;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_CONTROL;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_DELETE;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_DOWN;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_END;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_F1;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_F24;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_HOME;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_INSERT;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_LCONTROL;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_LEFT;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_LMENU;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_LSHIFT;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_LWIN;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_MENU;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_NEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_OEM_102;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_OEM_1;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_OEM_2;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_OEM_3;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_OEM_4;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_OEM_5;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_OEM_6;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_OEM_8;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_OEM_COMMA;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_OEM_MINUS;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_OEM_PERIOD;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_OEM_PLUS;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_PRIOR;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_RCONTROL;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_RETURN;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_RIGHT;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_RMENU;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_RSHIFT;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_RWIN;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_SHIFT;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_SPACE;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_TAB;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_UP;
use windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDOWN;
use windows::Win32::UI::WindowsAndMessaging::WM_MBUTTONDOWN;
use windows::Win32::UI::WindowsAndMessaging::WM_MBUTTONUP;
use windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONDOWN;
use windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONUP;

use super::{WHEEL_DELTA, WindowEvent};

/// 字母键。索引 0 对应 `'A'`。
pub(super) const LETTERS: [Key; 26] = [
    Key::A, Key::B, Key::C, Key::D, Key::E, Key::F, Key::G, Key::H, Key::I, Key::J, Key::K, Key::L,
    Key::M, Key::N, Key::O, Key::P, Key::Q, Key::R, Key::S, Key::T, Key::U, Key::V, Key::W, Key::X,
    Key::Y, Key::Z,
];

/// 数字键。索引 0 对应主键盘 `'0'`。
pub(super) const DIGITS: [Key; 10] = [
    Key::Num0, Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7,
    Key::Num8, Key::Num9,
];

/// 功能键 F1..F24。索引 0 对应 `VK_F1`。
const FKEYS: [Key; 24] = [
    Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6, Key::F7, Key::F8, Key::F9, Key::F10,
    Key::F11, Key::F12, Key::F13, Key::F14, Key::F15, Key::F16, Key::F17, Key::F18, Key::F19,
    Key::F20, Key::F21, Key::F22, Key::F23, Key::F24,
];

/// `VIRTUAL_KEY` 转裸 `u16`，便于与 `wParam` 比较。
pub(super) const fn vk(k: VIRTUAL_KEY) -> u16 {
    k.0
}

/// 虚拟键码 → egui 逻辑键。无法映射时返回 `None`。
///
/// 只覆盖 egui 有对应变体的键。带输入法时字母键的 `wParam` 是
/// `'A'..'Z'` 的 ASCII 码，与本函数的字母分支一致，因此输入法场景
/// 也能拿到正确的键。
pub fn key_from_vk(code: u16) -> Option<Key> {
    // 数字键与字母键走区间查表，覆盖 36 个键且无分支预测失败。
    if (b'0' as u16..=b'9' as u16).contains(&code) {
        return Some(DIGITS[(code - b'0' as u16) as usize]);
    }
    if (b'A' as u16..=b'Z' as u16).contains(&code) {
        return Some(LETTERS[(code - b'A' as u16) as usize]);
    }

    // 功能键 F1..F24 是连续区间
    if (vk(VK_F1)..=vk(VK_F24)).contains(&code) {
        return Some(FKEYS[(code - vk(VK_F1)) as usize]);
    }

    let key = match code {
        c if c == vk(VK_LEFT) => Key::ArrowLeft,
        c if c == vk(VK_RIGHT) => Key::ArrowRight,
        c if c == vk(VK_UP) => Key::ArrowUp,
        c if c == vk(VK_DOWN) => Key::ArrowDown,
        c if c == vk(VK_PRIOR) => Key::PageUp,
        c if c == vk(VK_NEXT) => Key::PageDown,
        c if c == vk(VK_HOME) => Key::Home,
        c if c == vk(VK_END) => Key::End,
        c if c == vk(VK_INSERT) => Key::Insert,
        c if c == vk(VK_DELETE) => Key::Delete,
        c if c == vk(VK_BACK) => Key::Backspace,
        c if c == vk(VK_SPACE) => Key::Space,
        c if c == vk(VK_TAB) => Key::Tab,
        c if c == vk(VK_RETURN) => Key::Enter,
        // IMPORTANT: Escape 必须映射——漏了它，`key_pressed(Key::Escape)`
        // 在实机上恒为假（2026-10-21 用户反馈「搜索面板按 Esc 关不掉」）。
        // 单测拦不住这条：测试直接注入 `egui::Event::Key`，绕过了
        // Win32 → egui 的键位翻译。
        c if c == vk(VK_ESCAPE) => Key::Escape,

        // 左右修饰键必须区分：egui 用它们判断单侧快捷键，
        // 合并会让「只用左 Ctrl」这类配置失效。
        c if c == vk(VK_LSHIFT) => Key::ShiftLeft,
        c if c == vk(VK_RSHIFT) => Key::ShiftRight,
        c if c == vk(VK_LCONTROL) => Key::ControlLeft,
        c if c == vk(VK_RCONTROL) => Key::ControlRight,
        c if c == vk(VK_LMENU) => Key::AltLeft,
        c if c == vk(VK_RMENU) => Key::AltRight,
        c if c == vk(VK_LWIN) => Key::SuperLeft,
        c if c == vk(VK_RWIN) => Key::SuperRight,

        // 不区分左右的通用键码（由 IME 或旧程序发出）
        c if c == vk(VK_SHIFT) => Key::ShiftLeft,
        c if c == vk(VK_CONTROL) => Key::ControlLeft,
        c if c == vk(VK_MENU) => Key::AltLeft,

        c if c == vk(VK_OEM_1) => Key::Semicolon,
        c if c == vk(VK_OEM_PLUS) => Key::Equals,
        c if c == vk(VK_OEM_COMMA) => Key::Comma,
        c if c == vk(VK_OEM_MINUS) => Key::Minus,
        c if c == vk(VK_OEM_PERIOD) => Key::Period,
        c if c == vk(VK_OEM_2) => Key::Slash,
        c if c == vk(VK_OEM_3) => Key::Backtick,
        c if c == vk(VK_OEM_4) => Key::OpenBracket,
        c if c == vk(VK_OEM_6) => Key::CloseBracket,
        c if c == vk(VK_OEM_5) => Key::Backslash,
        c if c == vk(VK_OEM_8) => Key::Quote,
        c if c == vk(VK_OEM_102) => Key::IntlBackslash,

        // egui 无对应变体（CapsLock、NumLock、ScrollLock、Apps、Zoom 等），
        // 返回 None 让上层忽略。
        _ => return None,
    };
    Some(key)
}

/// 由四个独立布尔量组合出 egui 修饰键状态。
///
/// Windows 上没有 `mac_cmd` 的概念，一律 `false`。
/// 「Windows 键」映射到 `command`——egui 判定快捷键的主字段是
/// `modifiers.command` 而非 `ctrl`，映射错会导致 Win+ 快捷键全失效。
pub fn modifiers_from(shift: bool, ctrl: bool, alt: bool, windows_key: bool) -> Modifiers {
    Modifiers {
        alt,
        ctrl,
        shift,
        mac_cmd: false,
        command: windows_key,
    }
}

/// 该事件是否自带精确的修饰键状态。
///
/// 自带状态的按键事件比`GetAsyncKeyState` 的实时查询更可靠：
/// 它反映的是「按下那一刻」的组合（例如 Shift+Ctrl 同时按下），
/// 而实时查询在某些键盘布局下无法区分左右修饰键的组合意图。
pub(super) fn is_key_event(ev: &WindowEvent) -> bool {
    matches!(ev, WindowEvent::Key { .. })
}

/// 查询当前修饰键状态。
pub(super) fn current_modifiers() -> Modifiers {
    unsafe {
        modifiers_from(
            is_key_down(VK_SHIFT),
            is_key_down(VK_CONTROL),
            is_key_down(VK_MENU),
            is_key_down(VK_LWIN) || is_key_down(VK_RWIN),
        )
    }
}

/// `GetAsyncKeyState` 的高位为按下。
///
/// 用异步版本而非 `GetKeyState`：后者只在消息队列被 pump 过之后才更新，
/// 而我们是在 pump **过程中**读修饰键，会拿到上一帧的值。
unsafe fn is_key_down(k: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(k.0 as i32) < 0 }
}

/// 滚轮单位 → 行数。正值表示向上拨（内容下滚）。
pub(super) fn wheel_lines(wparam: usize) -> f32 {
    // 高 16 位是有符号刻度数；正 = 远离用户 = 向上滚。
    let raw = ((wparam >> 16) & 0xFFFF) as u16 as i16 as f32;
    // Windows 通常给 120 的整数倍，但高精度触控板会给小数，
    // 因此统一除而不做截断。
    raw / WHEEL_DELTA
}

/// 消息是否表示「按下」（`*DOWN`）。
pub(super) fn is_press(msg: u32) -> bool {
    matches!(
        msg,
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN
    )
}

/// 鼠标消息 → egui 按键。
pub(super) fn mouse_button_of(msg: u32) -> PointerButton {
    match msg {
        WM_RBUTTONDOWN | WM_RBUTTONUP => PointerButton::Secondary,
        WM_MBUTTONDOWN | WM_MBUTTONUP => PointerButton::Middle,
        _ => PointerButton::Primary,
    }
}

/// `lParam` 低 16 位，按**有符号** 16 位解释。
pub(super) fn low_word_i32(lparam: isize) -> i32 {
    (lparam as u16) as i16 as i32
}

/// `lParam` 高 16 位，按**有符号** 16 位解释。
pub(super) fn high_word_i32(lparam: isize) -> i32 {
    ((lparam >> 16) as u16) as i16 as i32
}

/// `lParam` 打包的客户区坐标（物理像素）→ 逻辑点。
pub(super) fn unpack_pos(lparam: isize, scale: f32) -> Pos2 {
    Pos2::new(
        low_word_i32(lparam) as f32 / scale,
        high_word_i32(lparam) as f32 / scale,
    )
}

/// `WM_SIZE` 的 `lParam`：低 16 位宽、高 16 位高，**均为无符号**。
///
/// 与坐标不同，这里不能按有符号解释：宽度不会为负。
pub(super) fn unpack_size(lparam: isize) -> (f32, f32) {
    let w = (lparam as u16) as f32;
    let h = ((lparam >> 16) as u16) as f32;
    (w, h)
}

/// 单个 UTF-16 码元 → 可打印文本。
///
/// 过滤控制字符：`WM_CHAR` 会把 Enter 送成 `\r`、Tab 送成 `\t`、
/// Backspace 送成 `\x08`。这些已由 [`WindowEvent::Key`] 表达，
/// 不过滤会让文本框出现重复字符。
///
/// 代理项（BMP 外的字符）返回 `None`，由 [`CharDecoder`] 配对。
pub(super) fn char_from_utf16_unit(unit: u16) -> Option<String> {
    if (0xD800..=0xDFFF).contains(&unit) {
        return None;
    }
    let c = char::from_u32(unit as u32)?;
    if c.is_control() {
        return None;
    }
    Some(c.to_string())
}

/// UTF-16 代理项配对器。
///
/// Win32 把 BMP 外的字符（emoji、生僻汉字）拆成两条 `WM_CHAR`：
/// 先高代理后低代理。单独看任一条都是无效的，必须攒起来配对。
#[derive(Debug, Default)]
pub struct CharDecoder {
    pub(super) pending_high: Option<u16>,
}

impl CharDecoder {
    /// 新建解码器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一个码元，得到可能为空的文本增量。
    pub fn push(&mut self, unit: u16) -> Option<String> {
        if (0xD800..=0xDBFF).contains(&unit) {
            // 高代理：存起来等低代理。
            self.pending_high = Some(unit);
            return None;
        }
        if (0xDC00..=0xDFFF).contains(&unit) {
            // 低代理必须与高代理配对，孤立的下代理直接丢弃。
            let high = self.pending_high.take()?;
            let cp = 0x1_0000u32 + (((high as u32) - 0xD800) << 10) + ((unit as u32) - 0xDC00);
            return char::from_u32(cp).map(|c| c.to_string());
        }
        // 普通码元。若前面攒了未配对的高代理，说明序列损坏
        // （正常 Win32 不会这样，但输入注入可能），丢弃那个高代理。
        self.pending_high = None;
        char_from_utf16_unit(unit)
    }
}

// ---------------------------------------------------------------- 单测
