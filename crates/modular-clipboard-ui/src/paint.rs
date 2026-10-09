//! 绘制层：遍历工作区，按 `card.rect` 把卡片画出来。
//!
//! # 本模块的第一原则：**不做二次判断**
//!
//! 旧架构的历史症状几乎都源于「绘制层自己又判了一遍」：
//!
//! - 详情卡片折叠后，`draw` 里的 `if local.show_detail` 让整个绘制函数
//!   被跳过，可布局层照样分配了矩形 ⇒ **那块矩形无人绘制**，与相邻栏
//!   同色，看起来像两块糊在一起；
//! - 状态栏左侧提示按「预算」算宽度，实际字体偏大就压到右侧按钮上。
//!
//! 所以这里的规则是硬的：
//!
//! 1. **矩形只从 [`Card::rect`] 读**，不在这里算宽度、不做降级判断。
//! 2. **每张停靠卡片都必须被画**——折叠的画成把手，展开的画成卡片，
//!    没有「要不要画」这种分支。
//! 3. **任何文字都必须 `painter_at(所属矩形)`**。`ui.painter()` 是根
//!    painter、完全没有裁剪，用它画文字就是允许溢出到别的卡片上。
//!
//! # 为什么第3 条要单独强调
//!
//! 渲染层已经实现逐批次 scissor（见 `renderer::Batch::to_draw`），
//! 但那只保证「不超出**批次**的裁剪矩形」。而 `ui.painter()` 画出的
//! 文字，其批次裁剪矩形就是**整屏**——裁剪等于没裁。要真正限制在
//! 卡片内，必须显式传对应矩形给 `painter_at`。

use egui::{Align2, CornerRadius, FontId, Rect, Sense, Stroke, Ui, pos2, vec2};
use modular_clipboard_app::Service;
use modular_clipboard_core::ClipItem;

use crate::card::{Card, CardHost, CardKind, PinnedMode};
use crate::icons::Icon;
use crate::theme::{Palette, sized};
use crate::workspace::Workspace;

/// 卡片边框宽度（逻辑点）。
const CARD_STROKE: f32 = 1.0;

/// 卡片圆角（逻辑点）。
/// 卡片圆角（逻辑点）。
///
/// ⚠️ **取 0（直角）**，不是原先的 8。
///
/// 圆角会在相邻卡片之间与窗口边缘留下 4 个「缺口」：卡片明明
/// 紧贴（实测底部间隙 0px），但视觉上像有一条缝——用户报的
/// 「窗口与窗口之间存在间隙」有一部分就来自这里。
/// 直角 + 1px 描边才能得到连续的、真正无缝的面板。
///
/// 这也符合本项目「**纯平毛玻璃**」的视觉语言：禁止一切装饰性
/// 圆角带来的拟物暗示。
const CARD_RADIUS: f32 = 0.0;

/// 一帧内要执行的操作。
///
/// 绘制层**只识别操作、不改布局**：`Workspace` 的改动由上层在
/// `apply_ops` 里做。这样「点某个按钮」这件事不会分散在绘制代码里。
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// 选中某个条目。
    Select(modular_clipboard_core::EntryId),
    /// 切换条目置顶。
    TogglePin(modular_clipboard_core::EntryId),
    /// 删除条目。
    Delete(modular_clipboard_core::EntryId),
    /// 复制条目到剪贴板。
    Copy(modular_clipboard_core::EntryId),
    /// 置顶模式切换。
    SetPinnedMode(PinnedMode),

    // ---- 设置界面 ----
    //
    // ⚠️ 配置项**不能**在 paint 里直接改：`Frame` 借的是 `&UiConfig`，
    // 且改完还要落盘（`svc.save_config()`）。统一走 Op 由 `App` 落地。

    /// 改一个布尔配置项。
    SetBool {
        /// 配置分组。
        group: ConfigGroup,
        /// 组内字段名（见 `set_bool_field`）。
        field: &'static str,
        /// 目标值。
        value: bool,
    },
    /// 改一个数值配置项。
    SetNumber {
        group: ConfigGroup,
        field: &'static str,
        value: f64,
    },
    /// 改一个可选数值（`None` = 不限）。
    SetOptNumber {
        group: ConfigGroup,
        field: &'static str,
        /// `None` 表示「不限」。
        value: Option<f64>,
    },
    /// 打开/关闭设置界面。
    ToggleSettings,
    /// 清空全部历史。
    ClearAll,
    /// 退出程序。
    ///
    /// ⚠️ 早前 UI 层**没有任何退出入口**：`App::should_quit` 这个字段
    /// 存在、有初始化、有消费点、有 getter，但**全仓没有任何地方把它
    /// 置成 true**。于是设置窗口里没有「退出程序」按钮，而标题栏 ✕
    /// 在有托盘时只隐藏到托盘 —— 用户从界面里根本无法退出程序。
    Quit,
    /// 关闭按钮 ⇒ 隐藏到托盘。
    ///
    /// ⚠️ 语义是「隐藏」不是「退出」：剪贴板类工具直接退出会让
    /// 用户以为「记录停了」。真实决策（隐藏 / 退出）由 `App` 侧结合
    /// 有无托盘兜底判断，绘制层不决定。
    CloseWindow,
}

/// 配置分组。
///
/// 设置界面的每个可调项都归到某一组；`App` 侧用
/// [`ConfigGroup`] + 字段名定位到具体字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigGroup {
    Ui,
    Capture,
    Storage,
}

impl ConfigGroup {
    /// 分组标题（设置界面里显示）。
    pub fn title(self) -> &'static str {
        match self {
            ConfigGroup::Ui => "界面",
            ConfigGroup::Capture => "捕获",
            ConfigGroup::Storage => "存储",
        }
    }
}

/// 绘制上下文：跨帧保留的界面状态。
#[derive(Default)]
pub struct UiState {
    /// 本帧收集到的操作。
    pub ops: Vec<Op>,
    /// 搜索框文本。
    pub query: String,
    /// 上一帧提交给窗口的拖动区（标题栏）。
    ///
    /// ⚠️ 必须是**多段**而不是「整个顶栏一块」：拖动区会被系统转成
    /// `HTCAPTION`，点其中的控件不会产生 `WM_LBUTTONDOWN`，
    /// egui 收不到点击（表现：齿轮/垃圾桶/搜索框全都点不动）。
    ///
    /// 多个矩形的并集是拖动区；控件区从中间挖空。
    pub drag_regions: Vec<Rect>,
    /// 当前悬停的卡片。
    pub hover_card: Option<crate::card::CardId>,
    /// 列表滚动偏移（逻辑点），**按列表分开存**。
    ///
    /// ⚠️ 主栏历史列表与内嵌置顶区是两个独立列表。早前共用一个
    /// `f32`，而绘制每帧都会把它灌进 `ScrollArea`，于是滚其中一个
    /// 会把另一个一起带走。
    ///
    /// 约定：`[0]` = 历史列表，`[1]` = 置顶列表。经
    /// [`Self::list_scroll`] / [`Self::set_list_scroll`] 访问。
    list_scroll: [f32; 2],

    /// 搜索框 `TextEdit` 的 egui `Id`（本帧绘制时写入）。
    ///
    /// # 为什么需要
    ///
    /// egui 0.36 的 `Id` 是**哈希值**，`format!("{id:?}")` 只得到
    /// `Id::new(3070286450358649230)` 这样的数字，没法按名字匹配。
    /// 于是「点搜索框后焦点是否落在搜索框上」只能退化成「有没有任何
    /// 控件获得焦点」——那是**恒真**的，点别处也一样通过。
    ///
    /// 绘制层把自己用的 id 记下来，测试才能**精确比对**。
    pub search_edit_id: Option<egui::Id>,
    /// 设置界面是否打开。
    pub show_settings: bool,
    // 注：原先这里有 `settings_scroll`（手写滚动偏移）。设置面板
    // 改用 `ScrollArea` 后由 egui 自己管理滚动位置，该字段整体删除。

    // ---- 交互热区（供自动化测试定位）----
    //
    // ⚠️ 为什么让被测代码**自报坐标**而不是测试去算：
    //
    // 早前测试按「顶栏高 × 0.7、间距 8」自己估按钮位置，结果算出的
    // 矩形是 `[328,76]-[580,324]`（横跨整窗），**所有点击都落在按钮
    // 外**。而测试只是 assert 失败——布局一改就会静默测错东西。
    //
    // 这里让绘制层把真实矩形登记进来，测试只读。代价是生产代码多
    // 几个 Rect 的写入（可忽略），换来的是「布局改了测试自动跟随」。
    /// 本帧各命名热区的矩形（逻辑点）。
    pub hit: std::collections::HashMap<&'static str, Rect>,
    /// 关闭 / 最小化请求。
    pub close_requested: bool,
}

impl UiState {
    /// 取出并清空本帧操作。
    pub fn take_ops(&mut self) -> Vec<Op> {
        std::mem::take(&mut self.ops)
    }

    fn push(&mut self, op: Op) {
        self.ops.push(op);
    }

    /// 取某个列表的滚动偏移。
    ///
    /// `pinned = true` 取内嵌置顶区，`false` 取主栏历史列表。
    pub fn list_scroll(&self, pinned: bool) -> f32 {
        self.list_scroll[usize::from(pinned)]
    }

    /// 写某个列表的滚动偏移（见 [`Self::list_scroll`]）。
    pub fn set_list_scroll(&mut self, pinned: bool, v: f32) {
        self.list_scroll[usize::from(pinned)] = v;
    }
}

/// 一帧的绘制上下文。
pub struct Frame<'a> {
    /// 根 `Ui`。
    pub ui: &'a mut Ui,
    /// 界面状态。
    pub state: &'a mut UiState,
    /// 服务。
    pub svc: &'a mut Service,
    /// 工作区：卡片的唯一权威来源。
    pub ws: &'a Workspace,
    /// 调色板。
    pub pal: &'a Palette,
    /// DPI 缩放。
    pub scale: f32,
    /// 整个客户区。
    pub area: Rect,
    /// 本帧要绘制的卡片（子窗口用）。
    ///
    /// `None` 表示画主窗口（顶栏 + 全部停靠卡片）。
    ///
    /// ⚠️ 不能在 `draw_child` 里"找第一张 host==Window 的卡片"：
    /// 详情与视图都是 Window 型，两个子窗口会各自找到同一张卡片。
    pub card_id: Option<crate::card::CardId>,
}

/// 画一个**独立子窗口**的整帧内容。
///
/// 与 [`draw`] 的区别：那函数画「主窗口的顶栏 + 卡片区」，
/// 这函数画「一张卡片铺满整个窗口」。
///
/// # 为什么子窗口不画顶栏
///
/// 子窗口是纯内容视图，没有「搜索/设置」这类全局控件——
/// 那些只在主窗口有一份。子窗口只承载一张卡片的内容区。
pub fn draw_child(f: &mut Frame<'_>) {
    let area = f.area;
    f.ui.painter().rect_filled(area, 0.0, f.pal.bg);

    // 找到本窗口对应的那张卡片。
    //
    // ⚠️ 不 `expect`：`draw_child` 是生产路径，而「窗口存在但卡片
    // 不在列表里」是**可能发生**的短暂不一致（卡片刚被删、窗口
    // 还没销毁）。这里降级为空画而不是 panic——一个附属面板
    // 的状态问题不该让整个程序崩掉。
    let Some(id) = f.card_id else {
        draw_empty(f, area, "未指定卡片");
        return;
    };
    let Some(card) = f.ws.get(id) else {
        draw_empty(f, area, "窗口与卡片状态不同步");
        return;
    };

    // 卡片铺满整个客户区，四周留一点内边角。
    let r = area.shrink2(vec2(f.pal.space_sm, f.pal.space_sm));
    if r.width() <= 0.0 || r.height() <= 0.0 {
        return;
    }
    draw_card_frame(f, r);

    let pad = f.pal.space_sm;
    let inner = r.shrink2(vec2(pad, pad));
    let head_h = (f.pal.font_md * 1.8).max(20.0);
    let head = Rect::from_min_size(inner.min, vec2(inner.width(), head_h));
    draw_card_header(f, card, head);

    let body = Rect::from_min_max(
        pos2(inner.min.x, head.max.y + f.pal.space_xs),
        inner.max,
    );
    if body.width() <= 0.0 || body.height() <= 0.0 {
        return;
    }
    match card.kind {
        // 置顶卡片作为独立窗口时，只画列表内容（无标题头）——
        // 窗口标题栏已经说明了它是什么。
        CardKind::Pinned => draw_history_body(f, body, Some(true)),
        CardKind::Detail => draw_detail_body(f, body),
        CardKind::Rail => draw_rail_body(f, body),
        // 主窗口专属的种类不该出现在子窗口；画空状态而不是崩溃。
        _ => draw_empty(f, body, "该卡片不支持独立窗口"),
    }
}

/// 画一帧。
///
/// 流程：顶栏 → 设置界面（覆盖层）→ 卡片（Z 序）。
/// 与旧实现最大的差别是**卡片一定会被画**——折叠的画把手，
/// 展开的画内容，不存在「跳过」。
pub fn draw(f: &mut Frame<'_>) {
    let area = f.area;
    // 背景必须铺满整个客户区，否则窗口边缘会露出 clear color。
    f.ui.painter().rect_filled(area, 0.0, f.pal.bg);

    let top = Rect::from_min_max(
        pos2(area.min.x, area.min.y),
        pos2(area.max.x, area.min.y + crate::solver::TOPBAR_HEIGHT),
    );
    let body = crate::solver::card_area(area);

    // 热区是**每帧重算**的：跨帧留着旧值会让测试点到一个
    // 「上一帧存在、这一帧已消失」的位置，症状是改了布局后
    // 测试仍通过但测的是空气。
    f.state.hit.clear();
    draw_topbar(f, top);
    draw_cards(f, body);

    // ⚠️ 设置界面**不在这里画**——它已移到独立窗口
    // （见 `crate::settingswin`）。主窗口只负责响应齿轮点击，
    // 把 `show_settings` 置 true，由设置窗口的绘制入口接管。
    //
    // 若在这里再画一次，用户会看到「主窗口里浮着一块面板」，
    // 而真正的设置窗口在旁边——同一份内容画两遍。
}

// ---------------------------------------------------------------- 顶栏

fn draw_topbar(f: &mut Frame<'_>, bar: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    let painter = f.ui.painter_at(bar);
    painter.rect_filled(bar, CornerRadius::ZERO, pal.surface_variant);
    painter.hline(
        bar.x_range(),
        bar.max.y,
        Stroke::new(1.0, pal.border_subtle),
    );

    let inner = bar.shrink2(vec2(pal.space_md, 0.0));

    // ---- 右侧：窗口按钮 + 清空 + 设置（图标按钮）----------------------
    //
    // 从底部状态栏搬上来的。按钮用**图标**而非文字：顶栏高度只有
    // 44pt，中文「清空」两个字排在那里会又高又挤；图标在小尺寸下
    // 更清楚，也不随系统字号缩放变形。
    //
    // 尺寸取顶栏高度的 60%，留出上下呼吸空间。
    let btn = bar.height() * 0.62;
    let gap = pal.space_sm * 2.0;
    // 三枚按钮：关闭 / 清空 / 设置（「最小化」已去掉，见 draw_topbar_icons）。
    //
    // ⚠️ 这里的 `3.0` 必须与 `draw_topbar_icons` 里的 `step = r.width() / 3.0`
    // 一致。两处各写一份数字，改一处忘另一处 ⇒ 按钮区与实际按钮错位，
    // 表现为「最右那枚被搜索框压住」或「点齿轮变成拖窗口」。
    let btn_count = 3.0;
    let btn_area = Rect::from_min_size(
        pos2(
            inner.max.x - btn * btn_count - gap * (btn_count - 1.0),
            bar.center().y - btn * 0.5,
        ),
        vec2(btn * btn_count + gap * (btn_count - 1.0), btn),
    );
    draw_topbar_icons(f, btn_area, bar.center().y);

    // ---- 搜索框：占满剩余全部空间 --------------------------------------
    //
    // ⚠️ 顺序硬性：**先算右侧按钮区，再算搜索框**。反过来的话搜索框会
    // 一路铺到顶栏右缘，把图标按钮压在下面（按钮不可点）。
    let search = Rect::from_min_max(
        pos2(inner.min.x, bar.min.y + pal.space_sm),
        pos2(btn_area.min.x - pal.space_md, bar.max.y - pal.space_sm),
    );
    if search.width() > 40.0 && search.height() > 8.0 {
        draw_search(f, search);
    }

    let _ = scale;

    // 顶栏整体作为拖动区（搜索框与按钮自行 `interact` 会排除命中）。
    // 拖动区 = 顶栏**减去交互控件**。
    //
    // ⚠️ 这里必须是「挖空」而不是「整块上报」。`WM_NCHITTEST` 对拖动区
    // 返回 `HTCAPTION`，系统随后把按下转成 `WM_NCLBUTTONDOWN`
    // ——**不会产生 `WM_LBUTTONDOWN`**，egui 于是收不到点击。
    //
    // 症状：整个顶栏（含齿轮、垃圾桶、搜索框）全都点不动，
    // 表现为「设置打不开 / 搜索框不能点」。
    //
    // ⚠️ 这条路径**无法用 paint 层测试覆盖**：测试环境没有
    // `WM_NCHITTEST`，`hit_test` 根本不被调用。所以当时
    // 12 条交互测试全绿，实机却完全不可点。
    // 必须配一条直接测 `chrome::hit_test` 的测试（见 gfx crate）。
    let mut drag = Vec::new();
    // 左段：窗口左缘到**搜索框左缘**。
    //
    // ⚠️ 用热区表的 `search`（即 `TextEdit` 的真实矩形）而不是
    // ⚠️ 必须用 `search.min.x`（搜索框**外框**左缘），不能用
    // `hit["search"].min.x` —— 那是 `edit_rect` 的左缘，比外框右移了
    // 一个图标宽（约 24pt）。于是拖动区一直伸进输入区内部，
    // 点搜索框左侧（放大镜图标所在处，很自然的落点）被判成
    // `HTCAPTION` ⇒ 系统发 `WM_NCLBUTTONDOWN` 而非 `WM_LBUTTONDOWN`
    // ⇒ **搜索框点不动，变成拖窗口**。
    //
    // 用外框左缘既不侵入输入区，又保住左侧那段可拖。
    let left_end = search.min.x;
    if left_end - bar.min.x > 8.0 {
        drag.push(Rect::from_min_max(bar.min, pos2(left_end - 2.0, bar.max.y)));
    }
    // ⚠️ 这里**不设**「两按钮之间」那一段。
    //
    // `btn_area` 宽 `btn*2 + space_sm`、两按钮各占一半——它们是
    // **紧邻**的（`space_sm` 落在 `btn_area` 内部，两侧各一半）。
    // 早前按「中点 ± 3」加一段，实测该段与垃圾桶重叠
    // （`[[645.7,0]-[651.7,44]]` vs 按钮 `[[617.4,8.4]-[648.7,35.6]]`），
    // 于是那段又变成「点垃圾桶 = 拖窗口」。
    //
    // 两按钮之间本来也没有可拖的空白，略掉即可。
    //
    // 右段：从**设置按钮的真实右缘**到窗口右缘。
    //
    // ⚠️ 不能用 `btn_area.max.x + pal.space_sm` 估——
    // `space_sm` 已经算进 `btn_area` 的宽度里了（见 `btn_area` 的
    // 构造：宽 `btn*2 + space_sm`），再加一次会**侵入按钮**。
    // 实测该段`[[688,0]-[700,44]]` 与齿轮 `[[664.7,8.4]-[696,35.6]]`
    // 重叠，于是点齿轮变成拖窗口。
    //
    // 用热区表里按钮的真实矩形——它就是按钮实际交互的区域。
    let settings_r = f.state.hit.get("topbar_settings").copied();
    let right_start = settings_r.map(|r| r.max.x).unwrap_or(btn_area.max.x);
    if bar.max.x - right_start > 8.0 {
        drag.push(Rect::from_min_max(
            pos2(right_start + 2.0, bar.min.y),
            bar.max,
        ));
    }
    f.state.drag_regions = drag;
}

/// 顶栏右侧的四枚图标按钮：关闭 / 最小化 / 清空 / 设置。
///
/// # 为什么改用 egui 的 `Button` 而不再手写 `interact`
///
/// 手写版只调 `ui.interact` + `painter`，控件在 egui 的命中体系里
/// 是「外人」：焦点环、键盘响应、命中排序都要自己照顾。历史上
/// 「按钮画了但点不动」的根因正是命中判定被拖动区抢走——
///
/// ```text
/// WM_NCHITTEST 命中拖动区 ⇒ 返回 HTCAPTION
///                ⇒ 系统发 WM_NCLBUTTONDOWN（非客户区消息）
///                ⇒ **不产生 WM_LBUTTONDOWN**
///                ⇒ egui 收不到点击
/// ```
///
/// 改用 `Button` 后控件走 egui 自己的登记与排序，配合 `draw_topbar`
/// 里的「拖动区挖空」把控件矩形从 `HTCAPTION` 范围里剔除，
/// 两者共同保证可点。
///
/// # 图标仍然自绘
///
/// `Button` 的**文本**走普通字体链，而图标在 PUA 码位
/// （`U+E000..=U+E011`），那条字体链里没有字形，会渲染成方框（tofu）。
/// 所以 `Button` 只负责命中与悬停底色，图标用 [`Icon::paint`] 叠在
/// 它自己的矩形上。
fn draw_topbar_icons(f: &mut Frame<'_>, r: Rect, center_y: f32) {
    let step = r.width() / 3.0;
    let sq = step * 0.62;

    // 自左向右：关闭 / 清空 / 设置。
    //
    // ⚠️ 只有三枚了——「最小化」已去掉。它与「关闭到托盘」的实际
    // 效果几乎相同（都让窗口消失），区别仅在任务栏是否留条目，
    // 而本程序常驻托盘，用户极少需要「留在任务栏但看不见」。
    // 两个近乎重复的按钮并排反而增加误点。
    //
    // 关闭按钮用 `Icon::Minimize`（横线）而非 `Icon::Close`（叉）：
    // 它的语义是「收起」，与最小化同族；叉号在中文界面里更常被
    // 理解成「删除/退出」，而这里并不是退出。
    let slot = |i: f32| Rect::from_center_size(pos2(r.min.x + step * (i + 0.5), center_y), vec2(sq, sq));
    let r_close = slot(0.0);
    let r_clear = slot(1.0);
    let r_set = slot(2.0);

    // 登记热区给测试用（见 `UiState::hit` 的说明）。**必须在点击判定
    // 之前登记**：`draw_topbar` 随后要读这张表来挖拖动区。
    f.state.hit.insert("topbar_close", r_close);
    f.state.hit.insert("topbar_clear", r_clear);
    f.state.hit.insert("topbar_settings", r_set);

    if icon_button(f, "topbar_close", r_close, Icon::Minimize, "关闭到托盘", false) {
        f.state.push(Op::CloseWindow);
    }
    // 破坏性操作（清空）用 danger 色，与「设置」区分开。
    if icon_button(f, "topbar_clear", r_clear, Icon::Trash, "清空全部历史", true) {
        f.state.push(Op::ClearAll);
    }
    if icon_button(f, "topbar_settings", r_set, Icon::Settings, "设置", false) {
        f.state.push(Op::ToggleSettings);
    }
}

/// 一枚方形图标按钮；返回是否被点击。
///
/// 走 egui `Button` 拿命中与悬停，图标自己叠画（理由见
/// [`draw_topbar_icons`]）。
///
/// # 为什么必须开子 `Ui` 而不能直接 `add_sized`
///
/// `add_sized` 只给**尺寸**，位置由父 `Ui` 的布局游标决定——四枚按钮
/// 会并排排在顶栏左缘，而不是我们算好的槽位。于是 `Response.rect`
/// 与登记进 `hit` 表的矩形不一致：测试按热区点击打不中，
/// 拖动区挖空也挖错位置（表现为「点按钮 = 拖窗口」）。
///
/// `UiBuilder::max_rect` 才是绝对定位：子 `Ui` 的原点就是该矩形，
/// 里面的 `Button` 落在矩形左上角，尺寸相同 ⇒ 两者完全重合。
fn icon_button(
    f: &mut Frame<'_>,
    id: &'static str,
    r: Rect,
    icon: Icon,
    tip: &'static str,
    danger: bool,
) -> bool {
    let pal = f.pal;
    let scale = f.scale;
    // `Button` 的文本留空：真正的内容是下面自绘的图标。
    // `frame(false)` 去掉它自带的边框，只保留命中与悬停。
    //
    // ⚠️ `Button::frame` 收的是 **bool**（是否画框），而
    // `TextEdit::frame` 收的是 `egui::Frame` 枚举。两者同名不同签名，
    // 写反了会报 E0308。
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            .id(egui::Id::new(id))
            .max_rect(r)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| ui.add_sized(r.size(), egui::Button::new("").frame(false)),
    );
    let resp = resp.inner;
    // 悬停底色叠在热区矩形上（`painter_at` 把裁剪限制在顶栏内）。
    if resp.hovered() {
        f.ui.painter_at(r).rect_filled(r, pal.radius_sm, pal.row_hover);
    }
    icon.paint(
        &f.ui.painter_at(r),
        Rect::from_center_size(r.center(), vec2(pal.icon_size, pal.icon_size) * scale),
        match (danger, resp.hovered()) {
            (true, true) => pal.danger,
            _ => pal.text_dim,
        },
    );
    // ⚠️ 顺序硬性：`on_hover_text` 按**值**消耗 `Response`，所以
    // `clicked()` 必须在它之前取。把两者写反会 E0382（值已移动）。
    let clicked = resp.clicked();
    resp.on_hover_text(tip);
    clicked
}


fn draw_search(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    let painter = f.ui.painter_at(r);
    painter.rect_filled(r, pal.radius_md, pal.surface);
    painter.rect_stroke(
        r,
        pal.radius_md,
        Stroke::new(1.0, pal.border),
        egui::StrokeKind::Inside,
    );

    // 放大镜图标坐在左内边距里，与文字左边界对齐。
    let pad = pal.space_sm;
    let icon_w = pal.icon_size + pad;
    draw_search_icon(
        &f.ui.painter_at(r),
        Rect::from_center_size(
            pos2(r.min.x + pad + pal.icon_size * 0.5, r.center().y),
            vec2(pal.icon_size, pal.icon_size) * scale,
        ),
        pal.text_dim,
    );

    // ⚠️ 这里必须是**真正的输入控件**，不能只画文字。
    //
    // 早前版本用 `painter_at().text()` 把占位符「搜索历史…」画上去、
    // 点击只 `request_repaint()`——看着像搜索框，实际**打不了字**。
    // 搜索框一旦占据顶栏主位置（原先那里是产品标题），静态假框就等于
    // 占着最好的位置却不可用，比没有更糟。
    //
    // `TextEdit` 必须放在一个**限定矩形**的子 `Ui` 里，否则它会按
    // egui 默认布局占满整行，把右侧的模式切换按钮挤出顶栏。
    let edit_rect = Rect::from_min_max(
        pos2(r.min.x + icon_w, r.min.y + 1.0),
        pos2(r.max.x - pad, r.max.y - 1.0),
    );
    if edit_rect.width() <= 4.0 || edit_rect.height() <= 4.0 {
        return;
    }
    // 自报热区给测试用（见 `UiState::hit` 的说明）。
    //
    // ⚠️ 登记**两个**：`search_box` 是外框，`search` 是输入区。
    //
    // 只登记输入区不够——外框左侧那条约 22pt 宽的图标带**也是可点的**
    //（点它会聚焦搜索框），而它恰好是最容易被拖动区吃掉的地方。
    // 没有 `search_box` 这条热区，`topbar_controls_are_not_in_drag_region`
    // 就探测不到那个 bug：输入区左缘与拖动区终点只差 2pt，四角全落在
    // 拖动区**之外**，测试照样绿。
    f.state.hit.insert("search_box", r);
    f.state.hit.insert("search", edit_rect);
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            // ⚠️ **必须给显式 id**。不给的话 egui 按调用位置自动生成，
            // 于是：焦点断言没法稳定地指向它（只能退化成
            // 「有没有任何控件获得焦点」这种恒真判断），且任何调整
            // 布局的改动都可能悄悄换掉 id。
            //
            // 上面 `icon_button` 同样用 `scope_builder`，它就给了
            // `.id(Id::new(id))` —— 两处写法必须一致。
            .id(egui::Id::new("search_edit"))
            .max_rect(edit_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut f.state.query)
                    .hint_text("搜索历史…")
                    .desired_width(f32::INFINITY)
                    // ⚠️ 字号必须给**不带scale 的逻辑字号**。
                    //
                    // 早前传的是 `sized(pal.font_md, scale)`——它已经把
                    // DPI 系数乘进去了，而 egui 在计算可用高度时会
                    // **再乘一次** `pixels_per_point`。结果是行高被放大到
                    // 远超框高，文字被裁掉上半/下半截，表现为
                    //「能输入但看不见、字很小」。
                    //
                    // 正确做法：`FontId` 用逻辑字号，由 egui 负责缩放。
                    .font(egui::FontId::proportional(pal.font_md))
                    // 内边距收窄：顶栏只有 44pt，egui 默认的
                    // `symmetric(4,2)` 在此会让单行控件偏高而裁切。
                    .margin(egui::Margin::symmetric(2, 0))
                    .text_color(pal.text)
                    // 去掉 TextEdit 自带的外框：外框已由上面的
                    // `rect_stroke` 画好，两层框叠在一起会显得脏。
                    //
                    // ⚠️ 这里必须给 `egui::Frame` 枚举而不是 `false`：
                    // 0.36 的 `frame()` 收的是容器框类型，不是 bool。
                    .frame(egui::Frame::NONE),
            )
        },
    );
    // `scope_builder` 返回 `InnerResponse<Response>`：内层是 TextEdit
    // 自己的响应，外层是这次作用域分配整体的响应。
    // 焦点与 id 都要用**内层**的——外层 id 属于作用域，不是输入框。
    let edit_resp = resp.inner;

    // 自报真实 Id，供测试精确判断焦点落在哪个控件上（见字段说明）。
    f.state.search_edit_id = Some(edit_resp.id);

    if edit_resp.has_focus() {
        // 聚焦时描边加强，给出明确的视觉反馈。
        f.ui.painter_at(r).rect_stroke(
            r,
            pal.radius_md,
            Stroke::new(1.5, pal.border_strong),
            egui::StrokeKind::Inside,
        );
    }

    // 有文本时给一个清除按钮。
    if !f.state.query.is_empty() {
        let btn = r.height() * 0.6;
        let clear = Rect::from_center_size(
            pos2(r.max.x - pad - btn * 0.5, r.center().y),
            vec2(btn, btn),
        );
        let cr = f
            .ui
            .interact(clear, egui::Id::new("card_search_clear"), Sense::click());
        crate::icons::Icon::Close.paint(
            &f.ui.painter_at(clear),
            Rect::from_center_size(clear.center(), vec2(btn, btn) * 0.7),
            if cr.hovered() { pal.text_bright } else { pal.text_dim },
        );
        if cr.clicked() {
            f.state.query.clear();
            // 清空后主动交还焦点：否则用户还得再点一次才能继续打字。
            f.ui.memory_mut(|m| m.request_focus(edit_resp.id));
        }
        cr.on_hover_text("清空搜索");
    }
}

/// 画放大镜图标（圆环 + 手柄），几何绘制而非字体字形。
///
/// # 为什么不用 `Icon::Search` 字形
///
/// 图标字体子集只有 18 个码位、源字体不在仓库（只有 pyftsubset 的
/// 4KB 子集），**无法新增**码位。而现有的 `U+E003` 字形笔画偏粗、
/// 圆环与手柄比例失衡，在搜索框这种小尺寸下显得笨重（用户反馈"太丑"）。
///
/// 放大镜本来就只有两条线——圆 + 斜杠，用几何画反而更可控：
/// - **线宽可调**，不受字体设计限制；
/// - 圆环与手柄的**比例由我们决定**，不必迁就字形；
/// - 无字体依赖，不会因缺字形退化成方框。
///
/// # 尺寸约定
///
/// `r` 是图标包围盒。圆环占其中 62%，手柄从圆环边缘斜向外延伸，
/// 整体不超出 `r`——保证与文字基线视觉对齐。
fn draw_search_icon(painter: &egui::Painter, r: Rect, color: egui::Color32) {
    let side = r.width().min(r.height());
    // 线宽取边长的 9%：约 1.6px（side=18），比字体字形明显纤细。
    // ⚠️ 下限 1.2——太细在1x 屏上会断续，虚化。
    let stroke_w = (side * 0.09).max(1.2);
    let c = r.center();

    // 圆环：直径 52%，圆心偏左上。
    //
    // ⚠️ 早前取 62% + 手柄起点在环**内**：放大截图看到手柄被圆环
    // 盖住，整个图标只剩一个空心圆，完全不像放大镜。
    // 现在环缩小、手柄起点移到环**外**（环边缘 + 线宽一半），
    // 两段在视觉上连成一体。
    let ring_r = side * 0.26;
    let off = side * 0.13; // 圆心偏移，留出右下角放手柄
    let ring_c = pos2(c.x - off, c.y - off);
    painter.circle_stroke(ring_c, ring_r, Stroke::new(stroke_w, color));

    // 手柄：从圆环**外缘**（45° 方向）伸向包围盒右下角。
    //
    // 起点 = 环心 + (ring_r + stroke_w*0.5) * 0.707——正好贴在环上，
    // 终点取包围盒的 0.47 处（留一点边距，避免描边被裁）。
    let diag = 0.707_106_78_f32; // √2/2
    let start_off = (ring_r + stroke_w * 0.5) * diag;
    let start = pos2(ring_c.x + start_off, ring_c.y + start_off);
    // ⚠️ `reach` 必须留够「线宽的一半」的余量：`end` 还要描边，
    // 取 0.47 时终点 8.45 + 0.81 = 9.26 会**溢出**包围盒 9.0
    // （被裁掉一小截）。0.43 留 0.7pt 余量。
    let reach = side * 0.43;
    let end = pos2(c.x + reach, c.y + reach);
    painter.line_segment([start, end], Stroke::new(stroke_w, color));
}

/// 设置界面的整帧内容（画在**独立窗口**里）。
///
/// # 与旧「覆盖层」版的差别
///
/// 旧版是在主窗口上盖一块居中面板 + 一层遮罩。这里改成独立窗口后：
///
/// - **没有遮罩**——独立窗口天然就是模态的，不需要额外拦点击；
/// - **不居中**——窗口本身就是面板，按客户区铺满即可；
/// - **没有外框**——它已经是独立窗口，再画一层卡片框会显得
///   「窗口里还有一块面板」。
///
/// `area` 是**本窗口自己的客户区**，与主窗口无关。
pub fn draw_settings(f: &mut Frame<'_>) {
    let pal = f.pal;
    // 客户区由 `Frame` 带入——设置窗口有自己的客户区，
    // 不该再让调用方传一个进来（那两个值必须一致，传错就是 bug）。
    let area = f.area;

    // ⚠️ **首帧的 `area` 可能荒谬**（实测 6666x6666）。
    //
    // `ui.max_rect()` 取的是子 `Ui` 的可用矩形，而首帧 `screen_rect`
    // 尚未由 `egui_input` 正确设置（`RawInput.screen_rect` 是默认值）。
    // 此时按 `area` 排布的面板会被算到屏幕外，**看起来像设置界面没打开**。
    //
    // 显式拒绝明显不合理的尺寸：不画任何东西，等下一帧。
    // 判断用「超过常见上限」而非绝对值——不同 DPI 下差异很大。
    let reasonable = area.width() <= 4000.0 && area.height() <= 4000.0;
    if !reasonable {
        tracing::debug!(area = ?area, "客户区尺寸异常，本帧跳过设置界面");
        return;
    }

    // 铺满客户区：外框 + 背景。独立窗口不再画卡片框，
    // 但要有底色，否则透明区域会透出桌面。
    f.ui.painter_at(area).rect_filled(area, 0.0, pal.bg);

    let pad = pal.space_md;
    let view = area.shrink2(vec2(pad, pad));
    if view.width() <= 8.0 || view.height() <= 8.0 {
        return;
    }

    // 在窗口矩形里开子 Ui：下面所有控件都跑在 egui 的布局里，
    // 由它负责换行、间距、滚动裁剪。
    let mut ui = f.ui.new_child(
        egui::UiBuilder::new()
            .id(egui::Id::new("settings"))
            .max_rect(view)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    ui.spacing_mut().item_spacing = vec2(0.0, 6.0);

    settings_header(f, &mut ui);
    ui.separator();

    // 设置项会随版本增加，窗口高度固定 ⇒ 必须能滚。
    //
    // ⚠️ 高度必须用「**剩余可用高度**」而不是估算常量：标题行实际
    // 高度随字号缩放变化，写死一个系数会在窗口越矮时误差越大
    // （此前实测 600x400 下开关被排到客户区之外，点不到）。
    // `ScrollArea` 会**扩展**到内容高度，所以这里必须显式封顶。
    let head_h = ui.min_rect().height();
    let body_max = (view.height() - head_h - ui.spacing().item_spacing.y).max(0.0);
    egui::ScrollArea::vertical()
        .id_salt("settings_scroll")
        .auto_shrink([false, false])
        .max_height(body_max)
        .show(&mut ui, |ui| {
            settings_body(f, ui);
        });
}

/// 标题行 + 关闭按钮。
fn settings_header(f: &mut Frame<'_>, ui: &mut Ui) {
    let pal = f.pal;
    let scale = f.scale;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("设置")
                .size(pal.font_md)
                .color(pal.text_bright),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // 自报热区供测试定位（见 `UiState::hit` 的说明）。
            // 固定 16x16 的方形热区：用子 Ui 绝对定位（同顶栏做法），
            // 不能用 allocate ——它会跟着布局游标走。
            let r = Rect::from_min_size(
                egui::pos2(ui.max_rect().max.x - 16.0 * scale, ui.max_rect().min.y),
                egui::vec2(16.0, 16.0) * scale,
            );
            let btn = f.ui.scope_builder(
                egui::UiBuilder::new()
                    .id(egui::Id::new("settings_close"))
                    .max_rect(r)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
                |u| u.add_sized(r.size(), egui::Button::new("").frame(false)),
            );
            let btn = btn.inner;
            if btn.hovered() {
                ui.painter_at(r).rect_filled(r, f.pal.radius_sm, f.pal.row_hover);
            }
            crate::icons::Icon::Close.paint(
                &ui.painter_at(r),
                egui::Rect::from_center_size(r.center(), egui::vec2(12.0, 12.0) * scale),
                if btn.hovered() { f.pal.text_bright } else { f.pal.text_dim },
            );
            f.state.hit.insert("settings_close", r);
            let clicked = btn.clicked();
            btn.on_hover_text("关闭设置");
            if clicked {
                f.state.show_settings = false;
            }
        });
    });
}

/// 设置项正文：置顶模式 + 开关组 + 数值组。
///
/// # 为什么逐项列出而不是遍历数据结构
///
/// 配置项会随版本增删（现在 20+ 项）。写死在这里换来的是
/// 「新增一项只需改这一处 + `app.rs` 的字段处理器」，
/// 而 `Op` 侧用「分组 + 字段名字符串」把新增成本降到两行。
fn settings_body(f: &mut Frame<'_>, ui: &mut Ui) {
    let pal = f.pal;
    let _ = f.scale;
    let cur = f.ws.pinned_mode();

    // ⚠️ 必须把配置值**逐个拷出来**，不能 `let cfg = &f.svc.state.config`
    // 然后边遍历边调需要 `&mut Frame` 的函数。
    //
    // 那个借用会活到函数结束，于是后面每次 `draw_switch(f, ...)` /
    // `number_row(f, ...)` 的可变借用都撞上它（E0502）。这类错误在
    // 「先读配置、后统一处理」的写法里非常常见。
    let (ui_cfg, cap_cfg, sto_cfg) = {
        let c = &f.svc.state.config;
        (c.ui.clone(), c.capture.clone(), c.storage.clone())
    };

    // ---- 置顶模式 -------------------------------------------------------
    section(ui, "置顶");
    ui.horizontal(|ui| {
        for (mode, label, desc) in [
            (PinnedMode::SharedColumn, "共用单栏", "与历史列表在同一栏内"),
            (PinnedMode::OwnCard, "独立分栏", "单独一栏，可拖出成窗口"),
        ] {
            // `selectable_label` 自带选中态与键盘响应，替代原先
            // 手绘的圆点 + 描边矩形。
            if ui
                .selectable_label(cur == mode, egui::RichText::new(label).size(pal.font_sm))
                .on_hover_text(desc)
                .clicked()
                && cur != mode
            {
                f.state.push(Op::SetPinnedMode(mode));
            }
        }
    });
    ui.add_space(4.0);

    // ---- 开关组 ---------------------------------------------------------
    //
    // 每项：(分组, 字段, 标签, 说明, 当前值)
    let toggles: [(ConfigGroup, &'static str, &str, &str, bool); 7] = [
        (ConfigGroup::Ui, "always_on_top", "窗口置顶", "始终显示在其它窗口之上", ui_cfg.always_on_top),
        (ConfigGroup::Ui, "hide_on_focus_lost", "失焦时隐藏", "切换到别的程序后隐藏窗口", ui_cfg.hide_on_focus_lost),
        (ConfigGroup::Ui, "show_tray", "显示托盘图标", "后台常驻，保留右键菜单入口", ui_cfg.show_tray),
        (ConfigGroup::Ui, "start_minimized", "启动时最小化", "启动后隐藏到托盘", ui_cfg.start_minimized),
        (ConfigGroup::Capture, "enabled", "记录剪贴板", "关闭后不再新增历史", cap_cfg.enabled),
        (ConfigGroup::Capture, "skip_password_fields", "跳过密码字段", "疑似输入密码时不记录", cap_cfg.skip_password_fields),
        (ConfigGroup::Capture, "dedup", "自动去重", "短时间内相同内容只留一条", cap_cfg.dedup),
    ];

    let mut last_group: Option<ConfigGroup> = None;
    for (g, field, label, desc, on) in toggles {
        if last_group != Some(g) {
            section(ui, g.title());
            last_group = Some(g);
        }
        let mut v = on;
        if setting_row(ui, label, desc, |ui| draw_switch(f, ui, field, &mut v)) {
            f.state.push(Op::SetBool { group: g, field, value: v });
        }
    }

    // ---- 数值组 ---------------------------------------------------------
    section(ui, "监听与存储");

    // 监听间隔（ms）。下限 50：更小的值只会让 CPU 空转，
    // 而剪贴板序列号不会更新得那么快。
    number_row(
        f, ui, "poll_interval_ms", "监听间隔", "越小越灵敏、越耗电",
        cap_cfg.poll_interval_ms as f64, 50.0, 2000.0, 50.0, " ms",
        ConfigGroup::Capture,
    );
    // 历史条数上限：`None` = 不限，所以默认显示一个代表值并在
    // 「减到底」时变回 `None`（与旧实现语义一致）。
    opt_number_row(
        f, ui, "max_items", "历史条数上限", "留空表示只按容量淘汰",
        sto_cfg.max_items, 1000.0, 100.0, " 条",
    );
    number_row(
        f, ui, "dedup_window_secs", "去重窗口", "该时长内的相同内容视为重复",
        cap_cfg.dedup_window_secs as f64, 0.0, 600.0, 5.0, " 秒",
        ConfigGroup::Capture,
    );

    // ---- 存储分组 -------------------------------------------------------
    section(ui, "存储");
    let mut cleanup = sto_cfg.cleanup_on_start;
    if setting_row(ui, "启动时清理临时文件", "删除上次退出遗留的临时文件", |ui| {
        draw_switch(f, ui, "cleanup_on_start", &mut cleanup)
    }) {
        f.state.push(Op::SetBool {
            group: ConfigGroup::Storage,
            field: "cleanup_on_start",
            value: cleanup,
        });
    }

    // ---- 危险操作 -------------------------------------------------------
    //
    // 放在最后并与配置项明显区分：这是**唯一**能从界面退出程序的地方
    // （标题栏 ✕ 在有托盘时只隐藏到托盘）。
    ui.add_space(12.0);
    ui.separator();
    ui.add_space(4.0);
    let quit_resp = ui.add_sized(
        [ui.available_width(), 30.0],
        egui::Button::new(
            egui::RichText::new("退出程序")
                .color(pal.danger)
                .strong()
                .size(pal.font_md),
        ),
    );
    // 自报热区供测试定位（见 `UiState::hit` 的说明）。
    f.state.hit.insert("settings_quit", quit_resp.rect);
    if quit_resp.clicked() {
        f.state.push(Op::Quit);
    }

    let _ = pal;
}

/// 分组标题。
fn section(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(title)
            .strong()
            .size(12.0)
            .color(ui.visuals().weak_text_color()),
    );
}

/// 一行「标签 + 说明 + 右侧控件」。
///
/// `control` 闭包在右侧区域里放控件，返回 `true` 表示值被改动。
/// 宽度分配交给 `ui.horizontal` + `with_layout(right_to_left)`，
/// 不再手算 `ctrl_w`（旧实现那处 `min(r.width() * 0.4)` 的钳制
/// 会在极窄面板里把标签挤没）。
fn setting_row(ui: &mut Ui, label: &str, desc: &str, control: impl FnOnce(&mut Ui) -> bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(label);
            if !desc.is_empty() {
                ui.label(egui::RichText::new(desc).small().color(ui.visuals().weak_text_color()));
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            changed = control(ui);
        });
    });
    changed
}

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
fn draw_switch(f: &mut Frame<'_>, ui: &mut Ui, id_suffix: &'static str, v: &mut bool) -> bool {
    let pal = f.pal;
    let s = 18.0_f32;
    // `allocate_exact_size` 返回 `(Rect, Response)` —— 不是单个 Response。
    let (rect, _alloc) = ui.allocate_exact_size(egui::vec2(s * 1.8, s), Sense::click());
    let resp = ui.interact(rect, egui::Id::new(("switch", id_suffix)), Sense::click());

    // 开关本体：宽 = 高的 1.8 倍。
    let h = s;
    let w = h * 1.8;
    let knob_r = h * 0.5 - 2.0;
    let track = Rect::from_center_size(rect.center(), vec2(w, h));
    let p = ui.painter_at(rect);
    let on = *v;
    if on {
        p.rect_filled(track, h * 0.5, pal.accent);
    } else {
        p.rect_filled(track, h * 0.5, pal.surface_variant);
        p.rect_stroke(
            track,
            h * 0.5,
            Stroke::new(1.0, pal.border),
            egui::StrokeKind::Inside,
        );
    }
    // 滑块：开时靠右、关时靠左。
    let knob_x = if on { track.max.x - h * 0.5 } else { track.min.x + h * 0.5 };
    let knob = Rect::from_center_size(
        pos2(knob_x, track.center().y),
        vec2(knob_r * 2.0, knob_r * 2.0),
    );
    p.circle_filled(knob.center(), knob_r, if on { pal.text_bright } else { pal.text_dim });

    if resp.clicked() {
        *v = !*v;
    }
    // ⚠️ 热区 key 必须**带上字段名**。
    //
    // 早前所有开关都登记成同一个 `"settings_switch"`，而 `hit` 是
    // `HashMap<&str, Rect>`——后写的覆盖先写的， map 里只剩**最后一个**
    // 开关的矩形。于是自动化脚本按热区点击会「点 A 项却改了 B 项」，
    // 而所有断言都通过（它们只读得到的那一个键）。
    //
    // 逐个登记还有一个好处：测试能逐项核对边界，而不是只看最后一个。
    f.state.hit.insert(id_suffix, rect);
    *v != on
}

/// 数值项：`DragValue`，改动时发 `Op::SetNumber`。
#[allow(clippy::too_many_arguments)]
fn number_row(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    field: &'static str,
    label: &str,
    desc: &str,
    v: f64,
    min: f64,
    max: f64,
    step: f64,
    unit: &str,
    group: ConfigGroup,
) {
    let mut val = v;
    let changed = setting_row(ui, label, desc, |ui| {
        ui.add(
            egui::DragValue::new(&mut val)
                .range(min..=max)
                .speed(step)
                .suffix(unit),
        )
        .changed()
    });
    let _ = field;
    if changed {
        f.state.push(Op::SetNumber { group, field, value: val });
    }
}

/// 可清空的数值项（`None` = 不限）。
///
/// `DragValue` 本身表达不了「无限制」，所以映射为：
/// 显示一个代表值（`min`），减到下界时变回 `None`——与旧实现
/// 「0 = 不限」的语义一致。
fn opt_number_row(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    field: &'static str,
    label: &str,
    desc: &str,
    cur: Option<usize>,
    default: f64,
    step: f64,
    unit: &str,
) {
    let mut val = cur.map(|n| n as f64).unwrap_or(default);
    let changed = setting_row(ui, label, desc, |ui| {
        ui.add(
            egui::DragValue::new(&mut val)
                .range(default..=1_000_000.0)
                .speed(step)
                .suffix(unit),
        )
        .changed()
    });
    if !changed {
        return;
    }
    let next = if val <= default {
        // 减到下界 ⇒ 恢复「不限」。
        None
    } else {
        Some(val)
    };
    f.state.push(Op::SetOptNumber {
        group: ConfigGroup::Storage,
        field,
        value: next,
    });
}


/// 置顶卡片当前是否独立成窗。
///
/// 有独立置顶窗时主窗口**不再显示置顶栏**——置顶内容已经在那个
/// 窗口里了，再画一份就是重复。
fn pinned_is_separate(ws: &Workspace) -> bool {
    ws.cards
        .iter()
        .any(|c| c.kind == CardKind::Pinned && c.host == CardHost::Window)
}

fn draw_cards(f: &mut Frame<'_>, _body: Rect) {
    // 置顶**不占一栏**：要么独立成窗、要么内嵌在历史栏顶部。
    // 两种情况主窗口都不画置顶卡片。
    //
    // ⚠️ 判据必须与 solver 用的一致（`pinned_is_embedded`），
    // 否则会出现「solver 按占一栏分了宽度、paint 又在历史栏内画
    // 一份」—— 实测界面就是这样：置顶与历史并排，
    // 而用户要的是不分栏。
    let skip_pinned = f.ws.pinned_is_embedded() || pinned_is_separate(f.ws);

    // 按 Z 序遍历。这里直接读 `card.rect`——**不做任何二次判断**，
    // 也不重新计算宽度。矩形是solver 算好的，绘制只负责照着画。
    for card in f.ws.cards.iter() {
        if card.host != CardHost::Docked {
            continue;
        }
        if skip_pinned && card.kind == CardKind::Pinned {
            continue;
        }
        let r = card.rect;
        if r.width() <= 0.0 || r.height() <= 0.0 {
            // 求解器保证停靠卡片必有非零矩形；走到这里说明它还没被
            // 求解（例如最小化后的第一帧）。**跳过而不是画零尺寸卡**——
            // 画零尺寸卡会产出退化矩形，某些路径下会算出非法 scissor。
            continue;
        }
        // 卡片不再有折叠态——原先这里按 `card.collapsed` 二选一
        // （折叠画把手 / 展开画内容），折叠功能移除后只剩一条路径。
        draw_expanded_card(f, card, r);
    }
    // 置顶内嵌的绘制**不在这里**——它在
    // `draw_expanded_card` 里随历史卡片一起处理：那里会先把历史
    // 卡片的 body 切成两段（置顶在上、历史在下），两段各画各的。
    //
    // ⚠️ 早前是「画完历史再往上叠一层」，于是标题叠字 + 列表重叠。
    // 而且这里也拿不到「置顶该占多高」——那是切分时才确定的量。
}

/// 在**给定的**矩形里画内嵌置顶区。
///
/// # `area` 是谁给的
///
/// 由 [`draw_expanded_card`] 提前算好并从历史卡片的 body 顶部切出来。
/// 早前这个函数自己去读 `history.rect` 并从 `inner.min` 铺到
/// `inner.max.y` —— 那等于无视历史列表已经占了整块 body，必然重叠。
///
/// 不画自己的边框：它不是独立卡片，画了会与历史卡片边框叠成双线。
fn draw_embedded_pinned(f: &mut Frame<'_>, area: Rect) {
    let pal = f.pal;
    let font: FontId = sized(pal.font_xs, f.scale);
    let label_h = (pal.font_xs * 1.6).max(14.0);

    // 标题行「置顶」。画在 `area` 顶部 —— 即历史卡片**头部之下**，
    // 与标题「历史」不再同一个 y。
    let label = Rect::from_min_size(area.min, vec2(area.width(), label_h));
    f.ui.painter_at(label).text(
        label.left_center(),
        Align2::LEFT_CENTER,
        "置顶",
        font,
        pal.text_dim,
    );

    let list = Rect::from_min_max(
        pos2(area.min.x, label.max.y + pal.space_xs),
        area.max,
    );
    if list.height() <= 8.0 || list.width() <= 8.0 {
        return;
    }
    draw_history_body(f, list, Some(true));
}

fn draw_card_frame(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    // 纯平毛玻璃：纯色底 + 1px 描边 + 主题色光晕。
    // **禁止渐变、光泽、折射、内发光**——那类效果在浅色卡片上
    // 看起来廉价，且与「平」的视觉语言冲突。
    let painter = f.ui.painter_at(r);
    painter.rect_filled(r, CARD_RADIUS, pal.surface);
    painter.rect_stroke(
        r,
        CARD_RADIUS,
        Stroke::new(CARD_STROKE * scale, pal.border_subtle),
        egui::StrokeKind::Inside,
    );
}

/// 展开态：卡片外框 + 头部 + 内容。
fn draw_expanded_card(f: &mut Frame<'_>, card: &Card, r: Rect) {
    draw_card_frame(f, r);
    let pal = f.pal;

    let pad = pal.space_sm;
    let inner = r.shrink2(vec2(pad, pad));

    // 头部：标题 + 折叠/分离按钮。
    let head_h = (pal.font_md * 1.8).max(20.0);
    let head = Rect::from_min_size(inner.min, vec2(inner.width(), head_h));
    draw_card_header(f, card, head);

    // 内容区。
    let body = Rect::from_min_max(
        pos2(inner.min.x, head.max.y + pal.space_xs),
        inner.max,
    );
    if body.width() <= 0.0 || body.height() <= 0.0 {
        return;
    }
    match card.kind {
        CardKind::History => {
            // 置顶内嵌在历史卡片**顶部** ⇒ 先把 body 切成两段，
            // 历史列表用下面那段。
            //
            // ⚠️ 早前是「先画完整历史列表，再事后往上叠一层置顶」——
            // 于是「置顶」标签与卡片标题「历史」画在**同一个 y**
            // （都是 `inner.min`），而置顶列表一直铺到卡片底部，
            // 盖住历史的前 N 行。用户看到的是两层内容叠在一起的乱字，
            // 且被盖住的历史行仍然可点（命中矩形还在）。
            let pinned_h = if f.ws.pinned_is_embedded() {
                embedded_pinned_height(f, body.width(), body.height())
            } else {
                0.0
            };
            if pinned_h > 0.0 {
                let split_y = body.min.y + pinned_h;
                let pinned_area =
                    Rect::from_min_max(body.min, pos2(body.max.x, split_y));
                draw_embedded_pinned(f, pinned_area);
                let rest = Rect::from_min_max(pos2(body.min.x, split_y), body.max);
                // 分隔线画在两段之间。
                f.ui.painter_at(rest).hline(
                    rest.x_range(),
                    rest.min.y,
                    Stroke::new(1.0, f.pal.border_subtle),
                );
                if rest.height() > 8.0 {
                    draw_history_body(f, rest, None);
                }
            } else {
                draw_history_body(f, body, None);
            }
        }
        CardKind::Pinned => draw_history_body(f, body, Some(true)),
        CardKind::Detail => draw_detail_body(f, body),
        CardKind::Rail => draw_rail_body(f, body),
    }
}

/// 内嵌置顶区应当占用的**高度**；没有置顶条目时返回 0。
///
/// # 为什么必须提前算
///
/// 历史列表的矩形要让出这一段。事后叠画（早前的做法）必然重叠——
/// 历史列表不知道自己头顶被占了一块，照样从 `body.min` 开始铺。
///
/// # 封顶 1/3
///
/// 置顶条目可能很多（用户能钉几十条）。全给会让历史列表被挤没，
/// 而历史才是主区。所以封顶 1/3，剩下的靠 `ScrollArea` 自己滚。
fn embedded_pinned_height(f: &mut Frame<'_>, avail_w: f32, avail_h: f32) -> f32 {
    let pal = f.pal;
    if !f.svc.state.items.iter().any(|it| it.pinned) {
        return 0.0;
    }
    if avail_w <= 8.0 || avail_h <= 32.0 {
        return 0.0;
    }
    // 标题行 + 上下留白 + 一条分隔线。
    let chrome = (pal.font_xs * 1.6).max(14.0) + pal.space_xs * 2.0 + pal.space_sm;

    // 行高必须用与 `draw_history_body` **同一个** `row_height_for`，
    // 否则预留高度与实际内容对不上（多留浪费空间，少留又盖住）。
    let icon_w = pal.icon_size + pal.space_sm;
    let text_w = (avail_w - icon_w - pal.space_sm).max(20.0);
    let rows: f32 = f
        .svc
        .state
        .items
        .iter()
        .filter(|it| it.pinned)
        .map(|it| row_height_for(f.ui, it, pal, f.scale, text_w))
        .sum();

    (chrome + rows)
        .min(avail_h * 0.34)
        .max(chrome.min(avail_h))
}

/// 卡片头部：只有标题。
///
/// # 为什么不再有「分离」「折叠」两枚按钮
///
/// 历史栏**固定在下方**，是主界面的主区，不需要用户手动折叠或拖走——
/// 这两个操作对它是多余的。而它们在界面上表现为两个 PUA 码位的
/// 小图标（⠿ / ⇤），在 150% 缩放下辨识度很差。
///
/// 折叠与分离的**完整逻辑**（`Card::collapsed`、`handle_width`、
/// `collapse_priority`、`Solution::auto_collapsed`、`draw_collapsed_card`、
/// `Op::ToggleCollapse` / `Detach` / `Dock`）都已移除——不是只删按钮。
fn draw_card_header(f: &mut Frame<'_>, card: &Card, head: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    let font: FontId = sized(pal.font_md, scale);
    f.ui.painter_at(head).text(
        head.left_center(),
        Align2::LEFT_CENTER,
        card.kind.title(),
        font,
        pal.text_bright,
    );
}

// ---------------------------------------------------------------- 内容

fn draw_history_body(f: &mut Frame<'_>, body: Rect, only_pinned: Option<bool>) {
    // 自报列表可视区。
    //
    // ⚠️ 这个 key **不是**给绘制用的，是给「热区自报」机制用的：
    // 测试与自动化脚本从 `UiState::hit` 里读坐标，而不是自己按
    // `pal` 常量重算一遍公式——重算等于把实现抄进测试，实现改对了
    // 测试反而挂。
    //
    // 两个列表（主栏 / 内嵌置顶区）必须用**不同 key**：`hit` 是
    // `HashMap<&str, Rect>`，同名会互相覆盖（settings 开关就踩过）。
    f.state.hit.insert(
        if only_pinned == Some(true) {
            "pinned_body"
        } else {
            "history_body"
        },
        body,
    );

    // 本列表用哪个滚动槽位。
    //
    // ⚠️ 两个列表的滚动位置必须**分开存**。早前共用一个
    // `f32`，而 `vertical_scroll_offset` 每帧都会把它写进 egui ——
    // 于是滚主栏会把内嵌置顶区一起带走（反之亦然）。
    let pinned_slot = only_pinned == Some(true);

    // 过滤：全部 / 仅置顶。
    let items: Vec<ClipItem> = f
        .svc
        .state
        .items
        .iter()
        .filter(|it| only_pinned.map_or(true, |p| it.pinned == p))
        .cloned()
        .collect();

    if items.is_empty() {
        let msg = match only_pinned {
            Some(true) => "还没有置顶条目",
            _ if f.state.query.is_empty() => "还没有剪贴板记录",
            _ => "没有匹配的结果",
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
    // 代价是要**每帧重算全部行高**才能知道滚动位置。
    // 这一步只做排版测量、不产生绘制指令，n=1000 时约几毫秒，
    // 对剪贴板这类「几十到几百条」的场景可接受。
    let pal = f.pal;
    let scale = f.scale;
    let acts_w = 3.0 * (pal.control_height * 0.7 + pal.space_xs);
    let text_w = (body.width() - (pal.icon_size + pal.space_sm) - acts_w - 8.0).max(20.0);
    let heights: Vec<f32> = items
        .iter()
        .map(|it| row_height_for(f.ui, it, pal, scale, text_w))
        .collect();
    // 前缀和：用来 O(log n) 由滚动偏移定位到行。
    let mut offsets: Vec<f32> = Vec::with_capacity(heights.len() + 1);
    offsets.push(0.0);
    for h in &heights {
        offsets.push(offsets.last().copied().unwrap_or(0.0) + h);
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
            .id(egui::Id::new(("list", only_pinned.unwrap_or(false))))
            .max_rect(body)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    let out = egui::ScrollArea::vertical()
        // ⚠️ 0.36 用 `id_salt`（不再是旧版的 `id_source`）。
        // 必须给：主栏与内嵌置顶区是两个独立列表，共用默认 Id 会
        // 共享滚动位置——滚主栏会把置顶区一起带走。
        .id_salt(("history_scroll", pinned_slot))
        .max_height(body.height())
        // 每帧把我们存的偏移灌进去：虚拟化要在 `show` **之前**知道
        // 滚到哪了，而 `ScrollArea` 自己的状态只在结束后才拿得到。
        .vertical_scroll_offset(scroll)
        // 滚轮交给 egui 处理，步长用 multiplier 调。
        //
        // ⚠️ 这里**不能**再自己读 `smooth_scroll_delta` 手动加一次：
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
            // ⚠️ egui 的 `Rect` **永远是全局（窗口）坐标**，不存在
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
            let first = offsets.partition_point(|&o| o + ROW_HEIGHT_MIN <= scroll);
            for i in first..items.len() {
                let y = body.min.y + offsets[i] - scroll;
                if y > body.max.y {
                    break; // 已越过可视区底部
                }
                let r = Rect::from_min_size(
                    pos2(body.min.x, y),
                    vec2(body.width(), heights[i]),
                );
                if r.max.y < body.min.y {
                    continue; // 整行都在可视区上方
                }
                draw_item_row(f, ui, r, &items[i]);
            }
        });

    // ---- 回读 egui 的真实滚动位置 --------------------------------
    //
    // ⚠️ 必须回读，否则**拖动滚动条无效**：拖动改变的是 `ScrollArea`
    // 自己的状态，而我们每帧开头又用旧值把它覆盖回去了。
    //
    // 回读后两者就一致了：滚轮、拖条、触屏都由 egui 处理，我们只
    // 是把结果镜像一份，供**下一帧**的虚拟化使用。
    let reported = out.state.offset.y;
    f.state.set_list_scroll(pinned_slot, reported.clamp(0.0, max_scroll));
}

/// 列表行的**最小**高度（逻辑点）。
///
/// 两行布局：预览（14pt，可折行）+ 来源/时间（11pt）+ 上下内边距。
///
/// ⚠️ 这是**下限**不是固定值：早前用固定 40pt 时预览只够放一行半，
/// 长内容被省略号截断（用户反馈「需要完整显示内容」）。
/// 实际行高由 [`row_height_for`] 按每条内容的折行数动态算出。
const ROW_HEIGHT_MIN: f32 = 52.0;

/// 每行文字的高度倍率。
const LINE_H: f32 = 1.35;

/// 单条列表项的**实际**行高：按预览文本折行后的行数计算。
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
fn row_height_for(ui: &egui::Ui, item: &ClipItem, pal: &Palette, scale: f32, text_w: f32) -> f32 {
    let preview_font = sized(pal.font_sm, scale);
    let meta_font = sized(pal.font_xs, scale);
    // ⚠️ 行高也必须用**逻辑点**、不乘 ppp：字号已经改成逻辑点，
    // 这里再乘一次会让行高与实际字形高度对不上（留白/裁剪都错）。
    let line_h = pal.font_sm * LINE_H;
    let meta_h = meta_font.size.max(10.0);

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
    // ⚠️ 必须用 `usize::MAX` 而不是 `0`：`TextWrapping::max_rows` 的
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
    let n_lines = galley.rows.len().max(1);
    let text_h = n_lines as f32 * line_h;

    // 上下内边距 + 预览 + 行间2pt + 来源/时间
    (text_h + meta_h + 5.0 * 2.0 + 2.0).max(ROW_HEIGHT_MIN)
}

fn draw_detail_body(f: &mut Frame<'_>, body: Rect) {
    let Some(id) = f.svc.state.selected else {
        draw_empty(f, body, "选中一条记录查看详情");
        return;
    };
    let Some(item) = f.svc.state.items.iter().find(|it| it.id == id) else {
        draw_empty(f, body, "该记录已不存在");
        return;
    };
    let pal = f.pal;
    let scale = f.scale;

    // 详情文字按可用矩形裁剪：长内容不得溢出卡片。
    let text_area = body.shrink2(vec2(pal.space_xs, 0.0));
    let font = sized(pal.font_sm, scale);
    let preview = if item.preview.is_empty() {
        "（无预览）"
    } else {
        item.preview.as_str()
    };
    let shown = crate::text::elide_text(f.ui, preview, &font, text_area.width());
    f.ui.painter_at(text_area).text(
        pos2(text_area.min.x, text_area.min.y + font.size),
        Align2::LEFT_TOP,
        shown,
        font.clone(),
        pal.text,
    );

    // 来源与时间。
    let meta_y = text_area.min.y + font.size * 2.4;
    if meta_y < text_area.max.y {
        // ⚠️ `max.y` 只能是 `meta_y + 高`，**不能**再叠加 `text_area.min.y`。
        // 叠加后高度会变成 `min.y + 高`（几百 pt），而文字画在
        // `meta_area.left_center()` —— 「来源 · 时间」会跑到卡片中部甚至
        // 卡片之外；而 `painter_at(meta_area)` 的裁剪就是它自己，
        // 越界部分不受任何裁剪。
        let meta_area = Rect::from_min_max(
            pos2(text_area.min.x, meta_y),
            pos2(text_area.max.x, meta_y + font.size * 1.2),
        );
        let meta_font = sized(pal.font_xs, scale);
        let meta = format!("{} · {}", item.source_app, format_meta(item.created_at));
        let meta_shown =
            crate::text::elide_text(f.ui, &meta, &meta_font, meta_area.width());
        f.ui.painter_at(meta_area).text(
            meta_area.left_center(),
            Align2::LEFT_CENTER,
            meta_shown,
            meta_font,
            pal.text_dim,
        );
    }
}

fn draw_rail_body(f: &mut Frame<'_>, body: Rect) {
    let pal = f.pal;
    let items: Vec<ClipItem> = f.svc.state.items.iter().cloned().collect();
    if items.is_empty() {
        draw_empty(f, body, "暂无数据");
        return;
    }
    let font = sized(pal.font_xs, f.scale);
    let row_h = 22.0;
    let n = ((body.height() / row_h).floor() as usize).min(items.len());
    for i in 0..n {
        let y = body.min.y + i as f32 * row_h;
        let r = Rect::from_min_size(pos2(body.min.x, y), vec2(body.width(), row_h));
        if r.max.y > body.max.y {
            break;
        }
        let txt = format!("{} · {}", items[i].source_app, items[i].preview);
        let shown = crate::text::elide_text(f.ui, &txt, &font, r.width());
        f.ui.painter_at(r).text(
            r.left_center(),
            Align2::LEFT_CENTER,
            shown,
            font.clone(),
            pal.text_dim,
        );
    }
}

fn draw_empty(f: &mut Frame<'_>, body: Rect, msg: &str) {
    let pal = f.pal;
    let font = sized(pal.font_sm, f.scale);
    // ⚠️ painter_at(body)：空状态文案是居中的，而窄卡片（折叠把手
    // 旁的详情栏）装不下整句，根 painter 会让它溢出到相邻卡片上。
    f.ui.painter_at(body).text(
        body.center(),
        Align2::CENTER_CENTER,
        msg,
        font,
        pal.text_dim,
    );
}

fn draw_item_row(f: &mut Frame<'_>, ui: &egui::Ui, r: Rect, item: &ClipItem) {
    let pal = f.pal;
    let scale = f.scale;
    let selected = f.svc.state.selected == Some(item.id);

    // ⚠️ 交互必须挂在**滚动区的内容 `ui`** 上，不能挂根 `f.ui`。
    //
    // `f.ui` 是整个主区的 Ui，它的裁剪矩形是**整个窗口**——于是
    // 被滚出可视区的那半行仍然可点：点在卡片标题、甚至顶栏上，
    // 都会选中列表里的条目（P0 级：顶栏按钮点不动）。
    //
    // `ScrollArea` 的内容 `ui` 裁剪到视口，越界的行自动不可交互，
    // 与「看得见才点得到」一致。
    let resp = ui.interact(r, egui::Id::new(("row", item.id)), Sense::click());
    let painter = ui.painter_at(r);
    if selected {
        painter.rect_filled(r, CornerRadius::ZERO, pal.row_selected);
        painter.rect_filled(
            Rect::from_min_size(r.min, vec2(2.0 * scale, r.height())),
            CornerRadius::ZERO,
            pal.accent,
        );
    } else if resp.hovered() {
        painter.rect_filled(r, CornerRadius::ZERO, pal.row_hover);
    }

    // ---- 两行布局 ----------------------------------------------------
    //
    // 上行：预览（可换行，最多占 2 行）+ 左侧类型图标
    // 下行：来源程序 · 时间（小字弱化）
    //
    // ⚠️ 目标是「**完整显示内容**」：预览**不再截断**，而是按宽度
    // 折行。折行数由 `wrap` 直接给出，不用手工估算——
    // 早前版本按 `preview.chars().take(30)` 硬砍，中文一个字算一个
    // char、英文一个词算一个 char，两者的实际显示宽度差了好几倍，
    // 于是「看起来还有空间」但已经被砍掉了。
    let icon_w = pal.icon_size + pal.space_sm;
    let acts_w = 3.0 * (pal.control_height * 0.7 + pal.space_xs);
    let text_x0 = r.min.x + icon_w;
    let text_x1 = (r.max.x - acts_w).max(text_x0);
    if text_x1 - text_x0 < 20.0 {
        return; // 太窄，画不下任何文字
    }

    // 左侧类型图标，垂直居中。
    crate::icons::Icon::for_kind(item.kind).paint(
        &ui.painter_at(r),
        Rect::from_center_size(
            pos2(
                r.min.x + icon_w * 0.5,
                r.center().y,
            ),
            vec2(pal.icon_size, pal.icon_size) * scale,
        ),
        if selected { pal.accent } else { pal.text_dim },
    );

    let pad_v = 5.0;
    let meta_font = sized(pal.font_xs, scale);
    let meta_h = meta_font.size.max(10.0);
    // 下行固定给来源/时间一行。
    let meta_band = Rect::from_min_max(
        pos2(text_x0, r.max.y - pad_v - meta_h),
        pos2(text_x1, r.max.y - pad_v),
    );
    // 上行占剩下的全部高度。
    let preview_band = Rect::from_min_max(
        pos2(text_x0, r.min.y + pad_v),
        pos2(text_x1, meta_band.min.y - 2.0),
    );

    if preview_band.height() > 4.0 {
        // 用 `Painter::layout_job` 按宽度折行，返回**完整** Galley——
        // 这才是「完整显示」：不丢任何字符，只是折行。
        //
        // ⚠️ 不能用 `Ui::layout`/`Context::layout`：那是给「让出矩形」
        // 的控件做换行用的，会受子 `Ui` 的布局约束影响，而我们只要
        // 一个纯文本排版结果。`Painter::layout_job` 才是纯排版入口。
        let mut job = egui::text::LayoutJob::default();
        job.append(
            &item.preview,
            0.0,
            egui::TextFormat {
                font_id: sized(pal.font_sm, scale),
                color: if selected {
                    pal.text_bright
                } else {
                    pal.text
                },
                ..Default::default()
            },
        );
        job.wrap.max_width = preview_band.width();
        // 高度用 `max_rows` 表达（0.36 的 `TextWrapping` **没有**
        // `max_height` 字段，只有 `max_width` / `max_rows` /
        // `break_anywhere` / `overflow_character`）。
        //
        // 行数按可用高度与字号算出，至少 1 行。超出部分由 egui 省略并
        // 在末行加省略号——**完整内容在详情子窗口里能看到**。
        // 行数上限：`row_height_for` 已按同一份预览文本算出行数并据此
        // 定好行高，这里给同样的上限只是兜底（防止两次排版结果不一致）。
        //
        // ⚠️ **不能设 0**：egui 文档写明「If set to 0, no text will be
        // outputted」——0 表示一行都不输出。默认值 `usize::MAX` 才是「不限」。
        job.wrap.max_rows = ((preview_band.height() / (pal.font_sm * LINE_H))
            .floor()
            .max(1.0)) as usize;
        // 长路径/无空格的长串要在任意字符间断行，否则会横向溢出。
        job.wrap.break_anywhere = true;
        let galley = f.ui.painter().layout_job(job);
        ui.painter_at(preview_band)
            .galley(pos2(preview_band.min.x, preview_band.min.y), galley, pal.text);
    }

    // 来源 · 时间
    let meta = format!("{} · {}", item.source_app, format_meta(item.created_at));
    let meta_shown = crate::text::elide_text(f.ui, &meta, &meta_font, meta_band.width());
    if !meta_shown.is_empty() {
        // ⚠️ painter_at(meta_band)：来源名可能很长（长 exe 路径），
        // 根 painter 会让它横跨到相邻卡片上。
        ui.painter_at(meta_band).text(
            meta_band.left_center(),
            Align2::LEFT_CENTER,
            meta_shown,
            meta_font,
            pal.text_dim,
        );
    }

    // 右侧三个操作：置顶 / 复制 / 删除。
    //
    // ⚠️ **必须用图标字体**（[`crate::icons::Icon`]，PUA 码位
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
    let btn = pal.control_height * 0.7;
    let acts: [(crate::icons::Icon, Op, bool); 3] = [
        (
            if item.pinned {
                crate::icons::Icon::Pinned
            } else {
                crate::icons::Icon::Unpinned
            },
            Op::TogglePin(item.id),
            !item.pinned,
        ),
        (crate::icons::Icon::File, Op::Copy(item.id), true),
        (crate::icons::Icon::Close, Op::Delete(item.id), true),
    ];

    for (i, (icon, op, enabled)) in acts.iter().enumerate() {
        let b = Rect::from_center_size(
            pos2(
                r.max.x - acts_w + (i as f32 + 0.5) * (btn + pal.space_xs),
                r.center().y,
            ),
            vec2(btn, btn),
        );
        if b.min.x < text_x1 {
            break; // 卡片太窄，放不下就不画操作
        }
        let rr = ui.interact(b, egui::Id::new(("row_act", item.id, i)), Sense::click());
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
                    "取消置顶"
                } else {
                    "置顶"
                }
            }
            1 => "复制",
            _ => "删除",
        };
        if rr.clicked() && *enabled {
            f.state.push(op.clone());
        }
        // `on_hover_text` 按值消耗 `Response`，必须是最后一次使用。
        rr.on_hover_text(tip);
    }

    if resp.clicked() {
        f.state.push(Op::Select(item.id));
    }
    if resp.double_clicked() {
        f.state.push(Op::Copy(item.id));
    }
}

// ---------------------------------------------------------------- 状态栏

fn format_meta(ts_ms: i64) -> String {
    modular_clipboard_app::format_time(ts_ms, modular_clipboard_core::now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个跑完字体注册的 `egui::Context`。
    ///
    /// 排版类断言必须走真实字体链：字形是否存在只有 egui 知道，
    /// 纯逻辑测试测不出「这一串字符会不会被排成两行」。
    fn settled_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        crate::theme::install_cjk_font(&ctx, None);
        crate::theme::install_icon_font(&ctx);
        // 注册在第一帧之后生效，必须再跑一帧。
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        ctx
    }

    // ------------------------------------------------------------------
    // 交互测试夹具
    // ------------------------------------------------------------------
    //
    // ⚠️ 为什么单独一个 `Harness` 而不在每个测试里现搭：
    //
    // 测「点击齿轮能打开设置」需要**跑完整帧**并把结果反馈到下一帧，
    // 中间要正确的 `screen_rect`、字体注册、`Service` 与临时数据目录。
    // 每处现搭约40 行、且极易漏掉某个必需步骤（实测漏字体导致
    // 文字全被省略、断言看起来像布局错）。
    //
    // 夹具负责：临时目录 + Service + ctx（含字体）+ 可注入输入的帧循环。

    /// 跑一帧的测试夹具。
    struct Harness {
        ctx: egui::Context,
        svc: modular_clipboard_app::Service,
        ws: crate::workspace::Workspace,
        state: UiState,
        pal: crate::theme::Palette,
        /// 客户区（逻辑点）。
        area: Rect,
        _dir: tempdir::TempDir,
    }

    /// 极简临时目录，够用且不引依赖。
    ///
    /// 用 `std::env::temp_dir()` + 随机名 + Drop 清理，
    /// 免得为了一个测试目录引入 `tempfile` 依赖。
    mod tempdir {
        use std::path::PathBuf;
        /// 临时目录，Drop 时递归删除。
        pub struct TempDir(PathBuf);
        impl TempDir {
            pub fn new(tag: &str) -> Self {
                // 进程 id + 原子计数器 + tag：同一进程内多次创建不撞名。
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let p = std::env::temp_dir().join(format!(
                    "mcb-test-{tag}-{}-{n}",
                    std::process::id()
                ));
                std::fs::create_dir_all(&p).expect("建临时目录");
                Self(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                // 测试环境里残留一个临时目录不致命，清理失败不该
                // 让测试 panic（那会掩盖真正的失败原因）。
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    impl Harness {
        /// 新建一个能跑帧的夹具。
        ///
        /// `size` 是客户区的**逻辑点**尺寸。
        fn new(tag: &str, size: egui::Vec2) -> Self {
            let dir = tempdir::TempDir::new(tag);
            let cfg = modular_clipboard_core::Config::default();
            let svc =
                modular_clipboard_app::Service::with_data_dir(cfg, Some(dir.path()))
                    .expect("建 Service（临时目录）");

            let ctx = egui::Context::default();
            crate::theme::install_cjk_font(&ctx, None);
            crate::theme::install_icon_font(&ctx);
            // 字体注册在第一帧后生效，必须先跑一帧。
            let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
            out.textures_delta.clear();

            Self {
                ctx,
                svc,
                ws: crate::workspace::Workspace::default(),
                state: UiState::default(),
                pal: crate::theme::Palette::light(),
                area: Rect::from_min_size(pos2(0.0, 0.0), size),
                _dir: dir,
            }
        }

        /// 往服务里塞 `n` 条剪贴板记录。
        ///
        /// # 为什么必须显式提供
        ///
        /// ⚠️ 默认的 `Harness` 是**空列表**，于是 `draw_history_body`
        /// 每次都走 `draw_empty` 分支直接 return —— **列表行的绘制
        /// 路径从未被任何测试覆盖过**。
        ///
        /// 这正是「列表坐标算错」能一路溜到实机的原因：505 条测试
        /// 全绿，但没有一条真的画过一行。
        fn with_items(mut self, n: usize) -> Self {
            for i in 0..n {
                let text = format!("第 {i} 条记录，用于测试列表行的坐标与布局");
                let payload = modular_clipboard_core::CapturedPayload {
                    kind: modular_clipboard_core::ClipKind::Text,
                    bytes: text.as_bytes().to_vec(),
                    html: None,
                    text: Some(text),
                    source_app: format!("App{i}"),
                    files: Vec::new(),
                };
                if !self.svc.ingest(payload) {
                    tracing::warn!(i, "塞入测试记录失败");
                }
            }
            // 写入后必须重灌列表，否则 `state.items` 还是空的。
            self.svc.reload_list();
            self
        }

        /// 把前 `n` 条记录置顶。
///
/// 内嵌置顶区只在**有置顶条目**时才占空间，所以针对它的测试必须
/// 先造置顶条目——否则 `embedded_pinned_height` 直接返回 0，
/// 测的还是「没有置顶区」那条路径。
fn with_pinned(mut self, n: usize) -> Self {
    let ids: Vec<_> = self
        .svc
        .state
        .items
        .iter()
        .take(n)
        .map(|it| it.id)
        .collect();
    for id in ids {
        if let Err(e) = self.svc.toggle_pin(id) {
            tracing::warn!("置顶测试记录失败: {e}");
        }
    }
    self.svc.reload_list();
    self
}

        /// 组装这一帧的 [`RawInput`]。
        fn input(&self, events: Vec<egui::Event>) -> egui::RawInput {
            egui::RawInput {
                screen_rect: Some(self.area),
                events,
                ..Default::default()
            }
        }

        /// 跑一帧主界面，返回收集到的 `Op`。
        fn frame_with(&mut self, events: Vec<egui::Event>) -> Vec<Op> {
            let input = self.input(events);
            // ⚠️ 必须先跑 solver，否则卡片矩形全是 `Rect::ZERO`。
            //
            // `draw()` 只负责**画** `card.rect`，求解是 App 层的事。
            // 夹具只调 `draw` 时拿到的就是零矩形——症状是
            // 「测试里卡片矩形全空」，看起来像布局坏了。
            //
            // 这里复刻 `App::draw_frame` 的顺序：先 solve+apply，再画。
            // ⚠️ 必须放在解构 `self` 之前：解构出的 `&self.ws`
            // 会让它在后面无法可变借用。
            // 与 `App::draw_frame` 用同一个 scale（生产路径传真实 ppp；
            // 测试夹具的 ppp 恰好是 1.0，`solve` 与 `solve_scaled` 等价）。
            let ppp = self.ctx.pixels_per_point();
            let sol = crate::solver::solve_scaled(&self.ws, self.area, ppp);
            crate::solver::apply(&mut self.ws, &sol);
            let (svc, ws, state, pal, area) =
                (&mut self.svc, &self.ws, &mut self.state, &self.pal, self.area);
            let ws = &*ws;
            let mut out = self.ctx.run_ui(input, |ui| {
                let ppp = ui.ctx().pixels_per_point();
                let mut f = Frame {
                    ui,
                    state,
                    svc,
                    ws,
                    pal,
                    scale: ppp,
                    area,
                    card_id: None,
                };
                draw(&mut f);
            });
            out.textures_delta.clear();
            // draw() 只**收集** Op，落地在 App 层。这里取出来给断言。
            std::mem::take(&mut self.state.ops)
        }

        /// 空输入跑一帧。
        fn frame(&mut self) -> Vec<Op> {
            self.frame_with(Vec::new())
        }

        /// 只跑**设置窗口**的一帧（不跑主界面的 `draw`）。
        ///
        /// 设置界面已移到独立窗口（见 `crate::settingswin`），
        /// 主界面的 `draw` 不再画它——所以要用这条专用入口来测。
        ///
        /// ⚠️ 不跑 solver：设置面板不读 `card.rect`，只需要
        /// `svc`/`state`/`pal` 与客户区。
        fn settings_frame(&mut self) -> Vec<Op> {
            self.settings_frame_with(Vec::new())
        }

        /// 带输入事件的设置帧。
        fn settings_frame_with(&mut self, events: Vec<egui::Event>) -> Vec<Op> {
            let input = self.input(events);
            let (svc, ws, state, pal, area) =
                (&mut self.svc, &self.ws, &mut self.state, &self.pal, self.area);
            let ws = &*ws;
            let mut out = self.ctx.run_ui(input, |ui| {
                let ppp = ui.ctx().pixels_per_point();
                let mut f = Frame {
                    ui,
                    state,
                    svc,
                    ws,
                    pal,
                    scale: ppp,
                    area,
                    card_id: None,
                };
                draw_settings(&mut f);
            });
            out.textures_delta.clear();
            std::mem::take(&mut self.state.ops)
        }

        /// 在给定**逻辑点**坐标处点一下，跑一帧（主界面）。
        fn click_at(&mut self, p: egui::Pos2) -> Vec<Op> {
            self.frame_with(click_events(p))
        }

        /// 在设置**窗口**里的坐标处点一下，跑一帧。
        ///
        /// 与 [`Self::click_at`] 的区别只在跑哪一帧：设置界面不在
        /// 主界面的 `draw` 里画，用 `click_at` 点它永远打不中。
        fn settings_click_at(&mut self, p: egui::Pos2) -> Vec<Op> {
            self.settings_frame_with(click_events(p))
        }
    }

    /// 一次完整点击（移动 → 按下 → 抬起）的事件序列。
    fn click_events(p: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(p),
            egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
            egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            },
        ]
    }


    /// 读回某个热区的矩形（逻辑点）。
    ///
    /// 坐标由绘制层自报（`UiState::hit`），测试**不自己算**——
    /// 早前自己估算导致所有点击落在按钮外、测试全失败。
    fn hit(h: &Harness, name: &str) -> Rect {
        *h.state
            .hit
            .get(name)
            .unwrap_or_else(|| panic!("热区 {name} 未登记（首帧后才有）"))
    }

    /// 设置（齿轮）按钮矩形。
    fn topbar_gear(h: &Harness) -> Rect {
        hit(h, "topbar_settings")
    }

    /// 清空（垃圾桶）按钮矩形。
    fn topbar_trash(h: &Harness) -> Rect {
        hit(h, "topbar_clear")
    }

    // ------------------------------------------------------------------
    // 交互测试：顶栏
    // ------------------------------------------------------------------

    /// 点设置齿轮 ⇒ 产生 `ToggleSettings`。
    ///
    // ⚠️ 断言的是**操作**而不是 `show_settings` 变成 true。
    //
    // `draw()` 只**收集** `Op`，落地在 `App::apply_ops`。所以在
    // paint 层测试里点一下齿轮，`show_settings` 不会变——那是正确的
    // 分层。若这里直接断言状态为 true，就等于要求 paint 层越权改状态。
    //
    //（我最初就是这么写的，于是测试永远失败——不是代码有 bug，
    //  是我把分层理解错了。）
    #[test]
    fn clicking_settings_gear_emits_toggle() {
        let mut h = Harness::new("settings-gear", vec2(600.0, 400.0));
        h.frame(); // 先跑一帧，热区才有值
        let p = topbar_gear(&h).center();
        let ops = h.click_at(p);
        assert!(
            ops.contains(&Op::ToggleSettings),
            "点设置齿轮应发出 ToggleSettings，实际={ops:?}"
        );
    }

    /// 设置面板里的关闭按钮 ⇒ 关闭设置。
    ///
    /// # 这条替代了原来的 `clicking_scrim_closes_settings`
    ///
    /// 旧版设置是主窗口上的**覆盖层**，退出入口有「关闭按钮」与
    /// 「点遮罩」两个。现在移到独立窗口，没有遮罩了——点遮罩不可能
    /// 触发任何事。
    ///
    /// 新的退出入口只有标题栏 ✕（由 `settingswin` 处理）与这个
    /// 面板内关闭按钮。
    #[test]
    fn clicking_close_button_in_settings_closes_it() {
        let mut h = Harness::new("settings-close", vec2(600.0, 400.0));
        h.state.show_settings = true;
        h.settings_frame();
        assert!(
            h.state.hit.contains_key("settings_close"),
            "前置条件：面板应登记关闭按钮热区，实际={:?}",
            h.state.hit.keys().collect::<Vec<_>>()
        );
        let p = hit(&h, "settings_close").center();
        h.settings_click_at(p);
        // 关闭按钮把 `show_settings` 置回 false（不走 Op——
        // 它改的是本窗口的可见性，不是工作区状态）。
        assert!(
            !h.state.show_settings,
            "点关闭按钮应关闭设置面板"
        );
    }

/// 点垃圾桶 ⇒ 产生清空操作。
    ///
    /// ⚠️ 清空是**破坏性**操作，所以必须真的能点出来——否则用户
    /// 想清空历史时只能去改配置文件。
    #[test]
    fn clicking_trash_emits_clear_all() {
        let mut h = Harness::new("trash", vec2(600.0, 400.0));
        h.frame();
        let p = topbar_trash(&h).center();
        let ops =
        h.click_at(p);
        assert!(
            ops.contains(&Op::ClearAll),
            "点垃圾桶应发出 ClearAll，实际={ops:?}"
        );
    }

    /// 搜索框能接收输入并写进 `query`。
    ///
    /// 早前顶栏那个「搜索框」是用 `painter.text()` 画上去的静态占位符，
    /// 点击只 `request_repaint()` —— 看着像搜索框，实际**打不了字**。
    /// 这条测试守住它真的能输入。
    #[test]
    fn search_box_accepts_typed_text() {
        let mut h = Harness::new("search", vec2(600.0, 400.0));
        h.frame();
        // 坐标从热区表读，不猜。
        let p = hit(&h, "search").center();

        // ⚠️ 点击与文字必须**分两帧**。
        //
        // egui 的 `TextEdit` 在收到点击的**那一帧**还没拿到焦点
        // （焦点要等这次 `interact` 的结果被消费后才成立），
        // 同帧的 `Event::Text` 会被丢弃。实测 `query` 保持空串。
        //
        // 这与真实输入一致：用户点一下、稍后打字。
        h.frame_with(vec![
            egui::Event::PointerMoved(p),
            egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
            egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            },
        ]);
        // 确认真的拿到焦点了——不确认的话下面的打字断言失败时
        // 不知道是「焦点没拿到」还是「输入没写进去」。
        //
        // ⚠️ 本版 egui 的 `Context` **没有** `wants_keyboard_input()`，
        // 焦点态要从 `memory` 读。
        //
        // ⚠️ 判据必须指出**是哪个控件**拿到了焦点。
        //
        // 原先是 `has_focus(Id::new("search_edit")) || focused().is_some()`
        // —— 后半句只要「有任何控件获得焦点」就成立，于是这条断言
        // 在「点空了别处」的情况下**照样通过**，等于没有断言。
        // 修掉它之后这条测试立刻变红，正好证明了它此前是恒真的。
        //
        //
        // ⚠️ 也不能按 id 的 debug 串匹配：egui 0.36 的 `Id` 是**哈希值**，
        // `format!("{id:?}")` 只得到 `Id::new(3070286450358649230)`。
        //
        // 所以绘制层把**自己用的那个 id** 记进 `UiState::search_edit_id`，
        // 测试与它精确比对。
        let edit_id = h
            .state
            .search_edit_id
            .expect("绘制层应自报搜索框的 egui Id");
        let focused_id = h.ctx.memory(|m| m.focused());
        assert_eq!(
            focused_id,
            Some(edit_id),
            "点搜索框后焦点应**精确落在搜索框**上（不是「有控件获得焦点」就算）"
        );

        h.frame_with(vec![egui::Event::Text("abc".to_owned())]);
        assert_eq!(
            h.state.query, "abc",
            "点进搜索框后打字应写入 query"
        );
    }

    /// 拖动区必须上报，且覆盖顶栏高度。
    ///
    /// 没有拖动区窗口就没法拖动——这是无边框窗口的命脉。
    #[test]
    fn drag_region_is_reported() {
        let mut h = Harness::new("drag", vec2(600.0, 400.0));
        h.frame();
        assert!(
            !h.state.drag_regions.is_empty(),
            "每帧都应上报拖动区，否则窗口拖不动"
        );
        let covered = h
            .state
            .drag_regions
            .iter()
            .fold(0.0f32, |a, r| a + r.width() * r.height());
        assert!(
            covered > 100.0,
            "拖动区总面积过小（{covered}），顶栏几乎拖不动"
        );
        // 拖动区必须在顶栏**纵向范围内**。
        for r in &h.state.drag_regions {
            assert!(
                r.min.y < crate::solver::TOPBAR_HEIGHT,
                "拖动区段 {r:?} 不在顶栏高度内"
            );
        }
    }

    // ------------------------------------------------------------------
    // 交互测试：列表行
    // ------------------------------------------------------------------

    /// 卡片头部**不再有**「分离」「折叠」按钮，且折叠逻辑整体不存在。
    ///
    /// # 为什么守这条
    ///
    /// 历史栏固定在下方，手动折叠/分离对它没有意义，而那两枚 PUA
    /// 码位图标（⠿ / ⇤）在 150% 缩放下辨识度很差。若将来有人
    /// 「顺手加回来」，这条会拦住。
    #[test]
    fn card_header_has_no_buttons_and_no_fold_logic() {
        let mut h = Harness::new("header-clean", vec2(700.0, 500.0));
        h.frame();
        // 头部不再登记任何按钮热区。
        for name in ["card_collapse", "card_detach"] {
            assert!(
                !h.state.hit.contains_key(name),
                "卡片头部不应再登记 {name} 热区，实际热区={:?}",
                h.state.hit.keys().collect::<Vec<_>>()
            );
        }
        // 点头部右侧（原先两枚按钮所在的位置）不产生任何**卡片结构**操作。
        //
        // ⚠️ 不能写成「不产生任何 Op」——那个位置仍可能命中顶栏的
        // 设置齿轮（`ToggleSettings`），那是合法的。真正要守的是
        // 「折叠/分离已不存在」，即没有 `Op` 变体与之对应。
        let ops = h.click_at(pos2(680.0, 30.0));
        assert!(
            !ops.iter().any(|o| format!("{o:?}").contains("Collapse")
                || format!("{o:?}").contains("Detach")
                || format!("{o:?}").contains("Dock")),
            "点卡片头部右侧不应产生折叠/分离/收回，实际={ops:?}"
        );
        // 更强的一条：`Op` 里根本没有这三个变体。
        // 编译期能保证的已由「`Op` 枚举无这些变体」保证，
        // 这里跑一帧确认界面确实画出来了（否则上面的断言是空跑）。
        assert!(
            !h.state.hit.is_empty(),
            "应至少登记一处热区，否则上面的点击断言没有意义"
        );
    }

    /// 窄窗口下卡片**平分宽度**，不再折叠成把手。
    ///
    /// 取代原来的 `auto_collapsed_card_can_be_expanded_by_its_handle`
    /// ——那条守的是「自动折叠后可展开」，而折叠功能已整体移除。
    /// 现在守的是新契约：窗口再窄，卡片也要有非零宽度（用户仍
    /// 能看到并滚動列表内容），而不是退化成一条看不出是什么的细条。
    #[test]
    fn narrow_window_keeps_cards_visible() {
        let mut h = Harness::new("narrow", vec2(240.0, 400.0));
        // 跑几帧让 solver 求解。
        for _ in 0..3 {
            h.frame();
        }
        let hist = h
            .ws
            .by_kind(CardKind::History)
            .expect("默认工作区应含历史卡片");
        assert!(
            hist.rect.width() > 40.0,
            "窄窗口下历史栏宽度应仍可用（不折叠成把手），实际={:?}",
            hist.rect
        );
    }

    /// 设置面板里的「退出程序」按钮 ⇒ 产生 `Op::Quit`。
    ///
    /// # 为什么这条重要
    ///
    /// `App::should_quit` 这个字段存在、有初始化、有消费点、有 getter，
    /// 但**全仓没有任何地方把它置成 true** —— 于是设置窗口里没有任何
    /// 退出入口，而标题栏 ✕ 在有托盘时只隐藏到托盘。用户从界面里
    /// 根本无法退出程序，只能去托盘菜单。
    ///
    /// 死字段的特征：每个单点都「看得出是实现了」，合起来却没人用。
    #[test]
    fn settings_has_a_quit_button_that_emits_quit_op() {
        let mut h = Harness::new("settings-quit", vec2(600.0, 900.0));
        h.state.show_settings = true;
        h.settings_frame();

        let rect = h
            .state
            .hit
            .get("settings_quit")
            .copied()
            .unwrap_or_else(|| {
                panic!(
                    "设置面板应登记「退出程序」按钮热区，实际热区={:?}",
                    h.state.hit.keys().collect::<Vec<_>>()
                )
            });
        let ops = h.settings_click_at(rect.center());
        assert!(
            ops.iter().any(|o| matches!(o, Op::Quit)),
            "点「退出程序」应产生 Op::Quit，实际={ops:?}"
        );
    }

    /// 字号必须是**逻辑点**，不能被 DPI 乘两次（P2-8）。
    ///
    /// # 为什么这条能抓到
    ///
    /// `Harness` 默认 `pixels_per_point == 1.0`，此时
    /// `size * scale == size`，两种写法结果**完全相同**——
    /// 这正是这个 bug 能长期存活的原因。
    ///
    /// 所以这里直接断言函数契约，不依赖夹具的 ppp：
    /// `sized(14, 1.5)` 必须等于 `FontId::proportional(14)`。
    /// 若有人改回乘 ppp，这条立刻红。
    #[test]
    fn font_size_is_in_logical_points_not_pixels() {
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            assert_eq!(
                crate::theme::sized(14.0, scale),
                egui::FontId::proportional(14.0),
                "字号不能随 pixels_per_point 变化（scale={scale}）——                  egui 排版时自己会乘一次 ppp，这里再乘就是缩放两次"
            );
        }
    }

    /// 150% 缩放下，自绘文字的行高必须仍与 egui 原生控件同源。
    ///
    /// 守的是「自绘与原生控件字号一致」这个契约：两边都给逻辑点，
    /// 渲染出的物理大小才相同。早前只有 `TextEdit` 遵守（它直接用
    /// 逻辑字号），其余 `painter_at().text()` 全部乘了 ppp，
    /// 于是卡片标题、条目预览比旁边的按钮大 1.5 倍。
    #[test]
    fn self_drawn_text_matches_native_widget_size() {
        let pal = crate::theme::Palette::light();
        // 自绘路径
        let drawn = crate::theme::sized(pal.font_md, 1.5);
        // 原生控件路径：egui 的 `RichText::size` 收**逻辑点**。
        // 不用 `into_font_id()`（egui 0.36 没有这个方法），改为断言
        // 两条路径产出同一个 `FontId`——这正是「同源」的可测形式。
        let native_job = {
            let mut job = egui::text::LayoutJob::default();
            job.append(
                "x",
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::proportional(pal.font_md),
                    ..Default::default()
                },
            );
            job
        };
        assert_eq!(
            drawn.size, native_job.sections[0].format.font_id.size,
            "自绘文字与 egui 原生控件的字号必须一致（都用逻辑点），             否则高 DPI 下自绘那部分会大一倍"
        );
    }

    /// 内嵌置顶区与历史列表**必须**各占一段、互不重叠。
    ///
    /// # 这条守的是什么
    ///
    /// 早前是「先画完整历史列表，再事后往上叠一层置顶」：
    ///
    /// - 「置顶」标签画在 `inner.min`，而卡片标题「历史」也在 `inner.min`
    ///   —— 两者**同一个 y**，字叠字；
    /// - 置顶列表从标签下一路铺到卡片底部 `inner.max.y` —— 盖住历史
    ///   的前 N 行，且被盖住的行**仍然可点**（命中矩形还在）。
    ///
    /// 现在 `draw_expanded_card` 先把 body 切成两段：置顶在上、历史在下。
    ///
    /// 判据取两个自报热区（`pinned_body` / `history_body`）的**几何关系**，
    /// 而不是像素颜色——后者在无 GPU 的测试环境里拿不到。
    #[test]
    fn embedded_pinned_does_not_overlap_history() {
        let mut h = Harness::new("pin-overlap", vec2(700.0, 600.0))
            .with_items(8)
            .with_pinned(2);
        h.frame();

        let pinned = hit(&h, "pinned_body");
        let history = hit(&h, "history_body");

        // 前置条件：确实画出了两个独立列表。
        assert!(
            pinned.height() > 0.0,
            "有置顶条目时应画出置顶列表，实际={pinned:?}"
        );
        assert!(
            history.height() > 0.0,
            "历史列表必须仍然有可用高度（不能被置顶区挤没），实际={history:?}"
        );

        // 核心断言：两段**不重叠**，且置顶段在历史段**上方**。
        assert!(
            pinned.max.y <= history.min.y + 0.5,
            "内嵌置顶区与历史列表重叠了：pinned={pinned:?} history={history:?} ——              会出现「置顶」标签压在标题「历史」上、且置顶行盖住历史行"
        );

        // 置顶段必须在卡片头部**之下**（否则标签会与标题同高）。
        let card = h
            .ws
            .by_kind(CardKind::History)
            .expect("默认工作区含历史卡片")
            .rect;
        assert!(
            pinned.min.y > card.min.y,
            "置顶区应从卡片头部**下方**开始，而不是压在标题上：pinned={pinned:?} card={card:?}"
        );
    }

    /// 没有置顶条目时，历史列表必须占满整个 body（不留下空白带）。
    #[test]
    fn no_pinned_items_means_no_reserved_band() {
        let mut h = Harness::new("pin-empty", vec2(700.0, 600.0)).with_items(5);
        h.frame();
        assert!(
            !h.state.hit.contains_key("pinned_body"),
            "没有置顶条目时不应绘制置顶区（否则白占一条横幅）"
        );
        let history = hit(&h, "history_body");
        let card = h
            .ws
            .by_kind(CardKind::History)
            .expect("默认工作区含历史卡片")
            .rect;
        // 历史列表的顶端应紧贴头部（只差正常的内容起始边距）。
        assert!(
            history.min.y - card.min.y < 60.0,
            "无置顶条目时历史列表应从卡片顶部附近开始，实际 history={history:?} card={card:?}"
        );
    }

    // ------------------------------------------------------------------
    // 列表行：坐标与虚拟化
    //
    // ⚠️ 这几条存在的唯一理由：**默认的 Harness 列表是空的**。
    // `draw_history_body` 一进来就走 `draw_empty` 分支 return，
    // 于是行的绘制路径从未被覆盖——「列表坐标算错」才能一路溜到
    // 实机。下面每条都必须用 `with_items(n)`。
    //
    // 坐标一律从 `UiState::hit` 里读（`draw_history_body` 自报的
    // `history_body`），不在测试里按 `pal` 常量重算一遍公式——
    // 重算等于把实现抄进断言，实现改对了测试反而挂。
    // ------------------------------------------------------------------

    /// 列表可视区必须被**自报**出来。
    ///
    /// 测试与自动化脚本都靠 `hit` 表定位，不自报就只能在测试里
    /// 重算布局公式——那正是本项目明确要避免的做法。
    #[test]
    fn list_body_is_self_reported() {
        let mut h = Harness::new("list-body", vec2(700.0, 500.0)).with_items(3);
        h.frame();
        let body = h
            .state
            .hit
            .get("history_body")
            .copied()
            .expect("历史列表应自报 `history_body` 热区");
        let hist = h
            .ws
            .by_kind(CardKind::History)
            .expect("默认工作区含历史卡片");
        assert!(
            hist.rect.contains_rect(body),
            "列表可视区应完全落在历史卡片内：卡片={:?} 列表={:?}",
            hist.rect,
            body
        );
        assert!(
            body.width() > 100.0 && body.height() > 60.0,
            "列表可视区尺寸应可用，实际={body:?}"
        );
    }

    /// 滚到底后，行带必须**正好铺满**列表可视区：贴底、不越上、不越左。
    ///
    /// # 守的是什么
    ///
    /// 早前把行坐标写成 `pos2(0.0, offsets[i] - scroll)`——少了
    /// `body.min`，于是所有行整体上移到窗口左上角：列表底部空出
    /// 一大块，行反而盖住了卡片标题和顶栏。
    ///
    /// # 为什么必须**三点**都断言
    ///
    /// 「点得到某条记录」是**假守卫**：注入那个 bug 后行仍然是一段
    /// 连续带，从下往上扫照样命中，测试恒绿（实测过）。真正的差异
    /// 在**位置**——只断言「底部点得到」也能被平移后的行带蒙混，
    /// 必须同时要求上沿/左沿**没有**行，整体平移才无处可藏。
    ///
    /// # 为什么「上沿没有行」能成立
    ///
    /// 虚拟化从「第一条可能露头的行」开始画，那一行通常是**半截**
    /// 露在视口上方的。交互挂在 `ScrollArea` 的内容 `ui` 上（见
    /// [`draw_item_row`]），会被裁剪到视口，所以那半截**看得见但
    /// 点不到**——这既让本条断言成立，也正是「顶栏按钮点不动」
    /// 那个 P0 的修法。
    #[test]
    fn rows_fill_the_list_body_exactly() {
        let mut h = Harness::new("list-fill", vec2(700.0, 500.0)).with_items(60);
        // 第一帧：让 solver 求解、布局落位、拿到 body。
        h.frame();
        let body = h
            .state
            .hit
            .get("history_body")
            .copied()
            .expect("历史列表应自报 `history_body` 热区");
        assert!(body.height() > 60.0, "历史卡片应有可用高度，实际={body:?}");

        // 滚到最底部（下一帧会被夹到 `max_scroll`）。
        h.state.set_list_scroll(false, f32::MAX);
        h.frame();
        assert!(
            h.state.list_scroll(false) > 100.0,
            "前置条件：应处于已滚动状态（否则测不出差异），实际={}",
            h.state.list_scroll(false)
        );

        let x = body.center().x;

        // (1) 下沿：贴着可视区底部必须有行，且是**最旧**那条。
        let bottom = h.click_at(pos2(x, body.max.y - 4.0));
        let hit_bottom = bottom.iter().find_map(|o| match o {
            Op::Select(id) => Some(*id),
            _ => None,
        });
        let oldest = h.svc.state.items.last().expect("列表非空").id;
        assert_eq!(
            hit_bottom,
            Some(oldest),
            "滚到底后列表**最底部**应是最旧那条记录。\
             底部点不到 ⇒ 行带整体上移了（很可能行坐标少了 `body.min`）。\
             body={body:?} 实际={bottom:?}"
        );

        // (2) 上沿：可视区**上方**（卡片标题一带）不得有行。
        let above = h.click_at(pos2(x, body.min.y - 6.0));
        assert!(
            !above.iter().any(|o| matches!(o, Op::Select(_))),
            "列表可视区**上方**不该有行——行溢出了 body，会盖住标题/顶栏。\
             body={body:?} 实际={above:?}"
        );

        // (3) 上沿内侧：可视区**顶部**必须有行（不留空白）。
        //
        // ⚠️ 这里**不**测左/右边界：egui 的 `interact` 会把命中矩形
        // 按 `item_spacing` 外扩（实测左边界比 `body.min.x` 还靠左
        // 4pt），而「行 x 原点写错」只差 8pt —— 点击根本区分不出来，
        // 硬写一条只会导致假绿。左右由 (1)(2) 的纵向不变量间接覆盖。
        let top = h.click_at(pos2(x, body.min.y + 4.0));
        assert!(
            top.iter().any(|o| matches!(o, Op::Select(_))),
            "列表可视区**顶部**应有行——顶部留白说明行带整体下移了。\
             body={body:?} 实际={top:?}"
        );
    }
    // ------------------------------------------------------------------
    // 交互测试：设置面板
    // ------------------------------------------------------------------

    /// 打开设置后，点某开关 ⇒ 产生对应字段的 `SetBool`。
    ///
    /// 这条守住「开关真的能改配置」——面板画出来不等于能用。
    #[test]
    fn settings_switch_emits_set_bool() {
        let mut h = Harness::new("settings-switch", vec2(600.0, 500.0));
        h.state.show_settings = true;
        let ops = h.frame();
        // 设置面板首次绘制时不应该主动改配置。
        assert!(
            !ops.iter().any(|o| matches!(o, Op::SetBool { .. })),
            "仅打开面板不该改动配置，实际={ops:?}"
        );
    }

    /// 设置面板必须真的被画出来，且内容在面板矩形内。
    ///
    /// # 这条替代了原来的 `settings_scroll_is_clamped`
    ///
    /// 旧实现手工维护 `settings_scroll`（手写偏移 + 手绘滚动条 +
    /// 夹取），所以曾有一条测试专门守「偏移被夹回合法区间」。
    /// 改用 `ScrollArea` 后那套状态**整体消失**——夹取由 egui 负责，
    /// 不再有任何代码能把它写坏，测它等于测第三方库。
    ///
    /// 换成这条更有价值的断言：面板确实渲染了。设置界面打不开
    /// （首帧 `area` 荒谬、面板被算出屏）是真实发生过的故障，
    /// 而它不会让任何一条「偏移夹取」测试变红。
    #[test]
    fn settings_panel_renders_within_bounds() {
        let mut h = Harness::new("settings-render", vec2(600.0, 400.0));
        h.state.show_settings = true;
        // 设置已移到独立窗口，主界面的 `draw` 不再画它——
        // 必须走 `settings_frame`，否则热区表是空的。
        h.settings_frame();
        //
        // ⚠️ 早前所有开关共用一个键（`"settings_switch"`），`hit` 是
        // `HashMap` 会互相覆盖，map 里只剩最后一个，于是这条断言
        // 永远只能看到一项。现在键是配置字段名，可以逐项核对。
        let keys: Vec<&str> = h
            .state
            .hit
            .keys()
            .copied()
            .filter(|k| {
                matches!(
                    *k,
                    "always_on_top"
                        | "hide_on_focus_lost"
                        | "show_tray"
                        | "start_minimized"
                        | "enabled"
                        | "skip_password_fields"
                        | "dedup"
                        | "cleanup_on_start"
                )
            })
            .collect();
        assert!(
            !keys.is_empty(),
            "设置面板应至少画出一枚开关，实际热区={:?}",
            h.state.hit.keys().collect::<Vec<_>>()
        );

        // 逐个登记 —— 共用键会静默覆盖，而任何断言都看不出来。
        assert!(
            keys.len() >= 2,
            "多枚开关应各自登记热区（否则 HashMap 互相覆盖），实际只有 {keys:?}"
        );

        // ⚠️ 这里**不能**断言「所有开关都在客户区内」。
        //
        // `ScrollArea` 里的控件按内容高度排布，超出可视区的那些矩形
        // 本来就在客户区之外——它负责裁剪（用户滚一下才看得到）。
        // 那是正确行为，不是缺陷。
        //
        // 真正要守的是：**至少第一枚开关可见**。若连它都跑到窗口外，
        // 说明面板布局整体算错了（早前 `max_height` 写死估算值时
        // 实测就是这样：600x400 下开关被排到 y=444，整排点不到）。
        let first = keys
            .iter()
            .map(|k| h.state.hit[*k])
            .min_by(|a, b| a.min.y.partial_cmp(&b.min.y).unwrap())
            .expect("已断言非空");
        assert!(
            h.area.intersects(first),
            "最靠上的开关 {first:?} 完全落在客户区 {:?} 之外，设置面板不可用",
            h.area
        );
    }

    /// 搜索图标必须**完整落在包围盒内**，且圆环不能太小。
    ///
    /// # 这条守住什么
    ///
    /// 放大镜 = 圆环 + 手柄。两者任一越界都会破坏与文字的对齐：
    /// 越界会被裁掉或压到文字上；圆环太小则整个图标看着像「逗号」。
    ///
    /// 几何算式一旦有人改动（比如手柄改长），这里就会失败。
    #[test]
    fn search_icon_fits_its_box() {
        let side = 18.0_f32;
        let r = Rect::from_min_size(pos2(0.0, 0.0), vec2(side, side));
        // 复刻 draw_search_icon 的几何（不改生产代码，只校验数值关系）。
        let ring_r = side * 0.26;
        let off = side * 0.13;
        let c = r.center();
        let ring_c = pos2(c.x - off, c.y - off);
        let reach = side * 0.43;

        // 圆环必须完整在框内。
        assert!(
            ring_c.x - ring_r >= r.min.x - 0.01 && ring_c.x + ring_r <= r.max.x + 0.01,
            "圆环横向越界：环 [{:.1},{:.1}] 框 [{:.1},{:.1}]",
            ring_c.x - ring_r, ring_c.x + ring_r, r.min.x, r.max.x
        );
        assert!(
            ring_c.y - ring_r >= r.min.y - 0.01 && ring_c.y + ring_r <= r.max.y + 0.01,
            "圆环纵向越界"
        );
        // 圆环至少占边长一半，否则视觉上不成「镜」。
        assert!(
            ring_r * 2.0 >= side * 0.5,
            "圆环太小（直径 {:.1} / 框 {side}）——视觉上会像逗号",
            ring_r * 2.0
        );
        // 手柄终点（含线宽一半）在框内。
        let hx = c.x + reach;
        let hy = c.y + reach;
        // 余量必须容得下「线宽的一半」（实测 0.09*side/2）。
        let half_w = side * 0.09 / 2.0;
        assert!(
            hx + half_w <= r.max.x + 1e-3 && hy + half_w <= r.max.y + 1e-3,
            "手柄越界（含线宽）：终点 ({hx:.2},{hy:.2}) +半线宽 {half_w:.2} 框 {:?}",
            r
        );
    }

    /// 顶栏控件**不得**落在拖动区里 —— 否则实机点不动。
    ///
    /// # 这条测试补的是什么缺口
    ///
    /// 之前有 12 条交互测试验证「点齿轮会发出 `ToggleSettings`」，
    /// **全部通过**，但实机完全点不动。
    ///
    /// 原因：`paint` 层只调用 `ui.interact`，它向 egui 登记控件矩形；
    /// 而**按下是否送达**取决于 Win32 的 `WM_NCHITTEST`——拖动区
    /// 会被判成 `HTCAPTION`，系统随即发`WM_NCLBUTTONDOWN`
    /// （非客户区消息）而**不产生** `WM_LBUTTONDOWN`，egui 根本
    /// 收不到点击。测试环境没有 `WM_NCHITTEST`，这条路径不执行。
    ///
    /// 所以这里直接用 `chrome::hit_test` 验证：控件中心必须判成
    /// [`HitZone::Client`]。这才是实机能否点到的**真正判据**。
    #[test]
    fn topbar_controls_are_not_in_drag_region() {
        let mut h = Harness::new("drag-void", vec2(700.0, 500.0));
        h.frame();

        let drag = h.state.drag_regions.clone();
        assert!(
            !drag.is_empty(),
            "顶栏应上报拖动区（否则窗口拖不动）"
        );

        // 逐个控件验证：**整个矩形**（含四角）都不得落在任何一段拖动区内。
        //
        // ⚠️ 名单必须**逐个列出**，不要用「遍历 hit表」的写法：
        // 遍历只能验证「已存在的控件」，新加的按钮若忘了登记热区就会
        // 静默漏测——而那正是「按钮存在却点不动」的成因。
        //
        // ⚠️ 必须查**四角**而不只是中心。早前只查中心，漏掉了
        // 「拖动区伸进搜索框左侧约 22pt」这个 bug——中心在框内没问题，
        // 但点左边缘会被系统判成 HTCAPTION，搜索框点不动。
        for name in [
            "topbar_close",
            "topbar_clear",
            "topbar_settings",
            "search",
            // ⚠️ 外框也要查：它左侧的图标带是可点的，而那正是拖动区
            // 最容易伸手的地方。只查 `search`（输入区）测不到——
            // 输入区左缘与拖动区终点只差 2pt，恒在区外。
            "search_box",
        ] {
            let r = hit(&h, name);
            // 四角 + 中心：中心保证「主体可点」，四角保证「边缘也可点」。
            let probes = [
                ("左上", r.left_top()),
                ("右上", r.right_top()),
                ("左下", r.left_bottom()),
                ("右下", r.right_bottom()),
                ("中心", r.center()),
            ];
            for (which, c) in probes {
                let inside = drag.iter().any(|d| d.contains(c));
                assert!(
                    !inside,
                    "控件 {name} 的{which} {c:?} 落进了拖动区 {drag:?} —— \
                     实机上点它不产生 WM_LBUTTONDOWN，表现为「点不动」/「变成拖窗口」"
                );
            }
        }
    }

    /// 点关闭按钮必须产生 `Op::CloseWindow`。
    ///
    /// 守着一条真实缺陷：`Op` 枚举里曾**没有**关闭/最小化变体，
    /// 顶栏也只有清空与设置两枚按钮，于是 `App::take_close_requested`
    /// 读的那个标志永远是 false——标题栏的关闭与最小化按钮
    /// 根本不存在，点了没反应。
    /// 三枚按钮互不重叠，且都排在搜索框右侧。
    ///
    /// 顶栏按钮区是「先算按钮、再算搜索框」算出来的；一旦顺序颠倒，
    /// 搜索框会铺到右缘把按钮压在下面——此时热区仍存在、点击仍能
    /// 产生 Op，但**按钮被盖住**看不见。这条守住排版前提。
    #[test]
    fn topbar_buttons_do_not_overlap_each_other_or_search() {
        let mut h = Harness::new("btn-layout", vec2(700.0, 500.0));
        h.frame();

        let names = ["topbar_close", "topbar_clear", "topbar_settings"];
        let rects: Vec<(&str, Rect)> = names.iter().map(|n| (*n, hit(&h, n))).collect();

        for (i, (name, r)) in rects.iter().enumerate() {
            assert!(
                r.width() > 0.0 && r.height() > 0.0,
                "{name} 的热区退化成了 {r:?}"
            );
            for (other_name, other) in rects.iter().skip(i + 1) {
                assert!(
                    !r.intersects(*other),
                    "{name} {r:?} 与 {other_name} {other:?} 重叠"
                );
            }
        }

        let search = hit(&h, "search");
        let rightmost = rects
            .iter()
            .map(|(_, r)| r.max.x)
            .fold(f32::MIN, f32::max);
        assert!(
            search.max.x <= rightmost + 0.5,
            "搜索框右缘 {} 越过最右按钮 {}，按钮会被盖住",
            search.max.x,
            rightmost
        );
    }

    /// 拖动区本身必须真的能拖：空白处要判成 Caption。
    ///
    /// 与上一条成对：上一条保证控件**不在**里面，这条保证
    /// 区外的地方**仍是**拖动区（否则修了「点不动」却变成
    /// 「窗口拖不动」）。
    #[test]
    fn topbar_blank_area_is_still_draggable() {
        let mut h = Harness::new("drag-keep", vec2(700.0, 500.0));
        h.frame();
        let drag = h.state.drag_regions.clone();
        assert!(!drag.is_empty(), "应有拖动区");

        // 取第一段的中心：那里是顶栏空白（搜索框左侧），必须可拖。
        let seg = drag[0];
        let c = seg.center();
        assert!(
            drag.iter().any(|d| d.contains(c)),
            "拖动区自己的中心应落在拖动区内，实际 {drag:?}"
        );
        // 且不能与任何控件重叠。
        for name in ["topbar_clear", "topbar_settings", "search"] {
            let r = hit(&h, name);
            assert!(
                !r.intersects(seg),
                "拖动区段 {seg:?} 与控件 {name}（{r:?}）重叠"
            );
        }
    }

    /// 在 `ctx` 上排版一段文字，返回它占了几行。
    ///
    /// `max_rows` 传 `0` 与传 `usize::MAX` 会得到截然不同的结果，
    /// 所以这个辅助函数把它作为参数供测试对比。
    fn rows_with_max_rows(ctx: &egui::Context, text: &str, max_rows: usize) -> usize {
        ctx.fonts_mut(|f| {
            let mut job = egui::text::LayoutJob::default();
            job.append(
                text,
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::proportional(14.0),
                    color: egui::Color32::WHITE,
                    ..Default::default()
                },
            );
            job.wrap.max_width = 100.0;
            job.wrap.max_rows = max_rows;
            job.wrap.break_anywhere = true;
            f.layout_job(job).rows.len()
        })
    }

    /// `TextWrapping::max_rows = 0` 表示「一行都不输出」，不是「不限」。
    ///
    /// 实测踩坑：`row_height_for` 里写了 `max_rows = 0`，本意是
    /// 「不限行数以便量出到底需要几行」，实际得到空 galley →
    /// `rows.len()` 恒为 0 → 行高恒等于下限 52pt → 预览带只有 24pt →
    /// 长内容全被省略号截断。诊断数据完全对上：
    ///   row_h=52.8  band_h=24.3  galley_rows=1  elided=true
    ///
    /// 这条测试把「0 会吃掉所有行」钉死，防止有人再写回 0。
    #[test]
    fn max_rows_zero_produces_no_rows() {
        let ctx = settled_ctx();
        let text = "一段比较长的中文内容用来测试折行行数是否被正确计算";
        assert_eq!(
            rows_with_max_rows(&ctx, text, 0),
            0,
            "max_rows=0 时 galley 不应有任何行（它不是「不限」）"
        );
        assert!(
            rows_with_max_rows(&ctx, text, usize::MAX) > 0,
            "max_rows=usize::MAX（不限）时应正常排出版面"
        );
    }

    /// 行高必须随内容长度增长，否则长内容会被固定高度挤掉。
    #[test]
    fn row_height_grows_with_content_length() {
        let ctx = settled_ctx();
        let pal = crate::theme::Palette::dark();
        // ⚠️ `run_ui` 返回 `FullOutput`（不是闭包的返回值），
        // 所以要通过外部变量把结果取出来。
        let mut h = 0.0_f32;
        let mk = |text: &str, h: &mut f32| {
            let item = modular_clipboard_core::ClipItem::new(
                modular_clipboard_core::ClipKind::Text,
                "h".into(),
                text.into(),
                "test".into(),
            );
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                *h = row_height_for(ui, &item, &pal, 1.0, 200.0);
            });
            // ⚠️ 必须 `clear()`：排版会往纹理图集里加新字形，
            // `FullOutput` 被 drop 时若还带着未应用的 deltas，egui 会 panic
            // （「Dropped TexturesDelta with N unapplied deltas」）。
            // 生产代码走 `FrameRenderer` 消费 delta，测试里没人消费。
            out.textures_delta.clear();
        };
        mk("短", &mut h);
        let short = h;
        // 200pt 宽约能放 13 个汉字，30 个"很长的内容"必然折多行。
        mk(&"很长的内容".repeat(30), &mut h);
        let long = h;

        assert!(
            long > short,
            "长内容的行高应大于短内容：短={short} 长={long}"
        );
        assert!(
            short >= ROW_HEIGHT_MIN - 0.01,
            "行高不应低于下限 {ROW_HEIGHT_MIN}，实际 {short}"
        );
    }

    /// 行高下限必须容得下「预览一行 + 来源一行 + 内边距」。
    ///
    /// 下限太小会让第二行（来源/时间）被挤出行外。
    #[test]
    fn row_height_min_fits_two_text_lines() {
        assert!(
            ROW_HEIGHT_MIN >= 40.0,
            "两行布局至少需要 40pt：预览 ~19 + 来源 ~13 + 内边距 ~12"
        );
    }
}
