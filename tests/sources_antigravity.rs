//! Antigravity 适配器：conversation_summaries.db（测试时用 tests/fixtures/antigravity/conversation_summaries.sql 建，
//! WAL 模式，和真库一样）加 annotations/、brain/。样例都是手写的：在用的（两块正文、空的 AI 步）、没标题的、
//! 全文表标了归档的、标注文件里归档的、标注文件取消了归档的、子代理、没说过话的。

mod sources_common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use huitai::core::{Session, Vendor, VendorDirs};
use huitai::index::Index;
use huitai::sources::{
    AntigravitySource, Delivery, Part, Role, Source, Trash, accounts, source_of, sources_for,
};
use sources_common::{at, build_db, copy_tree, fixture, temp_dir};

const ACTIVE: &str = "a9e10000-1111-4aaa-8bbb-000000000001";
const UNTITLED: &str = "a9e10000-2222-4aaa-8bbb-000000000002";
const ARCHIVED: &str = "a9e10000-3333-4aaa-8bbb-000000000003";
const ANNOTATED: &str = "a9e10000-4444-4aaa-8bbb-000000000004";
const UNARCHIVED: &str = "a9e10000-5555-4aaa-8bbb-000000000005";
const SUBAGENT: &str = "a9e10000-6666-4aaa-8bbb-000000000006";
const EMPTY: &str = "a9e10000-7777-4aaa-8bbb-000000000007";

/// 样例数据目录（在 `base/agy` 下），和读它的来源。
fn setup(name: &str) -> (PathBuf, AntigravitySource) {
    let base = temp_dir(name);
    let root = base.join("agy");
    build_root(&root);
    let src = AntigravitySource::new(&root, Trash::new(base.join("trash")));
    (base, src)
}

fn build_root(root: &Path) {
    copy_tree(
        &fixture("antigravity/annotations"),
        &root.join("annotations"),
    );
    copy_tree(&fixture("antigravity/brain"), &root.join("brain"));
    let sql = fs::read_to_string(fixture("antigravity/conversation_summaries.sql")).unwrap();
    build_db(&root.join("conversation_summaries.db"), &sql);
}

fn by_id<'a>(sessions: &'a [Session], id: &str) -> &'a Session {
    sessions
        .iter()
        .find(|s| s.id.as_str() == id)
        .unwrap_or_else(|| panic!("没扫到 {id}"))
}

fn texts(msgs: &[huitai::sources::PreviewMessage]) -> Vec<(Role, &str)> {
    msgs.iter().map(|m| (m.role, m.text.as_str())).collect()
}

#[test]
fn lists_top_level_unarchived_conversations() {
    let (base, src) = setup("agy-scan");
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    let mut ids: Vec<&str> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
    ids.sort();
    // 全文表或标注文件里归档的、子代理、没说过话的不列；标注文件取消了归档的以它为准。
    assert_eq!(ids, [ACTIVE, UNTITLED, UNARCHIVED]);
    for gone in [ARCHIVED, ANNOTATED, SUBAGENT, EMPTY] {
        assert!(!ids.contains(&gone));
    }

    let a = by_id(&scan.sessions, ACTIVE);
    assert_eq!(a.vendor, Vendor::Antigravity);
    assert_eq!(a.title.as_deref(), Some("下单接口加幂等"));
    assert_eq!(a.cwd, Path::new("/Users/demo/projects/my shop"));
    assert_eq!(a.message_count, 3, "两块正文里的用户发言都算");
    assert_eq!(
        a.last_active,
        at(1_795_140_900) + Duration::from_millis(500)
    );
    assert_eq!(a.path, base.join("agy/brain").join(ACTIVE));
    assert!(!a.automatic);

    // 标题、预览列都空：取第一句话（多行的压成一行）；工作区第一个是远程的，取第一个本机目录；
    // 最后修改时间是零值，退用最后输入的时间。
    let u = by_id(&scan.sessions, UNTITLED);
    assert_eq!(u.title.as_deref(), Some("RSS 输出全文 别只给摘要"));
    assert_eq!(u.cwd, Path::new("/Users/demo/projects/blog"));
    assert_eq!(u.last_active, at(1_795_082_400));
    assert_eq!(u.message_count, 1);

    let x = by_id(&scan.sessions, UNARCHIVED);
    assert_eq!(x.cwd, PathBuf::new(), "没有工作区是零散对话");
}

#[test]
fn preview_and_transcript_follow_step_order() {
    let (_, src) = setup("agy-preview");
    let scan = src.scan();
    let a = by_id(&scan.sessions, ACTIVE);

    let all = src.load_preview(a, 10).unwrap();
    // 按步号排：两块正文接起来，同一块里用户、AI 交错；只调工具的 AI 步（没字）不算。
    assert_eq!(
        texts(&all),
        [
            (Role::User, "下单接口加幂等，重复点提交会下两单"),
            (Role::Assistant, "先看下单接口现在怎么处理重复请求。"),
            (Role::User, "幂等键放请求头还是 body？"),
            (
                Role::Assistant,
                "放请求头 Idempotency-Key，服务端存 24 小时。"
            ),
            (Role::User, "好，就这么做"),
            (
                Role::Assistant,
                "改好了：重复的键直接回上次的结果，补了测试。"
            ),
        ]
    );
    assert!(all.iter().all(|m| m.at.is_none()), "每条消息没有自己的时间");
    let last2 = src.load_preview(a, 2).unwrap();
    assert_eq!(texts(&last2), texts(&all[4..]));

    let t = src.load_transcript(a).unwrap();
    assert!(!t.truncated);
    assert_eq!(t.parts.len(), 6);
    assert_eq!(
        t.parts[0],
        Part::Message(huitai::sources::PreviewMessage {
            role: Role::User,
            text: "下单接口加幂等，重复点提交会下两单".into(),
            at: None,
        })
    );
    assert_eq!(src.count_messages(a).unwrap(), 3);

    let u = by_id(&scan.sessions, UNTITLED);
    assert_eq!(
        src.load_preview(u, 5).unwrap()[0].text,
        "RSS 输出全文\n别只给摘要",
        "多行的话照原样"
    );
}

/// 往样例库里加一条长对话：第 0 块是 9 MiB 的旧回复，第 1 块是最后一问一答。
fn add_long_history(root: &Path, id: &str) {
    let old_len = 9 * 1024 * 1024;
    let sql = format!(
        "insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,\
         user_step_byte_offsets,agent_step_byte_offsets,user_body,agent_body) values \
         ('{id}',0,0,1,'[[0,0]]','[[1,0]]','旧问题\n',replace(hex(zeroblob({half})),'0','旧')||char(10)),\
         ('{id}',1,2,3,'[[2,0]]','[[3,0]]','新问题\n','新回答。\n');",
        half = old_len / 2 / 3,
    );
    build_db(&root.join("conversation_summaries.db"), &sql);
}

/// 预览、「此刻」只要最后几条：从最新的块往前读，够条数就停，读满 8 MiB 也停，不把整段历史取出来。
#[test]
fn preview_reads_from_the_latest_chunk_backward() {
    let (base, src) = setup("agy-tail");
    let id = "a9e10000-8888-4aaa-8bbb-000000000008";
    add_long_history(&base.join("agy"), id);
    let s = Session {
        vendor: Vendor::Antigravity,
        id: huitai::core::SessionId::from(id),
        cwd: PathBuf::new(),
        path: base.join("agy/brain").join(id),
        title: None,
        git_branch: None,
        last_active: at(0),
        message_count: 2,
        automatic: false,
        account: String::new(),
    };
    assert_eq!(
        texts(&src.load_preview(&s, 1).unwrap()),
        [(Role::Assistant, "新回答。")]
    );
    // 往前是 9 MiB 的旧回复，超过 8 MiB 就不再读（jsonl 几家的预览也最多往回读 8 MiB）。
    assert_eq!(
        texts(&src.load_preview(&s, 10).unwrap()),
        [(Role::User, "新问题"), (Role::Assistant, "新回答。")]
    );
    assert_eq!(
        src.load_moment(&s).unwrap().summary.as_deref(),
        Some("新回答。")
    );
    // 整段对话的上限是 32 MiB：旧的也读得到，按步号排。
    let t = src.load_transcript(&s).unwrap();
    assert_eq!(t.parts.len(), 4);
    assert!(!t.truncated);
}

#[test]
fn search_hits_title_and_body() {
    let (base, src) = setup("agy-search");
    let index = Index::open(&base.join("index"), vec![Box::new(src)]).unwrap();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    let ids = |q: &str| -> Vec<(String, bool)> {
        index
            .search(Vendor::Antigravity, q, 50)
            .unwrap()
            .iter()
            .map(|h| (h.entry.session.id.as_str().to_owned(), h.in_title))
            .collect()
    };
    assert_eq!(ids("下单接口"), [(ACTIVE.to_owned(), true)]);
    assert_eq!(ids("Idempotency"), [(ACTIVE.to_owned(), false)]);
    assert_eq!(ids("feed.xml"), [(UNTITLED.to_owned(), false)]);
    // 不列的对话（归档的、子代理）搜不到。
    assert!(ids("打包脚本").is_empty());
    assert!(ids("状态机").is_empty());
}

#[test]
fn resume_and_new_commands() {
    let (_, src) = setup("agy-cmd");
    let scan = src.scan();
    let a = by_id(&scan.sessions, ACTIVE);
    let r = src.resume_cmd(a);
    assert_eq!(r.shell_line(), format!("agy --conversation {ACTIVE}"));
    assert_eq!(r.cwd, Path::new("/Users/demo/projects/my shop"));
    // 带话接着聊：只恢复，R 报要自己输入。
    assert_eq!(src.resume_with_message_cmd(a, "接着做").args, r.args);
    assert_eq!(src.message_delivery(a), Delivery::Manual);
    let n = src.new_cmd(Path::new("/work/my app"));
    assert_eq!(n.shell_line(), "agy");
    assert_eq!(n.cwd, Path::new("/work/my app"));
    assert!(!Vendor::Antigravity.info().presets_new_id);
}

#[test]
fn trash_only_hides() {
    let (base, src) = setup("agy-trash");
    let scan = src.scan();
    let entry = src.trash(by_id(&scan.sessions, UNTITLED)).unwrap();
    assert!(
        src.scan()
            .sessions
            .iter()
            .all(|s| s.id.as_str() != UNTITLED)
    );
    assert!(base.join("agy/conversation_summaries.db").is_file());
    src.restore(&entry).unwrap();
    assert!(
        src.scan()
            .sessions
            .iter()
            .any(|s| s.id.as_str() == UNTITLED)
    );
}

#[test]
fn missing_or_broken_db() {
    let base = temp_dir("agy-missing");
    let src = AntigravitySource::new(base.join("nope"), Trash::new(base.join("trash")));
    let scan = src.scan();
    assert!(scan.sessions.is_empty() && scan.problems.is_empty());

    let root = base.join("broken");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("conversation_summaries.db"), b"not a database").unwrap();
    let src = AntigravitySource::new(&root, Trash::new(base.join("trash")));
    let scan = src.scan();
    assert!(scan.sessions.is_empty());
    assert!(
        scan.problems[0].contains("conversation_summaries.db"),
        "{:?}",
        scan.problems
    );
}

#[test]
fn only_summaries_and_annotations_affect_the_list() {
    let root = PathBuf::from("/h/.gemini/antigravity-cli");
    let src = AntigravitySource::new(&root, Trash::new("/tmp/t"));
    assert_eq!(src.watch_roots(), std::slice::from_ref(&root));
    assert!(src.affects(&root.join("conversation_summaries.db")));
    assert!(src.affects(&root.join("conversation_summaries.db-wal")));
    assert!(src.affects(&root.join("annotations/x.pbtxt")));
    assert!(!src.affects(&root.join("conversation_summaries.db-shm")));
    assert!(!src.affects(&root.join("conversations/x.pb")));
    assert!(!src.affects(&root.join("brain/x/task.md")));
    assert!(!src.affects(&root.join("log/cli.log")));
    assert!(!src.affects(Path::new("/h/.gemini/tmp/x/chats/a.json")));
}

/// `agy` 的目录是主账号；Antigravity App 的 `~/.gemini/antigravity` 有摘要库时认成「App」账号，
/// 两个都不带改目录的变量（官方没有这样的变量）。
#[test]
fn app_directory_is_a_second_account() {
    let base = temp_dir("agy-accounts");
    let home = base.join("h");
    fs::create_dir_all(&home).unwrap();
    let mine = |home: &Path| {
        accounts(home, home, |_| None, &[])
            .into_iter()
            .filter(|a| a.vendor == Vendor::Antigravity)
            .collect::<Vec<_>>()
    };
    let only = mine(&home);
    assert_eq!(only.len(), 1);
    assert!(only[0].is_default());
    assert_eq!(only[0].dir, home.join(".gemini/antigravity-cli"));

    build_root(&home.join(".gemini/antigravity"));
    let both = mine(&home);
    assert_eq!(both.len(), 2, "{both:?}");
    let app = &both[1];
    assert_eq!(app.name, "App");
    assert_eq!(app.dir, home.join(".gemini/antigravity"));
    assert!(!app.is_default());
    assert!(app.env(&home).is_empty());
    // 环境变量改不了它的目录。
    let dirs = VendorDirs::new(&home, &home, |_| Some("/elsewhere".into()));
    assert_eq!(
        dirs.dir(Vendor::Antigravity),
        home.join(".gemini/antigravity-cli")
    );
}

/// 审计 v0.1.4 第 11 条：`agy` 和 App 两个账号的库里有同一个 id 的对话，删（只隐藏）App 那条，`agy` 那条还在；
/// 恢复也只放回 App 那条。以前按厂商 + 原生 id 记，两条一起消失。
#[test]
fn hiding_in_one_account_keeps_the_same_id_in_another() {
    let base = temp_dir("agy-hide-per-account");
    let home = base.join("h");
    build_root(&home.join(".gemini/antigravity-cli"));
    build_root(&home.join(".gemini/antigravity"));
    let all: Vec<_> = accounts(&home, &home, |_| None, &[])
        .into_iter()
        .filter(|a| a.vendor == Vendor::Antigravity)
        .collect();
    assert_eq!(all.len(), 2, "{all:?}");
    let sources = sources_for(&all, Trash::new(base.join("trash")));
    let listed = |i: usize| -> Vec<Session> { sources[i].scan().sessions };
    let has = |i: usize| listed(i).iter().any(|s| s.id.native() == UNTITLED);
    assert!(has(0) && has(1));

    let app_copy = listed(1)
        .into_iter()
        .find(|s| s.id.native() == UNTITLED)
        .unwrap();
    let src = source_of(&sources, &app_copy).expect("App 账号的来源");
    let entry = src.trash(&app_copy).unwrap();
    assert!(!has(1), "App 那条隐藏了");
    assert!(has(0), "agy 那条同 id 的不跟着隐藏");

    src.restore(&entry).unwrap();
    assert!(has(0) && has(1));
}
