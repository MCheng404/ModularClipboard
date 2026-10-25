//! 历史列表：虚拟化行、行高测量与图片显示尺寸。
//!
//! 行高按「内容签名」缓存（`RowHeightCache`），测量宽度与绘制
//! 宽度逐位相同——两处算式分叉就会裁字。

use egui::Rect;
use egui::pos2;
use egui::vec2;
use modular_clipboard_core::ClipItem;
use modular_clipboard_core::i18n::t;
use crate::card::CardKind;
use crate::theme::Palette;
use crate::theme::sized;

use super::{Frame, RowHeightCache, RowFilterKey, ROW_RADIUS};

use super::detail::draw_empty;
use super::shadow::draw_card_shadow;
use super::row::draw_item_row;
use super::row::preview_is_image;

// ---------------------------------------------------------------- 内容

/// 首帧该从哪一行开始画（手工虚拟化的起始下标）。
///
/// `offsets[i]` 是第 i 行的顶边，`heights[i]` 是它的**实际**高度
/// （预览折行数不同则高度不同），`scroll` 是滚动偏移。
///
/// # 为什么 `ROW_HEIGHT_MIN` 不能用在这里
///
/// 「第 i 行已完全滚出可视区上方」的真正条件是
/// `offsets[i] + heights[i] <= scroll`。用代表**单行**高度的
/// `ROW_HEIGHT_MIN` 代替，则任何预览超过 1 行的条目都会被提前判定为
/// 「已滚过」—— 它明明还铺满整个视口，却一行都不画。
///
/// 实测推演（heights = [58, 3816, 58]，视口 500，scroll = 200）：
/// 第 1 行占 67..3883 仍铺满视口，但 `67 + 58 = 125 <= 200` ⇒ 被跳过，
/// 第 2 行的 y 又超出视口底 ⇒ `break` ⇒ **列表整块空白**。
///
/// # 为什么抽成函数
///
/// 这段逻辑躲在 `draw_history_body` 里、依赖 `Frame`，而列表行**不登记**
/// 自报热区 —— 于是从外部**没有任何办法**观测到「行被跳过了」。
/// 抽成纯函数后判据才能落在真正出错的地方。
pub(super) fn first_visible_row(offsets: &[f32], heights: &[f32], scroll: f32) -> usize {
    debug_assert!(
        offsets.len() >= heights.len(),
        "offsets 应比 heights 多一项（末项是总高），实际 {} vs {}",
        offsets.len(),
        heights.len()
    );
    // `take_while` 在首个不满足处停止，语义与 `partition_point` 相同；
    // 用它而非 `partition_point` 是因为后者是 slice 方法，而这里要同时
    // 查两个数组的第 i 项。零分配——这条路径每帧都跑。
    (0..heights.len())
        .take_while(|&i| offsets[i] + heights[i] <= scroll)
        .count()
}

pub(super) fn draw_history_body(f: &mut Frame<'_>, body: Rect, only_pinned: Option<bool>) {
    // 自报列表可视区。
    //
    // NOTE: 这个 key **不是**给绘制用的，是给「热区自报」机制用的：
    // 测试与自动化脚本从 `UiState::hit` 里读坐标，而不是自己按
    // `pal` 常量重算一遍公式——重算等于把实现抄进测试，实现改对了
    // 测试反而挂。
    //
    // 两个列表（主栏 / 内嵌置顶区）必须用**不同 key**：`hit` 是
    // `HashMap<String, Rect>`，同名会互相覆盖（settings 开关就踩过）。
    f.hit_insert(
        if only_pinned == Some(true) {
            "pinned_body"
        } else {
            "history_body"
        },
        body,
    );

    // ---- 列表显示区域**不画背景** --------------------------------------
    //
    // 早前这里铺一层 `list_bg`（比窗口底更暗）来衬出亮卡片。现在去掉：
    //
    // 1. 它与「一体性」目标冲突——多一层灰就多一道色块边界，
    //    整窗读起来是「拼出来的」；
    // 2. 它把窗口的半透明/磨砂**整块盖住**——列表是主界面最大的一块，
    //    铺了不透明的底，背后透出来的东西（磨砂底图、桌面）就全被挡住，
    //    「半透明」只剩顶栏与边距那几像素看得见。
    //
    // 层次改由**行卡片自身**承担：`surface_raised` 的亮底 + 一圈极弱描边
    // 已经足够让卡片「浮」起来，不需要暗色容器衬托。

    // 本列表用哪个滚动槽位。
    //
    // 内嵌置顶区已移除，所以常态下**只有一个列表、一个槽位**。
    // 仅当工作区里存在 `CardKind::Pinned` 独立卡片时才会出现第二个，
    // 此时才需要分开存 —— 早前内嵌置顶区与主栏共用一个 `f32`，
    // 而 `vertical_scroll_offset` 每帧都会把它写进 egui，
    // 于是滚主栏会把置顶区一起带走（反之亦然）。
    let pinned_slot = only_pinned == Some(true);

    // ---- 列表边距 ------------------------------------------------------
    //
    // 内容区左右各缩进一条竖边距（`LIST_PAD_X`），历史条目像卡片一样
    // 悬浮在留白中；左右留白竖条同时上报为**拖动区**（WM_NCHITTEST
    // 返回 HTCAPTION），用户按住列表两侧的空白即可拖动窗口。
    //
    // NOTE: 滚动条放在内容区右缘（`ScrollArea` 的 `max_rect` 用
    // `content`），因此滚动条不会压进右留白拖动区——若滚动条贴着
    // `body.max.x` 画，右拖动区就正好压在滚动条上，拖滚动条会变成
    // 拖窗口。
    let content = Rect::from_min_max(
        pos2(body.min.x + LIST_PAD_X, body.min.y),
        pos2(body.max.x - LIST_PAD_X, body.max.y),
    );
    if content.width() <= 8.0 || content.height() <= 8.0 {
        return;
    }
    f.state.drag_regions.push(Rect::from_min_max(
        pos2(body.min.x, body.min.y),
        pos2(body.min.x + LIST_PAD_X, body.max.y),
    ));
    f.state.drag_regions.push(Rect::from_min_max(
        pos2(body.max.x - LIST_PAD_X, body.min.y),
        pos2(body.max.x, body.max.y),
    ));

    // 过滤：全部 / 仅置顶。
    //
    // 主栏（`None`）默认**包含**置顶条目 —— 置顶就在同一个列表里，
    // 只靠 `pin_tint` 标记区分。
    //
    // 但若置顶卡片真的会被**独立绘制**（`PinnedMode = 独立分栏`），
    // 主栏必须让位，否则同一个 `("row", item.id)` 被画两次，
    // egui 的 ID 冲突检查会直接在界面上打出警告。
    //
    // CAUTION: 判断条件必须用 [`Workspace::docked`] 而不是
    // `cards.iter().any(...)`。默认工作区**建了**一张
    // `CardKind::Pinned` 卡片（`workspace.rs` 的默认构造里就有），
    // 用 `cards.iter()` 会永远得到 true ⇒ 主栏永远排除置顶条目；
    // 而内嵌模式下那张卡片又被 `docked()` 跳过、无人绘制 ——
    // 两边都不画，**置顶条目彻底从界面上消失**。
    // `docked()` 内部已经处理了「置顶内嵌时不参与 dock」这个规则，
    // 它才是「这张卡片会不会真的被画出来」的准确来源。
    let pinned_drawn_separately = f.ws.docked().any(|c| c.kind == CardKind::Pinned);
    // 类型过滤（2026-10-15 第三轮：悬浮类型栏；2026-10-20 多选）。
    // 与 `query` 一样是**界面本地**过滤——不进 store 查询、不进 IPC
    // 快照，绘制时筛行。置顶条目同样参与类型过滤：置顶只是一个排序
    // 权重，不是豁免。
    //
    // CAUTION: 只从 `active_type_kinds()` 取**一个**集合——它是单选
    // （`type_filter`）与多选（`type_filter_multi`）两个字段的唯一
    // 汇合点（见那里的说明）。
    let type_kinds = f.state.active_type_kinds();
    // 只收**索引**，不克隆条目（PF-1）：整表 `.cloned()` 每帧拷贝全部
    // preview / hash / source_app 三个 String（2000 条时是数 MB/帧），
    // 而绘制只碰可见行。索引 8 字节/行，测量与绘制都按需取原表。
    let rows: Vec<usize> = f
        .svc
        .state
        .items
        .iter()
        .enumerate()
        .filter(|(_, it)| {
            (type_kinds.is_empty() || type_kinds.contains(&it.kind))
                && match only_pinned {
                    Some(p) => it.pinned == p,
                    None => !(pinned_drawn_separately && it.pinned),
                }
        })
        .map(|(i, _)| i)
        .collect();
    // NOTE: 这里**不排序**。置顶排最前是**数据层**保证的 ——
    // store 的 SQL 写着 `ORDER BY pinned DESC, created_at DESC`
    // （见 `modular-clipboard-store` 的 `list()`）。绘制层再排一次
    // 属于重复且错层：排序规则要改就得改两个地方，而早前内嵌置顶区
    // 与主栏各画一份时，两边排序规则一旦不一致就会出现
    // 「同一条目在两处位置不同」。

    if rows.is_empty() {
        let msg = match only_pinned {
            Some(true) => t("No pinned entries yet"),
            // 有查询词**或类型过滤**生效 ⇒ 「没有匹配的结果」；
            // 两者都空才是「还没有剪贴板记录」（从没复制过东西）。
            _ if !f.state.query.is_empty() || !type_kinds.is_empty() => t("No matching results"),
            _ => t("No clipboard history yet"),
        };
        draw_empty(f, body, msg);
        return;
    }

    // 行高：足够放两行文字（预览 + 来源/时间），让内容**完整可读**。
    //
    // 早前版本是 40pt 单行 + `elide_text` 截断，长路径/长文本只能看到
    // 一小段（截图里 `miniz_oxide-36a1…` 就是这样）。用户要求「完整
    // 显示内容」，因此改成两行：上行给预览（最多两行），下行给来源时间。
    // ---- 动态行高 -------------------------------------------------
    //
    // 每条内容按折行数算自己的高度（见 [`row_height_for`]），
    // 这样长内容能被完整展开、短内容不占多余空间。
    //
    // 带缓存（PF-1）：行高只取决于「条目文本 + 测量宽度 + 缩放」，
    // 跨帧不变。失效判据是三者的等价类——列表版本号（每次 reload +1）、
    // `text_w`、`scale`——全部相同才复用，否则重算并换新缓存。
    let pal = f.pal;
    let scale = f.scale;
    // IMPORTANT: 必须用 `preview_full_width`——与 `draw_item_row` 共用同一个算式。
    // 宽度取内容区（左右已扣边距），保证测量与渲染一致。
    //
    // NOTE: 这里用的是**整行**宽度（不减按钮列）：预览带位于按钮行下方，
    // 因此测量折行数时也必须按整行宽算，否则量出的行数会偏多、行高虚高。
    //
    // IMPORTANT: **无条件**让出滚动条占位与呼吸空间。绘制侧的行矩形在
    // 「可滚动」时会内缩 [`SCROLLBAR_RESERVE`] + [`SCROLLBAR_GAP`]
    // （否则 egui 的 floating 滚动条会盖住删除按钮，见下面
    // `row_w` 的注释），而「是否可滚动」取决于 `content_h`、也就是
    // 本函数算出来的行高——直接用 `content.width()` 测量就是一个**先鸡
    // 先蛋**：量的时候按宽宽度算，量完才知道要滚动，那时行已经窄了
    // 16pt ⇒ 折行数偏少 ⇒ 行高偏小 ⇒ 文字被裁。实测症状正是
    // 「条目少时正常、条目一多就每行少显示一点」。
    //
    // 无条件让出为什么**不会**引入新问题（两个方向都是安全的）：
    // 1. 可滚动时：测量与绘制**完全同宽**（都让出），这是要修的情况；
    // 2. 不可滚动时：绘制宽度比测量**多** 16pt ⇒ 文字实际能放的比
    //    量出来的更宽 ⇒ 行高**偏大**而非偏小 ⇒ 最多多留一点空白，
    //    绝不会裁字。方向反了才是 bug（那是早前的写法）。
    //
    // 单调性保证不会来回抖：让出 16pt 只会让行变高，`content_h` 只增
    // 不减，所以「本来可滚动 ⇒ 让出后仍可滚动」，滚动条不会闪。
    let text_w = preview_full_width(row_measure_width(content.width()));
    let version = f.svc.state.list_version;
    // 解码得到的尺寸数：图片行高的输入之一，进缓存的失效判据
    // （见 [`RowHeightCache::dims_known`]）。
    let dims_known = f.state.thumb_dims.len();
    // 过滤维度必须进键（见 [`RowFilterKey`]）：`type_filter` 等是绘制层
    // 本地状态，不过 store ⇒ `list_version` 不变，而长度相同的两次过滤
    // 会让缓存误命中、下标错位。
    let filter = RowFilterKey {
        type_kinds,
        only_pinned,
        pinned_drawn_separately,
    };
    let heights: Vec<f32> = {
        let hit = f.state.row_height_cache[pinned_slot as usize]
            .as_ref()
            .filter(|c| {
                c.version == version
                    && c.filter == filter
                    && c.text_w == text_w
                    && c.scale == scale
                    && c.dims_known == dims_known
                    && c.heights.len() == rows.len()
            });
        match hit {
            Some(c) => c.heights.clone(),
            None => {
                // CAUTION: `thumb_dims` 的引用必须在**可变借用**
                // `row_height_cache` 之前取好，否则借用冲突。
                let known = &f.state.thumb_dims;
                let h: Vec<f32> = rows
                    .iter()
                    .map(|&i| {
                        let item = &f.svc.state.items[i];
                        row_height_for(
                            f.ui,
                            item,
                            pal,
                            scale,
                            text_w,
                            &f.svc.state.config.ui,
                            known.get(&item.id).copied(),
                        )
                    })
                    .collect();
                f.state.row_height_cache[pinned_slot as usize] = Some(RowHeightCache {
                    version,
                    filter,
                    text_w,
                    scale,
                    dims_known,
                    heights: h.clone(),
                });
                h
            }
        }
    };
    // 行间缝隙：阴影开启时按「偏移 + 弥散半径」动态加宽（见
    // `history_row_gap`），保证投影不被下一张卡片盖住；关闭时回落
    // 到基础 `ROW_GAP`。首项顶部留半条，每项之后再留整条。
    // **最后一项之后不留**——否则滚到底时最后一行会悬在
    // 列表底部之上一条缝（gap），贴底点击/视觉都对不上。
    let row_gap = history_row_gap(&f.svc.state.config.ui.card_shadow);
    let mut offsets: Vec<f32> = Vec::with_capacity(heights.len() + 1);
    offsets.push(row_gap * 0.5);
    for (i, h) in heights.iter().enumerate() {
        let inc = if i + 1 == heights.len() { *h } else { h + row_gap };
        offsets.push(offsets.last().copied().unwrap_or(0.0) + inc);
    }
    let content_h = offsets.last().copied().unwrap_or(0.0);

    // ---- 滚动 ----
    //
    // 滚动偏移夹在 `[0, content_h - 可视高]`：不夹的话快速滚到底后
    // 列表会整体上移、最后几项「跳」出可视区。
    let max_scroll = (content_h - body.height()).max(0.0);
    let scroll = f.state.list_scroll(pinned_slot).clamp(0.0, max_scroll);

    // # 为什么用 `ScrollArea::show` 而不是 `show_rows`
    //
    // `show_rows` 要求**恒定行高**（参数就叫 `row_height_sans_spacing`），
    // 而本列表的行高是**逐条算出来**的（[`row_height_for`]）——
    // 预览长度差异极大，固定行高要么截断长内容、要么给短内容留白。
    // 那是早前 40pt 固定行高被用户否决过的方案。
    //
    // 所以用 `show` + 自己做虚拟化：外层 `ScrollArea` 提供**裁剪与
    // 滚动条**（这两件事手写最容易出错），内层仍按前缀和只画可视行。
    //
    // 顺带解决了一个旧缺陷：自绘滚动条的滑块长度曾用
    // 「可视高 / 固定行高」反推行数，算出来恒为 1（永远是满格），
    // 现在交给 egui 按真实内容高算。
    let mut list_ui = f.ui.new_child(
        egui::UiBuilder::new()
            .scope_id(egui::Id::unique(("list", only_pinned.unwrap_or(false))))
            .max_rect(content)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    let out = egui::ScrollArea::vertical()
        // NOTE: 0.36 用 `id_salt`（不再是旧版的 `id_source`）。
        // 必须给：主栏与内嵌置顶区是两个独立列表，共用默认 Id 会
        // 共享滚动位置——滚主栏会把置顶区一起带走。
        .id_salt(("history_scroll", pinned_slot))
        .max_height(body.height())
        // 每帧把我们存的偏移灌进去：虚拟化要在 `show` **之前**知道
        // 滚到哪了，而 `ScrollArea` 自己的状态只在结束后才拿得到。
        .vertical_scroll_offset(scroll)
        // 滚轮交给 egui 处理，步长用 multiplier 调。
        //
        // CAUTION: 这里**不能**再自己读 `smooth_scroll_delta` 手动加一次：
        // `ScrollArea` 内部已经消费了同一个事件（它同样只在指针悬停
        // 于本区域时生效），两边叠加会以两倍速度滚。早前就是双份的。
        //
        // 平台层一个刻度 = 1.0 行（见 `wheel_lines`），egui 默认
        // `line_scroll_speed = 40`，即 40pt/刻度 ≈ 1 行，太慢；
        // ×3 对齐 Windows 的「一个刻度滚 3 行」惯例。
        .wheel_scroll_multiplier(egui::vec2(1.0, 3.0))
        .auto_shrink([false, false])
        .show(&mut list_ui, |ui| {
            // 内容高度决定滚动条比例，必须显式给出，否则 egui 按
            // 「子控件实际占用」算，而虚拟化下只画了可视行 ⇒ 比例失真。
            ui.set_height(content_h);

            // ---- 坐标系：**一律用全局坐标** --------------------------------
            //
            // NOTE: egui 的 `Rect` **永远是全局（窗口）坐标**，不存在
            // 「相对某个 Ui 的本地坐标」。`ScrollArea` 内部虽然把内容
            // Ui 建成 `content_max_rect = from_min_size(inner_rect.min
            // - state.offset, …)`，但那只是**可用区域**的偏移，用来让
            // 常规控件（跟着布局游标走的那些）自动排到滚动后的位置；
            // 它**不会**给传进来的坐标做任何变换。
            //
            // 早前这里写成 `pos2(0.0, offsets[i])`，于是行全部被画到
            // 窗口左上角（x=0），而列表区域一片空白。这类错误的
            // 隐蔽之处在于：空列表时根本不执行这段代码。
            //
            // 正确做法与改 ScrollArea 之前一致——用绝对坐标：
            // `body.min.y + offsets[i] - scroll`。ScrollArea 在这套
            // 手工虚拟化里的作用只剩**裁剪**与**滚动条**。
            // CAUTION: 判据必须用**每行自己的实际高度**，不能用 `ROW_HEIGHT_MIN`。
            //
            // 早前写成 `offsets[i] + ROW_HEIGHT_MIN <= scroll`：
            // 「第 i 行已经滚过去了」的真正条件是
            // `offsets[i] + heights[i] <= scroll`，而 `heights[i]` 只在
            // 预览恰好 1 行时才等于 `ROW_HEIGHT_MIN`。
            //
            // 只要某条预览超过 1 行（复制一段代码/长文本，剪贴板工具的
            // 最常规用法），就存在区间
            // `scroll ∈ [offsets[i]+58, offsets[i]+heights[i])`
            // —— 此区间内该行**明明还看得见**却被判定为「已滚过」而跳过。
            //
            // 实测推演（heights=[58, 3816, 58]，视口 500，scroll=200）：
            //   第 1 行占 67..3883，滚到 200 时仍然铺满整个视口；
            //   但 `67 + 58 = 125 <= 200` ⇒ 被跳过；
            //   第 2 行的 y = 3689 > 视口底 ⇒ break；
            //   ⇒ **一行都没画，列表整块空白**。
            //
            // `offsets[i] + heights[i]` 随 i 单调递增（offsets 递增、
            // heights 非负），满足 `partition_point` 的分段前提。
            let first = first_visible_row(&offsets, &heights, scroll);

            // 可见行范围预扫：与下面绘制循环的 break/continue 判据一致
            // （顶越过视口底即止）。只为确定「要克隆哪些行」。
            //
            // 只克隆可见行（PF-1）：整表克隆每帧拷贝全部 preview String，
            // 而一屏可见的通常不到 30 条。`draw_item_row` 需要 `&mut Frame`，
            // 无法直接借用 `f.svc.state.items`，克隆这一小段是最廉价的解法。
            let mut last = first;
            for (i, off) in offsets.iter().enumerate().take(rows.len()).skip(first) {
                if content.min.y + off - scroll > content.max.y {
                    break;
                }
                last = i + 1;
            }
            let visible: Vec<ClipItem> = rows
                .get(first..last)
                .map(|s| s.iter().map(|&i| f.svc.state.items[i].clone()).collect())
                .unwrap_or_default();

            // IMPORTANT: 行矩形本身必须避开垂直滚动条（仅**可滚动**时）。
            //
            // egui 0.36 默认 `ScrollStyle` 是 **floating**：滚动条
            // **不占布局宽度**，而是浮在 `content` 右缘覆盖
            // `[content.max.x - 10, content.max.x]`（宽度见
            // `ScrollStyle::floating().bar_width = 10.0`）。
            //
            // 而滚动条在内容闭包**之后**才注册交互（`scroll_area.rs`
            // 用 `Sense::CLICK | Sense::DRAG` 注册 `max_bar_rect`），
            // egui 的 hit-test 规则是「并列取最后注册 = 最上层」
            // ⇒ **滚动条压过行内所有控件**。
            //
            // 早前行矩形用 `content.width()`，最右侧的删除按钮中心
            // 恰好落进滚动条 ⇒ 列表一多就**点不动**，而条目少（无滚动条）
            // 时一切正常 —— 「条目少就好、条目多就坏」是这条 bug 的指纹。
            //
            // 让行矩形右缘内缩，按钮/文字都以 `r.max.x` 为基准，于是
            // 自动全部避开，不必逐个改。
            //
            // 仅在 `content_h > 视口高`（有滚动条）时让出：否则白白
            // 损失 16pt 可用宽度。
            //
            // 让出量 = 滚动条热区（`SCROLLBAR_RESERVE`）+ 呼吸空间
            // （`SCROLLBAR_GAP`）：滑块与卡片之间不再零距相贴
            // （2026-10-17 用户反馈「滑动条容易和卡片挤在一起」）。
            let row_w = if content_h > content.height() {
                content.width() - SCROLLBAR_RESERVE - SCROLLBAR_GAP
            } else {
                content.width()
            };

            // ---- 新条目入场：首行从上方滑入 --------------------------------
            //
            // 版本号变化 = store 侧有增删置顶（复制新内容、置顶、删除）。
            // 变化的那一帧把入场补间**重启**到 0→1（200ms OutCubic），
            // 之后每帧由下面的 `tween_f32` 推进；行矩形按当前值从上方
            // 12pt 归位。只滑行矩形——`offsets`/行高不参与动画，布局
            // 与测量保持稳态（否则行高缓存要跟着抖）。
            //
            // 三个闸门缺一不可：
            // 1. **只首行**（`first == 0`）：滚到半截时复制的新条目在
            //    视口外，放了也看不见；且不该让「当前第一屏」无缘无故
            //    滑一下。
            // 2. **版本闸门**：搜索/类型过滤是绘制层本地状态、不 bump
            //    `list_version`（见 `RowFilterKey`）⇒ 敲键筛选不触发。
            //    没有这层，每敲一个键首行就滑一次，像卡带。
            // 3. **重启而非惰性目标**：`tween_f32` 的目标恒为 1，第一次
            //    调用后 from==to，之后再也不会重播（主题切换踩过同一个
            //    坑，见 `anim::restart` 的文档）。
            let slot_seen = &mut f.state.list_version_seen[pinned_slot as usize];
            let play_entry = first == 0 && slot_seen.is_some_and(|v| v != version);
            *slot_seen = Some(version);
            if play_entry {
                crate::anim::restart(
                    ui.ctx(),
                    egui::Id::unique(("list_row_in", pinned_slot)),
                    0.0,
                    1.0,
                    std::time::Duration::from_millis(200),
                    crate::anim::Ease::OutCubic,
                );
            }

            // 先收集真正可见行的几何（过滤条件与下面的内容绘制循环
            // 完全一致），供阴影/内容两遍渲染共用。
            let mut frames: Vec<(usize, usize, Rect)> = Vec::new();
            for (k, i) in (first..last).enumerate() {
                let y = content.min.y + offsets[i] - scroll;
                if y > content.max.y {
                    break; // 已越过可视区底部
                }
                // 首行的入场位移：稳态 k=1（无位移）；动画进行中时行矩形
                // （绘制与交互同一矩形）从上方 12pt 归位，被 ScrollArea
                // 的视口裁剪 ⇒ 观感是「新卡片从列表上缘被发出来」。
                let y = if i == 0 {
                    let k_in = crate::anim::tween_f32(
                        ui.ctx(),
                        egui::Id::unique(("list_row_in", pinned_slot)),
                        1.0,
                        std::time::Duration::from_millis(200),
                        crate::anim::Ease::OutCubic,
                    );
                    y - (1.0 - k_in) * ROW_IN_SLIDE
                } else {
                    y
                };
                let r = Rect::from_min_size(
                    pos2(content.min.x, y),
                    vec2(row_w, heights[i]),
                );
                if r.max.y < content.min.y {
                    continue; // 整行都在可视区上方
                }
                frames.push((k, i, r));
            }

            // 本帧本列表可见的图片条目报到帧级集合（`thumb_seen`）——
            // 淘汰在 `draw` 收尾统一做，见该字段的说明。
            for (_, i, _) in &frames {
                let it = &f.svc.state.items[rows[*i]];
                // CAUTION: 判据必须是 [`preview_is_image`] 而不是
                // `kind == Image`。文件型图片（从资源管理器复制的 .png）
                // 同样会上传纹理，漏报它 ⇒ `draw` 收尾的 retain 把刚传的
                // 纹理当「已滚出屏」丢掉 ⇒ **下一帧重新读盘解码**，
                // 每帧一次磁盘 IO（正是 PF-1 禁止的），界面会持续卡顿。
                if preview_is_image(it) {
                    f.state.thumb_seen.insert(it.id);
                }
            }

            // ---- 第一遍：所有行的卡片阴影（统一在内容之前） ----
            //
            // 必须用视口级 painter（不能用 `painter_at(r)`，否则外扩
            // 晕带被行矩形裁掉）；必须先于所有行内容画：落在卡片矩形
            // 内的阴影随后会被该行的不透明填充盖住，于是投影只在行间
            // 缝隙与列表边缘可见，后一行的阴影也不会压暗前一行的文字。
            // 几何细节与反混合公式见 `draw_card_shadow`。
            {
                let shadow_painter = ui.painter();
                let shadow_cfg = &f.svc.state.config.ui.card_shadow;
                for &(_, _, r) in &frames {
                    // CAUTION: `ui.painter()` 返回的就是 `&Painter`，不要再
                    // 套一层 `&`——clippy 的 needless_borrow 会报，且
                    // `&&Painter` 靠自动解引用勉强能编译，属于暗坑。
                    draw_card_shadow(shadow_painter, r, ROW_RADIUS, shadow_cfg, f.pal);
                }            }

            // ---- 第二遍：缝隙拖动区 + 行内容 ----
            for (k, i, r) in frames {
                // ---- 卡片缝隙 = 拖动区 ------------------------------------
                // 卡片与卡片之间的空白横带（含首行顶部的半条缝）同样上报
                // 为拖动区：WM_NCHITTEST 返回 HTCAPTION，用户按住行间
                // 缝隙即可拖动窗口，不必精确瞄准左右留白。
                //
                // NOTE: 右缘让出滚动条热区**与呼吸空间**（`content.max.x
                // - RESERVE - GAP`），否则「拖滚动条」会被系统判成
                // HTCAPTION 变成拖窗口——与左右留白拖动区避开滚动条是同
                // 一个原因。GAP 一并让出：拖动条若伸进呼吸空间，用户在
                // 滑块旁按住会同时命中两个区域。
                let gap_top = if i == 0 { content.min.y } else { r.min.y - row_gap };
                let gap_bottom = r.min.y.min(content.max.y);
                if gap_bottom > gap_top {
                    f.state.drag_regions.push(Rect::from_min_max(
                        pos2(content.min.x, gap_top.max(content.min.y)),
                        pos2(content.max.x - SCROLLBAR_RESERVE - SCROLLBAR_GAP, gap_bottom),
                    ));
                }
                draw_item_row(f, ui, r, &visible[k]);
            }
            // NOTE: 纹理淘汰**不在这里做**——主栏与内嵌置顶区各调一次
            // 本函数，各自 retain 会互相淘汰（见 UiState::thumb_seen）。
            // 统一收到帧级，由 `draw` 收尾处理。
            // 最后可视行之后的空白（内容不满一屏时的列表底部）也作为拖动
            // 区——它不覆盖任何卡片，按住中部空白即可拖动窗口。
            //
            // NOTE: 只在**内容不满一屏**（无滚动条）时补这条：滚动状态下底部
            // 会被下一行占据，误报会把下一行的可点击区域吃成 HTCAPTION。
            if content_h < content.height() {
                f.state.drag_regions.push(Rect::from_min_max(
                    pos2(content.min.x, content.min.y + content_h),
                    pos2(content.max.x, content.max.y),
                ));
            }
        });

    // ---- 回读 egui 的真实滚动位置 --------------------------------
    //
    // CAUTION: 必须回读，否则**拖动滚动条无效**：拖动改变的是 `ScrollArea`
    // 自己的状态，而我们每帧开头又用旧值把它覆盖回去了。
    //
    // 回读后两者就一致了：滚轮、拖条、触屏都由 egui 处理，我们只
    // 是把结果镜像一份，供**下一帧**的虚拟化使用。
    let reported = out.state.clamped_offset().y;
    f.state.set_list_scroll(pinned_slot, reported.clamp(0.0, max_scroll));
}

/// 行内「文字区」的可用宽度。
///
/// # 为什么必须是**同一个函数**
///
/// 早前测量（`row_height_for`）与渲染（`draw_item_row`）各写一份算式，
/// 而测量那份额外减了 `8.0`：于是按 `text_w` 量折行数、按宽 8pt 的
/// `preview_band` 渲染，折行数可能**多算一行** ⇒ 行高凭空高出一整行
/// （约 16pt），长内容下方留一条空白。
///
/// 「动态行高」的价值就在于贴合内容，两处不一致会让它稳定地偏大，
/// 而且偏差随窗口宽度变化、不同机器表现不同，极难定位。
pub(super) fn preview_width(body_w: f32, pal: &Palette) -> f32 {
    let acts_w = acts_width(pal);
    (body_w - ROW_PAD_H - acts_w - ROW_PAD_H).max(20.0)
}

/// 预览文本的可用宽度——**整行**宽度，不减按钮列。
///
/// # 为什么预览可以用整行宽
///
/// 三个操作按钮只占卡片的**右上角一行**，而预览带整体位于按钮行
/// **下方**（见 [`row_top_height`]），因此不会被按钮遮挡。
///
/// 早前预览与顶部 meta 共用 [`preview_width`]（减掉了整个按钮列宽），
/// 于是**每一行**预览右侧都白空出一列：长文本（URL、路径）明明还有
/// 位置却被迫折行或截断。用户反馈的「内容显示不到右侧」就是它。
pub(super) fn preview_full_width(body_w: f32) -> f32 {
    (body_w - ROW_PAD_H * 2.0).max(20.0)
}

/// 行**测量**用的内容宽度：内容区宽度**无条件**让出滚动条占位与呼吸空间。
///
/// # 为什么必须让出，以及为什么无条件
///
/// 绘制侧的行矩形在「可滚动」时会内缩 [`SCROLLBAR_RESERVE`] +
/// [`SCROLLBAR_GAP`]（见 `draw_history_body` 里的 `row_w`；不缩的话
/// egui 的 floating 滚动条会盖住删除按钮，滚动条压过所有行内控件）。
/// 而「是否可滚动」由 `content_h > 可视高` 判定，`content_h` 又来自
/// 行高——行高来自本函数。
///
/// 于是直接按 `content.width()` 测量就是先鸡先蛋：量的时候按**宽**
/// 宽度算折行，量完才发现要滚动，那时真实宽度已经窄了 16pt ⇒ 折行数
/// 偏少 ⇒ 行高偏小 ⇒ 文字被裁。症状有明确指纹：**条目少时正常、
/// 条目一多（出现滚动条）就每行少显示一点**。
///
/// 无条件让出在两个方向都安全：
/// - 可滚动：测量与绘制同宽（都让出）——修好的正是这一支；
/// - 不可滚动：绘制比测量**多** 16pt ⇒ 行高**偏大**，最多留一点空白，
///   绝不裁字。
///
/// 且单调：让出只会让行变高，`content_h` 只增不减 ⇒ 滚动条不会在两
/// 帧之间来回闪。
pub(super) fn row_measure_width(content_w: f32) -> f32 {
    (content_w - SCROLLBAR_RESERVE - SCROLLBAR_GAP).max(20.0)
}

/// 行内**顶部一行**的高度：左侧「来源·时间」与右侧操作按钮取较高者。
///
/// 必须由 [`draw_item_row`] 与 [`row_height_for`] **共用**：按钮比 meta
/// 文字高（`ROW_ACTS_DROP + btn`），若测量时只算 meta 高，预览带就会
/// 上移到按钮所在那一行、被按钮压住——而「压住」在截图上表现为
/// 「文字跑到按钮底下」，很容易被误判成按钮位置错了。
pub(super) fn row_top_height(pal: &Palette) -> f32 {
    let meta_h = pal.font_xs.max(10.0);
    let acts_h = ROW_ACTS_DROP + pal.control_height * 0.55;
    meta_h.max(acts_h)
}

/// 行内三个操作按钮的总宽（含间隙）。
///
/// 三处都用它：[`preview_width`] 算文本区右边界、`draw_item_row`
/// 算按钮位置、`row_height_for` 兜底检查。**必须同源**，
/// 否则文本会伸到按钮底下、或按钮被提前 break 掉。
pub(super) fn acts_width(pal: &Palette) -> f32 {
    3.0 * (pal.control_height * 0.55 + pal.space_xs)
}

/// 行卡片的**左右**内边距（逻辑点）。
///
/// 早前只有垂直的 `pad_v`，左右为 0 —— 三段文字直接贴死卡片边缘，
/// 视觉上像溢出卡面。去掉类型图标后左侧空出来的位置正好用作内边距。
pub(super) const ROW_PAD_H: f32 = 12.0;

/// 行内三个操作按钮相对来源行的**下移量**（逻辑点）。
///
/// 按钮贴在右上角、但比来源行低这么多。来源行字高 11pt、按钮 15.4pt，
/// 两者顶部对齐时按钮会探进来源行里，下移 4pt 让开字形上沿。
pub(super) const ROW_ACTS_DROP: f32 = 4.0;

/// 列表行的**最小**高度（逻辑点）。
///
/// 三段布局：左上来源行 + 预览（14pt，可折行）+ 右下类型/大小行。
/// 预览一行 ≈ 19pt、上下两行 meta 各 ≈ 11pt、内边距 2×5 + 行间 4，
/// 合计 ≈ 55pt，取下限 58 兜底按钮区域。
///
/// NOTE: 这是**下限**不是固定值：早前用固定 40pt 时预览只够放一行半，
/// 长内容被省略号截断（用户反馈「需要完整显示内容」）。
/// 实际行高由 [`row_height_for`] 按每条内容的折行数动态算出。
pub(super) const ROW_HEIGHT_MIN: f32 = 58.0;

/// 列表内容区左右留白（逻辑点）。
///
/// 这条竖边距既是「卡片与窗口边缘之间的呼吸感」，也是**拖动区**：
/// 用户按住留白拖动即可移动窗口（见 `draw_history_body` 上报的
/// `drag_regions`）。
pub(super) const LIST_PAD_X: f32 = 8.0;

/// 列表卡片之间的垂直间隙（逻辑点）。
///
/// 卡片样式 = 每条历史是一张圆角小卡，卡与卡之间留这条缝；
/// 首项顶部再留半条，让整体上下边缘也有留白。
pub(super) const ROW_GAP: f32 = 6.0;
/// 新条目入场滑入的距离（逻辑点）：首行从上方这么远归位。
///
/// 取 2 倍行隙：足以被感知为「滑入」，又不至于让首行在归位途中压到
/// 顶栏。位移只作用于**行矩形**（绘制与交互同一矩形），`offsets`/行高
/// 不参与——布局保持稳态，行高缓存不会因动画失效。
pub(super) const ROW_IN_SLIDE: f32 = 12.0;
/// 阴影开启时行间缝隙的最大值：参数拉满（offset 16 + blur 24）理论
/// 需要 40pt，列表会过度松散，视觉上也不成立——设上限截断。
pub(super) const ROW_GAP_MAX: f32 = 30.0;

/// 行间缝隙：阴影开启时按「偏移 + 弥散半径」动态加宽，保证投影主体
/// 不被下一张卡片盖住；关闭时回落到基础 [`ROW_GAP`]。
///
/// 弥散系数取 1.0（≈ 2σ：该距离内高斯 alpha ≥ ~2.3%，肉眼可见；
/// 更远的残余淡影 <1%，被下一张卡片盖住无感知）。`offset_y` 取绝对值：
/// 偏移向下时投影需要下方缝隙、向上时需要上方缝隙，统一按绝对值给。
pub(super) fn history_row_gap(cfg: &modular_clipboard_core::config::CardShadowConfig) -> f32 {
    if !cfg.enabled {
        return ROW_GAP;
    }
    let need = cfg.offset_y.abs() + cfg.blur.clamp(0.0, 24.0);
    need.clamp(ROW_GAP, ROW_GAP_MAX)
}

/// 离开搜索区域后延迟多久折叠（秒）。
///
/// 必须 >0：见 [`UiState::search_leave_since`]。0.35s 足以覆盖
/// 「从图标移到输入框」的正常鼠标行程（图标与输入框之间是
/// `space_md` 的间隙），又不至于让输入框赖着不走。
pub(super) const SEARCH_COLLAPSE_DELAY: f64 = 0.35;

/// 悬停多久才自动展开搜索框（秒）。
///
/// 取值权衡：太短（<0.1s）会让「鼠标划过图标去点旁边的设置」也弹出搜索框；
/// 太长（>0.4s）用户会觉得「没反应」。0.18s 接近 Windows 悬停提示的节奏。
pub(super) const SEARCH_HOVER_DELAY: f64 = 0.18;

/// 面板刚展开后的**免折叠宽限**（秒）。
///
/// 展开的头几帧里状态还不稳：焦点要等弹窗画完才请求、再等它回报；
/// 指针可能已经离开图标。若此时折叠计时就开始跑，`SEARCH_COLLAPSE_DELAY`
/// 一到面板就自己收回去——用户看到的是「点一下闪一下就没了」。
///
/// 0.6s 覆盖「点击 → 弹窗首帧 → 焦点落地」的整条链路（实测 1~2 帧），
/// 又不至于让「点开就想关」的用户觉得迟钝（Esc / 点图标仍是立即收起）。
pub(super) const SEARCH_OPEN_GRACE: f64 = 0.6;

/// 「正在打字」的活跃窗口（秒）：最近这么长时间内输入内容变过才钉住
/// 面板不折叠（见 [`UiState::search_last_input_at`]）。
///
/// # 为什么不是焦点、也不是查询词非空
///
/// 2026-10-21 用户反馈「搜索框打开后无法自动关闭」，根因是旧判据
/// 「输入框有 egui 焦点 ⇒ 永不折叠」：egui 的焦点不因用户点到窗口
/// 别处、切到别的应用而释放（generic interact 不抢焦点），面板一旦
/// 获焦就永远收不回去。查询词非空同理——搜完就走的用户永远收不掉。
///
/// 改为**打字活跃**：键入/删除的间隙（2.5s）内不收（「打字打到一半
/// 弹窗自己没了」不会发生，每次键入都刷新时间戳）；停手 2.5s 后
/// 走正常的宽限折叠。查询词保留在状态里（列表过滤不丢），面板
/// 收起只是把输入框藏起来。
pub(super) const SEARCH_TYPING_GRACE: f64 = 2.5;

/// 垂直滚动条占位（逻辑点），缝隙拖动区右缘必须让出这一段。
///
/// `ScrollArea` 的滚动条画在内容区右缘内侧；若缝隙横条直抵
/// `content.max.x`，按住滚动条会被系统判成 HTCAPTION 变成拖窗口
/// （与左右留白拖动区避开滚动条是同一个原因）。
/// 垂直滚动条占用的宽度。
///
/// IMPORTANT: 必须对齐 egui 的 `ScrollStyle::floating().bar_width`（**10.0**，
/// 见 egui 0.36.2 `style.rs:646`）。
///
/// 早前这里是 8.0，比滚动条实际宽度**小 2pt** —— 于是按它算出的
/// 「已避开滚动条」的位置，仍然有 2pt 落在滚动条下面。
pub(super) const SCROLLBAR_RESERVE: f32 = crate::theme::SCROLLBAR_WIDTH;

/// 滚动条热区与行卡片之间的**呼吸空间**（逻辑点）。
///
/// # 为什么需要它（2026-10-17 用户反馈「滑动条容易和卡片挤在一起」）
///
/// egui 0.36 默认 `ScrollStyle` 是 **floating**：滚动条不占布局宽度，
/// 而是浮在 `[content.max.x - bar_width, content.max.x]` 上。行矩形按
/// [`SCROLLBAR_RESERVE`] 让位后，右缘恰好贴在滚动条热区的左边界上——
/// **零间距**。滑块本身还有 `bar_inner_margin`（3pt）的内缩，视觉上
/// 缝隙只剩 1~2pt，卡片的删除按钮、文字都与滑块擦身而过；拖动滑块时
/// 手指压到的也是卡片边缘，「挤在一起」的观感与误触都源于此。
///
/// 6pt 的取值依据：滑块视觉宽 = `bar_width - 2×bar_inner_margin` = 4pt，
/// 让出 6pt 后滑块与卡片之间约有 9pt 净空——与 `ROW_GAP`（6~8pt）的
/// 行间缝隙同档，整窗的「空白节奏」一致。
///
/// # 为什么测量侧要**无条件**让出（与 `SCROLLBAR_RESERVE` 同一理由）
///
/// 「是否可滚动」取决于行高，行高又取决于测量宽度——直接按
/// `content.width()` 量就是先鸡先蛋。两个方向都安全：可滚动时测量与
/// 绘制同宽（要修的情况）；不可滚动时绘制比测量宽 ⇒ 行高**偏大**，
/// 最多多留空白，绝不裁字。且单调 ⇒ 滚动条不会两帧之间闪。
pub(super) const SCROLLBAR_GAP: f32 = 6.0;

/// 每行文字的高度倍率。
pub(super) const LINE_H: f32 = 1.35;

/// 缩略图默认限高（逻辑点）。
///
/// 仅作为列表行内嵌缩略图的默认上限；详情面板在
/// [`draw_image_preview`] 里另行传更大的值（图占满整宽）。
///
/// 2026-10-17 从 108 提到 160：108pt 下 16:9 的截图只有 192pt 宽，
/// 在 400+pt 的列表里只占不到一半宽度，用户反馈「尺寸太小」。
/// 160 是「看得清」与「一屏还能放三四条」之间的折中——再往上
/// 一条记录就会占掉半屏，列表信息密度崩塌。
///
/// 配套：缩略图解码上限 [`crate::thumbnail::THUMB_MAX_EDGE`] 必须
/// ≥ 本值 × 常见 DPI 缩放（2x ⇒ 320px），否则放大显示会糊。
pub const IMG_MAX_H_LIST: f32 = 160.0;

/// 图片在给定带内的显示尺寸（宽, 高），保持长宽比。
///
/// 限高 `max_h`、限宽 `band_w`，两步都保持长宽比——
///
/// CAUTION: 不能「高先定死、宽超了再截」：那会把图拉扁
/// （`painter.image` 按 uv 0..1 把整张图填进矩形）。宽超限时必须
/// 回头按比例缩高，否则极窄窗口下截图变成压扁的条。
///
/// # 为什么必须是共用函数
///
/// [`row_height_for`] 用它量出行高，[`draw_image_preview`] 按那个行高
/// 画——两处各写一份算式的后果是**图片被压扁**：
/// 2026-10-17 修的就是这个（`row_height_for` 的文件型分支没走图片算式，
/// 行高按文件名文本算成 ~24pt，图被 `band.height()` 压成一条细带）。
/// 想改显示规则只能改这里。
pub fn image_display_size(dims: (u32, u32), band_w: f32, max_h: f32) -> (f32, f32) {
    if dims.0 == 0 || dims.1 == 0 {
        return (0.0, 0.0);
    }
    let ratio = dims.0 as f32 / dims.1 as f32;
    let mut h = max_h;
    let mut w = h * ratio;
    if w > band_w {
        w = band_w;
        h = w / ratio;
    }
    (w, h)
}

/// 单条列表项的**实际**行高：按内容排版结果计算。
///
/// # 为什么必须逐条算
///
/// 预览长度差异极大（`1` 和一段几百字的粘贴内容）。固定行高会
/// 出现两种坏结果：
/// - 行高 < 需要 → 长内容被省略号截断（看不到完整内容）；
/// - 行高 > 需要 → 短内容下方留一大块空白。
///
/// 折行数由 egui 自己算（[`egui::TextWrapping::max_rows`]），
/// 这里只取「实际用了几行」——比按字符数估算准得多，
/// 中英文混排、全角半角混排都能正确处理。
///
/// 2026-10-15 第三轮起有三种内容形态，行高各按各的算：
/// - 图片（`image_inline` 开）= 缩略图显示高（限高 [`IMG_MAX_H_LIST`]）；
/// - Markdown（`md_rendered` 开）= 排版实测高；
/// - 其余 = 折行文本高。
///
/// # 图片分支的判据必须是 [`preview_is_image`]
///
/// 2026-10-17 修：这里原先写 `item.kind == ClipKind::Image`，于是从资源
/// 管理器复制的图片**文件**（`ClipKind::Files`）走的是文本分支——行高按
/// 文件名那一行算（~24pt），而绘制侧 [`draw_image_preview`] 受
/// `band.height()` 约束，图就被压成一条细带（用户反馈「尺寸太小」）。
/// 判据与绘制侧**必须同源**，两类条目共用 [`image_display_size`]。
///
/// `known_dims` 是 [`UiState::thumb_dims`] 里已解码得到的尺寸：老条目
/// 的载荷里没有 `dims`，只能靠它排图（第一次解码后行高才对）。
pub(super) fn row_height_for(
    ui: &egui::Ui,
    item: &ClipItem,
    pal: &Palette,
    scale: f32,
    text_w: f32,
    cfg: &modular_clipboard_core::config::UiConfig,
    known_dims: Option<(u32, u32)>,
) -> f32 {
    let preview_font = sized(pal.font_md, scale);
    let meta_font = sized(pal.font_xs, scale);
    // IMPORTANT: 行高也必须用**逻辑点**、不乘 ppp：字号已经改成逻辑点，
    // 这里再乘一次会让行高与实际字形高度对不上（留白/裁剪都错）。
    let line_h = pal.font_md * LINE_H;
    let meta_h = meta_font.size.max(10.0);

    // 三段布局：顶部一行（meta / 按钮取较高者）+ 预览 + 底部 meta。
    // 与 `draw_item_row` 的 `preview_band` 严格对应。
    let top_h = row_top_height(pal);

    // ---- 内容高：图片 / Markdown / 文本三分支 ----
    // 尺寸两个来源，都不需要当场解码：
    // - `payload.dims`：入库时读的图片头（文件型由
    //   `app::first_image_file_dims` 补齐）；
    // - `known_dims`：上一次解码记下的真实尺寸（老条目缺前者时靠它）。
    let content_h = if cfg.image_inline
        && preview_is_image(item)
        && let Some(dims) = item
            .payload
            .as_ref()
            .and_then(|p| p.dims)
            .filter(|(w, h)| *w > 0 && *h > 0)
            .or(known_dims.filter(|(w, h)| *w > 0 && *h > 0))
    {
        // 缩略图显示高：与 [`draw_image_preview`] 共用算式。
        image_display_size(dims, text_w, IMG_MAX_H_LIST).1
    } else if item.kind == modular_clipboard_core::ClipKind::Markdown && cfg.md_rendered {
        crate::md::MdDoc::parse(&item.preview).height(ui, pal, scale, text_w)
    } else {
        // 折行一次，拿到实际行数。
        let mut job = egui::text::LayoutJob::default();
        job.append(
            &item.preview,
            0.0,
            egui::TextFormat {
                font_id: preview_font,
                color: pal.text,
                ..Default::default()
            },
        );
        job.wrap.max_width = text_w;
        // 不限行数，先量出「到底需要几行」。
        //
        // IMPORTANT: 必须用 `usize::MAX` 而不是 `0`：`TextWrapping::max_rows` 的
        // 默认值是 `usize::MAX`（不限），而它的文档明确写着
        // 「If set to `0`, no text will be outputted」——
        // **0 表示一行都不输出**，不是「不限」。
        //
        // 误设成 0 的后果：galley.rows 恒为空 → `n_lines` 恒为 1 →
        // 行高恒等于最小值 52pt → 预览带只有 24pt 高 → 长内容被省略号
        // 截断。实测诊断数据完全对上：
        //   row_h=52.8  band_h=24.3  galley_rows=1  elided=true
        job.wrap.max_rows = usize::MAX;
        job.wrap.break_anywhere = true;
        let galley = ui.painter().layout_job(job);
        // `rows` 是字段不是方法（`Arc<Galley>` 直接解引用）。
        galley.rows.len().max(1) as f32 * line_h
    };

    (content_h + meta_h + top_h + 5.0 * 2.0 + 4.0).max(ROW_HEIGHT_MIN)
}
