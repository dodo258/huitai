//! 索引缓存损坏（截断、中间乱写、文件头乱写、WAL 乱写、版本不对、表没了）：打开不报错，
//! 后台第一步发现坏了就自动重建，会话台自己的标注（改名/置顶/归档，单独存在 annotations.json）一条不丢。
//! 边写边读：半截末行扫描结果稳定，补齐后增量补上；FSEvents 风暴不卡列表、不重复条目。

mod fuzz_support;
mod index_common;

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use fuzz_support::Rng;
use huitai::core::{SessionId, Vendor};
use huitai::index::{Edit, Index, Update};
use index_common::{Env, assistant_line, ids, user_line};

const SESSIONS: usize = 300;

/// 写一批会话、扫一遍、建全文索引，再给几条加标注。返回 Env。
fn populated(name: &str) -> Env {
    let env = Env::new(name);
    for i in 0..SESSIONS {
        env.write_session(
            &format!("s{i:03}"),
            &format!("第 {i} 个问题：{}", "补充说明".repeat(20)),
            &format!("回答 {i}：{}", "改动说明".repeat(40)),
        );
    }
    let index = env.open();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    annotate(&index);
    assert_eq!(index.list(Vendor::Claude).unwrap().len(), SESSIONS);
    drop(index);
    env
}

fn annotate(index: &Index) {
    let id = |s: &str| SessionId::from(s);
    index
        .annotate(
            Vendor::Claude,
            &id("s007"),
            Edit::Rename(Some("改过的名字".into())),
        )
        .unwrap();
    index
        .annotate(Vendor::Claude, &id("s100"), Edit::Pin(true))
        .unwrap();
    index
        .annotate(Vendor::Claude, &id("s200"), Edit::Archive(true))
        .unwrap();
}

/// 标注都还在、列表完整、搜索能用。
fn assert_recovered(index: &Index) {
    let list = index.list(Vendor::Claude).unwrap();
    assert_eq!(list.len(), SESSIONS, "重建后列表不全");
    assert_eq!(ids(&list).len(), SESSIONS);
    assert_eq!(list[0].session.id.as_str(), "s100", "置顶的标注丢了");
    assert!(list[0].note.pinned);
    let renamed = list
        .iter()
        .find(|e| e.session.id.as_str() == "s007")
        .unwrap();
    assert_eq!(renamed.title(), "改过的名字");
    let archived = list
        .iter()
        .find(|e| e.session.id.as_str() == "s200")
        .unwrap();
    assert!(archived.note.archived);
    index.index_bodies(&mut || true).unwrap();
    let hits = index.search(Vendor::Claude, "回答 123", 10).unwrap();
    assert!(hits.iter().any(|h| h.entry.session.id.as_str() == "s123"));
}

/// 坏掉之后：打开不能失败；跑一遍后台（检查 + 增量扫描）后恢复如初。
fn reopen_and_recover(env: &Env) {
    // 打开失败会在 env.open() 里 panic：缓存坏了会话台就起不来。
    let index = env.open();
    // 列表可以暂时读不到或是空的，但不能 panic。
    let _ = index.list(Vendor::Claude);
    let (bg, rx) = index.start();
    wait_listed(&rx, Duration::from_secs(20));
    drop(bg);
    assert_recovered(&index);
}

/// 等后台扫完：收到 Listed 就返回；缓存本来就好的（没有变化、不发 Listed）等到安静 2 秒为止。
fn wait_listed(rx: &Receiver<Update>, max: Duration) {
    let deadline = Instant::now() + max;
    let mut quiet_since = Instant::now();
    while Instant::now() < deadline && quiet_since.elapsed() < Duration::from_secs(2) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Update::Listed(_)) => return,
            Ok(u) => {
                eprintln!("后台：{u:?}");
                quiet_since = Instant::now();
            }
            Err(_) => {}
        }
    }
}

fn db(env: &Env) -> std::path::PathBuf {
    env.data().join("index.sqlite")
}

/// 把 WAL 里的内容并回主库，之后改主库文件才有意义。
fn checkpoint(path: &Path) {
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch("pragma wal_checkpoint(truncate);").unwrap();
}

#[test]
fn truncated_cache_is_rebuilt() {
    let env = populated("trunc");
    let path = db(&env);
    checkpoint(&path);
    for keep in [0.5, 0.1] {
        let len = fs::metadata(&path).unwrap().len();
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len((len as f64 * keep) as u64 / 1000 * 1000 + 37)
            .unwrap();
        drop(f);
        reopen_and_recover(&env);
        checkpoint(&path);
    }
}

#[test]
fn scribbled_pages_are_rebuilt() {
    let env = populated("scribble");
    let path = db(&env);
    checkpoint(&path);
    let len = fs::metadata(&path).unwrap().len();
    let mut rng = Rng::new(0x5C1B);
    let mut f = OpenOptions::new().write(true).open(&path).unwrap();
    // 文件头之后的若干页里乱写。
    for _ in 0..8 {
        let at = 4096 + rng.below((len - 8192) as usize) as u64;
        f.seek(SeekFrom::Start(at)).unwrap();
        f.write_all(&rng.bytes(3000)).unwrap();
    }
    drop(f);
    reopen_and_recover(&env);
}

#[test]
fn scribbled_header_is_rebuilt() {
    let env = populated("header");
    let path = db(&env);
    checkpoint(&path);
    let mut f = OpenOptions::new().write(true).open(&path).unwrap();
    f.write_all(b"this is not a sqlite database at all......")
        .unwrap();
    drop(f);
    reopen_and_recover(&env);
}

#[test]
fn garbage_wal_is_ignored() {
    let env = populated("wal");
    let path = db(&env);
    checkpoint(&path);
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    fs::write(&wal, Rng::new(7).bytes(64 * 1024)).unwrap();
    reopen_and_recover(&env);
}

#[test]
fn wrong_version_and_missing_tables_are_rebuilt() {
    let env = populated("version");
    let path = db(&env);
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch("pragma user_version = 999;").unwrap();
    }
    reopen_and_recover(&env);
    {
        // 版本号对、表却没了（别的程序改过）。
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch("drop table files; drop table body;")
            .unwrap();
    }
    reopen_and_recover(&env);
}

/// 厂商正写着的文件：末尾半截行时列表稳定（不多算、不重复），补齐后增量扫描补上。
#[test]
fn half_written_file_is_stable_then_completed() {
    let env = Env::new("halfwrite");
    env.write_session("live", "第一句", "第一句的回答");
    let index = env.open();
    index.refresh();
    let path = env.session_path("live");
    let next = user_line("live", "第二句写到一半", "2026-11-01T09:10:00.000Z");
    let cut = next.len() / 2;
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(&next.as_bytes()[..cut]).unwrap();
    drop(f);
    for _ in 0..3 {
        index.refresh_paths(Vendor::Claude, std::slice::from_ref(&path));
        let list = index.list(Vendor::Claude).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session.message_count, 1);
        assert_eq!(list[0].title(), "第一句");
    }
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(&next.as_bytes()[cut..]).unwrap();
    f.write_all(b"\n").unwrap();
    drop(f);
    let report = index.refresh_paths(Vendor::Claude, std::slice::from_ref(&path));
    assert!(report.changed());
    let list = index.list(Vendor::Claude).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].session.message_count, 2);
}

/// 1 秒内 1000 次文件变动（新建、追加、改名、删除混着来）：后台照常跟上，列表读取一直很快，
/// 没有重复条目，最后和磁盘上一致。
#[test]
fn fsevents_storm_does_not_stall_or_duplicate() {
    let env = Env::new("storm");
    for i in 0..50 {
        env.write_session(&format!("base{i:02}"), "已有的问题", "已有的回答");
    }
    let index = env.open();
    index.refresh();
    let (bg, rx) = index.start();
    // 等启动那一轮扫完。
    std::thread::sleep(Duration::from_millis(500));
    while rx.try_recv().is_ok() {}

    let mut rng = Rng::new(0x5707);
    let mut alive: Vec<String> = (0..50).map(|i| format!("base{i:02}")).collect();
    let mut slowest = Duration::ZERO;
    let start = Instant::now();
    for op in 0..1000 {
        match rng.below(10) {
            0..=3 => {
                let id = format!("new{op:04}");
                env.write_session(&id, &format!("新问题 {op}"), "新回答");
                alive.push(id);
            }
            4..=6 if !alive.is_empty() => {
                let id = rng.pick(&alive).clone();
                env.append(
                    &id,
                    &assistant_line("追加的回答", "2026-11-01T10:00:00.000Z"),
                );
            }
            7 if !alive.is_empty() => {
                // 原子改写：写临时文件再改名替换（编辑器、部分厂商的写法）。
                let id = rng.pick(&alive).clone();
                let p = env.session_path(&id);
                let tmp = p.with_extension("jsonl.tmp");
                fs::copy(&p, &tmp).unwrap();
                fs::rename(&tmp, &p).unwrap();
            }
            8 if alive.len() > 10 => {
                let i = rng.below(alive.len());
                let id = alive.swap_remove(i);
                fs::remove_file(env.session_path(&id)).unwrap();
            }
            _ => {
                let p = env.session_path(rng.pick(&alive));
                let _ = fs::File::open(p);
            }
        }
        // 界面每隔几次变动读一次列表：读不能被后台写卡住。
        if op % 20 == 0 {
            let t = Instant::now();
            let list = index.list(Vendor::Claude).unwrap();
            slowest = slowest.max(t.elapsed());
            let unique: HashSet<&str> = list.iter().map(|e| e.session.id.as_str()).collect();
            assert_eq!(unique.len(), list.len(), "列表里有重复条目");
        }
    }
    let storm = start.elapsed();

    // 风暴过去后几秒内追上磁盘。
    let want: HashSet<String> = alive.iter().cloned().collect();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got: HashSet<String> = HashSet::new();
    while Instant::now() < deadline {
        let list = index.list(Vendor::Claude).unwrap();
        got = list
            .iter()
            .map(|e| e.session.id.as_str().to_owned())
            .collect();
        assert_eq!(got.len(), list.len(), "列表里有重复条目");
        if got == want {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(bg);
    eprintln!(
        "风暴：1000 次变动用了 {storm:?}，期间读列表最慢 {slowest:?}，最后 {} 条",
        got.len()
    );
    assert_eq!(got, want, "风暴过后列表和磁盘不一致");
    assert!(
        slowest < Duration::from_millis(200),
        "风暴期间读列表最慢 {slowest:?}，界面会卡"
    );
}
