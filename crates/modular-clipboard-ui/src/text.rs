//! 文本度量与截断。
//!
//! # 为什么不放在 titlebar 里
//!
//! 这两个函数原先住在 `titlebar.rs`，但它们与标题栏毫无关系——
//! 任何在 painter 上按绝对坐标画字的地方都要用（卡片标题、条目摘要、
//! 来源应用名……）。挂在 titlebar 下只是因为最早只有标题栏需要它们，
//! 随后 `paint.rs` 接管绘制时全都去引用`titlebar::elide_text`，
//! 形成「卡片文字的截断依赖标题栏模块」这种反向依赖。
//!
//! 单独成模块后，`titlebar.rs` 可以整文件删除而不影响绘制层。

use egui::Ui;

/// 实测一段文本在给定字号下的宽度（逻辑点）。
///
/// **必须用它而不是估算**：中英文混排的宽度只能由字体度量得出，
/// 按「汉字算 1、西文算 0.55」估会在真实字体下差好几个点——
/// 而标题栏的间距本来就只有几个点，估错就直接压到搜索框上。
pub fn measure_text(ui: &Ui, text: &str, font: &egui::FontId) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    ui.painter()
        .layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
        .rect
        .width()
}

/// 把文本截断到 `max_w` 之内，超出部分用 `…` 收尾。
///
/// egui 自带的 `Label::truncate` 需要真实的布局上下文（要占一个 `Ui`），
/// 而这里是在 painter 上按绝对坐标画字，只能自己按实测宽度裁。
///
/// 按**实测**宽度逐字累加，而不是估算：与 [`measure_text`] 同源，
/// 保证「裁完一定放得下」。
pub fn elide_text(ui: &Ui, text: &str, font: &egui::FontId, max_w: f32) -> String {
    if text.is_empty() || max_w <= 0.0 {
        return String::new();
    }
    if measure_text(ui, text, font) <= max_w {
        return text.to_owned();
    }
    // 省略号本身也要占宽度，所以先给 ellipsis 留出预算再逐字加。
    let ellipsis = "…";
    let ellipsis_w = measure_text(ui, ellipsis, font);
    if ellipsis_w > max_w {
        // 连省略号都放不下：一个字都不给，返回空串而不是画出半截。
        return String::new();
    }
    let mut budget = max_w - ellipsis_w;
    let mut out = String::new();
    for ch in text.chars() {
        let w = measure_text(ui, &ch.to_string(), font);
        if w > budget {
            break;
        }
        budget -= w;
        out.push(ch);
    }
    out.push_str(ellipsis);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::FontId;

    /// 跑一帧并把帧内的 `Ui` 借出来跑断言。
    ///
    /// 字体度量依赖 `Context::run_ui` 建立的字体集合，空上下文里
    /// `layout_no_wrap` 会 panic，所以必须先跑一帧。`Ui` 只在闭包期间
    /// 有效，因此把断言整个塞进闭包里，而不是想办法把 `Ui` 带出来。
    ///
    /// 收 `FnMut` 而非 `FnOnce`：`CentralPanel::show` 的闭包是 `FnMut`，
    /// 在里面调用捕获来的 `FnOnce` 会 E0507。
    fn with_ui(mut f: impl FnMut(&egui::Ui)) {
        let ctx = egui::Context::default();
        // ⚠️ 必须装字体，否则 `layout_no_wrap` 找不到任何字形而 panic。
        crate::theme::install_cjk_font(&ctx, None);
        let mut out = ctx.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.allocate_painter(egui::Vec2::new(400.0, 200.0), egui::Sense::hover());
                f(ui);
            });
        });
        // ⚠️ 必须 clear：`TexturesDelta` 的 `Drop` 在 debug 断言下要求
        // 「没有被应用的 delta 必须先 clear」，否则整条测试 panic 在
        // epaint 内部，报错信息与真实原因毫无关系。
        out.textures_delta.clear();
    }

    #[test]
    fn empty_text_measures_zero() {
        with_ui(|ui| {
            let f = FontId::proportional(14.0);
            assert_eq!(measure_text(ui, "", &f), 0.0);
            assert_eq!(elide_text(ui, "", &f, 100.0), "");
        });
    }

    #[test]
    fn short_text_is_returned_verbatim() {
        with_ui(|ui| {
            let f = FontId::proportional(14.0);
            assert_eq!(elide_text(ui, "短", &f, 10_000.0), "短", "放得下就不该截断");
        });
    }

    /// 截断后的宽度必须**真的**放得下——这是本函数存在的全部意义。
    ///
    /// 若逐字累加的预算算错（例如漏算省略号），返回值就会超出 `max_w`，
    /// 表现为文字压到相邻元素上。
    #[test]
    fn elided_text_always_fits_within_budget() {
        with_ui(|ui| {
            let f = FontId::proportional(14.0);
            let long = "这是一段很长的中文文本用来测试截断行为是否正确";
            for max_w in [10.0f32, 20.0, 40.0, 80.0, 160.0] {
                let out = elide_text(ui, long, &f, max_w);
                let w = measure_text(ui, &out, &f);
                assert!(
                    w <= max_w + 0.5,
                    "max_w={max_w} 时截断结果宽度 {w} 超出预算（输出 {out:?}）"
                );
            }
        });
    }

    /// 预算连一个省略号都放不下时返回空串，而不是画出半截。
    #[test]
    fn budget_smaller_than_ellipsis_yields_empty() {
        with_ui(|ui| {
            let f = FontId::proportional(14.0);
            assert_eq!(elide_text(ui, "文本", &f, 0.5), "");
        });
    }

    /// 非正预算直接返回空，不做任何逐字累加。
    #[test]
    fn non_positive_budget_yields_empty() {
        with_ui(|ui| {
            let f = FontId::proportional(14.0);
            assert_eq!(elide_text(ui, "文本", &f, 0.0), "");
            assert_eq!(elide_text(ui, "文本", &f, -10.0), "");
        });
    }
}
