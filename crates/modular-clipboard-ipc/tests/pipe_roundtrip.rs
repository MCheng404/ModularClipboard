//! 真实命名管道往返。
//!
//! 单元测试里的 `MemoryTransport` 验证的是**帧编解码**，
//! 验证不了「`CreateNamedPipeW` 的参数组合真的能建出管道、
//! `CreateFileW` 真的能连上、双向读写不死锁」。
//!
//! 这三条正是整个双进程方案的底座：它们任何一条不成立，
//! 界面进程就会启动失败，而那时候排查成本极高（现象只是
//! 「双击没反应」）。所以必须有一条真管道的测试。

use modular_clipboard_core::{ClipItem, Config, DropStats};
use modular_clipboard_ipc::protocol::{Command, DataVersion, Request, Response, Snapshot};
use modular_clipboard_ipc::{Client, Server};

/// 每次运行独占的管道名。
///
/// CAUTION: 不能用默认名 `ModularClipboard.Daemon.v1`——那是**真实常驻
/// 进程**占着的固定串。测试进程去 bind 会拿到 `WIN32_ERROR(5)`（拒绝访问），
/// 客户端连过去则是 `ERROR_PIPE_BUSY`，于是这条测试在「开发机上开着
/// 常驻进程」时必然假失败。名字带 pid + 原子序号 + 随机分量（同
/// `store::open_in_memory` 的 SEC-005 做法），并行跑多个测试实例也不撞。
fn unique_pipe_name() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    use std::hash::{BuildHasher, RandomState};
    let rand = RandomState::new().hash_one((std::process::id(), n));
    format!(r"\\.\pipe\ModularClipboard.Test.{}-{n}-{rand:016x}", std::process::id())
}

fn snapshot(version: DataVersion) -> Snapshot {
    Snapshot {
        version,
        items: vec![ClipItem::new(
            modular_clipboard_core::ClipKind::Text,
            "deadbeef".into(),
            "hello".into(),
            "test".into(),
        )],
        groups: Vec::new(),
        stats: DropStats::default(),
        capturing: true,
        hotkey_error: Some("显示主窗口：Win+V 注册失败（测试用）".into()),
        config: Config::default(),
        list_version: 3,
    }
}

/// 一次完整的「界面连上来、握手、同步、下命令、断开」。
#[test]
fn client_and_server_talk_over_a_real_pipe() {
    let pipe = unique_pipe_name();
    let server = Server::bind_named(&pipe).expect("绑定管道");
    let handle = std::thread::spawn(move || {
        let mut conn = server.accept().expect("接受连接");
        loop {
            match conn.recv().expect("收请求") {
                None => break,
                Some(Request::Hello { version }) => {
                    assert_eq!(version, modular_clipboard_ipc::PROTOCOL_VERSION);
                    conn.send(&Response::Snapshot(snapshot(1))).expect("回快照");
                }
                Some(Request::Sync { known }) => {
                    if known == 1 {
                        conn.send(&Response::Unchanged).expect("回未变");
                    } else {
                        conn.send(&Response::Snapshot(snapshot(2))).expect("回快照");
                    }
                }
                Some(Request::Cmd(Command::Quit)) => {
                    conn.send(&Response::Ok).expect("回 Ok");
                }
                Some(Request::Cmd(other)) => {
                    conn.send(&Response::Error(format!("不支持：{other:?}")))
                        .expect("回错误");
                }
            }
        }
    });

    let mut client = Client::connect_named(&pipe, std::time::Duration::from_secs(5))
        .expect("连接常驻进程");
    let snap = client.hello().expect("握手");
    assert_eq!(snap.version, 1);
    assert_eq!(snap.items.len(), 1);
    assert_eq!(snap.list_version, 3);
    assert!(snap.capturing);

    // 版本未变 ⇒ 不回传列表（这是每帧同步的常态路径）。
    assert!(client.sync(1).expect("同步").is_none());
    // 版本变了 ⇒ 回传新快照。
    assert_eq!(client.sync(0).expect("同步").unwrap().version, 2);

    assert!(matches!(
        client.command(Command::Quit).expect("退出命令"),
        Response::Ok
    ));
    // 未实现的命令必须变成 Err 而不是让界面误以为成功。
    assert!(client.command(Command::CopyItem(1)).is_err());

    // 客户端 drop ⇒ 服务端 recv 得到 None ⇒ 线程退出。
    drop(client);
    handle.join().expect("服务端线程不应 panic");
}

/// 没有常驻进程时，连接必须**快速失败**并给出可理解的中文错误，
/// 而不是卡住或 panic。
#[test]
fn connect_without_server_fails_fast() {
    // 用一个必然无人监听的管道名：默认名可能真有常驻进程在跑。
    let err = modular_clipboard_ipc::transport::connect(r"\\.\pipe\ModularClipboard.NoSuchPipe")
        .expect_err("无人监听时应失败");
    let msg = err.to_string();
    assert!(msg.contains("失败"), "错误信息应可读，实际：{msg}");
}
