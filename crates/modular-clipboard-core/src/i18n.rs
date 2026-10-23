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

    /// 查该语言的复数子表：`en_key` 是英文**复数形**（`tp` 的 `other.1`），
    /// `cat` 决定取四形态中的哪一个。
    ///
    /// 没有收录该短语时返回 `None`，由 [`tp`] 回落 [`Self::pick`]——
    /// 不分单复数的语言（中/日）天然走那条路，不用在这里铺空表。
    fn plural_form(self, en_key: &str, cat: Plural) -> Option<&'static str> {
        let table = match self {
            Locale::ZhTw => zh_hant::PLURALS,
            Locale::Ja => ja::PLURALS,
            Locale::Ru => ru::PLURALS,
            Locale::ZhCn | Locale::EnUs => return None,
        };
        table
            .iter()
            .find(|(k, _)| *k == en_key)
            .map(|(_, forms)| forms[cat.index()])
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

/// CLDR 复数类别。整数场景里英文只用 One/Other，俄语用 One/Few/Many
/// （见 <https://www.unicode.org/cldr/charts/latest/supplemental/language_plural_rules.html>）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plural {
    /// 一（en: n==1；ru: 个位 1 且不是 11）。
    One,
    /// 少（ru: 个位 2–4 且不是 12–14）。
    Few,
    /// 多（ru: 其余整数）。
    Many,
    /// 其余（小数等；整数场景里中/日语恒为它）。
    Other,
}

impl Plural {
    /// 在 `PLURALS` 四元组里的下标（`[one, few, many, other]`）。
    pub fn index(self) -> usize {
        match self {
            Plural::One => 0,
            Plural::Few => 1,
            Plural::Many => 2,
            Plural::Other => 3,
        }
    }
}

/// 按**当前语言**给出 `n` 的 CLDR 复数类别。
#[inline]
pub fn plural(n: i64) -> Plural {
    plural_for(locale(), n)
}

/// [`plural`] 的显式语言版（供测试逐语言断言）。
pub fn plural_for(l: Locale, n: i64) -> Plural {
    match l {
        // 中文/日文不分单复数：`1 个文件` / `5 个文件` 同形。
        Locale::ZhCn | Locale::ZhTw | Locale::Ja => Plural::Other,
        // 英文只有 n==1 是单数。
        Locale::EnUs => {
            if n == 1 {
                Plural::One
            } else {
                Plural::Other
            }
        }
        // 俄语三类。整数不产生 Other，所以 Many 承担「其余全部」。
        //
        // 取绝对值：CLDR 的规则按数的**绝对值**算（负数的符号不参与
        // 词形选择）。本项目的计数都是非负，这里只是不让负值走进
        // 奇怪的分支。
        Locale::Ru => {
            let n = n.unsigned_abs();
            let (n10, n100) = (n % 10, n % 100);
            if n10 == 1 && n100 != 11 {
                Plural::One
            } else if (2..=4).contains(&n10) && !(12..=14).contains(&n100) {
                Plural::Few
            } else {
                Plural::Many
            }
        }
    }
}

/// 计数短语：`n` 决定形态。
///
/// - `en_one` 是**英文单数**形态（`"day ago"`）——英文是主语言，
///   单数形态本身就是英文，**不进译文表**，也不该进（进了就是「译文
///   等于英文」的无操作条目）。
/// - `other` 是复数/通用形态的 (中文, 英文) 对，查表用它的英文那一半。
///
/// 形态选择与本地化是两步：先按 `n` 选出该显示哪一种**英文形态**，
/// 再本地化。中/日文不分单复数 ⇒ 一律取 `other` 的本地化半；
/// 繁中/日/俄先查 [`Locale::plural_form`]（各语言的 `PLURALS` 子表），
/// 没有再回落普通覆盖表，最后回落英文。
///
/// # 为什么表键是 `other` 的英文而不是每个形态一个键
///
/// 俄语一/少/多三种形态对应英文**同一个**复数形（"files"），给每个
/// 形态一个键必然撞键。所以复数子表按「英文复数形」聚合，值是该语言
/// 四个 CLDR 形态的数组。
#[inline]
pub fn tp(n: i64, en_one: &'static str, other: Bi) -> &'static str {
    tp_for(locale(), n, en_one, other)
}

/// [`tp`] 的显式语言版。**纯函数**——测试用它逐语言断言，不必去翻转
/// 进程级的 [`set_locale`]（那是全局原子量，并行测试会互相踩）。
pub fn tp_for(l: Locale, n: i64, en_one: &'static str, other: Bi) -> &'static str {
    match l {
        Locale::ZhCn => other.0,
        Locale::EnUs => {
            if plural_for(l, n) == Plural::One {
                en_one
            } else {
                other.1
            }
        }
        _ => {
            let cat = plural_for(l, n);
            l.plural_form(other.1, cat).unwrap_or_else(|| l.pick(other))
        }
    }
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

    // ---- CLDR 复数选形 ----

    /// 俄语三类的边界必须逐个钉住。
    ///
    /// CLDR 俄语按**个位**与**十位**联合判断：`21` 与 `11` 同个位却分属
    /// 「一」与「多」。这种边界写错不编译报错，只会让俄语用户看到
    /// `21 файлов`（应为 `21 файл`）。
    #[test]
    fn russian_plural_categories() {
        use Plural::{Few, Many, One};
        for (n, want) in [
            (0, Many),
            (1, One),
            (2, Few),
            (3, Few),
            (4, Few),
            (5, Many),
            (11, Many), // 11 不是「一」
            (12, Many),
            (14, Many),
            (21, One), // 21 回到「一」
            (22, Few),
            (25, Many),
            (101, One),
            (102, Few),
            (111, Many), // 111 的十位仍是 11
            (1000, Many),
        ] {
            assert_eq!(plural_for(Locale::Ru, n), want, "n={n}");
        }
    }

    #[test]
    fn english_plural_categories() {
        use Plural::{One, Other};
        assert_eq!(plural_for(Locale::EnUs, 1), One);
        assert_eq!(plural_for(Locale::EnUs, 0), Other);
        assert_eq!(plural_for(Locale::EnUs, 2), Other);
        assert_eq!(plural_for(Locale::EnUs, 11), Other);
    }

    /// 中/日文不分单复数：任何 n 都是 Other，`tp` 因此恒取通用形。
    #[test]
    fn chinese_and_japanese_have_a_single_form() {
        for l in [Locale::ZhCn, Locale::ZhTw, Locale::Ja] {
            for n in [0_i64, 1, 2, 5, 11, 21, 100] {
                assert_eq!(plural_for(l, n), Plural::Other, "{l:?} n={n}");
            }
        }
    }

    /// 计数短语的端到端选形。**不**翻转全局语言——那是进程级原子量，
    /// 并行测试会互踩（`BOLD_READY` 踩过的坑）。[`tp_for`] 是纯函数。
    #[test]
    fn tp_selects_the_right_form_per_language() {
        // 用「hours ago」这条真实词条：它在 ru::PLURALS 里有四形态。
        let phrase = ("小时前", "hours ago");
        // 英文：单数走 en_one，复数走 other 的英文半。
        assert_eq!(tp_for(Locale::EnUs, 1, "hour ago", phrase), "hour ago");
        assert_eq!(tp_for(Locale::EnUs, 2, "hour ago", phrase), "hours ago");
        assert_eq!(tp_for(Locale::EnUs, 0, "hour ago", phrase), "hours ago");
        // 简体中文：一律取中文那一半。
        assert_eq!(tp_for(Locale::ZhCn, 1, "hour ago", phrase), "小时前");
        assert_eq!(tp_for(Locale::ZhCn, 5, "hour ago", phrase), "小时前");
        // 俄语：CLDR 三形态各自不同。
        assert_eq!(tp_for(Locale::Ru, 1, "hour ago", phrase), "час назад");
        assert_eq!(tp_for(Locale::Ru, 2, "hour ago", phrase), "часа назад");
        assert_eq!(tp_for(Locale::Ru, 5, "hour ago", phrase), "часов назад");
        assert_eq!(tp_for(Locale::Ru, 21, "hour ago", phrase), "час назад");
        // 繁中/日语：复数子表是空的 ⇒ 回落普通覆盖表，与 `pick` 一致。
        assert_eq!(tp_for(Locale::ZhTw, 7, "hour ago", phrase), Locale::ZhTw.pick(phrase));
        assert_eq!(tp_for(Locale::Ja, 7, "hour ago", phrase), Locale::Ja.pick(phrase));
    }

    /// 表里没有的计数短语必须回落 `pick`（通用形），不能崩、不能显示空串。
    #[test]
    fn tp_falls_back_when_a_phrase_is_not_in_the_plural_table() {
        let phrase = ("某条", "NoSuchPluralPhraseXyz");
        // 俄语：PLURALS 无此键 ⇒ 走 `pick` ⇒ 表里也没有 ⇒ 回落英文。
        assert_eq!(tp_for(Locale::Ru, 2, "NoSuchPluralPhraseXyz", phrase), "NoSuchPluralPhraseXyz");
        // 英文单复数仍然正确——回落不影响主语言。
        assert_eq!(tp_for(Locale::EnUs, 1, "Xyz", phrase), "Xyz");
        assert_eq!(tp_for(Locale::EnUs, 3, "Xyz", phrase), "NoSuchPluralPhraseXyz");
    }

    /// 复数子表的不变量：四形态都非空，且键必须在 [`TABLE`] 里。
    ///
    /// 后者是 `tp` 的回落链要求的：PLURALS 没覆盖到某个类别时走
    /// `pick`，若 TABLE 也没有这个键就只剩英文——对俄语用户是静默缺译。
    #[test]
    fn plural_tables_are_well_formed() {
        for (name, plurals, table) in [
            ("zh-TW", zh_hant::PLURALS, zh_hant::TABLE),
            ("ja", ja::PLURALS, ja::TABLE),
            ("ru", ru::PLURALS, ru::TABLE),
        ] {
            for (key, forms) in plurals {
                for (i, f) in forms.iter().enumerate() {
                    assert!(!f.trim().is_empty(), "{name} 的 {key:?} 第 {i} 形态为空");
                }
                assert!(
                    table.iter().any(|(k, _)| k == key),
                    "{name} 的复数键 {key:?} 不在 TABLE 里——`tp` 的回落拿不到通用形"
                );
            }
        }
    }

    // ---- 源码 ↔ 译文表覆盖守门 ----
    //
    // 抽取/核验脚本是一次性的，用完即删。没有这道测试时，加一条新文案
    // 或改一个英文措辞，译文表就静默缺条，直到有人把界面切到日语才发现
    // 那一段是英文。这里把「覆盖」从「记得跑脚本」变成 `cargo test` 的一部分。

    /// 剥掉注释与字符字面量，只留可扫描的代码与字符串字面量。
    ///
    /// CAUTION: 字符串必须在注释**之前**判——`"https://…"` 里的 `//`
    /// 不是注释。反过来会把大半行吃掉，进而误判「该文案已不存在」而漏查。
    fn strip_noise(src: &str) -> String {
        let v: Vec<char> = src.chars().collect();
        let mut out = String::with_capacity(src.len());
        let mut i = 0;
        while i < v.len() {
            // 字符字面量：`'x'` / `'\n'`。生命周期 `'a` 不是字面量，原样放过。
            if v[i] == '\'' {
                let mut j = i + 1;
                if j < v.len() && v[j] == '\\' {
                    j += 1;
                }
                if j < v.len() {
                    j += 1;
                }
                if j < v.len() && v[j] == '\'' {
                    for c in &v[i..=j] {
                        out.push(*c);
                    }
                    i = j + 1;
                } else {
                    out.push(v[i]);
                    i += 1;
                }
                continue;
            }
            // 字符串字面量：原样保留（含转义），内部的 `//` 与 `/*` 不当注释。
            if v[i] == '"' {
                out.push('"');
                i += 1;
                while i < v.len() {
                    if v[i] == '\\' {
                        out.push('\\');
                        i += 1;
                        if i < v.len() {
                            out.push(v[i]);
                            i += 1;
                        }
                        continue;
                    }
                    let c = v[i];
                    out.push(c);
                    i += 1;
                    if c == '"' {
                        break;
                    }
                }
                continue;
            }
            // 行注释
            if v[i] == '/' && i + 1 < v.len() && v[i + 1] == '/' {
                while i < v.len() && v[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            // 块注释（Rust 允许嵌套）
            if v[i] == '/' && i + 1 < v.len() && v[i + 1] == '*' {
                let mut depth = 1usize;
                i += 2;
                while i < v.len() && depth > 0 {
                    if v[i] == '/' && i + 1 < v.len() && v[i + 1] == '*' {
                        depth += 1;
                        i += 2;
                    } else if v[i] == '*' && i + 1 < v.len() && v[i + 1] == '/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            out.push(v[i]);
            i += 1;
        }
        out
    }

    /// 砍掉 `mod tests { … }` 整块（含 `#[cfg(test)]`）。
    /// 测试里的假词条（`NoSuchEntryXyz` 之类）不是界面文案，不该被查覆盖。
    fn strip_test_modules(src: &str) -> String {
        let mut out = String::from(src);
        while let Some(pos) = out.find("mod tests") {
            // 花括号配对砍掉整个模块体；找不到 `{` 就只删标记本身。
            let Some(brace) = out[pos..].find('{') else {
                out.replace_range(pos..pos + "mod tests".len(), "         ");
                continue;
            };
            let start = pos + brace;
            let mut depth = 0usize;
            let mut end = start;
            let bytes: Vec<char> = out.chars().collect();
            // 用字符下标重新定位 `start` 在字符向量里的位置。
            let start_idx = out[..start].chars().count();
            for (k, c) in bytes.iter().enumerate().skip(start_idx) {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = out
                                .char_indices()
                                .nth(k)
                                .map(|(b, _)| b + c.len_utf8())
                                .unwrap_or(out.len());
                            break;
                        }
                    }
                    _ => {}
                }
            }
            out.replace_range(pos..end, " ");
        }
        out
    }

    /// 从（已剥注释/测试的）源码里提取 `(中文, 英文)` 二元组字面量。
    ///
    /// 只认 `( "…" , "…" ,? )` 这一种形状：排除 `("a", "b", "c")` 三元组
    /// 与函数调用的 `f("a", &other)`——后者第二个参数不是字面量。
    fn bi_pairs(clean: &str) -> Vec<(String, String)> {
        fn skip_ws(v: &[char], mut i: usize) -> usize {
            while i < v.len() && v[i].is_whitespace() {
                i += 1;
            }
            i
        }
        /// 解析 `"…"`（含转义），返回 (内容, 闭引号之后的下标)。
        fn parse_string(v: &[char], i: usize) -> Option<(String, usize)> {
            if i >= v.len() || v[i] != '"' {
                return None;
            }
            let mut s = String::new();
            let mut j = i + 1;
            while j < v.len() {
                if v[j] == '\\' {
                    j += 1;
                    if j >= v.len() {
                        return None;
                    }
                    s.push(match v[j] {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                    j += 1;
                    continue;
                }
                if v[j] == '"' {
                    return Some((s, j + 1));
                }
                s.push(v[j]);
                j += 1;
            }
            None
        }

        let v: Vec<char> = clean.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < v.len() {
            if v[i] != '(' {
                i += 1;
                continue;
            }
            // CAUTION: `(` 前面若紧挨标识符字符，那是**函数/方法调用**的
            // 参数表（`s.replace("&lt;", "<")`），不是元组字面量。不挡的话，
            // `.replace()` 调用链里的相邻两个字符串会被配成假二元组——
            // HTML 实体表（`"&lt;", "<"`）就是这么被误报成待译文案的。
            if i > 0 && (v[i - 1].is_alphanumeric() || v[i - 1] == '_' || v[i - 1] == ':' || v[i - 1] == '>' || v[i - 1] == '.') {
                i += 1;
                continue;
            }
            let mut j = skip_ws(&v, i + 1);
            let Some((a, after_a)) = parse_string(&v, j) else {
                i += 1;
                continue;
            };
            j = skip_ws(&v, after_a);
            if j >= v.len() || v[j] != ',' {
                i += 1;
                continue;
            }
            j = skip_ws(&v, j + 1);
            let Some((b, after_b)) = parse_string(&v, j) else {
                i += 1;
                continue;
            };
            j = skip_ws(&v, after_b);
            if j < v.len() && v[j] == ',' {
                j = skip_ws(&v, j + 1);
            }
            if j < v.len() && v[j] == ')' {
                // CAUTION: `(字符串, 字符串)` 元组不都是文案——egui 的 Id
                // 元组（`("icon_btn_hover", "topbar_clear")`）、关于页的
                // (名称, 链接) 表都是。判据两条满足其一：
                //
                // 1. 首半含 CJK——[`Bi`] 的定义就是 (中文, English)；
                // 2. 两半全同——自称词（`("日本語", "日本語")`）与不变单位
                //    （`(" MB", " MB")`）的中文半就是符号本身，不含 CJK，
                //    但它们是真文案（俄语要写 `МБ`）。
                //
                // 另外英文半是 URL 的不收——链接里的字符不能动。
                let cjk = |s: &str| s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
                let is_url = b.starts_with("http://") || b.starts_with("https://");
                if (cjk(&a) || a == b) && !is_url {
                    out.push((a, b));
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
        out
    }

    /// 递归收集 `*.rs`。跳过 i18n 表目录（表自身就是 `(en, 译文)` 对，
    /// 扫它等于自己证明自己）与集成测试目录（那是断言界面文案的地方）。
    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name == "target" || name == "tests" || name == "i18n" {
                continue;
            }
            if p.is_dir() {
                collect_rs(&p, out);
            } else if name.ends_with(".rs") && name != "i18n.rs" {
                out.push(p);
            }
        }
    }

    /// 扫全部源码拿到的 `(中文, 英文)` 对，按英文键去重。
    fn source_pairs() -> Vec<(String, String)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        let mut files = Vec::new();
        collect_rs(&root.join("crates"), &mut files);
        assert!(
            files.len() > 50,
            "只扫到 {} 个源码文件，路径可能不对（root = {}）",
            files.len(),
            root.display()
        );
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for f in &files {
            let src = std::fs::read_to_string(f).unwrap_or_default();
            let clean = strip_test_modules(&strip_noise(&src));
            for pair in bi_pairs(&clean) {
                if seen.insert(pair.1.clone()) {
                    out.push(pair);
                }
            }
        }
        out
    }

    /// 源码里的每条用户可见文案，三张表都必须有译文（自称词除外）。
    ///
    /// # 自称词豁免
    ///
    /// `zh == en` 的二元组是**自称词**（`("日本語", "日本語")`）：任何语言下
    /// 显示的都是它自己，回落英文即得正确结果，进表反而是「译文 == 英文」
    /// 的无操作条目（2026-10-23 刚清掉一批）。
    #[test]
    fn every_source_string_is_translated() {
        let mut missing: Vec<String> = Vec::new();
        for (zh, en) in source_pairs() {
            if zh == en {
                continue;
            }
            for (name, t) in all_tables() {
                if !t.iter().any(|(k, _)| *k == en) {
                    missing.push(format!("  [{name}] 缺 {en:?}（源码中文 {zh:?}）"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "译文表覆盖不全，共 {} 处（加文案时要同时补三张表）:\n{}",
            missing.len(),
            missing.join("\n")
        );
    }

    /// 表里不许有源码里已经不存在的条目。
    ///
    /// 它抓的是**改英文措辞**：键就是英文，改一个词就等于换了键——旧键
    /// 留在表里是垃圾，新键没人译。两侧都查才能把这种改动暴露出来。
    #[test]
    fn no_orphan_translations() {
        let src: std::collections::BTreeSet<String> =
            source_pairs().into_iter().map(|(_, en)| en).collect();
        let mut orphans: Vec<String> = Vec::new();
        for (name, t) in all_tables() {
            for (k, _) in t {
                if !src.contains(*k) {
                    orphans.push(format!("  [{name}] {k:?}"));
                }
            }
        }
        // 复数子表的键也必须还在源码里——同理会随英文措辞改动而失效。
        for (name, plurals, _) in [
            ("zh-TW", zh_hant::PLURALS, zh_hant::TABLE),
            ("ja", ja::PLURALS, ja::TABLE),
            ("ru", ru::PLURALS, ru::TABLE),
        ] {
            for (k, _) in plurals {
                if !src.contains(*k) {
                    orphans.push(format!("  [{name}/PLURALS] {k:?}"));
                }
            }
        }
        assert!(
            orphans.is_empty(),
            "表里有源码已不用的条目（多半是英文措辞改了没同步）:\n{}",
            orphans.join("\n")
        );
    }
}
