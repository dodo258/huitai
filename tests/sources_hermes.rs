//! Hermes 适配器：state.db（测试时用 tests/fixtures/hermes/state.sql 建，WAL 模式，和真库一样）。
//! 样例覆盖本机各入口、消息平台、子代理、一次性跑、归档、压缩续写、空对话、回退的消息、cwd 为空、多模态正文。
//! 较新版本的库（tests/fixtures/hermes/display.sql）另测：带图片的消息、压缩后的历史、坏时间、改标题和原地改正文。

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

const IMAGE: &str = "20261122_090000_aa0001";
const COMPACTED: &str = "20261122_100000_bb0002";
const BAD_TIME: &str = "20261122_110000_cc0003";
const EDITED: &str = "20261122_120000_dd0004";

fn setup(name: &str) -> (PathBuf, HermesSource) {
    setup_from(name, "hermes/state.sql")
}

fn setup_from(name: &str, sql: &str) -> (PathBuf, HermesSource) {
    let base = temp_dir(name);
    let root = base.join("hermes");
    fs::create_dir_all(&root).unwrap();
    build_db(
        &root.join("state.db"),
        &fs::read_to_string(fixture(sql)).unwrap(),
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

#[test]
fn moment_from_last_turn() {
    let (_, src) = setup("hermes-moment");
    let scan = src.scan();
    let m = src.load_moment(by_id(&scan.sessions, TUI)).unwrap();
    assert_eq!(m.tool, None, "工具调用都有返回");
    assert_eq!(m.summary.as_deref(), Some("旧路径都加了重定向。"));
    assert_eq!(m.turn_started, Some(at(1_795_000_060)));
}

#[test]
fn moment_of_a_non_default_account_goes_through_its_native_id() {
    use huitai::core::Account;
    use huitai::sources::AccountSource;
    let (base, inner) = setup("hermes-moment-account");
    let profile = Account::new(
        Vendor::Hermes,
        "work",
        base.join("hermes"),
        Path::new("/nowhere"),
    );
    assert!(!profile.key.is_empty(), "要测的是带前缀的账号");
    let src = AccountSource::new(Box::new(inner), &profile);
    let scan = src.scan();
    let tui = scan
        .sessions
        .iter()
        .find(|s| s.id.as_str().ends_with(TUI))
        .unwrap();
    assert_ne!(tui.id.as_str(), TUI, "id 带着账号前缀");
    let m = src.load_moment(tui).unwrap();
    assert_eq!(m.summary.as_deref(), Some("旧路径都加了重定向。"));
}

fn texts(msgs: &[huitai::sources::PreviewMessage]) -> Vec<(Role, &str)> {
    msgs.iter().map(|m| (m.role, m.text.as_str())).collect()
}

/// 审计第 09 条：带图片的消息存成 NUL + `json:` 前缀加 JSON，直接取列只剩空串。要取出随图的文字当标题、预览、正文。
#[test]
fn image_messages_keep_their_text() {
    let (base, src) = setup_from("hermes-image", "hermes/display.sql");
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    let s = by_id(&scan.sessions, IMAGE);
    // 第一句只有图片，标题取下一句有字的。
    assert_eq!(s.title.as_deref(), Some("截图里的提交按钮为什么是灰的"));
    assert_eq!(s.message_count, 3, "只有图片的那句也是一句话");
    assert_eq!(
        texts(&src.load_preview(s, 10).unwrap()),
        [
            (Role::User, "截图里的提交按钮为什么是灰的"),
            (Role::Assistant, "表单校验没过，按钮被禁用了。"),
            (Role::User, "照着改完再截一张"),
        ]
    );
    let t = src.load_transcript(s).unwrap();
    assert_eq!(t.parts.len(), 3, "{:?}", t.parts);
    assert!(
        t.parts.iter().all(|p| !format!("{p:?}").contains("base64")),
        "图片数据不进整段对话"
    );

    let index = Index::open(&base.join("index"), vec![Box::new(src)]).unwrap();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    let hits = index.search(Vendor::Hermes, "禁用", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].entry.session.id.as_str(), IMAGE);
}

/// 审计第 10 条：压缩后原话归档成 `active = 0, compacted = 1`，Hermes 自己照样显示；压缩摘要只给模型看；
/// 留下的几句复制成新行、工具返回复制了一份，都只显示一次；`/rewind` 回退的不显示。
#[test]
fn compacted_history_stays_visible_once() {
    let (base, src) = setup_from("hermes-compacted", "hermes/display.sql");
    let scan = src.scan();
    let s = by_id(&scan.sessions, COMPACTED);
    assert_eq!(s.message_count, 3);
    assert_eq!(src.count_messages(s).unwrap(), 3);

    let t = src.load_transcript(s).unwrap();
    let user = |text: &str| {
        Part::Message(huitai::sources::PreviewMessage {
            role: Role::User,
            text: text.into(),
            at: None,
        })
    };
    let shown: Vec<Part> = t
        .parts
        .into_iter()
        .map(|p| match p {
            Part::Message(mut m) => {
                m.at = None;
                Part::Message(m)
            }
            other => other,
        })
        .collect();
    let assistant = |text: &str| {
        Part::Message(huitai::sources::PreviewMessage {
            role: Role::Assistant,
            text: text.into(),
            at: None,
        })
    };
    assert_eq!(
        shown,
        [
            user("先把订单表迁到新库"),
            Part::ToolCall {
                name: "terminal".into(),
                input: "migrate orders".into(),
            },
            Part::ToolResult {
                text: "migrated 1200 rows".into(),
            },
            assistant("订单表迁好了，索引也建了。"),
            user("再核对一遍行数"),
            assistant("行数一致。"),
            user("接着迁用户表"),
            assistant("用户表也迁完了。"),
        ]
    );
    assert_eq!(
        texts(&src.load_preview(s, 3).unwrap()),
        [
            (Role::Assistant, "行数一致。"),
            (Role::User, "接着迁用户表"),
            (Role::Assistant, "用户表也迁完了。"),
        ]
    );

    let index = Index::open(&base.join("index"), vec![Box::new(src)]).unwrap();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    let ids = |q: &str| -> Vec<String> {
        index
            .search(Vendor::Hermes, q, 10)
            .unwrap()
            .iter()
            .map(|h| h.entry.session.id.as_str().to_owned())
            .collect()
    };
    assert_eq!(ids("订单表迁到新库"), [COMPACTED], "压缩前的原话搜得到");
    assert!(ids("压缩摘要").is_empty(), "只给模型看的搜不到");
    assert!(ids("被回退").is_empty());
}

/// 补审第 2 条：时间写成 `1e999`（存成无穷大）时 `sqlite3 -json` 输出裸 `Inf`，整家解析失败；`1e20` 也不能盖掉
/// 正常的最后时间。坏时间当没有，别的对话照常列。
#[test]
fn unrepresentable_times_are_dropped_before_max() {
    let (_, src) = setup_from("hermes-badtime", "hermes/display.sql");
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    assert_eq!(scan.sessions.len(), 4);
    let s = by_id(&scan.sessions, BAD_TIME);
    assert_eq!(s.last_active, at(1_795_287_100));
    let msgs = src.load_preview(s, 10).unwrap();
    assert_eq!(
        msgs.iter().map(|m| m.at).collect::<Vec<_>>(),
        [Some(at(1_795_287_100)), None, None]
    );
    assert_eq!(src.load_transcript(s).unwrap().parts.len(), 3);
}

/// 审计第 11 条：Hermes 里只改标题（不动最后活动时间和条数），列表和标题搜索要跟着变；原地改正文（脱敏）时
/// 上游触发器给 `transcript_revision` 加一，旧词要从搜索里消失。
#[test]
fn renamed_and_redacted_sessions_refresh() {
    let (base, src) = setup_from("hermes-edited", "hermes/display.sql");
    let db = base.join("hermes/state.db");
    let index = Index::open(&base.join("index"), vec![Box::new(src)]).unwrap();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    let title = |index: &Index| -> String {
        index
            .list(Vendor::Hermes)
            .unwrap()
            .into_iter()
            .find(|e| e.session.id.as_str() == EDITED)
            .unwrap()
            .title()
            .to_owned()
    };
    let hit = |index: &Index, q: &str| -> Vec<(String, bool)> {
        index
            .search(Vendor::Hermes, q, 10)
            .unwrap()
            .iter()
            .map(|h| (h.entry.session.id.as_str().to_owned(), h.in_title))
            .collect()
    };
    assert_eq!(title(&index), "部署口令");
    assert_eq!(hit(&index, "hunter2"), [(EDITED.to_owned(), false)]);

    build_db(
        &db,
        "update sessions set title = '测试环境配置' where id = '20261122_120000_dd0004';\n\
         update messages set content = '测试环境的口令是 [已隐去]' where id = 40;\n",
    );
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    assert_eq!(title(&index), "测试环境配置");
    assert_eq!(hit(&index, "测试环境配置"), [(EDITED.to_owned(), true)]);
    assert!(hit(&index, "部署口令").is_empty(), "旧标题搜不到");
    assert!(hit(&index, "hunter2").is_empty(), "脱敏前的词搜不到");
    assert_eq!(hit(&index, "已隐去"), [(EDITED.to_owned(), false)]);
}

/// 字里有坏 UTF-8（按字节截断的汉字、孤立代理项解出来的三个字节）：以前 `sqlite3 -json` 原样输出、整批结果解析不了，
/// 整家列表一条都没有。现在只坏那个字，别的对话照列。
#[test]
fn broken_utf8_only_breaks_that_character() {
    let (base, _) = setup("hermes-bad-utf8");
    let root = base.join("hermes");
    build_db(
        &root.join("state.db"),
        &format!(
            "UPDATE sessions SET title = cast(x'e695b4e79086e68ea5e58fa3e696' as text) WHERE id = '{TUI}';\n\
             UPDATE messages SET content = cast(x'e68ea5e58fa3eda0bde69687e6a1a3' as text) \
             WHERE session_id = '{TUI}' AND content = '接口文档按模块分目录';\n"
        ),
    );
    let src = HermesSource::new(&root, Trash::new(base.join("trash")));
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    assert_eq!(scan.sessions.len(), 4);
    let tui = by_id(&scan.sessions, TUI);
    assert_eq!(tui.title.as_deref(), Some("整理接口\u{FFFD}"));
    assert_eq!(
        texts(&src.load_preview(tui, 4).unwrap())[0],
        (Role::User, "接口\u{FFFD}文档")
    );
}
