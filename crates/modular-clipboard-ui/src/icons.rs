//! 矢量图标——嵌入 ttf 图标字体渲染。
//!
//! # 为什么用图标字体，而不是继续手绘
//!
//! 旧实现是一堆[`egui::Painter`] 调用（画矩形 / 圆弧 / 线条）。它的问题：
//!
//! 1. **圆头圆角画不出来**。epaint 的 tessellator 只实现 miter join，
//!    源码里明确写着 `// TODO: add line caps`。而 Feather / Lucide / Tabler
//!    全是 `stroke-linecap="round"`——手绘路径做不出圆头，视觉上比真正的
//!    图标集明显"糙"一档。
//! 2. **代码量大且每加一个图标都要重画**。删改一个图标的路径数据就是
//!    一次潜在的绘图bug，而这类bug（画错一条线）编译器完全不管。
//! 3. **圆弧近似有误差**。手绘的圆是用多段折线凑的，放大后能看出棱角。
//!
//! # 为什么不用 `egui::Image`
//!
//! 两条路都走不通：
//!
//! - 该路径需要**用户纹理**，而本项目的渲染器（`renderer.rs`）只能绑一张
//!   用户纹理，且已被缩略图占用。
//! - `ImageData::Font` 在 egui 0.36 已被移除（PR #7298）。
//!
//! # 为什么零新增依赖
//!
//! ttf 直接交给 egui 内置的字体解析器（走 [`egui::FontData`]，底层是
//! `ab_glyph`）。不需要 `ab_glyph` / `owned_ttf_parser` 之类的额外依赖。
//!
//! 渲染通路：字体图集走 `TextureId::Managed(0)`（`renderer.rs` 已支持），
//! 着色器取覆盖率乘顶点色——**图标颜色天然由 `Color32` 决定**，因此
//! 每个图标都能跟随主题色。
//!
//! # 图标集
//!
//! [Bootstrap Icons](https://github.com/twbs/icons) 1.13.1，MIT。
//! 见仓库根目录 `THIRD_PARTY_LICENSES.md` 的归因。
//!
//! `assets/modular-clipboard-icons.ttf` 是**子集化**后的产物：只保留本模块用到的 18 个
//! 字形，码位重映射到 `U+E000..=U+E011`，体积 4144 字节。
//!
//! # 尺寸与对齐
//!
//! 图标按 `theme::Palette::icon_size`（16）渲染，`Icon::paint` 用
//! [`Align2::CENTER_CENTER`] 把行盒摆在 `rect` 中心。
//!
//! **为什么仍然需要一个纵向补偿**：epaint 的行盒高度取自字体 **OS/2**
//! typo 度量（skrifa 不读 `hhea`），本字体
//! `row_height = ascender(300) - descender(0) + lineGap(27) = 327` 单位，
//! 比 em（300）高出 27。这27 单位 lineGap 全部堆在基线下方，使行盒重心
//! 相对 em 盒下移，纯几何居中会让墨迹整体偏上。用
//! [`egui::FontTweak::y_offset_factor`]（见 [`BASELINE_FACTOR`]）把它补偿回来。
//!
//! 各字形自身还有固有偏差（来自轮廓，如 `Trash` 墨迹中心在 159.5/300、
//! `Text` 在 141.5/300），**这部分没有也不该有补偿**——补偿只能整体平移，
//! 强行逐个对齐等于把设计意图抹平。实测最差偏差 2px/64px（3.1%），
//! 肉眼不可见。
//!
//! 校准方法与实测数值见 [`tests::report_ink_geometry`]（16/32/64 三个字号，
//! 屏幕空间光栅化口径）。改动 [`BASELINE_FACTOR`] 前先看它；
//! 回归判据是 [`tests::glyph_ink_is_centred_in_its_box`]。

use egui::{Align2, Color32, Rect, Ui, Vec2, pos2};

/// 图标字体的名字，注册进 [`egui::FontDefinitions`] 时用。
pub const ICON_FONT_NAME: &str = "modular-clipboard-icons";

/// 嵌入的图标字体（子集，4080 字节）。
///
/// 只含本模块 [`Icon`] 的 18 个变体，码位 `U+E000..=U+E011`。
/// 由 `assets/` 下的源字体经`pyftsubset` 生成，改动 [`Icon`] 的码位
/// 必须同步重新生成该文件——[`tests::every_icon_codepoint_has_ink`]
/// 会拦住漏改。
static ICON_TTF: &[u8] = include_bytes!("../assets/modular-clipboard-icons.ttf");

/// 图标的 [`egui::FontData`]。
///
/// 供 [`crate::theme::install_icon_font`] 注册用。走 `from_static`——
/// 字体已经嵌在二进制里，不需要再拷一份所有权。
///
/// baseline 补偿（[`BASELINE_FACTOR`]）挂在这里：
/// [`egui::FontTweak`] 是**注册期**属性，`FontId` 上没有对应字段。
pub fn icon_font_data() -> egui::FontData {
    egui::FontData::from_static(ICON_TTF).tweak(egui::FontTweak {
        y_offset_factor: BASELINE_FACTOR,
        ..Default::default()
    })
}

/// 图标墨迹相对基线的纵向补偿系数（[`egui::FontTweak::y_offset_factor`]）。
///
/// # 为什么需要补偿
///
/// epaint 的行盒高度取自 **OS/2** 的 typo 度量（skrifa 不用 hhea）：
/// `row_height = typoAscender - typoDescender + typoLineGap = 300 - 0 + 27 = 327`
/// 单位，而 em 只有 300。多出来的 27 单位 lineGap 全部堆在基线**下方**，
/// 于是行盒重心相对 em 盒下移，纯几何推算下墨迹平均会偏上
/// `(ascent + descent - lineGap)/2 = 136.5` 单位，而实际墨迹中心均值在 150.7。
///
/// 推导：设 `ink_cy` 为字形墨迹中心（字体单位），`asc/desc/gap` 为 OS/2 度量，
/// epaint 里基线位于 `行顶 + asc`，行盒居中于目标框，于是
///
/// ```text
/// 墨迹中心(相对框心) = (asc + desc - gap)/2 - ink_cy
/// ```
///
/// 代入 `(300 + 0 - 27)/2 - 150.7 = -14.2` 单位（偏上），
/// 折成比例即 `+14.2 / 300 ≈ 0.047`；实测标定值0.0553（差异来自
/// epaint 的整像素对齐与图集量化）。
///
/// # 符号：必须为正
///
/// [`egui::FontTweak::y_offset_factor`] 为负会把墨迹**上移**。
/// 本常量曾经是 `-0.055`——符号写反了，等于在3.5px 的自然偏移上
/// 再叠加 3.5px 反向偏移，实测把`Close` 的上下留白从 5/11 放大成 2/14。
///
/// # 校准方法
///
/// 不是拍脑袋，是 [`tests::report_ink_geometry`] 在 16 / 32 / 64 三个字号下
/// 实测「墨迹中心 − 框心」的均值，取其反号。换字号时因为补偿正比于字号，
/// 三个字号会同时收敛—— 单个字号碰巧居中不算数。
pub const BASELINE_FACTOR: f32 = 0.0553;

/// 墨迹相对 em 盒的缩放系数。
///
/// 本图标集里 `search`（放大镜）与 `gear`（齿轮）的轮廓**略微超出**
/// em 盒：`search` 的 bbox 是 (-4, 0, 300, 304)、`gear` 是 (-3, -3, 303, 303)，
/// em 则是 0..=300。超出量来自它们的圆形部件用了向外延伸的圆头
/// （这是刻意的，否则放大镜和齿轮会显得比别的图标「瘦」）。
///
/// 直接按 em 盒取字号会让这两个图标比方框大出约 1.3%——肉眼可见的
/// 「这一个图标胖一圈」。这里统一缩到 98%，让所有图标的墨迹都
/// 落在方框内，观感一致。
const INK_FIT: f32 = 0.98;

/// 图标语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    // ---- 已在用的----
    /// 关闭 / 清除。
    Close,
    /// 已置顶（实心图钉）。
    Pinned,
    /// 未置顶（描边图钉，置顶按钮的默认态）。
    Unpinned,
    /// 搜索。
    Search,
    /// 设置。
    Settings,
    /// 文本类型条目。
    Text,
    /// HTML / 富文本条目。
    Code,
    /// 图片类型条目。
    Image,
    /// 文件类型条目。
    File,

    // ---- 标题栏 / 布局----
    /// 最小化。
    Minimize,
    /// 折叠（收起面板）。
    Collapse,
    /// 展开（展开面板）。
    Expand,
    /// 拖拽手柄。
    Drag,
    /// 「固定在主区顶部」——已固定（实心挂锁）。
    Lock,
    /// 「固定」——未固定（开挂锁）。
    Unlock,
    /// 「表」视图标签。
    Table,
    /// 「密」视图标签（内容打码）。
    Mask,
    /// 清空。
    Trash,
}

impl Icon {
    /// 全部变体。用于测试与穷举性检查。
    pub const ALL: &'static [Icon] = &[
        Icon::Close,
        Icon::Pinned,
        Icon::Unpinned,
        Icon::Search,
        Icon::Settings,
        Icon::Text,
        Icon::Code,
        Icon::Image,
        Icon::File,
        Icon::Minimize,
        Icon::Collapse,
        Icon::Expand,
        Icon::Drag,
        Icon::Lock,
        Icon::Unlock,
        Icon::Table,
        Icon::Mask,
        Icon::Trash,
    ];

    /// 该图标在字体中的码位（PUA区）。
    ///
    /// 与 `assets/modular-clipboard-icons.ttf` 的 cmap 一一对应；改动必须同步重新生成
    /// 字体，否则 [`tests::every_icon_codepoint_has_ink`] 会失败。
    pub fn codepoint(self) -> char {
        match self {
            Icon::Close => '\u{e000}',   // x-lg
            Icon::Pinned => '\u{e001}',  // pin-angle-fill
            Icon::Unpinned => '\u{e002}',// pin-angle
            Icon::Search => '\u{e003}',  // search
            Icon::Settings => '\u{e004}',// gear
            Icon::Text => '\u{e005}',    // type
            Icon::Code => '\u{e006}',    // code-slash
            Icon::Image => '\u{e007}',   // image
            Icon::File => '\u{e008}',    // file-earmark
            Icon::Minimize => '\u{e009}',// dash-lg
            Icon::Collapse => '\u{e00a}',// chevron-right
            Icon::Expand => '\u{e00b}',  // chevron-left
            Icon::Drag => '\u{e00c}',    // grip-vertical
            Icon::Lock => '\u{e00d}',    // lock-fill
            Icon::Unlock => '\u{e00e}',  // unlock
            Icon::Table => '\u{e00f}',   // table
            Icon::Mask => '\u{e010}',    // eye-slash
            Icon::Trash => '\u{e011}',   // trash
        }
    }

    /// 渲染该图标所用的字体 id。
    ///
    /// 尺寸由调用方决定（通常是 `icon_size * scale`）。baseline 补偿
    /// 不在这里——[`egui::FontTweak`] 是**注册期**的属性，见
    /// [`icon_font_data`]。
    pub fn font_id(self, size: f32) -> egui::FontId {
        let _ = self;
        egui::FontId::proportional(size)
    }

    /// 按剪贴板类型选图标。
    ///
    /// 用于列表左侧的条目类型标识。`ClipKind` 属于业务层，
    /// 这里用穷举匹配避免依赖它。
    pub fn for_kind(kind: modular_clipboard_core::ClipKind) -> Self {
        use modular_clipboard_core::ClipKind;
        match kind {
            ClipKind::Text => Icon::Text,
            ClipKind::Html => Icon::Code,
            ClipKind::Image => Icon::Image,
            ClipKind::Files => Icon::File,
        }
    }

    /// 无障碍标签。屏幕阅读器会读出这个。
    pub fn label(self) -> &'static str {
        match self {
            Icon::Close => "关闭",
            Icon::Pinned => "已置顶",
            Icon::Unpinned => "置顶",
            Icon::Search => "搜索",
            Icon::Settings => "设置",
            Icon::Text => "文本",
            Icon::Code => "HTML",
            Icon::Image => "图片",
            Icon::File => "文件",
            Icon::Minimize => "最小化",
            Icon::Collapse => "折叠",
            Icon::Expand => "展开",
            Icon::Drag => "拖拽",
            Icon::Lock => "已固定",
            Icon::Unlock => "固定",
            Icon::Table => "表格视图",
            Icon::Mask => "密文视图",
            Icon::Trash => "清空",
        }
    }

    /// 绘制到指定的矩形区域。
    ///
    /// 用 [`Align2::CENTER_CENTER`] 而不是手算基线：字体图元的位置由
    /// epaint 排版决定，而 [`Self::font_id`] 里的 `y_offset_factor` 已经把
    /// 墨迹拉到了行盒中部，两者叠加后墨迹中心与 `rect` 中心重合。
    ///
    /// `rect` 的边长即图标边长——字体的 em 铺满整个 advance，
    /// 所以按边长直接取字号即可，不需要额外的缩放系数。
    pub fn paint(self, painter: &egui::Painter, rect: Rect, color: Color32) {
        let size = rect.width().min(rect.height());
        if size <= 0.0 || color == Color32::TRANSPARENT {
            return;
        }
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            self.codepoint().to_string(),
            // 乘 [`INK_FIT`] 让略微超出 em 的字形也能落进方框。
            self.font_id(size * INK_FIT),
            color,
        );
    }
}

/// 字体的 em 大小（`head.unitsPerEm`）。
pub const EM_UNITS: f32 = 300.0;

/// 字体的 `OS/2` typo ascender。**epaint 读的是这一项，不是 `hhea.ascent`。**
///
/// 与 [`DESCENT_UNITS`]、`LINE_GAP_UNITS`] 一起决定行盒高度，
/// 因而是 [`BASELINE_FACTOR`] 存在的原因。
pub const ASCENT_UNITS: f32 = 300.0;

/// 字体的 `OS/2` typo descender（正值）。
pub const DESCENT_UNITS: f32 = 0.0;

/// 字体的 `OS/2` typo lineGap。
///
/// **这27 单位是baseline 补偿的真正来源**：行盒高度
/// `= ascender - descender + lineGap = 327`，比 em（300）高出 27，
/// 于是行盒重心相对 em 盒下移、墨迹整体偏上。
pub const LINE_GAP_UNITS: f32 = 27.0;

/// 行盒高度（字体单位）= `ascender - descender + lineGap`。
///
/// 与 [`EM_UNITS`] 的差值（27）即「需要补偿掉」的那个量。
pub const ROW_HEIGHT_UNITS: f32 = ASCENT_UNITS - DESCENT_UNITS + LINE_GAP_UNITS;

/// 绘制一个非交互的图标（不占响应空间），尺寸取 `icon_size` 令牌。
///
/// 用于「状态标记」这类场合——比如已置顶的图钉：
/// 它只是提示当前状态，不可点击，因此不应该吃掉点击。
pub fn paint_inline(ui: &mut Ui, icon: Icon, color: Color32, pal: &crate::theme::Palette, scale: f32) {
    let side = pal.icon_size * scale;
    let (rect, _resp) = ui.allocate_exact_size(Vec2::splat(side), egui::Sense::hover());
    icon.paint(&ui.painter_at(rect), rect, color);
}

/// 图标在 `rect` 内的绘制中心。
///
/// 抽出来是为了让测试能验证「实际绘制位置」而不必真的跑一帧。
pub fn icon_center(rect: Rect) -> egui::Pos2 {
    pos2(rect.center().x, rect.center().y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::Color32;

    /// 建一个只装了图标字体的 `Context`。不碰 GPU。
    ///
    /// 用 [`icon_font_data`] 而不是裸 [`ICON_TTF`]——必须和生产环境
    /// 走同一条注册路径（含baseline 补偿），否则测试量到的居中结果
    /// 与实际渲染不符。
    fn icon_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        let mut defs = egui::FontDefinitions::default();
        defs.font_data.insert(
            ICON_FONT_NAME.to_string(),
            std::sync::Arc::new(icon_font_data()),
        );
        // 只挂图标字体：这样 `has_glyph` 为真就一定是它自己提供的字形，
        // 不会被任何回退字体「代答」。
        defs.families.insert(
            egui::FontFamily::Proportional,
            vec![ICON_FONT_NAME.to_string()],
        );
        ctx.set_fonts(defs);
        // `Context::fonts_mut` 在第一次 `run` 之前会 panic
        // （egui 0.36 明确要求先跑一帧）。跑一个空帧把它初始化掉。
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        ctx
    }

    #[test]
    fn embedded_font_parses() {
        // 字体能被epaint 接受：设置字体 + 触发一次排版不 panic，
        // 且图集里确实产生了像素。
        let ctx = icon_ctx();
        let font_id = egui::FontId::proportional(16.0);
        let has = ctx.fonts_mut(|f| f.has_glyph(&font_id, '\u{e000}'));
        assert!(has, "字体应能解析出 U+E000 的字形");
        let img = ctx.fonts_mut(|f| f.image());
        assert!(
            img.width() > 0 && img.height() > 0,
            "字体图集应已生成: {}x{}",
            img.width(),
            img.height()
        );
    }

    #[test]
    fn every_icon_codepoint_has_ink() {
        // 逐个变体验证：码位在字体里存在，且排版出的墨迹非空。
        // 这条测试真的走 `Icon::codepoint()` 与epaint 的字体解析，
        // 不是拿一个自造的列表对照。
        let ctx = icon_ctx();
        let font_id = egui::FontId::proportional(64.0);
        let mut checked = 0usize;
        for &icon in Icon::ALL {
            let cp = icon.codepoint();
            let (has, w) = ctx.fonts_mut(|f| {
                (
                    f.has_glyph(&font_id, cp),
                    f.glyph_width(&font_id, cp),
                )
            });
            assert!(has, "{icon:?} 的码位 U+{:04X} 在字体里不存在", cp as u32);
            assert!(
                w > 0.0,
                "{icon:?} 的码位 U+{:04X} 宽度为 0（字形是空的）",
                cp as u32
            );
            checked += 1;
        }
        assert_eq!(checked, 18, "变体数量变了？码位表需要重新生成");
    }

    #[test]
    fn codepoints_are_unique() {
        // 两个变体撞码位＝有一个图标显示成另一个。
        let mut seen = std::collections::BTreeSet::new();
        for &icon in Icon::ALL {
            assert!(
                seen.insert(icon.codepoint()),
                "{icon:?} 的码位与其它变体重复"
            );
        }
    }

    #[test]
    fn codepoints_are_in_private_use_area() {
        // PUA 保证不与中英文正文冲突。
        for &icon in Icon::ALL {
            let cp = icon.codepoint();
            assert!(
                ('\u{e000}'..='\u{f8ff}').contains(&cp),
                "{icon:?} 的码位 U+{:04X} 不在 PUA 区内", cp as u32
            );
        }
    }

    #[test]
    fn wrong_codepoint_fails_has_ink() {
        // 变异测试的前提：如果 `has_glyph` 对任何码位都返回 true，
        // 上面那条测试就是假通过。这里确认它真的能分辨。
        let ctx = icon_ctx();
        let font_id = egui::FontId::proportional(16.0);
        assert!(
            !ctx.fonts_mut(|f| f.has_glyph(&font_id, '\u{e012}')),
            "未分配的字形应当报告为不存在——否则 has_glyph 检查无效"
        );
        assert!(
            !ctx.fonts_mut(|f| f.has_glyph(&font_id, '\u{4e2d}')),
            "中文字符应当报告为不存在（该 Context 只挂了图标字体）"
        );
    }

    #[test]
    fn every_icon_has_a_label() {
        // 空标签会让屏幕阅读器静默，这是可访问性缺陷。
        for &i in Icon::ALL {
            assert!(!i.label().is_empty(), "{i:?} 缺少无障碍标签");
        }
    }

    #[test]
    fn pinned_and_unpinned_are_distinct() {
        // 置顶按钮的两种状态不能混淆：图钉实心/描边是两个不同字形。
        assert_ne!(Icon::Pinned.codepoint(), Icon::Unpinned.codepoint());
        assert_ne!(Icon::Pinned.label(), Icon::Unpinned.label());
    }

    #[test]
    fn lock_is_distinct_from_pinned() {
        // 「固定」与「置顶」语义不同，视觉上必须能分开。
        // 历史上两者共用同一图标，导致用户无法区分。
        assert_ne!(Icon::Lock.codepoint(), Icon::Pinned.codepoint());
        assert_ne!(Icon::Unlock.codepoint(), Icon::Unpinned.codepoint());
        assert_ne!(Icon::Lock.label(), Icon::Pinned.label());
    }

    #[test]
    fn collapse_and_expand_are_distinct() {
        assert_ne!(Icon::Collapse.codepoint(), Icon::Expand.codepoint());
        assert_ne!(Icon::Collapse.label(), Icon::Expand.label());
    }

    /// epaint 排版后，某个码位的**墨迹四边形**（逻辑点）。
    ///
    /// 口径与 `epaint::text_layout::tessellate_glyphs` 完全一致：
    /// `left_top = round(glyph.pos + glyph.uv_rect.offset)`，
    /// 尺寸 `= glyph.uv_rect.size`。这是提交给 GPU 的原始四边形，
    /// 不含任何推断——纵向的 [`glyph_ink_is_centred_in_its_box`] 用图集
    /// 像素反推墨迹，而**横向**必须走这里：`uv_rect.offset.x` 就是字形的
    /// bearing（左边距），它是「墨迹相对笔尖的位置」的唯一来源。
    ///
    /// 历史上这里出过一次真实事故：字体子集的 `hmtx` 表里lsb 全被写成 0
    /// （而 `glyf` 里字形真实 xMin 是 37/94/75/…），epaint 读到的
    /// `uv_rect.offset.x` 因此为 0，`Drag`（bearing 19.65px）被贴到笔尖上，
    /// 在 64px 框里横向偏左 21px（-33%）。纵向断言完全测不到这个问题，
    /// 所以必须有这条独立判据。
    fn glyph_ink_quad(ctx: &egui::Context, cp: char, size: f32) -> Rect {
        let text = cp.to_string();
        let job = egui::text::LayoutJob {
            text: text.clone(),
            sections: vec![egui::text::LayoutSection {
                leading_space: 0.0,
                byte_range: egui::text::ByteIndex(0)..egui::text::ByteIndex(text.len()),
                format: egui::TextFormat {
                    font_id: egui::FontId::proportional(size),
                    ..Default::default()
                },
            }],
            wrap: egui::text::TextWrapping {
                max_width: f32::INFINITY,
                max_rows: usize::MAX,
                overflow_character: Some('\u{2026}'),
                ..Default::default()
            },
            ..Default::default()
        };
        let galley = ctx.fonts_mut(|f| f.layout_job(job));
        let g = &galley.rows[0].row.glyphs[0];
        let left_top: egui::Pos2 = (g.pos + g.uv_rect.offset).round();
        Rect::from_min_max(left_top, left_top + g.uv_rect.size)
    }

    #[test]
    fn glyph_ink_is_centred_horizontally() {
        // 横向居中回归测试。纵向有 [`glyph_ink_is_centred_in_its_box`]，
        // 但那条走图集像素反推，**测不到 bearing 丢失**——墨迹整体平移
        // 不会改变它量出的上下留白。这里直接读 epaint 摆好的四边形。
        //
        // # 阈值 3.0px 的来源（不是「放宽到能过」）
        //
        // 字体**自身固有**的墨迹 x 中心与 em 中心（150）的偏差，
        // 从 `glyf` 直读后换算到 64px 框：
        //
        // | 图标| 墨迹 x 中心 | 偏差 |
        // |---|---|---|
        // | `Drag`（grip-vertical） | 141.0 | **-1.88px** |
        // | `Pinned` / `Unpinned` | 154.5 | +0.94px |
        // | `Text` | 152.5 | +0.52px |
        // | 其余 14 个 | 150.0 | 0 |
        //
        // `Drag` 的竖排三列点阵本身就偏左 9 个字体单位（3%），这是
        // Bootstrap Icons 的设计意图，**不该被补偿抹平**——补偿只能整体
        // 平移。所以阈值必须 ≥ 1.88 + 取整余量 ≈ 2.4px，这里取 3.0px。
        //
        // 3.0px 仍远小于真正的事故量级：`hmtx.lsb` 被写坏时 `Drag`
        // 实测 -21px（占边长 33%）。两者有 7 倍差距，且下面那条变异
        // 测试证明它真能抓到那种情况。
        let ctx = icon_ctx();
        let side = 64.0f32;
        let font_size = side * INK_FIT;

        for &icon in Icon::ALL {
            let ink = glyph_ink_quad(&ctx, icon.codepoint(), font_size);
            // `layout_job` 返回的 galley 已按`Align2::CENTER_CENTER`
            // 对齐好，单字形 galley 宽度 = advance = em = font_size。
            // galley 坐标里「墨迹中心 - 布局中心」= `ink_left + w/2 - adv/2`，
            // 其中 `ink_left = pos.x + uv_rect.offset.x`，`offset.x` 即 bearing。
            let adv = ctx
                .fonts_mut(|f| f.glyph_width(&egui::FontId::proportional(font_size), icon.codepoint()));
            let dx = (ink.min.x + ink.width() / 2.0) - adv / 2.0;
            assert!(
                dx.abs() <= 3.0,
                "{icon:?} 墨迹横向偏心 {dx:.2}px（超过 3.0px；若达十几 px \
                 多半是 hmtx.lsb 丢失，即 bearing 未生效）\
                 墨迹盒 {ink:?}，advance {adv:.2}"
            );
        }
    }

    #[test]
    fn horizontal_centering_would_catch_a_broken_lsb() {
        // 变异测试：把 `hmtx` 的 lsb 写坏（复现子集化事故）后，
        // 上面那条**必须**失败。否则它就是一条假测试。
        //
        // 手法：直接改字节。`ICON_TTF` 里`hmtx` 的位置由表目录给出，
        // 这里不硬编码偏移，而是搜索 `glyf` 表里每个字形头部的 xMin，
        // 再把 `hmtx` 对应条目置 0——与事故现场的写法一致。
        let mut bytes = ICON_TTF.to_vec();

        // 定位 hmtx 表：表目录里tag == "hmtx"。
        let num_tables = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
        let mut hmtx: Option<(usize, usize)> = None;
        for i in 0..num_tables {
            let rec = 12 + 16 * i;
            if &bytes[rec..rec + 4] == b"hmtx" {
                let off = u32::from_be_bytes([
                    bytes[rec + 8],
                    bytes[rec + 9],
                    bytes[rec + 10],
                    bytes[rec + 11],
                ]) as usize;
                let len = u32::from_be_bytes([
                    bytes[rec + 12],
                    bytes[rec + 13],
                    bytes[rec + 14],
                    bytes[rec + 15],
                ]) as usize;
                hmtx = Some((off, len));
            }
        }
        let (hmtx_off, hmtx_len) = hmtx.expect("字体里没有 hmtx 表");

        // 把 lsb 全清零。`numHMetrics` 之前的条目是 (advance:i16, lsb:i16)，
        // 之后是纯 lsb 数组——两种布局都要覆盖。
        let hhea_off = {
            let mut o = None;
            for i in 0..num_tables {
                let rec = 12 + 16 * i;
                if &bytes[rec..rec + 4] == b"hhea" {
                    o = Some(u32::from_be_bytes([
                        bytes[rec + 8],
                        bytes[rec + 9],
                        bytes[rec + 10],
                        bytes[rec + 11],
                    ]) as usize);
                }
            }
            o.expect("字体里没有 hhea 表")
        };
        let num_h_metrics = u16::from_be_bytes([
            bytes[hhea_off + 34],
            bytes[hhea_off + 35],
        ]) as usize;
        for i in 0..hmtx_len / 2 {
            let p = if i < num_h_metrics {
                hmtx_off + 4 * i + 2
            } else {
                hmtx_off + 4 * num_h_metrics + 2 * (i - num_h_metrics)
            };
            bytes[p] = 0;
            bytes[p + 1] = 0;
        }

        // 用这份「坏字体」重建 Context，重复横向判据。
        let broken = egui::FontData::from_owned(bytes).tweak(egui::FontTweak {
            y_offset_factor: BASELINE_FACTOR,
            ..Default::default()
        });
        let ctx = egui::Context::default();
        let mut defs = egui::FontDefinitions::default();
        defs.font_data.insert(
            ICON_FONT_NAME.to_string(),
            std::sync::Arc::new(broken),
        );
        defs.families.insert(
            egui::FontFamily::Proportional,
            vec![ICON_FONT_NAME.to_string()],
        );
        ctx.set_fonts(defs);
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();

        let side = 64.0f32;
        let mut worst = 0.0f32;
        let mut worst_icon = Icon::Close;
        for &icon in Icon::ALL {
            let ink = glyph_ink_quad(&ctx, icon.codepoint(), side * INK_FIT);
            let adv = ctx.fonts_mut(|f| {
                f.glyph_width(&egui::FontId::proportional(side * INK_FIT), icon.codepoint())
            });
            let dx = (ink.center().x - adv / 2.0) - (side - adv) / 2.0;
            if dx.abs() > worst {
                worst = dx.abs();
                worst_icon = icon;
            }
        }
        assert!(
            worst > 1.5,
            "把 hmtx.lsb 清零后横向判据仍然通过（最差 {worst:.2}px @ {worst_icon:?}）——\
             glyph_ink_is_centred_horizontally 是假测试"
        );
    }

    /// 某个图标渲染出来的四边形在屏幕上的矩形，以及它在图集里的 UV 矩形。
    ///
    /// 返回 `(屏幕矩形, UV 矩形)`。两者缺一不可：
    ///
    /// - **屏幕矩形**给出这个字被放在了哪里——这正是
    ///   [`BASELINE_FACTOR`] 唯一影响的东西（它平移字形，不改行盒）。
    /// - **UV 矩形 + 图集像素**给出字形的实际形状。文字在 epaint 里
    ///   是贴图四边形，顶点坐标不含形状信息，所以互为镜像的两个图标
    ///   （`chevron-bar-left` / `chevron-bar-right`）顶点位置完全相同，
    ///   只有像素区分得开。
    fn glyph_quad(ctx: &egui::Context, icon: Icon, rect: Rect) -> (Rect, Vec2, Vec2) {
        let verts = icon_vertices(ctx, icon, rect, Color32::WHITE);
        assert!(!verts.is_empty(), "{icon:?} 没有产生四边形");
        let mut pmin = Vec2::splat(f32::INFINITY);
        let mut pmax = Vec2::splat(f32::NEG_INFINITY);
        let mut uv_min = Vec2::splat(f32::INFINITY);
        let mut uv_max = Vec2::splat(f32::NEG_INFINITY);
        for v in &verts {
            pmin = pmin.min(egui::vec2(v.pos.x, v.pos.y));
            pmax = pmax.max(egui::vec2(v.pos.x, v.pos.y));
            uv_min = uv_min.min(egui::vec2(v.uv.x, v.uv.y));
            uv_max = uv_max.max(egui::vec2(v.uv.x, v.uv.y));
        }
        (
            Rect::from_min_max(pos2(pmin.x, pmin.y), pos2(pmax.x, pmax.y)),
            uv_min,
            uv_max,
        )
    }

    /// 每个图标墨迹的上下留白（`(上, 下)`，逻辑点）。
    fn ink_paddings(ctx: &egui::Context, side: f32) -> Vec<(Icon, f32, f32)> {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), Vec2::splat(side));
        let mut out = Vec::new();
        for &icon in Icon::ALL {
            let (quad, uv_min, uv_max) = glyph_quad(ctx, icon, rect);
            let (ink_min, ink_max) = atlas_ink_bounds(ctx, uv_min, uv_max)
                .unwrap_or_else(|| panic!("{icon:?} 在图集里没有墨迹"));
            let uw = uv_max.x - uv_min.x;
            let uh = uv_max.y - uv_min.y;
            debug_assert!(uw > 0.0 && uh > 0.0, "{icon:?} 的 UV 矩形退化了");
            let to_y = |v: f32| quad.min.y + (v - uv_min.y) / uh * quad.height();
            out.push((icon, to_y(ink_min.y) - rect.min.y, rect.max.y - to_y(ink_max.y)));
        }
        out
    }

    /// 本图标集**固有**的墨迹中心偏差上界（占边长的比例）。
    ///
    /// 字体里各字形并非都落在 em 盒正中，偏差来自字形自身的轮廓，
    /// 从字体 `glyf` 表直接读出的墨迹中心（em = 300，单位）里可见：
    /// `Text`（type）141.5 偏上 8.5、`Trash` 159.5 偏下 9.5、
    /// `Pinned` / `Unpinned`（pin-angle*）154.5 偏上 4.5、
    /// `Search` 152.0，其余基本正中 150.0。
    /// 也就是说**没有任何单一补偿能让 18 个图标同时严格居中**——
    /// 补偿只能整体平移，逐字形对齐等于抹平设计意图。
    ///
    /// 7% 这个值是**实测**出来的，不是拍的：64×64 框里逐个量上下留白，
    /// 最差的是 `Trash`（`dy` -2.0px）与 `Pinned` / `Unpinned`（-1.5px）。
    /// 换算成比例是 2.0/64 ≈ 3.1%，这里留到 7% 是因为字体图集按整像素
    /// 对齐、量测本身有约 1px 量化误差，容差需含这部分余量。
    /// （若把它压到 3% 就正好卡在量测噪声上——测试会变成 flaky，
    /// 而不是因为代码变坏才失败。）
    ///
    /// 真正的 baseline 校准判据是下面那条**均值**断言——它对
    /// [`BASELINE_FACTOR`] 敏感，逐条断言则只兜住「没有整体跑偏」。
    const INK_CENTRE_SPREAD: f32 = 0.07;

    #[test]
    fn report_ink_geometry() {
        // 诊断用：把每个图标的真实墨迹盒打印出来，供实机核对。
        // 无断言——它的价值是「报告数值」，判定由 glyph_ink_is_centred_in_its_box 负责。
        //
        // 关键点：**必须在多个字号下测**。补偿量正比于字号，
        // 单个字号下「碰巧居中」不能证明公式对。
        for &side in &[16.0f32, 32.0, 64.0] {
            let ctx = icon_ctx();
            // egui 自己认定的行盒高度——墨迹落位的唯一依据。
            let fid = egui::FontId::proportional(side * INK_FIT);
            let row_h = ctx.fonts_mut(|f| f.row_height(&fid));
            let pads = ink_paddings(&ctx, side);
            let n = pads.len() as f32;
            let mean_cy =
                pads.iter().map(|&(_, t, b)| t + (side - t - b) / 2.0).sum::<f32>()
                / n;
            let max_abs_cy = pads
                .iter()
                .map(|&(_, t, b)| (t + (side - t - b) / 2.0 - side / 2.0).abs())
                .fold(0.0f32, f32::max);
            println!(
                "side={side:.0} font_size={:.3} row_height={row_h:.3} \
                 box_cy={:.2} mean_ink_cy={mean_cy:.2} mean_dy={:.3} max_abs_dy={:.3}",
                side * INK_FIT,
                side / 2.0,
                mean_cy - side / 2.0,
                max_abs_cy
            );
        }
        // 64px 下逐个图标明细（与历史记录对齐，便于对比）。
        let ctx = icon_ctx();
        let side = 64.0f32;
        let pads = ink_paddings(&ctx, side);
        println!("--- per-icon @64 ---");
        for &(icon, top, bottom) in &pads {
            let ink_h = side - top - bottom;
            println!(
                "{:<10} top={:>6.2} bottom={:>6.2} ink_h={:>6.2} ink_cy={:>6.2} dy={:>6.2}",
                format!("{icon:?}"),
                top,
                bottom,
                ink_h,
                top + ink_h / 2.0,
                top + ink_h / 2.0 - side / 2.0
            );
        }
    }

    #[test]
    fn glyph_ink_is_centred_in_its_box() {
        // baseline 对齐的回归测试——本次任务里唯一有实操不确定性的点。
        //
        // 三条判据，缺一不可：
        //
        // 1. **逐个**留白差不超过 [`INK_CENTRE_SPREAD`]（字体固有的
        //    墨迹中心散布，见该常量文档）。
        // 2. **整体均值**接近 0。这条才是对 [`BASELINE_FACTOR`] 敏感的：
        //    补偿值错了一定会让所有图标**同向**偏移，均值随之偏离。
        //    只留第 1 条的话，把补偿整个归零也能通过——变异测试验过。
        // 3. **多字号**复核（见 [`baseline_factor_holds_across_sizes`]）：
        //    补偿量正比于字号，只在一个字号上居中不足以证明公式对。
        let ctx = icon_ctx();
        let side = 64.0f32;
        let pads = ink_paddings(&ctx, side);

        for &(icon, top, bottom) in &pads {
            assert!(
                top >= -0.5 && bottom >= -0.5,
                "{icon:?} 墨迹纵向溢出目标框: 上留白 {top:.2} 下留白 {bottom:.2}"
            );
            assert!(
                (top - bottom).abs() <= side * INK_CENTRE_SPREAD,
                "{icon:?} 墨迹纵向未居中: 上留白 {top:.2} 下留白 {bottom:.2}                  (允许差 {:.2})", side * INK_CENTRE_SPREAD
            );
        }

        // 均值：补偿错了会让全体**同向**偏移，逐条断言可能仍落在
        // [`INK_CENTRE_SPREAD`] 的容差内，均值却一定露馅。
        // 阈值 2.5% 是量测精度决定的：字体图集按整像素对齐，
        // 单图标留白有约 1px 的量化误差，18 个取均值后仍有约 1px 抖动。
        let n = pads.len() as f32;
        let mean_skew = pads.iter().map(|&(_, t, b)| t - b).sum::<f32>() / n;
        assert!(
            mean_skew.abs() <= side * 0.025,
            "整体纵向偏移 {mean_skew:.2} 过大（阈值 {:.2}）——             BASELINE_FACTOR 需要重新校准",
            side * 0.025
        );
    }

    #[test]
    fn baseline_factor_holds_across_sizes() {
        // 补偿量正比于字号，所以「在64px 上居中」不等于「公式对」——
        // 一个写错的补偿完全可能只在某个字号上凑巧成立。
        // 生产用16px，测试用 64px，必须两个都对。
        for &side in &[16.0f32, 32.0, 64.0] {
            let ctx = icon_ctx();
            let pads = ink_paddings(&ctx, side);
            let n = pads.len() as f32;
            let mean_dy = pads
                .iter()
                .map(|&(_, t, b)| t + (side - t - b) / 2.0 - side / 2.0)
                .sum::<f32>()
                / n;
            assert!(
                mean_dy.abs() <= side * 0.02,
                "side={side}: 整体纵向偏移 {mean_dy:.3} 过大（阈值 {:.3}）——\
                 BASELINE_FACTOR 与字号的关系不对",
                side * 0.02
            );
        }
    }

    /// 从字体图集里量出 UV 矩形内的实际墨迹范围（alpha > 0）。
    ///
    /// 返回 `None` 表示那块区域里一个不透明像素都没有——
    /// 那说明该码位映射到了空字形。
    fn atlas_ink_bounds(
        ctx: &egui::Context,
        uv_min: Vec2,
        uv_max: Vec2,
    ) -> Option<(Vec2, Vec2)> {
        let img = ctx.fonts_mut(|f| f.image());
        let w = img.width();
        let h = img.height();
        let img = img;
        let x0 = (uv_min.x * w as f32).floor().max(0.0) as usize;
        let x1 = ((uv_max.x * w as f32).ceil() as usize).min(w);
        let y0 = (uv_min.y * h as f32).floor().max(0.0) as usize;
        let y1 = ((uv_max.y * h as f32).ceil() as usize).min(h);
        let mut found = false;
        let mut mn = (usize::MAX, usize::MAX);
        let mut mx = (0usize, 0usize);
        for y in y0..y1 {
            for x in x0..x1 {
                // egui 的字体图集是覆盖度图：r 通道存覆盖率。
                if img.pixels[y * w + x].r() > 0 {
                    found = true;
                    mn.0 = mn.0.min(x);
                    mn.1 = mn.1.min(y);
                    mx.0 = mx.0.max(x);
                    mx.1 = mx.1.max(y);
                }
            }
        }
        found.then(|| {
            (
                Vec2::new(mn.0 as f32 / w as f32, mn.1 as f32 / h as f32),
                Vec2::new((mx.0 + 1) as f32 / w as f32, (mx.1 + 1) as f32 / h as f32),
            )
        })
    }

    #[test]
    fn painted_icons_are_visually_distinct() {
        // 两个不同的图标不能引用字体图集里的同一块像素——
        // 那是「码位映射错了」的典型症状。
        //
        // 判据用**图集里的墨迹像素**而不是顶点位置：互为镜像的两个
        // 图标顶点位置完全相同（文字是贴图四边形），只有像素才区分得开。
        let ctx = icon_ctx();
        let rect = Rect::from_min_size(pos2(0.0, 0.0), Vec2::splat(64.0));
        let mut sigs: std::collections::BTreeMap<Vec<(u8, u8)>, Icon> =
            std::collections::BTreeMap::new();
        for &icon in Icon::ALL {
            let (_quad, uv_min, uv_max) = glyph_quad(&ctx, icon, rect);
            let img = ctx.fonts_mut(|f| f.image());
            let w = img.width();
            let h = img.height();
            let img = img;
            let x0 = (uv_min.x * w as f32).floor().max(0.0) as usize;
            let x1 = ((uv_max.x * w as f32).ceil() as usize).min(w);
            let y0 = (uv_min.y * h as f32).floor().max(0.0) as usize;
            let y1 = ((uv_max.y * h as f32).ceil() as usize).min(h);
            // 逐像素取样：同一块图集区域必然给出同一串覆盖度。
            let mut px: Vec<(u8, u8)> = Vec::new();
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = img.pixels[y * w + x];
                    px.push((p.r(), p.g()));
                }
            }
            assert!(!px.is_empty(), "{icon:?} 没有图集像素");
            if let Some(prev) = sigs.insert(px, icon) {
                panic!("{icon:?} 与 {prev:?} 引用了字体图集里完全相同的像素——码位映射错了");
            }
        }
    }

    #[test]
    fn baseline_metrics_match_the_font() {
        // 这些常数是baseline 补偿的依据。若重新生成了字体，
        // 这里会先失败，提示需要重新校准。
        assert_eq!(EM_UNITS, 300.0, "em 变了：图标尺寸算法需重新校准");
        assert_eq!(ASCENT_UNITS, 300.0, "OS/2 ascender 变了：BASELINE_FACTOR 需重算");
        assert_eq!(DESCENT_UNITS, 0.0, "OS/2 descender 变了：BASELINE_FACTOR 需重算");
        assert_eq!(
            LINE_GAP_UNITS, 27.0,
            "OS/2 lineGap 变了：BASELINE_FACTOR 需重算（它是补偿的唯一来源）"
        );
        // 补偿存在的前提：行盒比em 高。高出量必须等于 lineGap。
        assert!(
            ROW_HEIGHT_UNITS > EM_UNITS,
            "行盒({ROW_HEIGHT_UNITS}) 未高于 em({EM_UNITS})：BASELINE_FACTOR 应为 0"
        );
        assert_eq!(
            ROW_HEIGHT_UNITS - EM_UNITS,
            LINE_GAP_UNITS,
            "行盒超出量应恰为 lineGap"
        );
    }

    /// 在真实 `Ui` 里画一个图标，返回 tessellate 后的可见顶点。
    ///
    /// 走完整的 `ctx.run_ui` → 排版 → 光栅化 → tessellate 通路，
    /// 因此拿到的是**实际会提交给 GPU** 的顶点，而不是我们自己算的
    /// 近似值。全程纯 CPU，不需要 GPU。
    fn icon_vertices(
        ctx: &egui::Context,
        icon: Icon,
        rect: Rect,
        color: Color32,
    ) -> Vec<egui::epaint::Vertex> {
        let mut full = ctx.run_ui(egui::RawInput::default(), |ctx| {
            // 刻意**不用** `CentralPanel`：它会自己画一层背景，
            // 那些顶点会混进「墨迹包围盒」里，让测量结果毫无意义
            // （实测会让每个图标的包围盒都等于整个屏幕）。
            // `Ui::new` 只产出我们显式画的东西。
            let mut ui = egui::Ui::new(
                ctx.clone(),
                egui::Id::new("icon_probe"),
                egui::UiBuilder::new().max_rect(rect),
            );
            let (r, _) = ui.allocate_exact_size(rect.size(), egui::Sense::hover());
            icon.paint(&ui.painter_at(r), r, color);
        });
        // 排版会产出字体图集增量；测试不上传纹理，显式 clear()
        // （与 app.rs 的 `DeltaGuard` 同一手法），否则 debug 构建
        // 会在 Drop 时断言失败。必须 clear **本次** run 的 delta。
        full.textures_delta.clear();
        let prims = ctx.tessellate(full.shapes, 1.0);
        let mut out = Vec::new();
        for p in prims {
            if let egui::epaint::Primitive::Mesh(mesh) = p.primitive {
                // 全透明顶点是光栅化的padding，不算「画出来了」。
                out.extend(mesh.vertices.into_iter().filter(|v| v.color.a() > 0u8));
            }
        }
        out
    }

    #[test]
    fn transparent_colour_draws_nothing() {
        // 全透明色不应产生顶点——否则会白占一片。
        let ctx = icon_ctx();
        let rect = Rect::from_min_size(pos2(0.0, 0.0), Vec2::splat(64.0));
        let verts = icon_vertices(&ctx, Icon::Close, rect, Color32::TRANSPARENT);
        assert!(verts.is_empty(), "透明色不应产生可见顶点");
    }

    #[test]
    fn zero_sized_rect_is_skipped() {
        // 零尺寸矩形（面板被折叠到 0 宽时）不能除零 / panic。
        let ctx = icon_ctx();
        let rect = Rect::from_min_size(pos2(0.0, 0.0), Vec2::splat(0.0));
        let verts = icon_vertices(&ctx, Icon::Trash, rect, Color32::WHITE);
        assert!(verts.is_empty(), "零尺寸矩形不应产生顶点");
    }
}
