//! 端到端集成测试：验证「捕获 → 过滤 → 去重 → 入库 → 检索 → 联动」全链路。
//!
//! 这些测试不触碰真实剪贴板，而是直接构造 `CapturedPayload` 注入服务层，
//! 因此可重复执行且不会干扰用户当前的剪贴板内容。

use modular_clipboard_app::Service;
use modular_clipboard_core::{ClipKind, Config};

/// 每个测试用独立数据目录，避免相互干扰。
fn svc() -> Service {
    Service::in_memory()
}

fn text_payload(text: &str, app: &str) -> modular_clipboard_core::CapturedPayload {
    modular_clipboard_core::CapturedPayload {
        kind: ClipKind::Text,
        bytes: text.as_bytes().to_vec(),
        html: None,
        text: Some(text.to_string()),
        source_app: app.to_string(),
        files: Vec::new(),
    }
}

#[test]
fn ingest_creates_item_and_appears_in_list() {
    let mut s = svc();
    assert_eq!(s.state.items.len(), 0);

    assert!(s.ingest(text_payload("hello world", "notepad")));
    assert_eq!(s.state.items.len(), 1);
    assert_eq!(s.state.items[0].preview, "hello world");
    assert_eq!(s.state.items[0].source_app, "notepad");
}

#[test]
fn whitespace_only_is_rejected_and_counted() {
    let mut s = svc();
    s.ingest(text_payload("   \n\t ", "notepad"));
    assert!(s.state.items.is_empty());
    assert_eq!(s.state.stats.empty, 1);
}

#[test]
fn blocked_app_is_rejected() {
    let mut s = svc();
    s.ingest(text_payload("my master password", "1Password.exe"));
    assert!(s.state.items.is_empty());
    assert_eq!(s.state.stats.app_blocked, 1);
}

#[test]
fn duplicate_within_window_refreshes_instead_of_duplicating() {
    let mut s = svc();
    s.ingest(text_payload("same content", "notepad"));
    s.ingest(text_payload("same content", "chrome"));
    // 窗口内重复不应新增条目
    assert_eq!(s.state.items.len(), 1, "重复内容应复用而非新增");
    assert_eq!(s.state.stats.duplicate, 1);
}

#[test]
fn different_content_both_kept() {
    let mut s = svc();
    s.ingest(text_payload("content A", "notepad"));
    s.ingest(text_payload("content B", "notepad"));
    assert_eq!(s.state.items.len(), 2);
}

#[test]
fn search_finds_chinese_substring() {
    let mut s = svc();
    s.ingest(text_payload("剪贴板历史记录", "notepad"));
    s.ingest(text_payload("unrelated content", "notepad"));

    s.search("剪贴板".into());
    assert_eq!(s.state.items.len(), 1, "中文子串检索应生效");
    assert_eq!(s.state.items[0].preview, "剪贴板历史记录");
}

#[test]
fn search_cleared_restores_full_list() {
    let mut s = svc();
    s.ingest(text_payload("aaa", "app"));
    s.ingest(text_payload("bbb", "app"));
    s.search("aaa".into());
    assert_eq!(s.state.items.len(), 1);
    s.search(String::new());
    assert_eq!(s.state.items.len(), 2);
}

#[test]
fn search_with_no_match_yields_empty() {
    let mut s = svc();
    s.ingest(text_payload("something", "app"));
    s.search("zzzz-nothing".into());
    assert!(s.state.items.is_empty());
}

#[test]
fn pin_toggles_and_pinned_survives_retention() {
    let mut s = svc();
    s.ingest(text_payload("keep me", "app"));
    let id = s.state.items[0].id;

    s.toggle_pin(id).unwrap();
    assert!(s.state.items[0].pinned);
    s.toggle_pin(id).unwrap();
    assert!(!s.state.items[0].pinned);
}

#[test]
fn delete_removes_from_list() {
    let mut s = svc();
    s.ingest(text_payload("delete me", "app"));
    let id = s.state.items[0].id;
    s.delete_item(id).unwrap();
    assert!(s.state.items.is_empty());
}

#[test]
fn groups_can_be_created_and_items_moved() {
    let mut s = svc();
    let gid = s.create_group("工作").unwrap();
    assert_eq!(s.state.groups.len(), 1);

    s.ingest(text_payload("grouped item", "app"));
    let id = s.state.items[0].id;
    s.move_to_group(id, Some(gid)).unwrap();

    let filtered = s.state.items.iter().find(|i| i.id == id).unwrap();
    assert_eq!(filtered.group_id, Some(gid));
}

#[test]
fn url_item_offers_open_url_action() {
    let mut s = svc();
    s.ingest(text_payload("https://github.com/rust-lang", "chrome"));
    let item = s.state.items[0].clone();
    let props = s.proposals_for(&item);
    assert!(
        props.iter().any(|p| p.action == "open_url"),
        "URL 条目应提供浏览器打开动作，实际: {:?}",
        props.iter().map(|p| p.action).collect::<Vec<_>>()
    );
}

#[test]
fn path_item_offers_open_and_reveal() {
    let mut s = svc();
    s.ingest(text_payload(r"C:\Users\test\doc.txt", "explorer"));
    let item = s.state.items[0].clone();
    let props = s.proposals_for(&item);
    assert!(props.iter().any(|p| p.action == "open_external"));
    assert!(props.iter().any(|p| p.action == "reveal"));
}

#[test]
fn plain_text_has_no_file_actions() {
    let mut s = svc();
    s.ingest(text_payload("just some text", "notepad"));
    let item = s.state.items[0].clone();
    let props = s.proposals_for(&item);
    assert!(!props.iter().any(|p| p.action == "open_external"));
    assert!(!props.iter().any(|p| p.action == "reveal"));
}

#[test]
fn user_rule_overrides_builtin_inference() {
    let mut s = svc();
    s.ingest(text_payload("https://example.com", "chrome"));
    let item = s.state.items[0].clone();
    let before = s.proposals_for(&item).len();

    // 添加一条针对 open_url 的自定义规则
    s.state.config.rules.push(modular_clipboard_core::ActionRule {
        id: 99,
        name: "我的浏览器".into(),
        keyword: None,
        kind: None,
        action: "open_url".into(),
        arg: None,
        sort_order: 0,
    });
    let after = s.proposals_for(&item);
    let renamed = after.iter().find(|p| p.rule_id == Some(99));
    assert!(renamed.is_some(), "自定义规则应命中");
    assert_eq!(renamed.unwrap().label, "我的浏览器");
    assert_eq!(
        after.len(),
        before,
        "覆盖不应增加动作数量，只应改变来源"
    );
}

#[test]
fn clear_all_empties_list_and_index() {
    let mut s = svc();
    s.ingest(text_payload("first", "app"));
    s.ingest(text_payload("second", "app"));
    assert_eq!(s.state.items.len(), 2);

    s.clear_all().unwrap();
    assert!(s.state.items.is_empty());

    // 清空后检索也应无结果，说明 FTS 索引同样被清掉
    s.search("first".into());
    assert!(s.state.items.is_empty());
}

#[test]
fn ingestion_after_clear_still_works() {
    let mut s = svc();
    s.ingest(text_payload("before", "app"));
    s.clear_all().unwrap();
    s.ingest(text_payload("after", "app"));
    assert_eq!(s.state.items.len(), 1);
    assert_eq!(s.state.items[0].preview, "after");
}

#[test]
fn files_payload_produces_summary_preview() {
    let mut s = svc();
    s.ingest(modular_clipboard_core::CapturedPayload {
        kind: ClipKind::Files,
        bytes: b"C:\\a.txt\nC:\\b.txt\nC:\\c.txt".to_vec(),
        html: None,
        text: Some("C:\\a.txt\nC:\\b.txt\nC:\\c.txt".into()),
        source_app: "explorer".into(),
        files: vec!["C:\\a.txt".into(), "C:\\b.txt".into(), "C:\\c.txt".into()],
    });
    assert_eq!(s.state.items.len(), 1);
    assert!(
        s.state.items[0].preview.contains("3 个文件"),
        "实际: {}",
        s.state.items[0].preview
    );
}

#[test]
fn large_payload_is_dropped_but_metadata_survives() {
    let mut s = svc();
    // 设置一个很小的上限
    s.store.set_payload_limit(16);
    let big = "x".repeat(4096);
    s.ingest(text_payload(&big, "app"));

    assert_eq!(s.state.items.len(), 1, "元数据应保留");
    assert!(
        s.state.items[0].payload.is_none(),
        "超限载荷不应落盘"
    );
}

#[test]
fn retention_limits_total_items() {
    let mut s = svc();
    s.state.config.storage.max_items = Some(3);
    for i in 0..10 {
        s.ingest(text_payload(&format!("item {i}"), "app"));
    }
    // 置顶一条，应额外保留
    let first_id = s.state.items[0].id;
    s.toggle_pin(first_id).unwrap();
    for i in 10..20 {
        s.ingest(text_payload(&format!("later {i}"), "app"));
    }
    assert!(
        s.state.items.len() <= 4,
        "置顶 1 条 + 上限 3 条 = 最多 4，实际 {}",
        s.state.items.len()
    );
    assert!(
        s.state.items.iter().any(|i| i.pinned),
        "置顶条目必须保留"
    );
}

#[test]
fn disabled_capture_ignores_everything() {
    let mut s = svc();
    s.state.config.capture.enabled = false;
    assert!(!s.ingest(text_payload("should be ignored", "app")));
    assert!(s.state.items.is_empty());
}

#[test]
fn config_roundtrip_preserves_settings() {
    let mut cfg = Config::default();
    cfg.ui.window_width = 555.0;
    cfg.capture.poll_interval_ms = 777;
    cfg.rules.push(modular_clipboard_core::ActionRule {
        id: 1,
        name: "r".into(),
        keyword: Some("k".into()),
        kind: None,
        action: "open_url".into(),
        arg: Some("a".into()),
        sort_order: 3,
    });

    let dir = std::env::temp_dir().join(format!("tiez-cfg-rt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("c.json");
    cfg.save(&path).unwrap();
    let loaded = Config::load(&path).unwrap();

    assert_eq!(loaded, cfg, "配置应完整往返");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pump_without_capture_is_harmless() {
    let mut s = svc();
    // 未启动捕获线程时 pump 不应 panic
    assert!(!s.pump());
}
