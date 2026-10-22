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

    /// 从配置里的 [`modular_clipboard_core::config::Hotkey`] 构造。
    ///
    /// 配置存的是**可读文本**（`key: "V"` / `"F5"`），而注册需要虚拟键码。
    /// 转换集中在这里，避免调用方各自拼一套键名表。
    ///
    /// 一律带上 `MOD_NOREPEAT`：长按热键连发会把「唤一次界面」变成
    /// 「一秒钟唤几十次」，界面表现为疯狂闪烁。
    pub fn from_config(hk: &modular_clipboard_core::config::Hotkey) -> Result<Self> {
        let mut mods = MOD_NOREPEAT;
        if hk.ctrl {
            mods |= MOD_CONTROL;
        }
        if hk.shift {
            mods |= MOD_SHIFT;
        }
        if hk.alt {
            mods |= MOD_ALT;
        }
        if hk.win {
            mods |= MOD_WIN;
        }
        Self::from_key_str(mods, &hk.key)
    }

    /// 按主键名构造（支持 `V` / `5` / `F5`）。
    pub fn from_key_str(modifiers: u32, key: &str) -> Result<Self> {
        let t = key.trim();
        // F1..F24
        if let Some(rest) = t.strip_prefix(['f', 'F'])
            && let Ok(n) = rest.parse::<u32>()
            && (1..=24).contains(&n)
        {
            return Ok(Self::new(modifiers, 0x70 + n - 1));
        }
        let mut chars = t.chars();
        let c = chars.next().ok_or_else(|| anyhow::anyhow!("主键为空"))?;
        if chars.next().is_some() {
            anyhow::bail!("主键 {key:?} 不是单个按键");
        }
        Self::from_key(modifiers, c)
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

/// 默认唤起快捷键：**Win+V**。
///
/// 与 `Config::default()` 保持一致（那边才是配置权威）。
/// 与 Windows 自带剪贴板历史同键位——接管它才能让换工具没有学习成本。
///
/// 注意：系统可能已经占用这个组合，那时注册会失败（只记日志，
/// 不阻断运行），用户可以自己在设置里换。
pub const DEFAULT_HOTKEY: HotkeySpec = HotkeySpec::new(MOD_WIN | MOD_NOREPEAT, 0x56);

/// 从配置构造注册规格；键名无法解析时**回退到默认键**并记日志。
///
/// 配置里的键名是用户可编辑的文本（也可能是老版本写下的），
/// 解析失败不该让常驻进程起不来，更不该让用户没有任何热键可用——
/// 退到默认键位，用户想改在设置里改回来即可。
pub fn spec_or_default(hk: &modular_clipboard_core::config::Hotkey) -> HotkeySpec {
    match HotkeySpec::from_config(hk) {
        Ok(spec) => spec,
        Err(e) => {
            tracing::warn!(
                %e,
                fallback = %describe(DEFAULT_HOTKEY),
                "快捷键配置无法解析，回退到默认键位"
            );
            DEFAULT_HOTKEY
        }
    }
}

/// 热键槽位。托盘按槽位注册/注销，改绑定时只动对应那个。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// 显示 / 隐藏主窗口。
    Toggle,
    /// 粘贴上一条。
    PastePrevious,
}

impl Slot {
    /// Win32 热键 id。`RegisterHotKey` 用 id 区分同窗口内的多个热键。
    pub const fn id(self) -> i32 {
        match self {
            Slot::Toggle => 0x0BEE,
            Slot::PastePrevious => 0x0BEF,
        }
    }

    /// 用户可见的槽位名。注册失败的信息会带上它——
    /// 「某个快捷键注册失败」不告诉用户是哪一个，等于没说。
    ///
    /// 与设置界面的行标题保持一致（`paint.rs` 的快捷键卡片）。
    pub const fn label(self) -> modular_clipboard_core::i18n::Bi {
        match self {
            Slot::Toggle => ("显示主窗口", "Show main window"),
            Slot::PastePrevious => ("粘贴上一个条目", "Paste previous entry"),
        }
    }
}

/// 注册失败的用户可读信息。
///
/// **格式必须稳定**：设置界面靠「信息里包含该行当前的组合文本」
/// 来判断是哪一行出了问题（见 `paint.rs` 的 Hotkey 渲染）。
/// 启动注册与运行时重注册都用它拼，两处措辞才不会漂移。
pub fn failed_message(slot: Slot, spec: HotkeySpec) -> String {
    format!(
        "{}：{} 注册失败（可能已被其它程序占用）",
        modular_clipboard_core::i18n::t(slot.label()),
        describe(spec)
    )
}

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
    fn default_hotkey_is_win_v() {
        let d = DEFAULT_HOTKEY;
        assert!(d.win(), "默认应为 Win+V");
        assert!(!d.ctrl());
        assert!(!d.shift());
        assert!(d.no_repeat(), "应禁止长按连发");
        assert_eq!(d.vk, 0x56);
    }

    #[test]
    fn slots_have_distinct_ids() {
        assert_ne!(Slot::Toggle.id(), Slot::PastePrevious.id());
    }

    #[test]
    fn from_config_maps_modifiers_and_key() {
        use modular_clipboard_core::config::Hotkey;
        let hk = Hotkey {
            key: "F5".into(),
            ctrl: true,
            shift: true,
            alt: false,
            win: false,
        };
        let spec = HotkeySpec::from_config(&hk).unwrap();
        assert!(spec.ctrl() && spec.shift() && !spec.alt() && !spec.win());
        assert_eq!(spec.vk, 0x74, "F5 的虚拟键码是 0x74");
        assert!(spec.no_repeat());
    }

    #[test]
    fn from_key_str_accepts_letters_digits_and_fkeys() {
        assert_eq!(HotkeySpec::from_key_str(0, "v").unwrap().vk, 0x56);
        assert_eq!(HotkeySpec::from_key_str(0, "7").unwrap().vk, 0x37);
        assert_eq!(HotkeySpec::from_key_str(0, "F1").unwrap().vk, 0x70);
        assert_eq!(HotkeySpec::from_key_str(0, "f12").unwrap().vk, 0x7B);
        assert!(HotkeySpec::from_key_str(0, "F25").is_err(), "超出 F24 应拒绝");
        assert!(HotkeySpec::from_key_str(0, "中").is_err());
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
        assert_eq!(describe(DEFAULT_HOTKEY), "Win+V");
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

    #[test]
    fn spec_or_default_falls_back_on_bad_key() {
        use modular_clipboard_core::config::Hotkey;
        let bad = Hotkey {
            key: "中".into(),
            ctrl: true,
            shift: false,
            alt: false,
            win: false,
        };
        // 旧配置/手改配置里可能有解析不了的键名：不能没有热键，
        // 也不能 panic——退回默认键位，用户改回来即可。
        assert_eq!(spec_or_default(&bad), DEFAULT_HOTKEY);

        let good = Hotkey {
            key: "F5".into(),
            ctrl: true,
            shift: false,
            alt: false,
            win: false,
        };
        let spec = spec_or_default(&good);
        assert_eq!(spec.vk, 0x74, "正常配置必须原样注册，不能退默认");
        assert!(spec.ctrl());
    }

    #[test]
    fn failed_message_names_slot_and_combo() {
        // 格式是**契约**：设置界面靠「信息里包含该行当前组合文本」
        // 定位是哪一行注册失败了（paint.rs 的 Hotkey 渲染）。
        let msg = failed_message(Slot::Toggle, DEFAULT_HOTKEY);
        assert!(msg.contains("显示主窗口"), "应含槽位名: {msg}"); // 默认中文环境
        assert!(msg.contains("Win+V"), "应含组合文本: {msg}");
        assert!(msg.contains("占用"), "应说明可能的原因: {msg}");

        let msg2 = failed_message(
            Slot::PastePrevious,
            HotkeySpec::new(MOD_CONTROL, 0x56),
        );
        assert!(msg2.contains("粘贴上一个条目"), "{msg2}"); // 默认中文环境
        assert!(msg2.contains("Ctrl+V"), "{msg2}");
    }
}