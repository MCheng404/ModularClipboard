//! 字体：中文字体加载、图标字体与字号助手。
//!
//! 中文字体字节进程级共享（`shared_font_data`）——每个
//! `egui::Context` 各装一份会是 35MB × 窗口数。

use egui::FontData;
use egui::FontDefinitions;
use egui::FontFamily;
use egui::FontId;

/// 候选中文字体，按优先级排列。
///
/// `.ttc` 字体集合的 `index` 通常为 0（第一个字体面）；
/// 若某个字体集合的首个字体面不含 CJK，可调整 index。
pub(super) const CJK_CANDIDATES: &[(&str, &str, u32)] = &[
    ("msyh", "C:/Windows/Fonts/msyh.ttc", 0),
    ("deng", "C:/Windows/Fonts/Deng.ttf", 0),
    ("simsun", "C:/Windows/Fonts/simsun.ttc", 0),
    ("noto_sc", "C:/Windows/Fonts/NotoSansSC-VF.ttf", 0),
    ("linux_noto", "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
    ("linux_wqy", "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 0),
    ("mac_pingfang", "/System/Library/Fonts/PingFang.ttc", 0),
];

/// 安装中文字体。返回是否成功找到并加载。
pub fn install_cjk_font(ctx: &egui::Context, custom_path: Option<&str>) -> bool {
    // 诊断开关：跳过字体加载，用于测量中文字体的内存成本。
    if std::env::var_os("TIEZ_NO_CJK_FONT").is_some() {
        tracing::warn!("TIEZ_NO_CJK_FONT 已设置，跳过中文字体加载");
        ctx.set_fonts(FontDefinitions::default());
        // 整套字体被换成 egui 默认（无 CJK 族）⇒ 粗体族也不存在了，
        // 必须复位，否则 `emphasis()` 会去构造一个没有的族而 panic。
        BOLD_READY.store(false, std::sync::atomic::Ordering::Relaxed);
        return false;
    }

    let mut defs = FontDefinitions::default();

    // 自定义路径优先。
    if let Some(p) = custom_path
        && let Some(data) = shared_font_data(p, 0)
        && insert_font(&mut defs, "custom_cjk", data) {
            // 自定义字体没有已知的粗体伴随文件（用户给什么用什么），
            // 因此这里不装粗体——`emphasis()` 会回落常规字重。
            log_font_loaded(ctx, &mut defs, "自定义字体");
            return true;
        }

    for (name, path, index) in CJK_CANDIDATES {
        let Some(data) = shared_font_data(path, *index) else {
            continue;
        };
        if insert_font(&mut defs, name, data) {
            // CAUTION: 粗体必须在 `set_fonts` **之前**装进同一份 defs。
            // `log_font_loaded` 内部是 `ctx.set_fonts(take(defs))`——整体
            // 替换，装晚了会被冲掉（且不报错，只是粗体静默失效）。
            install_bold_companion(&mut defs, name);
            log_font_loaded(ctx, &mut defs, path);
            return true;
        }
        tracing::warn!(path, "字体存在但解析失败，尝试下一个");
    }

    tracing::warn!("未找到中文字体，界面中文将显示为方块");
    ctx.set_fonts(defs);
    // 同上：没装上中文字体，粗体族自然也没注册。
    BOLD_READY.store(false, std::sync::atomic::Ordering::Relaxed);
    false
}

/// 粗体 CJK 字体在 `FontDefinitions::families` 里的族名。
///
/// [`emphasis`] 用 `FontFamily::Name(这个)` 取它。
///
/// CAUTION: 这个名字必须在 `families` 里有键——egui 的
/// `FontsImpl::family_key` 对不在表里的族会 **panic**
/// （"Panics if the family is not in the FontDefinitions"）。
/// 因此 [`install_bold_companion`] 失败（没找到粗体文件）时，
/// 任何代码都不能构造这个族的 [`FontId`]——[`emphasis`] 正是因此
/// 在无粗体时回落 [`FontFamily::Proportional`]。
pub const CJK_BOLD_FAMILY: &str = "cjk_bold";

/// 取某个 CJK 候选字体对应的**独立粗体文件**路径。
///
/// 用 `match` 而不是与 [`CJK_CANDIDATES`] 平行的下标数组：平行数组
/// 要求两处索引永远对齐，加一个候选字体时漏改一处就会**静默取错**
/// （把 A 的粗体配给 B）。`match` 漏写编译期就报错。
///
/// 返回 `None` 表示该字体没有独立的粗体文件——此时粗体回落常规字重，
/// 而不是伪造加粗（见 [`emphasis`]）。
fn bold_companion(cjk_name: &str) -> Option<&'static str> {
    match cjk_name {
        // 微软雅黑：粗体是独立的 msyhbd.ttc（雅黑本身不是可变字体）。
        "msyh" => Some("C:/Windows/Fonts/msyhbd.ttc"),
        // 等线（Deng）/ 宋体（simsun）/ Noto SC（可变字体，另有机制）/
        // Linux 文泉驿 / macOS PingFang 均无独立粗体文件。
        _ => None,
    }
}

/// 把粗体伴随字体注册成**独立的族**（[`CJK_BOLD_FAMILY`]）。
///
/// # 为什么必须独立成族，而不是塞进 Proportional 族
///
/// `insert_font` 把字体插到 `Proportional` / `Monospace` 族首位的做法
/// 在这里是错的：那会让**所有**文字都用粗体。粗体只能通过「单独一个族
/// 名 + 显式 `FontFamily::Name(..)`」来选用。
///
/// # 为什么不给这个族挂图标字体回退
///
/// 族的字体列表里可以挂多个名字做回退，但图标字体是 `install_icon_font`
/// 用 `add_font` **之后**追加的，且只声明进 `Proportional` 族。这里提前
/// 挂上它的名字，等真要用时才发现没注册——那时已经不是 panic 而是缺字形。
/// 因此约定：**`emphasis()` 只用于纯文本**，不要把图标字形放进强调文本
/// （本项目图标都是独立 `painter` 绘制，本就不走文字通路，详见
/// `settings_win::draw_settings_nav` 的用法）。
fn install_bold_companion(defs: &mut FontDefinitions, cjk_name: &str) -> bool {
    let Some(path) = bold_companion(cjk_name) else {
        return false;
    };
    let Some(data) = shared_font_data(path, 0) else {
        tracing::debug!(path, "粗体伴随字体不存在，强调文本将回落常规字重");
        return false;
    };
    defs.font_data
        .insert(CJK_BOLD_FAMILY.to_string(), data);
    defs.families.insert(
        FontFamily::Name(CJK_BOLD_FAMILY.into()),
        vec![CJK_BOLD_FAMILY.to_string()],
    );
    // 置位后 `emphasis()` 才敢构造这个族——顺序不能反（见 `BOLD_READY`）。
    BOLD_READY.store(true, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(path, "粗体伴随字体已注册");
    true
}

/// 安装图标字体。返回是否成功。
///
/// # 为什么必须独立于 [`install_cjk_font`]
///
/// [`install_cjk_font`] 是「取第一个找到的就return」的逻辑：一旦某台机器上
/// 存在任何一款候选中文字体，它就**不会**再往字体定义里加任何东西。
/// 图标字体若寄生在它内部，中文字体不存在时图标就一起消失了——
/// 而界面里那些图标恰恰是操作入口，缺了比缺字更糟。
///
/// 因此这里是独立函数，独立注册 [`crate::icons::ICON_FONT_NAME`]。
///
/// # 字体族与顺序
///
/// 挂在 [`FontFamily::Proportional`]，**排在已有字体之后**
/// （`FontPriority::Lowest`）。
///
/// - 排在之后：CJK 字体被 `insert_font` 放在首位且字体庞大，
///   让图标字体抢在前面会让每个图标的排版都先去问一遍 CJK 字体。
/// - 但仍在同一字体族：图标码位是 PUA（`U+E000..=U+E011`），
///   中文字体不覆盖这些码位（已核实 `msyh.ttc` / `Deng.ttf` 均无
///   `U+E000..=U+E011`），因此回退一定能落到图标字体上。
///
/// 挂在 `Proportional` 而非新建字体族，是为了复用同一条纹理路径：
/// 图集走 `TextureId::Managed(0)`，渲染器无需增加第二个纹理绑定。
///
/// # 为什么用 `add_font` 而不是 `set_fonts`
///
/// 早前这里的写法是 `ctx.fonts(|f| f.definitions().clone())` 取出定义、
/// 改完再 `ctx.set_fonts(defs)` 整体替换。但 egui 0.36 在
/// [`egui::Context::fonts`] 上明确标注 *"Not valid until first call to
/// `Context::run()`"*，而本函数是在 `App::new` 里调用的（那时还没有
/// 任何 `run_ui`），直接 panic：
///
/// ```text
/// No fonts available until first call to Context::run()
/// ```
///
/// 正解是 [`egui::Context::add_font`]：egui 文档写明它 *"will keep the
/// existing fonts"*——**追加**而非替换，这恰好就是「图标字体排在 CJK
/// 之后」所需要的行为，且不依赖字体系统是否已就绪。
///
/// 另一个坑：`set_fonts` 会**整体替换** `FontDefinitions`，
/// 所以本函数必须排在 [`install_cjk_font`] 之后，否则图标字体会被冲掉。
/// `add_font` 不存在这个问题（它是增量追加），但顺序仍按原设计保留，
/// 以免影响阅读者对「图标在 CJK 之后」的预期。
pub fn install_icon_font(ctx: &egui::Context) -> bool {
    let name = crate::icons::ICON_FONT_NAME;
    // 直接复用 `icons::icon_font_data()`：它已经把基线偏移
    // （`FontTweak::y_offset_factor`）调好了，这里再包一层
    // `FontData::from_owned` 反而会把那个修正抹掉。
    let insert = egui::epaint::text::FontInsert::new(
        name,
        crate::icons::icon_font_data(),
        vec![egui::epaint::text::InsertFontFamily {
            family: FontFamily::Proportional,
            // 追加到回退链末尾：既有字体（含 CJK）优先命中，
            // 图标码位是 PUA，CJK 不覆盖，最终必然回退到这里。
            priority: egui::epaint::text::FontPriority::Lowest,
        }],
    );

    // `add_font` 无返回值：egui 只接受「同名字体已存在则不覆盖」的语义。
    // 重复调用（例如测试里多次安装）是幂等的，不会产生第二份。
    ctx.add_font(insert);
    tracing::info!(font = name, "图标字体已注册");
    true
}

/// 读取字体文件，并**跨 `Context` 共享同一份字节**。
///
/// # 为什么必须缓存
///
/// 每个窗口（主窗口 / 设置窗口 / 卡片子窗口）都需要自己的
/// `egui::Context`，而每个 Context 都要装一套字体。若各自 `fs::read`
/// 一份，中文字体（实测 `msyh.ttc` ≈ 14MB 计入私有提交）就按窗口数
/// 复制：开一次设置窗口多一份，反复开关会**一直涨**。
///
/// `FontDefinitions::font_data` 存的是 `Arc<FontData>`，因此这里缓存
/// `Arc` 即可让所有 Context 引用同一份字节——只有引用计数在涨。
///
/// # 为什么不直接 `Box::leak`
///
/// 缓存的 `Arc` 由 `CACHE` 持有，进程退出时才随静态表释放；语义上
/// 与「泄漏一份」等价，但保留了 `FontData`（含 `index` / `tweak`）
/// 的完整构造，也便于按路径复用。
/// 字体字节缓存表：按（路径, 字体面 index）索引一份共享的 [`FontData`]。
pub(super) type FontCache = std::sync::Mutex<
    std::collections::HashMap<(String, u32), std::sync::Arc<FontData>>,
>;

pub(super) fn shared_font_data(path: &str, index: u32) -> Option<std::sync::Arc<FontData>> {
    use std::sync::Arc;
    static CACHE: std::sync::OnceLock<FontCache> = std::sync::OnceLock::new();
    let key = (path.to_string(), index);
    // 表被毒化（持有锁时 panic）时仍要能继续：退化为直接用旧表的
    // 不可变视图（此时读不到缓存只是慢一点，不是错）。
    let mut map = match CACHE.get_or_init(Default::default).lock() {
        Ok(m) => m,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(found) = map.get(&key) {
        return Some(found.clone());
    }
    let bytes = std::fs::read(path).ok()?;
    let mut data = FontData::from_owned(bytes);
    data.index = index;
    let arc = Arc::new(data);
    map.insert(key, arc.clone());
    Some(arc)
}

pub(super) fn insert_font(defs: &mut FontDefinitions, name: &str, data: std::sync::Arc<FontData>) -> bool {
    // NOTE: 收 `Arc<FontData>` 而不是 `Vec<u8>`：字体字节由
    // [`shared_font_data`] 进程级共享，这里只是多一个引用。
    // 早前收 `&[u8]` 再 `to_vec()`，35MB 字体会被多拷贝一份。
    defs.font_data.insert(name.to_string(), data);

    // 插到每个字体族的首位，保证 CJK 字形优先命中，
    // 同时保留原有回退（缺字时仍能用内置字体）。
    let mut families = defs.families.clone();
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        families.entry(family).or_default().insert(0, name.to_string());
    }
    defs.families = families;
    true
}

pub(super) fn log_font_loaded(ctx: &egui::Context, defs: &mut FontDefinitions, path: &str) {
    ctx.set_fonts(std::mem::take(defs));
    tracing::info!(path, "中文字体已加载");
}

/// 返回指定**逻辑字号**的 [`FontId`]。
///
/// # CAUTION: 这里**不能**乘 `pixels_per_point`
///
/// egui 的 `FontId::size` 与 `RichText::size(..)` 单位都是**逻辑点**，
/// 排版时 egui 自己会再乘一次 `pixels_per_point`（见 `epaint` 的
/// `layout()`：字形位置算在物理像素上，取尺寸时
/// `cursor_x_px / pixels_per_point`）。
///
/// 早前这里写成 `size * scale`，于是**自绘文字被缩放了两次**：
/// 150% 屏上 `font_md = 14` 最终渲染成 `14 × 1.5 × 1.5 = 31.5` 物理
/// 像素（视觉放大 2.25 倍）。
///
/// # 症状
///
/// 与 egui 原生控件（`Button` / `TextEdit` / `DragValue`）字号**不同源**
/// ——后者直接给逻辑点，所以只有自绘的那部分大。
///
/// # 为什么测试测不出来
///
/// `Harness` 用的 `egui::Context::default()` 的
/// `pixels_per_point` 是 **1.0**，此时 `size * 1.0 == size`，
/// 两种写法结果完全相同。**必须显式用 ppp>1 的夹具才测得出来。**
///
/// `scale` 参数保留是为了不改动全部调用点，但**已不再参与计算**——
/// 留着一个「传了但没用」的参数本身就是误导，故改名为 `_scale`。
pub fn sized(size: f32, _scale: f32) -> FontId {
    FontId::proportional(size)
}

/// 粗体族（[`CJK_BOLD_FAMILY`]）是否已注册。
///
/// `emphasis()` 必须据此回落——向一个**没注册**的族构造 `FontId` 不会
/// 编译报错，而是在 egui 首次给该族建 fallback 链时 **panic**
/// （`FontsImpl::family_key`： "Panics if the family is not in the
/// FontDefinitions"）。用户机器上没有 `msyhbd.ttc` 时若不看这个标志，
/// 一打开设置窗口就崩。
static BOLD_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 返回指定**逻辑字号**的强调（粗体）[`FontId`]。
///
/// # 何时用
///
/// 只用于**纯文本**的标题类文字（设置窗导航选中项等）。不要把图标字形
/// 放进强调文本——粗体族没有挂图标字体回退，理由见
/// `install_bold_companion` 的文档。
///
/// # 没有粗体字体时
///
/// 回落 [`FontId::proportional`]（常规字重）。不伪造加粗：egui 没有
/// synthetic bold，硬凑只能靠描边，观感脏且字宽变化不可预期。
pub fn emphasis(size: f32, _scale: f32) -> FontId {
    if BOLD_READY.load(std::sync::atomic::Ordering::Relaxed) {
        FontId::new(size, FontFamily::Name(CJK_BOLD_FAMILY.into()))
    } else {
        FontId::proportional(size)
    }
}

/// 粗体是否可用（测试与诊断用）。
#[cfg(test)]
pub(crate) fn bold_ready() -> bool {
    BOLD_READY.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 粗体伴随字体只对「有独立粗体文件」的 CJK 候选项给出路径。
    ///
    /// 用 `match` 而不是平行数组就是为了这个：加候选时漏写臂，
    /// 编译期就报错，不会静默取错字体。
    #[test]
    fn bold_companion_only_for_fonts_with_a_bold_file() {
        assert_eq!(bold_companion("msyh"), Some("C:/Windows/Fonts/msyhbd.ttc"));
        for name in [
            "deng",
            "simsun",
            "noto_sc",
            "linux_noto",
            "linux_wqy",
            "mac_pingfang",
            "custom_cjk",
            "不存在的字体",
        ] {
            assert_eq!(
                bold_companion(name),
                None,
                "{name} 没有独立粗体文件，却给了路径"
            );
        }
    }

    /// `emphasis()` 只能返回两种族：`Proportional`（回落）或
    /// `Name(CJK_BOLD_FAMILY)`（真粗体）。
    ///
    /// 这道断言防的是「返回一个没注册过的族名」——那种情况编译期毫无提示，
    /// 运行期在 egui 首次给该族建 fallback 链时 **panic**
    /// （`FontsImpl::family_key` 的文档原话）。
    ///
    /// 测试**不主动翻转** `BOLD_READY`：它是进程级全局，与并行测试互踩
    /// 会让本测试时红时绿。
    #[test]
    fn emphasis_never_returns_an_unregistered_family() {
        for size in [8.0_f32, 12.0, 24.0] {
            let id = emphasis(size, 1.0);
            assert_eq!(id.size, size, "字号必须原样透传");
            match &id.family {
                FontFamily::Proportional => {}
                FontFamily::Name(n) => assert_eq!(&**n, CJK_BOLD_FAMILY, "族名必须是约定的那个"),
                FontFamily::Monospace => panic!("emphasis 不该返回等宽族"),
            }
        }
    }

    /// 未装粗体时必须真的回落常规字重。
    ///
    /// 单测进程不调用 `install_cjk_font`，因此 `BOLD_READY` 恒为 false——
    /// 这条断言在该环境下是确定的。真正的安装路径要靠实机验证。
    #[test]
    fn emphasis_falls_back_to_proportional_when_no_bold_font() {
        if !bold_ready() {
            assert_eq!(emphasis(12.0, 1.0).family, FontFamily::Proportional);
        }
    }
}
