//! 历史行：行内按钮、内联编辑、预览带与元信息。
//!
//! 一行 = 顶带（来源 · 时间）+ 预览带（文本 / 来源 / Markdown /
//! 图片）+ 底带（类型 · 大小）；编辑态把预览带换成编辑器。

use egui::Align2;
use egui::Rect;
use egui::Sense;
use egui::Stroke;
use egui::pos2;
use egui::vec2;
use modular_clipboard_app::Service;
use modular_clipboard_core::ClipItem;
use modular_clipboard_core::config::RowMetaPart;
use modular_clipboard_core::i18n::t;
use crate::theme::sized;

use super::{Frame, Op, ROW_RADIUS};

use super::edit_icons::paint_check_icon;
use super::edit_icons::paint_pencil_icon;
use super::edit_icons::paint_undo_icon;
use super::list::IMG_MAX_H_LIST;
use super::list::LINE_H;
use super::list::ROW_ACTS_DROP;
use super::list::ROW_PAD_H;
use super::list::acts_width;
use super::list::image_display_size;
use super::list::preview_full_width;
use super::list::preview_width;
use super::list::row_top_height;
use super::shadow::fade_color;
use super::shadow::fill_rounded_border;

pub(super) fn draw_item_row(f: &mut Frame<'_>, ui: &egui::Ui, r: Rect, item: &ClipItem) {
    let pal = f.pal;
    let scale = f.scale;
    let selected = f.svc.state.selected == Some(item.id);

    // CAUTION: 交互必须挂在**滚动区的内容 `ui`** 上，不能挂根 `f.ui`。
    //
    // `f.ui` 是整个主区的 Ui，它的裁剪矩形是**整个窗口**——于是
    // 被滚出可视区的那半行仍然可点：点在卡片标题、甚至顶栏上，
    // 都会选中列表里的条目（P0 级：顶栏按钮点不动）。
    //
    // `ScrollArea` 的内容 `ui` 裁剪到视口，越界的行自动不可交互，
    // 与「看得见才点得到」一致。
    let resp = ui.interact(r, egui::Id::unique(("row", item.id)), Sense::click());
    // 自报行本体热区：测试与 MCP 自动化需要「点这一行」的坐标，
    // 不该各自重算行矩形。CAUTION: 多行共用同一个 key，HashMap 后者
    // 覆盖前者 ⇒ 读到的是**最后绘制的那一行**（可见行里最靠下的）。
    // 单行夹具无歧义；多行测试请按「末行」使用。
    f.hit_insert("row_body", r);
    let painter = ui.painter_at(r);

    // 卡片样式：每条历史是一张圆角小卡。
    //
    // 常态 = `surface_raised`（提亮后的卡片）底 + 1px 描边，浮在暗色
    // 列表区（`list_bg`）之上；悬停在半透明提亮上叠一层。
    //
    // NOTE: 圆角用 `ROW_RADIUS` 而不是 `CornerRadius::ZERO`——后者是
    // 早前「通栏行」的直角样式，与卡片语义不符。
    //
    // NOTE: 描边在所有状态都画：悬停若只换底色，边框会「消失一瞬」
    // （从有到无再到有），视觉上跳变。
    let radius = ROW_RADIUS;

    // 卡片阴影**不在本函数画**：它在行内容之外（行间缝隙、列表边缘），
    // 而这里的 `painter` 被裁剪到行矩形 `r`，外扩晕带会被裁掉。
    // 阴影由 `draw_history_body` 在所有行内容之前统一画一遍
    // （见 `draw_card_shadow` 的裁剪/顺序说明）。

    // 常态描边已取消，原因见下面 `selected` 分支处的说明（feathering 在
    // 凸角处重叠会把它变成一圈深色短线）。
    // 选中态描边加粗到 2pt。向内生长、不占布局 —— 内容位置与常态完全一致。

    //
    // # CAUTION: 非选中态**不画描边**（这里曾有一圈极弱描边）
    //
    // 根因（像素级排查得出，不是猜的）：egui 的边缘抗锯齿（feathering）
    // 会沿每条边生成一条 alpha 渐变的填充带；在**凸角**处，「直线 → 弧线」
    // 的接点两侧填充带**重叠**，alpha 被累加 2~4 次。
    //
    // 实测数据：描边色亮度 ED(237)、背景 F4(244)，而弧线两端的像素是
    // C4/CD(196/205)——**比描边本身还暗**。单纯叠加两个颜色不可能产生
    // 比两者都暗的结果，只有「同一形状叠了多次」才会。
    //
    // 卡片自身的白色填充也有同样的重叠，但白叠白看不出来；**只有比背景
    // 更暗的描边会把它暴露成圆角末端的一段深色短线**。
    //
    // 所以这里彻底去掉常态描边：卡片靠 `surface_raised` 与窗口底 `bg`
    // 的明暗差「浮起」，本就是这个界面「层次不靠描边、靠浮起」的设计意图。
    //
    // 选中态保留 accent 描边：它是饱和亮色，叠加后只是边缘更实一点，
    // 不产生「比自身更暗」的灰边；且整列表只有一行处于选中态。
    //
    // CAUTION: 宽度是**逻辑点**、不乘 scale（F-35 / UI-P1-4）：egui 的
    // Stroke 单位就是逻辑点，乘 ppp 会让 150% DPI 下画成 3pt，与注释
    // 承诺的「2pt」不符，也和其他 1.0 逻辑点的描边不成体系。
    //
    // 本行是否正处于**内联编辑态**（编辑模式的所有让位判据都靠它）。
    //
    // CAUTION: 必须算在**最前面**——卡片分层（悬停/按下/高光）就在这
    // 下面，它们也要按编辑态让位。
    //
    // 判据必须是 `editing == Some(本行 id)`：`editing` 是**全局单编辑器**
    // （同一时刻只有一条在改），别的行照常画预览。
    let editing_this = f.state.editing == Some(item.id);

    // ---- 卡片分层：底 → 置顶 → 选中 → 悬停 → 按下 ---------------------
    //
    // 每层都是一层半透明矩形往上叠；除置顶（静态数据标记）外全部走补间
    // （`anim::tween_f32`：目标值由本帧状态给出、从当前显示值续接）——
    // 鼠标划过、按下、选中、取消选中都有平滑过渡，不再是硬切。
    //
    // 顺序上选中在悬停/按下**之下**：accent 环是稳定骨架，指针反馈叠在
    // 它上面——悬停一个已选中的行仍然看得到反应（旧实现选中与悬停互斥，
    // 选中行对指针毫无反馈）。
    // ---- 卡片磨砂底：模糊的身后桌面 --------------------------------
    //
    // 必须画在卡片色**之前**：磨砂是**底**，卡片色是盖在它上面的那层
    // 玻璃，顺序反了就把底图盖死。
    //
    // 没有这一层时，把卡片调透只能透出窗口自己的 `list_bg`（比卡片更暗）
    // ⇒ 观感是「卡片变暗」而不是「卡片变透」。玻璃的关键不是能透多少，
    // 而是**透过来的是什么**（2026-10-23 用户反馈）。
    //
    // 纹理是**整窗客户区**的底图，按卡片矩形取其中一块（见
    // `card_blur_uv`），因此每张卡片看到的是它自己身后那一块桌面。
    // 液态玻璃：本帧起这张卡片要参与折射合成（仅背景效果 = 玻璃时生效）。
    // 必须在画底色**之前**——玻璃 pass 跑在 egui 内容之前。
    // CAUTION: 判据是 `glass_covers()`（本窗口真的被玻璃 pass 覆盖），
    // 不是全局的 `glass_mode()`。子窗口/弹窗配置同样是玻璃，却没有
    // 任何东西替它们铺背景——用错判据会让那些窗口整块透出桌面
    // （见 `Frame::glass_covers`）。
    let glass_mode = f.glass_covers();
    // 底色**作为参数**交给玻璃层：玻璃层用它算 tint 与覆盖度。
    //
    // 早前是「先 push 再 set_tint / set_alpha」的两步式，而 tint 全局只有
    // 一份 ⇒ 顶栏、搜索面板、类型栏都被行卡片的底色染一遍。现在每块
    // 区域自带底色，语义也统一了：**覆盖度 = 底色的 alpha**。
    f.push_glass(r, radius, pal.card_bg);
    // 卡片磨砂补丁：**玻璃模式下不画**（2026-10-25 修「玻璃参数无效」）。
    //
    // 病根：这层补丁是 GDI 捕获的**不透明**纹理，画在玻璃 pass 之后，
    // 把卡片矩形内的折射产出**整个盖住**——玻璃模式下卡片看到的仍是
    // 「不透明补丁 + 卡片色」，与磨砂模式逐字节一致，折射/色散/边缘光
    // 三个参数于是无处生效（实测只变 refract：A/B 差异 0.02%，噪声级）。
    //
    // 玻璃模式下卡片的「身后内容」由玻璃 pass 提供（清晰+模糊双底图、
    // 按卡片形状两级折射，输出 alpha = 卡片色 alpha），补丁属于被取代
    // 的旧机制，继续画只会帮倒忙。磨砂/半透明模式照画（那里没有玻璃
    // pass，补丁是卡片能透出身后桌面的唯一途径）。
    if !glass_mode
        && let Some(tex) = f.state.card_blur_tex.as_ref()
    {
        let mut shape = egui::epaint::RectShape::filled(r, radius, egui::Color32::WHITE);
        // `brush` 是 `Option<Arc<Brush>>`——纹理刷子要能在图元之间
        // 共享（同一张底图被几十张卡片采样），所以是 Arc 而不是值。
        shape.brush = Some(std::sync::Arc::new(egui::epaint::Brush {
            fill_texture_id: tex.id(),
            uv: card_blur_uv(f.area, r),
        }));
        painter.add(shape);
    }
    // 行卡片底色用 `card_bg` 而不是 `surface_raised`：前者是「卡片透明度」
    // 设置的作用对象，后者是浮层令牌（见 `Palette::with_card_opacity`）。
    //
    // CAUTION: **玻璃模式下不画**（2026-10-27）。玻璃 pass 已经把
    // 「底图 + 卡片色」合成好了（`GlassParams::tint`），这里再画一层
    // 就是两次覆盖：
    //   最终 alpha = a + a(1-a) = a(2-a)
    // 卡片色不透明（a = 1）时玻璃整层被盖死——折射、色散、边缘光全部
    // 看不见，用户只能看到一张实色卡片；半透明时又多叠一份桌面而发白。
    // 这也是「玻璃参数拖了没反应」在 2026-10-25 修过一次、却仍不够通透的
    // 根因：补丁被跳过了，底色没有。
    if !glass_mode {
        painter.rect_filled(r, radius, pal.card_bg);
    }
    // 置顶标记：底色微亮。置顶条目就在同一个列表里，靠这个区分。
    //
    // NOTE: 只靠色差是不够的 —— 实测 7% 透明叠加几乎看不出差别。
    // 行内 pin 按钮的实心/描边是主要的形状线索，底色只是辅助。
    if item.pinned {
        painter.rect_filled(r, radius, pal.pin_tint);
    }
    // 选中态补间（140ms OutCubic）：accent 描边 0→2pt、选中底 alpha
    // 0→满。切换选中时 accent 环从细到粗「长」出来，取消时收回去。
    //
    // 阈值 0.004：淡出完成后一层 alpha≈0 的矩形是纯浪费。同时它保证
    // `fill_rounded_border` 收到的 w > 0——该函数对 w<=0 的兜底是
    // 「整块实心 border 填充」，被 0 触发的后果是整行刷成 accent。
    let sel = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::unique(("row_sel", item.id)),
        if selected { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(140),
        crate::anim::Ease::OutCubic,
    );
    if sel > 0.004 {
        if glass_mode {
            // 玻璃模式下**不能**重填卡片底色（2026-10-28 修）。
            //
            // 原路径是 `fill_rounded_border(...)`：它先把整块填成描边色、
            // 再用闭包把内部覆盖回 `pal.card_bg`。在磨砂/半透明模式下
            // 这是对的——那里卡片本来就该是一块实底。
            //
            // 但玻璃模式下 `pal.card_bg` 的 alpha 来自「卡片不透明度」
            // （缺省 1.0，完全不透明），于是**被选中的那一行整个丢失玻璃**：
            // 折射、色散、磨砂、边缘光全被一层实色盖死。列表默认总是选中
            // 第一行，用户看到的是「三张玻璃卡里最上面那张是实心板」——
            // 割裂感极强，且不报任何错。
            //
            // 这里改成「只叠色 + 只描边」：底色由玻璃 pass 合成，选中态
            // 只额外增加一层 accent 半透明（预乘缩放，不引入第二次覆盖）。
            painter.rect_filled(r, radius, fade_color(pal.row_selected, sel));
            painter.rect_stroke(
                r,
                radius,
                Stroke::new(2.0 * sel, pal.accent),
                egui::StrokeKind::Inside,
            );
        } else {
            fill_rounded_border(&painter, r, radius, 2.0 * sel, pal.accent, |p, inner, inner_r| {
                p.rect_filled(inner, inner_r, pal.card_bg);
                p.rect_filled(inner, inner_r, fade_color(pal.row_selected, sel));
            });
        }
    }
    // 悬停底色**补间**（110ms OutCubic）：
    // 目标值由本帧是否悬停给出，补间从当前显示值续接——鼠标划过时
    // 背景平滑淡入淡出，不再是硬切（「界面显得廉价」的主要来源之一）。
    //
    // id 带 `item.id`：每行一个独立补间，滚动时各行互不影响。
    // 滚出可视区的行停止绘制，其补间条目留在 ctx.data 里。本函数每行
    // 最多持有 4 条（悬停 / 按下 / 选中 + 三个操作按钮各自的悬停），
    // 上界 ≈ 条目数 × 4 × 50 字节（默认上限 1 万条 ⇒ ~2MB），可接受；
    // 需要更紧的宿主可在列表刷新时按 id 调 `anim::clear`。
    let hover = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::unique(("row_hover", item.id)),
        if resp.hovered() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(110),
        crate::anim::Ease::OutCubic,
    );
    // 阈值跳过：淡出完成后再画一层 alpha≈0 的矩形是纯浪费。
    if hover > 0.004 && !editing_this {
        // CAUTION: 必须整体缩放预乘分量（`fade_color`）——`to_array()`
        // 拿到的就是预乘后的 rgb，若用 `from_rgba_unmultiplied` 重组
        // 会二次预乘，把 11.8% 的悬停提亮压暗成 ~1.6%（几乎看不见）。
        painter.rect_filled(r, radius, fade_color(pal.row_hover, hover));
    }
    // 按下反馈（90ms OutCubic）：指针按住条目时叠一层 `row_active`
    // （白 20%），松手淡出。桌面列表的标准按压反馈；与悬停叠加表达
    // 「按住了」，也让点击到选中的衔接有 intermediate 状态。
    let press = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::unique(("row_press", item.id)),
        if resp.is_pointer_button_down_on() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(90),
        crate::anim::Ease::OutCubic,
    );
    if press > 0.004 && !editing_this {
        painter.rect_filled(r, radius, fade_color(pal.row_active, press));
    }

    // ---- 顶部受光高光（在卡片填充之上）----
    //
    // 深色主题下卡片顶部 3pt 白色渐变亮边（alpha 0.20 → 0），模拟
    // 上方光源——深色卡片在近黑列表底上靠它「浮」起来（立体阴影的
    // 另一半；近投影在 `draw_card_shadow` 里画）。浅色主题高光透明。
    let hl_top = pal.card_highlight;
    let [hl_r, hl_g, hl_b, _] = hl_top.to_array();
    if hl_top.a() > 0 && !editing_this {
        // 高光只覆盖直边段（左右各让开 ROW_RADIUS），避免在卡片圆角外
        // 露出直角亮角；gradient_rect 是直角渐变，圆角处自然留空。
        //
        // CAUTION: egui 的 Color32 是 **premultiplied** 存储，`to_array()`
        // 返回的就是预乘分量——渐变端点必须直接用预乘值（透明端保持
        // 同 RGB、alpha=0），再用 `from_rgba_unmultiplied` 包一遍会
        // 二次预乘，把高光压暗成灰。
        let hl = Rect::from_min_max(
            pos2(r.min.x + radius, r.min.y),
            pos2(r.max.x - radius, r.min.y + 3.0),
        );
        if hl.width() > 1.0 {
            painter.add(egui::Shape::gradient_rect(
                hl,
                egui::Direction::TopDown,
                [
                    hl_top,
                    egui::Color32::from_rgba_premultiplied(hl_r, hl_g, hl_b, 0),
                ],
            ));
        }
    }

    // ---- 三段布局 ----------------------------------------------------
    //
    // 左上：来源程序 · 时间（小字弱化）
    // 中间：预览（主字号，可折行）
    // 右下：类型徽标 + 类型 · 大小（圆角小方块图标 + 文字，右对齐）
    // 右上：置顶 / 复制 / 删除 三个小图标按钮
    //
    // NOTE: 类型徽标放在**右下角**与「类型 · 大小」文字组合成一组
    // （2026-10-15 用户定稿）：不再占用左侧文本宽度，预览/测量两侧
    // 都不需要为它让位。
    //
    // NOTE: 目标是「**完整显示内容**」：预览**不再截断**，而是按宽度
    // 折行。折行数由 `wrap` 直接给出，不用手工估算——
    // 早前版本按 `preview.chars().take(30)` 硬砍，中文一个字算一个
    // char、英文一个词算一个 char，两者的实际显示宽度差了好几倍，
    // 于是「看起来还有空间」但已经被砍掉了。
    let pad_v = 5.0;
    let acts_w = acts_width(pal);
    let text_x0 = r.min.x + ROW_PAD_H;
    // 顶部 meta 行右侧要让开按钮列；预览与底部 meta 位于按钮行**下方**，
    // 用整行宽度（见 `preview_full_width`）。
    let text_x1 = text_x0 + preview_width(r.width(), pal);
    let full_x1 = text_x0 + preview_full_width(r.width());
    if full_x1 - text_x0 < 20.0 {
        // IMPORTANT: 太窄画不下文字，但**行仍必须可点**。
        //
        // 早前这里是裸`return`，直接跳到函数末尾的
        // `resp.clicked()` / `resp.double_clicked()` 之前——
        // 而 `resp` 早已通过 `ui.interact(r, …)` 注册好了。于是
        // 极窄栏时整行**不可点**，却仍然画了 hover 底色与选中条，
        // 视觉上像是能点，一点没反应。
        //
        // 只跳过绘制，不跳过交互。
        if resp.clicked() {
            f.state.push(Op::Select(item.id));
        }
        if resp.double_clicked() {
            // CAUTION: 必须先算后 push——`f.state.push(double_click_op(f, …))`
            // 让 `f.state` 与实参 `&mut f` 同时活着一帧，借用检查直接拦。
            let op = double_click_op(f, item);
            f.state.push(op);
        }
        return;
    }

    let meta_font = sized(pal.font_xs, scale);
    let meta_h = meta_font.size.max(10.0);
    // 左上角：来源 · 时间（小字，左对齐）。右侧要给按钮让位。
    let top_band = Rect::from_min_max(
        pos2(text_x0, r.min.y + pad_v),
        pos2(text_x1, r.min.y + pad_v + meta_h),
    );
    // 右下角：类型 · 大小（小字，右对齐）。它在按钮行下方，可用整行宽。
    let bot_band = Rect::from_min_max(
        pos2(text_x0, r.max.y - pad_v - meta_h),
        pos2(full_x1, r.max.y - pad_v),
    );
    // 中间：预览。起点在「顶部一行（含按钮）」之下，因此可以横跨整行宽度，
    // 不会被按钮遮挡——这正是「内容能显示到右侧」的关键。
    //
    // 高度与 `row_height_for` 严格对应：行高 = 顶部一行 + 预览 + 底部 meta，
    // 这里同样扣掉底部 meta 与 2pt 行间，保证两次排版结果一致。
    let preview_top = r.min.y + pad_v + row_top_height(pal) + 2.0;
    let preview_band = Rect::from_min_max(
        pos2(text_x0, preview_top),
        pos2(full_x1, bot_band.min.y - 2.0),
    );

    // 内容形态三分支（第三轮）：图片内嵌 / Markdown 渲染（或源码）/ 文本。
    // CAUTION: 判据用的开关要从 config 现取，且**不能**持有 `&cfg` 跨越
    // 下面的 `&mut f` 调用——先拷成两个 bool。
    let image_inline = f.svc.state.config.ui.image_inline;
    let md_rendered = f.svc.state.config.ui.md_rendered;
    // 「该不该内嵌成图片」用 [`preview_is_image`]：除了 `ClipKind::Image`，
    // 还包括从资源管理器复制的**单个图片文件**（载荷只有路径，图在磁盘上）。
    let is_image = preview_is_image(item);
    let is_markdown = item.kind == modular_clipboard_core::ClipKind::Markdown;

    // 编辑态优先于一切预览形态：这条正在被改，预览没有意义。
    //
    // CAUTION: 判据必须是 `editing == Some(本行 id)`——`editing` 是
    // **全局单编辑器**（同一时刻只有一条在改），别的行照常画预览。
    if preview_band.height() > 4.0 {
        if editing_this {
            draw_inline_editor(f, ui, preview_band, item);
        } else if is_image && image_inline {
            draw_image_preview(f, ui, preview_band, item, IMG_MAX_H_LIST);
        } else if is_markdown && md_rendered {
            draw_markdown_preview(f, ui, preview_band, item);
        } else if is_markdown {
            draw_source_preview(f, ui, preview_band, item);
        } else {
            draw_text_preview(f, ui, preview_band, item, selected);
        }
    }

    // 左上：来源 · 时间（小字）。
    let meta = format!("{} · {}", item.source_app, format_meta(item.created_at));
    let meta_shown = crate::text::elide_text(f.ui, &meta, &meta_font, top_band.width());
    if !meta_shown.is_empty() {
        // NOTE: painter_at(top_band)：来源名可能很长（长 exe 路径），
        // 根 painter 会让它横跨到相邻卡片上。
        ui.painter_at(top_band).text(
            top_band.left_center(),
            Align2::LEFT_CENTER,
            meta_shown,
            meta_font.clone(),
            pal.text_dim,
        );
    }

    // ---- 左下角：编辑 / 撤销（2026-10-21）----
    //
    // 位置选在底部 meta 行的**左端空白**：预览带在它上方、右侧是
    // 右对齐的类型/大小文字——两者都不与它重叠，因此它**不占用显示
    // 区域**（卡片内容一点没少，只是用上了原本空着的一角）。
    //
    // 可编辑判据：文本三兄弟（Text / Html / Markdown）走**内联编辑**
    // （预览带就地换成输入框）；图片走**编辑窗口**（简单图像变换）；
    // 文件类不画按钮——它的载荷是路径清单，「改内容」没有意义。
    //
    // 撤销按钮只在这条**被编辑过**时出现（`edit_backup.id == item.id`），
    // 画在编辑按钮左边——「先撤销再编辑」的阅读顺序。
    let editable = matches!(
        item.kind,
        modular_clipboard_core::ClipKind::Text
            | modular_clipboard_core::ClipKind::Html
            | modular_clipboard_core::ClipKind::Markdown
            | modular_clipboard_core::ClipKind::Image
    );
    let has_backup = f
        .state
        .edit_backup
        .as_ref()
        .is_some_and(|b| b.id == item.id);
    // 与右下角类型徽标同尺寸（16pt）：左下角这组按钮在视觉上才是一套。
    let badge_side = pal.icon_size;
    // 按钮块总宽（供下面的 meta 文字让位，防止窄卡片上文字压按钮）。
    let edit_btns_w = if editable && bot_band.width() > 64.0 {
        let mut w = badge_side + 5.0;
        if has_backup {
            w += badge_side + 5.0;
        }
        w
    } else {
        0.0
    };

    // 画按钮块（在 meta 之前画：meta 右对齐，两者不重叠，顺序无所谓；
    // 但按钮的交互注册要早于行本体的 `resp.clicked()` 判定）。
    if edit_btns_w > 0.0 {
        let bcy = bot_band.center().y;
        let mut bx = bot_band.min.x;
        if has_backup {
            let b = Rect::from_center_size(pos2(bx + badge_side * 0.5, bcy), vec2(badge_side, badge_side));
            // 自报热区（与其它命名区域同一套路）：测试与 MCP 自动化读坐标
            // 而不重算公式。CAUTION: 多行都会登记同一个 key，HashMap 后者
            // 覆盖前者——单条目夹具里无歧义，多条目场景请用显隐判据。
            f.hit_insert("row_undo", b);
            let rr = ui.interact(b, egui::Id::unique(("row_undo", item.id)), Sense::click());
            let bh = crate::anim::tween_f32(
                ui.ctx(),
                egui::Id::unique(("row_undo_hover", item.id)),
                if rr.hovered() { 1.0 } else { 0.0 },
                std::time::Duration::from_millis(110),
                crate::anim::Ease::OutCubic,
            );
            if bh > 0.004 {
                ui.painter_at(b)
                    .rect_filled(b, pal.radius_sm, fade_color(pal.row_hover, bh));
            }
            let color = if rr.hovered() { pal.text_bright } else { pal.text_dim };
            paint_undo_icon(&ui.painter_at(b), b, color);
            if rr.clicked() {
                f.state.push(Op::UndoEdit(item.id));
            }
            rr.on_hover_text(t("Undo edit"));
            bx += badge_side + 5.0;
        }
        let b = Rect::from_center_size(pos2(bx + badge_side * 0.5, bcy), vec2(badge_side, badge_side));
        f.hit_insert("row_edit", b);
        let rr = ui.interact(b, egui::Id::unique(("row_edit", item.id)), Sense::click());
        let bh = crate::anim::tween_f32(
            ui.ctx(),
            egui::Id::unique(("row_edit_hover", item.id)),
            if rr.hovered() { 1.0 } else { 0.0 },
            std::time::Duration::from_millis(110),
            crate::anim::Ease::OutCubic,
        );
        if bh > 0.004 {
            ui.painter_at(b)
                .rect_filled(b, pal.radius_sm, fade_color(pal.row_hover, bh));
        }
        let color = if rr.hovered() { pal.text_bright } else { pal.text_dim };
        paint_pencil_icon(&ui.painter_at(b), b, color);
        // CAUTION: `clicked()` 必须在 `on_hover_text` 之前取（后者按值
        // 消耗 `Response`，与行内三个操作按钮同一个坑）。
        let clicked = rr.clicked();
        rr.on_hover_text(if item.kind == modular_clipboard_core::ClipKind::Image {
            t("Edit image")
        } else {
            t("Edit content")
        });
        if clicked {
            if item.kind == modular_clipboard_core::ClipKind::Image {
                f.state.push(Op::OpenImageEditor(item.id));
            } else {
                f.state.push(Op::BeginEdit(item.id));
            }
        }
    }

    // 右下：可配置的信息组合（类型徽标 / 类型文字 / 大小文字）。
    //
    // 三个元素的**显隐与前后顺序**由 [`RowMetaConfig`] 决定（设置中
    // 可单击切换、按住拖拽重排）。元素之间只留固定间距，不再用「·」
    // 分隔。整组右对齐 bot_band 右缘。
    //
    // 大小来自 `PayloadRef::len`（字节数），`format_size` 自适应
    // B/KB/MB/GB；载荷被淘汰（`payload = None`）时大小元素自动缺席。
    // 宽度不足时优先保住短的「大小」，「类型」elide 截断。
    //
    // CAUTION: 左缘要让出左下角编辑/撤销按钮块（`edit_btns_w`）——
    // 右对齐的文字在窄卡片上会一路伸到左边，不让位就会压住按钮
    // （按钮变透明文字区，点不动）。
    let meta_cfg = &f.svc.state.config.ui.row_meta;
    let type_text = kind_label(item.kind).to_string();
    let size_text = item.payload.as_ref().map(|p| format_size(p.len));
    let badge_radius = 4.0;
    let meta_gap = 6.0;
    let bot_band = Rect::from_min_max(
        pos2(bot_band.min.x + edit_btns_w, bot_band.min.y),
        bot_band.max,
    );

    let icon_on = meta_cfg.visible(RowMetaPart::Icon);
    let type_on = meta_cfg.visible(RowMetaPart::Type) && !type_text.is_empty();
    let size_on = meta_cfg.visible(RowMetaPart::Size) && size_text.is_some();
    let n = icon_on as u32 + type_on as u32 + size_on as u32;
    // 编辑模式下整组隐去：编辑时这些元信息没有意义（内容正在被改），
    // 且右端要为编辑器让出空间（2026-10-22 用户要求）。
    if n > 0 && !editing_this {
        // 固定占位（徽标宽 + 元素间 gap）；余下全部给文字。
        let fixed = if icon_on { badge_side } else { 0.0 }
            + if n > 1 { meta_gap * (n - 1) as f32 } else { 0.0 };
        let text_budget = (bot_band.width() - fixed).max(0.0);
        let size_full = size_text.unwrap_or_default();
        let size_w = if size_on {
            crate::text::measure_text(f.ui, &size_full, &meta_font)
        } else {
            0.0
        };
        // 类型与大小同处一行时，gap 已计入 fixed，这里只扣 size_w。
        let type_budget = if type_on && size_on {
            (text_budget - size_w).max(0.0)
        } else {
            text_budget
        };
        let type_shown = if type_on {
            crate::text::elide_text(f.ui, &type_text, &meta_font, type_budget)
        } else {
            String::new()
        };
        let type_w = crate::text::measure_text(f.ui, &type_shown, &meta_font);

        let cy = bot_band.center().y;
        let mut rx = bot_band.right_center().x;
        // 文字用 bot_band 的 painter（受文字带裁剪）；徽标比文字带
        // 高（16pt vs ~12pt），画在行 painter 上不被裁掉。
        let bot_painter = ui.painter_at(bot_band);
        for part in meta_cfg.order.iter().rev() {
            match part {
                RowMetaPart::Icon if icon_on => {
                    let br = Rect::from_min_max(
                        pos2(rx - badge_side, cy - badge_side * 0.5),
                        pos2(rx, cy + badge_side * 0.5),
                    );
                    painter.rect_filled(br, badge_radius, pal.surface_variant);
                    // Markdown 无字体字形，走几何绘制（见 paint_kind_icon）。
                    crate::icons::paint_kind_icon(&painter, item.kind, br, pal.text_dim);
                    rx -= badge_side + meta_gap;
                }
                RowMetaPart::Size if size_on => {
                    bot_painter.text(
                        pos2(rx, cy),
                        Align2::RIGHT_CENTER,
                        size_full.clone(),
                        meta_font.clone(),
                        pal.text_dim,
                    );
                    rx -= size_w + meta_gap;
                }
                RowMetaPart::Type if type_on && !type_shown.is_empty() => {
                    bot_painter.text(
                        pos2(rx, cy),
                        Align2::RIGHT_CENTER,
                        type_shown.clone(),
                        meta_font.clone(),
                        pal.text_dim,
                    );
                    rx -= type_w + meta_gap;
                }
                _ => {}
            }
        }
    }

    // 右侧三个操作：置顶 / 复制 / 删除。
    //
    // IMPORTANT: **必须用图标字体**（[`crate::icons::Icon`]，PUA 码位
    // `U+E000..=U+E011`），不能用 Unicode 字形。
    //
    // 实测：早前这里写的是 `"⧉"` / `"✕"` / `"★"`，其中 `⧉`(U+29C9)
    // 与 `✕`(U+2715) 在当前字体链（微软雅黑 + 图标字体）里**没有
    // 字形**，于是渲染成**空心方框**（tofu）。`★`/`☆` 恰好在雅黑里
    // 有字形，所以截图里只有「复制/删除」变方框、置顶正常——
    // 这种「一部分符号缺字形」的现象最容易误判成字体没加载。
    //
    // 代价：图标字体只有 18 个码位，「复制」没有现成图标，
    // 只能用 `File`（文档）近似表达「复制到剪贴板」。
    let btn = pal.control_height * 0.55;
    let acts: [(crate::icons::Icon, Op, bool); 3] = [
        (
            if item.pinned {
                crate::icons::Icon::Pinned
            } else {
                crate::icons::Icon::Unpinned
            },
            Op::TogglePin(item.id),
            // IMPORTANT: 必须始终可点：置顶条目正是靠这个按钮取消置顶。
            // 早前写成 `!item.pinned` —— 条目置顶后按钮变灰、点击不触发，
            // 于是「置顶后无法取消置顶」。图标已区分 Pinned/Unpinned 状态，
            // 语义就是置顶 ↔ 取消置顶的开关，任何状态下都可点。
            true,
        ),
        (crate::icons::Icon::Copy, Op::Copy(item.id), true),
        (crate::icons::Icon::Close, Op::Delete(item.id), true),
    ];

    for (i, (icon, op, enabled)) in acts.iter().enumerate() {
        // 编辑模式下让位：这三个按钮（置顶/复制/删除）在编辑时会与
        // 编辑器抢右上角空间，且「改到一半去删除」是误操作温床
        // （2026-10-22 用户要求）。
        if editing_this {
            break;
        }
        // 三个操作按钮排**卡片右上角**（小图标）：从右缘向左排，
        // 纵向比来源行再下移 `ROW_ACTS_DROP`（避开字形上沿）。
        //
        // IMPORTANT: 基准右缘必须让出 `SCROLLBAR_RESERVE` **和**
        // `ROW_PAD_H`。
        //
        // egui 0.36 默认 `ScrollStyle` 是 **floating** —— 滚动条
        // **不占布局宽度**，而是浮在 `content` 右缘覆盖
        // `[content.max.x - 10, content.max.x]`。而 `r.max.x` 恰好等于
        // `content.max.x`，所以按 `r.max.x` 排的最后一个按钮（删除）
        // 中心会落在滚动条里。
        //
        // 滚动条在内容闭包**之后**才注册交互（`scroll_area.rs` 里用
        // `Sense::CLICK | Sense::DRAG` 注册 `max_bar_rect`），而 egui 的
        // hit-test 规则是「并列时取最后注册的 = 最上层」⇒ **滚动条压过
        // 行内所有控件**。
        //
        // 现象（极难自查）：条目少、列表不满一屏时**没有滚动条**，一切
        // 正常；条目一多（有滚动条）删除按钮就点不动——而这是常态。
        // 「条目少就好、条目多就坏」是这条 bug 的指纹。
        let acts_right = r.max.x - ROW_PAD_H;
        let b = Rect::from_center_size(
            pos2(
                acts_right - acts_w + (i as f32 + 0.5) * (btn + pal.space_xs),
                r.min.y + pad_v + ROW_ACTS_DROP + btn * 0.5,
            ),
            vec2(btn, btn),
        );
        if b.min.x < text_x1 {
            break; // 卡片太窄，放不下就不画操作
        }
        let rr = ui.interact(b, egui::Id::unique(("row_act", item.id, i)), Sense::click());
        // 悬停圆角底（110ms OutCubic 补间）：小图标按钮只有「图标变色」
        // 时反馈太弱，加一层 `row_hover` 底让按钮有可点的实体感。底色先
        // 画、图标后画，顺序不能反。id 带条目与槽位，三个按钮各自独立。
        let bh = crate::anim::tween_f32(
            ui.ctx(),
            egui::Id::unique(("row_act_hover", item.id, i)),
            if rr.hovered() { 1.0 } else { 0.0 },
            std::time::Duration::from_millis(110),
            crate::anim::Ease::OutCubic,
        );
        if bh > 0.004 {
            ui.painter_at(b)
                .rect_filled(b, pal.radius_sm, fade_color(pal.row_hover, bh));
        }
        let color = if !*enabled {
            pal.text_dim
        } else if i == 2 && rr.hovered() {
            // 删除是破坏性操作，悬停用 danger 色与前两个区分。
            pal.danger
        } else if rr.hovered() {
            pal.text_bright
        } else {
            pal.text_dim
        };
        icon.paint(
            &ui.painter_at(b),
            Rect::from_center_size(b.center(), vec2(btn, btn) * 0.78),
            color,
        );
        // 悬停说明：图标本身太小，必须给文字提示。
        let tip = match i {
            0 => {
                if item.pinned {
                    t("Unpin")
                } else {
                    t("Pin")
                }
            }
            1 => t("Copy"),
            _ => t("Delete"),
        };
        if rr.clicked() && *enabled {
            f.state.push(op.clone());
        }
        // `on_hover_text` 按值消耗 `Response`，必须是最后一次使用。
        rr.on_hover_text(tip);
    }

    if resp.clicked() {
        // 正在编辑**别的**行时点这一行 ⇒ 先提交正在编辑的内容，再选中
        // 本行（「改完点下一条」的连续编辑流；不提交的话缓冲会跟着
        // `editing` 一起被下一行覆盖，用户改的字凭空蒸发）。
        //
        // CAUTION: 只能 commit **当前 buffer**——此时 draw 还没走到
        // 目标行，`editing` 仍是旧 id；这里 push 的 Op 带旧 id 与旧
        // buffer，落地由 apply_ops 一并处理（先 commit 后 select）。
        if let Some(editing_id) = f.state.editing
            && editing_id != item.id
        {
            f.state.push(Op::CommitEditText {
                id: editing_id,
                text: f.state.edit_buf.clone(),
            });
        }
        f.state.push(Op::Select(item.id));
    }
    if resp.double_clicked() {
        // 同窄行分支：先算后 push（避免 `f.state` 与 `&mut f` 同帧共存）。
        let op = double_click_op(f, item);
        f.state.push(op);
    }
}

// ---------------------------------------------------------------- 内联编辑

/// 内联编辑器：预览带**就地**换成多行输入框 + 确认 / 取消（2026-10-21）。
///
/// # 交互契约
///
/// - ✓ 或 `Ctrl+Enter` 提交；✕ 或 `Esc` 取消；
/// - **Enter 不提交**——多行文本里 Enter 是换行（剪贴板内容天然
///   多行，与文件管理器的单行重命名不同）；
/// - 点**别的行**自动提交（由 `draw_item_row` 的行点击分支负责）——
///   「改完点下一条」是最自然的连续编辑流。
///
/// # CAUTION: 字号必须用**逻辑值**
///
/// `FontId::proportional(pal.font_md)` 不乘 `f.scale`：egui 计算可用
/// 高度时会再乘一次 `pixels_per_point`，传乘过的值会把行高放大到
/// 远超框高、文字被裁成半截（搜索面板 2026-10-17 踩过，见
/// `draw_search_panel` 的注释）。
pub(super) fn draw_inline_editor(f: &mut Frame<'_>, ui: &egui::Ui, band: Rect, item: &ClipItem) {
    let pal = f.pal;
    // 两枚按钮（✓ / ✕）横排竖居中在带右端；文字区让出它们的宽。
    let btn = (pal.control_height * 0.62).clamp(16.0, 24.0);
    let gap = 4.0;
    let text_rect =
        Rect::from_min_max(band.min, pos2((band.max.x - btn * 2.0 - gap).max(band.min.x), band.max.y));
    if text_rect.width() <= 8.0 {
        // 窄到放不下输入框：仍要能退出编辑态，只画按钮。
        draw_editor_buttons(f, ui, band, item, btn, gap);
        return;
    }
    // 输入框底：实底 + 1px 弱描边——「这里是可编辑的」要有实体感。
    // 磨砂模式下用 surface_variant（与列表底拉开一档），常态模式同色
    // 也成立（它在 surface_raised 的卡片上，稍暗一档）。
    ui.painter_at(text_rect)
        .rect_filled(text_rect, pal.radius_sm, pal.surface_variant);
    ui.painter_at(text_rect).rect_stroke(
        text_rect,
        pal.radius_sm,
        Stroke::new(1.0, pal.border_subtle),
        egui::StrokeKind::Inside,
    );
    f.hit_insert("edit_box", text_rect);
    let mut resp_opt = None;
    f.ui.scope_builder(
        egui::UiBuilder::new()
            .scope_id(egui::Id::unique(("inline_editor_scope", item.id)))
            .max_rect(text_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| {
            let te = egui::TextEdit::multiline(&mut f.state.edit_buf)
                .id(egui::Id::unique(("inline_editor", item.id)))
                .hint_text(t("Type content…"))
                .font(egui::FontId::proportional(pal.font_md))
                .desired_width(text_rect.width())
                .frame(egui::Frame::NONE);
            resp_opt = Some(ui.add(te));
        },
    );
    let Some(resp) = resp_opt else { return };
    // 打开后的第一帧把焦点交给输入框（要跨帧：控件刚注册，本帧请求
    // 下一帧才生效——与搜索弹窗的 `search_focus_pending` 同一套路）。
    if f.state.edit_focus_pending {
        resp.request_focus();
        f.state.edit_focus_pending = false;
    }
    // Ctrl+Enter 提交。判据带 `has_focus()`：没焦点时组合键不属于编辑
    // 器，不该被它吞掉。
    let ctrl_enter = resp.has_focus()
        && ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.command);
    draw_editor_buttons(f, ui, band, item, btn, gap);
    if ctrl_enter {
        f.state.push(Op::CommitEditText {
            id: item.id,
            text: f.state.edit_buf.clone(),
        });
    }
}

/// 编辑器的 ✓ / ✕ 两枚按钮（横排、带右端、竖居中）。
pub(super) fn draw_editor_buttons(f: &mut Frame<'_>, ui: &egui::Ui, band: Rect, item: &ClipItem, btn: f32, gap: f32) {
    let pal = f.pal;
    let cy = band.center().y;
    let x1 = band.max.x - btn * 0.5;
    let x0 = x1 - btn - gap;
    let ok_r = Rect::from_center_size(pos2(x0, cy), vec2(btn, btn));
    let no_r = Rect::from_center_size(pos2(x1, cy), vec2(btn, btn));

    // ✓ 确认。CAUTION: `clicked()` 必须在 `on_hover_text` 之前取
    // （后者按值消耗 `Response`）。
    f.hit_insert("editor_ok", ok_r);
    f.hit_insert("editor_no", no_r);
    let rr = ui.interact(ok_r, egui::Id::unique(("editor_ok", item.id)), Sense::click());
    let bh = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::unique(("editor_ok_hover", item.id)),
        if rr.hovered() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(110),
        crate::anim::Ease::OutCubic,
    );
    if bh > 0.004 {
        ui.painter_at(ok_r)
            .rect_filled(ok_r, pal.radius_sm, fade_color(pal.row_hover, bh));
    }
    let color = if rr.hovered() { pal.accent } else { pal.text_dim };
    paint_check_icon(
        &ui.painter_at(ok_r),
        Rect::from_center_size(ok_r.center(), vec2(btn * 0.72, btn * 0.72)),
        color,
    );
    let ok_clicked = rr.clicked();
    rr.on_hover_text(t("Save (Ctrl+Enter)"));
    if ok_clicked {
        f.state.push(Op::CommitEditText {
            id: item.id,
            text: f.state.edit_buf.clone(),
        });
    }

    // ✕ 取消。图标用 Close 字体字形（PUA 码位里有现成的，不必几何画）。
    let rr = ui.interact(no_r, egui::Id::unique(("editor_no", item.id)), Sense::click());
    let bh = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::unique(("editor_no_hover", item.id)),
        if rr.hovered() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(110),
        crate::anim::Ease::OutCubic,
    );
    if bh > 0.004 {
        ui.painter_at(no_r)
            .rect_filled(no_r, pal.radius_sm, fade_color(pal.row_hover, bh));
    }
    let color = if rr.hovered() { pal.text_bright } else { pal.text_dim };
    crate::icons::Icon::Close.paint(
        &ui.painter_at(no_r),
        Rect::from_center_size(no_r.center(), vec2(btn * 0.62, btn * 0.62)),
        color,
    );
    let no_clicked = rr.clicked();
    rr.on_hover_text(t("Cancel (Esc)"));
    if no_clicked {
        f.state.push(Op::CancelEdit);
    }
}

// ---------------------------------------------------------------- 预览带

/// 文本预览（默认形态）：按预览带宽度折行，**完整显示**。
///
/// 选中态把文字提亮一档（与 `sel_fx` 的描边/底色叠加表达选中）。
/// # CAUTION: 必须用**调用方给的那个 `Ui`**（列表传滚动区的内容 Ui）
///
/// 早前这里用的是 `f.ui`（根 Ui），它的裁剪矩形是**整个窗口** ⇒ 列表滚动时
/// 预览文字会画到列表之外：越过卡片上缘、直接盖在顶栏上（图标被文字压住），
/// 而卡片底色由滚动区的 `ui.painter_at` 画、被正确裁剪 —— 于是「只剩文字
/// 浮在顶栏上，没有遮蔽」（2026-10-19 用户反馈）。
///
/// `Ui::painter_at` 的裁剪 = 自身矩形 ∩ **本 Ui 的裁剪矩形**，所以只要
/// 传入滚动区的内容 Ui，越界的行自动被裁掉，与「看得见才画得出」一致。
pub(super) fn draw_text_preview(
    f: &mut Frame<'_>,
    ui: &egui::Ui,
    band: Rect,
    item: &ClipItem,
    selected: bool,
) {
    let pal = f.pal;
    // 用 `Painter::layout_job` 按宽度折行，返回**完整** Galley——
    // 这才是「完整显示」：不丢任何字符，只是折行。
    //
    // CAUTION: 不能用 `Ui::layout`/`Context::layout`：那是给「让出矩形」
    // 的控件做换行用的，会受子 `Ui` 的布局约束影响，而我们只要
    // 一个纯文本排版结果。`Painter::layout_job` 才是纯排版入口。
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &item.preview,
        0.0,
        egui::TextFormat {
            font_id: sized(pal.font_md, f.scale),
            color: if selected {
                pal.text_bright
            } else {
                pal.text
            },
            ..Default::default()
        },
    );
    job.wrap.max_width = band.width();
    // 高度用 `max_rows` 表达（0.36 的 `TextWrapping` **没有**
    // `max_height` 字段，只有 `max_width` / `max_rows` /
    // `break_anywhere` / `overflow_character`）。
    //
    // 行数按可用高度与字号算出，至少 1 行。超出部分由 egui 省略并
    // 在末行加省略号——**完整内容在详情子窗口里能看到**。
    // 行数上限：`row_height_for` 已按同一份预览文本算出行数并据此
    // 定好行高，这里给同样的上限只是兜底（防止两次排版结果不一致）。
    //
    // CAUTION: **不能设 0**：egui 文档写明「If set to 0, no text will be
    // outputted」——0 表示一行都不输出。默认值 `usize::MAX` 才是「不限」。
    job.wrap.max_rows = ((band.height() / (pal.font_md * LINE_H)).floor().max(1.0)) as usize;
    // 长路径/无空格的长串要在任意字符间断行，否则会横向溢出。
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    ui.painter_at(band)
        .galley(pos2(band.min.x, band.min.y), galley, pal.text);
}

/// Markdown 源码预览（`md_rendered` 关闭时）：等宽 + 弱化色。
///
/// 与渲染态明确区分——用户能一眼看出「现在看到的是原始 Markdown」。
pub(super) fn draw_source_preview(f: &mut Frame<'_>, ui: &egui::Ui, band: Rect, item: &ClipItem) {
    let pal = f.pal;
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &item.preview,
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::monospace(pal.font_sm * f.scale),
            color: pal.text_dim,
            ..Default::default()
        },
    );
    job.wrap.max_width = band.width();
    job.wrap.max_rows = ((band.height() / (pal.font_sm * 1.45)).floor().max(1.0)) as usize;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    ui.painter_at(band)
        .galley(pos2(band.min.x, band.min.y), galley, pal.text_dim);
}

/// Markdown 渲染预览（第三轮新增）。
///
/// 行高由 `row_height_for` 调**同一个** `md::MdDoc` 的 `height()` 算出，
/// 绘制调 `draw()`——同一函数、同一宽度，测量与绘制不可能漂移。
/// 选中态不提亮文字（`sel_fx` 覆盖层已足够表达），与原型一致。
pub(super) fn draw_markdown_preview(f: &mut Frame<'_>, ui: &egui::Ui, band: Rect, item: &ClipItem) {
    let doc = crate::md::MdDoc::parse(&item.preview);
    if doc.is_empty() {
        return;
    }
    let pal = f.pal;
    doc.draw(ui, pal, f.scale, band.min, band.width());
}

/// 图片预览（第三轮新增）：内容区直接显示缩略图。
///
/// 管线：`thumb_tex` 命中 → 直接用；未命中 → `thumb_cache`
/// （LRU 像素缓存，失败有短时抑制）解码 + `load_texture` 上传。
/// 解码在**缓存未命中**时才发生：载荷读取与解码都不进每帧热路径
/// （PF-1 的同款理由——列表每帧重排，不能每帧读盘）。
///
/// 任何一步失败（载荷被淘汰 / 不是图片 / 解码失败）都**回落文本预览**：
/// 剪贴板工具的列表不能因为一张坏图就少画一行。
///
/// `max_h` 是缩略图显示高的**上限**——列表行传 [`IMG_MAX_H_LIST`] 防止
/// 单条记录撑爆，详情面板传 `[body.width(), body.height()]` 让图按
/// 长宽比尽量放大。
pub(super) fn draw_image_preview(f: &mut Frame<'_>, ui: &egui::Ui, band: Rect, item: &ClipItem, max_h: f32) {
    // 纹理先取：解码产物自带尺寸，取不到就说明这张图不该内嵌显示
    // （载荷被淘汰 / 文件已删 / 解码失败）⇒ 降级文本。
    //
    // 顺序为什么是「先纹理后尺寸」：文件型图片（从资源管理器复制的
    // `.png`）**没有** `PayloadRef::dims`——载荷里只有路径列表，尺寸
    // 只能靠解码拿到。统一走解码结果，两类条目共用同一条排版路径。
    let Some(tex) = preview_image_tex(f, item) else {
        draw_text_preview(f, ui, band, item, false);
        return;
    };
    let Some((w, h)) = preview_image_dims(f, item) else {
        // 没有任何维度信息：按普通文本显示（「图片 WxH」/「图片 N 字节」/
        // 文件名），剪贴板工具的列表不能因为一张坏图就少画一行。
        draw_text_preview(f, ui, band, item, false);
        return;
    };
    // 显示矩形：与 [`row_height_for`] 共用 [`image_display_size`]
    // （行高就是按它量出来的，这里再写一份算式必然漂移）。
    //
    // `max_h` 与 `band.height()` 取小：调用方给的限高只是「默认」，
    // 带子本身更矮时（列表行按内容算出的行高）以带子为准。
    let (disp_w, disp_h) = image_display_size((w, h), band.width(), max_h.min(band.height()));
    if disp_w <= 0.0 || disp_h <= 0.0 {
        draw_text_preview(f, ui, band, item, false);
        return;
    }
    let rect = Rect::from_min_size(band.min, vec2(disp_w, disp_h));

    let painter = ui.painter_at(rect);
    // uv 给整张图（0..1），tint 白色（不染色——缩略图颜色即原图）。
    painter.image(
        tex,
        rect,
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
    // 1px 弱描边：浅色图片（白底截图）与深色列表背景之间需要一道边界。
    painter.rect_stroke(
        rect,
        6.0,
        Stroke::new(1.0, f.pal.border_subtle),
        egui::StrokeKind::Inside,
    );
}

/// 文件型图片**读盘**的大小上限（字节）。
///
/// 读盘与解码都发生在 UI 线程上：一次几十 MB 的读 + 解码会明显卡一帧
/// （主循环空转 ~125Hz，掉一帧就是 8ms+ 的停顿）。超过这个量就不读，
/// 直接按文本显示——超大图的缩略图价值远低于一次卡顿。
pub(super) const FILE_IMAGE_MAX_BYTES: u64 = 24 * 1024 * 1024;

/// 条目是否应当**内嵌显示成图片**。
///
/// 两类来源：
/// - `ClipKind::Image`：载荷就是像素（现行规范是裸 RGBA8）；
/// - `ClipKind::Files`：**单个**图片文件（从资源管理器复制的 `.png` 等）。
///
/// # 文件型为什么只看 `preview`（零 IO）
///
/// 完整路径只存在于载荷字节里，而列表**每帧**都要跑这条判定 ——
/// 每帧读载荷正是 PF-1 禁止的。好在单文件条目 `preview` 就是文件名
/// （`build_preview` 的 `names.len() == 1` 分支），按扩展名判定即可。
///
/// # 多文件为什么不画缩略图
///
/// 多文件条目 `preview` 是摘要（`a.png 等 5 个文件`），拿不到首行路径
/// 除非读载荷；而且「显示第一张图」与「这一行代表 5 个文件」语义冲突
/// ——用户会以为缩略图就是全部内容。保持文本列表。
pub fn preview_is_image(item: &ClipItem) -> bool {
    match item.kind {
        modular_clipboard_core::ClipKind::Image => true,
        modular_clipboard_core::ClipKind::Files => {
            crate::thumbnail::is_image_file(&item.preview)
        }
        _ => false,
    }
}

/// 取条目图片的字节：图片型读载荷，文件型按首行路径读盘。
///
/// 只在缓存未命中时被调用（见 [`preview_image_thumb`]），不是每帧 IO。
pub(super) fn load_image_bytes(svc: &mut Service, item: &ClipItem) -> anyhow::Result<Vec<u8>> {
    if item.kind != modular_clipboard_core::ClipKind::Files {
        return svc.load_payload_bytes(item.id);
    }
    // 载荷是换行拼接的路径列表，取第一行——与双击跳转
    // （`double_click_op` → `Op::RevealPath`）同一条规则：看到的缩略图
    // 和双击打开的是同一个文件。
    let bytes = svc.load_payload_bytes(item.id)?;
    let path = String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("文件条目载荷里没有路径"))?;
    // 大小先查 metadata（比读全文件廉价得多）：超限直接放弃。
    let len = std::fs::metadata(&path)
        .map_err(|e| anyhow::anyhow!("读取图片文件信息失败 {path}: {e}"))?
        .len();
    if len > FILE_IMAGE_MAX_BYTES {
        anyhow::bail!("图片文件过大（{len} 字节 > {FILE_IMAGE_MAX_BYTES}），跳过缩略图");
    }
    std::fs::read(&path).map_err(|e| anyhow::anyhow!("读取图片文件失败 {path}: {e}"))
}

/// 实验性并行预热：把本帧**可见但缓存未命中**的图片缩略图并行解码。
///
/// # 开关
///
/// 环境变量 `MC_PARALLEL_THUMBS=1`（默认关）。关掉时行为与从前逐位
/// 相同，开与不开的**画面也相同**——这里只是把「绘制过程中逐个同步
/// 解码」提前到绘制之前并并行完成，随后 [`preview_image_thumb`] 全部
/// 命中缓存。
///
/// # 为什么字节读取仍然是顺序的
///
/// `load_image_bytes` 走 `Service`（rusqlite 连接 + 文件读），那条连接
/// **不跨线程**（`Service` 不是 `Send`）。真正的热点是解码本身（PNG /
/// JPEG 解码 + 缩放），它是纯函数、无共享状态，正好切出去并行。
///
/// # 为什么至少要 2 张才并行
///
/// 单张时开线程的代价（约几十微秒）与解码同量级，纯粹白烧；而列表
/// 一屏滚进多张图片时，串行解码会把首帧拖出肉眼可见的卡顿。
pub(super) fn prefetch_thumbs(f: &mut Frame<'_>, items: &[&ClipItem]) {
    /// 并行度上限：解码是 CPU 密集，超过核数只会互相抢占。
    const MAX_PARALLEL: usize = 4;

    /// 一条待解码任务：`(缓存键, 载荷尺寸, 原始字节)`。
    type Job = (crate::thumbnail::CacheKey, Option<(u32, u32)>, Vec<u8>);

    // 1) 顺序准备：命中缓存直接跳过；否则读字节（含 DB/文件 IO）。
    let mut jobs: Vec<Job> = Vec::new();
    for item in items {
        let key = crate::thumbnail::CacheKey::new(item.id, item.hash.clone());
        if f.state.thumb_cache.get(&key).is_some() {
            continue;
        }
        let dims = item
            .payload
            .as_ref()
            .and_then(|p| p.dims)
            .filter(|(w, h)| *w > 0 && *h > 0);
        match load_image_bytes(f.svc, item) {
            Ok(bytes) => jobs.push((key, dims, bytes)),
            Err(e) => tracing::debug!(id = item.id, %e, "预热缩略图：字节读取失败"),
        }
    }
    if jobs.len() < 2 {
        return;
    }

    // 2) 并行解码：纯 CPU、无共享状态，结果按下标回填。
    let mut decoded: Vec<Option<std::sync::Arc<crate::thumbnail::Thumbnail>>> =
        vec![None; jobs.len()];
    let started = std::time::Instant::now();
    let chunk = jobs.len().div_ceil(MAX_PARALLEL.min(jobs.len()));
    std::thread::scope(|s| {
        let handles: Vec<_> = jobs
            .chunks(chunk)
            .enumerate()
            .map(|(ci, part)| {
                s.spawn(move || {
                    part.iter()
                        .enumerate()
                        .map(|(i, (_, dims, bytes))| {
                            (
                                ci * chunk + i,
                                crate::thumbnail::decode_auto(
                                    bytes,
                                    *dims,
                                    crate::thumbnail::THUMB_MAX_EDGE,
                                )
                                .ok()
                                .map(std::sync::Arc::new),
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            // 线程 panic 时放弃这一批（缩略图是可选的，不该杀死界面）。
            if let Ok(rows) = h.join() {
                for (i, t) in rows {
                    if let Some(slot) = decoded.get_mut(i) {
                        *slot = t;
                    }
                }
            }
        }
    });

    // 3) 顺序回填缓存（LRU + 字节预算的淘汰在 `insert` 里）。
    let total = jobs.len();
    let mut ok = 0usize;
    for ((key, _, _), thumb) in jobs.into_iter().zip(decoded) {
        if let Some(t) = thumb {
            f.state.thumb_cache.insert(key, t);
            ok += 1;
        }
    }
    tracing::debug!(
        decoded = ok,
        total,
        threads = total.div_ceil(chunk),
        ms = started.elapsed().as_secs_f32() * 1000.0,
        "并行预热缩略图完成"
    );
}

/// 解码并上传缩略图纹理；命中已上传的纹理时直接返回。
///
/// 上传成功后把**尺寸**写进 `state.thumb_dims`：文件型图片没有
/// `PayloadRef::dims`，排版需要的长宽比只能从这里拿。
pub(super) fn preview_image_tex(f: &mut Frame<'_>, item: &ClipItem) -> Option<egui::TextureId> {
    let id = item.id;
    if let Some(t) = f.state.thumb_tex.get(&id).copied() {
        return Some(t);
    }
    let thumb = preview_image_thumb(f, item)?;
    let dims = (thumb.width, thumb.height);
    let img = thumb.to_color_image();
    // 纹理名带条目 id：同一条目重复上传会**替换**而不是累积，
    // egui 侧不会因此长出新纹理。
    // 只存 `TextureId`：纹理由 Context 持有（注册即托管），
    // 句柄（TextureHandle）只是引用，没必要长留。
    let t = f
        .ui
        .ctx()
        .load_texture(
            format!("mc_thumb_{id}"),
            img,
            egui::TextureOptions::LINEAR,
        )
        .id();
    f.state.thumb_tex.insert(id, t);
    f.state.thumb_dims.insert(id, dims);
    Some(t)
}

/// 缩略图的长宽比来源。
///
/// 优先解码得到的真实尺寸（`thumb_dims`），其次载荷里存的 `dims`。
pub(super) fn preview_image_dims(f: &Frame<'_>, item: &ClipItem) -> Option<(u32, u32)> {
    f.state
        .thumb_dims
        .get(&item.id)
        .copied()
        .or_else(|| {
            item.payload
                .as_ref()
                .and_then(|p| p.dims)
                .filter(|(w, h)| *w > 0 && *h > 0)
        })
}

/// 取（必要时解码）条目的缩略图像素。
///
/// CAUTION: `state.thumb_cache` 与 `svc` 是 `Frame` 的不同字段，
/// 先分别借开才能在同一条表达式里用（闭包要读字节、缓存要可写）。
pub(super) fn preview_image_thumb(
    f: &mut Frame<'_>,
    item: &ClipItem,
) -> Option<std::sync::Arc<crate::thumbnail::Thumbnail>> {
    let id = item.id;
    // 载荷里存的尺寸（裸 RGBA8 时是唯一来源）；文件型条目没有。
    let dims = item
        .payload
        .as_ref()
        .and_then(|p| p.dims)
        .filter(|(w, h)| *w > 0 && *h > 0);
    // 第一段：缓存（含失败抑制）。
    let from_cache = {
        let cache = &mut f.state.thumb_cache;
        let svc = &mut f.svc;
        let key = crate::thumbnail::CacheKey::new(id, item.hash.clone());
        let load = || load_image_bytes(svc, item).map_err(|e| e.to_string());
        // dims 一起交给缓存：载荷是裸 RGBA8 时（现行规范）必须走 RGBA
        // 旁路，编码格式嗅探对它必然失败；文件型传 `None` ⇒ 走嗅探。
        cache.get_or_load_auto(key, dims, load).ok()
    };
    // 第二段（必要时）：旁路 cache 直接解一次。
    // thumb_cache 的失败抑制会让 2 秒内的失败调用直接返回 Err，但用户
    // 看到的「图片」很可能就卡在这条链路上——绕过抑制再解一次，往往就
    // 能显示（例如文件刚写好、上一帧还没落盘）。
    from_cache.or_else(|| {
        let svc = &mut f.svc;
        let bytes = match load_image_bytes(svc, item) {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(id, %e, "缩略图字节读取失败");
                return None;
            }
        };
        match crate::thumbnail::decode_auto(&bytes, dims, crate::thumbnail::THUMB_MAX_EDGE) {
            Ok(t) => Some(std::sync::Arc::new(t)),
            Err(e) => {
                tracing::debug!(id, ?e, "缩略图解码失败，回落文本预览");
                None
            }
        }
    })
}

/// 双击列表行的目标操作（2026-10-15 第三轮定稿）：
///
/// - 链接型文本（`https?://…`）⇒ 浏览器打开（尊重 `ui.browser_path`）；
/// - 文件型条目 / 形似路径的文本 ⇒ 资源管理器定位（尊重 `ui.explorer_path`）；
/// - 其余 ⇒ 复制。
///
/// CAUTION: 判据放在绘制层（而不是让 `App` 再认一遍内容）：
/// 双击时条目就在手上，kind + preview 都是现成的；`App` 侧只负责
/// 「用哪个程序打开」，不重复判断「该不该打开」。
///
/// CAUTION 2: **文件型条目必须读载荷取路径，不能用 preview**。
/// `build_preview` 给 Files 存的是**文件名摘要**（`paint.rs 等 12 个文件`），
/// 完整路径只存在于载荷字节（换行拼接）里。拿 preview 去定位，
/// explorer 收到的是一个文件名——打不开用户想要的位置。
/// 文本型路径没有这个问题（preview 即原文）。
pub(super) fn double_click_op(f: &mut Frame<'_>, item: &modular_clipboard_core::item::ClipItem) -> Op {
    use modular_clipboard_core::ClipKind;
    use modular_clipboard_platform::{looks_like_path, looks_like_url};
    let text = item.preview.trim();
    if item.kind == ClipKind::Text && looks_like_url(text) {
        return Op::OpenUrl(text.to_string());
    }
    let is_pathish =
        item.kind == ClipKind::Files || (item.kind == ClipKind::Text && looks_like_path(text));
    if is_pathish {
        let first = if item.kind == ClipKind::Files {
            // 载荷是换行拼接的路径列表，取第一行。读失败（载荷已被淘汰 /
            // 通道故障）就放弃跳转——回落成复制好过打开一个错误的位置。
            match f.svc.load_payload_bytes(item.id) {
                Ok(bytes) => String::from_utf8_lossy(&bytes)
                    .lines()
                    .map(str::trim)
                    .find(|s| !s.is_empty())
                    .map(str::to_string),
                Err(e) => {
                    tracing::debug!(id = item.id, %e, "读取文件条目载荷失败，双击跳转降级为复制");
                    None
                }
            }
        } else {
            text.lines().next().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
        };
        if let Some(path) = first {
            return Op::RevealPath(path);
        }
    }
    Op::Copy(item.id)
}

// ---------------------------------------------------------------- 状态栏

pub(super) fn format_meta(ts_ms: i64) -> String {
    modular_clipboard_app::format_time(ts_ms, modular_clipboard_core::now_ms())
}

/// 内容类型的展示名（右下角标签）。
///
/// 单一数据源在 [`modular_clipboard_core::ClipKind::label`]：
/// 类型过滤栏的 tooltip、设置界面、捕获层提示都读同一份。
pub(super) fn kind_label(kind: modular_clipboard_core::ClipKind) -> &'static str {
    t(kind.label())
}

/// 字节数 → 自适应单位（B / KB / MB / GB）。
///
/// 直接显示原始字节数既占位又难读：`1234567 字节` 一眼看不出量级。
/// 这里按 1024 进位取一档，整数部分 <10 时保留一位小数（`1.5 KB`），
/// 否则整数显示（`12 KB`），避免「12.3 KB」这类不必要的小数噪音。
pub(super) fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    let one = |v: f64, unit: &str| {
        if v >= 10.0 {
            format!("{v:.0} {unit}")
        } else {
            format!("{v:.1} {unit}")
        }
    };
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < MB {
        one(b / KB, "KB")
    } else if b < GB {
        one(b / MB, "MB")
    } else {
        one(b / GB, "GB")
    }
}

/// 卡片矩形在**窗口客户区**里的归一化区域（卡片磨砂底图的采样 UV）。
///
/// 底图抓的是整个客户区身后的桌面，所以 UV 就是卡片相对客户区的位置。
///
/// 卡片部分滚出视口时 UV 会越界：线性采样按 clamp 处理，而那部分本来
/// 也被滚动区的裁剪挡住，看不到——不值得为它做额外的裁剪计算。
fn card_blur_uv(area: Rect, r: Rect) -> Rect {
    let w = area.width().max(1.0);
    let h = area.height().max(1.0);
    Rect::from_min_max(
        pos2((r.min.x - area.min.x) / w, (r.min.y - area.min.y) / h),
        pos2((r.max.x - area.min.x) / w, (r.max.y - area.min.y) / h),
    )
}
