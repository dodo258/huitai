//! Kimi Code 适配器：tests/fixtures/kimi/sessions（手写脱敏，结构照 2.7.2 源码和本机 2.1.1 写的数据）。
//! 样例覆盖起了标题的对话（注入、模型自己调的技能、子代理、工具调用）、老版本写法（workDir、字符串时间、
//! 占位标题、已归档）、只有注入没说过话的对话、Kimi 自己的索引缓存目录。

mod sources_common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use huitai::core::{Session, Vendor};
use huitai::index::Index;
use huitai::sources::{KimiSource, Part, PreviewMessage, Role, Source, Trash, kimi};
use sources_common::{at, copy_tree, fixture, temp_dir};

const APP: &str = "session_6b1f0c2e-4d3a-4b5c-9e8f-7a6b5c4d3e2f";
const OLD: &str = "session_0a1b2c3d-1111-4222-8333-944455566677";
const EMPTY: &str = "session_9f8e7d6c-2222-4333-8444-a55566677788";
const APP_WS: &str = "wd_app_3f2a9c1d7e6b";
const T: u64 = 1_795_000_000_000;

fn ms(n: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(n)
}

fn setup(name: &str) -> (PathBuf, PathBuf, KimiSource) {
    let base = temp_dir(name);
    let root = base.join("kimi-code/sessions");
    copy_tree(&fixture("kimi/sessions"), &root);
    let src = KimiSource::new(&root, Trash::new(base.join("trash")));
    (base, root, src)
}

fn by_id<'a>(sessions: &'a [Session], id: &str) -> &'a Session {
    sessions
        .iter()
        .find(|s| s.id.as_str() == id)
        .unwrap_or_else(|| panic!("没扫到 {id}"))
}

#[test]
fn lists_conversations_from_state_and_wire() {
    let (_, root, src) = setup("kimi-scan");
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    let mut ids: Vec<&str> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
    ids.sort();
    // 只有注入、没说过话的不列；归档了的照列。
    assert_eq!(ids, [OLD, APP]);

    let app = by_id(&scan.sessions, APP);
    assert_eq!(app.vendor, Vendor::Kimi);
    assert_eq!(app.cwd, Path::new("/work/app"));
    assert_eq!(app.title.as_deref(), Some("整理接口文档"));
    assert_eq!(app.message_count, 2, "注入的和模型自己调的技能不算");
    assert_eq!(app.last_active, ms(T + 60_000), "正文最后一条记录的时间");
    assert_eq!(
        app.path,
        root.join(APP_WS).join(APP).join("agents/main/wire.jsonl")
    );
    assert!(!app.automatic);

    let old = by_id(&scan.sessions, OLD);
    assert_eq!(old.cwd, Path::new("/work/blog"), "老版本把目录记在 workDir");
    assert_eq!(
        old.title.as_deref(),
        Some("把草稿里的图片压一遍"),
        "占位标题 New Session 不用，取第一句话"
    );
    assert_eq!(old.message_count, 1);
    // updatedAt 是字符串，比正文里（秒数）的最后时间晚。
    assert_eq!(old.last_active, at(1_795_161_900));
}

#[test]
fn both_files_are_session_files() {
    let (_, root, src) = setup("kimi-files");
    let files = src.session_files().unwrap().unwrap();
    let rel: Vec<String> = files
        .iter()
        .map(|f| f.strip_prefix(&root).unwrap().display().to_string())
        .collect();
    assert_eq!(
        rel,
        [
            format!("{APP_WS}/{APP}/agents/main/wire.jsonl"),
            format!("{APP_WS}/{APP}/state.json"),
            format!("{APP_WS}/{EMPTY}/agents/main/wire.jsonl"),
            format!("{APP_WS}/{EMPTY}/state.json"),
            format!("wd_blog_8d354d546691/{OLD}/agents/main/wire.jsonl"),
            format!("wd_blog_8d354d546691/{OLD}/state.json"),
        ]
    );
    // 两个文件读出的是同一条。
    let read = src.read_files(&files[..2]);
    let a = read[0].as_ref().unwrap().as_ref().unwrap();
    let b = read[1].as_ref().unwrap().as_ref().unwrap();
    assert_eq!(a, b);
}

#[test]
fn preview_and_transcript() {
    let (_, _, src) = setup("kimi-preview");
    let scan = src.scan();
    let app = by_id(&scan.sessions, APP);

    let msgs = src.load_preview(app, 3).unwrap();
    let texts: Vec<(Role, &str)> = msgs.iter().map(|m| (m.role, m.text.as_str())).collect();
    assert_eq!(
        texts,
        [
            (Role::Assistant, "分成订单、用户两块了。"),
            (Role::User, "旧链接要能跳过去"),
            (Role::Assistant, "旧路径都加了重定向。"),
        ]
    );
    assert_eq!(msgs[1].at, Some(ms(T + 40_000)));

    let t = src.load_transcript(app).unwrap();
    assert!(!t.truncated);
    assert_eq!(
        t.parts[..3],
        [
            Part::Message(PreviewMessage {
                role: Role::User,
                text: "接口文档按模块分目录".into(),
                at: Some(ms(T + 10_000)),
            }),
            Part::ToolCall {
                name: "Shell".into(),
                input: "ls docs/api".into(),
            },
            Part::ToolResult {
                text: "orders.md\nusers.md".into(),
            },
        ]
    );
    assert_eq!(t.parts.len(), 6, "思考块、注入、子代理不在整段对话里");

    let old = by_id(&scan.sessions, OLD);
    let first = &src.load_preview(old, 10).unwrap()[0];
    assert_eq!(first.text, "把草稿里的图片压一遍", "附件标签不算字");
    assert_eq!(first.at, Some(at(1_795_132_830)), "秒数也认");
    assert_eq!(src.count_messages(app).unwrap(), 2);
}

#[test]
fn resume_and_new_commands() {
    let (_, _, src) = setup("kimi-cmd");
    let scan = src.scan();
    let app = by_id(&scan.sessions, APP);
    let r = src.resume_cmd(app);
    assert_eq!(r.shell_line(), format!("kimi -S {APP}"));
    assert_eq!(
        r.cwd,
        Path::new("/work/app"),
        "-S 只认在对话自己的目录里恢复"
    );
    let n = src.new_cmd(Path::new("/work/my app"));
    assert_eq!(n.shell_line(), "kimi");
    assert_eq!(n.cwd, Path::new("/work/my app"));
}

#[test]
fn created_at_reads_state() {
    let (_, _, src) = setup("kimi-created");
    let scan = src.scan();
    assert_eq!(
        kimi::created_at(&by_id(&scan.sessions, APP).path),
        Some(ms(T))
    );
    assert_eq!(
        kimi::created_at(&by_id(&scan.sessions, OLD).path),
        Some(at(1_795_161_600))
    );
    assert_eq!(
        kimi::created_at(Path::new("/nope/agents/main/wire.jsonl")),
        None
    );
}

#[test]
fn search_hits_title_and_body() {
    let (base, _, src) = setup("kimi-search");
    let index = Index::open(&base.join("index"), vec![Box::new(src)]).unwrap();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    let ids = |q: &str| -> Vec<(String, bool)> {
        index
            .search(Vendor::Kimi, q, 50)
            .unwrap()
            .iter()
            .map(|h| (h.entry.session.id.as_str().to_owned(), h.in_title))
            .collect()
    };
    assert_eq!(ids("接口文档"), [(APP.to_owned(), true)]);
    assert_eq!(ids("重定向"), [(APP.to_owned(), false)]);
    // 注入的、子代理的、思考块里的都搜不到。
    assert!(ids("项目说明").is_empty());
    assert!(ids("优惠券").is_empty());
    assert!(ids("有哪些文件").is_empty());
}

/// 标题只写 state.json、正文只写 wire.jsonl：哪个变了，FSEvents 报来那一个路径就够把这条重读。
#[test]
fn either_file_changing_refreshes_the_conversation() {
    let (base, root, src) = setup("kimi-refresh");
    let index = Index::open(&base.join("index"), vec![Box::new(src)]).unwrap();
    index.refresh();
    let app = |index: &Index| {
        index
            .list(Vendor::Kimi)
            .unwrap()
            .into_iter()
            .find(|e| e.session.id.as_str() == APP)
            .unwrap()
            .session
    };
    let dir = root.join(APP_WS).join(APP);

    // Kimi 起标题、改名只改 state.json。
    let state = dir.join("state.json");
    let renamed = fs::read_to_string(&state)
        .unwrap()
        .replace("\"整理接口文档\"", "\"接口文档拆分\"");
    fs::write(&state, renamed).unwrap();
    index.refresh_paths(Vendor::Kimi, std::slice::from_ref(&state));
    assert_eq!(app(&index).title.as_deref(), Some("接口文档拆分"));

    // 发一句话只追加 wire.jsonl。
    let wire = dir.join("agents/main/wire.jsonl");
    let mut f = OpenOptions::new().append(true).open(&wire).unwrap();
    writeln!(
        f,
        r#"{{"type":"context.append_message","agentId":"main","message":{{"role":"user","content":[{{"type":"text","text":"再加个目录页"}}],"origin":{{"kind":"user"}}}},"time":{}}}"#,
        T + 70_000
    )
    .unwrap();
    drop(f);
    index.refresh_paths(Vendor::Kimi, std::slice::from_ref(&wire));
    let s = app(&index);
    assert_eq!(s.message_count, 3);
    assert_eq!(s.last_active, ms(T + 70_000));
    assert_eq!(s.title.as_deref(), Some("接口文档拆分"));

    // 整家再扫一遍不丢、不重复。
    index.refresh();
    let all = index.list(Vendor::Kimi).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(app(&index).message_count, 3);
}

#[test]
fn trash_moves_the_whole_session_dir() {
    let (_, root, src) = setup("kimi-trash");
    let scan = src.scan();
    let dir = root.join(APP_WS).join(APP);
    let entry = src.trash(by_id(&scan.sessions, APP)).unwrap();
    assert!(!dir.exists());
    assert!(src.scan().sessions.iter().all(|s| s.id.as_str() != APP));
    src.restore(&entry).unwrap();
    assert!(dir.join("agents/sub_explore/wire.jsonl").is_file());
    assert!(src.scan().sessions.iter().any(|s| s.id.as_str() == APP));
}

#[test]
fn missing_root_and_broken_state() {
    let base = temp_dir("kimi-missing");
    let src = KimiSource::new(base.join("nope"), Trash::new(base.join("trash")));
    let scan = src.scan();
    assert!(scan.sessions.is_empty() && scan.problems.is_empty());

    let (_, root, src) = setup("kimi-broken");
    fs::write(
        root.join(APP_WS).join(APP).join("state.json"),
        b"{ not json",
    )
    .unwrap();
    let scan = src.scan();
    assert_eq!(scan.sessions.len(), 1, "坏的那条不影响别的");
    assert!(
        scan.problems.iter().any(|p| p.contains("state.json")),
        "{:?}",
        scan.problems
    );
}

#[test]
fn only_session_files_affect_the_list() {
    let root = PathBuf::from("/h/.kimi-code/sessions");
    let src = KimiSource::new(&root, Trash::new("/tmp/t"));
    assert_eq!(src.watch_roots(), std::slice::from_ref(&root));
    let s = root.join(APP_WS).join(APP);
    assert!(src.affects(&root.join(APP_WS)));
    assert!(src.affects(&s));
    assert!(src.affects(&s.join("state.json")));
    assert!(src.affects(&s.join("agents/main/wire.jsonl")));
    assert!(!src.affects(&s.join("agents/sub_explore/wire.jsonl")));
    assert!(!src.affects(&s.join("notify/state.json")));
    assert!(!src.affects(&s.join("logs/kimi-code.log")));
    assert!(!src.affects(&root.join(".index-dirty").join(format!("{APP}.1"))));
    assert!(!src.affects(&root.join(".index-cache")));
    assert!(!src.affects(Path::new("/h/.kimi-code/config.toml")));
}
