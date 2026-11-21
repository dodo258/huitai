//! Claude 适配器：用 tests/fixtures/claude 的手写样例（正常、缺字段、坏行、空文件、无对话）。

mod sources_common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use huitai::core::{Session, Vendor};
use huitai::sources::{ClaudeSource, FoldGroup, Role, Source, Trash, fold_group};
use sources_common::{at, copy_tree, fixture, temp_dir};

const NORMAL: &str = "11111111-1111-4111-8111-111111111111";
const MISSING: &str = "22222222-2222-4222-8222-222222222222";
const BAD_LINES: &str = "33333333-3333-4333-8333-333333333333";
const HOME_LOOSE: &str = "66666666-6666-4666-8666-666666666666";

fn source(root: &Path, name: &str) -> ClaudeSource {
    ClaudeSource::new(root, Trash::new(temp_dir(name).join("trash")))
}

fn by_id(sessions: &[Session], id: &str) -> Session {
    sessions
        .iter()
        .find(|s| s.id.as_str() == id)
        .unwrap_or_else(|| panic!("没扫到 {id}"))
        .clone()
}

#[test]
fn scan_reads_all_fixture_kinds() {
    let src = source(&fixture("claude/projects"), "scan");
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    let mut ids: Vec<&str> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
    ids.sort();
    // 空文件（4444）和只有快照没有对话的（5555）不列。
    assert_eq!(ids, [NORMAL, MISSING, BAD_LINES, HOME_LOOSE]);
    assert!(scan.sessions.iter().all(|s| s.vendor == Vendor::Claude));
}

#[test]
fn normal_session_metadata() {
    let s = by_id(
        &source(&fixture("claude/projects"), "normal")
            .scan()
            .sessions,
        NORMAL,
    );
    // 最新的 custom-title 优先于 ai-title 和更早的 custom-title。
    assert_eq!(s.title.as_deref(), Some("修结算按钮"));
    assert_eq!(s.cwd, PathBuf::from("/Users/demo/projects/shop"));
    // 分支取最后一次出现的。
    assert_eq!(s.git_branch.as_deref(), Some("fix/checkout"));
    // 工具结果、isMeta、斜杠命令回显不算用户发言。
    assert_eq!(s.message_count, 2);
    assert_eq!(s.last_active, at(1_793_524_230)); // 2026-11-01T09:10:30Z
    assert!(s.path.ends_with(format!("{NORMAL}.jsonl")));
}

#[test]
fn missing_fields_fall_back() {
    let s = by_id(
        &source(&fixture("claude/projects"), "missing")
            .scan()
            .sessions,
        MISSING,
    );
    assert_eq!(
        s.cwd,
        PathBuf::from("/Users/demo/projects/shop"),
        "从目录名反推"
    );
    assert_eq!(
        s.title.as_deref(),
        Some("缺字段的会话：没有 cwd、分支和标题")
    );
    assert_eq!(s.git_branch, None);
    assert_eq!(s.message_count, 1);
    // 没有时间戳时用文件修改时间。
    assert_eq!(
        s.last_active,
        fs::metadata(&s.path).unwrap().modified().unwrap()
    );
}

#[test]
fn bad_lines_are_skipped() {
    let s = by_id(
        &source(&fixture("claude/projects"), "bad").scan().sessions,
        BAD_LINES,
    );
    assert_eq!(s.title.as_deref(), Some("坏行中间的第一句"));
    assert_eq!(s.git_branch.as_deref(), Some("dev"));
    assert_eq!(s.message_count, 1);
    assert_eq!(s.last_active, at(1_793_613_609)); // 2026-11-02T10:00:09Z
}

#[test]
fn summary_title_and_home_fold_group() {
    let s = by_id(
        &source(&fixture("claude/projects"), "home").scan().sessions,
        HOME_LOOSE,
    );
    assert_eq!(s.title.as_deref(), Some("随手问"));
    assert_eq!(
        fold_group(&s.cwd, Path::new("/Users/demo")),
        Some(FoldGroup::Loose)
    );
}

#[test]
fn preview_and_exact_count() {
    let src = source(&fixture("claude/projects"), "preview");
    let s = by_id(&src.scan().sessions, NORMAL);
    let p = src.load_preview(&s, 3).unwrap();
    let got: Vec<(Role, &str)> = p.iter().map(|m| (m.role, m.text.as_str())).collect();
    assert_eq!(
        got,
        [
            (Role::Assistant, "我先看一下结算组件。"),
            (Role::User, "改好了吗？"),
            (Role::Assistant, "改好了，按钮绑定的事件名写错了。"),
        ]
    );
    assert_eq!(p[2].at, Some(at(1_793_524_230)));
    assert!(src.load_preview(&s, 0).unwrap().is_empty());
    assert_eq!(src.count_messages(&s).unwrap(), 2);
    let bad = by_id(&src.scan().sessions, BAD_LINES);
    assert_eq!(src.count_messages(&bad).unwrap(), 1);
}

#[test]
fn commands() {
    let src = source(&fixture("claude/projects"), "cmd");
    let s = by_id(&src.scan().sessions, NORMAL);
    let c = src.resume_cmd(&s);
    assert_eq!(c.shell_line(), format!("claude --resume {NORMAL}"));
    assert_eq!(c.cwd, s.cwd);
    let n = src.new_cmd(Path::new("/Users/demo/projects/shop"));
    assert_eq!(n.shell_line(), "claude");
    assert_eq!(n.cwd, PathBuf::from("/Users/demo/projects/shop"));
}

#[test]
fn resume_with_message() {
    let src = source(&fixture("claude/projects"), "cmd-msg");
    let s = by_id(&src.scan().sessions, NORMAL);
    let c = src.resume_with_message_cmd(&s, "继续上次没做完的事。it's 「先检查」");
    assert_eq!(c.program, "claude");
    assert_eq!(
        c.args,
        ["--resume", NORMAL, "继续上次没做完的事。it's 「先检查」"]
    );
    assert_eq!(
        c.shell_line(),
        format!(r"claude --resume {NORMAL} '继续上次没做完的事。it'\''s 「先检查」'")
    );
    assert_eq!(c.cwd, s.cwd);
}

#[test]
fn trash_and_restore_move_file_and_companion_dir() {
    let root = temp_dir("claude-trash-root");
    copy_tree(&fixture("claude/projects"), &root);
    let companion = root.join(format!("-Users-demo-projects-shop/{NORMAL}/subagents"));
    fs::create_dir_all(&companion).unwrap();
    fs::write(companion.join("agent-1.jsonl"), b"{}\n").unwrap();

    let src = source(&root, "claude-trash");
    let s = by_id(&src.scan().sessions, NORMAL);
    let entry = src.trash(&s).unwrap();
    assert_eq!(entry.files.len(), 2);
    assert!(!s.path.exists() && !companion.exists());
    assert!(src.scan().sessions.iter().all(|x| x.id.as_str() != NORMAL));

    src.restore(&entry).unwrap();
    assert!(s.path.exists());
    assert!(companion.join("agent-1.jsonl").exists());
    assert_eq!(by_id(&src.scan().sessions, NORMAL), s);
}

/// 大文件只读头尾：cwd 在头里，标题在尾里，中间的长内容不读。
#[test]
fn large_file_reads_head_and_tail_only() {
    let root = temp_dir("claude-large");
    let dir = root.join("-Users-demo-projects-big");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("77777777-7777-4777-8777-777777777777.jsonl");
    let mut f = fs::File::create(&path).unwrap();
    let filler = "填".repeat(400);
    writeln!(f, r#"{{"type":"user","message":{{"role":"user","content":"开头的问题"}},"timestamp":"2026-11-03T08:00:00Z","turnPosition":{{"promptIndex":0}},"cwd":"/Users/demo/projects/big","gitBranch":"main"}}"#).unwrap();
    for i in 1..=300 {
        writeln!(f, r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{filler}"}}]}},"timestamp":"2026-11-03T09:00:00Z"}}"#).unwrap();
        if i % 10 == 0 {
            writeln!(f, r#"{{"type":"user","message":{{"role":"user","content":"第 {} 个问题"}},"timestamp":"2026-11-03T10:00:00Z","turnPosition":{{"promptIndex":{}}}}}"#, i / 10, i / 10).unwrap();
        }
    }
    writeln!(f, r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"最后的回答"}}]}},"timestamp":"2026-11-03T12:00:00Z","gitBranch":"feature/big"}}"#).unwrap();
    writeln!(f, r#"{{"type":"custom-title","customTitle":"大文件"}}"#).unwrap();
    drop(f);
    assert!(fs::metadata(&path).unwrap().len() > 256 * 1024);

    let src = source(&root, "claude-large-src");
    let scan = src.scan();
    let s = by_id(&scan.sessions, "77777777-7777-4777-8777-777777777777");
    assert_eq!(s.title.as_deref(), Some("大文件"));
    assert_eq!(s.cwd, PathBuf::from("/Users/demo/projects/big"));
    assert_eq!(s.git_branch.as_deref(), Some("feature/big"));
    assert_eq!(s.last_active, at(1_793_707_200)); // 2026-11-03T12:00:00Z
    // 尾部的 promptIndex 给出准确的发言数，和整文件精确计数一致。
    assert_eq!(src.count_messages(&s).unwrap(), 31);
    assert_eq!(s.message_count, 31);
    let p = src.load_preview(&s, 2).unwrap();
    assert_eq!(p[1].text, "最后的回答");
    assert_eq!(
        (p[0].role, p[0].text.as_str()),
        (Role::User, "第 30 个问题")
    );
}

/// 中途换过目录的对话：项目目录取「编码后等于文件所在目录名」的那个 cwd，恢复也在那里跑。
#[test]
fn moved_session_groups_by_storage_dir() {
    let src = source(&fixture("claude/moved"), "moved");
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);

    // 先 /Users/demo/a、后 /Users/demo/projects/b，文件在 b 的编码目录下：归 b。
    let b = by_id(&scan.sessions, "88888888-8888-4888-8888-888888888888");
    assert_eq!(b.cwd, PathBuf::from("/Users/demo/projects/b"));
    let c = src.resume_cmd(&b);
    assert_eq!(c.cwd, PathBuf::from("/Users/demo/projects/b"));
    assert_eq!(
        c.shell_line(),
        "claude --resume 88888888-8888-4888-8888-888888888888"
    );

    // agentq 任务：先项目目录、后 .claude/worktrees/x，文件在项目目录下：仍归项目。
    let shop = by_id(&scan.sessions, "99999999-9999-4999-8999-999999999999");
    assert_eq!(shop.cwd, PathBuf::from("/Users/demo/projects/shop"));
    assert_eq!(fold_group(&shop.cwd, Path::new("/Users/demo")), None);

    // 都对不上：取最后一个 cwd。
    let y = by_id(&scan.sessions, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
    assert_eq!(y.cwd, PathBuf::from("/Users/demo/y"));
}

#[test]
fn missing_root_is_empty_without_problems() {
    let scan = source(&temp_dir("claude-none").join("nope"), "claude-none-src").scan();
    assert!(scan.sessions.is_empty() && scan.problems.is_empty());
}

#[test]
fn headless_sessions_are_automatic() {
    let scan = source(&fixture("claude/headless"), "headless").scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    let auto = |id: &str| by_id(&scan.sessions, id).automatic;
    // 全程 `entrypoint: sdk-cli`（claude -p）：自动运行。
    assert!(auto("bbbbbbbb-1111-4111-8111-111111111111"));
    // 无头起的、后来在终端（cli）里接着聊过：算自己的。
    assert!(!auto("bbbbbbbb-2222-4222-8222-222222222222"));
    // 桌面端起的不算。
    assert!(!auto("bbbbbbbb-3333-4333-8333-333333333333"));
    // 没有 entrypoint 的旧文件不算。
    let old = source(&fixture("claude/projects"), "headless-old").scan();
    assert!(old.sessions.iter().all(|s| !s.automatic));
}
