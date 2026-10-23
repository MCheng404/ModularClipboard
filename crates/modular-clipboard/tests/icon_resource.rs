//! exe 必须带图标资源。
//!
//! # 为什么要一条测试守这个
//!
//! 图标是**构建期**由 `build.rs` 调 `rc.exe` 缝进 exe 的，而那条链路
//! 处处都能静默失败：找不到 SDK、`.rc` 里的路径被转义吃掉、
//! `assets/icon.ico` 没生成…… `build.rs` 刻意选择「只警告不报错」，
//! 于是「图标没了」这件事在编译输出里只是一行 warning，很容易被刷过去。
//!
//! 症状会出现在用户那里：资源管理器、任务栏、Alt+Tab 全是一张白纸。
//! 所以在这里用 Win32 自己的 `ExtractIconExW` 数一遍——
//! 它数的就是 Shell 真正会用的那份资源。
//!
//! 注意：`ExtractIconExW` 在资源缺失时返回 0，不报错。

#![cfg(windows)]

use windows::Win32::UI::Shell::ExtractIconExW;
use windows::core::PCWSTR;

#[test]
fn exe_carries_an_icon_resource() {
    let exe = env!("CARGO_BIN_EXE_modular-clipboard");
    let wide: Vec<u16> = exe.encode_utf16().chain(std::iter::once(0)).collect();

    // nicons = 0 表示「只问有几个图标，不真的取句柄」。
    let count = unsafe { ExtractIconExW(PCWSTR(wide.as_ptr()), 0, None, None, 0) };

    assert!(
        count >= 1,
        "exe 里没有图标资源（ExtractIconExW 返回 {count}）。
         build.rs 可能没找到 rc.exe，或者 assets/icon.ico 缺失——
         跑 `python scripts/make_icons.py` 再重新构建。"
    );
}
