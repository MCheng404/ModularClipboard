//! 悬浮层布局：`FloatLayout` 与类型栏几何常量。
//!
//! 搜索面板、类型过滤栏、通知都是「贴在卡片区某侧」的悬浮层，
//! 共享同一套「面板必须比按钮组大一圈以盖住投影」的几何约定。

use egui::Rect;
use egui::Vec2;
use egui::pos2;
use egui::vec2;

/// 类型过滤栏与主窗口的间距（逻辑点）。
pub(super) const RAIL_GAP: f32 = 12.0;

/// 类型过滤栏**面板**相对按钮组的内边距（逻辑点）。
///
/// 面板必须比按钮组大一圈：它是**不透明**的底，用来盖住投影。
///
/// # 为什么不能让面板 = 按钮组
///
/// 投影是「面板矩形外扩」的实心圆角矩形，天然**包含面板内部**；别的
/// 面板都有自己的底色把它盖住，而类型栏此前直接拿按钮组当面板 ⇒
/// 按钮之间的缝隙和四周一圈透出投影的颜色，整块看着就是**黑色背景**
/// （投影色是纯黑，见 [`POPUP_SHADOW_STRENGTH`] 的说明）。
///
/// 弹窗窗口尺寸 = 面板 + [`crate::popup::SHADOW_MARGIN`]，所以加这份
/// padding 会让窗口跟着大一圈——这是必须的，不是浪费。
pub const RAIL_PAD: f32 = 6.0;

/// 类型过滤栏的按钮顺序（与设置界面、捕获层同一份）。
pub(super) const RAIL_KIND_ORDER: [modular_clipboard_core::ClipKind; 5] = {
    use modular_clipboard_core::ClipKind;
    [
        ClipKind::Text,
        ClipKind::Html,
        ClipKind::Image,
        ClipKind::Files,
        ClipKind::Markdown,
    ]
};

/// 悬浮装饰（搜索面板 / 类型过滤栏）的**弹窗几何**。
///
/// # 坐标系：主窗口客户区左上角为原点，允许负坐标
///
/// 搜索面板在窗口**上方** ⇒ `y ∈ [-(RAIL_GAP + 面板高度), -RAIL_GAP]`
/// （高度取自 `search_panel.height`，2026-10-20 起可调）；
/// 类型栏在左 / 上方同理。`crate::popup` 据此换算屏幕坐标（加主窗口
/// 屏幕位置）。
///
/// 这正是原型（`main-proto/index.html`）的布局：两个面板**漂在窗口
/// 之外**，间距透出桌面。2026-10-15 第三轮初版把它们画在窗口内，
/// 与原型不符——Win32 窗口画不出客户区之外，只能靠独立弹窗实现。
///
/// 因为面板不再压在内容上，**列表也不需要为它们让位**（第三轮初版
/// 里的 `insets` 已随之删除，卡片区恢复满宽）。
///
/// **纯函数** `compute` 的产物：帧循环（弹窗定位）与绘制层（弹窗内容）
/// 各自调它，同一份输入必然同一份矩形。
pub struct FloatLayout {
    /// 类型栏按钮组的矩形（`None` = 不显示）。
    pub rail: Option<Rect>,
    /// 搜索面板矩形（`None` = 折叠）。
    pub search: Option<Rect>,
}

impl FloatLayout {
    /// 当前应显示的类型栏按钮（顺序固定）。
    ///
    /// 抽成函数是为了让**几何**（[`FloatLayout::compute`] 决定弹窗尺寸）
    /// 与**绘制**（弹窗内容里的按钮）读同一份结果——两处各判一次
    /// 「该显示哪几类」必然漂移（弹窗按 4 类算尺寸、内容画 3 枚按钮）。
    pub fn kinds(
        cfg: &modular_clipboard_core::config::UiConfig,
        items: &[modular_clipboard_core::item::ClipItem],
    ) -> Vec<modular_clipboard_core::ClipKind> {
        if !cfg.type_rail.visible {
            return Vec::new();
        }
        RAIL_KIND_ORDER
            .iter()
            .copied()
            // 默认只显示「已有记录」的类型；`always` 时五类齐全。
            .filter(|k| cfg.type_rail.always || items.iter().any(|it| it.kind == *k))
            .collect()
    }

    /// 计算悬浮弹窗的布局。
    ///
    /// - `area`：主窗口客户区（逻辑点，窗口坐标）；
    /// - `search_expanded`：搜索面板是否展开；
    /// - `rail_size_hint`：类型栏**实测**面板尺寸（见
    ///   [`UiState::rail_panel_size`]）。仅 `labels` 开启时使用；
    ///   `None` 用估算值（宁大勿小，估小按钮会被窗口边界裁掉）。
    pub fn compute(
        cfg: &modular_clipboard_core::config::UiConfig,
        items: &[modular_clipboard_core::item::ClipItem],
        search_expanded: bool,
        area: Rect,
        rail_size_hint: Option<Vec2>,
    ) -> Self {
        use modular_clipboard_core::config::{SearchPanelPos, TypeRailPos};

        let rail_cfg = &cfg.type_rail;
        let size = modular_clipboard_core::config::clamp_type_rail_size(rail_cfg.size);
        let gap = RAIL_GAP.min(8.0); // 按钮间距比面板间距紧一档

        // ---- 类型栏：**面板**矩形（弹窗按它定尺寸，投影边距由弹窗自己加） ----
        //
        // 面板 = 按钮组 + 四周 [`RAIL_PAD`]。多出来的这一圈是不透明底色，
        // 用来盖住投影（投影矩形包含面板内部，见 `RAIL_PAD` 的说明）。
        let rail_kinds = Self::kinds(cfg, items);
        let rail = match rail_kinds.len() {
            0 => None,
            n => {
                // 弹窗客户区只包住面板本身（+ 投影边距），不做成
                // 「整条侧栏那么高」——那圈空白是透明窗口面积，
                // 既费显存又会挡下面的窗口（要靠点击穿透兜，更容易出岔子）。
                let vertical = matches!(rail_cfg.pos, TypeRailPos::Left | TypeRailPos::Right);
                let (panel_w, panel_h) = if rail_cfg.labels {
                    // 带文字标签：尺寸由弹窗实测回报（hint），首帧估算。
                    let est = rail_size_hint
                        .unwrap_or_else(|| rail_label_panel_estimate(n, gap, size, vertical));
                    (est.x, est.y)
                } else {
                    let total = n as f32 * size + (n as f32 - 1.0) * gap + RAIL_PAD * 2.0;
                    let cross = size + RAIL_PAD * 2.0;
                    if vertical {
                        (cross, total)
                    } else {
                        (total, cross)
                    }
                };
                let card = crate::solver::card_area(area);
                Some(match rail_cfg.pos {
                    // 右 / 左：贴在窗口对应侧缘外，垂直对齐卡片区中心。
                    TypeRailPos::Right => Rect::from_center_size(
                        pos2(area.max.x + RAIL_GAP + panel_w * 0.5, card.center().y),
                        vec2(panel_w, panel_h),
                    ),
                    TypeRailPos::Left => Rect::from_center_size(
                        pos2(area.min.x - RAIL_GAP - panel_w * 0.5, card.center().y),
                        vec2(panel_w, panel_h),
                    ),
                    // 上 / 下：横向一行，对齐卡片区中心。
                    TypeRailPos::Top => Rect::from_center_size(
                        pos2(card.center().x, area.min.y - RAIL_GAP - panel_h * 0.5),
                        vec2(panel_w, panel_h),
                    ),
                    TypeRailPos::Bottom => Rect::from_center_size(
                        pos2(card.center().x, area.max.y + RAIL_GAP + panel_h * 0.5),
                        vec2(panel_w, panel_h),
                    ),
                })
            }
        };

        // ---- 搜索面板：窗口上方（默认）或下方通栏 ----
        let search = search_expanded.then(|| {
            let h = modular_clipboard_core::config::clamp_search_panel_height(cfg.search_panel.height);
            match cfg.search_panel.pos {
                SearchPanelPos::Top => Rect::from_min_max(
                    pos2(area.min.x, area.min.y - RAIL_GAP - h),
                    pos2(area.max.x, area.min.y - RAIL_GAP),
                ),
                SearchPanelPos::Bottom => Rect::from_min_max(
                    pos2(area.min.x, area.max.y + RAIL_GAP),
                    pos2(area.max.x, area.max.y + RAIL_GAP + h),
                ),
            }
        });

        // NOTE: 按钮列表不进产物——弹窗绘制路径自己调 `kinds()` 取
        // （与 `compute` 同输入同输出，纯函数无缓存成本）；曾经存过
        // 一份字段，全仓无读取方，已删。
        Self { rail, search }
    }
}

/// 类型栏 `labels` 开启、但还没有实测尺寸时的**估算**面板尺寸。
///
/// 文字宽度要字体度量，而 [`FloatLayout::compute`] 是无 ctx 的纯函数。
/// 估算值只活一帧（弹窗首帧画完就有实测值），但口径必须与
/// `draw_type_rail` 一致，且**宁大勿小**：估小了按钮会被窗口边界裁掉
/// （既看不见也点不到），估大了只多一圈透明边距。
pub(super) fn rail_label_panel_estimate(n: usize, gap: f32, size: f32, vertical: bool) -> Vec2 {
    /// 最长标签 "Markdown"（9 个拉丁字符）在 font_xs(14) 下的宽度估计，
    /// 已含余量（实测约 63pt）。
    const LABEL_W: f32 = 72.0;
    /// 标签行高：与 `draw_type_rail` 的 `label_h`（font_xs × 1.25）同口径。
    const LABEL_H: f32 = 17.5;
    /// 图标与文字的间距（同 `draw_type_rail`）。
    const LABEL_GAP: f32 = 4.0;
    let cell = if vertical {
        size + LABEL_GAP + LABEL_H
    } else {
        size + LABEL_GAP + LABEL_W
    };
    let total = n as f32 * cell + (n as f32 - 1.0) * gap + RAIL_PAD * 2.0;
    let cross = size + RAIL_PAD * 2.0;
    if vertical {
        vec2(cross, total)
    } else {
        vec2(total, cross)
    }
}
