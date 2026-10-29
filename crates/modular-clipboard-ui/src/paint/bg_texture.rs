//! 二级主题的质感层：纸质纤维 / 星空星点。
//!
//! 与颜色层（`theme::apply_secondary`）分工：那里换整套令牌，
//! 这里补程序化纹理。纹理 id 每个 `egui::Context` 独立，
//! 因此只在主窗口绘制。

use egui::Rect;
use egui::pos2;
use egui::vec2;

use super::{Frame};

/// 二级主题的质感层：纸质纤维 / 星空星点。
///
/// 与颜色层（`theme::apply_secondary`）分工：那里换**整套令牌**，
/// 这里补程序化纹理。判据是配置里的 `secondary_theme` 与参数
/// （`paper_texture` / `purple_stars`，0 时整层不画——省一次
/// draw call）。
///
/// # 为什么只在主窗口画
///
/// 纹理 id 是**每个 `egui::Context` 独立**的：主窗口 ctx 加载的
/// tile 直接拿到设置窗口 / 弹窗的 ctx 上画会显示成别的纹理（或空白）。
/// 要么每窗口各缓存一份（`UiState` 要按窗口分槽，纹理翻倍），要么
/// 接受「设置窗口没有质感层」。选后者：设置窗口是工具页，用户盯着
/// 看的是主窗口；且**颜色层完全同步**（`settingswin` 每帧从
/// `App.palette()` apply），二级主题的色温在设置窗口里一样看得到。
///
/// # tint 的取色
///
/// tile 是白底、alpha 起伏（见 [`crate::tex`] 的说明），颜色由 tint
/// 乘入，所以深浅两套共用一份纹理：
/// - 纸质深色：暖白墨（纤维比纸基亮一点，读出「一丝一丝」的反光）；
/// - 纸质浅色：暖棕墨（印刷纸的纤维是深色的）；
/// - 星空：纯白，密度即 tint alpha。
pub(super) fn draw_theme_texture(f: &mut Frame<'_>) {
    use modular_clipboard_core::config::SecondaryTheme;
    let ui = &f.svc.state.config.ui;
    match ui.secondary_theme {
        SecondaryTheme::Paper => {
            let k = ui.theme_params.paper_texture.clamp(0.0, 1.0);
            if k <= 0.004 {
                return;
            }
            let tex = match &f.state.paper_tex {
                Some(t) => t.clone(),
                None => {
                    let t = crate::tex::paper_tile(f.ui.ctx());
                    f.state.paper_tex = Some(t.clone());
                    t
                }
            };
            // 墨色强度（标定值，2026-10-18 实测修正）：
            //
            // 原值（浅 18 / 深 14）是按「 Subtle 」猜的，实测整个窗口的
            // 亮度标准差只有 0.8 色阶——纹理等于不存在（用户反馈
            // 「纸质渲染不正常」）。重标后的目标量级：
            // - 均值 Δ ≈ 8~10 色阶（看得出「这不是纯色板」）；
            // - 纤维峰值 Δ ≈ 15~25 色阶（「一丝一丝」读得出来）；
            // - 且不超过背景对比度的 1/10（不抢内容）。
            //
            // 深色下明显更弱：亮纤维在暗底上比暗纤维在亮底上更显眼，
            // 且人眼对暗部噪声更敏感（同样的 Δ 在暗底上觉得「脏」而非
            // 「纸」）。实测 30 像落了层灰、20 仍偏噪点，16 才回到
            // 「暗色纸张」的观感（2026-10-18 截图标定）。
            let (ir, ig, ib, base_a) = if f.pal.is_dark {
                (235u8, 220u8, 195u8, 16.0)
            } else {
                (112u8, 94u8, 66u8, 42.0)
            };
            let a = (base_a * k).round() as u8;
            crate::tex::paint_tiled(
                f.ui.painter(),
                f.area,
                &tex,
                egui::Color32::from_rgba_unmultiplied(ir, ig, ib, a),
                crate::tex::PAPER_TILE_PT,
            );
        }
        SecondaryTheme::Purple => {
            let params = &ui.theme_params;
            let k = params.purple_stars.clamp(0.0, 1.0);
            if k <= 0.004 {
                return;
            }
            // ---- 星星列表：参数指纹变化才重建 ----------------------------
            //
            // 三个影响列表的参数（密度 / 随机化 / 大小）编成指纹，
            // `StarField` 自己记住生成时的指纹——没变就每帧复用同一份
            // 列表，只重算闪烁。random > 0 时把 egui 时间当熵混入种子，
            // 于是「每次重建换一片星空」；random = 0 时确定性（同一片星）。
            let fp = crate::tex::StarField::fingerprint_of(
                params.purple_stars,
                params.star_random,
                params.star_size_var,
            );
            let rebuild = f
                .state
                .star_field
                .as_ref()
                .is_none_or(|s| s.fingerprint != fp);
            if rebuild {
                let entropy = if params.star_random > 0.004 {
                    (f.ui.input(|i| i.time) * 1000.0) as u64
                } else {
                    0
                };
                f.state.star_field = Some(crate::tex::StarField::generate(
                    params.purple_stars,
                    params.star_random,
                    params.star_size_var,
                    entropy,
                ));
            }
            let Some(field) = f.state.star_field.as_ref() else {
                return;
            };
            if field.stars.is_empty() {
                return;
            }
            // ---- 精灵纹理（惰性，每 Context 一份）------------------------
            let tex = match &f.state.star_tex {
                Some(t) => t.clone(),
                None => {
                    let t = crate::tex::star_sprite(f.ui.ctx());
                    f.state.star_tex = Some(t.clone());
                    t
                }
            };
            // ---- 逐颗绘制：闪烁 alpha + 大小 + 光标视差 ------------------
            //
            // 每颗星一个小 quad（16px 精灵按半径放大）。tint 只带 alpha
            // （白星）——闪烁 = tint alpha 随时间呼吸（2026-10-18 修好的
            // 预乘 tint 通道）。暗到谷底的星跳过，省 draw call。
            //
            // 光标视差（`star_cursor`，0 = 关）：星点随光标反向偏移，
            // 近的动得多、远的几乎不动（视差纵深）。光标离开窗口
            // （hover_pos = None）⇒ 零偏移回位。指针移动本身就是事件，
            // 不需要额外续帧。
            let t = f.ui.input(|i| i.time);
            let pointer = f.ui.input(|i| i.pointer.hover_pos());
            let painter = f.ui.painter();
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            let area = f.area;
            for star in &field.stars {
                let a = crate::tex::twinkle_alpha(star, params.star_twinkle, t);
                if a <= 0.004 {
                    continue;
                }
                let base = pos2(
                    area.min.x + star.x * area.width(),
                    area.min.y + star.y * area.height(),
                );
                let c = base + crate::tex::cursor_offset(star, pointer, area, params.star_cursor);
                let r = Rect::from_center_size(c, vec2(star.radius * 2.0, star.radius * 2.0));
                painter.image(
                    tex.id(),
                    r,
                    uv,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, (a * 255.0) as u8),
                );
            }
            // ---- 闪烁续帧 -------------------------------------------------
            //
            // CAUTION: 闪烁由 `input.time` 驱动（不走 anim 补间注册表 ⇒
            // 不被 `anim::busy` 覆盖），而主循环有空闲闸门：无事件、无
            // 重绘请求时整帧跳过 ⇒ 闪烁会停在某一帧（与 2026-10-17
            // Toast 出场动画被饿死是同一个坑）。twinkle > 0 时每帧续请求；
            // twinkle = 0（用户关掉闪烁）不请求，界面恢复空闲。
            //
            // 必须在绘制闭包内调用：egui 只在 pass 结束时把请求写进
            // `FullOutput.repaint_delay`，闸门读的正是这个值。
            //
            // 用 `request_repaint_after(16ms)` 而不是 `request_repaint()`：
            // 后者把 `repaint_delay` 置成 ZERO，主循环会按「立即重绘」跑
            // 满速（2026-10-29 实测 ~300fps 空转、六成单核）。闪烁是慢
            // 动画，60fps 绰绰有余。
            if params.star_twinkle > 0.004 {
                f.ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(16));
            }
        }
        _ => {}
    }
}
