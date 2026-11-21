//! 后台：启动立刻返回；先扫一遍、建全文索引；之后 FSEvents 发现新文件和改动，只重读变了的文件。

mod index_common;

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use huitai::core::Vendor;
use huitai::index::Update;
use index_common::{Env, assistant_line, ids};

/// 等到 `want` 返回真，最多 `secs` 秒；期间把消息都收掉。
fn wait_for(rx: &Receiver<Update>, secs: u64, mut want: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if want() {
            return true;
        }
        let _ = rx.recv_timeout(Duration::from_millis(100));
    }
    want()
}

#[test]
fn background_scans_then_follows_file_changes() {
    let env = Env::new("watch");
    env.write_session("a", "修购物车", "好");
    let index = env.open();

    let t = Instant::now();
    let (bg, rx) = index.start();
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "start 不阻塞：{:?}",
        t.elapsed()
    );

    assert!(
        wait_for(&rx, 10, || ids(&index.list(Vendor::Claude).unwrap())
            == ["a"]),
        "后台首扫没出列表"
    );
    assert!(
        wait_for(&rx, 10, || !index
            .search(Vendor::Claude, "购物车", 10)
            .unwrap()
            .is_empty()),
        "首扫后没建全文索引"
    );
    env.take_reads();

    // 新文件：FSEvents 通知后只读这一个。
    env.write_session("b", "加优惠券", "好");
    assert!(
        wait_for(&rx, 10, || ids(&index.list(Vendor::Claude).unwrap())
            == ["a", "b"]),
        "新文件没被发现"
    );
    let reads = env.take_reads();
    assert!(
        reads.iter().all(|p| p == &env.session_path("b")),
        "只该重读新文件，实际读了 {reads:?}"
    );

    // 会话的附属目录（子代理记录）变了不影响列表，不重读。
    let side = env.session_path("a").with_extension("").join("subagents");
    std::fs::create_dir_all(&side).unwrap();
    std::fs::write(side.join("agent-1.jsonl"), "{}\n").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(env.take_reads().is_empty(), "附属文件不该触发重读");
    assert_eq!(ids(&index.list(Vendor::Claude).unwrap()), ["a", "b"]);

    // 改动：追加的新内容几秒内能搜到。
    env.append(
        "a",
        &assistant_line("顺便加了发票功能", "2026-11-01T10:00:00.000Z"),
    );
    assert!(
        wait_for(&rx, 20, || !index
            .search(Vendor::Claude, "发票", 10)
            .unwrap()
            .is_empty()),
        "改动后的正文没进索引"
    );

    // 丢掉 Background 线程就退出（不会卡住）。
    let t = Instant::now();
    drop(bg);
    assert!(t.elapsed() < Duration::from_secs(5));
}
