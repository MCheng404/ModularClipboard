//! 界面文案国际化（i18n）。
//!
//! # 设计：内联双语对，不做键名目录
//!
//! 每条文案以 `(中文, English)` 对的形式**写在调用点**，
//! [`t`] 按当前语言取其一：
//!
//! ```ignore
//! painter.text(..., t(("窗口置顶", "Always on top")), ...);
//! ```
//!
//! 为什么不做 `t("settings.always_on_top")` 式的键名目录：
//!
//! 1. 键名只是把「不知道叫什么随便起名」的问题挪了一层——起键名
//!    和起中文名一样会词穷，还多一份键↔文案的对照表要维护；
//! 2. 双语对相邻放置，缺英文、英中语义漂移在代码评审里一眼可见；
//!    键名目录里两种语言隔着几百行，漂移了没人发现；
//! 3. 本项目没有运行时换词条（无语言包热加载），键名带来的间接层
//!    没有回报。
//!
//! # 语言状态
//!
//! 全局单值（[`set_locale`] / [`locale`]），启动时与设置变更时各写一次。
//! UI 是单线程渲染，用原子量只是为了让 `t()` 不用穿引用。
//!
//! # 身份与显示分离
//!
//! 文案会随语言变，**不能**当身份用（搜索过滤、拖拽 id、测试断言）。
//! 需要身份的地方一律用稳定标识（配置字段名 / `key` / 枚举判别式），
//! 显示才走 [`t`]。

use std::sync::atomic::{AtomicU8, Ordering};

/// 一条文案的 (中文, English) 对。
pub type Bi = (&'static str, &'static str);

/// 界面语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    ZhCn,
    EnUs,
}

impl Locale {
    /// 从语言对里取当前语言的那一条。
    #[inline]
    pub fn pick(self, bi: Bi) -> &'static str {
        match self {
            Locale::ZhCn => bi.0,
            Locale::EnUs => bi.1,
        }
    }
}

/// 配置文件里 `ui.language` 的「跟随系统」取值。
pub const LANG_SYSTEM: &str = "system";

const LOCALE_ZH: u8 = 0;
const LOCALE_EN: u8 = 1;

static LOCALE: AtomicU8 = AtomicU8::new(LOCALE_ZH);

/// 设置当前界面语言（设置变更 / 启动解析后调用）。
pub fn set_locale(l: Locale) {
    LOCALE.store(
        match l {
            Locale::ZhCn => LOCALE_ZH,
            Locale::EnUs => LOCALE_EN,
        },
        Ordering::Relaxed,
    );
}

/// 当前界面语言。
pub fn locale() -> Locale {
    match LOCALE.load(Ordering::Relaxed) {
        LOCALE_EN => Locale::EnUs,
        _ => Locale::ZhCn,
    }
}

/// 按当前语言取文案。
#[inline]
pub fn t(bi: Bi) -> &'static str {
    locale().pick(bi)
}

/// 解析配置里的语言字符串（`"system"` / `"zh-CN"` / `"en-US"` /
/// 以及各种宽松写法）。
///
/// 解析不出来的一律回中文——本项目的默认语言是中文，英文是
/// 「跟随系统且系统非中文」时的国际化出口。
pub fn parse_lang(s: &str) -> Option<Locale> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case(LANG_SYSTEM) {
        return None; // None = 跟随系统
    }
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("zh") {
        Some(Locale::ZhCn)
    } else if lower.starts_with("en") {
        Some(Locale::EnUs)
    } else {
        // 未知写法视为跟随系统，交给系统语言决定。
        None
    }
}

/// 解析 `ui.language` + 系统语言 → 实际生效语言。
///
/// - 配置显式指定（zh/en）→ 用配置；
/// - `system` 或无法解析 → 跟随系统（系统非中文一律英文）。
pub fn resolve(cfg_lang: &str, system: Locale) -> Locale {
    parse_lang(cfg_lang).unwrap_or(system)
}

/// 系统语言 → [`Locale`] 的粗映射：中文语系 → 中文，其余 → 英文。
///
/// 收**完整 LANGID**（`GetUserDefaultUILanguage` 的返回值），
/// 主语言在低 10 位（`PRIMARYLANGID`，0x04 = LANG_CHINESE）。
/// 英文是国际化的兜底——比「其它语言全部显示中文」更可接受。
pub fn from_system_langid(langid: u16) -> Locale {
    if langid & 0x03ff == 0x04 {
        Locale::ZhCn
    } else {
        Locale::EnUs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_returns_matching_half() {
        let bi = ("置顶", "Pinned");
        assert_eq!(Locale::ZhCn.pick(bi), "置顶");
        assert_eq!(Locale::EnUs.pick(bi), "Pinned");
    }

    #[test]
    fn set_and_get_roundtrip() {
        set_locale(Locale::EnUs);
        assert_eq!(locale(), Locale::EnUs);
        assert_eq!(t(("中", "EN")), "EN");
        set_locale(Locale::ZhCn);
        assert_eq!(locale(), Locale::ZhCn);
        assert_eq!(t(("中", "EN")), "中");
    }

    #[test]
    fn parse_lang_covers_config_spellings() {
        assert_eq!(parse_lang("system"), None);
        assert_eq!(parse_lang(""), None);
        assert_eq!(parse_lang("zh-CN"), Some(Locale::ZhCn));
        assert_eq!(parse_lang("zh"), Some(Locale::ZhCn));
        assert_eq!(parse_lang("en-US"), Some(Locale::EnUs));
        assert_eq!(parse_lang("EN"), Some(Locale::EnUs));
        // 未知写法不猜，按跟随系统处理。
        assert_eq!(parse_lang("fr-FR"), None);
    }

    #[test]
    fn resolve_prefers_explicit_then_system() {
        assert_eq!(resolve("en-US", Locale::ZhCn), Locale::EnUs);
        assert_eq!(resolve("system", Locale::EnUs), Locale::EnUs);
        assert_eq!(resolve("system", Locale::ZhCn), Locale::ZhCn);
        assert_eq!(resolve("", Locale::EnUs), Locale::EnUs);
    }

    #[test]
    fn chinese_langid_maps_to_chinese_others_to_english() {
        assert_eq!(from_system_langid(0x0804), Locale::ZhCn); // zh-CN
        assert_eq!(from_system_langid(0x0404), Locale::ZhCn); // zh-TW
        assert_eq!(from_system_langid(0x0c04), Locale::ZhCn); // zh-HK
        assert_eq!(from_system_langid(0x0409), Locale::EnUs); // en-US
        assert_eq!(from_system_langid(0x0809), Locale::EnUs); // en-GB
        assert_eq!(from_system_langid(0x0411), Locale::EnUs); // ja-JP
        assert_eq!(from_system_langid(0x0412), Locale::EnUs); // ko-KR
    }
}
