//! 抓屏工作线程。
//!
//! # 为什么要把抓屏搬出主线程
//!
//! [`DesktopCapture::capture_behind`] 是 **GDI `BitBlt`**：同步阻塞、
//! 毫秒级（1080p 一帧 8MB，实测 20~50ms 量级）。它跑在帧循环里时，
//! 每一轮抓屏（30fps 节流）都会让主线程停摆——滚动列表时的「一顿一顿」
//! 有它一份，而它跟 GPU 一点关系都没有。
//!
//! 搬到工作线程后，主线程完全不阻塞：本帧发请求，拿**上一批**的结果。
//! 晚一帧无感——磨砂背景对 33ms 的延迟本来就不敏感，节流周期也是 33ms。
//!
//! # 生命周期
//!
//! 发送端 drop ⇒ 工作线程的 `recv()` 返回 `Err` ⇒ 线程自行退出。
//! [`CaptureWorker::shutdown`] 显式 join，避免进程退出时线程还在跑
//! （那时它持有的 GDI 对象会被系统回收，可能让退出路径抖动）。
//!
//! # 为什么只有一个工作线程
//!
//! `BitBlt` 的瓶颈在 GDI 的串行化（桌面 DC 是共享资源），多线程并发抓
//! 只会互相排队，不会更快。一个线程足够把主线程解放出来。

use std::sync::mpsc::{self, Receiver, Sender};

/// 抓屏请求。
pub(super) struct CaptureReq {
    /// 目标窗口（磨砂要抓它身后的桌面）。
    pub hwnd: isize,
    /// 交换链为 RGBA 字节序时置真（DIB 恒 BGRA，需要 swap）。
    pub swap_rb: bool,
}

/// 抓屏结果。
pub enum CaptureOut {
    /// 抓到了像素。
    Ok(Vec<u8>, u32, u32),
    /// 抓不到但不算错误（最小化 / 窗口不存在）。
    Skipped,
    /// 出错。
    Failed(String),
}

/// 抓屏工作线程的句柄。
pub struct CaptureWorker {
    tx: Sender<CaptureReq>,
    rx: Receiver<CaptureOut>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// 待处理的请求数（诊断用：堆压说明抓屏跟不上节流）。
    pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl CaptureWorker {
    /// 启动工作线程。
    ///
    /// NOTE: 故意**不实现 `Default`**。它会启动一个 OS 线程，而
    /// `Default` 的语义是「廉价的零值」——`#[derive(Default)]` 的结构体
    /// 一旦包含它，每次 derive 都会悄悄起一个线程。调用方显式写
    /// `CaptureWorker::new()`，意图才清楚。
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let (tx_req, rx_req) = mpsc::channel::<CaptureReq>();
        let (tx_out, rx_out) = mpsc::channel::<CaptureOut>();
        let pending = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let p2 = pending.clone();

        let handle = std::thread::Builder::new()
            .name("clipboard-capture".to_string())
            .spawn(move || {
                // `DesktopCapture` 归本线程所有：GDI 对象只在创建它的线程
                // 上用最稳妥（虽然 HDC 本身可以跨线程，但 DIB 的 bits 指针
                // 与 mem_dc 的生命周期绑在一起，没必要跨）。
                let mut cap: Option<crate::backdrop::DesktopCapture> = None;
                while let Ok(req) = rx_req.recv() {
                    p2.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    if cap.is_none() {
                        match crate::backdrop::DesktopCapture::new(req.swap_rb) {
                            Ok(c) => cap = Some(c),
                            Err(e) => {
                                let _ = tx_out.send(CaptureOut::Failed(format!("{e:#}")));
                                continue;
                            }
                        }
                    }
                    match cap.as_mut().expect("刚确保存在").capture_behind(req.hwnd) {
                        Ok(Some((bits, w, h))) => {
                            let _ = tx_out.send(CaptureOut::Ok(bits.to_vec(), w, h));
                        }
                        Ok(None) => {
                            let _ = tx_out.send(CaptureOut::Skipped);
                        }
                        Err(e) => {
                            let _ = tx_out.send(CaptureOut::Failed(format!("{e:#}")));
                        }
                    }
                }
                // 发送端已 drop：线程退出。
            })
            .expect("启动抓屏线程失败");

        Self {
            tx: tx_req,
            rx: rx_out,
            handle: Some(handle),
            pending,
        }
    }

    /// 发一次抓屏请求（**非阻塞**）。
    ///
    /// 上一次请求还没处理完时**丢弃本次**：堆积会让延迟线性累积，
    /// 而磨砂晚一帧完全无感，堆积晚十帧就是肉眼可见的拖影。
    pub fn request(&self, hwnd: isize, swap_rb: bool) {
        let n = self.pending.load(std::sync::atomic::Ordering::Relaxed);
        if n >= 1 {
            return;
        }
        if self
            .tx
            .send(CaptureReq { hwnd, swap_rb })
            .is_ok()
        {
            self.pending
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// 取回结果（**非阻塞**）。没有就绪的结果时返回 `None`。
    pub fn poll(&self) -> Option<CaptureOut> {
        self.rx.try_recv().ok()
    }

    /// 待处理请求数（诊断用）。
    pub fn pending(&self) -> usize {
        self.pending.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 停掉工作线程并等它退出。
    ///
    /// 发送端随 `CaptureWorker` 一起 drop（`self.tx` 是唯一 sender），
    /// 工作线程的 `recv()` 随即返回 `Err` 并自行退出；这里只负责 join，
    /// 避免进程退出时它还在跑（那时它持有的 GDI 对象会被系统回收）。
    pub fn shutdown(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}
