//! 界面文案国际化（i18n）。
//!
//! # 设计：内联双语对，多语言走覆盖表
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
//! ## 繁体 / 日语 / 俄语为什么另放 [`catalog`]
//!
//! 把五种语言都写成内联元组，158 个调用点全得改成五元组，以后每动
//! 一条文案就要同时维护五份——漏一份编译期毫无提示，运行期静默显示
//! 错语言。因此**中英仍是唯一的内联源**，另外三种语言在
//! [`catalog`] 里以中文为键查找，缺项回落英文。
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

mod catalog;

use std::sync::atomic::{AtomicU8, Ordering};

/// 一条文案的 (中文, English) 对。
///
/// 多语言（繁中/日/俄）不在元组里，见 [`catalog`]。
pub type Bi = (&'static str, &'static str);

/// 界面语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    /// 简体中文（默认）。
    ZhCn,
    /// 繁体中文。
    ZhTw,
    /// 英文（非中文语系的兜底）。
    EnUs,
    /// 日语。
    Ja,
    /// 俄语。
    Ru,
}

impl Locale {
    /// 该语言对应的配置写法（设置项 `ui.language` 的落值）。
    pub fn as_str(self) -> &'static str {
        match self {
            Locale::ZhCn => "zh-CN",
            Locale::ZhTw => "zh-TW",
            Locale::EnUs => "en-US",
            Locale::Ja => "ja-JP",
            Locale::Ru => "ru-RU",
        }
    }

    /// 在覆盖表里查中文键对应的译文；找不到返回 `None`。
    ///
    /// 只有繁中/日/俄三种语言走这条路径——中英由 [`Self::pick`] 直接
    /// 从内联元组取，不进表。
    fn overlay(self, zh: &str) -> Option<&'static str> {
        let table = match self {
            Locale::ZhTw => catalog::ZH_TW,
            Locale::Ja => catalog::JA,
            Locale::Ru => catalog::RU,
            Locale::ZhCn | Locale::EnUs => return None,
        };
        // 线性查找：表只有百来条，且只在「切到这三种语言」时才走。
        // 上二分查找要为几十个条目引入排序不变量，收益不抵复杂度。
        table
            .iter()
            .find(|(k, _)| *k == zh)
            .map(|(_, v)| *v)
    }

    /// 从语言对里取当前语言的那一条。
    ///
    /// 繁中/日/俄查覆盖表，**查不到回落英文**——宁可显示英文也不要
    /// 显示第三种语言混搭，更不要显示中文（用户已经明确选了别的语言）。
    #[inline]
    pub fn pick(self, bi: Bi) -> &'static str {
        match self {
            Locale::ZhCn => bi.0,
            Locale::EnUs => bi.1,
            other => other.overlay(bi.0).unwrap_or(bi.1),
        }
    }
}

/// 配置文件里 `ui.language` 的「跟随系统」取值。
pub const LANG_SYSTEM: &str = "system";

const LOCALE_ZH: u8 = 0;
const LOCALE_ZH_TW: u8 = 1;
const LOCALE_EN: u8 = 2;
const LOCALE_JA: u8 = 3;
const LOCALE_RU: u8 = 4;

static LOCALE: AtomicU8 = AtomicU8::new(LOCALE_ZH);

/// 设置当前界面语言（设置变更 / 启动解析后调用）。
pub fn set_locale(l: Locale) {
    LOCALE.store(
        match l {
            Locale::ZhCn => LOCALE_ZH,
            Locale::ZhTw => LOCALE_ZH_TW,
            Locale::EnUs => LOCALE_EN,
            Locale::Ja => LOCALE_JA,
            Locale::Ru => LOCALE_RU,
        },
        Ordering::Relaxed,
    );
}

/// 当前界面语言。
pub fn locale() -> Locale {
    match LOCALE.load(Ordering::Relaxed) {
        LOCALE_ZH_TW => Locale::ZhTw,
        LOCALE_EN => Locale::EnUs,
        LOCALE_JA => Locale::Ja,
        LOCALE_RU => Locale::Ru,
        _ => Locale::ZhCn,
    }
}

/// 按当前语言取文案。
#[inline]
pub fn t(bi: Bi) -> &'static str {
    locale().pick(bi)
}

/// 解析配置里的语言字符串（`"system"` / `"zh-CN"` / `"zh-TW"` / `"en-US"` /
/// `"ja"` / `"ru"` 以及各种宽松写法）。
///
/// 解析不出来的一律回中文——本项目的默认语言是中文。
pub fn parse_lang(s: &str) -> Option<Locale> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case(LANG_SYSTEM) {
        return None; // None = 跟随系统
    }
    let lower = s.to_ascii_lowercase();
    // 先判繁体再判简体：`zh-TW` / `zh-HK` / `zh-MO` / `zh-Hant` 走繁体，
    // 其余 `zh*`（含裸 `zh`、`zh-CN`、`zh-SG`）走简体。
    // CAUTION: 顺序不能反——`zh-Hant` 也以 `zh` 开头，先判简体会把它
    // 吞掉。
    if lower.starts_with("zh-tw")
        || lower.starts_with("zh-hk")
        || lower.starts_with("zh-mo")
        || lower.starts_with("zh-hant")
    {
        Some(Locale::ZhTw)
    } else if lower.starts_with("zh") {
        Some(Locale::ZhCn)
    } else if lower.starts_with("en") {
        Some(Locale::EnUs)
    } else if lower.starts_with("ja") {
        Some(Locale::Ja)
    } else if lower.starts_with("ru") {
        Some(Locale::Ru)
    } else {
        // 未知写法视为跟随系统，交给系统语言决定。
        None
    }
}

/// 解析 `ui.language` + 系统语言 → 实际生效语言。
///
/// - 配置显式指定 → 用配置；
/// - `system` 或无法解析 → 跟随系统。
pub fn resolve(cfg_lang: &str, system: Locale) -> Locale {
    parse_lang(cfg_lang).unwrap_or(system)
}

/// 系统语言 → [`Locale`] 的粗映射。
///
/// 收**完整 LANGID**（`GetUserDefaultUILanguage` 的返回值），
/// 主语言在低 10 位（`PRIMARYLANGID`）：
/// - `0x04` LANG_CHINESE ⇒ 再看**子语言**（高 6 位）区分简繁；
/// - `0x11` LANG_JAPANESE ⇒ 日语；
/// - `0x19` LANG_RUSSIAN ⇒ 俄语；
/// - 其余一律英文——比「其它语言全部显示中文」更可接受。
pub fn from_system_langid(langid: u16) -> Locale {
    let primary = langid & 0x03ff;
    if primary == 0x04 {
        // 中文语系：SUBLANGID 在高 6 位（`MAKELANGID(p,s) = (s << 10) | p`）。
        // 权威取值见 `windows` crate 的 SystemServices 常量：
        //   SUBLANG_CHINESE_TRADITIONAL 1 / SIMPLIFIED 2 /
        //   HONGKONG 3 / SINGAPORE 4 / MACAU 5
        // ⇒ 繁体 = {1, 3, 5}，简体 = {2, 4}。
        //
        // CAUTION: 早先这里把 2 也归进繁体（`zh-CN` 于是显示繁体），
        // 是 `system_langid_maps_chinese_japanese_russian_and_falls_back`
        // 抓出来的。子语言编号没有「连续区间」规律，只能逐值列。
        return match langid >> 10 {
            0x01 | 0x03 | 0x05 => Locale::ZhTw,
            _ => Locale::ZhCn,
        };
    }
    match primary {
        0x11 => Locale::Ja,  // LANG_JAPANESE
        0x19 => Locale::Ru,  // LANG_RUSSIAN
        _ => Locale::EnUs,
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
        assert_eq!(parse_lang("ja"), Some(Locale::Ja));
        assert_eq!(parse_lang("ja-JP"), Some(Locale::Ja));
        assert_eq!(parse_lang("ru"), Some(Locale::Ru));
        assert_eq!(parse_lang("ru-RU"), Some(Locale::Ru));
        // 繁体：三种常见写法都要认。
        assert_eq!(parse_lang("zh-TW"), Some(Locale::ZhTw));
        assert_eq!(parse_lang("zh-HK"), Some(Locale::ZhTw));
        assert_eq!(parse_lang("zh-Hant"), Some(Locale::ZhTw));
        // 未知写法不猜，按跟随系统处理。
        assert_eq!(parse_lang("fr-FR"), None);
    }

    #[test]
    fn resolve_prefers_explicit_then_system() {
        assert_eq!(resolve("en-US", Locale::ZhCn), Locale::EnUs);
        assert_eq!(resolve("system", Locale::EnUs), Locale::EnUs);
        assert_eq!(resolve("system", Locale::ZhCn), Locale::ZhCn);
        assert_eq!(resolve("", Locale::EnUs), Locale::EnUs);
        assert_eq!(resolve("zh-TW", Locale::EnUs), Locale::ZhTw);
    }

    /// 简繁不能互相吞：`zh-Hant` 也以 `zh` 开头，必须先判繁体。
    #[test]
    fn traditional_is_not_swallowed_by_simplified_prefix() {
        for s in ["zh-TW", "zh-HK", "zh-MO", "zh-Hant", "ZH-tw"] {
            assert_eq!(
                parse_lang(s),
                Some(Locale::ZhTw),
                "{s} 应解析为繁体，却被简体分支吃掉"
            );
        }
        // 反过来，简体写法不能被判成繁体。
        for s in ["zh", "zh-CN", "zh-SG", "zh-Hans"] {
            assert_eq!(parse_lang(s), Some(Locale::ZhCn), "{s} 应解析为简体");
        }
    }

    #[test]
    fn system_langid_maps_chinese_japanese_russian_and_falls_back() {
        assert_eq!(from_system_langid(0x0804), Locale::ZhCn); // zh-CN
        assert_eq!(from_system_langid(0x0404), Locale::ZhTw); // zh-TW
        assert_eq!(from_system_langid(0x0c04), Locale::ZhTw); // zh-HK
        assert_eq!(from_system_langid(0x1404), Locale::ZhTw); // zh-MO
        assert_eq!(from_system_langid(0x1004), Locale::ZhCn); // zh-SG
        assert_eq!(from_system_langid(0x0409), Locale::EnUs); // en-US
        assert_eq!(from_system_langid(0x0809), Locale::EnUs); // en-GB
        assert_eq!(from_system_langid(0x0411), Locale::Ja); // ja-JP
        assert_eq!(from_system_langid(0x0412), Locale::EnUs); // ko-KR
        assert_eq!(from_system_langid(0x0419), Locale::Ru); // ru-RU
        assert_eq!(from_system_langid(0x040c), Locale::EnUs); // fr-FR
    }

    /// 每个 `Locale` 变体的配置写法都必须能被 `parse_lang` 认回。
    ///
    /// 这是「新增语言却静默不可达」的守门：加了变体、加了下拉框选项，
    /// 但忘了在 `parse_lang` 里认这个写法 ⇒ 配置里存得进、读出来却落到
    /// 「跟随系统」，用户在设置里选的语言重启后失效，且没有任何测试会红。
    #[test]
    fn every_locale_round_trips_through_parse_lang() {
        for l in [
            Locale::ZhCn,
            Locale::ZhTw,
            Locale::EnUs,
            Locale::Ja,
            Locale::Ru,
        ] {
            assert_eq!(
                parse_lang(l.as_str()),
                Some(l),
                "{l:?} 的配置写法 {} 无法被解析回来",
                l.as_str(),
            );
        }
    }

    /// 每种新语言都要真的能从表里取到译文，且与中英不同。
    #[test]
    fn overlay_languages_actually_resolve() {
        let bi = ("置顶", "Pin");
        assert_eq!(Locale::ZhTw.pick(bi), "置頂");
        assert_eq!(Locale::Ja.pick(bi), "ピン留め");
        assert_eq!(Locale::Ru.pick(bi), "Закрепить");
    }

    /// 表里没有的条目回落英文，而不是显示中文或乱码。
    #[test]
    fn missing_overlay_falls_back_to_english() {
        let bi = ("这条不在任何覆盖表里", "Not in any catalog");
        assert_eq!(Locale::ZhTw.pick(bi), "Not in any catalog");
        assert_eq!(Locale::Ja.pick(bi), "Not in any catalog");
        assert_eq!(Locale::Ru.pick(bi), "Not in any catalog");
    }

    /// 占位符必须逐字保留：`{n}` 漏一个就是运行期格式化错位。
    #[test]
    fn catalog_placeholders_match_source() {
        let placeholders = |s: &str| -> Vec<String> {
            let mut v: Vec<String> = Vec::new();
            let b = s.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'{'
                    && let Some(end) = s[i..].find('}')
                {
                    v.push(s[i..i + end + 1].to_string());
                    i += end + 1;
                    continue;
                }
                i += 1;
            }
            v.sort();
            v
        };
        for (name, table) in [
            ("ZH_TW", catalog::ZH_TW),
            ("JA", catalog::JA),
            ("RU", catalog::RU),
        ] {
            for (zh, tr) in table {
                assert_eq!(
                    placeholders(zh),
                    placeholders(tr),
                    "{name} 表里「{zh}」的占位符与译文不一致：{:?} vs {:?}",
                    placeholders(zh),
                    placeholders(tr),
                );
            }
        }
    }

    /// 表内不允许重复键——重复会让前一条永远查不到，属于静默失效。
    #[test]
    fn catalog_has_no_duplicate_keys() {
        for (name, table) in [
            ("ZH_TW", catalog::ZH_TW),
            ("JA", catalog::JA),
            ("RU", catalog::RU),
        ] {
            let mut seen = std::collections::HashSet::new();
            for (zh, _) in table {
                assert!(seen.insert(*zh), "{name} 表里键「{zh}」重复");
            }
        }
    }

    /// 空译文等同于没译——宁可回落英文，也不要显示空字符串。
    #[test]
    fn catalog_has_no_empty_translations() {
        for (name, table) in [
            ("ZH_TW", catalog::ZH_TW),
            ("JA", catalog::JA),
            ("RU", catalog::RU),
        ] {
            for (zh, tr) in table {
                assert!(!tr.trim().is_empty(), "{name} 表里「{zh}」的译文为空");
                assert!(!zh.trim().is_empty(), "{name} 表里出现了空键");
            }
        }
    }
}
