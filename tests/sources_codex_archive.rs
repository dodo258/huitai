//! Codex 压缩归档的会话（`rollout-*.jsonl.zst`，Codex 把闲置的会话压成它、删掉 `.jsonl`）：
//! 扫描、预览、整段对话、「此刻」、消息数和解压前的 `.jsonl` 读出来一样；库里记的 `.jsonl` 没了、旁边有
//! `.zst` 时读归档；全文搜索和导出拿得到正文。样例是手写的 `tests/fixtures/codex/archived/*.jsonl`，测试里压成 `.zst`。

mod sources_common;

use std::fs;
use std::path::PathBuf;

use huitai::core::{Session, Vendor};
use huitai::export::{self, Meta};
use huitai::index::{Entry, Index};
use huitai::sources::{CodexSource, Part, Role, Source, Trash};
use sources_common::{at, build_db, fixture, temp_dir};

const ID: &str = "aaaaaaaa-0000-4000-8000-00000000000c";
const NAME: &str = "rollout-2026-11-01T09-00-00-aaaaaaaa-0000-4000-8000-00000000000c.jsonl";
const DAY: &str = "sessions/2026/11/01";

fn plain() -> Vec<u8> {
    fs::read(fixture(&format!("codex/archived/{NAME}"))).unwrap()
}

fn compress(bytes: &[u8]) -> Vec<u8> {
    zstd::encode_all(bytes, 3).unwrap()
}

/// 一个 Codex 根目录，`files` 是（相对根目录的路径，内容）。
fn root_with(name: &str, files: &[(String, Vec<u8>)]) -> (PathBuf, CodexSource) {
    let base = temp_dir(name);
    let root = base.join("codex");
    for (rel, bytes) in files {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
    }
    fs::create_dir_all(&root).unwrap();
    let src = CodexSource::new(&root, Trash::new(base.join("trash")));
    (root, src)
}

fn archived_root(name: &str) -> (PathBuf, CodexSource) {
    root_with(name, &[(format!("{DAY}/{NAME}.zst"), compress(&plain()))])
}

fn only(src: &CodexSource) -> Session {
    let scan = src.scan();
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    assert_eq!(scan.sessions.len(), 1, "{:?}", scan.sessions);
    scan.sessions.into_iter().next().unwrap()
}

#[test]
fn archive_reads_the_same_as_plain_rollout() {
    let (_, zsrc) = archived_root("codex-zst-same");
    let (_, psrc) = root_with("codex-zst-plain", &[(format!("{DAY}/{NAME}"), plain())]);
    let z = only(&zsrc);
    let p = only(&psrc);

    assert!(z.path.to_string_lossy().ends_with(".jsonl.zst"), "{z:?}");
    assert_eq!(z.id.as_str(), ID);
    assert_eq!(z.title.as_deref(), Some("把地图瓦片缓存起来"));
    assert_eq!(z.cwd, PathBuf::from("/Users/demo/projects/atlas"));
    assert_eq!(z.git_branch.as_deref(), Some("feat/archive"));
    assert_eq!(z.message_count, 2, "环境上下文那条 user 消息不算");
    assert_eq!(z.last_active, at(1_793_523_930));
    assert!(!z.automatic);
    assert_eq!(
        Session {
            path: p.path.clone(),
            ..z.clone()
        },
        p
    );

    let preview = zsrc.load_preview(&z, 5).unwrap();
    let got: Vec<(Role, &str)> = preview.iter().map(|m| (m.role, m.text.as_str())).collect();
    assert_eq!(
        got,
        [
            (Role::User, "把地图瓦片缓存起来"),
            (Role::Assistant, "瓦片缓存加好了，过期按经纬网格清理"),
            (Role::User, "再补一个清理的测试"),
            (Role::Assistant, "测试补好了"),
        ]
    );
    assert_eq!(preview, psrc.load_preview(&p, 5).unwrap());
    assert_eq!(zsrc.load_preview(&z, 1).unwrap()[0].text, "测试补好了");

    let tr = zsrc.load_transcript(&z).unwrap();
    assert!(!tr.truncated);
    assert!(
        tr.parts
            .iter()
            .any(|p| matches!(p, Part::ToolCall { name, .. } if name == "exec_command")),
        "{:?}",
        tr.parts
    );
    assert!(
        tr.parts
            .iter()
            .any(|p| matches!(p, Part::ToolResult { text } if text.contains("z1.png")))
    );
    assert_eq!(tr, psrc.load_transcript(&p).unwrap());

    assert_eq!(zsrc.count_messages(&z).unwrap(), 2);
    let c = zsrc.count_since(&z, None).unwrap();
    assert_eq!(c.users, 2);
    assert_eq!(c.progress, None, "归档不会再追加，不记接着数的位置");

    let m = zsrc.load_moment(&z).unwrap();
    assert_eq!(m.summary.as_deref(), Some("测试补好了"));
    assert_eq!(m, psrc.load_moment(&p).unwrap());
}

#[test]
fn db_thread_whose_rollout_was_archived_reads_the_archive() {
    let (root, src) = archived_root("codex-zst-db");
    let state = fs::read_to_string(fixture("codex/state.sql"))
        .unwrap()
        .replace("@ROOT@", &root.to_string_lossy());
    // 库里记的还是 `.jsonl`，正文库里也没有这条（本机八十多条就是这样）。
    let row = format!(
        "INSERT INTO threads VALUES ('{ID}', '{}', 1793523600, 1793523930, 'vscode', \
         '/Users/demo/projects/atlas', '库里的标题', 0, 'feat/archive', '把地图瓦片缓存起来', 1793523930000, NULL);",
        root.join(DAY).join(NAME).display()
    );
    build_db(&root.join("state_5.sqlite"), &format!("{state}\n{row}"));
    build_db(
        &root.join("thread_history_1.sqlite"),
        &fs::read_to_string(fixture("codex/history.sql")).unwrap(),
    );

    let scan = src.scan();
    let s = scan
        .sessions
        .iter()
        .find(|s| s.id.as_str() == ID)
        .expect("库里的对话照常列出")
        .clone();
    assert_eq!(
        scan.sessions.iter().filter(|s| s.id.as_str() == ID).count(),
        1,
        "归档不再单独列一条"
    );
    assert_eq!(s.path, root.join(DAY).join(format!("{NAME}.zst")));
    assert_eq!(s.title.as_deref(), Some("库里的标题"));
    assert_eq!(s.message_count, 2, "正文库里没有，从归档数");
    let texts: Vec<String> = src
        .load_preview(&s, 5)
        .unwrap()
        .into_iter()
        .map(|m| m.text)
        .collect();
    assert_eq!(texts.last().map(String::as_str), Some("测试补好了"));
    assert_eq!(src.count_messages(&s).unwrap(), 2);
    assert!(!src.load_transcript(&s).unwrap().parts.is_empty());
}

#[test]
fn plain_and_archive_side_by_side_list_once() {
    // Codex 正在压：`.zst` 写好、`.jsonl` 还没删。
    let (_, src) = root_with(
        "codex-zst-both",
        &[
            (format!("{DAY}/{NAME}"), plain()),
            (format!("{DAY}/{NAME}.zst"), compress(&plain())),
        ],
    );
    let s = only(&src);
    assert!(s.path.to_string_lossy().ends_with(".jsonl"), "{s:?}");
}

#[test]
fn overlong_line_in_archive_is_skipped() {
    let text = String::from_utf8(plain()).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let huge = format!(
        r#"{{"timestamp":"2026-11-01T09:00:05.000Z","type":"response_item","payload":{{"type":"function_call_output","call_id":"c9","output":"{}"}}}}"#,
        "x".repeat(17 * 1024 * 1024)
    );
    lines.insert(6, huge);
    let bytes = (lines.join("\n") + "\n").into_bytes();
    let (_, src) = root_with(
        "codex-zst-long",
        &[(format!("{DAY}/{NAME}.zst"), compress(&bytes))],
    );
    let s = only(&src);
    assert_eq!(s.message_count, 2);
    assert_eq!(src.load_preview(&s, 5).unwrap().len(), 4);
    let tr = src.load_transcript(&s).unwrap();
    assert!(
        tr.parts
            .iter()
            .all(|p| !matches!(p, Part::ToolResult { text } if text.starts_with("xxx")))
    );
}

#[test]
fn broken_archive_is_reported_not_fatal() {
    let (_, src) = root_with(
        "codex-zst-broken",
        &[(format!("{DAY}/{NAME}.zst"), b"not zstd at all".to_vec())],
    );
    let scan = src.scan();
    assert!(scan.sessions.is_empty());
    assert_eq!(scan.problems.len(), 1, "{:?}", scan.problems);
    assert!(
        scan.problems[0].contains(".jsonl.zst"),
        "{:?}",
        scan.problems
    );
}

#[test]
fn rewritten_archive_is_read_again() {
    let (root, src) = archived_root("codex-zst-rewrite");
    assert_eq!(only(&src).message_count, 2);
    let more = [
        plain(),
        r#"{"timestamp":"2026-11-01T10:00:00.000Z","type":"event_msg","payload":{"type":"user_message","message":"第三句"}}"#
            .as_bytes()
            .to_vec(),
        b"\n".to_vec(),
    ]
    .concat();
    fs::write(root.join(DAY).join(format!("{NAME}.zst")), compress(&more)).unwrap();
    let s = only(&src);
    assert_eq!(s.message_count, 3, "文件变了不用记下的旧结果");
    assert_eq!(s.last_active, at(1_793_527_200));
}

#[test]
fn search_and_export_reach_archived_text() {
    let (root, _) = archived_root("codex-zst-search");
    let src = || Box::new(CodexSource::new(&root, Trash::new(root.join("../trash"))));
    let index = Index::open(&root.join("../data"), vec![src()]).unwrap();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();

    let hits = index.search(Vendor::Codex, "经纬网格", 50).unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.entry.session.id.as_str()).collect();
    assert_eq!(ids, [ID]);
    assert!(hits[0].in_body);
    let entry: &Entry = &hits[0].entry;
    assert_eq!(entry.session.message_count, 2);
    let snippet = index
        .snippet(&entry.session, "经纬网格")
        .unwrap()
        .expect("命中片段从归档里取");
    assert!(snippet.text.contains("经纬网格"), "{}", snippet.text);

    let source = src();
    let tr = export::load(source.as_ref(), &entry.session).unwrap();
    let meta = Meta {
        title: entry.title(),
        account: None,
        project: "atlas",
        session: &entry.session,
    };
    let md = export::markdown(&meta, &tr, &|_| 8 * 3600);
    assert!(md.contains("## 你"), "{md}");
    assert!(md.contains("把地图瓦片缓存起来"), "{md}");
    assert!(md.contains("瓦片缓存加好了，过期按经纬网格清理"), "{md}");
    assert!(md.contains("测试补好了"), "{md}");
}
