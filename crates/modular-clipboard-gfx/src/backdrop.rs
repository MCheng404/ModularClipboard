//! 自研半透明背景的桌面捕获（GDI）。
//!
//! 主窗口的「磨砂」不再依赖 DWM 的 `DWMWA_SYSTEMBACKDROP_TYPE`，而是
//! **自己抓取窗口后方的桌面内容**、上传 GPU、经模糊链卷积后作为窗口
//! 底图（见 `ui/src/renderer.rs` 的 `BackdropFx`）。本模块只负责第一步：
//! 把窗口客户区对应的屏幕区域读回 CPU 内存。
//!
//! # 「身后内容」是怎么拿到的：捕获自排除
//!
//! 桌面 DC（`GetDC(None)`）读回的是 DWM 合成后的整屏，**包含本窗口自己**
//! ——直接拿去模糊会形成反馈回路：上一帧的界面（包括文字）被糊进
//! 下一帧的背景里，表现为拖影。解决办法是给窗口设置
//! `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`（Win10 2004+，
//! 见 `ui/src/presence.rs`）：DWM 在构建**捕获表面**时跳过本窗口，
//! 桌面 DC 的 BitBlt 读到的就是窗口身后的内容（壁纸、下层窗口）。
//!
//! 已实测的行为：GDI BitBlt / CopyFromScreen 对被排除窗口返回其身后的
//! 合成结果（不是黑块）；WGC 与 PrintWindow 同样遵守该排除。
//! 代价（刻意接受的副作用）：磨砂模式下窗口本身不出现在录屏/截图里。
//!
//! # 为什么是 GDI BitBlt 而不是 Windows.Graphics.Capture
//!
//! - BitBlt 同步返回一帧位图，无 D3D11 设备、无 WinRT 异步帧池，
//!   与现有「ash 直驱 + 帧内上传」的结构零摩擦；
//! - 捕获区域只有窗口客户区（约 720×800），30fps 节流下 CPU 开销
//!   约 1ms/次，远低于引入 WGC 的复杂度；
//! - WGC 抓整屏还要自己裁剪窗口区域，且帧回调线程与渲染线程的
//!   同步成本更高。
//!
//! # alpha 通道
//!
//! BitBlt 读回的 BGRA 里 alpha 字节是**未定义的**（GDI 不写 alpha）。
//! 背景图会以「不透明底图」进入模糊链与合成（混合关闭的 copy 管线），
//! 若 alpha 是垃圾值，模糊后写进交换链的 alpha 也不可预期——在
//! 预乘合成下窗口可能整块变透明。因此读回后**统一强制 alpha=255**。
//!
//! 字节序：DIB 恒为 BGRA；交换链格式多为 `B8G8R8A8`（可直接整块上传），
//! 若实际选中的是 `R8G8B8A8` 则由调用方传入 `swap_rb = true`，此处顺带
//! 完成 R/B 交换——一次 CPU 遍历同时做完两件事，避免两次扫内存。

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, ClientToScreen, CreateCompatibleDC, CreateDIBSection,
    DeleteDC, DeleteObject, DIB_RGB_COLORS, GetDC, HALFTONE, HDC, HGDIOBJ, ReleaseDC,
    SelectObject, SetStretchBltMode, SRCCOPY, StretchBlt,
};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, IsIconic};

/// 捕获时的降采样倍数。
///
/// 底图最终会被高斯糊掉（模糊链在 1/4 分辨率上卷积），全分辨率读回
/// 纯属浪费：GDI 拷贝、CPU alpha 修正、staging 上传三处都按像素数计费。
/// 1/2 分辨率把这三处开销降到 1/4，再经模糊链的 1/4 降采样后总倍率
/// 仍是 1/4（与不降采样时 `blur_a` 的尺寸一致），观感无差别。
///
/// 实测教训：按全分辨率（520×800 → 约 1.66MB/帧）上传会与首帧的字体
/// 图集上传一起把 staging arena 的 3 个块全部占满，触发
/// 「全部在途、无可分配空间」而让启动失败——上传量必须小。
pub const CAPTURE_DOWNSCALE: u32 = 2;

/// 捕获尺寸：客户区尺寸除以 [`CAPTURE_DOWNSCALE`]，向上取整且至少为 1。
pub fn capture_size(width: u32, height: u32) -> (u32, u32) {
    let d = CAPTURE_DOWNSCALE.max(1);
    ((width.div_ceil(d)).max(1), (height.div_ceil(d)).max(1))
}

/// 两次捕获之间的最小间隔。
///
/// 窗口身后多半是静态桌面/壁纸，60fps 抓屏是纯浪费；30fps 已高于
/// 人眼对「磨砂背景内容更新」的感知阈值。实测单次 BitBlt + alpha
/// 修正在 720×800 下约 1ms，30fps 占帧预算的 3%，可控。
pub const CAPTURE_INTERVAL: Duration = Duration::from_millis(33);

/// 距上次捕获是否已到节流间隔（纯逻辑，供单测与调用方共用）。
///
/// `last = None`（从未捕获过）时必须返回 `true`：磨砂模式刚开启的
/// 第一帧就要有背景可用，否则首帧闪黑。
pub fn capture_due(last: Option<Instant>, now: Instant, interval: Duration) -> bool {
    match last {
        None => true,
        Some(t) => now.duration_since(t) >= interval,
    }
}

    /// 客户区尺寸 → 捕获尺寸。负值/零值钳到 1（`vkCreateImage` 不接受 0，
    /// 最小化窗口的 `GetClientRect` 会返回 0×0，但调用方已先排除该情况）。
    pub fn capture_extent(width: i32, height: i32) -> (u32, u32) {
        (width.max(1) as u32, height.max(1) as u32)
    }


/// 把 BitBlt 读回的 BGRA 像素修成可上传形态（纯逻辑）：
/// alpha 强制 255；`swap_rb` 时顺带交换 R/B（DIB 恒 BGRA → RGBA）。
pub fn fixup_pixels(buf: &mut [u8], swap_rb: bool) {
    for px in buf.chunks_exact_mut(4) {
        if swap_rb {
            px.swap(0, 2);
        }
        px[3] = 255;
    }
}

/// 一个窗口客户区的桌面捕获器：持有一组 GDI 资源（屏幕 DC、内存 DC、
/// DIB），按需重建 DIB 以匹配客户区尺寸。
///
/// 生命周期：每个启用磨砂的窗口一份，随渲染器销毁（`Drop` 释放 GDI）。
pub struct DesktopCapture {
    screen_dc: HDC,
    mem_dc: HDC,
    bitmap: HBITMAPGuard,
    /// DIB 内存指针（由 `CreateDIBSection` 写出，随位图一起销毁）。
    bits: *mut u8,
    /// DIB 当前尺寸（物理像素）。
    size: (u32, u32),
    /// 交换链为 RGBA 字节序时置真（见模块文档「字节序」）。
    swap_rb: bool,
}

// bits 指向 GDI 拥有的内存，仅在本结构的方法内读写，不跨线程共享。
unsafe impl Send for DesktopCapture {}

impl DesktopCapture {
    /// 创建捕获器。`swap_rb` 见模块文档「字节序」。
    pub fn new(swap_rb: bool) -> anyhow::Result<Self> {
        // SAFETY: GDI 句柄获取，无参数约束；失败时句柄为 0，下方显式判空。
        unsafe {
            let screen_dc = GetDC(None);
            if screen_dc.is_invalid() {
                anyhow::bail!("GetDC(None) 失败：拿不到桌面 DC");
            }
            let mem_dc = CreateCompatibleDC(Some(screen_dc));
            if mem_dc.is_invalid() {
                ReleaseDC(None, screen_dc);
                anyhow::bail!("CreateCompatibleDC 失败：拿不到内存 DC");
            }
            Ok(Self {
                screen_dc,
                mem_dc,
                bitmap: HBITMAPGuard::null(),
                bits: std::ptr::null_mut(),
                size: (0, 0),
                swap_rb,
            })
        }
    }

    /// 抓取 `hwnd`（以 `isize` 传入，由 UI 层从 `HWND` 转换）客户区
    /// 身后的桌面内容。
    ///
    /// 返回 `Some((bits, w, h))` 时 `bits.len() == w * h * 4`，BGRA（或
    /// `swap_rb` 时 RGBA）字节序、alpha 恒 255，可直接整块上传 GPU。
    ///
    /// 返回 `None`：窗口最小化（`IsIconic`）——此时 `GetClientRect`
    /// 给出无意义尺寸，跳过本帧即可，不算错误。
    ///
    /// 客户区尺寸与 DIB 不符时自动重建 DIB（窗口 resize 后的首帧）。
    pub fn capture_behind(&mut self, hwnd: isize) -> anyhow::Result<Option<(&[u8], u32, u32)>> {
        // SAFETY: hwnd 由调用方从活的窗口句柄转换而来；
        // 本结构持有的 DC 与位图均由 GDI 合法分配。
        unsafe {
            let hwnd = HWND(hwnd as *mut _);
            if IsIconic(hwnd).as_bool() {
                return Ok(None);
            }
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc)?;
            let (cw, ch) = capture_extent(rc.right - rc.left, rc.bottom - rc.top);
            let (w, h) = capture_size(cw, ch);
            self.ensure_size(w, h)?;

            // 客户区原点转屏幕坐标：桌面 DC 以虚拟屏幕为坐标系。
            // 用客户区而非 GetWindowRect：无边框窗口二者一致，但带
            // 不可见拖拽边框时 GetWindowRect 会多出几像素，上传尺寸
            // 就和交换链对不上。
            let mut origin = POINT { x: rc.left, y: rc.top };
            if !ClientToScreen(hwnd, &mut origin).as_bool() {
                anyhow::bail!("ClientToScreen 失败");
            }

            // HALFTONE：缩放时做盒式平均而非最近邻采样。底图最终会被
            // 高斯糊掉，最近邻的锯齿本来看不见，但 HALFTONE 能避免
            // 桌面上的细线（1px 分隔线、文字笔画）产生摩尔纹，
            // 代价只是多一次 GDI 内部卷积——30fps 下可忽略。
            // SAFETY: 两个 DC 与位图均由本结构持有且已 `ensure_size` 就绪。
            let _ = SetStretchBltMode(self.mem_dc, HALFTONE);
            // `StretchBlt` 在 windows 0.62 里返回 `BOOL`（不是 `Result`），
            // 因此不能用 `?`——显式判假并给出带上下文的错误。
            let ok = StretchBlt(
                self.mem_dc,
                0,
                0,
                w as i32,
                h as i32,
                Some(self.screen_dc),
                origin.x,
                origin.y,
                cw as i32,
                ch as i32,
                SRCCOPY,
            );
            if !ok.as_bool() {
                anyhow::bail!("StretchBlt 抓取桌面区域失败（区域可能在虚拟屏幕之外）");
            }

            let len = (w * h * 4) as usize;
            let slice = std::slice::from_raw_parts_mut(self.bits, len);
            fixup_pixels(slice, self.swap_rb);
            Ok(Some((&*slice, w, h)))
        }
    }

    /// DIB 尺寸不匹配时销毁重建。
    fn ensure_size(&mut self, w: u32, h: u32) -> anyhow::Result<()> {
        if self.size == (w, h) && !self.bitmap.handle.is_invalid() {
            return Ok(());
        }
        // SAFETY: 重建 DIB；旧位图先解除绑定再销毁，避免内存 DC 持有悬空句柄。
        unsafe {
            if !self.bitmap.handle.is_invalid() {
                SelectObject(self.mem_dc, null_bitmap());
                self.bitmap = HBITMAPGuard::null();
            }
            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    // 负高度 = 自顶向下行序（与 Vulkan 图像上传一致，省一次翻转）。
                    biWidth: w as i32,
                    biHeight: -(h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(
                Some(self.mem_dc),
                &bmi,
                DIB_RGB_COLORS,
                &mut bits_ptr,
                None,
                0,
            )?;
            let old = SelectObject(self.mem_dc, bmp.into());
            let _ = old; // 旧对象为 null_bitmap，无需恢复
            self.bitmap = HBITMAPGuard { handle: bmp };
            self.bits = bits_ptr as *mut u8;
            self.size = (w, h);
            let _ = &mut bmi;
            Ok(())
        }
    }
}

impl Drop for DesktopCapture {
    fn drop(&mut self) {
        // SAFETY: 释放本结构持有的全部 GDI 资源；顺序无关（彼此独立）。
        unsafe {
            self.bitmap = HBITMAPGuard::null();
            let _ = DeleteDC(self.mem_dc);
            let _ = ReleaseDC(None, self.screen_dc);
        }
    }
}

/// `HBITMAP` 的 RAII 包装：离开作用域即 `DeleteObject`。
struct HBITMAPGuard {
    handle: windows::Win32::Graphics::Gdi::HBITMAP,
}

impl HBITMAPGuard {
    fn null() -> Self {
        Self {
            handle: windows::Win32::Graphics::Gdi::HBITMAP::default(),
        }
    }
}

impl Drop for HBITMAPGuard {
    fn drop(&mut self) {
        // SAFETY: 句柄有效或为空（DeleteObject 对空句柄返回 FALSE，无害）。
        unsafe {
            if !self.handle.is_invalid() {
                let _ = DeleteObject(self.handle.into());
            }
        }
    }
}

/// `SelectObject` 恢复用的空对象（把内存 DC 与位图解绑）。
// SAFETY 说明：`HGDIOBJ::default()` 是空句柄，SelectObject 接受。
fn null_bitmap() -> HGDIOBJ {
    HGDIOBJ::default()
}

// ---------------------------------------------------------------------------
// 测试（纯逻辑，不建窗口、不碰 GDI）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_due_first_call_is_true() {
        let now = Instant::now();
        assert!(capture_due(None, now, CAPTURE_INTERVAL), "首帧必须有背景");
    }

    #[test]
    fn capture_due_respects_interval() {
        let t0 = Instant::now();
        assert!(!capture_due(Some(t0), t0 + Duration::from_millis(10), CAPTURE_INTERVAL));
        assert!(capture_due(Some(t0), t0 + CAPTURE_INTERVAL, CAPTURE_INTERVAL));
        assert!(capture_due(Some(t0), t0 + Duration::from_millis(500), CAPTURE_INTERVAL));
    }

    #[test]
    fn capture_extent_never_zero() {
        assert_eq!(capture_extent(720, 800), (720, 800));
        assert_eq!(capture_extent(0, 0), (1, 1), "最小化窗口的 0×0 钳到 1");
        assert_eq!(capture_extent(-5, 100), (1, 100), "负值钳到 1");
    }

    #[test]
    fn capture_size_halves_and_rounds_up() {
        assert_eq!(capture_size(720, 800), (360, 400));
        // 奇数尺寸向上取整：515/2 = 257.5 -> 258
        assert_eq!(capture_size(515, 1), (258, 1));
        assert_eq!(capture_size(1, 1), (1, 1), "极小窗口不得为 0");
    }

    #[test]
    fn downscale_cuts_upload_bytes_by_four() {
        // 回归：全分辨率上传会把 staging arena 撑爆（启动失败），
        // 降采样后上传量必须显著变小。
        let full = 520 * 800 * 4;
        let (w, h) = capture_size(520, 800);
        assert_eq!(w * h * 4, full / 4);
    }

    #[test]
    fn fixup_forces_alpha_without_swap() {
        // BGRA 直通（交换链同为 BGRA）：只改 alpha。
        let mut buf = vec![0x10, 0x20, 0x30, 0x00, 0x40, 0x50, 0x60, 0x7F];
        fixup_pixels(&mut buf, false);
        assert_eq!(buf, vec![0x10, 0x20, 0x30, 0xFF, 0x40, 0x50, 0x60, 0xFF]);
    }

    #[test]
    fn fixup_swaps_rb_when_requested() {
        // 交换链为 RGBA：BGRA → RGBA，同时 alpha 拉满。
        let mut buf = vec![0x10, 0x20, 0x30, 0x00];
        fixup_pixels(&mut buf, true);
        assert_eq!(buf, vec![0x30, 0x20, 0x10, 0xFF]);
    }

    #[test]
    fn fixup_ignores_trailing_bytes() {
        // 非 4 字节对齐的尾部不属于任何像素，不得越界或改写。
        let mut buf = vec![1, 2, 3, 0, 9];
        fixup_pixels(&mut buf, true);
        assert_eq!(buf[0..4], vec![3, 2, 1, 0xFF]);
        assert_eq!(buf[4], 9, "尾部散字节必须原样保留");
    }
}
