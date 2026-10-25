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

// ---------------------------------------------------------------------------
// 设置窗口的悬浮卡投影 / 围绕（2026-10-25 界面改版）
// ---------------------------------------------------------------------------

/// 浅色悬浮卡的三层投影参数：`(模糊, 垂直偏移, 浓度)`。
///
/// 与 HTML 原型 v5 的 CSS `box-shadow` 等价：
/// `0 1px 2px / 0 8px 20px / 0 22px 52px`（颜色 alpha 分别为
/// .06 / .10 / .12）。**浓度 = CSS 颜色 alpha = 卷积强度 `c`**：
/// `gaussian_outer` 的半平面卷积在边缘处取值即为中心强度的一半，
/// 与 CSS 把颜色 alpha 交给模糊去扩散的行为一致。
///
/// 远距层模糊在 [`settings_shadow_bands`] 里被钳到 24（与
/// `card_shadow_layers` 同阈值）：52pt 模糊的最外二十几条带 alpha 已
/// 低于可见阈值，纯浪费形状预算。偏移 22pt 超过典型卡间距（14pt），
/// 远距层实际只落在卡间缝隙里——正是「远距环境影」该在的地方。
const SETTINGS_SHADOW_LIGHT: [(f32, f32, f32); 3] =
    [(2.0, 1.0, 0.06), (20.0, 8.0, 0.10), (52.0, 22.0, 0.12)];

/// 深色主题色光晕：`(模糊, 浓度)`——原型 CSS `0 0 22px rgba(accent,.11)`。
const SETTINGS_GLOW_BLUR: f32 = 22.0;
const SETTINGS_GLOW_C: f32 = 0.11;

/// 深色近场黑影：`(模糊, 垂直偏移, 浓度)`——原型 CSS `0 4px 18px rgba(0,0,0,.38)`。
const SETTINGS_SHADOW_DARK_BLUR: f32 = 18.0;
const SETTINGS_SHADOW_DARK_OFF: f32 = 4.0;
const SETTINGS_SHADOW_DARK_C: f32 = 0.38;

/// 主题色环的透明度（原型 `--accent-line: rgba(132,183,235,.38)`）。
const SETTINGS_RING_ALPHA: f32 = 0.38;

/// 顶部受光高光的白色透明度（原型 `inset 0 1px 0 rgba(255,255,255,.05)`）。
const SETTINGS_HIGHLIGHT_ALPHA: u8 = 13;

/// 设置窗口悬浮卡的分主题投影 / 围绕（HTML 原型 v5 定稿）。
///
/// # 分主题策略（2026-10-25 用户确认）
///
/// - **浅色**：三层黑影（贴地 / 中距 / 远距，CSS `box-shadow` 等价
///   参数）。浅底上黑投影有明度差，「浮起」直接由投影表达。
/// - **深色**：黑投影在近黑窗口底（`#121519`）上**几乎不可见**
///   （`theme.rs` 对 `shadow` 令牌的同一结论：深色 elevation 必须
///   靠亮环绕射），改用**主题色围绕**——22pt 主题色光晕 + 一层近场
///   黑影兜底。
///
/// # 为什么是「返回形状」而不是直接画
///
/// 卡片高度由内容决定——`Frame::show` 返回后才知道外矩形，投影没有
/// 「先于填充」的绘制机会。解法是调用方在**帧背景之前**插一枚
/// `Shape::Noop` 占位符（`ui.painter().add(Shape::Noop)`），内容画完
/// 后用 `Painter::set` 把本函数返回的形状写回那个索引：占位符早于帧
/// 背景 ⇒ 晕环位于填充与内容**之下**，外扩矩形盖住卡片内部的部分被
/// 不透明填充遮住，只剩卡片四周的晕带可见。
///
/// 2026-10-25 曾用「四条 1pt 环带、画在内容之后」的实现，已废弃：
/// ① 一帧画出近千条细带（诊断实测 light 894 / dark 1008），既费又
/// 把高斯梯度量化成硬阶跃，小模糊层读出一排细线；② 环带是方角的，
/// 卡片 16pt 圆角处晕环呈像素化折线；③ 三层的带互相互相叠加，合成
/// 结果偏离高斯轮廓。外扩圆角矩形 + 源 over 反解 alpha（本函数的
/// 数学）没有这三个问题——它与列表行卡片 `draw_card_shadow` 同族。
///
/// 返回 `Shape::Vec`（N 层同心外扩圆角矩形，从外到内）。
pub(super) fn settings_halo_shape(r: egui::Rect, radius: f32, pal: &Palette) -> egui::Shape {
    let mut shapes: Vec<egui::Shape> = Vec::new();
    if pal.is_dark {
        push_halo_layers(
            &mut shapes,
            r,
            SETTINGS_GLOW_BLUR,
            SETTINGS_GLOW_C,
            0.0,
            pal.accent,
            radius,
        );
        push_halo_layers(
            &mut shapes,
            r,
            SETTINGS_SHADOW_DARK_BLUR,
            SETTINGS_SHADOW_DARK_C,
            SETTINGS_SHADOW_DARK_OFF,
            egui::Color32::BLACK,
            radius,
        );
    } else {
        for (blur, off, c) in SETTINGS_SHADOW_LIGHT {
            push_halo_layers(&mut shapes, r, blur, c, off, egui::Color32::BLACK, radius);
        }
    }
    egui::Shape::Vec(shapes)
}

/// 把一层高斯投影的外扩圆角矩形序列追加进 `out`（从外到内）。
///
/// alpha 直接取 [`card_shadow_layers`] 的**源 over 反解**结果：层与层
/// 重叠（外扩矩形互相包含），后画的盖先画的，反解保证合成后的环带
/// 强度精确等于高斯目标值（误差 ±1/255）。矩形与圆角始终同心
/// （`radius + grow`），卡片圆角处晕环跟着圆，不会出方角。
///
/// 每层颜色由 [`diffuse_halo_color`] 分摊（目标色 × 本层 alpha + 误差
/// 扩散）——不能逐层 `fade_color`，理由见那个函数的文档（曾实测合成
/// 红色 164 vs 目标 132）。
fn push_halo_layers(
    out: &mut Vec<egui::Shape>,
    r: egui::Rect,
    blur: f32,
    c: f32,
    offset_y: f32,
    color: egui::Color32,
    radius: f32,
) {
    let layers = card_shadow_layers(blur, c);
    let [cr, cg, cb, _] = color.to_srgba_unmultiplied();
    let colors = diffuse_halo_color(&layers, [cr, cg, cb]);
    let base = r.translate(vec2(0.0, offset_y));
    for ((grow, a), fill) in layers.iter().zip(colors) {
        out.push(egui::Shape::Rect(egui::epaint::RectShape::filled(
            base.expand(*grow),
            radius + grow,
            fill,
        )));
        let _ = a;
    }
}

/// 把目标色分摊成各层的 8 位预乘颜色（每层理想值 + 误差扩散）。
///
/// # 每层的理想值 = 目标色 × 本层 alpha
///
/// 源 over 合成公式 `comp = Σ pv_i·remain_i / Σ a_i·remain_i` 里，
/// `remain_i` 权重恰好与本层 alpha 对分母的贡献抵消：代入
/// `pv_i = rgb·a_i` 恒等于目标色。（曾按「合成权重 `a_i·remain_i`」
/// 分摊——多乘一个 `remain_i`，系统性低估内层约 5%，实测合成蓝
/// 219 vs 目标 235。）
///
/// # 为什么还必须扩散（不能逐层独立舍入）
///
/// 反解后单层 alpha 只有 1~2/255（合成靠多层累加，单层本来就小），
/// 理想预乘通道 `132·1/255 = 0.52` 落在半个量化步长上：独立舍入会
/// 系统性地全向上取整（0.52→1），而**合成色的误差放大倍数是
/// `255/a_total ≈ 19`**（a_total ≈ 13.65/255）——每层 ±0.5 的通道
/// 误差被放大成 ±9~33 的合成色偏差（实测红色 164 vs 目标 132）。
///
/// 误差扩散把余数递带到下一层，使各层取整误差正负交替、加权和趋零：
/// 剩余误差只有**最后一层之后**的残余余数（≤ 0.5），合成色偏差
/// 从 ±33 降到 ~±3（最坏 ±9）。
///
/// NOTE: 该误差只影响**彩色**投影（深色主题色光晕）。浅色三层是纯黑
/// 投影，通道恒 0，与 alpha 无关，完全不受量化影响。
///
/// `layers` 是 `(grow, alpha)`，**从外到内**（绘制顺序）。
fn diffuse_halo_color(layers: &[(f32, f32)], rgb: [u8; 3]) -> Vec<egui::Color32> {
    let mut out = Vec::with_capacity(layers.len());
    let mut debt = [0.0f32; 3]; // 每通道未摊销完的余数
    for &(_, a) in layers {
        let alpha_u8 = (a * 255.0).round().clamp(0.0, 255.0) as u8;
        let mut ch = [0u8; 3];
        for i in 0..3 {
            let ideal = f32::from(rgb[i]) * f32::from(alpha_u8) / 255.0 + debt[i];
            // 预乘通道不许超过自身 alpha（`rgb ≤ 255` 时本就自动满足，
            // 钳位是防御性的）。
            let q = ideal.round().clamp(0.0, f32::from(alpha_u8));
            ch[i] = q as u8;
            debt[i] = ideal - q;
        }
        out.push(egui::Color32::from_rgba_premultiplied(
            ch[0], ch[1], ch[2], alpha_u8,
        ));
    }
    out
}

/// 悬浮卡的**边缘装饰**：深色主题色环 + 顶部受光高光。
///
/// 与 [`settings_halo_shape`] 分工：晕环在填充之下，本函数画在填充
/// 之上（描边要被看见必须叠在填充上）——调用方在卡片内容画完之后调。
/// 浅色主题不发环（边由 `Frame` 的 `border_subtle` 发丝边表达）。
pub(super) fn draw_settings_card_edge(
    painter: &egui::Painter,
    r: egui::Rect,
    radius: f32,
    pal: &Palette,
) {
    if !pal.is_dark {
        return;
    }
    // 顶部受光高光：贴在卡片内上缘的 1pt 亮线。裁剪在卡片矩形内 ⇒
    // 亮线自然沿圆角收边，不会戳出弧外。
    painter
        .with_clip_rect(r)
        .line_segment(
            [
                egui::pos2(r.min.x + radius, r.min.y + 0.5),
                egui::pos2(r.max.x - radius, r.min.y + 0.5),
            ],
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(SETTINGS_HIGHLIGHT_ALPHA)),
        );
    // 主题色环：1pt Inside（比背景亮 ⇒ 没有 shadow.rs 顶部记录的
    // 「暗描边在圆角凸角处 alpha 累加成暗斑」问题）。
    painter.rect_stroke(
        r,
        radius,
        egui::Stroke::new(1.0, fade_color(pal.accent, SETTINGS_RING_ALPHA)),
        egui::StrokeKind::Inside,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    /// 源 over 合成的晕环强度必须精确等于高斯轮廓。
    ///
    /// 这条守住 `settings_halo_shape` 的数学前提：外扩矩形层与层互相
    /// 包含，alpha 若不按「源 over」反解，合成结果会偏离高斯（正是
    /// 已废弃的 1pt 环带实现的病根之一）。从最外层向内逐层模拟合成，
    /// 每个环带的落点强度应与 `c · gaussian_outer(k-0.5, σ)` 一致。
    #[test]
    fn halo_layers_composite_to_the_gaussian_profile() {
        for (blur, c) in [(2.0_f32, 0.06), (20.0, 0.10), (24.0, 0.12), (18.0, 0.38)] {
            let sigma = blur * 0.5;
            let mut outer = 0.0_f32; // 已合成部分（更外层的贡献）
            for (grow, a) in card_shadow_layers(blur, c) {
                let target = c * gaussian_outer(grow - 0.5, sigma);
                let composite = a + outer * (1.0 - a);
                assert!(
                    (composite - target).abs() < 0.01,
                    "blur={blur} c={c} 第 {grow} 环带合成强度 {composite:.3}                      应等于高斯目标 {target:.3}"
                );
                outer = composite;
            }
        }
    }

    /// 晕环必须非空，且每层矩形都**包含卡片矩形**（外扩同心）。
    ///
    /// 空 Vec = 投影整个丢了；矩形不含卡片 = 层与卡片不同心，圆角处
    /// 会露出缺口（已废弃的方角环带实现的病根之二）。
    #[test]
    fn halo_shape_is_concentric_and_non_empty() {
        let card = Rect::from_min_max(pos2(100.0, 100.0), pos2(400.0, 300.0));
        for pal in [Palette::dark(), Palette::light()] {
            let egui::Shape::Vec(shapes) = settings_halo_shape(card, 16.0, &pal) else {
                panic!("晕环应为 Shape::Vec");
            };
            assert!(!shapes.is_empty(), "{:?} 主题晕环不应为空", pal.is_dark);
            for s in &shapes {
                let egui::Shape::Rect(r) = s else {
                    panic!("晕环成员应为 Rect");
                };
                assert!(
                    r.rect.min.x <= card.min.x && r.rect.max.x >= card.max.x,
                    "矩形 {:?} 应横向包含卡片",
                    r.rect
                );
            }
        }
    }

    /// 深色光晕**合成后**的颜色必须等于主题色（误差扩散的守门）。
    ///
    /// 按源 over 模拟各层合成（`comp = Σ pv·remain / Σ a·remain`），
    /// 检查最终的**非预乘**颜色落在主题色邻域内。
    ///
    /// # 容差 12 的依据
    ///
    /// 合成色误差的放大倍数是 `255/a_total ≈ 19`（单层 alpha 只有
    /// 1~2/255，见 `diffuse_halo_color` 文档）。误差扩散把逐层取整
    /// 误差摊成交替 pattern 后，残余只有最后一层之后的 debt（≤ 0.5）
    /// ⇒ 合成色偏差 ~±3、最坏 ±9；独立舍入则是 ±33（曾实测红 164
    /// vs 132）。留 12 的余量覆盖最坏情况。
    ///
    /// # 视觉量级
    ///
    /// 光晕合成不透明度只有 ~5.4%，±12 的色相差落到最终像素上是
    /// ±0.6/255——远低于可见阈值。这条测试守的是「色相不被量化
    /// 系统性抹偏」，不是像素级精度。
    #[test]
    fn dark_halo_composites_to_the_accent_color() {
        let card = Rect::from_min_max(pos2(0.0, 0.0), pos2(200.0, 100.0));
        let pal = Palette::dark();
        let egui::Shape::Vec(shapes) = settings_halo_shape(card, 16.0, &pal) else {
            panic!("晕环应为 Shape::Vec");
        };
        // 光晕层 = 零偏移的那批（矩形中心与卡片中心重合）。
        let glow: Vec<&egui::epaint::RectShape> = shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::Rect(r) => Some(r),
                _ => None,
            })
            .take_while(|r| (r.rect.center().y - card.center().y).abs() < 0.01)
            .collect();
        assert!(!glow.is_empty(), "深色晕环应有零偏移的主题色光晕层");

        // 源 over 合成（绘制顺序 = Vec 顺序，从外到内）。
        let mut comp = [0.0f32; 3];
        let mut a_total = 0.0f32;
        for r in &glow {
            let a = f32::from(r.fill.a()) / 255.0;
            let remain = 1.0 - a_total;
            a_total += a * remain;
            let [pr, pg, pb, _] = r.fill.to_array();
            for (i, pv) in [pr, pg, pb].iter().enumerate() {
                comp[i] += f32::from(*pv) / 255.0 * remain;
            }
        }
        assert!(a_total > 0.01, "光晕合成不透明度应可感知（实测 {a_total}）");
        let [ar, ag, ab, _] = pal.accent.to_srgba_unmultiplied();
        let got = [
            comp[0] / a_total * 255.0,
            comp[1] / a_total * 255.0,
            comp[2] / a_total * 255.0,
        ];
        let want = [f32::from(ar), f32::from(ag), f32::from(ab)];
        for i in 0..3 {
            assert!(
                (got[i] - want[i]).abs() <= 6.0,
                "合成色通道 {i} = {:.0} 应约等于主题色 {:.0}（色相被量化抹掉？）",
                got[i],
                want[i]
            );
        }
    }
}
