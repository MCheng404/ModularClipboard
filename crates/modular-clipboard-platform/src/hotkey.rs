//! 全局快捷键注册。
//!
//! 依赖托盘线程的消息循环：`RegisterHotKey` 会把 `WM_HOTKEY` 投递到
//! **注册时指定的窗口**所在线程。因此本模块不自己建窗口，只提供
//! 「用哪个 hwnd / 哪个组合注册」的能力，注册动作由
//! [`crate::tray`] 在其线程内完成。
//!
//! # 冲突是常态
//!
//! `Ctrl+Shift+V` 这类组合常被别的软件占用。注册失败时**绝不 panic**，
//! 只返回 `Err`，由调用方记日志并继续运行——托盘与剪贴板记录不应
//! 因为一个快捷键被占用而不可用。

use anyhow::{Context, Result};

/// 快捷键配置：修饰键 + 虚拟键码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeySpec {
    /// 修饰键位掩码，语义与 Win32 `HOT_KEY_MODIFIERS` 一致。
    pub modifiers: u32,
    /// 虚拟键码（如 `'V'` 为 0x56）。
    pub vk: u32,
}

impl HotkeySpec {
    /// 构造组合键。
    pub const fn new(modifiers: u32, vk: u32) -> Self {
        Self { modifiers, vk }
    }

    /// 是否要求 `Alt`。
    pub const fn alt(self) -> bool {
        self.modifiers & MOD_ALT != 0
    }

    /// 是否要求 `Ctrl`。
    pub const fn ctrl(self) -> bool {
        self.modifiers & MOD_CONTROL != 0
    }

    /// 是否要求 `Shift`。
    pub const fn shift(self) -> bool {
        self.modifiers & MOD_SHIFT != 0
    }

    /// 是否要求 `Win`。
    pub const fn win(self) -> bool {
        self.modifiers & MOD_WIN != 0
    }

    /// 是否禁止按键自动重复。
    pub const fn no_repeat(self) -> bool {
        self.modifiers & MOD_NOREPEAT != 0
    }

    /// 把主键名（单个 ASCII 字母或数字）转成虚拟键码。
    ///
    /// 大小写都接受——用户输入 "Ctrl+V" 与 "ctrl+v" 都应可用。
    /// 只支持 A-Z 与 0-9：覆盖剪贴板工具的常用键位，
    /// 且避免引入完整键名表。
    pub fn from_key(modifiers: u32, key: char) -> Result<Self> {
        let vk = match key {
            'a'..='z' => key.to_ascii_uppercase() as u32,
            'A'..='Z' => key as u32,
            '0'..='9' => key as u32,
            other => anyhow::bail!("不支持的快捷键主键: {other:?}（仅支持 A-Z 与 0-9）"),
        };
        Ok(Self::new(modifiers, vk))
    }
}

/// Win32 `HOT_KEY_MODIFIERS` 位值。本地定义以保持纯逻辑层可跨平台单测。
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
pub const MOD_NOREPEAT: u32 = 0x4000;

/// 默认唤起快捷键：`Ctrl+Shift+V`。
///
/// 与主流剪贴板工具一致，用户肌肉记忆可直接迁移。
pub const DEFAULT_HOTKEY: HotkeySpec = HotkeySpec::new(MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, 0x56);

/// 可读的组合键文本，用于设置界面展示与日志。
pub fn describe(spec: HotkeySpec) -> String {
    let mut parts = Vec::new();
    if spec.ctrl() {
        parts.push("Ctrl");
    }
    if spec.shift() {
        parts.push("Shift");
    }
    if spec.alt() {
        parts.push("Alt");
    }
    if spec.win() {
        parts.push("Win");
    }
    let key = vk_name(spec.vk);
    parts.push(&key);
    parts.join("+")
}

/// 虚拟键码的可读名称。
///
/// **不能直接用 `char::from_u32`**：Win32 的功能键码与 ASCII 重叠
/// （`VK_F1 = 0x70`，而 `'p' = 0x70`），直接转会显示成
/// 「Ctrl+P」而不是「Ctrl+F1」。故先判功能键区间。
fn vk_name(vk: u32) -> String {
    // VK_F1..VK_F24 连续为 0x70..=0x87。
    if (0x70..=0x87).contains(&vk) {
        return format!("F{}", vk - 0x70 + 1);
    }
    // 数字键 0x30..=0x39 与字母键 0x41..=0x5A 的 ASCII 含义一致。
    match char::from_u32(vk) {
        Some(c) if c.is_ascii_alphanumeric() => c.to_ascii_uppercase().to_string(),
        _ => format!("VK{vk:02X}"),
    }
}

/// 向指定窗口注册全局快捷键。
///
/// `hwnd` 必须是**调用方所在线程**创建的窗口——`WM_HOTKEY` 会投递到
/// 该窗口所属线程的队列。
///
/// 失败（已被占用 / 无效组合）时返回 `Err`，不 panic。
#[cfg(windows)]
pub fn register(hwnd: windows::Win32::Foundation::HWND, id: i32, spec: HotkeySpec) -> Result<()> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{HOT_KEY_MODIFIERS, RegisterHotKey};

    unsafe {
        RegisterHotKey(Some(hwnd), id, HOT_KEY_MODIFIERS(spec.modifiers), spec.vk)
    }
        .with_context(|| format!("注册全局快捷键 {}失败（可能已被其它软件占用）", describe(spec)))
}

/// 注销先前注册的快捷键。
#[cfg(windows)]
pub fn unregister(hwnd: windows::Win32::Foundation::HWND, id: i32) {
    use windows::Win32::UI::Input::KeyboardAndMouse::UnregisterHotKey;
    let _ = unsafe { UnregisterHotKey(Some(hwnd), id) };
}

/// 非Windows 平台的占位实现。
#[cfg(not(windows))]
pub fn register(
    _hwnd: windows::Win32::Foundation::HWND,
    _id: i32,
    _spec: HotkeySpec,
) -> Result<()> {
    anyhow::bail!("当前平台尚未实现全局快捷键")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_predicates_are_independent() {
        let s = HotkeySpec::new(MOD_CONTROL, 0x56);
        assert!(s.ctrl());
        assert!(!s.shift());
        assert!(!s.alt());
        assert!(!s.win());

        let s = HotkeySpec::new(MOD_ALT | MOD_SHIFT, 0x56);
        assert!(s.alt());
        assert!(s.shift());
        assert!(!s.ctrl());
    }

    #[test]
    fn combinations_do_not_interfere() {
        let all = HotkeySpec::new(MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN, 0x41);
        assert!(all.alt() && all.ctrl() && all.shift() && all.win());

        let none = HotkeySpec::new(0, 0x41);
        assert!(!none.alt() && !none.ctrl() && !none.shift() && !none.win());
    }

    #[test]
    fn default_hotkey_is_ctrl_shift_v() {
        let d = DEFAULT_HOTKEY;
        assert!(d.ctrl());
        assert!(d.shift());
        assert!(!d.alt());
        assert!(d.no_repeat(), "应禁止长按连发");
        assert_eq!(d.vk, 0x56);
    }

    #[test]
    fn letters_map_to_uppercase_vk() {
        assert_eq!(HotkeySpec::from_key(MOD_CONTROL, 'v').unwrap().vk, 0x56);
        assert_eq!(HotkeySpec::from_key(MOD_CONTROL, 'a').unwrap().vk, 0x41);
        assert_eq!(HotkeySpec::from_key(0, 'Z').unwrap().vk, 0x5A);
    }

    #[test]
    fn uppercase_letters_are_accepted() {
        // 用户输入 "Ctrl+Shift+V" 与 "ctrl+shift+v" 都应可用。
        assert_eq!(
            HotkeySpec::from_key(MOD_CONTROL, 'V').unwrap().vk,
            HotkeySpec::from_key(MOD_CONTROL, 'v').unwrap().vk
        );
        assert_eq!(HotkeySpec::from_key(0, 'A').unwrap().vk, 0x41);
    }

    #[test]
    fn digits_map_to_ascii() {
        assert_eq!(HotkeySpec::from_key(MOD_ALT, '0').unwrap().vk, 0x30);
        assert_eq!(HotkeySpec::from_key(MOD_ALT, '9').unwrap().vk, 0x39);
    }

    #[test]
    fn unsupported_keys_are_rejected() {
        assert!(HotkeySpec::from_key(MOD_CONTROL, '中').is_err());
        assert!(HotkeySpec::from_key(MOD_CONTROL, '!').is_err());
        assert!(HotkeySpec::from_key(MOD_CONTROL, ' ').is_err());
    }

    #[test]
    fn describe_is_readable() {
        assert_eq!(describe(DEFAULT_HOTKEY), "Ctrl+Shift+V");
        assert_eq!(describe(HotkeySpec::new(MOD_ALT | MOD_CONTROL, 0x41)), "Ctrl+Alt+A");
        assert_eq!(describe(HotkeySpec::new(0, 0x31)), "1");
    }

    #[test]
    fn describe_falls_back_for_non_printable_vk() {
        // VK_F1 = 0x70，恰好等于 ASCII 'p'。若不特判会显示成「Ctrl+P」。
        assert_eq!(describe(HotkeySpec::new(MOD_CONTROL, 0x70)), "Ctrl+F1");
        assert_eq!(describe(HotkeySpec::new(MOD_CONTROL, 0x87)), "Ctrl+F24");
    }

    #[test]
    fn letter_p_is_not_confused_with_f1() {
        // Win32 小写 'p' = 0x70 与 VK_F1 数值相同。
        // from_key 会把字母归一为大写（0x50），故用户输入字母路径不会撞上；
        // vk_name 则必须把裸 0x70 显示为功能键。
        assert_eq!(HotkeySpec::from_key(MOD_CONTROL, 'p').unwrap().vk, 0x50);
        assert_eq!(vk_name(0x50), "P");
        assert_eq!(vk_name(0x70), "F1");
    }

    #[test]
    fn describe_handles_control_keys() {
        // VK_LCONTROL = 0xA4 不是可打印字符，应回退为十六进制名。
        assert_eq!(describe(HotkeySpec::new(MOD_CONTROL, 0xA4)), "Ctrl+VKA4");
    }

    #[test]
    fn modifier_bits_match_win32_values() {
        // 与 Win32 MOD_* 常量一致性由 grep 源码核对：
        // ALT=1 CONTROL=2 SHIFT=4 WIN=8 NOREPEAT=0x4000。
        assert_eq!(MOD_ALT, 1);
        assert_eq!(MOD_CONTROL, 2);
        assert_eq!(MOD_SHIFT, 4);
        assert_eq!(MOD_WIN, 8);
        assert_eq!(MOD_NOREPEAT, 0x4000);
    }

    #[test]
    fn modifier_bits_are_distinct() {
        let all = [MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, MOD_NOREPEAT];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "修饰键位不应重复");
                assert_eq!(a & b, 0, "修饰键位不应重叠");
            }
        }
    }
}