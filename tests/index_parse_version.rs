//! 解析版本：适配器改了解析规则（版本加一）后，没变的旧文件也整家重读、正文重建，会话台的标注不受影响；
//! 还没记解析版本的旧缓存补列后同样重读；同时开着旧版 `ai` 时，旧版不重读、不覆盖新版读的结果。

mod index_common;

use std::fs;
use std::path::{Path, PathBuf};

use huitai::core::{Session, SessionId, Vendor};
use huitai::index::{Edit, Index};
use index_common::{Env, assistant_line, ids, title_line, user_line};
use serde_json::json;

fn find(index: &Index, id: &str) -> huitai::index::Entry {
    index
        .list(Vendor::Claude)
        .unwrap()
        .into_iter()
        .find(|e| e.session.id.as_str() == id)
        .unwrap_or_else(|| panic!("列表里没有 {id}"))
}

fn titled_v1(s: &mut Session) {
    s.title = Some("v1 读的".into());
}

fn titled_v2(s: &mut Session) {
    s.title = Some("v2 读的".into());
}

fn titled_v3(s: &mut Session) {
    s.title = Some("v3 读的".into());
}

fn db(env: &Env) -> PathBuf {
    env.data().join("index.sqlite")
}

#[test]
fn version_bump_rereads_every_file_and_keeps_notes() {
    let env = Env::new("pv-bump");
    for id in ["a", "b", "c"] {
        env.write_session(id, &format!("问题 {id}"), "好的");
    }
    let index = env.open_with(Some(1), titled_v1);
    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged, r.removed), (3, 0, 0), "{r:?}");
    assert_eq!(index.index_bodies(&mut || true).unwrap(), 3);
    index
        .annotate(
            Vendor::Claude,
            &SessionId::from("a"),
            Edit::Rename(Some("改过的名字".into())),
        )
        .unwrap();
    index
        .annotate(Vendor::Claude, &SessionId::from("b"), Edit::Pin(true))
        .unwrap();
    drop(index);

    // 同一个版本重开：一个都不重读。
    let index = env.open_with(Some(1), titled_v1);
    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged), (0, 3), "{r:?}");
    drop(index);
    env.take_reads();

    // 版本加一、文件一个没改：c 删了，其余整家重读。
    fs::remove_file(env.session_path("c")).unwrap();
    let index = env.open_with(Some(2), titled_v2);
    assert_eq!(
        ids(&index.list(Vendor::Claude).unwrap()),
        ["a", "b", "c"],
        "重读完之前旧结果照常显示"
    );
    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged, r.removed), (2, 0, 1), "{r:?}");
    let mut reads = env.take_reads();
    reads.sort();
    assert_eq!(reads, [env.session_path("a"), env.session_path("b")]);
    assert_eq!(find(&index, "b").session.title.as_deref(), Some("v2 读的"));
    // 正文跟着重建。
    assert_eq!(index.index_bodies(&mut || true).unwrap(), 2);

    // 标注都在。
    let a = find(&index, "a");
    assert_eq!(a.session.title.as_deref(), Some("v2 读的"));
    assert_eq!(a.title(), "改过的名字");
    assert!(find(&index, "b").note.pinned);
    assert_eq!(
        index.list(Vendor::Claude).unwrap()[0].session.id.as_str(),
        "b"
    );

    // 重读过的不再重读。
    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged), (0, 2), "{r:?}");
    assert_eq!(index.index_bodies(&mut || true).unwrap(), 0);
}

/// 「对话归错项目」那种：缓存是按旧规则（取第一个 cwd）读的，库还是没记解析版本的旧表。
/// 升级后补列、不删库，Claude 的解析版本比缓存新，文件没改也重读，归到对的项目。
#[test]
fn old_cache_without_versions_is_reread_with_new_claude_rules() {
    let env = Env::new("pv-legacy");
    let shop = Path::new("/Users/demo/projects/shop");
    let first = json!({
        "type": "user",
        "message": {"role": "user", "content": "先在 a 目录开的"},
        "timestamp": "2026-11-01T08:00:00.000Z",
        "cwd": "/Users/demo/a",
        "sessionId": "moved"
    });
    fs::write(
        env.session_path("moved"),
        format!(
            "{first}\n{}\n{}\n",
            user_line("moved", "后来切到 shop", "2026-11-01T09:00:00.000Z"),
            assistant_line("好的", "2026-11-01T09:00:05.000Z")
        ),
    )
    .unwrap();
    env.write_session("plain", "一直在 shop", "好的");

    let index = env.open_with(Some(1), |s| s.cwd = PathBuf::from("/Users/demo/a"));
    index.refresh_vendor(Vendor::Claude);
    index
        .annotate(
            Vendor::Claude,
            &SessionId::from("moved"),
            Edit::Archive(true),
        )
        .unwrap();
    drop(index);
    {
        // 变回加解析版本之前的表。
        let c = rusqlite::Connection::open(db(&env)).unwrap();
        c.execute_batch(
            "alter table sessions drop column parser; alter table files drop column parser;",
        )
        .unwrap();
    }

    let index = env.open();
    let before = find(&index, "moved");
    assert_eq!(
        before.session.cwd,
        Path::new("/Users/demo/a"),
        "旧缓存没被删，打开就有列表"
    );
    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged), (2, 0), "{r:?}");
    let moved = find(&index, "moved");
    assert_eq!(moved.session.cwd, shop);
    assert!(moved.note.archived, "标注还在");
    assert_eq!(find(&index, "plain").session.cwd, shop);
    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged), (0, 2), "{r:?}");
}

/// 同一个数据目录同时开着新旧两版（两个 Index 各自的连接，同两个进程）。
#[test]
fn older_version_does_not_overwrite_newer_results() {
    let env = Env::new("pv-older");
    env.write_session("a", "一", "好");
    env.write_session("b", "二", "好");
    let newer = env.open_with(Some(3), titled_v3);
    let older = env.open_with(Some(2), titled_v2);

    assert_eq!(newer.refresh_vendor(Vendor::Claude).read, 2);
    env.take_reads();
    let r = older.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged, r.removed), (0, 2, 0), "{r:?}");
    assert!(env.take_reads().is_empty(), "旧版不重读新版读过的");

    // a 变了，旧版先扫到：不读、不覆盖；新版扫到才更新。
    env.append("a", &title_line("新标题"));
    let r = older.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged), (0, 2), "{r:?}");
    assert_eq!(find(&older, "a").session.title.as_deref(), Some("v3 读的"));
    assert_eq!(newer.refresh_vendor(Vendor::Claude).read, 1);

    // 新文件旧版先读到：先按旧版的结果显示，新版照样重读一遍，之后旧版不再动它。
    env.write_session("c", "三", "好");
    assert_eq!(older.refresh_vendor(Vendor::Claude).read, 1);
    assert_eq!(find(&newer, "c").session.title.as_deref(), Some("v2 读的"));
    let r = newer.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged), (1, 2), "{r:?}");
    assert_eq!(find(&older, "c").session.title.as_deref(), Some("v3 读的"));
    assert_eq!(older.refresh_vendor(Vendor::Claude).read, 0);

    // 文件没了是事实，旧版也删。
    fs::remove_file(env.session_path("b")).unwrap();
    assert_eq!(older.refresh_vendor(Vendor::Claude).removed, 1);
    assert_eq!(ids(&newer.list(Vendor::Claude).unwrap()), ["a", "c"]);
    for e in newer.list(Vendor::Claude).unwrap() {
        assert_eq!(e.session.title.as_deref(), Some("v3 读的"), "{e:?}");
    }
}
