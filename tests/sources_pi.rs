//! pi 适配器：用 tests/fixtures/pi 的手写样例（正常、缺字段、坏行、空文件）。

mod sources_common;

use std::path::{Path, PathBuf};

use huitai::core::{Session, Vendor};
use huitai::sources::{PiSource, Role, Source, Trash};
use sources_common::{at, copy_tree, fixture, temp_dir};

const NORMAL: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const MISSING: &str = "aaaaaaaa-0000-4000-8000-000000000002";
const BAD_LINES: &str = "aaaaaaaa-0000-4000-8000-000000000003";

fn source(root: &Path, name: &str) -> PiSource {
    PiSource::new(root, Trash::new(temp_dir(name).join("trash")))
}

fn by_id(sessions: &[Session], id: &str) -> Session {
    sessions
        .iter()
        .find(|s| s.id.as_str() == id)
        .unwrap_or_else(|| panic!("没扫到 {id}"))
        .clone()
}

#[test]
fn scan_lists_sessions_and_skips_empty_file() {
    let scan = source(&fixture("pi/sessions"), "pi-scan").scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    let mut ids: Vec<&str> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, [NORMAL, MISSING, BAD_LINES]);
    assert!(scan.sessions.iter().all(|s| s.vendor == Vendor::Pi));
    // pi 的会话文件不记是不是无头起的，一律不算自动运行。
    assert!(scan.sessions.iter().all(|s| !s.automatic));
}

#[test]
fn normal_session_uses_latest_name() {
    let s = by_id(
        &source(&fixture("pi/sessions"), "pi-normal").scan().sessions,
        NORMAL,
    );
    assert_eq!(s.title.as_deref(), Some("博客订阅源"));
    assert_eq!(s.cwd, PathBuf::from("/Users/demo/projects/blog"));
    assert_eq!(s.git_branch, None);
    assert_eq!(s.message_count, 2, "只算用户发言，system、toolResult 不算");
    assert_eq!(s.last_active, at(1_795_169_100)); // 2026-11-20T10:05:00Z
}

#[test]
fn missing_fields_fall_back() {
    let s = by_id(
        &source(&fixture("pi/sessions"), "pi-missing")
            .scan()
            .sessions,
        MISSING,
    );
    assert_eq!(
        s.cwd,
        PathBuf::from("/Users/demo/projects/blog"),
        "从目录名反推"
    );
    // 名称被空的 session_info 清掉后，用第一句话。
    assert_eq!(s.title.as_deref(), Some("缺字段：没有 cwd，名称被清空"));
    assert_eq!(s.message_count, 1);
    assert_eq!(
        s.last_active,
        at(1_795_172_400),
        "没有消息时间时用会话头时间"
    );
}

#[test]
fn bad_lines_are_skipped() {
    let src = source(&fixture("pi/sessions"), "pi-bad");
    let s = by_id(&src.scan().sessions, BAD_LINES);
    assert_eq!(s.title.as_deref(), Some("坏行后面的问题"));
    assert_eq!(s.message_count, 1);
    assert_eq!(s.last_active, at(1_795_176_120));
    let p = src.load_preview(&s, 5).unwrap();
    let texts: Vec<&str> = p.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["坏行后面的问题", "坏行后面的回答"]);
}

#[test]
fn preview_count_and_commands() {
    let src = source(&fixture("pi/sessions"), "pi-preview");
    let s = by_id(&src.scan().sessions, NORMAL);
    let p = src.load_preview(&s, 2).unwrap();
    let got: Vec<(Role, &str)> = p.iter().map(|m| (m.role, m.text.as_str())).collect();
    assert_eq!(
        got,
        [
            (Role::User, "再加上 Atom"),
            (Role::Assistant, "Atom 也加好了。")
        ]
    );
    assert_eq!(p[1].at, Some(at(1_795_169_100)));
    assert_eq!(src.count_messages(&s).unwrap(), 2);

    assert_eq!(
        src.resume_cmd(&s).shell_line(),
        format!("pi --session {NORMAL}")
    );
    assert_eq!(src.resume_cmd(&s).cwd, s.cwd);
    assert_eq!(src.new_cmd(Path::new("/tmp/x")).shell_line(), "pi");
}

#[test]
fn resume_with_message_puts_it_after_double_dash() {
    let src = source(&fixture("pi/sessions"), "pi-msg");
    let s = by_id(&src.scan().sessions, NORMAL);
    // 以 - 或 @ 开头的话也不能被 pi 当成选项或文件。
    let c = src.resume_with_message_cmd(&s, "-继续 @上次");
    assert_eq!(c.program, "pi");
    assert_eq!(c.args, ["--session", NORMAL, "--", "-继续 @上次"]);
    assert_eq!(
        c.shell_line(),
        format!("pi --session {NORMAL} -- '-继续 @上次'")
    );
    assert_eq!(c.cwd, s.cwd);
}

#[test]
fn trash_and_restore() {
    let root = temp_dir("pi-trash-root");
    copy_tree(&fixture("pi/sessions"), &root);
    let src = source(&root, "pi-trash");
    let s = by_id(&src.scan().sessions, NORMAL);
    let entry = src.trash(&s).unwrap();
    assert!(!s.path.exists());
    assert!(src.scan().sessions.iter().all(|x| x.id.as_str() != NORMAL));
    src.restore(&entry).unwrap();
    assert_eq!(by_id(&src.scan().sessions, NORMAL), s);
}
