//! 对话导出成 Markdown：9 家各用 fixtures 导出一份，和 tests/export_snapshots/<家>.md 比对（`UPDATE_SNAPSHOTS=1` 重写）；
//! 空对话、读不到的文件报清楚的错；存文件重名加序号、不覆盖。样例都是手写的。

mod sources_common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use huitai::core::{Account, Session, SessionId, Vendor};
use huitai::export::{self, Meta};
use huitai::index::Entry;
use huitai::sources::{
    AntigravitySource, ClaudeSource, CodexSource, GeminiSource, HermesSource, KimiSource,
    OpenCodeSource, PiSource, Source, Trash,
};
use sources_common::{at, copy_tree, fixture, temp_dir};

/// 快照固定按东八区排时间，不随跑测试的机器变。
fn east8(_: i64) -> i64 {
    8 * 3600
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

/// 一家的样例来源。
fn source(vendor: Vendor, base: &Path) -> Box<dyn Source> {
    let trash = Trash::new(base.join("trash"));
    match vendor {
        Vendor::Claude => Box::new(ClaudeSource::new(fixture("claude/projects"), trash)),
        Vendor::Pi => Box::new(PiSource::new(fixture("pi/sessions"), trash)),
        Vendor::Codex => {
            let root = base.join("codex");
            copy_tree(&fixture("codex/sessions"), &root.join("sessions"));
            fs::copy(
                fixture("codex/session_index.jsonl"),
                root.join("session_index.jsonl"),
            )
            .unwrap();
            let state = fs::read_to_string(fixture("codex/state.sql"))
                .unwrap()
                .replace("@ROOT@", &root.to_string_lossy());
            build_db(&root.join("state_5.sqlite"), &state);
            build_db(
                &root.join("thread_history_1.sqlite"),
                &fs::read_to_string(fixture("codex/history.sql")).unwrap(),
            );
            Box::new(CodexSource::new(&root, trash))
        }
        Vendor::Hermes => {
            let root = base.join("hermes");
            fs::create_dir_all(&root).unwrap();
            build_db(
                &root.join("state.db"),
                &fs::read_to_string(fixture("hermes/state.sql")).unwrap(),
            );
            Box::new(HermesSource::new(&root, trash))
        }
        Vendor::Kimi => {
            let root = base.join("kimi-code/sessions");
            copy_tree(&fixture("kimi/sessions"), &root);
            Box::new(KimiSource::new(&root, trash))
        }
        Vendor::Gemini => {
            let gemini = base.join("gemini");
            copy_tree(&fixture("gemini"), &gemini);
            Box::new(GeminiSource::new(gemini.join("tmp"), trash))
        }
        Vendor::OpenCode => {
            let root = base.join("opencode");
            fs::create_dir_all(&root).unwrap();
            let db = root.join("opencode.db");
            build_db(
                &db,
                &fs::read_to_string(fixture("opencode/opencode.sql")).unwrap(),
            );
            Box::new(OpenCodeSource::new(&db, trash))
        }
        Vendor::Qwen => unreachable!("Qwen Code 不再支持，不在 Vendor::ALL 里"),
        Vendor::Antigravity => {
            let root = base.join("antigravity");
            copy_tree(&fixture("antigravity/brain"), &root.join("brain"));
            build_db(
                &root.join("conversation_summaries.db"),
                &fs::read_to_string(fixture("antigravity/conversation_summaries.sql")).unwrap(),
            );
            Box::new(AntigravitySource::new(&root, trash))
        }
    }
}

/// 每家导出哪一条：消息、工具调用都有的那条。
fn sample_id(vendor: Vendor) -> &'static str {
    match vendor {
        Vendor::Claude => "11111111-1111-4111-8111-111111111111",
        Vendor::Codex => "cccccccc-0000-4000-8000-000000000003",
        Vendor::Pi => "aaaaaaaa-0000-4000-8000-000000000001",
        Vendor::Hermes => "20261120_101500_a1b2c3",
        Vendor::Kimi => "session_6b1f0c2e-4d3a-4b5c-9e8f-7a6b5c4d3e2f",
        Vendor::Gemini => "1a2b3c4d-1111-4aaa-8bbb-000000000001",
        Vendor::OpenCode => "ses_e11a9a1ffffeV1onlyAbCdEfGh",
        Vendor::Qwen => unreachable!("Qwen Code 不再支持"),
        Vendor::Antigravity => "a9e10000-1111-4aaa-8bbb-000000000001",
    }
}

fn scanned(src: &dyn Source, id: &str) -> Session {
    let scan = src.scan();
    scan.sessions
        .into_iter()
        .find(|s| s.id.as_str() == id)
        .unwrap_or_else(|| panic!("样例里没有 {id}"))
}

fn project_name(cwd: &Path) -> String {
    cwd.file_name().map_or_else(
        || "未知目录".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// 和 tests/export_snapshots/<name>.md 比；`UPDATE_SNAPSHOTS=1` 时重写。
fn assert_snapshot(name: &str, got: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/export_snapshots")
        .join(format!("{name}.md"));
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, got).unwrap();
        return;
    }
    let want = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("没有快照 {}，用 UPDATE_SNAPSHOTS=1 生成", path.display()));
    assert_eq!(got, want, "{} 和快照不一致", path.display());
}

#[test]
fn every_vendor_matches_snapshot() {
    for vendor in Vendor::ALL {
        let base = temp_dir(&format!("export-{}", vendor.info().key));
        let src = source(vendor, &base);
        let s = scanned(src.as_ref(), sample_id(vendor));
        let tr = export::load(src.as_ref(), &s).unwrap();
        let title = s.title.clone().unwrap_or_else(|| s.id.to_string());
        let project = project_name(&s.cwd);
        let meta = Meta {
            title: &title,
            account: None,
            project: &project,
            session: &s,
        };
        let md = export::markdown(&meta, &tr, &east8);
        assert!(md.starts_with("# "), "{vendor}: {md}");
        assert!(
            md.contains(&format!("- 厂商：{}", vendor.name())),
            "{vendor}"
        );
        assert!(md.contains("## 你"), "{vendor} 没有用户消息：{md}");
        assert!(
            md.contains(&format!("## {}", vendor.name())),
            "{vendor} 没有 AI 消息：{md}"
        );
        assert_snapshot(vendor.info().key, &md);
    }
}

fn plain_session(path: PathBuf) -> Session {
    Session {
        vendor: Vendor::Claude,
        id: SessionId::from("s1"),
        cwd: PathBuf::from("/Users/demo/projects/shop"),
        path,
        title: Some("看看登录页".to_owned()),
        git_branch: None,
        last_active: at(1_795_000_000),
        message_count: 1,
        automatic: false,
        account: String::new(),
    }
}

#[test]
fn empty_conversation_is_a_clear_error() {
    let base = temp_dir("export-empty");
    let path = base.join("projects/-p/s1.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "").unwrap();
    let src = ClaudeSource::new(base.join("projects"), Trash::new(base.join("trash")));
    let err = export::load(&src, &plain_session(path)).unwrap_err();
    assert!(err.to_string().contains("还没有内容"), "{err}");
}

#[test]
fn unreadable_file_is_a_clear_error() {
    let base = temp_dir("export-missing");
    let path = base.join("projects/-p/gone.jsonl");
    let src = ClaudeSource::new(base.join("projects"), Trash::new(base.join("trash")));
    let err = export::load(&src, &plain_session(path.clone())).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("读不了这条对话"), "{msg}");
    assert!(msg.contains(&path.display().to_string()), "{msg}");
}

#[test]
fn header_names_account_and_truncation_and_tool_lines() {
    let base = temp_dir("export-header");
    let path = base.join("projects/-p/s1.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        [
            r#"{"type":"user","timestamp":"2026-11-20T02:00:00Z","message":{"role":"user","content":"看看登录页"}}"#,
            r#"{"type":"assistant","timestamp":"2026-11-20T02:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"我先看文件"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"echo `date`"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"工具返回不导出"}]}}"#,
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    let src = ClaudeSource::new(base.join("projects"), Trash::new(base.join("trash")));
    let home = Path::new("/Users/demo");
    let work = Account::new(
        Vendor::Claude,
        "工作",
        PathBuf::from("/Users/demo/.claude-work"),
        home,
    );
    let mut s = plain_session(path);
    s.account = work.key.clone();
    s.last_active = at(1_795_140_360);
    let entry = Entry {
        session: s,
        note: Default::default(),
    };
    let md = export::render(&src, &entry, &[work], "商城").unwrap();
    assert!(md.contains("- 账号：工作\n"), "{md}");
    assert!(
        md.contains("- 项目：商城（/Users/demo/projects/shop）\n"),
        "{md}"
    );
    assert!(md.contains("- 工具 Bash：`` echo `date` ``\n"), "{md}");
    assert!(!md.contains("工具返回不导出"), "{md}");

    let mut tr = export::load(&src, &entry.session).unwrap();
    tr.truncated = true;
    let meta = Meta {
        title: "看看登录页",
        account: None,
        project: "shop",
        session: &entry.session,
    };
    let md = export::markdown(&meta, &tr, &east8);
    // 截断可能是数据库按条数截的，说法不能怪文件太大（审计 v0.1.4 第 26 条）。
    assert!(md.contains("更早的消息没有导出"), "{md}");
    assert!(!md.contains("MB"), "{md}");
    assert!(
        md.contains("- 时间：2026-11-20 10:00 – 10:06（UTC+08:00）\n"),
        "{md}"
    );
    assert!(!md.contains("账号"), "默认账号不写账号：{md}");
}

#[test]
fn save_numbers_duplicates_instead_of_overwriting() {
    let dir = temp_dir("export-save").join("Downloads");
    let a = export::save(&dir, "修登录-2026-11-20", "一").unwrap();
    let b = export::save(&dir, "修登录-2026-11-20", "二").unwrap();
    let c = export::save(&dir, "修登录-2026-11-20", "三").unwrap();
    assert_eq!(a, dir.join("修登录-2026-11-20.md"));
    assert_eq!(b, dir.join("修登录-2026-11-20-2.md"));
    assert_eq!(c, dir.join("修登录-2026-11-20-3.md"));
    assert_eq!(fs::read_to_string(&a).unwrap(), "一");
    assert_eq!(fs::read_to_string(&c).unwrap(), "三");
}
