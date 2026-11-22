//! Hermes 适配器：state.db（测试时用 tests/fixtures/hermes/state.sql 建，WAL 模式，和真库一样）。
//! 样例覆盖本机各入口、消息平台、子代理、一次性跑、归档、压缩续写、空对话、回退的消息、cwd 为空、多模态正文。

mod sources_common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use huitai::core::{Session, Vendor};
use huitai::index::Index;
use huitai::sources::{HermesSource, Part, Role, Source, Trash};
use sources_common::{at, fixture, temp_dir};

const TUI: &str = "20261120_101500_a1b2c3";
const OLD_CLI: &str = "20261121_080000_b2c3d4";
const ONESHOT: &str = "20261121_100000_e5f6a7";
const COMPRESSED: &str = "20261121_120000_a7b8c9";
const CONTINUED: &str = "20261121_130000_b8c9d0";

fn setup(name: &str) -> (PathBuf, HermesSource) {
    let base = temp_dir(name);
    let root = base.join("hermes");
    fs::create_dir_all(&root).unwrap();
    build_db(
        &root.join("state.db"),
        &fs::read_to_string(fixture("hermes/state.sql")).unwrap(),
    );
    let src = HermesSource::new(&root, Trash::new(base.join("trash")));
    (base, src)
}

fn build_db(path: &Path, sql: &str) {
    let mut child = Command::new("sqlite3")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("测试需要 sqlite3（macOS 自带）");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(sql.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

fn by_id<'a>(sessions: &'a [Session], id: &str) -> &'a Session {
    sessions
        .iter()
        .find(|s| s.id.as_str() == id)
        .unwrap_or_else(|| panic!("没扫到 {id}"))
}

#[test]
fn lists_only_local_conversations() {
    let (_, src) = setup("hermes-scan");
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    let mut ids: Vec<&str> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
    ids.sort();
    // 消息平台、子代理、归档、压缩前的父行、没说过话的都不列。
    assert_eq!(ids, [TUI, OLD_CLI, ONESHOT, CONTINUED]);
    assert!(!ids.contains(&COMPRESSED));

    let tui = by_id(&scan.sessions, TUI);
    assert_eq!(tui.vendor, Vendor::Hermes);
    assert_eq!(tui.cwd, Path::new("/work/api"));
    assert_eq!(tui.title.as_deref(), Some("整理接口文档"));
    assert_eq!(tui.git_branch.as_deref(), Some("main"));
    assert_eq!(tui.message_count, 2, "被回退的那句不算");
    assert!(!tui.automatic);
    // 最后一条消息比 last_activity_at 还晚：取更晚的。
    assert_eq!(
        tui.last_active,
        at(1_795_000_400) + std::time::Duration::from_millis(750)
    );
    // 库是各条对话共用的，不当成会话文件。
    assert_eq!(tui.path, PathBuf::new());

    let old = by_id(&scan.sessions, OLD_CLI);
    assert_eq!(
        old.cwd,
        Path::new("/work/blog"),
        "cwd 空时退用 git_repo_root"
    );
    assert_eq!(old.title.as_deref(), Some("把草稿里的图片压一遍"));
    assert_eq!(old.last_active, at(1_795_100_100));

    assert!(by_id(&scan.sessions, ONESHOT).automatic);
    assert_eq!(
        by_id(&scan.sessions, CONTINUED).title.as_deref(),
        Some("修结算按钮 #2")
    );
}

#[test]
fn preview_and_transcript() {
    let (_, src) = setup("hermes-preview");
    let scan = src.scan();
    let tui = by_id(&scan.sessions, TUI);

    let msgs = src.load_preview(tui, 3).unwrap();
    let texts: Vec<(Role, &str)> = msgs.iter().map(|m| (m.role, m.text.as_str())).collect();
    // 只取有字的用户和助手消息，按时间先后；工具调用那条（正文空）、工具返回、回退的不算。
    assert_eq!(
        texts,
        [
            (Role::Assistant, "分成订单、用户两块了。"),
            (Role::User, "旧链接要能跳过去"),
            (Role::Assistant, "旧路径都加了重定向。"),
        ]
    );
    assert_eq!(msgs[1].at, Some(at(1_795_000_060)));

    let t = src.load_transcript(tui).unwrap();
    assert!(!t.truncated);
    assert_eq!(
        t.parts[..3],
        [
            Part::Message(huitai::sources::PreviewMessage {
                role: Role::User,
                text: "接口文档按模块分目录".into(),
                at: Some(at(1_795_000_010)),
            }),
            Part::ToolCall {
                name: "terminal".into(),
                input: "ls docs/api".into(),
            },
            Part::ToolResult {
                text: "orders.md\nusers.md".into(),
            },
        ]
    );
    assert_eq!(t.parts.len(), 6, "回退的那句不在整段对话里");

    let old = by_id(&scan.sessions, OLD_CLI);
    assert_eq!(
        src.load_preview(old, 10).unwrap()[0].text,
        "把草稿里的图片压一遍"
    );
    assert_eq!(src.count_messages(tui).unwrap(), 2);
}

#[test]
fn resume_and_new_commands() {
    let (_, src) = setup("hermes-cmd");
    let scan = src.scan();
    let tui = by_id(&scan.sessions, TUI);
    let r = src.resume_cmd(tui);
    assert_eq!(r.shell_line(), format!("hermes -r {TUI}"));
    assert_eq!(r.cwd, Path::new("/work/api"));
    let n = src.new_cmd(Path::new("/work/my app"));
    assert_eq!(n.shell_line(), "hermes --in '/work/my app'");
    assert_eq!(n.cwd, Path::new("/work/my app"));
}

#[test]
fn search_hits_title_and_body() {
    let (base, src) = setup("hermes-search");
    let index = Index::open(&base.join("index"), vec![Box::new(src)]).unwrap();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    let ids = |q: &str| -> Vec<(String, bool)> {
        index
            .search(Vendor::Hermes, q, 50)
            .unwrap()
            .iter()
            .map(|h| (h.entry.session.id.as_str().to_owned(), h.in_title))
            .collect()
    };
    assert_eq!(ids("接口文档"), [(TUI.to_owned(), true)]);
    assert_eq!(ids("重定向"), [(TUI.to_owned(), false)]);
    assert_eq!(ids("优惠券"), [(CONTINUED.to_owned(), false)]);
    // 不列的对话搜不到。
    assert!(ids("在吗").is_empty());
}

#[test]
fn trash_only_hides() {
    let (_, src) = setup("hermes-trash");
    let scan = src.scan();
    let entry = src.trash(by_id(&scan.sessions, OLD_CLI)).unwrap();
    assert!(src.scan().sessions.iter().all(|s| s.id.as_str() != OLD_CLI));
    src.restore(&entry).unwrap();
    assert!(src.scan().sessions.iter().any(|s| s.id.as_str() == OLD_CLI));
}

#[test]
fn missing_or_broken_db() {
    let base = temp_dir("hermes-missing");
    let src = HermesSource::new(base.join("nope"), Trash::new(base.join("trash")));
    let scan = src.scan();
    assert!(scan.sessions.is_empty() && scan.problems.is_empty());

    let root = base.join("broken");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("state.db"), b"this is not a database").unwrap();
    let src = HermesSource::new(&root, Trash::new(base.join("trash")));
    let scan = src.scan();
    assert!(scan.sessions.is_empty());
    assert!(scan.problems[0].contains("state.db"), "{:?}", scan.problems);
}

#[test]
fn only_db_and_wal_changes_affect_the_list() {
    let root = PathBuf::from("/h/.hermes");
    let src = HermesSource::new(&root, Trash::new("/tmp/t"));
    assert_eq!(src.watch_roots(), std::slice::from_ref(&root));
    assert!(src.affects(&root.join("state.db")));
    assert!(src.affects(&root.join("state.db-wal")));
    assert!(!src.affects(&root.join("state.db-shm")));
    assert!(!src.affects(&root.join("logs/agent.log")));
    assert!(!src.affects(&root.join("profiles/coder/state.db")));
}
