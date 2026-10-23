//! 配置字段写入：`apply_ops` 的字符串字段落地层。
//!
//! 分组 + 字段名的字符串通道把新增设置项的成本降到
//! 「paint 一行 + 这里一行」；拼错不 panic 由测试守。

/// 改一个布尔配置字段。
///
/// # 为什么用「分组 + 字段名字符串」而不是每项一个 `Op` 变体
///
/// 配置项会随版本增删（现在 20+ 项）。若每个都加一个 `Op` 变体，
/// `apply_ops` 会膨胀成几十个分支，且新增项要改三处。
/// 字符串字段名把新增成本降到「paint 里多一行 + 这里多一行」。
///
/// 代价是**失去编译期检查**——字段名写错会静默无效。
/// [`tests::unknown_config_field_is_ignored`] 守住「拼错不 panic」。
pub(super) fn set_bool_field(cfg: &mut modular_clipboard_core::Config, g: crate::paint::ConfigGroup, field: &str, v: bool) {
    use crate::paint::ConfigGroup as G;
    match (g, field) {
        (G::Capture, "enabled") => cfg.capture.enabled = v,
        (G::Capture, "track_source_app") => cfg.capture.track_source_app = v,
        (G::Capture, "skip_password_fields") => cfg.capture.skip_password_fields = v,
        (G::Capture, "dedup") => cfg.capture.dedup = v,
        (G::Ui, "always_on_top") => cfg.ui.always_on_top = v,
        (G::Ui, "hide_on_focus_lost") => cfg.ui.hide_on_focus_lost = v,
        (G::Ui, "show_tray") => cfg.ui.show_tray = v,
        (G::Ui, "start_minimized") => cfg.ui.start_minimized = v,
        // ---- 第三轮新增（图片直显 / Markdown 渲染） ----
        (G::Ui, "image_inline") => cfg.ui.image_inline = v,
        (G::Ui, "md_rendered") => cfg.ui.md_rendered = v,
        // 通知呈现：弹窗（true）/ 窗口内 toast（false）。
        (G::Ui, "notice_popup") => cfg.ui.notice_popup = v,
        // NOTE: type_rail 的参数走 `Op::SetTypeRail` 整包落地，
        // 没有按字段名的写入方。
    (G::Ui, "autostart") => cfg.ui.autostart = v,
    (G::MiniPanel, "enabled") => cfg.mini_panel.enabled = v,
    (G::MiniPanel, "ctrl") => cfg.mini_panel.ctrl = v,
    (G::MiniPanel, "shift") => cfg.mini_panel.shift = v,
    (G::MiniPanel, "alt") => cfg.mini_panel.alt = v,
    (G::MiniPanel, "win") => cfg.mini_panel.win = v,
        (G::Storage, "cleanup_on_start") => cfg.storage.cleanup_on_start = v,
        (G::Mcp, "enabled") => cfg.mcp.enabled = v,
        (G::Mcp, "resize_window") => cfg.mcp.resize_window = v,
        (G::Mcp, "read_layout") => cfg.mcp.read_layout = v,
        (G::Mcp, "click_element") => cfg.mcp.click_element = v,
        (G::Mcp, "type_text") => cfg.mcp.type_text = v,
        (G::Mcp, "scroll_view") => cfg.mcp.scroll_view = v,
        (G::Mcp, "screen_capture") => cfg.mcp.screen_capture = v,
        (G::Mcp, "window_focus") => cfg.mcp.window_focus = v,
        (G::Mcp, "drag_drop") => cfg.mcp.drag_drop = v,
        _ => tracing::warn!(?g, field, "未知的布尔配置字段，已忽略"),
    }
}

/// 改一个数值配置字段。
pub(super) fn set_number_field(
    cfg: &mut modular_clipboard_core::Config,
    g: crate::paint::ConfigGroup,
    field: &str,
    v: f64,
) {
    use crate::paint::ConfigGroup as G;
    match (g, field) {
        (G::Capture, "poll_interval_ms") => cfg.capture.poll_interval_ms = v.max(50.0) as u64,
        (G::Capture, "dedup_window_secs") => cfg.capture.dedup_window_secs = v.max(0.0) as i64,
        (G::Ui, "font_scale") => cfg.ui.font_scale = (v.clamp(0.75, 2.0)) as f32,
        (G::MiniPanel, "count") => cfg.mini_panel.count = (v.clamp(1.0, 10.0)) as usize,
    (G::MiniPanel, "preview_chars") => {
        cfg.mini_panel.preview_chars = (v.clamp(10.0, 100.0)) as usize
    }
        (G::Storage, "max_bytes") => cfg.storage.max_bytes = v.max(1.0) as u64,
        (G::Storage, "max_payload_bytes") => cfg.storage.max_payload_bytes = v.max(1.0) as u64,
        (G::Mcp, "port") => cfg.mcp.port = v.clamp(0.0, 65535.0) as u16,
        _ => tracing::warn!(?g, field, "未知的数值配置字段，已忽略"),
    }
}

/// 改一个文本配置字段（MCP 服务地址 / 密钥）。
pub(super) fn set_text_field(cfg: &mut modular_clipboard_core::Config, g: crate::paint::ConfigGroup, field: &str, v: &str) {
    use crate::paint::ConfigGroup as G;
    match (g, field) {
        (G::Mcp, "host") => cfg.mcp.host = v.trim().to_string(),
        (G::Mcp, "token") => cfg.mcp.token = v.trim().to_string(),
        // 第三轮：双击跳转的调用路径。**不 trim 掉内部空格**——Windows
        // 路径合法地包含空格（`C:\Program Files\...`），只去首尾。
        (G::Ui, "browser_path") => cfg.ui.browser_path = v.trim().to_string(),
        (G::Ui, "explorer_path") => cfg.ui.explorer_path = v.trim().to_string(),
        _ => tracing::warn!(?g, field, "未知的文本配置字段，已忽略"),
    }
}

/// 改一个可选数值配置字段（`None` = 不限）。
pub(super) fn set_opt_number_field(
    cfg: &mut modular_clipboard_core::Config,
    g: crate::paint::ConfigGroup,
    field: &str,
    v: Option<f64>,
) {
    use crate::paint::ConfigGroup as G;
    match (g, field) {
        (G::Storage, "max_items") => {
            cfg.storage.max_items = v.map(|x| x.max(1.0) as usize);
        }
        _ => tracing::warn!(?g, field, "未知的可选数值配置字段，已忽略"),
    }
}
