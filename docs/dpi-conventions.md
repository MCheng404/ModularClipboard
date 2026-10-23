# DPI / 缩放约定（ModularClipboard）

> **本文件是硬约定**。新功能（布局、控件、窗口、纹理、坐标换算）必须
> 遵守；违反约定的代码在高 DPI / 大 font_scale / 混合 DPI 多屏上必然
> 出现「文字被裁、元素错位、尺寸跳变」类 bug——这些 bug 在 100% 屏 +
> font_scale=1.0 的开发环境**不可见**，只在用户机器上爆发。
>
> 起源：2026-10-19「DPI 完整治理」（gfx 审计 P0=0/P1×3/P2×8 + UI 层
> 6 处 font_scale 脱钩修复），以及同日令牌面板「文字向上且渲染不完整」
> 的实机反馈（左列格子写死 18pt 与字号脱钩）。

## 一、三层量纲表

| 量纲 | 含义 | 谁负责换算 | 典型 API |
|---|---|---|---|
| **逻辑点**（pt） | egui 布局单位。字号、控件尺寸、行高、`Rect` 全在此空间 | egui 排版时自动乘 `pixels_per_point` | `Palette::font_*`、`allocate_exact_size`、`FontId::proportional` |
| **物理像素**（px） | GPU/Win32 原生单位。交换链、纹理、`CreateWindowExW`、`SetWindowPos` | 调用方乘 `scale_factor()` | `Window::new(w_px, h_px)`、`set_size_physical`、`DeviceImage` |
| **归一化** | 0..1 相对量（如星点坐标） | 绘制时乘逻辑尺寸 | `tex::Star.{x,y}` |

**铁律**：逻辑点与物理像素**永不混算**。换算必须显式：
```rust
let logical = physical / window.scale_factor();
let physical = (logical * window.scale_factor()).round() as u32;
```

## 二、字号与布局常数（最易违反）

### 2.1 字号一律经 `Palette`

```rust
// ✅ 对：pal.font_* 已含 font_scale 缩放
ui.label(RichText::new(title).size(pal.font_sm));
painter.text(pos, align, name, FontId::proportional(pal.font_xs), color);

// ❌ 错：裸常数不随 font_scale 放大——大字号下倒挂/被裁
ui.label(RichText::new(title).size(15.0));
FontId::proportional(24.0)
```

`theme::sized(size, scale)` 的约定：字号是**逻辑点**，不乘 ppp
（egui 排版时自己乘）。不要在渲染层再乘 `pixels_per_point`。

### 2.2 与文字同屏的固定高度必须按字号算

egui 的 horizontal **不重排先放置的控件**：行高被后放的高控件撑大时，
先放的控件仍顶部对齐。固定高度的格子在 font_scale > 1 时会「文字偏高
+ 字形被裁」（2026-10-19 令牌面板实机 bug 的精确根因）。

```rust
// ✅ 对：行高按字号派生，三列统一
let row_h = (pal.font_xs * 1.45).max(pal.font_sm * 1.07).max(18.0);
let (lr, _) = ui.allocate_exact_size(vec2(118.0, row_h), Sense::empty());
draw_slider(..., cell_h = row_h);          // 滑条也传行高兜底

// ❌ 错：格子写死，与字号脱钩
let (lr, _) = ui.allocate_exact_size(vec2(118.0, 18.0), Sense::empty());
```

参考实现：`paint::token_row_h`（含 CAUTION 注释）与其守门测试
`token_row_h_covers_glyphs_and_slider_at_any_font_scale`
（遍历 font_scale 0.75~2.0 全量程）。

### 2.3 图形元素（图标、圆点、色块）

- 图标尺寸用 `pal.icon_size`（已缩放）；
- 纯装饰图形（状态圆点）可固定，但**同屏有文字的容器**（色块、按钮）
  高度必须按字号派生；
- 按钮可点区域用 `pal.control_height` 派生。

## 三、多窗口与坐标

1. **每个窗口用自己的 `window.scale_factor()`**。混合 DPI 多屏上主窗与
   弹窗/子窗可能不同缩放——把主窗 scale 传给子窗是 P1 级 bug
   （`childwin.rs` 的复核路径是范例：建窗后按真实 scale 修正尺寸）。
2. **`move_to(x, y)` 收物理像素**；`screen_position_points()` 返回逻辑点
   ——一对 API 单位不对称，配对使用时显式换算并注释。
3. **配置字段语义**（`config.rs` 为准）：
   - `window_width/height`：**物理像素**
   - `window_pos` / `pinned_window_pos`：屏幕**逻辑点**（历史语义）
   - 位置恢复必须 `clamp_to_work_area`（跨 DPI 漂移的护栏，见
     `app.rs` 恢复路径的 P1-2 注释）。
4. **DPI 变化**（`WM_DPICHANGED`）：按系统建议矩形 `SetWindowPos`
   保持**逻辑尺寸恒定**（`window.rs` 的处理是范例）。

## 四、GPU / 纹理

- 交换链、纹理、render_extent、模糊半径、UV 位移分母：**物理像素**。
- 不变式：`screen_rect（逻辑） × ppp == render_extent（物理）`。
- `render_scale_for(dpi)` 的下限与上限**联动**（`ceiling.min(0.25)`）：
  硬编码下限会在超高 DPI 下架空 ppp 上限。

## 五、守门测试清单（新功能照此补）

| 不变量 | 测试 |
|---|---|
| 行高装得下字形与滑条（全 font_scale） | `paint::token_row_h_covers_glyphs_and_slider_at_any_font_scale` |
| font_scale 缩放字段与几何 | `theme::font_scale_scales_fonts_and_text_bearing_geometry` |
| 1.0× 恒等 / 幂等 / 脏输入 | `theme::font_scale_one_is_identity` 等 3 条 |
| 字号不被 ppp 双缩放 | `paint::font_size_is_in_logical_points_not_pixels` |
| ppp 不超上限（全 DPI 量程含 s=16） | `lib::render_ppp_never_exceeds_cap_at_any_dpi` |
| DPI 查询失败回 fallback | `window::dpi_scale_factor_guards_zero` |
| backdrop UV 位移分母用物理尺寸 | `renderer::backdrop_uv_offset_*`（黄金值） |

**新布局函数**若涉及字号，必须补「全 font_scale 量程」不变量测试——
夹具默认 font_scale=1.0 会掩盖越界，这正是 bug 能长期存活的原因。
