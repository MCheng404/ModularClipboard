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
        return false;
    }

    let mut defs = FontDefinitions::default();

    // 自定义路径优先。
    if let Some(p) = custom_path
        && let Some(data) = shared_font_data(p, 0)
        && insert_font(&mut defs, "custom_cjk", data) {
            log_font_loaded(ctx, &mut defs, "自定义字体");
            return true;
        }

    for (name, path, index) in CJK_CANDIDATES {
        let Some(data) = shared_font_data(path, *index) else {
            continue;
        };
        if insert_font(&mut defs, name, data) {
            log_font_loaded(ctx, &mut defs, path);
            return true;
        }
        tracing::warn!(path, "字体存在但解析失败，尝试下一个");
    }

    tracing::warn!("未找到中文字体，界面中文将显示为方块");
    ctx.set_fonts(defs);
    false
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
/// （[`FontPriority::Lowest`]）。
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
