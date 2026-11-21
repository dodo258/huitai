//! Codex 适配器：状态库 + 正文库（测试时用 tests/fixtures/codex/*.sql 建）+ rollout 文件 + session_index。
//! 样例覆盖正常、缺字段、坏行、空文件、归档、子代理、库坏了、没有库。

mod sources_common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use huitai::core::{Session, Vendor};
use huitai::sources::{CodexSource, FoldGroup, Role, Source, Trash, fold_group};
use sources_common::{at, copy_tree, fixture, temp_dir};

const DB_THREAD: &str = "cccccccc-0000-4000-8000-000000000003";
const ROLLOUT_ONLY: &str = "bbbbbbbb-0000-4000-8000-000000000002";
const OLD_FORMAT: &str = "dddddddd-0000-4000-8000-000000000004";
const DB_NO_FILE: &str = "ffffffff-0000-4000-8000-000000000006";
const BAD_LINES: &str = "99999999-0000-4000-8000-000000000009";

/// 复制 fixtures 到临时目录；`with_db` 时按 SQL 样例建状态库和正文库。
fn setup(name: &str, with_db: bool) -> (PathBuf, CodexSource) {
    let base = temp_dir(name);
    let root = base.join("codex");
    copy_tree(&fixture("codex/sessions"), &root.join("sessions"));
    fs::copy(
        fixture("codex/session_index.jsonl"),
        root.join("session_index.jsonl"),
    )
    .unwrap();
    if with_db {
        let state = fs::read_to_string(fixture("codex/state.sql"))
            .unwrap()
            .replace("@ROOT@", &root.to_string_lossy());
        build_db(&root.join("state_5.sqlite"), &state);
        build_db(
            &root.join("thread_history_1.sqlite"),
            &fs::read_to_string(fixture("codex/history.sql")).unwrap(),
        );
        // 更旧编号的库应被忽略。
        fs::write(root.join("state_4.sqlite"), b"old").unwrap();
    }
    let src = CodexSource::new(&root, Trash::new(base.join("trash")));
    (root, src)
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

fn by_id(sessions: &[Session], id: &str) -> Session {
    sessions
        .iter()
        .find(|s| s.id.as_str() == id)
        .unwrap_or_else(|| panic!("没扫到 {id}"))
        .clone()
}

fn ids(sessions: &[Session]) -> Vec<&str> {
    let mut v: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    v.sort();
    v
}

#[test]
fn scan_merges_db_and_rollouts() {
    let (_, src) = setup("codex-scan", true);
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    // 归档的、子代理的（库里和 rollout 里各一个）、空文件不列；库里有的 rollout 不重复。
    assert_eq!(
        ids(&scan.sessions),
        [BAD_LINES, ROLLOUT_ONLY, DB_THREAD, OLD_FORMAT, DB_NO_FILE]
    );
    assert!(scan.sessions.iter().all(|s| s.vendor == Vendor::Codex));
}

#[test]
fn db_thread_metadata() {
    let (_, src) = setup("codex-db", true);
    let sessions = src.scan().sessions;
    let c = by_id(&sessions, DB_THREAD);
    assert_eq!(
        c.title.as_deref(),
        Some("索引里的名字"),
        "session_index 最新的名字优先"
    );
    assert_eq!(c.cwd, PathBuf::from("/Users/demo/projects/api"));
    assert_eq!(c.git_branch.as_deref(), Some("main"));
    assert_eq!(c.message_count, 2, "正文库里的用户发言数");
    assert_eq!(c.last_active, at(1_795_169_100));
    assert!(!c.automatic, "vscode 起的不是自动运行");

    let f = by_id(&sessions, DB_NO_FILE);
    assert_eq!(
        f.title.as_deref(),
        Some("只有第一句话"),
        "没名字时用第一句话"
    );
    assert_eq!(f.git_branch, None);
    assert_eq!(f.message_count, 1);
    assert_eq!(
        f.last_active,
        at(1_795_172_400),
        "没有毫秒字段时用 updated_at 秒"
    );
    assert!(f.automatic, "库里 source 为 exec（codex exec）是自动运行");
    assert_eq!(
        fold_group(&f.cwd, Path::new("/Users/demo")),
        Some(FoldGroup::Loose)
    );
}

#[test]
fn rollout_only_sessions() {
    let (_, src) = setup("codex-rollout", true);
    let sessions = src.scan().sessions;
    let b = by_id(&sessions, ROLLOUT_ONLY);
    assert_eq!(
        b.title.as_deref(),
        Some("加一个搜索接口"),
        "索引名是空白时用第一句话"
    );
    assert_eq!(b.cwd, PathBuf::from("/Users/demo/projects/api"));
    assert_eq!(b.git_branch.as_deref(), Some("feat/search"));
    assert_eq!(b.message_count, 2, "环境上下文那条 user 消息不算");
    assert_eq!(b.last_active, at(1_793_617_260));
    assert!(!b.automatic, "rollout 的 source 为 cli 不是自动运行");

    let d = by_id(&sessions, OLD_FORMAT);
    assert_eq!(
        d.cwd,
        PathBuf::from("/Users/demo/projects/legacy"),
        "缺 cwd 时用 turn_context"
    );
    assert_eq!(d.title.as_deref(), Some("旧格式缺 cwd 的会话"));
    assert_eq!(d.message_count, 1);
    assert_eq!(d.last_active, at(1_793_620_810));

    let bad = by_id(&sessions, BAD_LINES);
    assert_eq!(bad.message_count, 1);
    assert_eq!(bad.last_active, at(1_793_628_009));
    assert!(bad.automatic, "rollout 的 source 为 exec 是自动运行");
}

#[test]
fn preview_from_rollout_or_history_db() {
    let (_, src) = setup("codex-preview", true);
    let sessions = src.scan().sessions;
    // rollout 还在：从文件尾读。
    let c = by_id(&sessions, DB_THREAD);
    let texts: Vec<String> = src
        .load_preview(&c, 5)
        .unwrap()
        .into_iter()
        .map(|m| m.text)
        .collect();
    assert_eq!(texts, ["接口加分页", "rollout 里的回答"]);
    // rollout 没了：从正文库读。
    let f = by_id(&sessions, DB_NO_FILE);
    let p = src.load_preview(&f, 5).unwrap();
    let got: Vec<(Role, &str)> = p.iter().map(|m| (m.role, m.text.as_str())).collect();
    assert_eq!(
        got,
        [
            (Role::User, "只有第一句话"),
            (Role::Assistant, "库里的回答")
        ]
    );
    assert_eq!(p[1].at, Some(at(1_795_172_400)));
    assert_eq!(src.load_preview(&f, 1).unwrap()[0].text, "库里的回答");

    let bad = by_id(&sessions, BAD_LINES);
    let texts: Vec<String> = src
        .load_preview(&bad, 5)
        .unwrap()
        .into_iter()
        .map(|m| m.text)
        .collect();
    assert_eq!(texts, ["坏行旁边的问题", "坏行旁边的回答"]);
}

#[test]
fn exact_counts() {
    let (_, src) = setup("codex-count", true);
    let sessions = src.scan().sessions;
    assert_eq!(
        src.count_messages(&by_id(&sessions, ROLLOUT_ONLY)).unwrap(),
        2
    );
    assert_eq!(
        src.count_messages(&by_id(&sessions, OLD_FORMAT)).unwrap(),
        1
    );
    assert_eq!(
        src.count_messages(&by_id(&sessions, DB_NO_FILE)).unwrap(),
        1
    );
}

#[test]
fn trash_only_hides_and_restore_unhides() {
    let (_, src) = setup("codex-trash", true);
    let sessions = src.scan().sessions;
    for id in [DB_THREAD, ROLLOUT_ONLY] {
        let s = by_id(&sessions, id);
        let entry = src.trash(&s).unwrap();
        assert!(entry.hide_only);
        assert!(s.path.exists(), "Codex 的文件不移动");
        assert!(!ids(&src.scan().sessions).contains(&id), "隐藏后不再列出");
        src.restore(&entry).unwrap();
        assert_eq!(by_id(&src.scan().sessions, id), s);
    }
}

#[test]
fn without_db_uses_rollouts_and_index_names() {
    let (_, src) = setup("codex-nodb", false);
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    assert_eq!(
        ids(&scan.sessions),
        [BAD_LINES, ROLLOUT_ONLY, DB_THREAD, OLD_FORMAT]
    );
    let c = by_id(&scan.sessions, DB_THREAD);
    assert_eq!(c.title.as_deref(), Some("索引里的名字"));
    assert_eq!(c.message_count, 1);
    assert_eq!(c.last_active, at(1_795_161_620));
}

#[test]
fn broken_db_is_reported_and_rollouts_still_listed() {
    let (root, src) = setup("codex-broken", false);
    fs::write(root.join("state_5.sqlite"), b"this is not a database").unwrap();
    let scan = src.scan();
    assert_eq!(scan.problems.len(), 1, "{:?}", scan.problems);
    assert!(scan.problems[0].contains("state_5.sqlite"));
    assert!(ids(&scan.sessions).contains(&ROLLOUT_ONLY));
}

#[test]
fn commands() {
    let (_, src) = setup("codex-cmd", true);
    let c = by_id(&src.scan().sessions, DB_THREAD);
    assert_eq!(
        src.resume_cmd(&c).shell_line(),
        format!("codex resume {DB_THREAD}")
    );
    assert_eq!(src.resume_cmd(&c).cwd, c.cwd);
    assert_eq!(src.new_cmd(Path::new("/tmp/x")).shell_line(), "codex");
}

#[test]
fn resume_with_message() {
    let (_, src) = setup("codex-cmd-msg", true);
    let c = by_id(&src.scan().sessions, DB_THREAD);
    let cmd = src.resume_with_message_cmd(&c, "继续上次没做完的事");
    assert_eq!(cmd.program, "codex");
    assert_eq!(cmd.args, ["resume", DB_THREAD, "继续上次没做完的事"]);
    assert_eq!(
        cmd.shell_line(),
        format!("codex resume {DB_THREAD} '继续上次没做完的事'")
    );
    assert_eq!(cmd.cwd, c.cwd);
}

#[test]
fn missing_root_is_empty() {
    let src = CodexSource::new(
        temp_dir("codex-none").join("nope"),
        Trash::new(temp_dir("codex-none-t")),
    );
    let scan = src.scan();
    assert!(scan.sessions.is_empty() && scan.problems.is_empty());
}
