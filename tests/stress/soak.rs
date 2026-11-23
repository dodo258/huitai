//! 长跑（docs/specs/host-stability-tests.md 第 9 项）：手动跑，不进 verify。
//!
//! `tests/stress/run.sh soak`（默认 6 小时）。一个真的 `ai daemon` 上同时跑混合负载：
//! - 常驻：6 条一直输出的对话（2 条一直有标签页接着读），模拟开着几个 CLI 干活；
//! - 来来去去：每轮新开一条对话、接上读一会儿、改几次窗口大小、断开再接、结束；再开一条自己很快退出的；
//! - 会话台那边：不停 List、取快照、订阅事件又关掉、会话台登记又关掉，偶尔发一串垃圾连接；
//!
//! 每分钟记一次守护进程的 RSS、描述符、线程、CPU（`SOAK|` 开头的行，同时写进 `SOAK_CSV` 指定的文件），
//! 每分钟另记一次存活堆（`heap`，判断泄漏的依据）。结束时用最小二乘算后半程的斜率：存活堆、描述符、线程不能持续上涨；
//! 跑满 [`RSS_MIN_MINUTES`] 的再看 RSS 斜率。全部结束后描述符、线程、存活堆回到基线。

use super::*;

use std::io::Write;
use std::os::unix::net::UnixStream;

/// 常驻对话数和其中一直有标签页接着的条数。
const RESIDENT: usize = 6;
const WATCHED: usize = 2;
/// 后半程 RSS 斜率上限（MB/小时）。分配器留着的空闲页会让 RSS 先涨到一个平台，平台之后不该再涨。
const RSS_SLOPE_LIMIT: f64 = 2.0;
/// RSS 斜率只在跑满这么久时判：11-22 的 6 小时长跑里 RSS 第 1 小时涨 12 MB、第 2 小时 2 MB 后才到平台，
/// 1 小时的后半程还在这段爬坡里，斜率说明不了泄漏。短跑靠存活堆斜率判。
const RSS_MIN_MINUTES: f64 = 180.0;
/// 存活堆「地板」（一段时间里的最小值）最后四分之一比第二个四分之一最多高这么多（MB）。
/// 采样那一刻常有正在转发的输出缓冲（来去的对话、机器忙时积压），单点会在 40↔44 MB 间跳，按斜率判会被噪声带偏；
/// 泄漏的内存放不掉，会把地板一起抬高。
const HEAP_FLOOR_RISE_LIMIT: f64 = 2.0;
/// 后半程描述符、线程斜率上限（个/小时）：来来去去的连接会让瞬时值上下跳，但不能一直涨。
const COUNT_SLOPE_LIMIT: f64 = 2.0;

/// 最小二乘斜率：`y` 对 `x` 每单位的变化。
fn slope(points: &[(f64, f64)]) -> f64 {
    let n = points.len() as f64;
    let (sx, sy) = points
        .iter()
        .fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
    let (mx, my) = (sx / n, sy / n);
    let (num, den) = points.iter().fold((0.0, 0.0), |(num, den), (x, y)| {
        (num + (x - mx) * (y - my), den + (x - mx) * (x - mx))
    });
    if den == 0.0 { 0.0 } else { num / den }
}

/// 一分钟一行。
struct Row {
    minute: f64,
    s: Sample,
    cpu: f64,
    heap: u64,
}

/// 来来去去的那部分负载，直到 `stop`。返回做了多少轮。
fn churn(rig: &Rig, stop: &AtomicBool) -> u64 {
    let mut round = 0u64;
    while !stop.load(Ordering::Relaxed) {
        round += 1;
        let id = rig.spawn_fast(&format!("churn{round}"), 256 * 1024);
        let mut att = rig.attach(&id);
        let w = att.writer();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut i = 0u16;
        while Instant::now() < deadline {
            match att.recv(Some(Duration::from_millis(200))) {
                Ok(AttachMsg::Output(_)) => {}
                Ok(other) => panic!("churn{round} 的标签页意外结束：{other:?}"),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => panic!("churn{round} 读出错：{e}"),
            }
            i += 1;
            if i.is_multiple_of(5) {
                w.resize(COLS - (i % 40), ROWS - (i % 10)).unwrap();
            }
        }
        drop(att);
        let mut att = rig.attach(&id);
        first_frame(&mut att, &id);
        drop(att);
        rig.kill_all(&[id]);

        let quick = rig.spawn(&format!("quick{round}"), "printf 'bye\\r\\n'; exit 0");
        let mut events = rig.subscribe();
        drop(rig.attach(&quick));
        wait_exited(&mut events, &quick);
        thread::sleep(Duration::from_secs(5));
    }
    round
}

fn wait_exited(events: &mut EventStream, id: &SessionId) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "{id} 等不到退出");
        if let Ok(Some(HostEvent::Exited { id: got, .. })) = events.next(Some(left))
            && &got == id
        {
            return;
        }
    }
}

/// 会话台那边的请求，直到 `stop`。
fn chatter(rig: &Rig, ids: &[SessionId], stop: &AtomicBool) -> u64 {
    let mut n = 0u64;
    while !stop.load(Ordering::Relaxed) {
        n += 1;
        rig.request(&Request::List);
        let id = &ids[n as usize % ids.len()];
        host::snapshot(&rig.home.data, id, None).expect("取快照");
        drop(rig.subscribe());
        let board = host::board(
            &rig.home.data,
            huitai::host::BoardInfo {
                command: "soak".to_owned(),
                ..Default::default()
            },
        )
        .expect("会话台登记");
        drop(board);
        if n.is_multiple_of(30) {
            let mut s = UnixStream::connect(host::socket_path(&rig.home.data)).unwrap();
            let _ = s.write_all(b"\x00\x00\x00\xff\xffgarbage");
        }
        thread::sleep(Duration::from_secs(2));
    }
    n
}

#[test]
#[ignore = "长跑，手动跑：tests/stress/run.sh soak（默认 6 小时）"]
fn soak() {
    let minutes = env_f64("SOAK_MINUTES", 360.0);
    let every = Duration::from_secs_f64(env_f64("SOAK_SAMPLE_SECS", 60.0));
    let rate = (env_f64("SOAK_RATE_KB", 64.0) * 1024.0) as u64;
    let mut csv = std::env::var("SOAK_CSV")
        .ok()
        .map(|p| fs::File::create(p).expect("SOAK_CSV 建不了"));

    let rig = Rig::start("soak");
    thread::sleep(Duration::from_millis(500));
    let base = rig.sample();
    let base_heap = heap_bytes(rig.pid);
    println!(
        "SOAK|基线|rss={:.1}MB fds={} threads={} heap={:.2}MB",
        mb(base.rss_kb),
        base.fds,
        base.threads,
        base_heap as f64 / 1048576.0
    );

    let ids: Vec<SessionId> = (0..RESIDENT)
        .map(|i| rig.spawn_fast(&format!("res{i}"), rate))
        .collect();
    let readers: Vec<Reader> = ids[..WATCHED]
        .iter()
        .map(|id| Reader::start(rig.attach(id), id, false))
        .collect();
    for id in &ids[WATCHED..] {
        drop(rig.attach(id));
    }

    let stop = AtomicBool::new(false);
    let start = Instant::now();
    let total = Duration::from_secs_f64(minutes * 60.0);
    let mut rows: Vec<Row> = Vec::new();
    let (rounds, requests) = thread::scope(|scope| {
        let churner = scope.spawn(|| churn(&rig, &stop));
        let chatterer = scope.spawn(|| chatter(&rig, &ids, &stop));
        let mut prev = rig.sample();
        let mut prev_at = Instant::now();
        while start.elapsed() < total {
            thread::sleep(every.min(total.saturating_sub(start.elapsed())));
            let s = rig.sample();
            let now = Instant::now();
            let row = Row {
                minute: start.elapsed().as_secs_f64() / 60.0,
                s,
                cpu: cpu_percent(&prev, &s, now - prev_at),
                heap: heap_bytes(rig.pid),
            };
            let line = format!(
                "{:.1},{:.1},{},{},{:.2},{:.1}",
                row.minute,
                mb(s.rss_kb),
                s.fds,
                s.threads,
                row.cpu,
                row.heap as f64 / 1048576.0
            );
            println!("SOAK|{line}");
            if let Some(f) = csv.as_mut() {
                writeln!(f, "{line}").unwrap();
            }
            rows.push(row);
            (prev, prev_at) = (s, now);
            assert!(
                !churner.is_finished(),
                "来来去去的负载线程提前结束（看上面的 panic）"
            );
            assert!(
                !chatterer.is_finished(),
                "会话台负载线程提前结束（看上面的 panic）"
            );
            for (r, id) in readers.iter().zip(&ids) {
                assert!(r.alive(), "{id} 的标签页被断开了");
            }
        }
        stop.store(true, Ordering::Relaxed);
        (churner.join().unwrap(), chatterer.join().unwrap())
    });

    // 后半程（前一半当预热：历史攒满、分配器长到平台）。
    let half: Vec<&Row> = rows.iter().skip(rows.len() / 2).collect();
    let fit = |f: &dyn Fn(&Row) -> f64| {
        let pts: Vec<(f64, f64)> = half.iter().map(|r| (r.minute / 60.0, f(r))).collect();
        slope(&pts)
    };
    let rss_slope = fit(&|r| mb(r.s.rss_kb));
    let heap_slope = fit(&|r| r.heap as f64 / 1048576.0);
    let floor = |part: &[Row]| part.iter().map(|r| r.heap).min().unwrap_or(0) as f64 / 1048576.0;
    let q = rows.len() / 4;
    let heap_floor_rise = floor(&rows[rows.len() - q..]) - floor(&rows[q..2 * q]);
    let fd_slope = fit(&|r| r.s.fds as f64);
    let thread_slope = fit(&|r| r.s.threads as f64);
    let avg_cpu = rows.iter().map(|r| r.cpu).sum::<f64>() / rows.len().max(1) as f64;
    let peak = rows.iter().map(|r| r.s.rss_kb).max().unwrap_or(0);
    println!(
        "SOAK|结果|{:.0} 分钟 {} 个点；后半程斜率 RSS {rss_slope:+.2} MB/h、存活堆 {heap_slope:+.2} MB/h（地板 {heap_floor_rise:+.2} MB）、描述符 {fd_slope:+.2}/h、线程 {thread_slope:+.2}/h；\
         RSS 峰值 {:.1} MB；平均 CPU {avg_cpu:.1}%；来去 {rounds} 轮、会话台请求 {requests} 轮",
        minutes,
        rows.len(),
        mb(peak)
    );

    for r in readers {
        assert!(r.alive(), "常驻标签页在结束前被断开了");
    }
    rig.kill_all(&ids);
    let after = back_to_baseline(rig.pid, &base, "长跑结束");
    let heap = heap_bytes(rig.pid);
    println!(
        "SOAK|收尾|fds={} threads={} heap={:.2}MB（基线 {:.2}MB）",
        after.fds,
        after.threads,
        heap as f64 / 1048576.0,
        base_heap as f64 / 1048576.0
    );
    assert!(rows.len() >= 4, "点太少，算不了斜率");
    assert!(
        heap_floor_rise < HEAP_FLOOR_RISE_LIMIT,
        "存活堆地板还在涨：最后四分之一比第二个四分之一高 {heap_floor_rise:+.2} MB"
    );
    assert!(
        minutes < RSS_MIN_MINUTES || rss_slope < RSS_SLOPE_LIMIT,
        "后半程内存还在涨：{rss_slope:+.2} MB/h"
    );
    // 描述符、线程也随采样那一刻有没有来去的对话在两三档之间跳（30/33/35）；短跑看地板，跑满 3 小时再加看斜率。
    let rise = |f: &dyn Fn(&Row) -> usize| {
        let lo = |part: &[Row]| part.iter().map(f).min().unwrap_or(0) as f64;
        lo(&rows[rows.len() - q..]) - lo(&rows[q..2 * q])
    };
    let (fd_rise, thread_rise) = (rise(&|r| r.s.fds), rise(&|r| r.s.threads));
    assert!(
        fd_rise < COUNT_SLOPE_LIMIT && (minutes < RSS_MIN_MINUTES || fd_slope < COUNT_SLOPE_LIMIT),
        "描述符还在涨：地板 {fd_rise:+.0}，后半程斜率 {fd_slope:+.2}/h"
    );
    assert!(
        thread_rise < COUNT_SLOPE_LIMIT
            && (minutes < RSS_MIN_MINUTES || thread_slope < COUNT_SLOPE_LIMIT),
        "线程还在涨：地板 {thread_rise:+.0}，后半程斜率 {thread_slope:+.2}/h"
    );
    assert!(
        heap <= base_heap + HEAP_SLACK,
        "全部结束后存活堆没回到基线：{heap} 字节（基线 {base_heap}）"
    );
}

#[test]
fn slope_of_a_line_is_its_gradient() {
    assert!((slope(&[(0.0, 1.0), (1.0, 3.0), (2.0, 5.0)]) - 2.0).abs() < 1e-9);
    assert_eq!(slope(&[(1.0, 4.0), (1.0, 9.0)]), 0.0);
}
