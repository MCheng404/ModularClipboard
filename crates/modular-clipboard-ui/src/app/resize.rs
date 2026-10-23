//! 窗口宽度调整：`WidthRequest` 两种语义与求解。
//!
//! 「切到独立分栏」只放大，「切回共用单栏」精确恢复默认宽——
//! 无脑一个目标宽度会把两种情况都变成「总是设成它」。

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

/// 窗口宽度调整请求（`Op::SetPinnedMode` 写入，帧循环消费）。
///
/// 两种语义分开是因为「切出去」和「切回来」要的行为**相反**：
/// 无脑一个「目标宽度」会把这两种情况都变成「总是设成它」。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WidthRequest {
    /// 至少放宽到这么多**逻辑点**。当前够宽时不动（只放大不收缩）。
    ///
    /// 用于切到「独立分栏」：多出一栏要宽度，而用户手动拉宽过的
    /// 窗口不该被改小。
    AtLeast(f32),
    /// 精确恢复**默认宽度**（物理像素）。
    ///
    /// 用于切回「共用单栏」：独立分栏会自动把窗口撑宽（并写回配置），
    /// 切回来若沿用「够宽就不动」，窗口就永远停在撑宽的宽度上。
    /// 默认宽度取自 [`modular_clipboard_core::config::UiConfig::default`]，
    /// 不写死数字——配置默认值变化时「恢复默认」跟着变。
    RestoreDefault,
}

/// 主窗口默认宽度（物理像素）。
///
/// 单一数据源是 `UiConfig::default().ui.window_width`。
pub fn default_window_width_px() -> u32 {
    modular_clipboard_core::config::UiConfig::default().window_width as u32
}

/// 算出「该把窗口宽度设成多少物理像素」，不该动就返回 `None`。
///
/// 抽成纯函数是为了能直接测：`take_resize_width_to` 需要 `&mut App`，
/// 而构造 App 要 config + 临时数据目录，放进单元测试太重。
///
/// 语义见 [`WidthRequest`]。`scale` 只对 `AtLeast` 有用（逻辑点 →
/// 物理像素）；`RestoreDefault` 的目标本身就是物理像素。
pub(super) fn resize_width_target(
    request: Option<WidthRequest>,
    current_px: (u32, u32),
    scale: f32,
) -> Option<u32> {
    let request = request?;
    match request {
        WidthRequest::AtLeast(needed) => {
            if needed <= 0.0 || scale <= 0.0 {
                return None;
            }
            let current_points = current_px.0 as f32 / scale;
            // 已经够宽就不动。
            //
            // 留 1pt 容差：浮点误差会让「刚好等于」时在两个分支间反复跳，
            // 表现为每次切模式窗口都抖一下。
            if current_points >= needed - 1.0 {
                return None;
            }
            Some(((needed * scale).round() as u32).max(1))
        }
        WidthRequest::RestoreDefault => {
            let target = default_window_width_px().max(1);
            // 已经就是默认宽度（1px 容差）则不动：省掉一次
            // `set_size_physical` + 交换链重建。
            if current_px.0.abs_diff(target) <= 1 {
                return None;
            }
            Some(target)
        }
    }
}

#[cfg(test)]
mod resize_tests {
    use super::{resize_width_target, WidthRequest};

    /// 放不下时必须给出目标物理宽度。
    ///
    /// 这条直接对应用户的反馈：「置顶模式选择后主窗口大小不会自己调整」
    /// —— 切到独立分栏多出一栏，需求超过可用宽，`solve_scaled`
    /// 就会按比例把卡片压扁。
    #[test]
    fn widens_when_current_is_too_narrow() {
        // 需求 600 逻辑点，当前只有 400 逻辑点（= 600 物理像素 @1.5）
        let target = resize_width_target(Some(WidthRequest::AtLeast(600.0)), (600, 800), 1.5);
        assert_eq!(target, Some(900), "应放宽到 600×1.5 = 900 物理像素");
    }

    /// 已经够宽时**不动**窗口。
    ///
    /// 这条守的是「只放大不收缩」：用户可能故意把窗口开到很大，
    /// 不该因为一次模式切换被改回来。无脑设成 `needed` 的话，
    /// 来回切两次模式窗口就会来回跳。
    #[test]
    fn leaves_window_alone_when_already_wide_enough() {
        assert_eq!(
            resize_width_target(Some(WidthRequest::AtLeast(400.0)), (900, 800), 1.5),
            None
        );
        // 刚好等于也要算「够宽」（1pt 容差内），否则浮点误差会抖。
        assert_eq!(
            resize_width_target(Some(WidthRequest::AtLeast(400.0)), (600, 800), 1.5),
            None
        );
    }

    /// 切回共用单栏：宽度**恢复默认**（物理像素），不管当前多宽。
    ///
    /// 用户反馈：「置顶模式切换回共用单栏时，宽度恢复为默认」。
    /// 独立分栏会自动撑宽窗口（并写回配置），切回来若按「够宽就不动」
    /// 判断，窗口就停在撑宽的宽度上回不去。
    #[test]
    fn restores_default_width_when_back_to_shared_column() {
        // 当前被独立分栏撑宽到 1180 物理像素（如 1.5x 下的舒适宽度）。
        let target = resize_width_target(Some(WidthRequest::RestoreDefault), (1180, 800), 1.5);
        assert_eq!(
            target,
            Some(super::default_window_width_px()),
            "切回共用单栏必须恢复默认宽度"
        );
        // 比默认窄时同样恢复到默认（篱它不负责「只放大」）。
        let narrow = resize_width_target(Some(WidthRequest::RestoreDefault), (400, 800), 1.0);
        assert_eq!(narrow, Some(super::default_window_width_px()));
    }

    /// 已经是默认宽度时不动（省一次 resize + 交换链重建）。
    #[test]
    fn restore_default_is_noop_at_default_width() {
        let d = super::default_window_width_px();
        assert_eq!(
            resize_width_target(Some(WidthRequest::RestoreDefault), (d, 800), 1.0),
            None
        );
        // 1px 容差内也算「已经是默认」。
        assert_eq!(
            resize_width_target(Some(WidthRequest::RestoreDefault), (d + 1, 800), 1.0),
            None
        );
    }

    /// 没有请求、或参数非法时不动。
    #[test]
    fn no_request_means_no_resize() {
        assert_eq!(resize_width_target(None, (300, 800), 1.5), None);
        assert_eq!(
            resize_width_target(Some(WidthRequest::AtLeast(0.0)), (300, 800), 1.5),
            None
        );
        assert_eq!(
            resize_width_target(Some(WidthRequest::AtLeast(-50.0)), (300, 800), 1.5),
            None
        );
        // scale = 0 会除出 inf / 除零，必须挡住。
        assert_eq!(
            resize_width_target(Some(WidthRequest::AtLeast(600.0)), (300, 800), 0.0),
            None
        );
    }

    /// `RestoreDefault` 不依赖 `scale`（目标本就是物理像素）——
    /// 防的是「将来有人给它也乘了 scale」这种静默错误。
    #[test]
    fn restore_default_ignores_scale() {
        let a = resize_width_target(Some(WidthRequest::RestoreDefault), (1180, 800), 1.0);
        let b = resize_width_target(Some(WidthRequest::RestoreDefault), (1180, 800), 1.5);
        assert_eq!(a, b, "恢复默认与 DPI 缩放无关");
    }

    /// 结果必须是**正**整数：0 会让客户区塌成 0×0，
    /// 下一帧的交换链重建拿不到可呈现的图像，窗口看着像消失了。
    ///
    /// 场景取「极小需求 × 极小缩放」—— 正常布局（需求数百点、
    /// 缩放 1.0~2.0）永远走不到这里，这是纯防御性分支：
    /// `3.0 × 0.1 = 0.3` 四舍五入就是 0，靠 `.max(1)` 兜住。
    #[test]
    fn target_is_never_zero() {
        let t = resize_width_target(Some(WidthRequest::AtLeast(3.0)), (0, 10), 0.1)
            .expect("应给出目标宽度");
        assert!(t >= 1, "目标宽度必须至少 1 像素，实际 {t}");
    }
}
