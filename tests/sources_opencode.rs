//! OpenCode 适配器：opencode.db（测试时用 tests/fixtures/opencode/opencode.sql 建，WAL 模式，和真库一样）。
//! 样例里 1.x、2.x 两套表同库：只在一套里的、两套都有（哪边更新各一条）、子代理、归档、空对话、占位标题、目录为空。

mod sources_common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use huitai::core::{Session, Vendor};
use huitai::index::Index;
use huitai::sources::{OpenCodeSource, Part, PreviewMessage, Role, Source, Trash};
use sources_common::{at, fixture, temp_dir};

const V1: &str = "ses_e11a9a1ffffeV1onlyAbCdEfGh";
const V1_FALLBACK: &str = "ses_e119a5fbfffeV1fallbackAbCd";
const DUP_V2_NEWER: &str = "ses_e115d56bfffeDupV2newerAbCd";
const DUP_V1_NEWER: &str = "ses_e114e147fffeDupV1newerAbCd";
const V2: &str = "ses_e113ed23fffeV2onlyAbCdEfGh";

fn setup(name: &str) -> (PathBuf, OpenCodeSource) {
    let base = temp_dir(name);
    let root = base.join("opencode");
    fs::create_dir_all(&root).unwrap();
    let db = root.join("opencode.db");
    build_db(
        &db,
        &fs::read_to_string(fixture("opencode/opencode.sql")).unwrap(),
    );
    let src = OpenCodeSource::new(&db, Trash::new(base.join("trash")));
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

fn ms(t: u64) -> std::time::SystemTime {
    at(t / 1000) + std::time::Duration::from_millis(t % 1000)
}

fn texts(msgs: &[PreviewMessage]) -> Vec<(Role, &str)> {
    msgs.iter().map(|m| (m.role, m.text.as_str())).collect()
}

#[test]
fn one_malformed_json_row_does_not_empty_the_list() {
    // 真库里写坏一条（截断的 JSON）：只影响那一行，别的对话照常列出（以前整句 SQL 报 malformed JSON，整家空）。
    let (base, src) = setup("opencode-bad-json");
    build_db(
        &base.join("opencode/opencode.db"),
        "INSERT INTO message VALUES ('msg_bad', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000070000, 1795000070000, '{\"role\":\"us');\n\
         INSERT INTO part VALUES ('prt_bad', 'msg_a1', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000070000, 1795000070000, '{\"type\":');\n\
         INSERT INTO session_message VALUES ('msg_badv2', 'ses_e113ed23fffeV2onlyAbCdEfGh', 'user', 99, 1795009990000, 1795009990000, '{\"text\":');\n",
    );
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    let mut ids: Vec<&str> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
    ids.sort();
    let mut want = [V1, V1_FALLBACK, DUP_V2_NEWER, DUP_V1_NEWER, V2];
    want.sort();
    assert_eq!(ids, want);
    // 预览和整段也跳过坏行。
    for id in [V1, V2] {
        let s = by_id(&scan.sessions, id).clone();
        assert!(!src.load_preview(&s, 8).unwrap().is_empty(), "{id}");
        assert!(src.load_transcript(&s).is_ok(), "{id}");
        assert!(src.count_messages(&s).unwrap() > 0, "{id}");
    }
}

#[test]
fn lists_both_schemas_deduplicated_by_id() {
    let (_, src) = setup("opencode-scan");
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    let mut ids: Vec<&str> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
    ids.sort();
    // 子代理（两套里各一条）、归档、没说过话的都不列；两套都有的只列一次。
    let mut want = [V1, V1_FALLBACK, DUP_V2_NEWER, DUP_V1_NEWER, V2];
    want.sort();
    assert_eq!(ids, want);

    let v1 = by_id(&scan.sessions, V1);
    assert_eq!(v1.vendor, Vendor::OpenCode);
    assert_eq!(v1.cwd, Path::new("/work/api"));
    assert_eq!(v1.title.as_deref(), Some("整理接口文档"));
    assert_eq!(v1.message_count, 2);
    assert_eq!(v1.last_active, ms(1_795_000_400_000));
    assert!(!v1.automatic);
    // 库是各条对话共用的，不当成会话文件。
    assert_eq!(v1.path, PathBuf::new());

    let fb = by_id(&scan.sessions, V1_FALLBACK);
    assert_eq!(fb.cwd, Path::new("/work/blog"), "目录空时退用项目根目录");
    assert_eq!(
        fb.title.as_deref(),
        Some("把草稿里的图片 压一遍"),
        "占位标题换成第一句话"
    );

    // 2.x 那套更新：标题、时间、条数都取 2.x。
    let d2 = by_id(&scan.sessions, DUP_V2_NEWER);
    assert_eq!(d2.title.as_deref(), Some("修结算按钮（2.x）"));
    assert_eq!(d2.message_count, 2);
    assert_eq!(d2.last_active, ms(1_795_005_500_000));
    // 1.x 那套更新：取 1.x。
    let d1 = by_id(&scan.sessions, DUP_V1_NEWER);
    assert_eq!(d1.message_count, 2);
    assert_eq!(d1.last_active, ms(1_795_006_900_000));

    let v2 = by_id(&scan.sessions, V2);
    assert_eq!(
        v2.title.as_deref(),
        Some("给登录页加验证码"),
        "没标题用第一句话"
    );
    assert_eq!(v2.cwd, Path::new("/work/api"));
    assert_eq!(v2.message_count, 1);
}

#[test]
fn preview_and_transcript_v1() {
    let (_, src) = setup("opencode-preview-v1");
    let scan = src.scan();
    let v1 = by_id(&scan.sessions, V1);

    let msgs = src.load_preview(v1, 3).unwrap();
    // 只要字；系统塞的 synthetic 文字、思考、工具不算。
    assert_eq!(
        texts(&msgs),
        [
            (Role::Assistant, "分成订单、用户两块了。"),
            (Role::User, "旧链接要能跳过去"),
            (Role::Assistant, "旧路径都加了重定向。"),
        ]
    );
    assert_eq!(msgs[1].at, Some(ms(1_795_000_060_000)));
    assert_eq!(
        src.load_preview(v1, 10).unwrap()[0].text,
        "把接口文档按模块拆开"
    );

    let t = src.load_transcript(v1).unwrap();
    assert!(!t.truncated);
    assert_eq!(
        t.parts[..4],
        [
            Part::Message(PreviewMessage {
                role: Role::User,
                text: "把接口文档按模块拆开".into(),
                at: Some(ms(1_795_000_010_000)),
            }),
            Part::ToolCall {
                name: "read".into(),
                input: "docs/api.md".into(),
            },
            Part::ToolResult {
                text: "# API\n订单、用户".into(),
            },
            Part::Message(PreviewMessage {
                role: Role::Assistant,
                text: "分成订单、用户两块了。".into(),
                at: Some(ms(1_795_000_020_000)),
            }),
        ]
    );
    // 出错的工具调用把错误当返回。
    assert!(t.parts.contains(&Part::ToolCall {
        name: "bash".into(),
        input: "npm test".into(),
    }));
    assert!(t.parts.contains(&Part::ToolResult {
        text: "exit 1".into()
    }));
    assert_eq!(t.parts.len(), 8);
    assert_eq!(src.count_messages(v1).unwrap(), 2);
}

#[test]
fn preview_and_transcript_v2() {
    let (_, src) = setup("opencode-preview-v2");
    let scan = src.scan();
    let v2 = by_id(&scan.sessions, V2);

    let msgs = src.load_preview(v2, 5).unwrap();
    // 拆进 session_part 的块在前、还内联的接在后；切换代理、空闲这些不是消息。
    assert_eq!(
        texts(&msgs),
        [
            (Role::User, "给登录页加验证码"),
            (
                Role::Assistant,
                "改了 src/login.tsx。\n验证码加好了，用的滑块。"
            ),
        ]
    );
    let t = src.load_transcript(v2).unwrap();
    assert_eq!(
        t.parts[1..3],
        [
            Part::ToolCall {
                name: "edit".into(),
                input: "src/login.tsx".into(),
            },
            Part::ToolResult {
                text: "已改 1 处".into(),
            },
        ]
    );
    assert_eq!(t.parts.len(), 5);
    assert_eq!(src.count_messages(v2).unwrap(), 1);
}

/// 两套都有的对话，预览、正文、计数从更新的那套读。
#[test]
fn duplicated_conversation_reads_the_newer_schema() {
    let (_, src) = setup("opencode-dup");
    let scan = src.scan();
    let d2 = by_id(&scan.sessions, DUP_V2_NEWER);
    let last = |s: &Session| src.load_preview(s, 1).unwrap()[0].text.clone();
    assert_eq!(last(d2), "2.x 里接着说的一句");
    assert_eq!(src.count_messages(d2).unwrap(), 2);
    let d1 = by_id(&scan.sessions, DUP_V1_NEWER);
    assert_eq!(last(d1), "回 1.x 接着说的一句");
    assert_eq!(src.count_messages(d1).unwrap(), 2);
}

#[test]
fn resume_and_new_commands() {
    let (_, src) = setup("opencode-cmd");
    let scan = src.scan();
    let v1 = by_id(&scan.sessions, V1);
    let r = src.resume_cmd(v1);
    assert_eq!(r.shell_line(), format!("opencode /work/api -s {V1}"));
    assert_eq!(r.cwd, Path::new("/work/api"));
    let r = src.resume_with_message_cmd(v1, "继续上次没做完的事。");
    assert_eq!(
        r.shell_line(),
        format!("opencode /work/api -s {V1} '--prompt=继续上次没做完的事。'")
    );
    let n = src.new_cmd(Path::new("/work/my app"));
    assert_eq!(n.shell_line(), "opencode '/work/my app'");
    assert_eq!(n.cwd, Path::new("/work/my app"));
    assert!(!Vendor::OpenCode.info().presets_new_id);
}

/// 新建对话托管在临时 id 下，靠 id 里的时间认回来：样例 id 按上游算法生成，还原出的时间就是建它的那一刻。
#[test]
fn id_time_matches_creation() {
    let (_, src) = setup("opencode-idtime");
    let scan = src.scan();
    for (id, created) in [(V1, 1_795_000_000_000), (V2, 1_795_007_000_000)] {
        let s = by_id(&scan.sessions, id);
        assert_eq!(
            huitai::sources::opencode::id_created(id, s.last_active),
            Some(ms(created))
        );
    }
}

#[test]
fn search_hits_title_and_body() {
    let (base, src) = setup("opencode-search");
    let index = Index::open(&base.join("index"), vec![Box::new(src)]).unwrap();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    let ids = |q: &str| -> Vec<(String, bool)> {
        index
            .search(Vendor::OpenCode, q, 50)
            .unwrap()
            .iter()
            .map(|h| (h.entry.session.id.as_str().to_owned(), h.in_title))
            .collect()
    };
    assert_eq!(ids("接口文档"), [(V1.to_owned(), true)]);
    assert_eq!(ids("重定向"), [(V1.to_owned(), false)]);
    assert_eq!(ids("滑块"), [(V2.to_owned(), false)]);
    // 两套都有的只出一条。
    assert_eq!(ids("结算"), [(DUP_V2_NEWER.to_owned(), true)]);
    // 不列的对话搜不到。
    assert!(ids("子任务").is_empty());
    assert!(ids("旧事").is_empty());
}

#[test]
fn trash_only_hides() {
    let (_, src) = setup("opencode-trash");
    let scan = src.scan();
    let entry = src.trash(by_id(&scan.sessions, V2)).unwrap();
    assert!(src.scan().sessions.iter().all(|s| s.id.as_str() != V2));
    src.restore(&entry).unwrap();
    assert!(src.scan().sessions.iter().any(|s| s.id.as_str() == V2));
}

#[test]
fn missing_broken_or_v1_only_db() {
    let base = temp_dir("opencode-missing");
    let src = OpenCodeSource::new(
        base.join("nope/opencode.db"),
        Trash::new(base.join("trash")),
    );
    let scan = src.scan();
    assert!(scan.sessions.is_empty() && scan.problems.is_empty());

    fs::create_dir_all(base.join("broken")).unwrap();
    let broken = base.join("broken/opencode.db");
    fs::write(&broken, b"this is not a database").unwrap();
    let scan = OpenCodeSource::new(&broken, Trash::new(base.join("trash"))).scan();
    assert!(scan.sessions.is_empty());
    assert!(
        scan.problems[0].contains("opencode.db"),
        "{:?}",
        scan.problems
    );

    // 只装过 1.x（没有 2.x 的表）：照样列、照样读。
    let sql = fs::read_to_string(fixture("opencode/opencode.sql")).unwrap();
    let v1_only = sql.split("-- ---- 2.x ----").next().unwrap();
    fs::create_dir_all(base.join("v1")).unwrap();
    let db = base.join("v1/opencode.db");
    build_db(&db, v1_only);
    let src = OpenCodeSource::new(&db, Trash::new(base.join("trash")));
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    assert_eq!(scan.sessions.len(), 4);
    let d2 = by_id(&scan.sessions, DUP_V2_NEWER);
    assert_eq!(src.count_messages(d2).unwrap(), 1);
    assert_eq!(
        src.load_preview(d2, 1).unwrap()[0].text,
        "结算按钮点了没反应"
    );
}

#[test]
fn only_db_and_wal_changes_affect_the_list() {
    let dir = PathBuf::from("/h/.local/share/opencode");
    let src = OpenCodeSource::new(dir.join("opencode.db"), Trash::new("/tmp/t"));
    assert_eq!(src.watch_roots(), std::slice::from_ref(&dir));
    assert!(src.affects(&dir.join("opencode.db")));
    assert!(src.affects(&dir.join("opencode.db-wal")));
    assert!(!src.affects(&dir.join("opencode.db-shm")));
    assert!(!src.affects(&dir.join("log/2026-11-22.log")));
    assert!(!src.affects(&dir.join("opencode-local.db")));
    assert!(!src.affects(&dir.join("snapshot/x/opencode.db")));
}
