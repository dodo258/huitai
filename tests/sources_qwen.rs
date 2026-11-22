//! Qwen Code 适配器：tests/fixtures/qwen/projects（手写脱敏，结构照上游 0.28.3 的 chatRecordingService 写）。
//! 样例覆盖注入的上下文、思考块、工具调用和返回、自动标题和手动改名、`/rewind` 回退、流式回复分片、
//! 斜杠命令、通知、插件替发的话、插话、频道机器人起的、上游自己也不列的 agent-host、托管引擎的会话、
//! 只有系统记录的、归档的、附属文件、子代理目录、路径里带点（目录名有损）。

mod sources_common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use huitai::core::{Session, Vendor};
use huitai::index::Index;
use huitai::sources::{Part, PreviewMessage, QwenSource, Role, Source, Trash};
use sources_common::{at, copy_tree, fixture, temp_dir};

const SHOP: &str = "5e1c0a2b-1111-4aaa-8bbb-000000000001";
const CHANNEL: &str = "5e1c0a2b-2222-4aaa-8bbb-000000000002";
const BLOG: &str = "5e1c0a2b-7777-4aaa-8bbb-000000000007";
const DOT: &str = "5e1c0a2b-8888-4aaa-8bbb-000000000008";
/// 样例里的时间都从 2026-11-20T10:00:00Z 起算。
const T: u64 = 1_795_168_800;

fn setup(name: &str) -> (PathBuf, PathBuf, QwenSource) {
    let base = temp_dir(name);
    let root = base.join("qwen/projects");
    copy_tree(&fixture("qwen/projects"), &root);
    let src = QwenSource::new(&root, Trash::new(base.join("trash")));
    (base, root, src)
}

fn by_id<'a>(sessions: &'a [Session], id: &str) -> &'a Session {
    sessions
        .iter()
        .find(|s| s.id.as_str() == id)
        .unwrap_or_else(|| panic!("没扫到 {id}"))
}

fn shop_chats(root: &Path) -> PathBuf {
    root.join("-Users-demo-projects-shop/chats")
}

#[test]
fn lists_conversations() {
    let (_, root, src) = setup("qwen-scan");
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    let mut ids: Vec<&str> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
    ids.sort();
    // agent-host 起的、托管引擎的、只有系统记录的、归档的、附属文件、子代理都不列。
    assert_eq!(ids, [SHOP, CHANNEL, BLOG, DOT]);

    let shop = by_id(&scan.sessions, SHOP);
    assert_eq!(shop.vendor, Vendor::Qwen);
    assert_eq!(shop.cwd, Path::new("/Users/demo/projects/shop"));
    assert_eq!(shop.path, shop_chats(&root).join(format!("{SHOP}.jsonl")));
    assert_eq!(shop.title.as_deref(), Some("修结算按钮"));
    assert_eq!(shop.git_branch.as_deref(), Some("fix/checkout"));
    assert_eq!(shop.message_count, 2, "回退掉的那句不算");
    assert_eq!(
        shop.last_active,
        at(T + 58),
        "分片合成一条，时间取最晚的；末尾的界面快照不算"
    );
    assert!(!shop.automatic);

    let channel = by_id(&scan.sessions, CHANNEL);
    assert!(channel.automatic, "聊天频道机器人起的算自动运行");
    assert_eq!(channel.message_count, 1);

    let blog = by_id(&scan.sessions, BLOG);
    assert_eq!(
        blog.title.as_deref(),
        Some("给博客加一个 RSS"),
        "没有标题取第一句（老版本没有 displayText，取 parts）"
    );
    assert_eq!(
        blog.message_count, 2,
        "插话算；斜杠命令、通知、插件替发的不算"
    );
    assert_eq!(blog.git_branch.as_deref(), Some("main"));

    let dot = by_id(&scan.sessions, DOT);
    assert_eq!(
        dot.cwd,
        Path::new("/Users/demo/projects/my.app"),
        "目录名把点也换成了横杠，项目路径以记录里的为准"
    );
    assert_eq!(dot.title.as_deref(), Some("首页改版"), "改名以最后一次为准");
}

#[test]
fn preview_follows_the_current_branch() {
    let (_, _, src) = setup("qwen-preview");
    let scan = src.scan();
    let shop = by_id(&scan.sessions, SHOP);
    let msgs = src.load_preview(shop, 10).unwrap();
    let texts: Vec<(Role, &str)> = msgs.iter().map(|m| (m.role, m.text.as_str())).collect();
    assert_eq!(
        texts,
        [
            (Role::User, "结算按钮点了没反应"),
            (Role::Assistant, "优惠券算完后没有重新启用按钮，已修好。"),
            (Role::User, "移动端也看一下"),
            (Role::Assistant, "移动端走的是同一个组件，已经一起修好。"),
        ],
        "注入的上下文、思考块、回退掉的两句都不出现"
    );
    assert_eq!(msgs[2].at, Some(at(T + 50)));
    let last2 = src.load_preview(shop, 2).unwrap();
    assert_eq!(last2.len(), 2);
    assert_eq!(last2[0].text, "移动端也看一下");

    let blog = by_id(&scan.sessions, BLOG);
    let texts: Vec<String> = src
        .load_preview(blog, 10)
        .unwrap()
        .into_iter()
        .map(|m| m.text)
        .collect();
    assert_eq!(
        texts,
        [
            "给博客加一个 RSS",
            "只要最近 20 篇",
            "RSS 已生成在 /feed.xml，包含最近 20 篇文章。"
        ]
    );
}

#[test]
fn transcript_has_tools_and_no_rewound_turns() {
    let (_, _, src) = setup("qwen-transcript");
    let scan = src.scan();
    let t = src.load_transcript(by_id(&scan.sessions, SHOP)).unwrap();
    assert!(!t.truncated);
    assert_eq!(
        t.parts,
        [
            Part::Message(PreviewMessage {
                role: Role::User,
                text: "结算按钮点了没反应".into(),
                at: Some(at(T + 10)),
            }),
            Part::ToolCall {
                name: "run_shell_command".into(),
                input: "rg onCheckout src".into(),
            },
            Part::ToolResult {
                text: "src/cart.tsx:12".into(),
            },
            Part::Message(PreviewMessage {
                role: Role::Assistant,
                text: "优惠券算完后没有重新启用按钮，已修好。".into(),
                at: Some(at(T + 20)),
            }),
            Part::Message(PreviewMessage {
                role: Role::User,
                text: "移动端也看一下".into(),
                at: Some(at(T + 50)),
            }),
            Part::Message(PreviewMessage {
                role: Role::Assistant,
                text: "移动端走的是同一个组件，已经一起修好。".into(),
                at: Some(at(T + 58)),
            }),
        ]
    );

    // 工具返回没有 resultDisplay 时取 functionResponse 里的 output。
    let t = src.load_transcript(by_id(&scan.sessions, BLOG)).unwrap();
    assert!(t.parts.contains(&Part::ToolResult {
        text: "写好了 feed.xml".into()
    }));
    assert!(t.parts.contains(&Part::ToolCall {
        name: "write_file".into(),
        input: "feed.xml".into()
    }));
}

#[test]
fn exact_count_reads_the_whole_branch() {
    let (_, _, src) = setup("qwen-count");
    let scan = src.scan();
    assert_eq!(src.count_messages(by_id(&scan.sessions, SHOP)).unwrap(), 2);
    assert_eq!(src.count_messages(by_id(&scan.sessions, BLOG)).unwrap(), 2);
    let c = src
        .count_since(by_id(&scan.sessions, CHANNEL), None)
        .unwrap();
    assert_eq!(c.users, 1);
}

/// 文件大到只读头尾时：开头那段照收，结尾那段按分支滤；精确计数读整份按分支数。
#[test]
fn large_file_head_and_tail() {
    let (_, root, src) = setup("qwen-large");
    let path = shop_chats(&root).join(format!("{BLOG}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = fs::File::create(&path).unwrap();
    let pad = "字".repeat(400);
    let mut parent = String::from("null");
    let n = 400;
    for i in 0..n {
        let (kind, role, uuid) = if i % 2 == 0 {
            ("user", "user", format!("u{i}"))
        } else {
            ("assistant", "model", format!("a{i}"))
        };
        writeln!(
            f,
            r#"{{"uuid":"{uuid}","parentUuid":{parent},"sessionId":"{BLOG}","timestamp":"2026-11-20T10:30:00Z","type":"{kind}","cwd":"/Users/demo/projects/shop","message":{{"role":"{role}","parts":[{{"text":"第 {i} 句 {pad}"}}]}}}}"#
        )
        .unwrap();
        parent = format!("\"{uuid}\"");
    }
    // 回退掉最后一问一答，再问一句。
    writeln!(
        f,
        r#"{{"uuid":"rw","parentUuid":"a{}","sessionId":"{BLOG}","timestamp":"2026-11-20T11:00:01Z","type":"system","subtype":"rewind","cwd":"/Users/demo/projects/shop"}}"#,
        n - 3
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"uuid":"last","parentUuid":"rw","sessionId":"{BLOG}","timestamp":"2026-11-20T11:00:02Z","type":"user","cwd":"/Users/demo/projects/shop","message":{{"role":"user","parts":[{{"text":"最后一句"}}]}}}}"#
    )
    .unwrap();
    drop(f);
    assert!(fs::metadata(&path).unwrap().len() > 512 * 1024);

    let s = src.read_files(std::slice::from_ref(&path)).remove(0);
    let s = s.unwrap().unwrap();
    assert!(s.title.as_deref().unwrap().starts_with("第 0 句 "));
    assert!(
        s.message_count > 0 && s.message_count < 200,
        "头尾两段是下限"
    );
    assert_eq!(s.last_active, at(T + 3602), "最后活动取结尾那段的最后一条");
    assert_eq!(src.count_messages(&s).unwrap(), n / 2 - 1 + 1);
    let last = src.load_preview(&s, 2).unwrap();
    assert_eq!(last[1].text, "最后一句");
    assert!(last[0].text.starts_with(&format!("第 {} 句", n - 3)));
}

#[test]
fn resume_and_new_commands() {
    let (_, _, src) = setup("qwen-cmd");
    let scan = src.scan();
    let shop = by_id(&scan.sessions, SHOP);
    let r = src.resume_cmd(shop);
    assert_eq!(r.shell_line(), format!("qwen -r {SHOP}"));
    assert_eq!(r.cwd, Path::new("/Users/demo/projects/shop"));
    let m = src.resume_with_message_cmd(shop, "-继续");
    assert_eq!(
        m.args,
        ["-r", SHOP, "--prompt-interactive=-继续"],
        "以 - 开头的话也不会被当成选项"
    );
    let n = src.new_cmd(Path::new("/work/my app"));
    assert_eq!(n.shell_line(), "qwen");
    assert_eq!(n.cwd, Path::new("/work/my app"));
    assert!(
        Vendor::Qwen.info().presets_new_id,
        "新建由 host 加 --session-id"
    );
}

#[test]
fn search_hits_title_and_body() {
    let (base, _, src) = setup("qwen-search");
    let index = Index::open(&base.join("index"), vec![Box::new(src)]).unwrap();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    let ids = |q: &str| -> Vec<(String, bool)> {
        index
            .search(Vendor::Qwen, q, 50)
            .unwrap()
            .iter()
            .map(|h| (h.entry.session.id.as_str().to_owned(), h.in_title))
            .collect()
    };
    assert_eq!(ids("结算按钮"), [(SHOP.to_owned(), true)]);
    assert_eq!(ids("优惠券"), [(SHOP.to_owned(), false)]);
    // 回退掉的、注入的、思考块里的、托管引擎的、子代理的都搜不到。
    assert!(ids("包邮").is_empty());
    assert!(ids("商城").is_empty());
    assert!(ids("事件处理").is_empty());
    assert!(ids("托管里的话").is_empty());
    assert!(ids("子代理的话").is_empty());
}

#[test]
fn appending_refreshes_the_conversation() {
    let (base, root, src) = setup("qwen-refresh");
    let index = Index::open(&base.join("index"), vec![Box::new(src)]).unwrap();
    index.refresh();
    let path = shop_chats(&root).join(format!("{SHOP}.jsonl"));
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(
        f,
        r#"{{"uuid":"a12","parentUuid":"a10","sessionId":"{SHOP}","timestamp":"2026-11-20T10:02:00.000Z","type":"user","cwd":"/Users/demo/projects/shop","message":{{"role":"user","parts":[{{"text":"再跑一遍测试"}}]}},"systemPayload":{{"displayText":"再跑一遍测试"}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"uuid":"a13","parentUuid":"a12","sessionId":"{SHOP}","timestamp":"2026-11-20T10:02:05.000Z","type":"system","subtype":"custom_title","cwd":"/Users/demo/projects/shop","systemPayload":{{"customTitle":"结算和移动端","titleSource":"manual"}}}}"#
    )
    .unwrap();
    drop(f);
    index.refresh_paths(Vendor::Qwen, std::slice::from_ref(&path));
    let s = index
        .list(Vendor::Qwen)
        .unwrap()
        .into_iter()
        .find(|e| e.session.id.as_str() == SHOP)
        .unwrap()
        .session;
    assert_eq!(s.message_count, 3);
    assert_eq!(s.title.as_deref(), Some("结算和移动端"));
    assert_eq!(s.last_active, at(T + 120), "改名不算动静");
}

#[test]
fn trash_moves_side_files_and_subagents() {
    let (_, root, src) = setup("qwen-trash");
    let scan = src.scan();
    let chats = shop_chats(&root);
    let entry = src.trash(by_id(&scan.sessions, SHOP)).unwrap();
    for suffix in ["jsonl", "runtime.json", "title.json", "ledger.jsonl"] {
        assert!(!chats.join(format!("{SHOP}.{suffix}")).exists(), "{suffix}");
    }
    assert!(
        chats
            .join("5e1c0a2b-2222-4aaa-8bbb-000000000002.jsonl")
            .is_file(),
        "别的对话不动"
    );
    assert!(src.scan().sessions.iter().all(|s| s.id.as_str() != SHOP));
    src.restore(&entry).unwrap();
    assert!(chats.join(format!("{SHOP}.runtime.json")).is_file());
    assert!(src.scan().sessions.iter().any(|s| s.id.as_str() == SHOP));

    let blog_dir = root.join("-Users-demo-projects-blog");
    let entry = src.trash(by_id(&scan.sessions, BLOG)).unwrap();
    assert!(!blog_dir.join("subagents").join(BLOG).exists());
    src.restore(&entry).unwrap();
    assert!(
        blog_dir
            .join("subagents")
            .join(BLOG)
            .join("agent-explore.jsonl")
            .is_file()
    );
}

#[test]
fn missing_root_and_broken_lines() {
    let base = temp_dir("qwen-missing");
    let src = QwenSource::new(base.join("nope"), Trash::new(base.join("trash")));
    let scan = src.scan();
    assert!(scan.sessions.is_empty() && scan.problems.is_empty());

    let (_, root, src) = setup("qwen-broken");
    let path = shop_chats(&root).join(format!("{CHANNEL}.jsonl"));
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    // 还没写完的半行、坏行。
    write!(f, "{{ not json\n{{\"uuid\":\"c09\",\"type\":\"us").unwrap();
    drop(f);
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    let c = by_id(&scan.sessions, CHANNEL);
    assert_eq!(c.message_count, 1);
    assert_eq!(src.load_preview(c, 5).unwrap().len(), 2);
    assert_eq!(c.last_active, at(T + 110));
}

#[test]
fn only_session_files_affect_the_list() {
    let root = PathBuf::from("/h/.qwen/projects");
    let src = QwenSource::new(&root, Trash::new("/tmp/t"));
    assert_eq!(src.watch_roots(), std::slice::from_ref(&root));
    let p = root.join("-work-app");
    assert!(src.affects(&root));
    assert!(src.affects(&p));
    assert!(src.affects(&p.join("chats")));
    assert!(src.affects(&p.join(format!("chats/{SHOP}.jsonl"))));
    assert!(!src.affects(&p.join(format!("chats/{SHOP}.runtime.json"))));
    assert!(!src.affects(&p.join(format!("chats/{SHOP}.ledger.jsonl"))));
    assert!(!src.affects(&p.join(format!("chats/archive/{SHOP}.jsonl"))));
    assert!(!src.affects(&p.join(format!("subagents/{SHOP}/agent-x.jsonl"))));
    assert!(!src.affects(Path::new("/h/.qwen/settings.json")));
}
