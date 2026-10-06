//! 主循环「事件交付链」的源码级守卫。
//!
//! # 为什么需要读源码
//!
//! `EventLoop::take_pending()` 会把累积缓冲抽空，而 `egui_input` 收的是
//! **参数** `&[WindowEvent]`。库层测试能证明「事件可以被翻译进
//! `RawInput`」，却**测不到调用方是否真的把事件接上了**——
//! 这正是「UI 收不到任何输入」那次回归发生的层：真实缺陷形态是把
//! `egui_input(&ctx, &[])` 里的 `&frame_events` 换成 `&[]`，
//! 此时全工作区 359 个测试无一失败（已实测确认）。
//!
//! 类型签名消除了「两份状态」的**可能性**，但没防止「不传」。
//! 剩下的这个缺口只能靠源码断言补上。
//!
//! 读源码而非跑运行时验证，是因为本crate 的主循环需要真实 Vulkan 设备与
//! Win32 消息循环，无法在测试里构造；而这一条恰恰是**接线是否正确**
//! 的问题，不是运行时行为问题。

use std::path::Path;

/// `app.rs` 的路径（相对本测试文件）。
fn app_rs() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app.rs")
}

fn source() -> String {
    std::fs::read_to_string(app_rs())
        .unwrap_or_else(|e| panic!("读不到 {}: {e}", app_rs().display()))
}

/// 取含 `needle` 的那一行。
fn line_containing(src: &str, needle: &str) -> String {
    src.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("app.rs 里找不到含 `{needle}` 的行"))
        .trim()
        .to_string()
}

/// 守卫：`egui_input` 的第二实参必须是本帧取出的事件切片。
///
/// 回归守卫：曾把 `&frame_events` 写成 `&[]`（事件只喂应用层关闭判断、
/// 不喂 egui），导致整个 UI 收不到任何鼠标与键盘输入，
/// 而全工作区测试无一失败。
#[test]
fn egui_input_receives_frame_events_not_empty_slice() {
    let src = source();
    let line = line_containing(&src, "egui_input(&ctx");

    assert!(
        line.contains("&frame_events"),
        "egui_input 的第二实参必须是 `&frame_events`，实际是：{line}\n\
         若传 `&[]`，事件就只喂了应用层（关闭/resize 判断）而没喂 egui，\n\
         表现为整个 UI 收不到任何鼠标与键盘输入。"
    );
    assert!(
        !line.contains("&[]"),
        "egui_input 收到了空事件切片，UI 输入会全部丢失：{line}"
    );
}

/// 守卫：事件必须**只取一次**并复用于两处。
///
/// 双重消费（先`take_pending()` 给应用层、再 `poll()` 取一次给 egui）
/// 会让 egui 收到空输入——同一类缺陷的另一种形态。
#[test]
fn frame_events_are_taken_exactly_once() {
    let src = source();
    // 只数**真实调用**：跳过注释行，否则文档里提到 `take_pending()`
    // 也会被算进去（实测踩过）。真实调用必然含 `events.take_pending()`。
    let takes: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//") && l.contains("events.take_pending()"))
        .collect();

    assert_eq!(
        takes.len(),
        1,
        "events.take_pending() 应恰好调用一次（事件只有一份），实际 {} 次：{takes:?}
         双重消费会让 egui 拿到空输入——同一类缺陷的另一种形态。",
        takes.len()
    );
}

/// 守卫：`take_pending()` 必须在 `egui_input` 之前。
///
/// 顺序反了会让 egui 拿到空输入——`take_pending` 已把缓冲抽空。
#[test]
fn take_pending_precedes_egui_input() {
    let src = source();
    let take_at = src
        .find("take_pending()")
        .expect("app.rs 里找不到 take_pending()");
    let egui_at = src.find("egui_input(&ctx").expect("app.rs 里找不到 egui_input 调用");

    assert!(
        take_at < egui_at,
        "take_pending() 必须早于 egui_input()：反了会让 egui 拿到已被抽空的缓冲 \
         （真实回归过一次，表现为 UI 完全收不到输入）。take_at={take_at}, egui_at={egui_at}"
    );
}

/// 守卫：隐藏态分支必须 `continue` 回循环顶部，不能 `break` 掉循环。
///
/// 隐藏态走 `poll_for(...)` + `continue`。`continue` 回到循环顶部，
/// 那里会 `take_pending()`，所以关闭请求仍能交付。
/// 若改成 `break`，程序会在隐藏后直接退出循环，事件随之丢失。
///
/// ⚠️ 判据只针对**隐藏态分支自己的代码块**，不能扫整个主循环区间：
/// `decide_on_close` 的 `CloseDecision::Quit` 分支里有合法的 `break`，
/// 那是「无托盘兜底时关闭即退出」的正常语义（早先的写法扫错了范围，
/// 把那条合法 break 判成了缺陷）。
#[test]
fn hidden_branch_continues_instead_of_breaking() {
    let src = source();

    // 截出隐藏态分支的代码块：从 `if hidden {` 起，按花括号配平。
    let start = src.find("if hidden {").expect("找不到隐藏态分支");
    let body_start = start + "if hidden {".len();
    let mut depth = 1usize;
    let mut end = body_start;
    for (i, ch) in src[body_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = body_start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    assert_eq!(depth, 0, "隐藏态分支的花括号不配平，测试判据失效");
    let body = &src[body_start..end];

    // 只看可执行行，注释里提到 break/continue 不算。
    let code: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//") && !l.is_empty())
        .collect();

    assert!(
        code.iter().any(|l| *l == "continue;"),
        "隐藏态分支必须以 `continue;` 回到循环顶部，         否则隐藏后关闭请求等事件无人交付。实际代码行：{code:?}"
    );
    assert!(
        !code.iter().any(|l| l.starts_with("break")),
        "隐藏态分支里不能有 `break`：那会让程序隐藏后直接退出循环，         事件随之丢失。实际代码行：{code:?}"
    );
}
