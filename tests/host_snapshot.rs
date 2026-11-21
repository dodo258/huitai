//! 实时预览用的画面快照：只读、和终端状态机一致、不影响正接着的标签页。

mod host_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use host_support::*;
use huitai::host::{self, CellColor, ScreenSnapshot, Snapshot, attr};

const RAW: &str = "stty raw -echo;";

fn screen_of(r: std::io::Result<Snapshot>) -> ScreenSnapshot {
    match r.expect("快照请求失败") {
        Snapshot::Screen(s) => s,
        Snapshot::Unchanged => panic!("应该拿到画面，收到 Unchanged"),
    }
}

fn texts(s: &ScreenSnapshot) -> Vec<String> {
    s.lines
        .iter()
        .map(|l| l.iter().map(|r| r.text.as_str()).collect())
        .collect()
}

#[test]
fn snapshot_matches_screen_while_output_streams_and_attached_tab_is_untouched() {
    let host = TestHost::start();
    // 假 CLI 分批持续输出 300 行彩色字，最后打一个记号。
    let id = host.spawn(
        "stream",
        &format!(
            "{RAW} i=1; while [ $i -le 300 ]; do printf '\\033[1;3%dm row %d\\033[0m\\r\\n' $((i % 8)) $i; \
             [ $((i % 30)) -eq 0 ] && sleep 0.05; i=$((i + 1)); done; printf DONE; sleep 60"
        ),
    );
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);

    // 输出期间另一个线程不停地取快照。
    let stop = Arc::new(AtomicBool::new(false));
    let taken = Arc::new(AtomicUsize::new(0));
    let hammer = {
        let (dir, id, stop, taken) = (
            host.dir.clone(),
            id.clone(),
            Arc::clone(&stop),
            Arc::clone(&taken),
        );
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let s = screen_of(host::snapshot(&dir, &id, None));
                assert_eq!((s.cols, s.rows, s.lines.len()), (COLS, ROWS, ROWS as usize));
                taken.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    read_until(&mut att, &mut seen, "DONE", |s| {
        s.live_text().ends_with("DONE")
    });
    stop.store(true, Ordering::Relaxed);
    hammer.join().expect("取快照的线程出错");
    assert!(
        taken.load(Ordering::Relaxed) >= 5,
        "输出期间只取到 {} 次快照",
        taken.load(Ordering::Relaxed)
    );

    // 标签页收到的字节一个不多一个不少（快照不往客户端写任何东西）。
    let mut want = String::new();
    for i in 1..=300 {
        want.push_str(&format!("\x1b[1;3{}m row {i}\x1b[0m\r\n", i % 8));
    }
    want.push_str("DONE");
    assert_eq!(seen.live_text(), want);

    // 快照和标签页那边的终端画面完全一致，大小没被改。
    let snap = screen_of(host::snapshot(&host.dir, &id, None));
    let client = seen.screen.snapshot();
    assert_eq!(snap.lines, client.lines);
    assert_eq!((snap.cols, snap.rows), (COLS, ROWS));
    let last = &snap.lines[ROWS as usize - 2];
    assert_eq!(last[0].text, " row 300");
    assert_eq!(last[0].fg, CellColor::Indexed(4));
    assert_eq!(last[0].attrs, attr::BOLD);
    assert_eq!(texts(&snap)[ROWS as usize - 1], "DONE");
    assert!(host.info(&id).unwrap().attached, "快照后标签页还接着");

    // 标签页照常收发：输入还能到 CLI（stty raw 下 CLI 卡在 sleep，不回显，只确认通道没断）。
    att.writer().input(b"x").unwrap();
    assert!(host.info(&id).unwrap().attached);
}

#[test]
fn unchanged_screen_is_not_resent_and_detached_session_still_previews() {
    let host = TestHost::start();
    let id = host.spawn(
        "idle",
        &format!("{RAW} printf '\\033[32mA\\033[0m'; read x; printf B; sleep 60"),
    );
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    read_until(&mut att, &mut seen, "A", |s| {
        s.live_text().ends_with("\x1b[0m")
    });

    let first = screen_of(host::snapshot(&host.dir, &id, None));
    assert_eq!(texts(&first)[0], "A");
    assert_eq!(first.lines[0][0].fg, CellColor::Indexed(2));
    // 没有新输出：只回 Unchanged。
    for _ in 0..3 {
        assert_eq!(
            host::snapshot(&host.dir, &id, Some(first.version)).unwrap(),
            Snapshot::Unchanged
        );
    }

    att.writer().input(b"x\n").unwrap();
    read_until(&mut att, &mut seen, "B", |s| s.live_text().ends_with('B'));
    let second = screen_of(host::snapshot(&host.dir, &id, Some(first.version)));
    assert_ne!(second.version, first.version);
    assert_eq!(texts(&second)[0], "AB");

    // 关掉标签页后对话还在跑，照样能预览。
    drop(att);
    host.wait_info(&id, "客户端断开", |i| i.is_some_and(|i| !i.attached));
    let detached = screen_of(host::snapshot(&host.dir, &id, None));
    assert_eq!(detached.lines, second.lines);
}

#[test]
fn snapshot_of_unknown_or_unstarted_session_is_an_error() {
    let host = TestHost::start();
    let id = host.spawn("waiting", "sleep 60");
    let err = host::snapshot(&host.dir, &id, None).unwrap_err();
    assert!(err.to_string().contains("还没启动"), "{err}");
    let err = host::snapshot(&host.dir, &"nope".into(), None).unwrap_err();
    assert!(err.to_string().contains("没有这条托管对话"), "{err}");
    // 取快照不会启动它。
    assert_eq!(host.info(&id).unwrap().pid, None);
}
