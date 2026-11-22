//! Gemini CLI 适配器：tests/fixtures/gemini 是手写的 `~/.gemini`（`projects.json` + `tmp/<项目>/chats/`）。
//! 样例覆盖控制行（`$set`、`$rewindTo`、`$patch`）、工具调用和结果、命令和工具返回不算发言、`displayContent`、
//! 子代理（嵌在父会话目录、直接放在 chats 下两种）、旧版整份 `.json`、迁移过的 `.json`/`.jsonl` 一对、
//! 0.30 以前的 sha256 目录、靠 `.project_root` 和靠 `projects.json` 认项目。

mod sources_common;

use std::fs;
use std::path::{Path, PathBuf};

use huitai::core::{Session, Vendor};
use huitai::index::Index;
use huitai::sources::{GeminiSource, Part, PreviewMessage, Role, Source, Trash};
use sources_common::{at, copy_tree, fixture, temp_dir};

const MAIN: &str = "1a2b3c4d-1111-4aaa-8bbb-000000000001";
const README: &str = "2b3c4d5e-2222-4aaa-8bbb-000000000002";
const LEGACY: &str = "3c4d5e6f-3333-4aaa-8bbb-000000000003";
const MIGRATED: &str = "4d5e6f70-4444-4aaa-8bbb-000000000004";

/// 把样例复制到临时目录：`<base>/gemini` 当 `~/.gemini`。
fn setup(name: &str) -> (PathBuf, GeminiSource) {
    let base = temp_dir(name);
    let gemini = base.join("gemini");
    copy_tree(&fixture("gemini"), &gemini);
    let src = GeminiSource::new(gemini.join("tmp"), Trash::new(base.join("trash")));
    (base, src)
}

fn by_id<'a>(sessions: &'a [Session], id: &str) -> &'a Session {
    sessions
        .iter()
        .find(|s| s.id.as_str() == id)
        .unwrap_or_else(|| panic!("没扫到 {id}"))
}

fn ids(src: &GeminiSource) -> Vec<String> {
    let mut ids: Vec<String> = src
        .scan()
        .sessions
        .iter()
        .map(|s| s.id.as_str().to_owned())
        .collect();
    ids.sort();
    ids
}

fn texts(msgs: &[PreviewMessage]) -> Vec<(Role, &str)> {
    msgs.iter().map(|m| (m.role, m.text.as_str())).collect()
}

#[test]
fn lists_conversations_from_both_registries() {
    let (_, src) = setup("gemini-scan");
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    // 子代理、只敲过命令的、sha256 旧目录里的副本都不列；迁移过的一对只列一条。
    assert_eq!(ids(&src), [MAIN, README, LEGACY, MIGRATED]);

    let main = by_id(&scan.sessions, MAIN);
    assert_eq!(main.vendor, Vendor::Gemini);
    assert_eq!(main.cwd, Path::new("/work/shop"), "按 .project_root");
    assert_eq!(main.title.as_deref(), Some("修结算按钮"), "$set 的 summary");
    // 第一句和回退之后那句；工具返回、/stats、被回退的那句不算。
    assert_eq!(main.message_count, 2);
    assert_eq!(
        main.last_active,
        at(1_795_000_180),
        "最后一个 $set 的 lastUpdated"
    );
    assert_eq!(main.git_branch, None);
    assert!(!main.automatic);
    assert!(
        main.path
            .ends_with("shop/chats/session-2026-11-18T11-06-1a2b3c4d.jsonl")
    );

    let readme = by_id(&scan.sessions, README);
    // 没有 summary：第一句话，取界面上显示的那份（@文件展开前）。
    assert_eq!(
        readme.title.as_deref(),
        Some("看 @README.md 写的部署步骤对不对")
    );
    assert_eq!(readme.message_count, 1);
    assert_eq!(
        readme.last_active,
        at(1_795_003_230),
        "消息比 lastUpdated 晚"
    );

    let legacy = by_id(&scan.sessions, LEGACY);
    assert_eq!(legacy.cwd, Path::new("/work/blog"), "按 projects.json");
    assert_eq!(legacy.title.as_deref(), Some("给博客加 RSS"));
    assert_eq!(legacy.message_count, 1);
    assert_eq!(legacy.last_active, at(1_772_442_300));

    let migrated = by_id(&scan.sessions, MIGRATED);
    assert_eq!(migrated.path.extension().unwrap(), "jsonl");
    assert_eq!(migrated.message_count, 2);
    assert_eq!(migrated.last_active, at(1_794_992_420));
}

#[test]
fn preview_transcript_and_exact_count() {
    let (_, src) = setup("gemini-preview");
    let scan = src.scan();
    let main = by_id(&scan.sessions, MAIN);

    let msgs = src.load_preview(main, 10).unwrap();
    assert_eq!(
        texts(&msgs),
        [
            (Role::User, "结算按钮点了没反应"),
            (Role::Assistant, "我先跑一下测试。"),
            (Role::Assistant, "优惠券算完后没有重新启用按钮，已修好。"),
            (Role::User, "移动端也一样吗"),
            (Role::Assistant, "移动端用的是同一个组件，一起修好了。"),
        ]
    );
    assert_eq!(msgs[3].at, Some(at(1_795_000_140)));
    assert_eq!(
        texts(&src.load_preview(main, 2).unwrap())[0].1,
        "移动端也一样吗"
    );

    let t = src.load_transcript(main).unwrap();
    assert!(!t.truncated);
    assert_eq!(t.parts.len(), 7, "{:?}", t.parts);
    assert_eq!(
        t.parts[2..4],
        [
            Part::ToolCall {
                name: "Shell".into(),
                input: "npm test".into(),
            },
            Part::ToolResult {
                text: "1 failing".into(),
            },
        ],
        "工具结果是后来的 $patch 补上的"
    );

    assert_eq!(src.count_messages(main).unwrap(), 2);
    let legacy = by_id(&scan.sessions, LEGACY);
    assert_eq!(
        texts(&src.load_preview(legacy, 10).unwrap()),
        [
            (Role::User, "给博客加 RSS"),
            (Role::Assistant, "加好了，在 /feed.xml。"),
        ]
    );
    assert_eq!(src.count_messages(legacy).unwrap(), 1);
    assert_eq!(src.load_transcript(legacy).unwrap().parts.len(), 2);
}

#[test]
fn resume_and_new_commands() {
    let (_, src) = setup("gemini-cmd");
    let scan = src.scan();
    let r = src.resume_cmd(by_id(&scan.sessions, MAIN));
    assert_eq!(r.shell_line(), format!("gemini --resume {MAIN}"));
    assert_eq!(r.cwd, Path::new("/work/shop"));
    let n = src.new_cmd(Path::new("/work/my app"));
    assert_eq!(n.shell_line(), "gemini");
    assert_eq!(n.cwd, Path::new("/work/my app"));
    assert!(
        Vendor::Gemini.info().presets_new_id,
        "新建时由会话台定 --session-id"
    );
}

#[test]
fn search_hits_title_and_body_but_not_rewound_text() {
    let (base, src) = setup("gemini-search");
    let index = Index::open(&base.join("index"), vec![Box::new(src)]).unwrap();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    let hits = |q: &str| -> Vec<(String, bool)> {
        index
            .search(Vendor::Gemini, q, 50)
            .unwrap()
            .iter()
            .map(|h| (h.entry.session.id.as_str().to_owned(), h.in_title))
            .collect()
    };
    assert_eq!(hits("结算按钮"), [(MAIN.to_owned(), true)]);
    assert_eq!(hits("优惠券"), [(MAIN.to_owned(), false)]);
    assert_eq!(hits("折叠按钮"), [(MIGRATED.to_owned(), false)]);
    assert!(hits("红色").is_empty(), "被回退的那段搜不到");
    assert!(hits("旧副本").is_empty(), "sha256 旧目录不进索引");
}

#[test]
fn trash_moves_legacy_twin_and_subagents_too() {
    let (base, src) = setup("gemini-trash");
    let chats = base.join("gemini/tmp/blog/chats");
    let scan = src.scan();
    let entry = src.trash(by_id(&scan.sessions, MIGRATED)).unwrap();
    assert!(
        !ids(&src).contains(&MIGRATED.to_owned()),
        "旧的 .json 没有冒出来"
    );
    assert!(
        !chats
            .join("session-2026-03-05T10-00-4d5e6f70.json")
            .exists()
    );
    src.restore(&entry).unwrap();
    assert!(ids(&src).contains(&MIGRATED.to_owned()));
    assert!(
        chats
            .join("session-2026-03-05T10-00-4d5e6f70.json")
            .exists()
    );

    let sub = base.join("gemini/tmp/shop/chats").join(MAIN);
    src.trash(by_id(&src.scan().sessions, MAIN)).unwrap();
    assert!(!sub.exists(), "子代理目录跟着移走");
}

#[test]
fn unknown_project_dir_is_reported() {
    let (base, src) = setup("gemini-orphan");
    let chats = base.join("gemini/tmp/orphan/chats");
    fs::create_dir_all(&chats).unwrap();
    fs::write(
        chats.join("session-2026-11-18T14-00-8b9c0d1e.jsonl"),
        concat!(
            r#"{"sessionId":"8b9c0d1e-8888-4aaa-8bbb-000000000008","projectHash":"x"}"#,
            "\n",
            r#"{"id":"o1","type":"user","content":"在吗"}"#,
            "\n"
        ),
    )
    .unwrap();
    let scan = src.scan();
    assert_eq!(scan.sessions.len(), 4);
    assert_eq!(scan.problems.len(), 1, "{:?}", scan.problems);
    assert!(scan.problems[0].contains("orphan"), "{}", scan.problems[0]);
}

#[test]
fn missing_root_is_empty_without_problems() {
    let base = temp_dir("gemini-missing");
    let src = GeminiSource::new(base.join("nope/tmp"), Trash::new(base.join("trash")));
    let scan = src.scan();
    assert!(scan.sessions.is_empty() && scan.problems.is_empty());
}

#[test]
fn watcher_only_cares_about_sessions_and_project_dirs() {
    let (base, src) = setup("gemini-affects");
    let tmp = base.join("gemini/tmp");
    assert_eq!(src.watch_roots(), std::slice::from_ref(&tmp));
    let a = |rel: &str| src.affects(&tmp.join(rel));
    assert!(a("shop"));
    assert!(a("shop/chats"));
    assert!(a("shop/chats/session-2026-11-18T11-06-1a2b3c4d.jsonl"));
    assert!(a("blog/chats/session-2026-03-02T09-00-3c4d5e6f.json"));
    assert!(!a("shop/.project_root"));
    assert!(!a("shop/logs/session-x.jsonl"));
    assert!(!a(&format!("shop/chats/{MAIN}/sub.jsonl")));
    assert!(!a("shop/chats/session-x.jsonl.tmp-123"));
    assert!(!src.affects(Path::new("/elsewhere/x.jsonl")));
}

/// 大文件扫描只读头尾：标题在尾部的 $set 里、消息数是下限；精确计数和预览照回放规则，
/// 尾部回退到头部那条时，之后的都作废。
#[test]
fn large_file_reads_head_and_tail_and_replays_rewinds() {
    let (base, src) = setup("gemini-large");
    let path = base.join("gemini/tmp/shop/chats/session-2026-11-19T08-00-9c0d1e2f.jsonl");
    let id = "9c0d1e2f-9999-4aaa-8bbb-000000000009";
    let pad = "x".repeat(400);
    let mut text = format!(
        r#"{{"sessionId":"{id}","projectHash":"h","startTime":"2026-11-19T08:00:00.000Z","lastUpdated":"2026-11-19T08:00:00.000Z"}}"#
    );
    text.push('\n');
    for i in 0..400 {
        text.push_str(&format!(
            r#"{{"id":"u{i}","timestamp":"2026-11-19T08:00:00.000Z","type":"user","content":"第 {i} 句 {pad}"}}"#
        ));
        text.push('\n');
        text.push_str(&format!(
            r#"{{"id":"g{i}","timestamp":"2026-11-19T08:00:00.000Z","type":"gemini","content":"答 {i}"}}"#
        ));
        text.push('\n');
    }
    text.push_str(r#"{"$set":{"summary":"很长的一次","lastUpdated":"2026-11-19T09:00:00.000Z"}}"#);
    text.push('\n');
    fs::write(&path, &text).unwrap();

    let scan = src.scan();
    let s = by_id(&scan.sessions, id);
    assert_eq!(s.title.as_deref(), Some("很长的一次"));
    assert!(
        s.message_count > 0 && s.message_count < 400,
        "{}",
        s.message_count
    );
    assert_eq!(s.last_active, at(1_795_078_800));
    assert_eq!(src.count_messages(s).unwrap(), 400);
    assert_eq!(src.load_preview(s, 1).unwrap()[0].text, "答 399");

    // 回退到第 10 句（在头部，尾部那段里没有）：之后的全作废。
    text.push_str("{\"$rewindTo\":\"u10\"}\n");
    text.push_str(
        r#"{"id":"u-new","timestamp":"2026-11-19T09:10:00.000Z","type":"user","content":"重来"}"#,
    );
    text.push('\n');
    fs::write(&path, &text).unwrap();
    assert_eq!(src.count_messages(s).unwrap(), 11);
    let tail = src.load_preview(s, 3).unwrap();
    assert_eq!(tail.last().unwrap().text, "重来");
    assert!(tail[0].text.starts_with("第 9 句"), "{:?}", tail[0].text);
    assert_eq!(tail[1].text, "答 9");
}
