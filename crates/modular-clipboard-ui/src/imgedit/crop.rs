//! 裁剪矩形（归一化坐标）。
//!
//! 用归一化而不是像素：同一份参数要给预览（≤1024 边）与保存
//! （原图 4K）两处用，写死像素必然有一处对不上。

/// 裁剪矩形，**归一化**坐标（0..1，相对它所在的图像空间）。
///
/// 用归一化而不是像素：同一份参数要给**预览**（≤1024 边）与**保存**
/// （原图 4K）两处用，写死像素必然有一处对不上（2026-10-22）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl CropRect {
    /// 全幅（不裁剪）。
    pub fn full() -> Self {
        Self { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }
    }

    /// 归一化矩形 → 像素矩形（按 `w`/`h` 尺寸展开，并钳进边界）。
    ///
    /// CAUTION: 必须保证结果**至少 1×1** 且落在图内——裁剪出 0 尺寸
    /// 会让后续所有步骤（纹理上传、保存）直接失败，而失败的的表现是
    /// 「画面空白」而不是报错，极难排查。
    pub(crate) fn to_pixels(self, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
        if w == 0 || h == 0 {
            return None;
        }
        let x = (self.x.clamp(0.0, 1.0) * w as f32).floor() as i64;
        let y = (self.y.clamp(0.0, 1.0) * h as f32).floor() as i64;
        let x1 = (self.w.clamp(0.0, 1.0) * w as f32 + x as f32).ceil() as i64;
        let y1 = (self.h.clamp(0.0, 1.0) * h as f32 + y as f32).ceil() as i64;
        let (x0, y0) = (x.clamp(0, w as i64), y.clamp(0, h as i64));
        let (x1, y1) = (x1.clamp(0, w as i64), y1.clamp(0, h as i64));
        let (cw, ch) = (x1 - x0, y1 - y0);
        if cw < 1 || ch < 1 {
            return None;
        }
        Some((x0 as u32, y0 as u32, cw as u32, ch as u32))
    }
}
