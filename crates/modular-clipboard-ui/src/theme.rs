//! 主题与字体。
//!
//! 字体是关键：egui 内置字体不含 CJK 字形，若不加载系统中文字体，
//! 所有中文都会渲染成方块。因此启动时按优先级探测系统字体并注册为
//! Proportional 的最高优先级回退。
//!
//! # 设计令牌
//!
//! [`Palette`] 是**唯一**的颜色与非颜色来源：egui 内置控件的观感由
//! [`apply`] 从它灌入 [`egui::Visuals`]，自绘图形（`view.rs` 的
//! `paint_inline`、`icons.rs` 的图标）直接读它的字段。两条路径同源，
//! 主题切换时才不会出现「一半控件不跟着变」。
//!
//! # 命名约定：语义而非外观
//!
//! 字段名描述**用途**（`text_dim` / `row_selected` / `danger`），
//! 不描述外观（没有 `gray_light` / `blue_accent` 这类名字）。
//! 原因：同一个语义在深浅两套下的外观本就不同——浅色模式的 `warn`
//! 是压深的琥珀色（要压深才够 4.5:1），深色模式是亮琥珀。
//! 按外观命名会让「换配色方案」变成逐个字段追��改名。
//!
//! # 对比度
//!
//! 所有取值经 [`tests`] 中的 WCAG 校验：正文 4.5:1、非文本组件 3:1。
//! 半透明令牌（`row_alt` / `row_hover` / `overlay_scrim`）的对比度
//! 一律**先合成到实际背景**再计算——面板叠在窗口底色上，
//! 直接拿半透明值算会高估。唯一豁免是 `border_subtle`：
//! 它是弱分隔线，不承载控件边界信息。

use std::time::Duration;


mod secondary;
mod accent;
mod system;
mod visuals;
mod fonts;

pub use accent::ACCENT_PRESETS;
pub use fonts::{install_cjk_font, install_icon_font, sized};
pub use fonts::emphasis;
pub use system::detect_system_theme;
pub use visuals::{SCROLLBAR_WIDTH, apply, composite, set_theme};
use accent::{apply_overrides, derive_accent};
use secondary::apply_secondary;


/// 一个令牌的值。供完整性测试遍历用。
#[derive(Debug, Clone, PartialEq)]
pub enum TokenValue {
    Color(egui::Color32),
    Num(f32),
    Dur(Duration),
}

/// 把令牌类别映射到字段类型。
macro_rules! palette_ty {
    (color) => { egui::Color32 };
    (num) => { f32 };
    (dur) => { Duration };
}

/// 把 `(类别, 值)` 包成 [`TokenValue`]。
macro_rules! palette_token {
    (color, $v:expr) => { TokenValue::Color($v) };
    (num, $v:expr) => { TokenValue::Num($v) };
    (dur, $v:expr) => { TokenValue::Dur($v) };
}

/// 按类别把一个 [`TokenValue`] 写进字段（供 [`Palette::set_token`] 展开）。
///
/// 类型不符返回 `false` 且**不写**——调用方（覆盖表应用）据此跳过
/// 与当前版本令牌表漂移的条目。
macro_rules! set_token_field {
    (color, $self:expr, $name:ident, $v:expr) => {
        if let TokenValue::Color(c) = $v {
            $self.$name = c;
            true
        } else {
            false
        }
    };
    (num, $self:expr, $name:ident, $v:expr) => {
        if let TokenValue::Num(n) = $v {
            $self.$name = n;
            true
        } else {
            false
        }
    };
    (dur, $self:expr, $name:ident, $v:expr) => {
        if let TokenValue::Dur(d) = $v {
            $self.$name = d;
            true
        } else {
            false
        }
    };
}

/// 调色板按 `t ∈ [0,1]` 插值时逐字段的取值规则（F-34 / UI-P1-3）。
///
/// 只有**颜色**参与补间。几何令牌（圆角/间距/字号）与时长令牌直接取
/// 目标值：它们瞬时生效不产生可感知的「过渡中」状态，插值只会让布局
/// 在 220ms 里持续抖动——那比硬切更糟。
macro_rules! palette_lerp {
    (color, $a:expr, $b:expr, $t:expr) => {{
        // sRGB 分量线性插值。对 220ms 的短暂过渡，人眼对中间色不敏感，
        // 感知均匀（oklab）插值的成本不划算。
        //
        // CAUTION: `to_array()` 返回的是**预乘**分量（Color32 内部预乘
        // alpha 存储），在预乘空间插值本来就是正确做法（避免半透明
        // 边缘出现暗晕），但重组时必须用 `from_rgba_premultiplied`——
        // 用 `from_rgba_unmultiplied` 会二次预乘，主题过渡途中半透明
        // 令牌（row_hover 等）整体发暗（2026-10-17 修复）。
        let [ar, ag, ab, aa] = $a.to_array();
        let [br, bg, bb, ba] = $b.to_array();
        let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * $t).round() as u8;
        egui::Color32::from_rgba_premultiplied(m(ar, br), m(ag, bg), m(ab, bb), m(aa, ba))
    }};
    (num, $a:expr, $b:expr, $_t:expr) => { $b };
    (dur, $a:expr, $b:expr, $_t:expr) => { $b };
}

/// 背景层变体：`color` 类且属于背景层时整体降 alpha，其余原样返回。
///
/// 供 [`Palette::with_backdrop_alpha`] 逐字段展开使用。四个背景层名
/// 是**编译期**常量分支——新增背景 token 必须同步加入这里的名单，
/// 否则磨砂/半透明模式下新背景不透明、遮住透出的模糊底图。
macro_rules! backdrop_layer_value {
    (color, $name:ident, $v:expr, $alpha:expr) => {{
        if matches!(stringify!($name), "bg" | "surface" | "surface_variant" | "list_bg") {
            $v.gamma_multiply($alpha as f32 / 255.0)
        } else {
            $v
        }
    }};
    (num, $name:ident, $v:expr, $_alpha:expr) => { $v };
    (dur, $name:ident, $v:expr, $_alpha:expr) => { $v };
}

/// 字体令牌的缩放变体：`font_*` 与**承载文字的几何令牌**乘以 `scale`。
///
/// 供 [`Palette::with_font_scale`] 逐字段展开使用。判据用字段名前缀
/// 而不是另维护一张名单：新增字号令牌时忘记登记会**静默不缩放**
/// （用户看到「只有一部分文字跟着放大」），前缀判据没有这个缺口。
///
/// # 为什么 `control_height` / `icon_size` 也要跟着缩放
///
/// 控件高度是**为了容纳文字**而存在的：字号放大 1.5× 而输入框仍是
/// 28pt 高，文字会被裁掉半截（用户反馈「字体调整太大导致布局错乱」）。
/// 图标与图标按钮同理——图标尺寸固定而周围文字放大，视觉比例会崩。
///
/// 圆角、描边、间距**不缩放**：它们与字号不是线性关系（1.5× 的圆角
/// 会让卡片变成胶囊），且留白不缩放正好让大字号下更紧凑。
macro_rules! font_scaled_value {
    (num, $name:ident, $v:expr, $scale:expr) => {{
        if stringify!($name).starts_with("font_")
            || matches!(stringify!($name), "control_height" | "icon_size")
        {
            $v * $scale
        } else {
            $v
        }
    }};
    (color, $name:ident, $v:expr, $_scale:expr) => { $v };
    (dur, $name:ident, $v:expr, $_scale:expr) => { $v };
}

/// 定义 [`Palette`] 的字段与两套取值。
///
/// 用宏而不是手写两个 `struct` 字面量，是为了让「深浅两套字段对称」
/// 由**构造**保证：新增字段时忘记给浅色版赋值会直接编译失败，
/// 而不是静默漏掉一半、运行后才发现某个控件在浅色下没跟着变。
/// 「换一个配色方案」也由此变成只改这张表的第三、四列。
macro_rules! palette {
    (
        $(
            $(#[$meta:meta])*
            $name:ident : $kind:ident = $dark:expr => $light:expr
        ),* $(,)?
    ) => {
        /// 配色与尺寸令牌。集中定义，避免界面里出现魔法数字。
        ///
        /// `Clone`：主题补间（[`Palette::lerp`]）每帧都要从源/目标两份
        /// 派生显示中的调色板，没有 Clone 就得逐字段手抄。
        ///
        /// `PartialEq`：主题补间走完后帧循环要判断「显示值是否已等于
        /// 目标值」——`tween_f32` 到点后直接返回目标值，`mix < 1.0` 的
        /// 分支不再执行，显示值会停在最后一帧的**近似**值上。
        /// 逐字段比较（55 个令牌）只在补间结束后的那一帧发生一次。
        ///
        /// `Debug`：主题解析链（二级主题 / 强调色 / 覆盖）层层派生，
        /// 出问题时需要把整份调色板打出来对比，没有 Debug 只能逐字段
        /// 手抄。
        #[derive(Clone, PartialEq, Debug)]
        pub struct Palette {
            $(
                $(#[$meta])*
                pub $name: palette_ty!($kind),
            )*

            /// 本调色板是否为深色。
            ///
            /// 由 [`Palette::dark`] / [`Palette::light`] 写入，
            /// [`apply`] 用它设置 `Visuals::dark_mode`。
            ///
            /// 不是设计令牌，故不进 [`Palette::tokens`]——它是元信息。
            pub is_dark: bool,
        }

        impl Palette {
            /// 深色主题。
            pub fn dark() -> Self {
                Self { $( $name: $dark, )* is_dark: true }
            }

            /// 浅色主题。
            pub fn light() -> Self {
                Self { $( $name: $light, )* is_dark: false }
            }

            /// 遍历全部设计令牌（字段名 + 值），供完整性测试使用。
            ///
            /// 不含 [`Palette::is_dark`]。
            pub fn tokens(&self) -> Vec<(&'static str, TokenValue)> {
                vec![$((stringify!($name), palette_token!($kind, self.$name))),*]
            }

            /// 按名字写单个令牌（[`ThemeOverrides`] 的落地入口）。
            ///
            /// 与 [`Palette::tokens`] 互为逆操作：设置界面的「高级微调」
            /// 用 `tokens()` 列出全部可调项，用本函数落地。返回 `false`
            /// 表示名字或类型不匹配（调用方应忽略而不是 panic——
            /// 覆盖表可能来自旧版本配置，令牌会被增删）。
            ///
            /// 类型不匹配时**不改动**：`nums` 表里的键指向一个颜色令牌
            /// （或反之）说明配置与当前版本的令牌表已漂移，静默跳过
            /// 比把颜色字段写成数值再在绘制侧炸掉好。
            pub fn set_token(&mut self, name: &str, value: TokenValue) -> bool {
                $(
                    if name == stringify!($name) {
                        return set_token_field!($kind, self, $name, value);
                    }
                )*
                false
            }

            /// 与另一份调色板按 `t ∈ [0,1]` 插值（F-34 / UI-P1-3）。
            ///
            /// 主题切换用：整窗瞬跳改成 220ms 的颜色渐变——纯平毛玻璃的
            /// 审美下，背景模糊内容不变而全部前景色瞬换尤其刺眼。
            ///
            /// 只插值颜色字段；几何/时长令牌取目标值（见
            /// [`palette_lerp`] 的理由）。`is_dark` 在中点翻转，
            /// 避免出现「深色文字配浅色底」的中间态。
            pub fn lerp(&self, other: &Palette, t: f32) -> Palette {
                let t = t.clamp(0.0, 1.0);
                Palette {
                    $(
                        $name: palette_lerp!($kind, self.$name, other.$name, t),
                    )*
                    is_dark: if t >= 0.5 { other.is_dark } else { self.is_dark },
                }
            }

            // ---- 旧字段别名 ----
            //
            // `panel_bg` 与上面的 `surface` 取值完全一致，保留是为了不
            // 破坏现有调用点（`view.rs` / `app.rs`）。新代码请直接用
            // `surface` / `accent` / `warn`——等调用点迁移完再删。

            /// 等同 [`Palette::surface`]。旧名，兼容保留。
            pub fn panel_bg(&self) -> egui::Color32 {
                self.surface
            }

            /// 由模式取调色板。等价于 [`ThemeMode::palette`]，
            /// 放在 [`Palette`] 上是为了让调用点写成
            /// `Palette::from_mode(mode)` 这样更顺。
            pub fn from_mode(mode: crate::theme::ThemeMode) -> Palette {
                mode.palette()
            }

            /// 背景透明变体：背景层（`bg` / `surface` / `surface_variant`
            /// / `list_bg`）整体降 alpha，内容层（文字/卡片/状态色）
            /// 保持不变。
            ///
            /// 磨砂 / 半透明模式下用：窗口背景画半透明，磨砂底图
            /// （自研实时模糊）或桌面透过这些区域显示；卡片与文字
            /// 仍不透明保证可读性。
            ///
            /// `alpha` 取值 0-255，255 等于原色（不透明）。
            pub fn with_backdrop_alpha(&self, alpha: u8) -> Palette {
                Self {
                    $( $name: backdrop_layer_value!($kind, $name, self.$name, alpha), )*
                    is_dark: self.is_dark,
                }
            }

            /// 卡片透明变体：只把「行卡片底色」（`card_bg`）降 alpha，
            /// 其余令牌原样。
            ///
            /// # 为什么不能改 `surface_raised`
            ///
            /// `surface_raised` 是**浮层**令牌：通知气泡、设置窗口面板、
            /// 类型徽标都在用它。早前本方法改的是它，于是「把列表卡片调
            /// 透」会连带把那些浮层一起弄透——它们身后没有磨砂底图，
            /// 透出来的只有窗口底色，观感是「整片界面一起变暗」。
            ///
            /// 现在改的是行卡片专用的 `card_bg`，浮层不受影响。
            ///
            /// # 为什么用 `gamma_multiply`
            ///
            /// egui 的 `Color32` 是**预乘**存储，`gamma_multiply` 四个
            /// 通道同比例缩放（rgb 与 alpha 一起降）——这正是「同色更淡」
            /// 而不是「变暗」的语义。只降 rgb 或只降 alpha 都会偏色。
            ///
            /// `alpha` 取值 0-255，255 等于原色（不透明）。
            pub fn with_card_opacity(&self, alpha: u8) -> Palette {
                let mut p = self.clone();
                p.card_bg = p.card_bg.gamma_multiply(alpha as f32 / 255.0);
                p
            }

            /// 字体缩放变体：`font_*` 令牌乘以 `scale`，其余令牌原样。
            ///
            /// # 为什么必须从**基础调色板**派生，而不是在显示值上再乘一次
            ///
            /// 这是幂等的纯函数：`base.with_font_scale(1.15)` 永远等于
            /// 「基础字号 × 1.15」。调用方（`App`）因此可以放心地每次
            /// 从 `pal_base` 重新派生；若在**已缩放**的调色板上再调一次，
            /// 会变成叠乘（1.15 × 1.15 = 1.3225），字号越调越大。
            ///
            /// `scale` 由 `UiConfig::font_scale` 提供，取值 0.75~2.0
            /// （写入时已钳制）。非有限值按 1.0 处理——NaN 会让所有
            /// 字号变 NaN，egui 的排版会静默产出空 galley（文字消失）。
            pub fn with_font_scale(&self, scale: f32) -> Palette {
                let s = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
                Self {
                    $( $name: font_scaled_value!($kind, $name, self.$name, s), )*
                    is_dark: self.is_dark,
                }
            }
        }
    };
}

palette! {
    // ---- 背景层 ----
    //
    // 2026-10-20 深色基底重调（基础主题，无二级主题时）：
    // 旧值四层背景（#14161A / #1E2127 / #262A31 / #2E333B）层间只差
    // 约 8/通道，整窗读成「一片灰」——亮卡片浮不浮出来全靠投影与
    // 高光兜。现在把**层间梯度拉开**（bg 更深、raised 更亮），同时
    // 保持冷蓝灰家族一致（R<G<B，与文字/描边同族）：
    //   bg #121519 < list_bg #0E1014（列表容器再暗一档）
    //   < surface #1D2128 < surface_variant #262B34 < surface_raised #2F3540
    /// 窗口最底层背景。
    bg: color = egui::Color32::from_rgb(0x12, 0x15, 0x19)
              => egui::Color32::from_rgb(0xF2, 0xF4, 0xF7),
    /// 面板 / 卡片背景。
    ///
    /// 旧字段 `panel_bg` 是本令牌的别名，保留是为了不破坏调用点。
    surface: color = egui::Color32::from_rgb(0x1D, 0x21, 0x28)
                => egui::Color32::from_rgb(0xFA, 0xFB, 0xFC),
    /// 次级面板（嵌套容器）。
    surface_variant: color = egui::Color32::from_rgb(0x26, 0x2B, 0x34)
                       => egui::Color32::from_rgb(0xEC, 0xEF, 0xF3),
    /// 列表显示区域背景。比窗口底 `bg` **更暗一档**，把亮卡片衬托出来。
    ///
    /// 历史列表的整块可视区铺这一层（`draw_history_body` 开头），
    /// 卡片用 `surface_raised` 浮在它之上——「亮卡片、暗容器」。
    list_bg: color = egui::Color32::from_rgb(0x0E, 0x10, 0x14)
              => egui::Color32::from_rgb(0xE8, 0xEB, 0xF0),
    /// 浮层：菜单、气泡、可拖拽模块。
    surface_raised: color = egui::Color32::from_rgb(0x2F, 0x35, 0x40)
                    => egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
    /// 列表行卡片底色。**只被列表行卡片读取**。
    ///
    /// # 为什么必须单独一个令牌
    ///
    /// 「卡片透明度」设置早前改的是 `surface_raised`，而那个令牌还被
    /// 通知气泡、设置窗口面板、类型徽标复用——于是把卡片调透会**连带
    /// 把那些浮层一起弄透**。它们身后没有磨砂底图可透，透出来的只有
    /// 窗口底色，观感就是「整片界面一起变暗」（2026-10-23 用户反馈
    /// 「卡片整体都变暗而不是变透明」）。
    ///
    /// 浮层的透明度是**另一件事**：它们要有可读的底，不该跟着列表卡片
    /// 的观感设置走。所以卡片透明度必须是只作用于行卡片的派生。
    card_bg: color = egui::Color32::from_rgb(0x2F, 0x35, 0x40)
             => egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
    /// 遮罩。半透明，压在 `bg` 之上。
    overlay_scrim: color = egui::Color32::from_black_alpha(170)
                     => egui::Color32::from_rgba_unmultiplied(0x10, 0x14, 0x1A, 110),

    // ---- 描边层 ----
    /// 常规描边。非文本组件需达 3:1。
    border: color = egui::Color32::from_rgb(0x6E, 0x78, 0x86)
              => egui::Color32::from_rgb(0x7C, 0x86, 0x92),
    /// 强调描边（聚焦态）。
    border_strong: color = egui::Color32::from_rgb(0x9C, 0xA6, 0xB4)
                     => egui::Color32::from_rgb(0x4A, 0x55, 0x62),
    /// 弱分隔线。**不要求** 3:1：它不承载控件边界信息，
    /// 强求高对比会让面板之间全是硬边，视觉噪声大于收益。
    ///
    /// 2026-10-20 随基底重调微亮一档（#2A2F36 → #2A2F38）：新 bg
    /// 更深之后旧值几乎看不见，分隔线要在「不抢戏」与「还看得见」
    /// 之间重新找平衡。
    border_subtle: color = egui::Color32::from_rgb(0x2A, 0x2F, 0x38)
                     => egui::Color32::from_rgb(0xDD, 0xE2, 0xE8),
    /// 卡片投影基色（列表卡片阴影，见 `draw_card_shadow`）。
    ///
    /// RGB 都是黑色（投影的物理色），**alpha 是主题推荐的「卷积黑
    /// 矩形」强度**：用户配置 `card_shadow.opacity = 0.16`（1.0×）
    /// 时按此 alpha 出图，滑块再线性缩放（默认 0.20 ≈ 1.25×）。
    ///
    /// 深色模式（1.00）比浅色（0.50）浓一倍：列表底 `list_bg`
    /// 深色为 `#0E1014`、近黑，黑投影叠上去几乎没有明度差，
    /// 这是物理必然，不是参数问题——Material 的深色 elevation
    /// 阴影同样靠提高 alpha 解决。2026-10-15 预览页验收后从
    /// 204/102 提到 255/128，深色主题阴影从此清晰可见。
    ///
    /// 注意：投影实际可见的边缘强度约为该 alpha 的一半
    /// （高斯卷积在半平面边缘处为 0.5），衰减到 1.5 倍模糊半径
    /// 处已 < 0.2%，不会留下硬边。
    shadow: color = egui::Color32::from_rgba_unmultiplied(0, 0, 0, 255)
                    => egui::Color32::from_rgba_unmultiplied(0, 0, 0, 128),
    /// 卡片顶部受光高光（「立体阴影」的一部分，见 `draw_item_row`）。
    ///
    /// 深色主题下是卡片顶部 3pt 的白色渐变亮边（alpha 0.20 → 0），
    /// 模拟光源从上方照下，让深色卡片在近黑背景上「浮」起来——
    /// 深色投影本身在深背景上对比有限，高光才是立体感的主要来源。
    /// 浅色主题卡片自身最亮（#FFFFFF），高光为透明（不绘制）。
    card_highlight: color = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 51)
                      => egui::Color32::TRANSPARENT,

    // ---- 文本层 ----
    /// 主要文字。
    text: color = egui::Color32::from_rgb(0xE3, 0xE6, 0xE8)
           => egui::Color32::from_rgb(0x24, 0x29, 0x2E),
    /// 次要文字。
    ///
    /// 2026-10-20 去掉纯中性灰：旧值 #C2C2C2 在冷蓝灰基底上略发
    /// 「脏」，微调进家族色相（#BCC2CB）后与 bg/surface/描边同族，
    /// 长段落读起来不再忽明忽暗。亮度基本不变，对比度守门不受影响。
    text_dim: color = egui::Color32::from_rgb(0xBC, 0xC2, 0xCB)
               => egui::Color32::from_rgb(0x54, 0x54, 0x54),
    /// 强调文字：数值、标题。
    text_bright: color = egui::Color32::from_rgb(0xFC, 0xFC, 0xFC)
                   => egui::Color32::from_rgb(0x0F, 0x0F, 0x0F),
    /// 强调色之上的文字。深色主题的强调色偏亮，故取近黑；
    /// 浅色主题的强调色偏深，故取纯白。
    text_on_accent: color = egui::Color32::from_rgb(0x0A, 0x0D, 0x12)
                     => egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),

    // ---- 状态色 ----
    //
    // 三态（normal / hover / active）**共用亮度档位，区分靠饱和度**。
    // 这不是省事：亮度一旦为了表达「按下」而下调，状态色作为前景
    // 会掉出 4.5:1，作为填充底色又要求更深（浅色主题），两者不可兼得。
    // 饱和度差异既能表达交互态，又不牺牲可读性。
    //
    /// 强调色（选中、主操作）。
    accent: color = egui::Color32::from_rgb(0x84, 0xB7, 0xEB)
             => egui::Color32::from_rgb(0x1D, 0x65, 0xAF),
    /// `accent` 悬停态。
    accent_hover: color = egui::Color32::from_rgb(0x7E, 0xB6, 0xF1)
                   => egui::Color32::from_rgb(0x14, 0x64, 0xB8),
    /// `accent` 按下态。
    accent_active: color = egui::Color32::from_rgb(0x79, 0xB6, 0xF6)
                    => egui::Color32::from_rgb(0x0C, 0x64, 0xC0),
    /// 成功。
    success: color = egui::Color32::from_rgb(0xA0, 0xDA, 0xAE)
              => egui::Color32::from_rgb(0x30, 0x7B, 0x43),
    /// `success` 悬停态。
    success_hover: color = egui::Color32::from_rgb(0x9A, 0xDF, 0xAB)
                    => egui::Color32::from_rgb(0x27, 0x7C, 0x3D),
    /// `success` 按下态。
    success_active: color = egui::Color32::from_rgb(0x95, 0xE4, 0xA9)
                     => egui::Color32::from_rgb(0x1F, 0x7C, 0x36),
    /// 警示。
    warn: color = egui::Color32::from_rgb(0xEB, 0xC9, 0x8E)
           => egui::Color32::from_rgb(0x90, 0x64, 0x19),
    /// `warn` 悬停态。
    warn_hover: color = egui::Color32::from_rgb(0xF2, 0xCA, 0x88)
                 => egui::Color32::from_rgb(0x93, 0x63, 0x10),
    /// `warn` 按下态。
    warn_active: color = egui::Color32::from_rgb(0xF8, 0xCC, 0x81)
                  => egui::Color32::from_rgb(0x96, 0x61, 0x08),
    /// 危险：删除、清空。
    danger: color = egui::Color32::from_rgb(0xE2, 0x8D, 0x8F)
             => egui::Color32::from_rgb(0xB4, 0x2D, 0x31),
    /// `danger` 悬停态。
    danger_hover: color = egui::Color32::from_rgb(0xEB, 0x84, 0x87)
                   => egui::Color32::from_rgb(0xC1, 0x1F, 0x24),
    /// `danger` 按下态。
    danger_active: color = egui::Color32::from_rgb(0xF4, 0x7C, 0x7F)
                    => egui::Color32::from_rgb(0xCE, 0x12, 0x17),
    /// 信息。
    info: color = egui::Color32::from_rgb(0x95, 0xCF, 0xE4)
           => egui::Color32::from_rgb(0x25, 0x75, 0x93),
    /// `info` 悬停态。
    info_hover: color = egui::Color32::from_rgb(0x8D, 0xD3, 0xEC)
                 => egui::Color32::from_rgb(0x19, 0x75, 0x98),
    /// `info` 按下态。
    info_active: color = egui::Color32::from_rgb(0x85, 0xD6, 0xF4)
                  => egui::Color32::from_rgb(0x0E, 0x75, 0x9C),

    // ---- 交互态背景 ----
    //
    // 半透明令牌：对比度测试必须先合成到 `surface` 才能算。
    //
    /// 隔行底色。
    row_alt: color = egui::Color32::from_white_alpha(16)
             => egui::Color32::from_black_alpha(10),
    /// 悬停底色。
    row_hover: color = egui::Color32::from_white_alpha(30)
               => egui::Color32::from_black_alpha(26),
    /// 选中底色。强调色的低透明色调，其上仍用 `text` / `text_dim`。
    row_selected: color = egui::Color32::from_rgba_unmultiplied(0x84, 0xB7, 0xEB, 56)
                   => egui::Color32::from_rgba_unmultiplied(0x1D, 0x65, 0xAF, 52),
    /// 置顶条目的底色微亮。
    ///
    /// 置顶条目与普通条目**在同一个列表里**，靠这个区分。
    ///
    /// NOTE: 刻意压得很淡（浅色 alpha 18 ≈ 7%）。实测更浓的色调会
    /// 让整行读起来像「另一类条目」，而置顶只是一个次要状态。
    /// 主要的形状线索其实来自行内 pin 按钮的实心/描边，
    /// 底色只是辅助——纯色差本身不足以让人一眼分辨。
    pin_tint: color = egui::Color32::from_rgba_unmultiplied(0x1D, 0x65, 0xAF, 18)
                => egui::Color32::from_rgba_unmultiplied(0x84, 0xB7, 0xEB, 30),
    /// 按下底色。**刻意不用 accent 色调**——按下时再叠一层强调色会
    /// 把底色推得太亮/太深，`text_dim` 落在其上就跌破 4.5:1。
    /// 「压下一层」也更贴合直觉：深色下提亮、浅色下压深。
    row_active: color = egui::Color32::from_white_alpha(52)
                  => egui::Color32::from_black_alpha(52),

    // ---- 圆角 ----
    /// 小圆角：复选框、内联标签。
    radius_sm: num = 4.0 => 4.0,
    /// 中圆角：按钮、输入框。
    radius_md: num = 6.0 => 6.0,
    /// 大圆角：面板、卡片。
    radius_lg: num = 10.0 => 10.0,
    /// 特大圆角：设置窗口的悬浮卡片（侧栏卡 / 内容卡）。
    ///
    /// 2026-10-25 设置界面改版（HTML 原型定稿）：全部控件统一胶囊
    /// 圆角、卡片改「悬浮式」——卡片圆角从 `radius_lg`(10) 提到 16，
    /// 与 14pt 起的卡间距、柔和投影一起读出「浮在窗口底上」的层级。
    /// 与其它 radius 一样**不随 font_scale 缩放**（圆角是形貌不是文字，
    /// 见 `font_scale_scales_fonts_and_text_bearing_geometry` 的断言）。
    radius_xl: num = 16.0 => 16.0,

    // ---- 间距（4 的倍数）----
    space_xs: num = 4.0 => 4.0,
    space_sm: num = 8.0 => 8.0,
    space_md: num = 12.0 => 12.0,
    space_lg: num = 16.0 => 16.0,
    space_xl: num = 24.0 => 24.0,

    // ---- 字号阶 ----
    //
    // 2026-10-11 用户反馈「整体字体偏小，放大一点」：全套字号 +1；
    // 随后「还不够大，全部调整」：再 +2。影响主窗口 / 设置窗口 /
    // 置顶窗口全部文字（font_scale 仍可叠加）。
    font_xs: num = 14.0 => 14.0,
    font_sm: num = 15.0 => 15.0,
    font_md: num = 17.0 => 17.0,
    font_lg: num = 19.0 => 19.0,
    font_xl: num = 24.0 => 24.0,
    font_title: num = 28.0 => 28.0,

    // ---- 描边宽度 ----
    stroke_thin: num = 1.0 => 1.0,
    stroke_normal: num = 1.5 => 1.5,
    stroke_thick: num = 2.0 => 2.0,

    // ---- 控件尺寸 ----
    /// 按钮 / 输入框统一高度。
    control_height: num = 28.0 => 28.0,
    /// 图标标准尺寸。
    icon_size: num = 16.0 => 16.0,

    // ---- 动效 ----
    /// 快速动效：悬停反馈。
    anim_fast: dur = Duration::from_millis(90) => Duration::from_millis(90),
    /// 常规动效：展开、淡入。
    anim_normal: dur = Duration::from_millis(160) => Duration::from_millis(160),
}

/// 主题切换的补间时长（F-34 / UI-P1-3）。
///
/// 单一数据源：`App::apply_ops` 的重启（`anim::restart`）与帧循环的
/// 推进（`anim::tween_f32`）**必须用同一个时长**——两处各写一份的话，
/// 推进侧的时长若更短，动画会在重启的时长走完前先吸附到 1.0，
/// 表现为「切换只动了开头一小段」。
pub const PALETTE_TWEEN: std::time::Duration = std::time::Duration::from_millis(220);

/// 字体缩放倍率的合法区间（写入侧与读取侧共用）。
///
/// 下限 0.75：再小则 font_xs(14) 只有 10.5pt，中文笔画糊成一团；
/// 上限 2.0：再大则设置页的卡片要横向滚动，且 CJK 图集膨胀明显。
pub const FONT_SCALE_MIN: f32 = 0.75;
/// 见 [`FONT_SCALE_MIN`]。
pub const FONT_SCALE_MAX: f32 = 2.0;

/// 把任意来源的缩放值收敛到合法区间（非有限值回落 1.0）。
pub fn clamp_font_scale(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.clamp(FONT_SCALE_MIN, FONT_SCALE_MAX)
    } else {
        1.0
    }
}

/// 主题模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    /// 强制深色。
    Dark,
    /// 强制浅色。
    Light,
    /// 跟随系统。
    #[default]
    FollowSystem,
}

impl ThemeMode {
    /// 从配置里的 `dark_mode: Option<bool>` 构造。
    ///
    /// `None` = 跟随系统，`Some(true)` = 深色，`Some(false)` = 浅色。
    /// 与 `UiConfig::dark_mode` 的既有语义一一对应，无需改动配置结构。
    pub fn from_dark_mode(dark_mode: Option<bool>) -> Self {
        match dark_mode {
            Some(true) => Self::Dark,
            Some(false) => Self::Light,
            None => Self::FollowSystem,
        }
    }

    /// 还原为配置里的 `dark_mode: Option<bool>`。
    ///
    /// [`ThemeMode::FollowSystem`] 存为 `None`。
    pub fn to_dark_mode(self) -> Option<bool> {
        match self {
            Self::Dark => Some(true),
            Self::Light => Some(false),
            Self::FollowSystem => None,
        }
    }

    /// 本模式下生效的调色板。`FollowSystem` 需先经 [`resolve`] 解析。
    ///
    /// `FollowSystem` 落到深色分支：调用方漏调 [`resolve`] 时
    /// 静默变浅色比静默变深色更难排查（旧实现也默认深色）。
    pub fn palette(self) -> Palette {
        match self {
            Self::Light => Palette::light(),
            Self::Dark | Self::FollowSystem => Palette::dark(),
        }
    }
}

/// 解析出实际生效的主题。
///
/// `FollowSystem` 取 `system`；若 `system` 本身也是 `FollowSystem`
/// 则兜底为 [`ThemeMode::Dark`]——必须有确定结果，不能递归。
pub fn resolve(mode: ThemeMode, system: ThemeMode) -> ThemeMode {
    match mode {
        ThemeMode::Dark => ThemeMode::Dark,
        ThemeMode::Light => ThemeMode::Light,
        ThemeMode::FollowSystem => match system {
            ThemeMode::Light => ThemeMode::Light,
            ThemeMode::Dark | ThemeMode::FollowSystem => ThemeMode::Dark,
        },
    }
}

// =====================================================================
// 主题解析链：一级模式 → 二级主题 → 强调色 → 令牌覆盖 → 字体缩放
// =====================================================================
//
// 四层各自的职责（顺序即优先级，后者覆盖前者）：
//
// 1. **一级模式**（`ThemeMode`）：明暗关系。功能位——错了整个界面
//    不可读。由 `Palette::dark()` / `Palette::light()` 提供。
// 2. **二级主题**（`SecondaryTheme`）：在同一种明暗关系里换色温与
//    质感（纸/绿/紫/OLED）。每个主题深浅各一套目标色板，按参数
//    浓度与一级混合。
// 3. **强调色**（`AccentColor`）：只换 accent 一族。放在二级之后，
//    免得二级主题的内置蓝把用户选的颜色盖掉。
// 4. **令牌覆盖**（`ThemeOverrides`）：用户显式改的单个令牌，
//    优先级最高——「界面上任何元素任何参数都可调」的落地。
//
// 字体缩放（`with_font_scale`）不在这条链里：它是**幂等派生**，
// 每帧从基础调色板重算（见 `with_font_scale` 的说明）。

/// 主题解析链的完整入口：配置 → 生效调色板（未应用字体缩放）。
///
/// `mode` 必须是已解析的（`FollowSystem` 已消除）——调用方用
/// [`resolve`] 处理。纯函数：同配置必然同调色板，测试直接覆盖。
pub fn resolve_palette(mode: ThemeMode, ui: &modular_clipboard_core::config::UiConfig) -> Palette {
    let mut pal = mode.palette();
    if ui.secondary_theme != modular_clipboard_core::config::SecondaryTheme::None {
        pal = apply_secondary(ui.secondary_theme, &pal, &ui.theme_params);
    }
    if ui.accent.is_some() {
        pal = derive_accent(&pal, &ui.accent_color());
    }
    apply_overrides(&mut pal, &ui.theme_overrides);
    pal
}

/// 直线（非预乘）空间的线性混合。
///
/// CAUTION: 不能对 `to_array()` 的分量直接混合——那是**预乘**后的值，
/// 混完再用 `from_rgba_unmultiplied` 重组会二次预乘（2026-10-17 修过
/// 三个同类 bug，见 [`fade_color`] 的说明）。走
/// `rgba_unmultiplied()` 取直线分量，交给 `from_rgba_unmultiplied`
/// 让它内部重新预乘，语义才正确。
fn mix_straight(a: egui::Color32, b: egui::Color32, k: f32) -> egui::Color32 {
    let k = k.clamp(0.0, 1.0);
    let [ar, ag, ab, aa] = a.to_srgba_unmultiplied();
    let [br, bg, bb, ba] = b.to_srgba_unmultiplied();
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k).round() as u8;
    egui::Color32::from_rgba_unmultiplied(m(ar, br), m(ag, bg), m(ab, bb), m(aa, ba))
}
#[cfg(test)]
mod tests {
    use super::fonts::*;
    use super::*;
    // NOTE: 字体构造只出现在测试里（生产走 fonts 子模块的内部路径），
    // 导入放在 tests 内，避免 lib 目标出现 unused import。
    use egui::{FontData, FontDefinitions, FontFamily};

    // ------------------------------------------------------------------
    // 主题解析链：二级主题 / 强调色 / 令牌覆盖
    // ------------------------------------------------------------------

    /// 默认配置（无二级主题、无自定义强调色）必须精确等于内置主题——
    /// 升级用户不看到任何变化。
    #[test]
    fn default_config_resolves_to_builtin_palette() {
        let ui = modular_clipboard_core::config::UiConfig::default();
        assert_eq!(resolve_palette(ThemeMode::Dark, &ui), Palette::dark());
        assert_eq!(resolve_palette(ThemeMode::Light, &ui), Palette::light());
    }

    /// 浓度 0 的二级主题是恒等变换（滑到最左 = 完全关掉该主题效果）。
    #[test]
    fn secondary_theme_zero_concentration_is_identity() {
        use modular_clipboard_core::config::{SecondaryTheme, ThemeParams};
        let params = ThemeParams {
            paper_tint: 0.0,
            green_tint: 0.0,
            purple_tint: 0.0,
            oled_black: 0.0,
            ..ThemeParams::default()
        };
        for base in [Palette::dark(), Palette::light()] {
            for t in [
                SecondaryTheme::Paper,
                SecondaryTheme::Green,
                SecondaryTheme::Purple,
                SecondaryTheme::Oled,
            ] {
                assert_eq!(apply_secondary(t, &base, &params), base, "{t:?} 浓度 0 应恒等");
            }
        }
    }

    /// 二级主题必须真的改色（浓度拉满时至少背景层变化），且深浅两套
    /// 都生效——「适配一级主题的改变」的最低判据。
    #[test]
    fn secondary_theme_changes_both_modes() {
        use modular_clipboard_core::config::{SecondaryTheme, ThemeParams};
        let params = ThemeParams::default();
        for t in [
            SecondaryTheme::Paper,
            SecondaryTheme::Green,
            SecondaryTheme::Purple,
            SecondaryTheme::Oled,
        ] {
            let dark = apply_secondary(t, &Palette::dark(), &params);
            let light = apply_secondary(t, &Palette::light(), &params);
            assert_ne!(dark.bg, Palette::dark().bg, "{t:?} 深色底必须变");
            assert_ne!(light.bg, Palette::light().bg, "{t:?} 浅色底必须变");
            // 明暗关系不能被打乱：深色套的文字仍应比底色亮。
            assert!(dark.is_dark && !light.is_dark);
        }
    }

    /// OLED 深色套必须真的把背景压到近黑（这是「OLED」的立身之本）。
    #[test]
    fn oled_dark_pushes_background_to_black() {
        use modular_clipboard_core::config::{SecondaryTheme, ThemeParams};
        let params = ThemeParams::default();
        let p = apply_secondary(SecondaryTheme::Oled, &Palette::dark(), &params);
        let [r, g, b, _] = p.bg.to_srgba_unmultiplied();
        assert!(r.max(g).max(b) <= 24, "OLED 底应近黑，实测 #{r:02X}{g:02X}{b:02X}");
    }

    /// 强调色派生：accent 一族全部换成用户色，且 hover/active 沿明度
    /// 压暗（与内置两套同向）；选中/置顶底色 alpha 沿用内置经验值。
    #[test]
    fn derive_accent_overrides_family() {
        use modular_clipboard_core::config::AccentColor;
        for base in [Palette::dark(), Palette::light()] {
            let p = derive_accent(&base, &AccentColor::new(140.0, 0.7, 0.6));
            let [r, g, b, _] = p.accent.to_srgba_unmultiplied();
            // 翠绿：g 通道必须最大。
            assert!(g > r && g > b, "accent 应是绿色系，实测 {r},{g},{b}");
            assert_ne!(p.accent, base.accent);
            assert_ne!(p.accent_hover, base.accent_hover);
            assert_ne!(p.accent_active, base.accent_active);
            assert_ne!(p.row_selected, base.row_selected);
            assert_ne!(p.pin_tint, base.pin_tint);
            // hover/active 比 accent 暗（压暗方向）。
            let lum = |c: egui::Color32| {
                let [r, g, b, _] = c.to_srgba_unmultiplied();
                0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32
            };
            assert!(lum(p.accent_hover) < lum(p.accent));
            assert!(lum(p.accent_active) < lum(p.accent_hover));
            // alpha 沿用内置（深 56 / 浅 52）。
            let [_, _, _, sel_a] = p.row_selected.to_srgba_unmultiplied();
            assert_eq!(sel_a, if base.is_dark { 56 } else { 52 });
        }
    }

    /// 强调色上的文字按亮度自动选黑白：亮色配黑字、暗色配白字
    /// （内置两套的实际行为，阈值 0.5）。
    #[test]
    fn derive_accent_picks_readable_text_on_accent() {
        use modular_clipboard_core::config::AccentColor;
        let dark = derive_accent(&Palette::dark(), &AccentColor::new(60.0, 0.9, 0.95));
        assert_eq!(dark.text_on_accent, egui::Color32::from_rgb(0x0A, 0x0D, 0x12), "亮黄应配黑字");
        let light = derive_accent(&Palette::light(), &AccentColor::new(240.0, 0.9, 0.25));
        assert_eq!(light.text_on_accent, egui::Color32::WHITE, "深蓝应配白字");
    }

    /// 令牌覆盖必须能改「任何元素」：颜色、数值各一条，且覆盖在
    /// 二级主题与强调色**之后**（用户显式微调优先级最高）。
    #[test]
    fn token_overrides_beat_everything_below() {
        use modular_clipboard_core::config::ThemeOverrides;
        let mut ov = ThemeOverrides::default();
        ov.colors
            .insert("bg".into(), [0x12, 0x34, 0x56, 0xFF]);
        ov.nums.insert("radius_md".into(), 13.0);
        // 强调色也设了：覆盖表的 bg 仍应赢（覆盖在最后）。
        let ui = modular_clipboard_core::config::UiConfig {
            accent: Some(modular_clipboard_core::config::AccentColor::new(300.0, 0.8, 0.6)),
            secondary_theme: modular_clipboard_core::config::SecondaryTheme::Paper,
            theme_overrides: ov,
            ..modular_clipboard_core::config::UiConfig::default()
        };
        let p = resolve_palette(ThemeMode::Dark, &ui);
        assert_eq!(p.bg, egui::Color32::from_rgb(0x12, 0x34, 0x56));
        assert_eq!(p.radius_md, 13.0);
    }

    /// `set_token` 的类型守卫：颜色表里的键指向数值令牌时必须拒绝且
    /// 不改动（配置与令牌表漂移时不能把数值字段写成颜色）。
    #[test]
    fn set_token_rejects_type_mismatch() {
        let mut p = Palette::dark();
        let before = p.radius_md;
        assert!(!p.set_token("radius_md", TokenValue::Color(egui::Color32::WHITE)));
        assert_eq!(p.radius_md, before, "类型不符不能写");
        assert!(p.set_token("radius_md", TokenValue::Num(7.0)));
        assert_eq!(p.radius_md, 7.0);
        assert!(!p.set_token("不存在的令牌", TokenValue::Num(1.0)));
    }

    /// `tokens()` 与 `set_token()` 互为逆操作：整表读出再写回必须
    /// 逐令牌相等（设置界面的「高级微调」依赖这个闭环）。
    #[test]
    fn tokens_round_trip_through_set_token() {
        let mut p = Palette::light();
        let snap: Vec<(&'static str, TokenValue)> = p.tokens();
        for (name, v) in snap {
            assert!(p.set_token(name, v), "令牌 {name} 应可写回");
        }
        // 全量再比一次（借用问题：先收集再逐条写）。
        let p2 = Palette::light();
        let names: Vec<&'static str> = p2.tokens().into_iter().map(|(n, _)| n).collect();
        assert!(names.len() > 40, "令牌数量应稳定在 40+");
    }

    // ------------------------------------------------------------------
    // 字体缩放（`UiConfig::font_scale` 的落地）
    // ------------------------------------------------------------------

    /// 缩放作用于「字号 + 承载文字的几何」，圆角/描边/间距/颜色原样。
    ///
    /// 这条守的是「字体缩放改了个寂寞」的反面：早前 `font_scale`
    /// 只有写入方、没有读取方，改它完全没反应。现在它是调色板的一个
    /// 派生维度，派生范围**恰好**是「字号 + 必须容纳字号的几何」
    /// （控件高度、图标尺寸）——多派生一点（圆角、间距）就会让设置页
    /// 在大字号下膨胀得离谱。
    #[test]
    fn font_scale_scales_fonts_and_text_bearing_geometry() {
        let base = Palette::dark();
        let scaled = base.with_font_scale(1.5);
        // 字号按倍率放大。
        assert!((scaled.font_xs - base.font_xs * 1.5).abs() < 1e-3);
        assert!((scaled.font_md - base.font_md * 1.5).abs() < 1e-3);
        assert!((scaled.font_title - base.font_title * 1.5).abs() < 1e-3);
        // 承载文字的几何同步放大：不放大则文字被裁（用户反馈的「布局错乱」）。
        assert!((scaled.control_height - base.control_height * 1.5).abs() < 1e-3);
        assert!((scaled.icon_size - base.icon_size * 1.5).abs() < 1e-3);
        // 圆角 / 间距 / 描边 / 动效令牌原样。
        assert_eq!(scaled.radius_md, base.radius_md);
        assert_eq!(scaled.space_md, base.space_md);
        assert_eq!(scaled.stroke_normal, base.stroke_normal);
        assert_eq!(scaled.anim_fast, base.anim_fast);
        // 颜色令牌原样（含透明度）。
        assert_eq!(scaled.bg, base.bg);
        assert_eq!(scaled.text, base.text);
        assert_eq!(scaled.accent, base.accent);
        assert_eq!(scaled.is_dark, base.is_dark);
    }

    /// 1.0× 是恒等变换：逐字段等于基础调色板。
    #[test]
    fn font_scale_one_is_identity() {
        for base in [Palette::dark(), Palette::light()] {
            let same = base.with_font_scale(1.0);
            assert_eq!(same.font_md, base.font_md);
            assert_eq!(same.font_xs, base.font_xs);
            assert_eq!(same.font_title, base.font_title);
        }
    }

    /// 非有限 / 非正缩放必须回落到 1.0。
    ///
    /// NaN 会让每个字号都变 NaN，egui 排版静默产出空 galley——
    /// 表现为「文字全部消失」，且不报任何错。这是必须挡住的输入。
    #[test]
    fn font_scale_rejects_nonsense_input() {
        let base = Palette::dark();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.5] {
            let p = base.with_font_scale(bad);
            assert_eq!(p.font_md, base.font_md, "非法缩放 {bad} 应回落 1.0");
            assert!(p.font_md.is_finite());
        }
    }

    /// 缩放从**基础**调色板派生才幂等；在已缩放的结果上再乘会叠乘。
    ///
    /// 这条把「必须从 `pal_base` 派生」写成可执行文档：`App` 每次
    /// 都从基础调色板重算，就是为了避开下面这个叠乘。
    #[test]
    fn font_scale_compounds_if_applied_twice() {
        let base = Palette::dark();
        let once = base.with_font_scale(1.15);
        let twice = once.with_font_scale(1.15);
        assert!(
            (twice.font_md - base.font_md * 1.15 * 1.15).abs() < 1e-3,
            "在已缩放的调色板上再乘会叠乘——调用方必须从 base 派生"
        );
        assert!(twice.font_md > once.font_md);
    }

    /// 钳制区间：越界收敛、NaN 回落 1.0。
    #[test]
    fn clamp_font_scale_bounds() {
        assert_eq!(clamp_font_scale(1.0), 1.0);
        assert_eq!(clamp_font_scale(0.1), FONT_SCALE_MIN);
        assert_eq!(clamp_font_scale(9.0), FONT_SCALE_MAX);
        assert_eq!(clamp_font_scale(f32::NAN), 1.0);
    }

    // ------------------------------------------------------------------
    // 对比度工具（WCAG 2.1）
    // ------------------------------------------------------------------

    /// sRGB 单通道线性化。阈值 0.03928 与 `((c+0.055)/1.055)^2.4`
    /// 来自 WCAG 2.1 的相对亮度定义，不可改动。
    fn linearize(c: u8) -> f32 {
        let s = c as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }

    /// WCAG 相对亮度。
    fn luminance(c: egui::Color32) -> f32 {
        0.2126 * linearize(c.r()) + 0.7152 * linearize(c.g()) + 0.0722 * linearize(c.b())
    }

    /// WCAG 对比度，返回 1.0..=21.0。
    fn contrast(a: egui::Color32, b: egui::Color32) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    /// 正文级阈值。
    const AA_TEXT: f32 = 4.5;
    /// 非文本 UI 组件阈值（WCAG 1.4.11）。
    const AA_UI: f32 = 3.0;

    /// 断言对比度达标，失败时打印实测值与参与计算的两个色值。
    fn assert_contrast(label: &str, fg: egui::Color32, bg: egui::Color32, need: f32) {
        let r = contrast(fg, bg);
        assert!(
            r >= need,
            "{label}: 对比度 {r:.3} < {need}（前景 {:?} / 背景 {:?}）",
            fg.to_array(),
            bg.to_array()
        );
    }

    /// 令牌在某背景上的最终合成色。
    ///
    /// 半透明令牌必须走这条路：`row_alt` 是叠在 `surface` 上的，
    /// 而 `surface` 又叠在 `bg` 上。直接拿半透明值算会高估——
    /// 这正是深色主题最容易踩的坑。
    fn over(p: &Palette, layer: &str, base: egui::Color32) -> egui::Color32 {
        match layer {
            "bg" => base,
            "surface" => base,
            "surface_variant" => composite(p.surface_variant, base),
            "surface_raised" => composite(p.surface_raised, base),
            "list_bg" => composite(p.list_bg, base),
            "row_alt" => composite(p.row_alt, composite(p.surface, base)),
            "row_hover" => composite(p.row_hover, composite(p.surface, base)),
            "row_selected" => composite(p.row_selected, composite(p.surface, base)),
            "row_active" => composite(p.row_active, composite(p.surface, base)),
            other => panic!("未知层 {other}"),
        }
    }

    /// 面板之上、窗口底色之下的全部交互层。
    ///
    /// 文字会出现在其中每一层上——包括 `row_active`：鼠标按住条目时
    /// 那一帧文字仍需可读，只校验悬停/选中会漏掉按下态。
    const LAYERS: &[&str] = &[
        "bg",
        "surface",
        "surface_variant",
        "surface_raised",
        "list_bg",
        "row_alt",
        "row_hover",
        "row_selected",
        "row_active",
    ];

    /// 状态色三态。
    const STATE_NAMES: &[&str] = &["", "_hover", "_active"];

    // ---- 对比度：深浅两套 ----

    #[test]
    fn text_layer_meets_aa_on_every_background_it_can_appear_on() {
        for p in [Palette::dark(), Palette::light()] {
            // 文字可能落在任何一层上——包括 `row_active`（鼠标按住
            // 那一帧条目上仍有文字），故遍历全部层而非只挑几种。
            for layer in LAYERS {
                let bg = over(&p, layer, p.bg);
                assert_contrast(
                    &format!("[{}] text on {layer}", mode_of(&p)),
                    p.text,
                    bg,
                    AA_TEXT,
                );
                assert_contrast(
                    &format!("[{}] text_dim on {layer}", mode_of(&p)),
                    p.text_dim,
                    bg,
                    AA_TEXT,
                );
                assert_contrast(
                    &format!("[{}] text_bright on {layer}", mode_of(&p)),
                    p.text_bright,
                    bg,
                    AA_TEXT,
                );
            }
        }
    }

    #[test]
    fn state_colors_meet_aa_as_foreground_on_every_panel() {
        for p in [Palette::dark(), Palette::light()] {
            for base in ["accent", "success", "warn", "danger", "info"] {
                for suffix in STATE_NAMES {
                    let c = state(&p, base, suffix);
                    // 状态色画在窗口底色与各级面板上。
                    for layer in ["bg", "surface", "surface_variant", "surface_raised"] {
                        assert_contrast(
                            &format!("[{}] {base}{suffix} on {layer}", mode_of(&p)),
                            c,
                            over(&p, layer, p.bg),
                            AA_TEXT,
                        );
                    }
                    // 作为填充底色时，其上文字用 text_on_accent。
                    assert_contrast(
                        &format!("[{}] text_on_accent on {base}{suffix}", mode_of(&p)),
                        p.text_on_accent,
                        c,
                        AA_TEXT,
                    );
                }
            }
        }
    }

    /// 磨砂变体：四个背景层降 alpha，内容层（文字/卡片/状态色）不动。
    ///
    /// 这是磨砂模式下「DWM 的 blur 能透出」的渲染侧保证——背景层
    /// 必须真的变透明，而不是只在文档里声明。
    ///
    /// NOTE: 不断言 RGB 不变：`gamma_multiply` 在 gamma 空间同时调整
    /// RGB 与 alpha（视觉上「同色更淡」），这是 egui 预乘的预期语义。
    #[test]
    fn backdrop_alpha_only_touches_backdrop_layers() {
        for p in [Palette::dark(), Palette::light()] {
            let glass = p.with_backdrop_alpha(96);
            for (name, c) in [("bg", p.bg), ("surface", p.surface), ("surface_variant", p.surface_variant), ("list_bg", p.list_bg)] {
                let g = field_of(&glass, name);
                assert!(
                    g.a() < c.a(),
                    "[{}] {name} 在磨砂变体下应降 alpha（{} -> {}）",
                    mode_of(&p),
                    c.a(),
                    g.a()
                );
            }
            // 内容层不受影响：直接按字段比对。
            assert_eq!(glass.text, p.text, "text 不应被磨砂变体改变");
            assert_eq!(glass.surface_raised, p.surface_raised, "卡片底色不应被改变");
            assert_eq!(glass.accent, p.accent, "状态色不应被改变");
            assert_eq!(glass.is_dark, p.is_dark, "主题模式不应被改变");
        }
    }

    /// 按字段名取色（测试用）。
    fn field_of(p: &Palette, name: &str) -> egui::Color32 {
        match name {
            "bg" => p.bg,
            "surface" => p.surface,
            "surface_variant" => p.surface_variant,
            "list_bg" => p.list_bg,
            other => panic!("未知字段 {other}"),
        }
    }

    #[test]
    fn borders_meet_ui_component_threshold() {
        for p in [Palette::dark(), Palette::light()] {
            for layer in ["bg", "surface"] {
                assert_contrast(
                    &format!("[{}] border on {layer}", mode_of(&p)),
                    p.border,
                    over(&p, layer, p.bg),
                    AA_UI,
                );
                assert_contrast(
                    &format!("[{}] border_strong on {layer}", mode_of(&p)),
                    p.border_strong,
                    over(&p, layer, p.bg),
                    AA_UI,
                );
            }
        }
    }

    #[test]
    fn overlay_scrim_actually_dims_whatever_is_under_it() {
        // 遮罩的意义就是压暗背景；若合不成更暗的颜色，它就是坏的。
        for p in [Palette::dark(), Palette::light()] {
            let under = p.surface_raised;
            let scrimmed = composite(p.overlay_scrim, under);
            assert!(
                luminance(scrimmed) < luminance(under),
                "[{}] overlay_scrim 应压暗底色",
                mode_of(&p)
            );
        }
    }

    /// 深色主题的投影必须比浅色更浓：列表底 `list_bg` 深色近黑
    /// （`#101215`），低 alpha 黑投影叠上去没有明度差。
    #[test]
    fn dark_shadow_is_stronger_than_light() {
        let d = Palette::dark();
        let l = Palette::light();
        assert!(
            d.shadow.a() > l.shadow.a(),
            "深色阴影 {} 应浓于浅色 {}（否则深色主题下看不见投影）",
            d.shadow.a(),
            l.shadow.a()
        );
        // 两个主题的投影基色都必须是黑色（物理投影色）。
        for p in [&d, &l] {
            let [r, g, b, _] = p.shadow.to_array();
            assert_eq!((r, g, b), (0, 0, 0), "投影基色应为黑色");
        }
        // 2026-10-15 预览页验收后：深色 255 / 浅色 128（此前 204/102）。
        assert_eq!(d.shadow.a(), 255, "深色阴影 alpha 应提到满值");
        assert_eq!(l.shadow.a(), 128, "浅色阴影 alpha 应提到 0.5");
    }

    /// `with_card_opacity` 必须**同时**降 alpha 与 RGB（预乘语义）。
    ///
    /// 只降 alpha 会让卡片「变亮发白」（预乘值不合法），只降 RGB 会让
    /// 它「变暗但完全不透明」——后者正是「卡片透明度失效」的另一种形态：
    /// 滑块拖到底，卡片只是变暗，透不出任何东西。
    #[test]
    fn card_opacity_lowers_alpha_and_keeps_hue() {
        for p in [Palette::dark(), Palette::light()] {
            let base = p.card_bg;
            let half = p.with_card_opacity(128).card_bg;
            assert!(
                half.a() < base.a(),
                "[{}] 不透明度没降：{} vs {}",
                mode_of(&p),
                half.a(),
                base.a()
            );
            // 非预乘色相不变（允许 ±2 的量化误差）。
            let [r0, g0, b0, _] = base.to_srgba_unmultiplied();
            let [r1, g1, b1, _] = half.to_srgba_unmultiplied();
            for (i, (a, b)) in [(r0, r1), (g0, g1), (b0, b1)].iter().enumerate() {
                assert!(
                    (*a as i32 - *b as i32).abs() <= 2,
                    "[{}] 通道 {i} 色相被改了：{a} -> {b}",
                    mode_of(&p)
                );
            }
            // 255 = 原样返回（`gamma_multiply(1.0)` 早退）。
            assert_eq!(p.with_card_opacity(255).card_bg, base);
        }
    }

    /// 卡片顶部受光高光：深色主题有（alpha 0.20 的白色渐变），浅色
    /// 主题透明（卡片自身最亮，不再叠加）。
    #[test]
    fn card_highlight_only_in_dark_theme() {
        let d = Palette::dark();
        let l = Palette::light();
        assert!(d.card_highlight.a() > 0, "深色主题应绘制顶部高光");
        assert_eq!(l.card_highlight.a(), 0, "浅色主题高光应透明");
        // 高光必须是白色系（受光模拟）。Color32 是 premultiplied 存储，
        // 白色 α 分量下 RGB 应等于 A（(51,51,51,51) = 白 × 20%）。
        let [r, g, b, a] = d.card_highlight.to_array();
        assert_eq!((r, g, b), (a, a, a), "高光应为白色（premultiplied 下 RGB==A）");
        assert_eq!(a, 51, "高光 alpha 应为 20%（255×0.2=51）");
    }

    // ------------------------------------------------------------------
    // 深浅两套完整性
    // ------------------------------------------------------------------

    /// 期望的颜色令牌清单。**新增颜色令牌必须同步登记**——
    /// 完整性测试会比对它，漏登记即失败。
    const EXPECTED_COLOR_TOKENS: &[&str] = &[
        // 背景层
        "bg", "surface", "surface_variant", "surface_raised", "list_bg", "overlay_scrim",
        // 行卡片专用底（`surface_raised` 的「卡片透明度」派生，
        // 与浮层令牌分开的理由见 `Palette::with_card_opacity`）
        "card_bg",
        // 描边层
        "border", "border_strong", "border_subtle",
        // 投影
        "shadow", "card_highlight",
        // 文本层
        "text", "text_dim", "text_bright", "text_on_accent",
        // 状态色五组三态
        "accent", "accent_hover", "accent_active",
        "success", "success_hover", "success_active",
        "warn", "warn_hover", "warn_active",
        "danger", "danger_hover", "danger_active",
        "info", "info_hover", "info_active",
        // 交互态背景
        "row_alt", "row_hover", "row_selected", "row_active",
        // 置顶条目标记（同列表内的次要状态）
        "pin_tint",
    ];

    /// 期望的非颜色令牌清单。登记规则同 [`EXPECTED_COLOR_TOKENS`]。
    const EXPECTED_NUM_TOKENS: &[&str] = &[
        "radius_sm", "radius_md", "radius_lg", "radius_xl",
        "space_xs", "space_sm", "space_md", "space_lg", "space_xl",
        "font_xs", "font_sm", "font_md", "font_lg", "font_xl", "font_title",
        "stroke_thin", "stroke_normal", "stroke_thick",
        "control_height", "icon_size",
    ];

    /// 期望的时长令牌清单。
    const EXPECTED_DUR_TOKENS: &[&str] = &["anim_fast", "anim_normal"];

    fn sorted_color_names(p: &Palette) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = p
            .tokens()
            .into_iter()
            .filter(|(_, t)| matches!(t, TokenValue::Color(_)))
            .map(|(n, _)| n)
            .collect();
        v.sort_unstable();
        v
    }

    fn sorted_num_names(p: &Palette) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = p
            .tokens()
            .into_iter()
            .filter(|(_, t)| matches!(t, TokenValue::Num(_)))
            .map(|(n, _)| n)
            .collect();
        v.sort_unstable();
        v
    }

    fn sorted_dur_names(p: &Palette) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = p
            .tokens()
            .into_iter()
            .filter(|(_, t)| matches!(t, TokenValue::Dur(_)))
            .map(|(n, _)| n)
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn dark_and_light_expose_exactly_the_same_tokens() {
        let (d, l) = (Palette::dark(), Palette::light());
        assert_eq!(
            d.tokens().len(),
            l.tokens().len(),
            "两套令牌数量应相同"
        );
        assert_eq!(
            sorted_color_names(&d),
            sorted_color_names(&l),
            "两套颜色令牌名应相同"
        );
        assert_eq!(sorted_num_names(&d), sorted_num_names(&l));
        assert_eq!(sorted_dur_names(&d), sorted_dur_names(&l));
    }

    #[test]
    fn token_inventory_matches_registered_expectation() {
        // 这一条是「新增令牌忘了登记」的护栏：宏保证了深浅对称，
        // 但保证不了「你记得给它起了名字、写进了清单」。
        let mut expected: Vec<&str> = Vec::new();
        expected.extend_from_slice(EXPECTED_COLOR_TOKENS);
        expected.extend_from_slice(EXPECTED_NUM_TOKENS);
        expected.extend_from_slice(EXPECTED_DUR_TOKENS);
        expected.sort_unstable();

        for p in [Palette::dark(), Palette::light()] {
            let mut actual: Vec<&str> = p.tokens().into_iter().map(|(n, _)| n).collect();
            actual.sort_unstable();
            assert_eq!(
                actual,
                expected,
                "[{}] 令牌清单与登记不一致",
                mode_of(&p)
            );
        }
    }

    #[test]
    fn no_token_name_is_duplicated() {
        for p in [Palette::dark(), Palette::light()] {
            let mut names: Vec<&str> = p.tokens().into_iter().map(|(n, _)| n).collect();
            let total = names.len();
            names.sort_unstable();
            names.dedup();
            assert_eq!(names.len(), total, "[{}] 存在重名令牌", mode_of(&p));
        }
    }

    #[test]
    fn backward_compatible_alias_is_the_same_color_as_surface() {
        // `panel_bg` 是旧字段名，必须与 `surface` 逐位相同，
        // 否则旧调用点画出的面板色会与新令牌漂移。
        for p in [Palette::dark(), Palette::light()] {
            assert_eq!(
                p.panel_bg(),
                p.surface,
                "[{}] panel_bg 应等于 surface",
                mode_of(&p)
            );
        }
    }

    // ------------------------------------------------------------------
    // 令牌值合法性
    // ------------------------------------------------------------------

    #[test]
    fn color_alphas_are_valid() {
        for p in [Palette::dark(), Palette::light()] {
            for (name, v) in p.tokens() {
                if let TokenValue::Color(c) = v {
                    let [r, g, b, a] = c.to_array();
                    assert!(
                        c.is_opaque() || a < 255,
                        "[{}] {name} 的 alpha 应在 0..=255",
                        mode_of(&p)
                    );
                    // `to_array` 已保证四通道均在 u8 范围内；
                    // 这里额外确认没有通道被错误地写成不透明。
                    let _ = (r, g, b);
                }
            }
        }
    }

    #[test]
    fn spacing_font_size_and_control_metrics_are_sane() {
        for p in [Palette::dark(), Palette::light()] {
            let m = mode_of(&p);
            for (name, v) in p.tokens() {
                match v {
                    TokenValue::Num(n) => {
                        if name.starts_with("radius_") {
                            assert!(n >= 0.0, "[{m}] {name} 圆角应非负，实际 {n}");
                        } else if name.starts_with("space_") {
                            assert!(n > 0.0, "[{m}] {name} 间距应为正，实际 {n}");
                            assert_eq!(
                                n % 4.0,
                                0.0,
                                "[{m}] {name} 应为 4 的倍数，实际 {n}"
                            );
                        } else if name.starts_with("font_") {
                            assert!(n > 0.0, "[{m}] {name} 字号应为正，实际 {n}");
                        } else if name.starts_with("stroke_") {
                            assert!(n > 0.0, "[{m}] {name} 描边宽应为正，实际 {n}");
                        } else {
                            // control_height / icon_size
                            assert!(n > 0.0, "[{m}] {name} 应为正，实际 {n}");
                        }
                    }
                    TokenValue::Dur(d) => {
                        assert!(!d.is_zero(), "[{m}] {name} 动效时长不应为 0");
                    }
                    TokenValue::Color(_) => {}
                }
            }
        }
    }

    #[test]
    fn scales_are_monotonic() {
        for p in [Palette::dark(), Palette::light()] {
            let m = mode_of(&p);
            let f = |n: &str| -> f32 {
                match p.tokens().into_iter().find(|(k, _)| *k == n).map(|(_, v)| v) {
                    Some(TokenValue::Num(x)) => x,
                    other => panic!("[{m}] {n} 应为数值令牌，实际 {other:?}"),
                }
            };
            let groups: [(&str, &[(&str, f32)]); 4] = [
                ("半径", &[("radius_sm", f("radius_sm")), ("radius_md", f("radius_md")), ("radius_lg", f("radius_lg")), ("radius_xl", f("radius_xl"))]),
                (
                    "间距",
                    &[
                        ("space_xs", f("space_xs")),
                        ("space_sm", f("space_sm")),
                        ("space_md", f("space_md")),
                        ("space_lg", f("space_lg")),
                        ("space_xl", f("space_xl")),
                    ],
                ),
                (
                    "字号",
                    &[
                        ("font_xs", f("font_xs")),
                        ("font_sm", f("font_sm")),
                        ("font_md", f("font_md")),
                        ("font_lg", f("font_lg")),
                        ("font_xl", f("font_xl")),
                        ("font_title", f("font_title")),
                    ],
                ),
                (
                    "描边宽",
                    &[
                        ("stroke_thin", f("stroke_thin")),
                        ("stroke_normal", f("stroke_normal")),
                        ("stroke_thick", f("stroke_thick")),
                    ],
                ),
            ];
            for (name, seq) in groups {
                for pair in seq.windows(2) {
                    assert!(
                        pair[0].1 < pair[1].1,
                        "[{m}] {name} 应递增：{}={} 未小于 {}={}",
                        pair[0].0,
                        pair[0].1,
                        pair[1].0,
                        pair[1].1
                    );
                }
            }
            let dur = |n: &str| -> Duration {
                match p.tokens().into_iter().find(|(k, _)| *k == n).map(|(_, v)| v) {
                    Some(TokenValue::Dur(d)) => d,
                    other => panic!("[{m}] {n} 应为时长令牌，实际 {other:?}"),
                }
            };
            assert!(
                dur("anim_fast") < dur("anim_normal"),
                "[{m}] 动效时长应递增：{:?} vs {:?}",
                dur("anim_fast"),
                dur("anim_normal")
            );
        }
    }

    #[test]
    fn state_triples_share_a_brightness_band_but_differ_in_saturation() {
        // 三态「同亮度、异饱和」是刻意的取舍：亮度一改，对比度就掉。
        // 这条测试固定住该取舍，防止后人「顺手」把 active 调暗。
        for p in [Palette::dark(), Palette::light()] {
            let m = mode_of(&p);
            for base in ["accent", "success", "warn", "danger", "info"] {
                let lumas: Vec<f32> = STATE_NAMES
                    .iter()
                    .map(|s| luminance(state(&p, base, s)))
                    .collect();
                let lo = lumas.iter().cloned().fold(f32::MAX, f32::min);
                let hi = lumas.iter().cloned().fold(0.0, f32::max);
                assert!(
                    (hi - lo).abs() < 0.12,
                    "[{m}] {base} 三态亮度跨度 {span:.3} 过大（{lo:.3}..{hi:.3}）",
                    span = hi - lo
                );
                // 三态必须真的互不相同，否则「按下」没有视觉反馈。
                for (i, name_i) in STATE_NAMES.iter().enumerate().take(lumas.len()) {
                    for name_j in STATE_NAMES.iter().skip(i + 1).take(lumas.len() - i - 1) {
                        assert_ne!(
                            state(&p, base, name_i),
                            state(&p, base, name_j),
                            "[{m}] {base} 的 {} 与 {} 不应同色",
                            name_i,
                            name_j
                        );
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // resolve：纯函数
    // ------------------------------------------------------------------

    #[test]
    fn resolve_picks_the_explicit_mode_regardless_of_system() {
        use ThemeMode::{Dark, FollowSystem, Light};
        assert_eq!(resolve(Dark, Light), Dark);
        assert_eq!(resolve(Light, Dark), Light);
        assert_eq!(resolve(Dark, Dark), Dark);
        assert_eq!(resolve(Light, Light), Light);
        // 系统为 FollowSystem 时，显式模式同样不受影响。
        assert_eq!(resolve(Dark, FollowSystem), Dark);
        assert_eq!(resolve(Light, FollowSystem), Light);
    }

    #[test]
    fn resolve_follows_the_system_theme() {
        use ThemeMode::{Dark, FollowSystem, Light};
        assert_eq!(resolve(FollowSystem, Dark), Dark);
        assert_eq!(resolve(FollowSystem, Light), Light);
    }

    #[test]
    fn resolve_falls_back_to_dark_when_system_is_also_follow_system() {
        // 双重 FollowSystem 必须有确定结果，不能递归。
        assert_eq!(resolve(ThemeMode::FollowSystem, ThemeMode::FollowSystem), ThemeMode::Dark);
    }

    #[test]
    fn theme_mode_round_trips_through_config_representation() {
        use ThemeMode::{Dark, FollowSystem, Light};
        for m in [Dark, Light, FollowSystem] {
            assert_eq!(ThemeMode::from_dark_mode(m.to_dark_mode()), m);
        }
        assert_eq!(ThemeMode::from_dark_mode(None), FollowSystem);
        assert_eq!(ThemeMode::from_dark_mode(Some(true)), Dark);
        assert_eq!(ThemeMode::from_dark_mode(Some(false)), Light);
    }

    #[test]
    fn default_mode_is_follow_system() {
        assert_eq!(ThemeMode::default(), ThemeMode::FollowSystem);
    }

    #[test]
    fn palette_lookup_never_returns_follow_system() {
        use ThemeMode::{Dark, FollowSystem, Light};
        assert!(Palette::from_mode(Dark).is_dark);
        assert!(!Palette::from_mode(Light).is_dark);
        // 漏调 resolve 时的兜底：必须是确定的深色。
        assert!(Palette::from_mode(FollowSystem).is_dark);
    }

    #[test]
    fn detect_system_theme_returns_a_concrete_mode_without_panicking() {
        // 不对返回值做断言：CI 与他人机器的主题设置不可控。
        // 这里只保证「调用不会 panic，且不会返回 FollowSystem」。
        let m = detect_system_theme();
        assert!(
            matches!(m, ThemeMode::Dark | ThemeMode::Light),
            "探测结果应为具体模式，实际 {m:?}"
        );
    }

    // ------------------------------------------------------------------
    // apply：egui Visuals 单一入口
    // ------------------------------------------------------------------

    #[test]
    fn apply_maps_tokens_onto_egui_visuals() {
        let ctx = egui::Context::default();
        let p = Palette::dark();
        apply(&ctx, &p);
        let v = ctx.style_of(egui::Theme::Dark).visuals.clone();

        assert_eq!(v.override_text_color, Some(p.text));
        assert_eq!(v.weak_text_color, Some(p.text_dim));
        assert_eq!(v.panel_fill, p.bg);
        assert_eq!(v.window_fill, p.surface);
        assert_eq!(v.extreme_bg_color, p.surface_raised);
        assert_eq!(v.faint_bg_color, p.surface_variant);
        assert_eq!(v.hyperlink_color, p.accent);
        assert_eq!(v.warn_fg_color, p.warn);
        assert_eq!(v.error_fg_color, p.danger);
        assert_eq!(
            v.selection.bg_fill,
            composite(p.row_selected, p.surface),
            "选中底色应是 row_selected 合成到面板后的结果"
        );
    }

    #[test]
    fn apply_covers_every_fg_stroke_in_both_themes() {
        // 回归「白底白字」：从 Theme::Dark 克隆来的 fg_stroke 是浅色文字，
        // 浅色主题若漏改就是白底白字——界面能跑但什么都看不见。
        for p in [Palette::dark(), Palette::light()] {
            let ctx = egui::Context::default();
            apply(&ctx, &p);
            // apply 会把 theme_preference 对齐到本调色板，
            // 故读 theme_of(is_dark) 拿到的就是刚写入的那套。
            let v = ctx.style_of(theme_of(p.is_dark)).visuals.clone();

            assert_eq!(v.dark_mode, p.is_dark, "[{}] dark_mode", mode_of(&p));
            assert!(
                ctx.theme() == theme_of(p.is_dark),
                "[{}] theme_preference 应与调色板一致",
                mode_of(&p)
            );

            let fg = luminance(p.text);
            for (name, wv) in [
                ("noninteractive", &v.widgets.noninteractive),
                ("inactive", &v.widgets.inactive),
                ("hovered", &v.widgets.hovered),
                ("active", &v.widgets.active),
                ("open", &v.widgets.open),
            ] {
                assert!(
                    wv.bg_fill != egui::Color32::TRANSPARENT,
                    "[{}] {name}.bg_fill 不可为全透明",
                    mode_of(&p)
                );
                // 不能是「从 Theme::Dark 克隆来的原始值」：
                // 该值在浅色主题下正是「白底白字」的元凶。
                // 合法值只有 tokens 里那几个前景色。
                let fg = wv.fg_stroke.color;
                let allowed = [p.text, p.text_bright, p.text_dim];
                assert!(
                    allowed.contains(&fg),
                    "[{}] {name}.fg_stroke {:?} 既非令牌前景色，也非预期值",
                    mode_of(&p),
                    fg.to_array()
                );
                // 前景必须与自己的背景形成对比，否则控件不可读。
                assert!(
                    contrast(wv.fg_stroke.color, wv.bg_fill) >= AA_TEXT,
                    "[{}] {name} 前景 {:?} 在背景 {:?} 上对比度不足",
                    mode_of(&p),
                    wv.fg_stroke.color.to_array(),
                    wv.bg_fill.to_array()
                );
            }

            // 深色主题前景要亮、浅色主题前景要暗。
            if p.is_dark {
                assert!(
                    fg > 0.5,
                    "深色主题的文字应是亮色，实际亮度 {fg:.3}"
                );
            } else {
                assert!(
                    fg < 0.5,
                    "浅色主题的文字应是深色，实际亮度 {fg:.3}"
                );
            }
        }
    }

    #[test]
    fn apply_is_idempotent() {
        // 同一调色板重复灌入不应改变结果——否则「每帧 set_theme」
        // 会在主题切换时产生漂移。
        let ctx = egui::Context::default();
        let p = Palette::light();
        apply(&ctx, &p);
        let once = ctx.style_of(egui::Theme::Light).visuals.clone();
        apply(&ctx, &p);
        let twice = ctx.style_of(egui::Theme::Light).visuals.clone();
        assert_eq!(once, twice);
    }

    #[test]
    fn switching_modes_replaces_rather_than_merges() {
        // 主题来回切换后必须回到原样，否则深色会「染」浅色
        // （例如浅色下漏设的字段残留深色克隆值）。
        let ctx = egui::Context::default();
        let (d, l) = (Palette::dark(), Palette::light());
        apply(&ctx, &d);
        let before = ctx.style_of(egui::Theme::Dark).visuals.clone();
        apply(&ctx, &l);
        apply(&ctx, &d);
        assert_eq!(ctx.style_of(egui::Theme::Dark).visuals.clone(), before);
    }

    // ------------------------------------------------------------------
    // 字体（原有测试）
    // ------------------------------------------------------------------

    fn mode_of(p: &Palette) -> &'static str {
        if p.is_dark { "深色" } else { "浅色" }
    }

    fn theme_of(is_dark: bool) -> egui::Theme {
        if is_dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        }
    }

    fn state(p: &Palette, base: &str, suffix: &str) -> egui::Color32 {
        let key: String = format!("{base}{suffix}");
        match p
            .tokens()
            .into_iter()
            .find(|(n, _)| *n == key)
            .map(|(_, v)| v)
        {
            Some(TokenValue::Color(c)) => c,
            other => panic!("{key} 应为颜色令牌，实际 {other:?}"),
        }
    }

    #[test]
    fn candidates_are_ordered_with_windows_first() {
        let first = CJK_CANDIDATES.first().unwrap();
        assert!(first.1.contains("msyh"), "应优先使用微软雅黑");
    }

    /// 字体字节必须**跨 Context 共享**：每个窗口一份的话，开一次设置
    /// 窗口就多驻留一份中文字体（实测约 14MB），反复开关只涨不落。
    #[test]
    fn font_bytes_are_shared_between_contexts() {
        let (path, index) = (CJK_CANDIDATES[0].1, CJK_CANDIDATES[0].2);
        // 候选字体在这台机器上不存在时跳过（不是本测试要守的东西）。
        if !std::path::Path::new(path).exists() {
            return;
        }
        let a = shared_font_data(path, index).expect("候选字体应能读到");
        let b = shared_font_data(path, index).expect("第二次同样应能读到");
        assert!(
            std::sync::Arc::ptr_eq(&a, &b),
            "同一路径两次取到的应是同一份字体数据（共享字节），而不是各读一份"
        );
    }

    #[test]
    fn insert_font_adds_to_both_families() {
        let mut defs = FontDefinitions::default();
        // 用任意字节验证注册流程不 panic（epaint 会延后解析）
        let data = std::sync::Arc::new(FontData::from_owned(b"not-a-real-font".to_vec()));
        let ok = insert_font(&mut defs, "x", data);
        assert!(ok);
        assert!(
            defs.families[&FontFamily::Proportional].contains(&"x".to_string()),
            "应加入 Proportional"
        );
        assert!(
            defs.families[&FontFamily::Monospace].contains(&"x".to_string()),
            "应加入 Monospace"
        );
    }

    #[test]
    fn palette_lerp_interpolates_colors_and_snaps_geometry() {
        // F-34：颜色字段必须真的在两端之间，几何令牌直接取目标值
        // （插值几何会让布局在过渡期抖动，比硬切更糟）。
        let dark = Palette::dark();
        let light = Palette::light();
        let mid = dark.lerp(&light, 0.5);
        let dr = dark.bg.r();
        let lr = light.bg.r();
        let mr = mid.bg.r();
        let expect = ((dr as f32 + lr as f32) / 2.0).round() as u8;
        assert!((mr as i16 - expect as i16).abs() <= 1, "bg 中点应约为两端均值：{mr} vs {expect}");
        // 端点：t=0 完全是源，t=1 完全是目标。
        assert_eq!(dark.lerp(&light, 0.0).bg, dark.bg, "t=0 应等于源");
        assert_eq!(dark.lerp(&light, 1.0).bg, light.bg, "t=1 应等于目标");
        // 几何令牌取目标值（不插值）。
        assert_eq!(mid.radius_md, light.radius_md, "几何令牌应取目标值");
        // is_dark 中点翻转：避免「深色文字配浅色底」的中间态。
        assert!(dark.lerp(&light, 0.4).is_dark, "t<0.5 应保持源的明暗");
        assert!(!dark.lerp(&light, 0.6).is_dark, "t≥0.5 应翻到目标的明暗");
    }

    /// 半透明令牌的补间必须在**预乘空间**线性插值后原样重组。
    ///
    /// 回归守卫（2026-10-17）：`to_array()` 拿到的是预乘分量，若用
    /// `from_rgba_unmultiplied` 重组会**二次预乘**——`row_hover` 从
    /// 白 30 → 黑 26 过渡时，中点的预乘 rgb 应是 15，二次预乘后被压到
    /// ~2，主题切换途中悬停/按下底色整体发暗。期望值从两端现算，
    /// 改色板数值也不会让这条测试失真。
    #[test]
    fn palette_lerp_keeps_premultiplied_channels() {
        let dark = Palette::dark();
        let light = Palette::light();
        let mid = dark.lerp(&light, 0.5);
        for (name, got) in [
            ("row_hover", mid.row_hover),
            ("row_active", mid.row_active),
            ("row_selected", mid.row_selected),
        ] {
            let (a, b) = match name {
                "row_hover" => (dark.row_hover, light.row_hover),
                "row_active" => (dark.row_active, light.row_active),
                _ => (dark.row_selected, light.row_selected),
            };
            for ch in 0..4 {
                let av = a.to_array()[ch] as f32;
                let bv = b.to_array()[ch] as f32;
                let want = ((av + bv) / 2.0).round() as u8;
                assert_eq!(
                    got.to_array()[ch], want,
                    "{name} 通道 {ch} 应为两端预乘均值 {want}，实际 {}（疑似二次预乘）",
                    got.to_array()[ch]
                );
            }
        }
    }

    #[test]
    fn size_scale_is_clamped() {
        // NOTE: 这条测试原先断言的是**错误行为**：`sized(12.0, 5.0).size == 24.0`
        // （12 × 2.0钳制）。那正是「字号被 ppp 乘两次」的根源——
        // egui 排版时自己会再乘一次 `pixels_per_point`。
        //
        // 现在 `sized` 恒返回逻辑字号，与 `scale` 无关。
        assert_eq!(sized(12.0, 0.1).size, 12.0, "极端 scale 不得改变字号");
        assert_eq!(sized(12.0, 5.0).size, 12.0, "极端 scale 不得改变字号");
        assert_eq!(sized(12.0, 1.5).size, 12.0, "字号与 ppp 无关");
    }

    // ------------------------------------------------------------------
    // 图标字体注册（回归判据）
    // ------------------------------------------------------------------

    /// 建一个「已跑过一帧」的 Context，让 `fonts_mut` 可用。
    ///
    /// `install_icon_font` 本身在 `run` 之前调用是合法的（这正是
    /// 改用 `add_font` 的原因），但**验证墨迹**必须先跑一帧，
    /// 否则 `fonts_mut` 会 panic。
    fn settled_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        ctx
    }

    /// 图标字体在**没有任何 CJK 字体**的机器上也能注册成功。
    ///
    /// 这条直接覆盖曾经的真实故障：早前实现调用 `ctx.fonts(|f| ..)`，
    /// 在 `App::new`（即第一次 `run` 之前）里直接 panic
    /// *"No fonts available until first call to Context::run()"*，
    /// 于是图标注册被临时整条停用、界面全是豆腐块。
    /// 现在用 `add_font`，**跑帧之前**调用也必须安全。
    #[test]
    fn icon_font_registers_before_first_run() {
        let ctx = egui::Context::default();
        // 关键：此时尚未调用过 run_ui，正是旧实现会 panic 的时机。
        assert!(install_icon_font(&ctx), "图标字体注册应返回成功");
        // 再注册一次也不应 panic（幂等）。
        assert!(install_icon_font(&ctx), "重复注册应仍然成功");
    }

    /// `add_font` 是**追加**而非替换：注册图标字体不能把已有字体挤掉。
    ///
    /// 这条守住 `install_icon_font` 与 `install_cjk_font` 的共存契约——
    /// 若哪天有人改回 `set_fonts`，这里会立刻失败。
    ///
    /// 判据用「注册图标字体后，内置 latin 字体的`A` 仍能命中」：
    /// 内置字体是 egui 自带的，无需构造哨兵数据。
    #[test]
    fn icon_font_keeps_existing_fonts() {
        let ctx = egui::Context::default();
        let font_id = egui::FontId::proportional(16.0);

        // 注册前：内置字体提供 'A'。
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        assert!(
            ctx.fonts_mut(|f| f.has_glyph(&font_id, 'A')),
            "前提：内置 latin 字体应能提供 'A'"
        );

        install_icon_font(&ctx);
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();

        assert!(
            ctx.fonts_mut(|f| f.has_glyph(&font_id, 'A')),
            "注册图标字体后既有字体应仍然可用（add_font 是追加而非替换）"
        );
        assert!(
            ctx.fonts_mut(|f| f.has_glyph(&font_id, crate::icons::Icon::Close.codepoint())),
            "图标码位应能命中图标字体"
        );
    }

    /// 图标字体注册后，每个 `Icon` 变体在字体图集里**有非空墨迹**。
    ///
    /// 这是「图标真的会出现」的下界判据：`has_glyph` 为真只说明
    /// 字体里有这个码位，仍可能是**空白字形**（豆腐块）。
    /// 因此这里进一步走完整排版，拿字形的 `uv_rect` 去字体图集里
    /// 数不透明像素。
    ///
    /// 与 `icons::tests::every_icon_codepoint_has_ink` 互补：那条直接
    /// 构造 `FontDefinitions`，绕开了 `install_icon_font`；本条走的是
    /// **真实注册路径**（`App::new` 用的就是它），因此能抓住
    /// 「注册方式写错→ 图标退化成豆腐块」这类回归。
    #[test]
    fn every_icon_glyph_rasterises_to_ink_after_registration() {
        let ctx = settled_ctx();
        install_icon_font(&ctx);
        // 注册发生在 settled_ctx 跑的那一帧之后，必须再跑一帧让它生效。
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();

        for icon in crate::icons::Icon::ALL {
            if icon.is_vector() {
                continue; // 自绘图标不走字形
            }
            let cp = icon.codepoint();
            // 排版 + 取图集。epaint 0.36 的 `layout` 收 4 个参数，
            // 字形在 `galley.rows[].row.glyphs[]`（不是 `galley.glyphs`）。
            // `FontId` 在本版本不是 `Copy`，所以在闭包内现建、不外传。
            let (has, image, glyphs) = ctx.fonts_mut(|f| {
                // `layout` 按值取 `FontId`（本版本不是 `Copy`），
                // `has_glyph` 按引用，所以这里建两个等价实例。
                let galley = f.layout(
                    cp.to_string(),
                    egui::FontId::proportional(64.0),
                    egui::Color32::WHITE,
                    f32::INFINITY,
                );
                let glyphs: Vec<egui::epaint::text::Glyph> = galley
                    .rows
                    .iter()
                    .flat_map(|r| r.row.glyphs.iter().cloned())
                    .collect();
                (
                    f.has_glyph(&egui::FontId::proportional(64.0), cp),
                    f.image().clone(),
                    glyphs,
                )
            });
            assert!(has, "{icon:?} U+{:04X} 未命中字体", cp as u32);

            let (w, h) = (image.size[0], image.size[1]);
            let mut inked = 0usize;
            for g in &glyphs {
                // epaint 0.36 的 `UvRect::min/max` 是 `[u16; 2]`（纹素坐标），
                // 不是 `Pos2`——直接按 `Vec2` 用会编译不过。
                let uv = g.uv_rect;
                let x0 = (uv.min[0] as usize).min(w);
                let y0 = (uv.min[1] as usize).min(h);
                let x1 = ((uv.max[0] as usize) + 1).min(w);
                let y1 = ((uv.max[1] as usize) + 1).min(h);
                for y in y0..y1 {
                    for x in x0..x1 {
                        if image.pixels[y * w + x].a() > 32 {
                            inked += 1;
                        }
                    }
                }
            }
            assert!(
                inked > 20,
                "{icon:?} U+{:04X} 栅格化后几乎无墨迹（{inked} px），\
                 界面里会显示为豆腐块",
                cp as u32
            );
        }
    }
}