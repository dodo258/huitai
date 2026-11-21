//! 增量扫描：只重读 mtime 或大小变了的会话文件；新文件加进来、没了的去掉；缓存重开就能出列表。
//! 不按文件组织的来源（Codex）整家重扫，按 id 对齐，暂时扫不到时留着缓存。

mod index_common;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use huitai::core::{Session, SessionId, Vendor};
use huitai::index::Index;
use huitai::sources::{Command, PreviewMessage, Scan, Source, TrashEntry};
use index_common::{Env, assistant_line, ids, temp_dir, title_line};

#[test]
fn only_changed_files_are_reread() {
    let env = Env::new("incr");
    env.write_session("a", "修购物车", "好的");
    env.write_session("b", "加优惠券", "好的");
    env.write_session("c", "改首页", "好的");
    let index = env.open();

    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged, r.removed), (3, 0, 0), "{r:?}");
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert_eq!(ids(&index.list(Vendor::Claude).unwrap()), ["a", "b", "c"]);
    env.take_reads();

    // 什么都没变：一个文件都不读。
    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged, r.removed), (0, 3, 0));
    assert!(!r.changed());
    assert!(env.take_reads().is_empty());

    // 只改 a：只重读 a，标题跟着变。
    env.append("a", &title_line("购物车结算修好了"));
    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged, r.removed), (1, 2, 0));
    assert_eq!(env.take_reads(), [env.session_path("a")]);
    let a = find(&index, "a");
    assert_eq!(a.session.title.as_deref(), Some("购物车结算修好了"));
}

#[test]
fn new_and_deleted_files_are_picked_up() {
    let env = Env::new("adddel");
    env.write_session("a", "一", "好");
    env.write_session("b", "二", "好");
    let index = env.open();
    index.refresh();
    env.take_reads();

    fs::remove_file(env.session_path("b")).unwrap();
    env.write_session("c", "三", "好");
    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged, r.removed), (1, 1, 1), "{r:?}");
    assert_eq!(env.take_reads(), [env.session_path("c")]);
    assert_eq!(ids(&index.list(Vendor::Claude).unwrap()), ["a", "c"]);

    // 空文件不是对话：读过一次记下 mtime，不列也不再读。
    fs::write(env.session_path("empty"), "").unwrap();
    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!(r.read, 1);
    assert_eq!(ids(&index.list(Vendor::Claude).unwrap()), ["a", "c"]);
    env.take_reads();
    index.refresh_vendor(Vendor::Claude);
    assert!(env.take_reads().is_empty());

    // 一个对话被清空：从列表去掉。
    fs::write(env.session_path("a"), "").unwrap();
    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!(r.removed, 1);
    assert_eq!(ids(&index.list(Vendor::Claude).unwrap()), ["c"]);
}

#[test]
fn same_size_rewrite_is_detected_by_mtime() {
    let env = Env::new("mtime");
    env.write_session("a", "甲甲", "好");
    let index = env.open();
    index.refresh();
    env.take_reads();

    // 内容换了但字节数一样，只有 mtime 变。
    let path = env.session_path("a");
    let old = fs::read_to_string(&path).unwrap();
    fs::write(&path, old.replace("甲甲", "乙乙")).unwrap();
    let later = SystemTime::now() + Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(later)
        .unwrap();
    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!(r.read, 1);
    assert_eq!(find(&index, "a").session.title.as_deref(), Some("乙乙"));
}

#[test]
fn cache_lists_without_scanning_after_reopen() {
    let env = Env::new("reopen");
    env.write_session("a", "一", "好");
    env.write_session("b", "二", "好");
    {
        let index = env.open();
        index.refresh();
    }
    env.take_reads();

    let index = env.open();
    // 不扫描直接出列表。
    assert_eq!(ids(&index.list(Vendor::Claude).unwrap()), ["a", "b"]);
    assert!(env.take_reads().is_empty());
    // 重开后增量扫描也不重读没变的文件。
    let r = index.refresh_vendor(Vendor::Claude);
    assert_eq!((r.read, r.unchanged), (0, 2));
    assert!(index.list(Vendor::Codex).unwrap().is_empty());
}

#[test]
fn broken_cache_is_rebuilt() {
    let env = Env::new("broken");
    env.write_session("a", "一", "好");
    fs::create_dir_all(env.data()).unwrap();
    fs::write(env.data().join("index.sqlite"), "这不是数据库").unwrap();
    let index = env.open();
    assert!(index.list(Vendor::Claude).unwrap().is_empty());
    index.refresh();
    assert_eq!(ids(&index.list(Vendor::Claude).unwrap()), ["a"]);
}

#[test]
fn list_is_newest_first() {
    let env = Env::new("order");
    env.write_session("old", "旧的", "好");
    env.write_session("new", "新的", "好");
    env.append(
        "new",
        &assistant_line("后来又说了一句", "2026-11-02T09:00:00.000Z"),
    );
    let index = env.open();
    index.refresh();
    let list = index.list(Vendor::Claude).unwrap();
    assert_eq!(list[0].session.id.as_str(), "new");
    assert_eq!(list[1].session.id.as_str(), "old");
}

fn find(index: &Index, id: &str) -> huitai::index::Entry {
    index
        .list(Vendor::Claude)
        .unwrap()
        .into_iter()
        .find(|e| e.session.id.as_str() == id)
        .unwrap_or_else(|| panic!("列表里没有 {id}"))
}

// ---- 整家重扫的来源 ----

/// 不按文件组织的假来源：扫描结果由测试直接给。
struct Whole {
    sessions: Arc<Mutex<Result<Vec<Session>, String>>>,
    version: u32,
}

fn session(id: &str, secs: u64, count: usize) -> Session {
    Session {
        vendor: Vendor::Codex,
        id: SessionId::from(id),
        cwd: PathBuf::from("/Users/demo/projects/shop"),
        path: PathBuf::from(format!("/nonexistent/{id}.jsonl")),
        title: Some(format!("对话 {id}")),
        git_branch: None,
        last_active: UNIX_EPOCH + Duration::from_secs(secs),
        message_count: count,
        automatic: false,
    }
}

impl Source for Whole {
    fn vendor(&self) -> Vendor {
        Vendor::Codex
    }
    fn parse_version(&self) -> u32 {
        self.version
    }
    fn scan(&self) -> Scan {
        match &*self.sessions.lock().unwrap() {
            Ok(s) => Scan {
                sessions: s.clone(),
                problems: Vec::new(),
            },
            Err(e) => Scan {
                sessions: Vec::new(),
                problems: vec![e.clone()],
            },
        }
    }
    fn load_preview(&self, _: &Session, _: usize) -> io::Result<Vec<PreviewMessage>> {
        Ok(Vec::new())
    }
    fn count_messages(&self, s: &Session) -> io::Result<usize> {
        Ok(s.message_count)
    }
    fn resume_cmd(&self, _: &Session) -> Command {
        unimplemented!("测试不用")
    }
    fn new_cmd(&self, _: &Path) -> Command {
        unimplemented!("测试不用")
    }
    fn trash(&self, _: &Session) -> io::Result<TrashEntry> {
        unimplemented!("测试不用")
    }
    fn restore(&self, _: &TrashEntry) -> io::Result<()> {
        unimplemented!("测试不用")
    }
    fn watch_roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

#[test]
fn whole_vendor_rescan_aligns_by_id() {
    let dir = temp_dir("whole");
    let state = Arc::new(Mutex::new(Ok(vec![
        session("x", 10, 1),
        session("y", 20, 1),
    ])));
    let index = Index::open(
        &dir,
        vec![Box::new(Whole {
            sessions: state.clone(),
            version: 1,
        })],
    )
    .unwrap();

    let r = index.refresh_vendor(Vendor::Codex);
    assert_eq!((r.read, r.unchanged, r.removed), (2, 0, 0));
    let r = index.refresh_vendor(Vendor::Codex);
    assert_eq!((r.read, r.unchanged, r.removed), (0, 2, 0));

    // y 有新消息、x 没了、z 新来。
    *state.lock().unwrap() = Ok(vec![session("y", 30, 2), session("z", 40, 1)]);
    let r = index.refresh_vendor(Vendor::Codex);
    assert_eq!((r.read, r.unchanged, r.removed), (2, 0, 1), "{r:?}");
    let list = index.list(Vendor::Codex).unwrap();
    assert_eq!(ids(&list), ["y", "z"]);
    assert_eq!(list[1].session.message_count, 2, "y 排第二且消息数更新");

    // 暂时扫不到（库被锁）：报问题，缓存留着。
    *state.lock().unwrap() = Err("state_5.sqlite: database is locked".into());
    let r = index.refresh_vendor(Vendor::Codex);
    assert_eq!(r.problems.len(), 1);
    assert_eq!(ids(&index.list(Vendor::Codex).unwrap()), ["y", "z"]);
}

/// 整家重扫的来源：版本加一时内容没变也全部重写；同时开着的旧版不覆盖、不删新版写的对话。
#[test]
fn whole_vendor_rescan_respects_parse_version() {
    let dir = temp_dir("whole-version");
    let open = |state: &Arc<Mutex<Result<Vec<Session>, String>>>, version| {
        Index::open(
            &dir,
            vec![Box::new(Whole {
                sessions: state.clone(),
                version,
            })],
        )
        .unwrap()
    };
    let state = Arc::new(Mutex::new(Ok(vec![
        session("x", 10, 1),
        session("y", 20, 1),
    ])));
    let v1 = open(&state, 1);
    assert_eq!(v1.refresh_vendor(Vendor::Codex).read, 2);
    drop(v1);

    let v2 = open(&state, 2);
    let r = v2.refresh_vendor(Vendor::Codex);
    assert_eq!(
        (r.read, r.unchanged, r.removed),
        (2, 0, 0),
        "版本变了整家重写"
    );
    assert_eq!(v2.refresh_vendor(Vendor::Codex).read, 0);

    // 旧版看到的不一样（少了 y、x 的消息数旧）：不覆盖 x、不删 y，只加它自己新看到的 z。
    let old_view = Arc::new(Mutex::new(Ok(vec![
        session("x", 15, 9),
        session("z", 30, 1),
    ])));
    let v1 = open(&old_view, 1);
    let r = v1.refresh_vendor(Vendor::Codex);
    assert_eq!((r.read, r.unchanged, r.removed), (1, 1, 0), "{r:?}");
    let list = v2.list(Vendor::Codex).unwrap();
    assert_eq!(ids(&list), ["x", "y", "z"]);
    let x = list.iter().find(|e| e.session.id.as_str() == "x").unwrap();
    assert_eq!(x.session.message_count, 1, "x 还是新版读的");

    // 新版再扫：旧版写的 z 不在新版的扫描结果里，删掉。
    let r = v2.refresh_vendor(Vendor::Codex);
    assert_eq!((r.read, r.unchanged, r.removed), (0, 2, 1), "{r:?}");
}

#[test]
fn refresh_paths_touches_only_the_given_files() {
    let env = Env::new("paths");
    env.write_session("a", "一", "好");
    env.write_session("b", "二", "好");
    let index = env.open();

    // 还没整家扫过：不知道这家按文件组织，退回整家扫。
    let r = index.refresh_paths(Vendor::Claude, &[env.session_path("a")]);
    assert_eq!(r.read, 2);
    env.take_reads();

    env.append("a", &title_line("甲"));
    env.append("b", &title_line("乙"));
    let r = index.refresh_paths(Vendor::Claude, &[env.session_path("a")]);
    assert_eq!((r.read, r.unchanged, r.removed), (1, 0, 0), "{r:?}");
    assert_eq!(env.take_reads(), [env.session_path("a")], "b 变了也不读");

    // 报来的文件没了：去掉。
    fs::remove_file(env.session_path("a")).unwrap();
    let r = index.refresh_paths(Vendor::Claude, &[env.session_path("a")]);
    assert_eq!(r.removed, 1);
    assert!(env.take_reads().is_empty());

    // 报来的是目录（项目目录建删、改名）：整家重列，b 的改动也读到。
    let r = index.refresh_paths(
        Vendor::Claude,
        &[env.projects().join(index_common::PROJECT)],
    );
    assert_eq!((r.read, r.removed), (1, 0), "{r:?}");
    assert_eq!(env.take_reads(), [env.session_path("b")]);
    assert_eq!(ids(&index.list(Vendor::Claude).unwrap()), ["b"]);
}
