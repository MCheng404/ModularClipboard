//! 设置控件：开关、滑条、步进滑条、分段控件、文本框、迷你按钮。
//!
//! 全部自绘：`egui` 自带控件带立体感，与纯平毛玻璃语言不符。

use egui::Align2;
use egui::Rect;
use egui::Sense;
use egui::Ui;
use egui::pos2;
use egui::vec2;
use modular_clipboard_core::i18n::Msg;
use modular_clipboard_core::i18n::t;

use super::{Frame};

use super::shadow::fade_color;

/// 开关：外观自绘（纯平毛玻璃），命中走 egui。
///
/// # 为什么要自己画而不用 `egui::Checkbox`
///
/// 项目视觉语言是「**纯平毛玻璃**」：禁止一切装饰性拟物效果
/// （渐变、光泽、内发光）。`egui::Checkbox` 自带的勾选框是带立体感的
/// 控件，与整体观感不符。这里保留自绘外观。
///
/// # `id_suffix` 必须稳定且唯一
///
/// `Pos2` 不能作 `egui::Id`（不满足 `Hash`），而用矩形坐标当 id
/// 会因窗口 resize 变化，导致同一开关在不同帧拿到不同 id、
/// 交互状态（hover/active）被反复丢弃。所以传配置**字段名**。
pub(super) fn draw_switch(f: &mut Frame<'_>, ui: &mut Ui, id_suffix: &'static str, v: &mut bool) -> bool {
    let pal = f.pal;
    // 尺寸：基准高 20、宽 38（约 1.9 倍），比旧版 18×32 更饱满。
    // 按 `font_sm` 缩放——开关不承载文字，但必须与旁边放大的
    // 标签视觉配重（1.5× 字号配 20pt 开关会显得开关「缩水」）。
    let h = pal.font_sm * 1.33;
    let w = h * 1.9;
    // NOTE: 同一个矩形只准注册**一次**交互。
    //
    // 早前这里写的是 `allocate_exact_size(size, Sense::click())`
    // 紧接着又 `interact(rect, Id::unique(("switch", …)), Sense::click())`
    // ——同一块区域注册了两个不同 Id 的交互区，靠「后注册者优先」
    // 侥幸正确：一旦有人在中间插一个控件，或者 egui 改了同层交互区
    // 的优先级规则，点击就会落到那个被忽略的 `_alloc` 上，开关直接
    // 失灵而且毫无征兆。
    //
    // 正确分工：`allocate_exact_size` 只负责**占位**（`Sense::empty()`，
    // 纯粹向后推游标），点击交互由下面那条显式 `interact` 承担——
    // 它的 Id 绑定字段名，跨帧稳定。
    let (rect, _alloc) = ui.allocate_exact_size(egui::vec2(w, h), Sense::empty());
    let resp = ui.interact(rect, egui::Id::unique(("switch", id_suffix)), Sense::click());

    let track = Rect::from_center_size(rect.center(), vec2(w, h));
    let p = ui.painter_at(rect);
    let on = *v;
    // 轨道：
    // - 开：accent 实底（hover 时 accent_hover）
    // - 关：中性灰实底（surface_variant），**无描边**——旧版那圈
    //   border 灰边 + 灰色滑块整体灰扑扑，是「丑」的主要来源。
    //   关闭态 hover 叠一层白提亮。
    //
    // 轨道颜色也走补间（130ms）：滑块位置本来就有 `animate_bool` 在滑，
    // 轨道却是硬切——两者不同步会读出「颜色先到、滑块后到」的撕裂感。
    // 设置窗口每帧驱动（无空闲闸门），不需要续帧请求。
    let track_target = if on {
        if resp.hovered() { pal.accent_hover } else { pal.accent }
    } else if resp.hovered() {
        crate::theme::composite(egui::Color32::from_white_alpha(16), pal.surface_variant)
    } else {
        pal.surface_variant
    };
    let track_color = crate::anim::tween_color(
        ui.ctx(),
        egui::Id::unique(("switch_track", id_suffix)),
        track_target,
        std::time::Duration::from_millis(130),
    );
    p.rect_filled(track, h * 0.5, track_color);

    // 滑块位置：平滑动画（egui 按帧插值），不再硬切。
    let anim = ui
        .ctx()
        .animate_bool(egui::Id::unique(("switch_anim", id_suffix)), on);
    let knob_r = h * 0.5 - 3.0;
    let left_x = track.min.x + h * 0.5;
    let right_x = track.max.x - h * 0.5;
    let knob_center = pos2(left_x + (right_x - left_x) * anim, track.center().y);
    // 轻微投影（增加层次），再画白色滑块——开/关都用白滑块
    // （Win11 风格：关闭态白滑块在灰底上比旧版灰滑块干净）。
    p.circle_filled(
        knob_center + vec2(0.0, 0.8),
        knob_r,
        egui::Color32::from_black_alpha(45),
    );
    p.circle_filled(knob_center, knob_r, egui::Color32::WHITE);

    if resp.clicked() {
        *v = !*v;
    }
    // IMPORTANT: 热区 key 必须**带上字段名**。
    //
    // 早前所有开关都登记成同一个 `"settings_switch"`，而 `hit` 是
    // `HashMap<&str, Rect>`——后写的覆盖先写的， map 里只剩**最后一个**
    // 开关的矩形。于是自动化脚本按热区点击会「点 A 项却改了 B 项」，
    // 而所有断言都通过（它们只读得到的那一个键）。
    //
    // 逐个登记还有一个好处：测试能逐项核对边界，而不是只看最后一个。
    f.hit_insert(id_suffix, rect);
    *v != on
}

/// 圆角滑块（替代 `egui::Slider`）：纯平毛玻璃风格。
///
/// # 为什么不用 `egui::Slider`
///
/// 与 [`draw_switch`] 同理：默认滑块是细方轨 + 方划块，与本仓自绘的
/// 开关 / 分段控件不是一套视觉语言，设置页里并排出现「三种控件三种
/// 质感」。这里统一为：全圆角轨道（半径 = 轨高一半）+ accent 填充
/// 已选部分 + 白色圆 knob（带轻投影，与开关滑块一致）。
///
/// # 交互
///
/// 点击轨道任意处跳转到该值，按住拖动连续调整。禁用态（如背景效果为
/// 「无」时不透明度无意义）只绘制不交互，整体压暗。
///
/// # `variant`：同一 `id_suffix` 下的第几条滑条
///
/// 令牌编辑器里「色相 / 透明度」双滑条共用令牌名，靠本参数区分 Id。
/// CAUTION: 不区分的话，同一帧两个矩形注册同一个 `Id`，后注册者赢得
/// 交互——拖动 A 会连带动 B，且毫无征兆。空串 = 唯一滑条。
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_slider(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    id_suffix: &'static str,
    variant: &'static str,
    v: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    enabled: bool,
    // CAUTION: 行高兜底，必须由调用方按「本行最高控件」传入（令牌行
    // 见 `token_row_h`）。传 0 = 保持旧的「滑条自定高」行为。
    //
    // 为什么需要它：egui 的 horizontal **不重排先放置的控件**——若滑条
    // 比左列文字高，左列被顶部对齐，文字相对滑条偏高；150% DPI 下
    // font_xs=21（galley 高 27pt）而左列格子写死 18pt 时还会把字形
    // 上下裁掉（用户实机反馈「文字向上且渲染不完整」，2026-10-19）。
    // 三列同高 + 本参数兜底，两个病一起治。
    cell_h: f32,
) -> bool {
    let pal = f.pal;
    // 轨道 150x6，knob 直径 16——比开关轨道（20）细，但 knob 同为白圆，
    // 两个控件摆在一页里粗细有致又不散。尺寸随字号缩放。
    let track_w = pal.font_sm * 10.0;
    let track_h = (pal.font_sm * 0.4).max(4.0);
    let knob_d: f32 = pal.font_sm * 1.07;
    let cell_h = cell_h.max(knob_d).max(track_h);
    // 分工同 draw_switch：allocate 只占位，交互走显式 interact（Id 绑字段名）。
    let (rect, _) = ui.allocate_exact_size(egui::vec2(track_w, cell_h), Sense::empty());
    let resp = ui.interact(
        rect,
        egui::Id::unique(("slider", id_suffix, variant)),
        if enabled {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        },
    );

    let (lo, hi) = (*range.start(), *range.end());
    let orig = *v;
    // 拖动/点击 → 值：指针在轨道上的相对位置直接映射。
    // `is_pointer_button_down_on` 覆盖「按下即跳转」，`dragged` 覆盖按住拖动
    // 连续调整。两者都要：只用 dragged 的话必须先移动一像素才响应。
    if enabled
        && (resp.dragged() || resp.is_pointer_button_down_on())
        && let Some(pp) = resp.interact_pointer_pos()
    {
        let t = ((pp.x - rect.min.x) / track_w).clamp(0.0, 1.0);
        *v = lo + (hi - lo) * t;
    }
    let t = ((*v - lo) / (hi - lo)).clamp(0.0, 1.0);

    let p = ui.painter_at(rect);
    let track = Rect::from_min_size(
        pos2(rect.min.x, rect.center().y - track_h * 0.5),
        vec2(track_w, track_h),
    );
    let fill_w = track_w * t;
    // 禁用态整体压暗：轨道退到描边色，填充退到弱文字色，knob 不再纯白。
    let track_bg = if enabled { pal.surface_variant } else { pal.border_subtle };
    let fill = if !enabled {
        pal.text_dim
    } else if resp.hovered() || resp.dragged() {
        pal.accent_hover
    } else {
        pal.accent
    };
    p.rect_filled(track, track_h * 0.5, track_bg);
    if fill_w > 0.5 {
        p.rect_filled(
            Rect::from_min_size(track.min, vec2(fill_w, track_h)),
            track_h * 0.5,
            fill,
        );
    }
    // knob 圆心钳在轨道内：值到两端时圆不出轨。
    let knob_x = (track.min.x + fill_w).clamp(track.min.x + knob_d * 0.5, track.max.x - knob_d * 0.5);
    let knob_c = pos2(knob_x, track.center().y);
    let knob_r = knob_d * 0.5;
    p.circle_filled(
        knob_c + vec2(0.0, 0.8),
        knob_r,
        egui::Color32::from_black_alpha(45),
    );
    p.circle_filled(
        knob_c,
        knob_r,
        if enabled { egui::Color32::WHITE } else { pal.text_dim },
    );

    // 热区登记：自动化按字段名定位（同开关的约定）。
    f.hit_insert(id_suffix, rect);
    (*v - orig).abs() > f32::EPSILON
}

/// 离散档位滑块：外观与 [`draw_slider`] 一致，但值**吸附到整数档位**
/// （磨砂强度 1~10）——knob 与填充都按吸附后的档位走，一格一格清晰，
/// 不会停在两档之间。返回本帧档位是否被改变。
pub(super) fn draw_step_slider(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    id_suffix: &'static str,
    v: &mut u8,
    range: std::ops::RangeInclusive<u8>,
    enabled: bool,
) -> bool {
    let pal = f.pal;
    // 尺寸与 `draw_slider` 同源（随字号缩放）。
    let track_w = pal.font_sm * 10.0;
    let track_h = (pal.font_sm * 0.4).max(4.0);
    let knob_d: f32 = pal.font_sm * 1.07;
    let cell_h = knob_d.max(track_h);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(track_w, cell_h), Sense::empty());
    let resp = ui.interact(
        rect,
        egui::Id::unique(("step_slider", id_suffix)),
        if enabled {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        },
    );

    let (lo, hi) = (*range.start() as f32, *range.end() as f32);
    let orig = *v;
    if enabled
        && (resp.dragged() || resp.is_pointer_button_down_on())
        && let Some(pp) = resp.interact_pointer_pos()
    {
        let t = ((pp.x - rect.min.x) / track_w).clamp(0.0, 1.0);
        // 指针位置 → 最近的整数档位（t=0 落 lo，t=1 落 hi）。
        let mapped = lo + (hi - lo) * t;
        *v = mapped.round().clamp(lo, hi) as u8;
    }
    // 关键：归一化用吸附后的整数档位，knob 才会一格一格跳。
    let t = ((*v as f32 - lo) / (hi - lo)).clamp(0.0, 1.0);

    let p = ui.painter_at(rect);
    let track = Rect::from_min_size(
        pos2(rect.min.x, rect.center().y - track_h * 0.5),
        vec2(track_w, track_h),
    );
    let fill_w = track_w * t;
    let track_bg = if enabled { pal.surface_variant } else { pal.border_subtle };
    let fill = if !enabled {
        pal.text_dim
    } else if resp.hovered() || resp.dragged() {
        pal.accent_hover
    } else {
        pal.accent
    };
    p.rect_filled(track, track_h * 0.5, track_bg);
    if fill_w > 0.5 {
        p.rect_filled(
            Rect::from_min_size(track.min, vec2(fill_w, track_h)),
            track_h * 0.5,
            fill,
        );
    }
    let knob_x = (track.min.x + fill_w).clamp(track.min.x + knob_d * 0.5, track.max.x - knob_d * 0.5);
    let knob_c = pos2(knob_x, track.center().y);
    let knob_r = knob_d * 0.5;
    p.circle_filled(
        knob_c + vec2(0.0, 0.8),
        knob_r,
        egui::Color32::from_black_alpha(45),
    );
    p.circle_filled(
        knob_c,
        knob_r,
        if enabled { egui::Color32::WHITE } else { pal.text_dim },
    );

    f.hit_insert(id_suffix, rect);
    *v != orig
}

/// MCP 总开关左侧的 ⓘ 详细按钮（打开 MCP 调用设置覆盖层）。
///
/// 自绘圆形信息图标：accent 底色 + 圆点 + 竖线（不依赖字体字形，
/// 图标字体里没有 info 码位，新增字形需要重新生成 TTF）。
/// 分段控件某一格的**交互 ID**（`ui.interact` 用）。
///
/// # CAUTION: 键里**不能掺选项译文**
///
/// 早前写成 `Id::unique(("seg", i, shown))`——`shown` 是当前语言的
/// 选项译文。设置重组（主题并入外观）后同一分类里会出现**同文**选项：
/// 风格主题（`secondary_theme`）与背景效果（`window_backdrop`）的第 0 项
/// 都叫「无」，于是两格的 ID 逐位相同。
///
/// 后果是 egui 的 widget ID 冲突：同一帧里后注册的那格把
/// `widgets.insert(id, ..)` 里的矩形覆盖掉，先注册那格的 `Response`
/// 按**别人的矩形**判命中 ⇒ **点不动**。用户报的「风格主题无法选择『无』」
/// 就是它（风格主题那行在上方，是先注册的一方）。同时 egui 会在画面上
/// 画红色冲突告警框。
///
/// 只用「控件实例 + 段下标」：与语言无关、与选项译文无关，同一帧内
/// 各控件的 `id_suffix` 互不相同（设置项字段名），下标天然不重复。
pub(super) fn seg_hit_id(id_suffix: &'static str, i: usize) -> egui::Id {
    egui::Id::unique(("seg", id_suffix, i))
}

/// 分段选择控件（segmented control）：整体一个圆角实底（surface_variant），
/// 选中项用 surface 实底 + accent 文字，未选中 text_dim。
///
/// 替代 egui 的 `selectable_label`——后者选中态是半透明 row_selected，
/// 多个胶囊标签挨着时半透明叠半透明，显脏（2026-10-11 用户反馈）。
///
/// # 选中指示器滑动（2026-10-17 动效）
///
/// 早前选中块是「哪项选中就往哪项画一个实底」——切换时旧块消失、
/// 新块出现，观感上是**跳变**。现在选中块的位置按**段索引**补间
/// （`("seg_pill", id_suffix)`，160ms OutCubic）：切换时它从旧段
/// **滑到**新段，文字颜色同步交叉淡入。`id_suffix` 必须是每个控件
/// 实例稳定且唯一的标识（传设置项字段名）——同一帧里可能有多个
/// 分段控件，共用 id 会互相串台。
///
/// 返回 `Some(value)` 表示本帧点击了某项。
///
/// # 热区键（2026-10-25 起两枚）
///
/// 1. `"{id_suffix}/{选项英文键}"`——命名空间键，**优先用它**。设置
///    重组（主题并入外观）后同一分类里会出现同文选项，译文键互相覆盖；
/// 2. 选项译文——兼容既有调用方与测试。
pub(super) fn draw_segmented<T: Copy + PartialEq>(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    // 本实例的唯一标识。同时是热区命名空间键的前缀与补间键前缀——
    // 同一帧里可能有多个分段控件，共用 id 会互相串台。
    id_suffix: &'static str,
    options: &[(T, Msg)],
    cur: T,
) -> Option<T> {
    let pal = f.pal;
    // 高度随字号缩放（`control_height` 已含字体缩放）：固定 30pt 时
    // 字号放大到 1.5× 会顶出控件上下边缘。
    let h = pal.control_height + 2.0;
    // 每项宽度按文字测量 + 内边距，统一取最长项（项宽一致更整齐）。
    let pad = 20.0;
    let font = egui::FontId::proportional(pal.font_sm);
    let mut seg_w = 0.0_f32;
    for (_, label) in options {
        let label = t(label);
        let w = ui
            .painter()
            .layout_no_wrap(label.to_string(), font.clone(), pal.text)
            .rect
            .width();
        seg_w = seg_w.max(w + pad);
    }
    // 自适应可用宽度：字体放大后自然宽度会超过卡片，必须等比压缩，
    // 否则控件被画到卡片之外（用户反馈「字体调整太大导致布局错乱」）。
    // 压到每段不足 1 个汉字宽时就不再压——宁可溢出，也不能把选项挤成
    // 不可辨认的窄条。
    let avail = ui.available_width().max(0.0);
    let min_seg = pal.font_sm + pad * 0.5;
    if avail > 0.0 {
        let max_seg = avail / options.len() as f32;
        seg_w = seg_w.min(max_seg).max(min_seg.min(seg_w));
    }
    let total_w = seg_w * options.len() as f32;
    let (r, _) = ui.allocate_exact_size(vec2(total_w, h), egui::Sense::empty());
    let painter = ui.painter_at(r);
    // 全圆角改版（2026-10-25）：容器从 `radius_md`(6) 改**胶囊**
    // （半径 = 控件高一半）——分段控件整体读作一枚胶囊，选中段是胶囊
    // 里的一枚小胶囊。
    painter.rect_filled(r, h * 0.5, pal.surface_variant);
    // ---- 选中指示器：按段索引补间滑动 --------------------------------
    //
    // 目标值 = 当前选中段的下标（`position` 找不到时给 0——理论上
    // `cur` 必在 options 里，但泛型调用方不保证，兜底 0 比 panic 好）。
    // 补间从当前显示值续接：连续切换时它在中途改方向，不会跳。
    let sel_idx = options
        .iter()
        .position(|(v, _)| *v == cur)
        .unwrap_or(0) as f32;
    let pill_idx = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::unique(("seg_pill", id_suffix)),
        sel_idx,
        std::time::Duration::from_millis(160),
        crate::anim::Ease::OutCubic,
    );
    // 指示器左右各内缩 2pt（与原实现一致），圆角改**胶囊**（2026-10-25
    // 全圆角改版：radius_sm → (h-4)/2，与容器同语言）。
    let pill = Rect::from_min_max(
        pos2(r.min.x + seg_w * pill_idx + 2.0, r.min.y + 2.0),
        pos2(r.min.x + seg_w * (pill_idx + 1.0) - 2.0, r.max.y - 2.0),
    );
    painter.rect_filled(pill, (h - 4.0) * 0.5, pal.surface);
    let mut picked = None;
    for (i, (value, label)) in options.iter().enumerate() {
        // `label` 是**英文键**（`Msg`）；`shown` 才是当前语言的译文。
        // 热区的命名空间键用英文键（语言无关、恒定），译文键作兼容保留。
        let shown = t(label);
        let seg = egui::Rect::from_min_max(
            pos2(r.min.x + seg_w * i as f32, r.min.y),
            pos2(r.min.x + seg_w * (i + 1) as f32, r.max.y),
        );
        let resp = ui.interact(seg, seg_hit_id(id_suffix, i), egui::Sense::click());
        // 文字放不下时按**段宽**省略：`draw_text` 不裁剪，直接画会
        // 越到相邻段上（视觉上两段文字叠在一起）。
        let text = elide_to_width(ui, shown, &font, seg.width() - 8.0);
        // 文字颜色补间（130ms）：选中态 accent、悬停提亮、常态弱化，
        // 三者之间交叉淡入，与滑动指示器同步。首帧直接落到目标
        // （`tween_color` 惰性插入），之后才在变化时补间。
        let target_color = if cur == *value {
            pal.accent
        } else if resp.hovered() {
            pal.text
        } else {
            pal.text_dim
        };
        let color = crate::anim::tween_color(
            ui.ctx(),
            egui::Id::unique(("seg_text", id_suffix, shown)),
            target_color,
            std::time::Duration::from_millis(130),
        );
        painter.text(
            seg.center(),
            egui::Align2::CENTER_CENTER,
            text,
            font.clone(),
            color,
        );
        if resp.clicked() {
            picked = Some(*value);
        }
        // 登记热区（两枚键，2026-10-25）：
        //
        // 1. `"{控件名}/{选项英文键}"`——**命名空间键**。设置重组后同一
        //    分类里会出现同文选项（外观的风格主题与背景效果都有
        //    `"None"`），译文键互相覆盖（HashMap 后注册的赢），自动化
        //    再也点不到先注册的那个。命名空间键按控件寻址，不受影响，
        //    且与界面语言无关（英文键恒定）。
        // 2. 选项译文——既有测试与调用方按选项文字寻址，保留兼容。
        f.hit_insert(format!("{id_suffix}/{label}"), seg);
        f.hit_insert(shown, seg);
    }
    picked
}

/// 把 `text` 省略到不超过 `max_w`（超出部分用 `…` 收尾）。
///
/// 与 `egui::Label::truncate()` 的分工：那个作用于**控件**，本函数作用于
/// **自绘文字**（`painter.text` 不做任何裁剪，直接画会越到相邻分段上）。
/// 逐字二分前缀 + `layout_no_wrap` 测量，成本与文字长度成对数关系。
pub(super) fn elide_to_width(ui: &Ui, text: &str, font: &egui::FontId, max_w: f32) -> String {
    if max_w <= 0.0 {
        return String::new();
    }
    let measure = |s: &str| {
        ui.painter()
            .layout_no_wrap(s.to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
            .rect
            .width()
    };
    if measure(text) <= max_w {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    // 二分找最长可容纳前缀（含省略号）。
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>() + "…";
        if measure(&candidate) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        return String::new();
    }
    chars[..lo].iter().collect::<String>() + "…"
}

/// 文本输入框：胶囊背景（surface_variant 实底 + 边框），在右侧区域里靠右。
///
/// 旧实现用 `frame(Frame::NONE)` 无背景，在 right_to_left 里虽靠右但没有
/// 可见边界，文字起点不明，看起来「没有对齐右边」。改为先 allocate 固定
/// 宽度区域、画背景，再在内部放 TextEdit；聚焦时边框变 accent。
///
/// 2026-10-25 全圆角改版：圆角从 `radius_md`(6) 改**胶囊**（半高）。
pub(super) fn draw_text_field(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    // 本输入框的唯一标识（配置字段名）。聚焦描边的补间键要按框分开——
    // 共用 id 会让两个框的聚焦动画互相串台（点 A 框时 B 框也亮）。
    id_suffix: &'static str,
    buf: &mut String,
    ph: &str,
    secret: bool,
    w: f32,
) -> bool {
    let pal = f.pal;
    // 高度随字号缩放：固定 30pt 时 1.5× 字号（行高 ~33）会被裁掉。
    let h = pal.control_height + 2.0;
    let pill = h * 0.5;
    let (r, _) = ui.allocate_exact_size(vec2(w, h), egui::Sense::empty());
    let painter = ui.painter_at(r);
    painter.rect_filled(r, pill, pal.surface_variant);
    // 常态描边；聚焦时**叠**一层补间的 accent 描边（颜色 130ms 过渡，
    // 宽度仍按 1.0 → 1.5 两档硬切——宽度补间在 1px 量级上看不出，反而
    // 要让抗锯齿边缘抖一下）。
    painter.rect_stroke(
        r,
        pill,
        egui::Stroke::new(1.0, pal.border),
        egui::StrokeKind::Inside,
    );
    let text_rect = r.shrink2(vec2(8.0, 4.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(text_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let resp = child.add(
        egui::TextEdit::singleline(buf)
            .hint_text(ph)
            .password(secret)
            .frame(egui::Frame::NONE)
            .text_color(pal.text)
            .desired_width(text_rect.width()),
    );
    // 聚焦描边补间（130ms）：失焦时 accent 淡回 border 色，不再是
    // 「亮点一下又缩回去」的硬切。设置窗口每帧驱动，无需续帧请求。
    let focus_k = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::unique(("field_focus", id_suffix)),
        if resp.has_focus() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(130),
        crate::anim::Ease::OutCubic,
    );
    if focus_k > 0.004 {
        let ring = crate::anim::tween_color(
            ui.ctx(),
            egui::Id::unique(("field_focus_color", id_suffix)),
            pal.accent,
            std::time::Duration::from_millis(130),
        );
        painter.rect_stroke(
            r,
            pill,
            egui::Stroke::new(1.0 + 0.5 * focus_k, ring),
            egui::StrokeKind::Inside,
        );
    }
    resp.changed()
}

/// 数字输入的胶囊外壳（2026-10-25 全圆角改版）。
///
/// 先铺胶囊底（`surface_variant` + 发丝边），再把调用方的控件
/// （`DragValue`）放进内区 child。存在理由：`DragValue` 没有
/// `desired_width`（egui 0.37 未提供），不能像 [`draw_text_field`]
/// 那样把 TextEdit 直接收进自绘外壳；固定宽 + 内 child `max_rect`
/// 约束是同等效果的最小改法。
///
/// 宽度按字号缩放（基准 112 @ font_sm 15，与热键输入框同档）：
/// 最长内容是「2000 entries」这类「数值 + 单位」组合。
pub(super) fn draw_number_field(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    add: impl FnOnce(&mut Ui) -> bool,
) -> bool {
    let pal = f.pal;
    let h = pal.control_height + 2.0;
    let w = pal.font_sm * 7.5;
    let (r, _) = ui.allocate_exact_size(vec2(w, h), egui::Sense::empty());
    let pill = h * 0.5;
    let p = ui.painter_at(r);
    p.rect_filled(r, pill, pal.surface_variant);
    p.rect_stroke(
        r,
        pill,
        egui::Stroke::new(1.0, pal.border_subtle),
        egui::StrokeKind::Inside,
    );
    let inner = r.shrink2(vec2(10.0, 2.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    add(&mut child)
}

/// 迷你文字按钮（设置页行内辅助动作：「恢复默认」「重置」等）。
/// 自绘理由同开关 / 滑块：纯平毛玻璃语言要统一，`egui::Button` 的
/// 默认立体感与整体观感不符。悬停底色走 110ms 补间（与顶栏图标按钮
/// 同一套机制，见 `icon_button` 附近的注释）。
///
/// # id_suffix 必须全局唯一
///
/// 同时是补间键与交互 Id——同一帧出现两个同名迷你按钮会共享交互与
/// 动画状态（`draw_slider` 的 `variant` 参数是同一教训的行内版）。
///
/// # `hit`：热区登记开关
///
/// `Some(key)` 时把按钮矩形登记进热区表（自动化寻址用）；`None` 时
/// 不登记——令牌行的「重置」按钮与色相滑条**共用令牌名**，都登记的
/// 话后画的覆盖先画的（`hit` 是 HashMap），自动化就再也找不到滑条。
/// 交互本身走 egui `Id`（`id_suffix`），与热区表无关。
pub(super) fn draw_mini_button(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    id_suffix: &'static str,
    text: &str,
    hit: Option<&'static str>,
) -> bool {
    let pal = f.pal;
    let font = egui::FontId::proportional(pal.font_sm);
    let text_w = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font.clone(), pal.text)
        .rect
        .width();
    let size = vec2(text_w + 16.0, pal.control_height * 0.82);
    let (r, _) = ui.allocate_exact_size(size, egui::Sense::empty());
    let resp = ui.interact(r, egui::Id::unique(("mini_btn", id_suffix)), Sense::click());
    let p = ui.painter_at(r);
    // 悬停补间（110ms OutCubic）：目标值由本帧是否悬停给出。
    let hover = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::unique(("mini_btn_hover", id_suffix)),
        if resp.hovered() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(110),
        crate::anim::Ease::OutCubic,
    );
    // 先铺不透明底再叠半透明提亮（与类型栏按钮同一个理由：半透明
    // 令牌不能直接当填充，见 `draw_type_rail` 的 CAUTION）。
    //
    // 全圆角改版（2026-10-25）：radius_sm → 胶囊（半高）。
    let pill = r.height() * 0.5;
    p.rect_filled(r, pill, pal.surface_variant);
    if hover > 0.004 {
        p.rect_filled(r, pill, fade_color(pal.row_hover, hover));
    }
    p.rect_stroke(
        r,
        pill,
        egui::Stroke::new(1.0, pal.border_subtle),
        egui::StrokeKind::Inside,
    );
    p.text(r.center(), Align2::CENTER_CENTER, text, font, pal.text);
    if let Some(key) = hit {
        f.hit_insert(key, r);
    }
    resp.clicked()
}

#[cfg(test)]
mod tests {
    use super::seg_hit_id;

    /// 设置里所有分段控件的实例名（与 `settings_rows.rs` 的调用点一致）。
    ///
    /// 同一帧内它们必须两两不撞——撞了就是「某一行点不动」。
    const SEG_SUFFIXES: [&str; 9] = [
        "font_scale",
        "notice_popup",
        "paste_previous_mode",
        "pinned_mode",
        "search_panel_pos",
        "secondary_theme",
        "theme_mode",
        "type_rail_pos",
        "window_backdrop",
    ];

    #[test]
    fn seg_ids_are_unique_across_controls() {
        // 回归：风格主题与背景效果的第 0 项都叫「无」。ID 掺了选项译文
        // 时两者逐位相同 ⇒ 先注册的行（风格主题）点不动，egui 还会画
        // 红色 ID 冲突告警框（2026-10-25 用户报「风格主题无法选择 无」）。
        let a = seg_hit_id("secondary_theme", 0);
        let b = seg_hit_id("window_backdrop", 0);
        assert_ne!(a, b, "不同控件的同一下标必须给出不同 ID");

        // 全部控件 × 前 4 个下标两两不同。
        for (i, x) in SEG_SUFFIXES.iter().enumerate() {
            for y in &SEG_SUFFIXES[i + 1..] {
                for idx in 0..4 {
                    assert_ne!(
                        seg_hit_id(x, idx),
                        seg_hit_id(y, idx),
                        "{x} 与 {y} 的第 {idx} 项 ID 冲突"
                    );
                }
            }
        }
    }

    #[test]
    fn seg_id_is_index_scoped_and_deterministic() {
        // 同一控件内各段必须分开（否则 `clicked` 会串到别的段上）。
        let ids: Vec<_> = (0..4).map(|i| seg_hit_id("theme_mode", i)).collect();
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(a, b, "同一控件的第 {i} 段与后面某段 ID 相同");
            }
        }
        // 确定性：同一 (控件, 下标) 每次必须给同一个 ID，否则补间与
        // 交互状态每帧丢失。
        assert_eq!(seg_hit_id("theme_mode", 2), seg_hit_id("theme_mode", 2));
    }
}
