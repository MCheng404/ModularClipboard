//! 颜色助手与卡片投影。
//!
//! 投影是多层高斯矩形叠加（自绘，无 DWM 投影）；`fade_color`
//! 在预乘空间工作——二次预乘是踩过的坑。

use egui::Rect;
use egui::vec2;
use crate::theme::Palette;

/// 按比例 `k ∈ [0,1]` 缩放颜色的**不透明度**（补间淡入淡出用）。
///
/// # 为什么必须走预乘分量
///
/// egui 的 [`egui::Color32`] 内部是**预乘 alpha** 存储（`r()` 返回的是
/// 已经乘过 alpha 的通道，见 ecolor `color32.rs` 模块文档）。因此缩放
/// 不透明度要把**四个通道一起乘 k**，再用 `from_rgba_premultiplied`
/// 组装：
///
/// ```text
/// from_rgba_premultiplied(r·k, g·k, b·k, a·k)
/// ```
///
/// 若改用 `from_rgba_unmultiplied(r, g, b, a·k)`（把预乘分量当普通
/// RGBA 传进去），rgb 会被**二次预乘**——本仓踩过：行悬停底色
/// `row_hover = from_white_alpha(30)`，正确值应是 11.8% 的白，二次
/// 预乘后只剩 ~1.6%，悬停提亮几乎看不见（2026-10-17 修复）。
///
/// 纯函数：不碰 egui 运行时，单测直接覆盖。
pub(crate) fn fade_color(c: egui::Color32, k: f32) -> egui::Color32 {
    let k = k.clamp(0.0, 1.0);
    let [r, g, b, a] = c.to_array();
    let s = |v: u8| (v as f32 * k).round().clamp(0.0, 255.0) as u8;
    egui::Color32::from_rgba_premultiplied(s(r), s(g), s(b), s(a))
}

/// 两色线性插值（`t ∈ [0,1]`）。
///
/// 用于「慢慢变红」这类渐变：比离散切换平滑，且 RGB 逐通道插值
/// 在深浅主题下都不会穿过非法色域。纯函数，可单测。
pub(super) fn lerp_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| -> u8 {
        (x as f32 + (y as f32 - x as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    egui::Color32::from_rgba_premultiplied(
        ch(a.r(), b.r()),
        ch(a.g(), b.g()),
        ch(a.b(), b.b()),
        ch(a.a(), b.a()),
    )
}

/// 画一圈圆角描边：**两次填充**，而不是路径描边。
///
/// # 为什么不用 `Painter::rect_stroke`
///
/// `rect_stroke` 把圆角矩形展平成一条**路径**再描边，路径在「弧 → 直线」
/// 接点处首尾重叠，`StrokeKind::Inside` 下重叠部分被叠加两次——同样是
/// 深色短线的一个来源。两次填充至少消除了这一份。
///
/// # CAUTION: 但它**不能**消除 feathering 自带的重叠
///
/// 真正的主因在 egui 的边缘抗锯齿（feathering）：它沿每条边生成一条
/// alpha 渐变的填充带，在**凸角**处「直线 → 弧线」接点两侧的填充带会
/// 重叠，alpha 累加 2~4 次。实测弧线两端像素亮度 196~205，比描边本身
/// （237）还暗——单次叠加不可能更暗，只有重复叠加才会。
///
/// 因此**只要描边色比背景暗，这个瑕疵就一定会出现在圆角两端**，
/// 换绘制方式没用。真正的解法是不画比背景暗的描边（见 `draw_item_row`）。
/// 本函数保留给确实需要圆环的场合（例如饱和色的选中态）。
pub(super) fn fill_rounded_border(
    painter: &egui::Painter,
    r: Rect,
    radius: f32,
    w: f32,
    border: egui::Color32,
    content: impl FnOnce(&egui::Painter, Rect, f32),
) {
    let inner = r.shrink(w);
    let inner_r = (radius - w).max(0.0);
    // 太窄时内缩矩形会退化甚至翻负：此时只画描边色（等价于一块实心
    // 圆角块）。**绝不能**把非正尺寸传给 egui——它的圆角路径会算出
    // 畸形几何，画出乱七八糟的图形。
    if w <= 0.0 || inner.width() <= 0.0 || inner.height() <= 0.0 {
        painter.rect_filled(r, radius, border);
        return;
    }
    painter.rect_filled(r, radius, border);
    content(painter, inner, inner_r);
}

/// 高斯核对半平面卷积的外轮廓（归一化）：`1 - Φ(d/σ) = 0.5·erfc(d/(σ√2))`。
///
/// `d` 为卡片边缘**向外**的距离（逻辑点）：边缘处为 0.5，向外单调
/// 衰减到 0——真实 box-shadow 在缝隙里的强度分布就是这条曲线，
/// 比线性羽化自然得多（边缘不生硬、外缘无截断）。
///
/// 用 Abramowitz & Stegun 7.1.26 的 Φ(x) 有理近似（|误差| < 7.5e-8），
/// 不引第三方数学库。
///
/// NOTE: 系数**刻意保留出版常数**（而非削到 f32 精度）：这组数字要与
/// 论文表格逐位对得上才敢声称「误差 < 7.5e-8」，削位反而让可追溯性
/// 断掉。`excessive_precision` 在这里是特性不是缺陷。
#[allow(clippy::excessive_precision)]
pub(super) fn gaussian_outer(d: f32, sigma: f32) -> f32 {
    if sigma <= 0.0 {
        return 0.0;
    }
    let x = (d / sigma).max(0.0);
    let t = 1.0 / (1.0 + 0.2316419 * x);
    let phi = 0.39894228 * (-0.5 * x * x).exp(); // 1/√(2π)·e^(-x²/2)
    let poly = t
        * (0.319381530
            + t * (-0.356563782 + t * (1.781477937 + t * (-1.821255978 + t * 1.330274429))));
    phi * poly
}

/// 列表卡片阴影：多层同心圆角矩形合成的**高斯投影**。
///
/// # 为什么不用 `epaint::Shadow`（旧实现被否决的原因）
///
/// 1. **被裁剪 + 被覆盖，等于没画**：旧实现用行内 `painter_at(r)`，
///    其 clip 与行矩形取交集，阴影外扩晕带全被裁掉；r 内部分又被
///    不透明的卡片填充完整盖住。
/// 2. **圆角暗斑**：`Shadow` 走 epaint feathering（沿轮廓的单条
///    抗锯齿渐变带），在圆角凸角「直线→弧线」接点两侧填充带重叠，
///    alpha 累加 2~4 次（本仓常态描边因此被迫移除，见
///    [`draw_item_row`] 里的排查记录），四个角变成深色斑点。
/// 3. **深色主题不可见**：黑色 16% 投影叠在近黑的 `list_bg`
///    （`#101215`）上几乎没有明度差。
///
/// # 做法：N 层实心圆角矩形 + 反解 alpha 混合
///
/// 把扩散区切成 1pt 宽的环带，第 k 层是一个「按 offset 平移后再
/// 外扩 k 点」的**实心**圆角矩形（无 feathering ⇒ 无凸角重叠），
/// 圆角半径同步外扩 k（同心圆角）。从最大的一层向卡片画：
///
/// - 环带 k（距卡片边缘 k-1~k 点）被第 k..n 层覆盖；
/// - 设其目标强度为 `T_k = C · gaussian_outer(k-0.5, σ)`；
/// - 画第 k 层时它已经与更外层混成强度 `outer`，按「源 over」
///   公式 `T = a + outer·(1-a)` 反解 `a = (T - outer)/(1 - outer)`；
/// - 画完后更新 `outer`。跳过的层（量化后不可见）不更新，
///   后续层自动补偿，误差不累积。
///
/// 不可加的 alpha 混合于是被精确反解，最终环带强度就是高斯
/// 轮廓，量化误差仅 ±1/255。
///
/// # 裁剪与顺序（由调用方 `draw_history_body` 保证）
///
/// 传入的 `painter` 必须是**视口级** painter（clip = ScrollArea
/// 视口），且所有行的阴影必须在**所有行内容之前**统一画一遍：
/// 外扩晕带才不会被行 clip 裁掉，后一行的阴影也不会压暗前一行
/// 卡片上的文字（落在卡片矩形内的阴影随后被该行填充覆盖）。
/// 计算高斯阴影每一层的 `(外扩距离, 填充 alpha)`，按**从外到内**的
/// 绘制顺序返回（即喂给 `Painter::rect_filled` 的顺序）。
///
/// 纯函数：便于单测模拟「源 over」合成后逐环带核对高斯轮廓。
/// `c` 是卷积黑矩形的强度（边缘处可见强度为其一半）。
pub(super) fn card_shadow_layers(blur: f32, c: f32) -> Vec<(f32, f32)> {
    let blur = blur.clamp(0.0, 24.0);
    if blur <= 0.0 {
        return Vec::new();
    }
    let sigma = blur * 0.5;
    let layers = (blur * 1.5).round().max(1.0) as i32;
    let c = c.clamp(0.0, 0.95);
    let mut out = Vec::new();
    // 实际混合强度（不是目标值）：跳过量化后不可见的层时不更新，
    // 后续层的反解会自动补偿，误差不累积。
    let mut outer = 0.0f32;
    for k in (1..=layers).rev() {
        let target = c * gaussian_outer(k as f32 - 0.5, sigma);
        let a = (target - outer) / (1.0 - outer);
        if a >= 0.003 {
            out.push((k as f32, a));
            outer += a * (1.0 - outer);
        }
    }
    out
}

pub(super) fn draw_card_shadow(
    painter: &egui::Painter,
    r: Rect,
    radius: f32,
    cfg: &modular_clipboard_core::config::CardShadowConfig,
    pal: &Palette,
) {
    if !cfg.enabled || cfg.opacity <= 0.0 {
        return;
    }
    let blur = cfg.blur.clamp(0.0, 24.0);
    if blur <= 0.0 {
        return;
    }
    // 主题推荐强度（`Palette::shadow` 的 alpha）为基准，用户浓度
    // 滑块是倍率：默认 0.16 → 1.0×；拖到 0 → 无阴影。
    let strength = (cfg.opacity / 0.16).clamp(0.0, 1.0 / 0.16);
    let c = pal.shadow.a() as f32 / 255.0 * strength;
    let offset_y = cfg.offset_y.clamp(-8.0, 16.0);
    let base = r.translate(vec2(0.0, offset_y));
    let [sr, sg, sb, _] = pal.shadow.to_array();

    // 主投影：用户调节的模糊/偏移。
    for (grow, a) in card_shadow_layers(blur, c) {
        let rr = base.expand(grow);
        // 半径随外扩同步增大：矩形与圆角始终同心，
        // 且 radius+k ≤ (h+2k)/2 恒成立（由 radius ≤ h/2 保证）。
        painter.rect_filled(
            rr,
            radius + grow,
            egui::Color32::from_rgba_unmultiplied(
                sr,
                sg,
                sb,
                (a * 255.0).round().clamp(0.0, 255.0) as u8,
            ),
        );
    }

    // 近投影（接触阴影）：紧贴卡片的第二组高斯层——更小模糊、更小
    // 偏移、浓度更高，让卡片边缘有「贴地」的锐利过渡。这是立体感的
    // 主要来源（深色背景下黑投影对比有限，靠近投影拉出边缘层次）。
    //
    // 参数与预览页一致：blur×0.35（至少 0.5）、浓度×1.15、偏移×0.25
    // （至少 0.5 下移，即使主偏移为 0 也保留一条贴边接触影）。
    let near_blur = (blur * 0.35).max(0.5);
    let near_offset = (offset_y * 0.25).max(0.5);
    let near_c = (c * 1.15).clamp(0.0, 0.95);
    let near_base = r.translate(vec2(0.0, near_offset));
    for (grow, a) in card_shadow_layers(near_blur, near_c) {
        let rr = near_base.expand(grow);
        painter.rect_filled(
            rr,
            radius + grow,
            egui::Color32::from_rgba_unmultiplied(
                sr,
                sg,
                sb,
                (a * 255.0).round().clamp(0.0, 255.0) as u8,
            ),
        );
    }
}
