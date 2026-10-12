//! 屏幕 / 窗口截图（GDI），输出 PNG 字节。
//!
//! 流程：建立与屏幕兼容的内存 DC + 位图 → BitBlt 拷贝像素 →
//! GetDIBits 取出 32 位 BGRA → 转 RGBA → `image` crate 编码 PNG。
//!
//! 选用 GDI 而非 Windows.Graphics.Capture：后者需要 WinRT 异步 +
//! 会在被截窗口加黄色边框，MCP 自动化场景下 GDI 更简单直接。

#![cfg(windows)]

use anyhow::{Context, Result};
use std::io::Cursor;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC,
    SelectObject, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
};

/// 截取主显示器整个屏幕。
pub fn capture_full_screen() -> Result<Vec<u8>> {
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let bmp = CreateCompatibleBitmap(screen_dc, w, h);
        let old = SelectObject(mem_dc, bmp.into());
        // CAPTUREBLT：包含分层/半透明窗口。
        BitBlt(
            mem_dc,
            0,
            0,
            w,
            h,
            Some(screen_dc),
            0,
            0,
            SRCCOPY | CAPTUREBLT,
        )
        .context("BitBlt 屏幕失败")?;
        let png = bits_to_png(mem_dc, bmp, w, h);
        SelectObject(mem_dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
        png
    }
}

/// 截取指定窗口的客户区。
///
/// 用 `GetDC(hwnd)` + BitBlt：对前台窗口可靠；后台/硬件加速窗口可能
/// 截不全（PrintWindow 更好但位于 Win32::Storage::Xps，依赖较重）。
pub fn capture_window(hwnd: HWND) -> Result<Vec<u8>> {
    unsafe {
        let mut rc = RECT::default();
        GetClientRect(hwnd, &mut rc).context("GetClientRect 失败")?;
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        if w <= 0 || h <= 0 {
            anyhow::bail!("窗口客户区尺寸为 0（可能已最小化）");
        }
        let win_dc = GetDC(Some(hwnd));
        let mem_dc = CreateCompatibleDC(Some(win_dc));
        let bmp = CreateCompatibleBitmap(win_dc, w, h);
        let old = SelectObject(mem_dc, bmp.into());
        BitBlt(mem_dc, 0, 0, w, h, Some(win_dc), 0, 0, SRCCOPY | CAPTUREBLT)
            .context("BitBlt 窗口失败")?;
        let png = bits_to_png(mem_dc, bmp, w, h);
        SelectObject(mem_dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(Some(hwnd), win_dc);
        png
    }
}

/// 从选入位图的内存 DC 里取出像素并编码 PNG。
///
/// # 安全前提
/// 调用方保证 `bmp` 已选入 `mem_dc`、尺寸为 `w × h`。
unsafe fn bits_to_png(
    mem_dc: windows::Win32::Graphics::Gdi::HDC,
    bmp: windows::Win32::Graphics::Gdi::HBITMAP,
    w: i32,
    h: i32,
) -> Result<Vec<u8>> {
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w;
    // 负高度 = top-down 像素顺序（第一行是屏幕顶部），省得编码时翻转。
    bmi.bmiHeader.biHeight = -h;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;
    let (w32, h32) = (w as u32, h as u32);
    let mut buf = vec![0u8; (w32 * h32 * 4) as usize];
    let copied = unsafe {
        GetDIBits(
            mem_dc,
            bmp,
            0,
            h32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    if copied == 0 {
        anyhow::bail!("GetDIBits 未返回任何扫描行");
    }
    // 32 位 DIB 内存序是 BGRA，PNG 需要 RGBA：交换 R/B。
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let img = image::RgbaImage::from_raw(w32, h32, buf)
        .context("像素缓冲尺寸与位图不匹配，无法构造图像")?;
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .context("PNG 编码失败")?;
    Ok(out.into_inner())
}
