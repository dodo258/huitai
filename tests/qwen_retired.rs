//! Qwen Code 不再支持（2026-11-23）：不列出、不扫描，但旧数据里的 `"qwen"` 还要解得开——旧守护进程的托管列表、
//! 被中断记录、`running.json`、账号设置、回收站。样例都是旧版写出来的格式（手写，不含真实数据）。

mod ui_support;

use std::fs;
use std::path::PathBuf;

use huitai::core::{Account, Config, SessionId, Vendor};
use huitai::host::{HostedInfo, Response, RunRecord, SpawnSpec, daemon::tab_prefix, interrupt};
use huitai::sources::{Trash, accounts, sources_for};

use ui_support::*;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("huitai-qwen-retired-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// 旧守护进程（0.1.3、0.1.4）托管着 Qwen 对话时回的 `List` 和 `Interrupted`：整份照解，别的对话不丢。
#[test]
fn old_daemon_replies_with_qwen_still_decode() {
    let list = r#"{"list":[
        {"id":"5e1c0a2b-1111-4aaa-8bbb-000000000001","vendor":"qwen","project":"shop","cwd":"/p/shop","pid":42,"attached":false,"title":null},
        {"id":"c1","vendor":"claude","project":"shop","cwd":"/p/shop","pid":null,"attached":false,"title":"x"}
    ]}"#;
    let Response::List(got) = serde_json::from_str::<Response>(list).unwrap() else {
        panic!("应当是 List")
    };
    let vendors: Vec<Vendor> = got.iter().map(|h: &HostedInfo| h.vendor).collect();
    assert_eq!(vendors, [Vendor::Qwen, Vendor::Claude]);

    let stopped = r#"{"interrupted":[
        {"id":"q1","vendor":"qwen","project":"shop","cwd":"/p/shop","started_at":7},
        {"id":"p1","vendor":"pi","project":"shop","cwd":"/p/shop","started_at":8}
    ]}"#;
    let Response::Interrupted(got) = serde_json::from_str::<Response>(stopped).unwrap() else {
        panic!("应当是 Interrupted")
    };
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].vendor, Vendor::Qwen);
}

/// 旧版会话台让新守护进程托管一条 Qwen 对话：请求解得开，标签页标题照旧写厂商名。
#[test]
fn old_client_spawn_of_qwen_decodes_and_titles() {
    let spec: SpawnSpec = serde_json::from_str(
        r#"{"id":"q1","vendor":"qwen","project":"shop","cwd":"/p/shop","command":"qwen --resume q1"}"#,
    )
    .unwrap();
    assert_eq!(spec.vendor, Vendor::Qwen);
    assert_eq!(tab_prefix(None, "shop", spec.vendor), "shop · Qwen Code · ");
}

/// 旧守护进程写的 `running.json` 里有 Qwen：那条照读，别的都在。
#[test]
fn running_file_with_qwen_loads() {
    let dir = temp("running");
    let path = interrupt::running_path(&dir);
    fs::write(
        &path,
        r#"[{"id":"q1","vendor":"qwen","project":"shop","cwd":"/p/shop","started_at":7},
            {"id":"c1","vendor":"claude","project":"shop","cwd":"/p/shop","started_at":8}]"#,
    )
    .unwrap();
    let got = interrupt::load(&path).unwrap();
    assert_eq!(got.len(), 2);
    let q: &RunRecord = &got[&SessionId::from("q1")];
    assert_eq!(q.vendor, Vendor::Qwen);
    let _ = fs::remove_dir_all(&dir);
}

/// 设置里手动加过 Qwen 账号：配置照读，但不认成账号、不建来源（不扫描）。
#[test]
fn qwen_account_setting_loads_but_is_not_scanned() {
    let dir = temp("config");
    let home = dir.join("h");
    fs::create_dir_all(home.join(".qwen/projects")).unwrap();
    fs::create_dir_all(home.join(".qwen-work/projects")).unwrap();
    let path = dir.join("config.json");
    fs::write(
        &path,
        r#"{"accounts":[{"vendor":"qwen","name":"工作号","dir":"~/.qwen-work"},{"vendor":"codex","name":"工作号","dir":"~/.codex-work"}]}"#,
    )
    .unwrap();
    let cfg = Config::load(&path).unwrap();
    assert!(cfg.accounts.iter().any(|a| a.vendor == Vendor::Qwen));
    let all = accounts(&home, &home, |_| None, &cfg.accounts);
    assert!(all.iter().all(|a| a.vendor != Vendor::Qwen), "{all:?}");
    assert!(
        all.iter()
            .any(|a| a.vendor == Vendor::Codex && a.name == "工作号")
    );
    // 直接给一个 Qwen 账号也不建来源。
    let q = Account::new(Vendor::Qwen, "默认", home.join(".qwen"), &home);
    assert!(sources_for(&[q], Trash::new(dir.join("trash"))).is_empty());
    let _ = fs::remove_dir_all(&dir);
}

/// 回收站里旧版删进来的 Qwen 对话：列得出来，能放回。
#[test]
fn trashed_qwen_session_lists_and_restores() {
    let dir = temp("trash");
    let file = dir.join("qwen/projects/-p-shop/chats/q1.jsonl");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "{}\n").unwrap();
    let trash = Trash::new(dir.join("trash"));
    trash
        .move_in(Vendor::Qwen, "q1", std::slice::from_ref(&file))
        .unwrap();
    let listed = trash.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].vendor, Vendor::Qwen);
    trash.restore(&listed[0]).unwrap();
    assert!(file.is_file());
    let _ = fs::remove_dir_all(&dir);
}

/// 界面只有八家；Qwen 的值落进来（旧守护进程报来的托管、旧缓存）也不显示、不越界。
#[test]
fn board_shows_eight_vendors_and_ignores_qwen() {
    let mut app = demo_app(Vendor::Claude);
    assert_eq!(app.shown.len(), 8);
    assert!(!app.shown.contains(&Vendor::Qwen));
    app.set_entries(Vendor::Qwen, claude_entries());
    assert!(!app.list_of(Vendor::Qwen).entries.is_empty());
    let text = dump(&render(&mut app, 200, 30));
    let top = text.lines().next().unwrap();
    assert!(!top.contains("Qwen"), "{top}");
    for v in Vendor::ALL {
        assert!(top.contains(v.name()), "顶栏缺 {}：{top}", v.name());
    }
    // Tab 走一圈回到 Claude，不会切进 Qwen。
    for _ in 0..Vendor::ALL.len() {
        app.key(key(ratatui::crossterm::event::KeyCode::Tab));
        assert_ne!(app.vendor, Vendor::Qwen);
    }
    assert_eq!(app.vendor, Vendor::Claude);
}
