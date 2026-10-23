//! 界面文案国际化（i18n）。
//!
//! # 语言分层：英文是主语言，中文是本地化之一
//!
//! 每条文案以 `(中文, English)` 对的形式**写在调用点**：
//!
//! ```ignore
//! painter.text(..., t(("窗口置顶", "Always on top")), ...);
//! ```
//!
//! ## 为什么英文是「主语言」
//!
//! 2026-10-23 之前中文是第一元素、也是译文表的查找键。那带来两个问题：
//!
//! 1. **开发者看不懂键**。想给项目加日语的人对着满表中文字面量无从下手，
//!    而中英都认识的人又没必要查表；
//! 2. **中文有大量同形不同义**。「置顶」既是卡片种类（`CardKind::Pinned`）、
//!    也是行操作按钮、还是图标提示，三种意思共用一个键——译文只能三选一，
//!    另外两处静默用错译文。
//!
//! 所以改成：**英文是主语言与查找键**，中文降为「跟随系统且系统是中文」
//! 时的一种本地化。查表走 `Bi.1`（英文），译文表以英文为键
//! （见 [`zh_hant`] / [`ja`] / [`ru`]）。
//!
//! 中文仍保留在调用点，不删：
//! - 它是「系统语言是中文」时唯一的内联源，没有表也能工作；
//! - 中英相邻放置，**语义漂移在代码评审里一眼可见**——这条是原设计的
//!   核心优点，与谁是查找键无关。
//!
//! ## 为什么不做 `t("settings.always_on_top")` 式的键名目录
//!
//! 1. 键名只是把「不知道叫什么随便起名」的问题挪了一层——起键名和起英文
//!    名一样会词穷，还多一份键↔文案的对照表要维护；
//! 2. 双语对相邻放置，缺英文、英中语义漂移在评审里一眼可见；键名目录里
//!    隔着几百行，漂移了没人发现；
//! 3. 本项目没有运行时换词条（无语言包热加载），键名带来的间接层没有回报。
//!
//! # 模块地图
//!
//! | 模块 | 职责 |
//! |---|---|
//! | 本文件（`i18n.rs`） | [`Locale`] / [`t`] / [`parse_lang`] / [`resolve`] / [`from_system_langid`]，以及跨语言的守门测试 |
//! | [`zh_hant`] | 繁体中文译文表（英文 → 繁体） |
//! | [`ja`] | 日语译文表 |
//! | [`ru`] | 俄语译文表 |
//!
//! 每种语言一个文件、自己持有自己的表与不变量测试（占位符 / 重复键 /
//! 空译文 / 译文不能等于英文）。加一门语言 = 加一个文件 + 一个 `Locale`
//! 变体 + 一行 `mod` + `overlay` 的一个臂。
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

mod ja;
mod ru;
mod zh_hant;

use std::sync::atomic::{AtomicU8, Ordering};

/// 一条文案的 (中文, English) 对。
///
/// **查找键是第二个元素（英文）**——见模块文档「为什么英文是主语言」。
pub type Bi = (&'static str, &'static str);

/// 界面语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    /// 简体中文。
    ZhCn,
    /// 繁体中文。
    ZhTw,
    /// 英文——**主语言**，也是所有回落的目标。
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

    /// 在译文表里查**英文键**对应的译文；找不到返回 `None`。
    ///
    /// 只有繁中/日/俄三种语言走这条路径——中英由 [`Self::pick`] 直接从
    /// 内联元组取，不进表。
    fn overlay(self, en: &str) -> Option<&'static str> {
        let table = match self {
            Locale::ZhTw => zh_hant::TABLE,
            Locale::Ja => ja::TABLE,
            Locale::Ru => ru::TABLE,
            Locale::ZhCn | Locale::EnUs => return None,
        };
        // 线性查找：表只有三百来条，且只在「切到这三种语言」时才走。
        // 上二分查找要为几百个条目引入排序不变量，收益不抵复杂度。
        table.iter().find(|(k, _)| *k == en).map(|(_, v)| *v)
    }

    /// 从语言对里取当前语言的那一条。
    ///
    /// - 简体中文 → 元组第一半；
    /// - 英文 → 元组第二半（**主语言**）；
    /// - 繁中/日/俄 → 查表，**查不到回落英文**。宁可显示英文也不要显示
    ///   第三种语言混搭，更不要显示中文（用户已经明确选了别的语言）。
    #[inline]
    pub fn pick(self, bi: Bi) -> &'static str {
        match self {
            Locale::ZhCn => bi.0,
            Locale::EnUs => bi.1,
            other => other.overlay(bi.1).unwrap_or(bi.1),
        }
    }
}

/// 配置文件里 `ui.language` 的「跟随系统」取值。
pub const LANG_SYSTEM: &str = "system";

// CAUTION: 初值是英文（主语言），不是中文。
//
// 这个原子量在 `set_locale` 之前就是生效值——也就是「任何代码在
// `App::run()` 解析配置之前调 `t()`」时看到的语言。那种场合应当得到
// 主语言（英文），而不是某种本地化：主语言是唯一保证人人可读的兜底。
const LOCALE_EN: u8 = 0;
const LOCALE_ZH: u8 = 1;
const LOCALE_ZH_TW: u8 = 2;
const LOCALE_JA: u8 = 3;
const LOCALE_RU: u8 = 4;

static LOCALE: AtomicU8 = AtomicU8::new(LOCALE_EN);

/// 设置当前界面语言（设置变更 / 启动解析后调用）。
pub fn set_locale(l: Locale) {
    LOCALE.store(
        match l {
            Locale::EnUs => LOCALE_EN,
            Locale::ZhCn => LOCALE_ZH,
            Locale::ZhTw => LOCALE_ZH_TW,
            Locale::Ja => LOCALE_JA,
            Locale::Ru => LOCALE_RU,
        },
        Ordering::Relaxed,
    );
}

/// 当前界面语言。
pub fn locale() -> Locale {
    match LOCALE.load(Ordering::Relaxed) {
        LOCALE_ZH => Locale::ZhCn,
        LOCALE_ZH_TW => Locale::ZhTw,
        LOCALE_JA => Locale::Ja,
        LOCALE_RU => Locale::Ru,
        // 未设置 / 非法值一律英文——主语言是唯一安全的兜底。
        _ => Locale::EnUs,
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
/// 解析不出来的一律回 `None`（跟随系统）——**不**在这里猜主语言，
/// 因为「跟随系统」本身就是默认行为，由 [`resolve`] 收口。
pub fn parse_lang(s: &str) -> Option<Locale> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case(LANG_SYSTEM) {
        return None; // None = 跟随系统
    }
    let lower = s.to_ascii_lowercase();
    // 先判繁体再判简体：`zh-TW` / `zh-HK` / `zh-MO` / `zh-Hant` 走繁体，
    // 其余 `zh*`（含裸 `zh`、`zh-CN`、`zh-SG`）走简体。
    // CAUTION: 顺序不能反——`zh-Hant` 也以 `zh` 开头，先判简体会把它吞掉。
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
/// - 其余一律**英文**——主语言，比「其它语言全部显示中文」更可接受。
pub fn from_system_langid(langid: u16) -> Locale {
    let primary = langid & 0x03ff;
    if primary == 0x04 {
        // 中文语系：SUBLANGID 在高 6 位（`MAKELANGID(p,s) = (s << 10) | p`）。
        // 权威取值见 `windows` crate 的 SystemServices 常量：
        //   SUBLANG_CHINESE_TRADITIONAL 1 / SIMPLIFIED 2 /
        //   HONGKONG 3 / SINGAPORE 4 / MACAU 5
        // ⇒ 繁体 = {1, 3, 5}，简体 = {2, 4}。
        //
        // CAUTION: 子语言编号没有「连续区间」规律，只能逐值列。
        // 早先这里把 2 也归进繁体（`zh-CN` 于是显示繁体），
        // 是 `system_langid_maps_chinese_japanese_russian_and_falls_back`
        // 抓出来的。
        return match langid >> 10 {
            0x01 | 0x03 | 0x05 => Locale::ZhTw,
            _ => Locale::ZhCn,
        };
    }
    match primary {
        0x11 => Locale::Ja, // LANG_JAPANESE
        0x19 => Locale::Ru, // LANG_RUSSIAN
        _ => Locale::EnUs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全部译文表（含语言名），供跨语言测试遍历。
    fn all_tables() -> Vec<(&'static str, &'static [(&'static str, &'static str)])> {
        vec![
            ("zh-TW", zh_hant::TABLE),
            ("ja", ja::TABLE),
            ("ru", ru::TABLE),
        ]
    }

    #[test]
    fn pick_returns_matching_half() {
        let bi = ("取消置顶", "Unpin");
        assert_eq!(Locale::ZhCn.pick(bi), "取消置顶");
        assert_eq!(Locale::EnUs.pick(bi), "Unpin");
    }

    #[test]
    fn set_and_get_roundtrip() {
        set_locale(Locale::EnUs);
        assert_eq!(locale(), Locale::EnUs);
        assert_eq!(t(("中", "EN")), "EN");
        set_locale(Locale::ZhCn);
        assert_eq!(locale(), Locale::ZhCn);
        assert_eq!(t(("中", "EN")), "中");
        // 还原主语言，避免影响同进程的其它测试。
        set_locale(Locale::EnUs);
    }

    /// 未显式 `set_locale` 时（以及任何非法原子值）必须是**英文**。
    ///
    /// 主语言是唯一保证人人可读的兜底——中文在这里反而糟糕：一个只看英文
    /// 的开发者会在日志/诊断里看到看不懂的中文。
    #[test]
    fn default_and_garbage_locale_is_english() {
        // 直接往原子里写一个不存在的编码，模拟「未来的版本加了新语言但
        // 这个二进制不认识」。
        LOCALE.store(200, Ordering::Relaxed);
        assert_eq!(locale(), Locale::EnUs);
        set_locale(Locale::EnUs);
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
    /// 「跟随系统」，用户在设置里选的语言重启后失效，且没有测试会红。
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

    /// 每种新语言都要真的能从表里取到译文，且与中英都不同。
    #[test]
    fn overlay_languages_actually_resolve() {
        let bi = ("取消置顶", "Unpin");
        assert_eq!(Locale::ZhTw.pick(bi), "取消置頂");
        assert_eq!(Locale::Ja.pick(bi), "ピン留め解除");
        assert_eq!(Locale::Ru.pick(bi), "Открепить");
    }

    /// 表里没有的条目回落**英文**（主语言），而不是中文或乱码。
    #[test]
    fn missing_overlay_falls_back_to_english() {
        let bi = ("这条不在任何覆盖表里", "Not in any catalog");
        assert_eq!(Locale::ZhTw.pick(bi), "Not in any catalog");
        assert_eq!(Locale::Ja.pick(bi), "Not in any catalog");
        assert_eq!(Locale::Ru.pick(bi), "Not in any catalog");
    }

    /// 非中英语言的 `pick` **绝不能返回中文那一半**。
    ///
    /// 这条挡的是「改键改到一半」或「表里混进中文键」：一旦发生，用户
    /// 切到日语却看到中文——比看到英文糟糕得多（英文至少是主语言，
    /// 切英文的人本来就能读）。回落只能回落到英文。
    ///
    /// # 为什么不要求三张表条数/键集一致
    ///
    /// 曾经有 `all_tables_have_the_same_size` 与 `all_tables_cover_the_same_keys`。
    /// 删掉「译文 == 英文」的无操作条目（单位类 ` ms` / ` MB` 在日语与
    /// 繁体里就等于英文）之后，三张表条数天然不同——而这是**对的**：
    /// 缺译文时 `pick` 的 `unwrap_or(en)` 回落英文，结果与塞一条
    /// `(en, en)` 完全一样。要求条数一致等于要求把冗余条目塞回去。
    #[test]
    fn non_chinese_locales_never_show_the_source_chinese() {
        const SRC: &str = "这里是源码那一半中文";
        for l in [Locale::ZhTw, Locale::Ja, Locale::Ru] {
            // 表里有译文的词条：返回译文，绝不返回源码中文。
            //
            // CAUTION: 判据是「是不是源码那一半」，不是「含不含汉字」。
            // 繁体译文本来就是汉字（「取消置頂」），用 CJK 判定会把它
            // 误报成没翻译——第一版就栽在这里。
            let got = l.pick((SRC, "Unpin"));
            assert_ne!(got, SRC, "{l:?} 返回了源码的中文那一半");
            // 表里没有的词条：回落英文，绝不回落源码中文。
            let got = l.pick((SRC, "NoSuchEntryXyz"));
            assert_eq!(got, "NoSuchEntryXyz", "{l:?} 缺译文时应回落英文");
            assert_ne!(got, SRC, "{l:?} 回落到了源码中文而不是英文");
        }
        // 反向对照：简体中文确实返回源码那一半——证明上面的断言有判别力，
        // 不是恒真。
        assert_eq!(Locale::ZhCn.pick((SRC, "Unpin")), SRC);
    }

    /// 译文键必须是**非空且不含中文**的英文串。
    ///
    /// 这条挡的是「改键改到一半」：若某条的键还是中文，说明它没被
    /// 英文化流程处理到，而那批条目在英文界面下会查不到译文。
    #[test]
    fn catalog_keys_are_english_not_chinese() {
        let cjk = |s: &str| s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
        for (name, t) in all_tables() {
            for (k, _) in t {
                assert!(!cjk(k), "{name} 表的键 {k:?} 含中文——键应为英文");
            }
        }
    }
}
