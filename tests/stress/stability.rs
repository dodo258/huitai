//! 托管层稳定性加测 2–8（docs/specs/host-stability-tests.md）。都不标 ignore，跟着 `cargo test` 跑；
//! 单独跑：`cargo test --test stress stability`。起真的 `ai daemon`（HOME 在 /tmp 下），从外面量描述符和线程。
//! 第 1 项（透传字节 proptest）在 tests/host_scan_prop.rs，第 9 项（长跑）在 soak.rs。

use super::*;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::host_support::contains;
use huitai::host::failpoint;
use huitai::host::protocol::{self, KIND_INPUT, KIND_REQUEST, KIND_RESIZE, KIND_RESPONSE};
use huitai::host::version;

/// 同 [`Rig::start`]，拉起守护进程前可以改它的启动命令（加环境变量、限描述符）。
fn rig_with(tag: &str, customize: impl FnOnce(&mut Command)) -> Rig {
    let mut home = Home::new(&format!("st-{tag}"));
    let mut launch = home.ai();
    launch.arg("daemon");
    customize(&mut launch);
    host::ensure_daemon(&home.data, launch).expect("拉起守护进程失败");
    let Response::Pong { pid } = host::request(&home.data, &Request::Ping).unwrap() else {
        panic!("Ping 应答不对");
    };
    home.daemon_pid = Some(pid);
    let script = home.home.join("fake_cli.pl");
    fs::write(&script, FAKE_CLI).unwrap();
    let stop = home.home.join("stop");
    Rig {
        home,
        pid,
        script,
        stop,
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: 信号 0 只查进程在不在。
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// 进程没了（也不是僵尸）：`ps` 查不到它。
fn gone(pid: u32) -> bool {
    run("ps", &["-o", "stat=", "-p", &pid.to_string()])
        .trim()
        .is_empty()
}

fn wait_for(what: &str, timeout: Duration, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ok() {
        assert!(Instant::now() < deadline, "等不到：{what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// 握过手的裸连接：后面想发什么帧自己发。
fn hello(rig: &Rig) -> UnixStream {
    let mut s = UnixStream::connect(host::socket_path(&rig.home.data)).expect("连不上守护进程");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    protocol::write_json(
        &mut s,
        KIND_REQUEST,
        &Request::Hello(version::current().clone()),
    )
    .unwrap();
    match protocol::read_json::<Response>(&mut s, KIND_RESPONSE).unwrap() {
        Response::Hello(h) => assert!(h.refused.is_none(), "握手被拒：{h:?}"),
        other => panic!("握手应答不对：{other:?}"),
    }
    s
}

/// 把本测试进程的描述符软上限调到至少 `n`（500 个并发连接要用）。
fn ensure_own_fd_limit(n: libc::rlim_t) {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: lim 是有效的可写 rlimit。
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim), 0);
        if lim.rlim_cur < n {
            lim.rlim_cur = n.min(lim.rlim_max);
            assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &lim), 0);
        }
    }
}

/// 读到对端关闭（EOF 或连接被重置）；`within` 内没关就失败。
fn closed_by_daemon(s: &mut UnixStream, within: Duration) -> bool {
    // 对端已经关了时 macOS 的 setsockopt 返回 EINVAL：那就是关了。
    if let Err(e) = s.set_read_timeout(Some(within)) {
        assert_eq!(e.raw_os_error(), Some(libc::EINVAL), "设读超时出错：{e}");
        return true;
    }
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + within;
    loop {
        match s.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return false;
            }
            Err(_) => return true,
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

fn live_bytes(r: &Reader) -> u64 {
    r.live.lock().unwrap().bytes
}

fn visible_has(r: &Reader, needle: &str) -> bool {
    r.visible().iter().any(|l| l.contains(needle))
}

// ---------- 2. 输入方向：大块输入原样到达 ----------

/// 5 MB 括号粘贴（0–255 每种字节都有）、kitty 键盘协议/CSI u 的 Shift+Enter 等按键、背靠背的 2 MB 大块，
/// 经守护进程到 CLI 后逐字节一致，顺序不乱。
#[test]
fn stability_big_input_arrives_byte_for_byte() {
    let rig = Rig::start("input");
    let mut want = b"\x1b[200~".to_vec();
    let mut x: u32 = 1;
    want.extend((0..5 << 20).map(|i: u32| {
        // 前 256 个字节把每种值都过一遍，后面伪随机。
        if i < 256 {
            i as u8
        } else {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (x >> 16) as u8
        }
    }));
    want.extend_from_slice(b"\x1b[201~");
    let keys: [&[u8]; 8] = [
        b"\x1b[13;2u",
        b"\x1b[13;5u",
        b"\x1b[97;5u",
        b"\x1b[57441;2u",
        b"\x1b[27;2;13~",
        b"\x1b[>1u",
        b"\x1bOP",
        "中文输入\r".as_bytes(),
    ];
    let mut sends: Vec<Vec<u8>> = vec![want.clone()];
    for k in keys {
        sends.push(k.to_vec());
    }
    for n in 0..3u8 {
        let block: Vec<u8> = (0..2 << 20)
            .map(|i: u32| b"ABCDEFGHIJKLMNOPQRSTUVWXYZ+/="[(i as usize + n as usize) % 29])
            .collect();
        sends.push(block);
    }
    let all: Vec<u8> = sends.concat();
    let id = rig.spawn(
        "input",
        &format!("printf READY; exec head -c {} > in.bin", all.len()),
    );
    let mut events = rig.subscribe();
    let mut att = rig.attach(&id);
    let mut seen = Vec::new();
    while !contains(&seen, b"READY") {
        match att.recv(Some(Duration::from_secs(10))) {
            Ok(AttachMsg::Output(b)) => seen.extend(b),
            other => panic!("等不到 READY：{other:?}"),
        }
    }
    let w = att.writer();
    let t = Instant::now();
    for s in &sends {
        w.input(s).expect("发输入");
    }
    // head 读满就退出，对话随之结束。
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "CLI 一直没读完 {} 字节", all.len());
        if let Ok(Some(HostEvent::Exited { id: got, .. })) = events.next(Some(left))
            && got == id
        {
            break;
        }
    }
    let got = fs::read(rig.home.home.join("in.bin")).unwrap();
    println!(
        "STABILITY|大输入|{:.1} MB 到达用时 {:?}",
        all.len() as f64 / 1048576.0,
        t.elapsed()
    );
    assert_eq!(got.len(), all.len(), "到达的字节数不对");
    if let Some(at) = got.iter().zip(&all).position(|(a, b)| a != b) {
        panic!(
            "第 {at} 个字节起不一致：期望 {:?}，收到 {:?}",
            &all[at..(at + 16).min(all.len())],
            &got[at..(at + 16).min(got.len())]
        );
    }
}

// ---------- 3. 慢客户端 ----------

/// 接着却完全不读的标签页：同时另一条对话的标签页照常收；守护进程内存有上限；
/// 积压超限后这个标签页被断开，它那条对话靠补发画面重连恢复；之前不读的连接收到结束而不是一直挂着。
#[test]
fn stability_slow_client_is_cut_off_and_reconnects() {
    let rig = Rig::start("slow");
    thread::sleep(Duration::from_millis(300));
    let base = rig.sample();
    let slow = rig.spawn_fast("slow", 8 << 20);
    let other = rig.spawn_fast("other", 256 << 10);
    // 不读：连补发画面都不读。
    let stuck = rig.attach(&slow);
    let reader = Reader::start(rig.attach(&other), &other, false);
    let peak = PeakRss::start(rig.pid);

    let mut worst = Duration::ZERO;
    let mut cut = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let before = live_bytes(&reader);
        let t = Instant::now();
        let attached = match rig.request(&Request::List) {
            Response::List(l) => l.iter().any(|i| i.id == slow && i.attached),
            other => panic!("List 应答不对：{other:?}"),
        };
        worst = worst.max(t.elapsed());
        thread::sleep(Duration::from_millis(200));
        assert!(
            live_bytes(&reader) > before,
            "另一条对话的标签页 200ms 没收到输出"
        );
        if !attached {
            cut = true;
            break;
        }
    }
    assert!(cut, "积压超限后不读的标签页没被断开");
    let peak_mb = mb(peak.finish()) - mb(base.rss_kb);
    println!("STABILITY|慢客户端|List 最慢 {worst:?}，守护进程 RSS 峰值增长 {peak_mb:.1} MB");
    assert!(worst < Duration::from_secs(1), "List 被拖慢到 {worst:?}");
    assert!(peak_mb < 96.0, "守护进程替不读的标签页攒了 {peak_mb:.1} MB");

    // 被断开的连接：读下去能读到底（积压的一部分加结束），不会一直挂着。
    let mut stuck = stuck;
    let end_by = Instant::now() + Duration::from_secs(30);
    loop {
        let left = end_by.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "被断开的标签页一直读不到头");
        match stuck.recv(Some(left)) {
            Ok(AttachMsg::Output(_)) => {}
            Ok(_) => break,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                panic!("被断开的标签页一直读不到头")
            }
            Err(_) => break,
        }
    }

    fs::write(&rig.stop, "").unwrap();
    rig.wait_screen(&slow, "FINAL slow row 0", Duration::from_secs(20));
    wait_for(
        "另一条的标签页画到 FINAL",
        Duration::from_secs(20),
        || visible_has(&reader, "FINAL other row 0"),
    );
    thread::sleep(Duration::from_millis(300));
    assert_eq!(rig.restored(&slow).visible_text(), final_rows("slow"));
    assert!(reader.alive(), "照常读的标签页被断开了");
    assert_eq!(reader.visible(), final_rows("other"));
    drop(reader);
    rig.kill_all(&[slow, other]);
    back_to_baseline(rig.pid, &base, "慢客户端之后");
}

// ---------- 4. 协议滥用 ----------

/// 垃圾、半帧、连上不说话、500 个并发连接、发完请求立刻断开、离谱的窗口大小：守护进程不崩，
/// 一直输出的对话和它的标签页不受影响，描述符和线程回到基线。守护进程按终端常见的 256 个描述符上限启动。
#[test]
fn stability_protocol_abuse() {
    ensure_own_fd_limit(2048);
    let rig = rig_with("abuse", |cmd| {
        // SAFETY: pre_exec 里只调 setrlimit。
        unsafe {
            cmd.pre_exec(|| {
                let lim = libc::rlimit {
                    rlim_cur: 256,
                    rlim_max: libc::RLIM_INFINITY,
                };
                libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
                Ok(())
            });
        }
    });

    // 没有对话、没有事件时，会话台和订阅关掉后也要放掉线程和描述符。
    thread::sleep(Duration::from_millis(300));
    let quiet = rig.sample();
    for _ in 0..20 {
        drop(host::board(&rig.home.data, huitai::host::BoardInfo::default()).unwrap());
        drop(rig.subscribe());
    }
    back_to_baseline(rig.pid, &quiet, "会话台和订阅关掉之后（没有事件）");

    let steady = rig.spawn_fast("steady", 256 << 10);
    let victim = rig.spawn("victim", "printf VICTIM; exec sleep 600");
    let reader = Reader::start(rig.attach(&steady), &steady, false);
    drop(rig.attach(&victim));
    rig.wait_screen(&victim, "VICTIM", Duration::from_secs(10));
    thread::sleep(Duration::from_millis(500));
    let base = rig.sample();
    let bytes_before = live_bytes(&reader);
    let sock = host::socket_path(&rig.home.data);

    // 随机垃圾：从第一个字节就不像话，或者帧头说的长度离谱。
    let mut x: u64 = 7;
    let mut rnd = |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    for i in 0..100 {
        let mut s = UnixStream::connect(&sock).unwrap();
        let junk: Vec<u8> = (0..rnd(2000)).map(|_| rnd(256) as u8).collect();
        let _ = match i % 3 {
            0 => s.write_all(&junk),
            1 => s.write_all(&[b'Q', 0xff, 0xff, 0xff, 0xff]),
            _ => {
                let _ = s.write_all(&[b'Q', 0, 0, 0, 5]);
                s.write_all(b"{\"x\"")
            }
        };
    }
    // 握手之后的垃圾请求：类型不对、JSON 不对。
    for _ in 0..20 {
        let mut s = hello(&rig);
        let _ = protocol::write_frame(&mut s, KIND_REQUEST, b"{\"no_such_request\":1}");
        let mut s = hello(&rig);
        let _ = protocol::write_frame(&mut s, b'Z', b"zzz");
    }

    // 连上不说话、发半帧不发完：守护进程到时间断开它们。
    let mut silent: Vec<UnixStream> = (0..20)
        .map(|_| UnixStream::connect(&sock).unwrap())
        .collect();
    for _ in 0..10 {
        let mut s = UnixStream::connect(&sock).unwrap();
        s.write_all(&[b'Q', 0, 0, 1, 0, b'{']).unwrap();
        silent.push(s);
    }

    // 500 个并发连接：同时开着，再一个个握手、Ping。
    let t = Instant::now();
    let mut many = Vec::new();
    for _ in 0..500 {
        let s = loop {
            match UnixStream::connect(&sock) {
                Ok(s) => break s,
                // 监听队列满了：等守护进程接走一些再连。
                Err(e) if t.elapsed() < Duration::from_secs(10) => {
                    let _ = e;
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("500 个并发连接连不上：{e}"),
            }
        };
        many.push(s);
    }
    for s in &mut many {
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        protocol::write_json(s, KIND_REQUEST, &Request::Hello(version::current().clone()))
            .expect("握手发不出去：守护进程没接这条连接");
        assert!(matches!(
            protocol::read_json::<Response>(s, KIND_RESPONSE).expect("握手没应答"),
            Response::Hello(_)
        ));
        protocol::write_json(s, KIND_REQUEST, &Request::Ping).unwrap();
        assert!(matches!(
            protocol::read_json::<Response>(s, KIND_RESPONSE).expect("Ping 没应答"),
            Response::Pong { .. }
        ));
    }
    println!("STABILITY|500 个并发连接|{:?}", t.elapsed());
    assert!(alive(rig.pid), "500 个并发连接后守护进程没了");
    drop(many);

    // 发完请求立刻断开，各种请求轮着来。
    let quick: Vec<SessionId> = (0..10).map(|i| SessionId::from(format!("q{i}"))).collect();
    for round in 0..10 {
        for (k, id) in quick.iter().enumerate() {
            let req = match (round + k) % 10 {
                0 => Request::List,
                1 => Request::Ping,
                2 => Request::Snapshot {
                    id: steady.clone(),
                    since: None,
                },
                3 => Request::Events,
                4 => Request::Board(huitai::host::BoardInfo::default()),
                5 => Request::Interrupted,
                6 => Request::Spawn(SpawnSpec {
                    id: id.clone(),
                    vendor: Vendor::Claude,
                    project: "abuse".to_owned(),
                    cwd: rig.home.home.clone(),
                    command: "exec sleep 600".to_owned(),
                }),
                7 => Request::Attach {
                    id: id.clone(),
                    env: fake_env(&rig.home.home),
                    cols: COLS,
                    rows: ROWS,
                },
                8 => Request::Seen { id: id.clone() },
                _ => Request::Kill {
                    id: SessionId::from("no-such"),
                },
            };
            let mut s = hello(&rig);
            protocol::write_json(&mut s, KIND_REQUEST, &req).unwrap();
        }
    }

    // 离谱的窗口大小：按原样分配会是几千万格。
    let rss_before = rig.sample().rss_kb;
    let mut big = hello(&rig);
    protocol::write_json(
        &mut big,
        KIND_REQUEST,
        &Request::Attach {
            id: victim.clone(),
            env: fake_env(&rig.home.home),
            cols: 5000,
            rows: 5000,
        },
    )
    .unwrap();
    assert_eq!(
        protocol::read_json::<Response>(&mut big, KIND_RESPONSE).unwrap(),
        Response::Ok
    );
    protocol::write_frame(&mut big, KIND_RESIZE, &protocol::resize_payload(0, 0)).unwrap();
    protocol::write_frame(&mut big, KIND_RESIZE, &protocol::resize_payload(4000, 6000)).unwrap();
    protocol::write_frame(&mut big, KIND_INPUT, b"").unwrap();
    thread::sleep(Duration::from_millis(500));
    let grew = mb(rig.sample().rss_kb) - mb(rss_before);
    println!("STABILITY|5000×5000 窗口|守护进程 RSS 增长 {grew:.1} MB");
    assert!(
        grew < 100.0,
        "报一个离谱的窗口大小就让守护进程多用了 {grew:.1} MB"
    );
    drop(big);

    for (i, s) in silent.iter_mut().enumerate() {
        assert!(
            closed_by_daemon(s, Duration::from_secs(10)),
            "第 {i} 条不说话/半帧的连接 10 秒还没被断开"
        );
    }
    drop(silent);

    assert!(alive(rig.pid), "守护进程没了");
    assert_eq!(rig.request(&Request::Ping), Response::Pong { pid: rig.pid });
    assert!(
        matches!(
            host::snapshot(&rig.home.data, &steady, None),
            Ok(Snapshot::Screen(_))
        ),
        "一直输出的对话取不到快照"
    );
    let ids: Vec<SessionId> = rig
        .pids()
        .into_iter()
        .map(|(id, _)| id)
        .filter(|id| quick.contains(id))
        .collect();
    rig.kill_all(&ids);
    for id in &quick {
        // 登记了但没启动的直接取消登记。
        let _ = host::request(&rig.home.data, &Request::Kill { id: id.clone() });
    }
    thread::sleep(Duration::from_millis(300));
    assert!(reader.alive(), "一直输出的对话的标签页被断开了");
    assert!(
        live_bytes(&reader) > bytes_before,
        "一直输出的对话的标签页没再收到输出"
    );
    back_to_baseline(rig.pid, &base, "协议滥用之后");
    drop(reader);
    rig.kill_all(&[steady, victim]);
}

// ---------- 5. 改窗口风暴 ----------

/// 大量输出期间连续改窗口大小 1000 次：守护进程照常应答，标签页不断开，CLI 收到最后的大小，
/// 重连的补发画面和一直接着的标签页画面一致。
#[test]
fn stability_resize_storm() {
    let rig = Rig::start("storm");
    let id = rig.spawn(
        "storm",
        &format!(
            "exec perl '{}' storm '{}' storm",
            rig.script.display(),
            rig.stop.display()
        ),
    );
    let att = rig.attach(&id);
    let w = att.writer();
    let reader = Reader::start(att, &id, false);
    thread::sleep(Duration::from_secs(1));

    let mut x: u32 = 99;
    let t = Instant::now();
    let mut worst = Duration::ZERO;
    for i in 0..1000 {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let cols = 20 + (x >> 8) as u16 % 200;
        let rows = 5 + (x >> 20) as u16 % 60;
        w.resize(cols, rows).expect("发改大小");
        if i % 100 == 0 {
            let q = Instant::now();
            rig.request(&Request::List);
            worst = worst.max(q.elapsed());
        }
    }
    w.resize(COLS, ROWS).unwrap();
    // 守护进程处理完全部改大小：画面大小回到最后一次。
    wait_for(
        "1000 次改大小处理完",
        Duration::from_secs(120),
        || {
            matches!(
                host::snapshot(&rig.home.data, &id, None),
                Ok(Snapshot::Screen(s)) if (s.cols, s.rows) == (COLS, ROWS)
            )
        },
    );
    println!(
        "STABILITY|改窗口风暴|1000 次处理完 {:?}，期间 List 最慢 {worst:?}",
        t.elapsed()
    );
    assert!(
        worst < Duration::from_secs(2),
        "改大小期间 List 卡了 {worst:?}"
    );
    thread::sleep(Duration::from_millis(300));
    fs::write(&rig.stop, "").unwrap();
    let size_line = format!("SIZE {COLS} x {ROWS}");
    rig.wait_screen(&id, &size_line, Duration::from_secs(20));
    wait_for("标签页画到 SIZE 行", Duration::from_secs(20), || {
        visible_has(&reader, &size_line)
    });
    thread::sleep(Duration::from_millis(300));
    assert!(reader.alive(), "改大小期间标签页被断开了");
    let live = reader.live.lock().unwrap().screen.signature();
    let restored = rig.restored(&id);
    assert!(
        restored.visible_text().iter().any(|l| l == &size_line),
        "CLI 没收到最后的窗口大小：{:?}",
        restored.visible_text()
    );
    assert_eq!(
        restored.signature(),
        live,
        "重连画面和一直接着的标签页不一致"
    );
    drop(reader);
    rig.kill_all(&[id]);
}

// ---------- 6. 难杀的子进程 ----------

/// 读出脚本记下的进程号（每行一个）。
fn read_pids(path: &PathBuf) -> Vec<u32> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// 结束对话时整个进程组清干净：CLI 和子孙都不理 SIGHUP/SIGTERM 的；CLI 一收 SIGHUP 就退、
/// 留下不理 SIGHUP 的子孙的。都不留孤儿和僵尸。
#[test]
fn stability_hard_to_kill_children_are_cleaned_up() {
    let rig = Rig::start("kill");
    let dir = &rig.home.home;
    // 全都不理 SIGHUP/SIGTERM（忽略的信号会传给子孙），子进程再生孙进程。
    let stubborn = rig.spawn(
        "stubborn",
        "trap '' HUP TERM; \
         (sleep 1000 & echo $! >> a.pids; sleep 1000) & echo $! >> a.pids; \
         sleep 1000 & echo $! >> a.pids; echo $$ >> a.pids; \
         printf READY; while :; do sleep 1; done",
    );
    // CLI 自己收 SIGHUP 就退，留下不理 SIGHUP/SIGTERM 的子孙（还占着 PTY）。
    let leaver = rig.spawn(
        "leaver",
        "(trap '' HUP TERM; exec sleep 1000) & echo $! >> b.pids; \
         (trap '' HUP TERM; sleep 1000 & echo $! >> b.pids; wait) & echo $! >> b.pids; \
         echo $$ >> b.pids; printf READY; wait",
    );
    for id in [&stubborn, &leaver] {
        drop(rig.attach(id));
        rig.wait_screen(id, "READY", Duration::from_secs(10));
    }
    let a = read_pids(&dir.join("a.pids"));
    let b = read_pids(&dir.join("b.pids"));
    assert_eq!((a.len(), b.len()), (4, 4), "脚本没记全进程号");
    assert!(a.iter().chain(&b).all(|&p| alive(p)));

    let t = Instant::now();
    rig.kill_all(&[stubborn, leaver]);
    println!("STABILITY|难杀的子进程|Kill 到两条都退出 {:?}", t.elapsed());
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !a.iter().chain(&b).all(|&p| gone(p)) {
        thread::sleep(Duration::from_millis(50));
    }
    let left: Vec<u32> = a.iter().chain(&b).copied().filter(|&p| !gone(p)).collect();
    assert!(
        left.is_empty(),
        "还剩进程（孤儿或僵尸）：a={a:?} b={b:?} 剩 {left:?}\n{}",
        run(
            "ps",
            &[
                "-o",
                "pid,ppid,pgid,stat,command",
                "-p",
                &left
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ]
        )
    );
}

// ---------- 7. 启动竞争 ----------

/// `lsof` 看谁开着锁文件：活着的守护进程一直开着它。
fn lock_holders(rig_data: &std::path::Path) -> Vec<u32> {
    let lock = rig_data.join("host.lock");
    run("lsof", &["-t", lock.to_str().unwrap()])
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// 10 个同时拉起守护进程（`ai` 启动时都这么做），只起一个；`kill -9` 留下旧 socket 和锁文件后，
/// 再同时拉起 10 个也能自动恢复，还是只有一个。
#[test]
fn stability_concurrent_start_brings_up_one_daemon() {
    let mut home = Home::new("race");
    for round in 0..2 {
        let barrier = std::sync::Barrier::new(10);
        let pids: Vec<u32> = thread::scope(|s| {
            let hs: Vec<_> = (0..10)
                .map(|_| {
                    s.spawn(|| {
                        let mut launch = home.ai();
                        launch.arg("daemon");
                        barrier.wait();
                        host::ensure_daemon(&home.data, launch)
                            .expect("拉起守护进程失败")
                            .pid
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let pid = pids[0];
        assert!(
            pids.iter().all(|&p| p == pid),
            "第 {round} 轮连上了不止一个守护进程：{pids:?}"
        );
        home.daemon_pid = Some(pid);
        // 另外再直接起 10 个 `ai daemon`：都发现已经有了，正常退出。
        let extra: Vec<_> = (0..10)
            .map(|_| home.ai().arg("daemon").spawn().unwrap())
            .collect();
        for mut c in extra {
            // 已经有守护进程时它抢不到锁、马上退出；限时 10 秒，卡住就直接失败，不让测试一直挂着。
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let status = loop {
                if let Some(s) = c.try_wait().unwrap() {
                    break s;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = c.kill();
                    panic!("第 {round} 轮：多起的守护进程（pid {}）10 秒没退出", c.id());
                }
                thread::sleep(Duration::from_millis(20));
            };
            assert!(status.success(), "多起的守护进程没正常退出");
        }
        wait_for(
            "只剩一个守护进程开着锁文件",
            Duration::from_secs(5),
            || lock_holders(&home.data) == vec![pid],
        );
        assert_eq!(
            host::request(&home.data, &Request::Ping).unwrap(),
            Response::Pong { pid }
        );
        if round == 0 {
            home.kill_daemon_hard();
            assert!(
                host::socket_path(&home.data).exists() && home.data.join("host.lock").exists(),
                "kill -9 后应该留着旧 socket 和锁文件"
            );
        }
    }
}

// ---------- 8. panic 隔离 ----------

/// 守护进程处理一条对话的输出时 panic（debug 构建的故障注入）：这条对话换新状态机接着转发，标签页不断；
/// 别的对话照常；守护进程照常应答；daemon.log 里有 panic 记录。
#[test]
fn stability_panic_is_isolated_and_logged() {
    if !cfg!(debug_assertions) {
        println!(
            "STABILITY|panic 隔离|release 构建没有故障注入点，这项只在 debug 构建（cargo test）里测"
        );
        return;
    }
    const MARK: &str = "BOOM-FAILPOINT-MARK";
    let rig = rig_with("panic", |cmd| {
        cmd.env(failpoint::PANIC_ON_OUTPUT, MARK);
    });
    let crash = rig.spawn(
        "crash",
        &format!(
            "printf 'before\\r\\n'; while [ ! -e go ]; do sleep 0.05; done; \
             printf '{MARK}\\r\\n'; sleep 0.3; printf 'after-panic\\r\\n'; exec sleep 600"
        ),
    );
    let calm = rig.spawn_fast("calm", 64 << 10);
    let crash_reader = Reader::start(rig.attach(&crash), &crash, false);
    let calm_reader = Reader::start(rig.attach(&calm), &calm, false);
    wait_for("crash 画出 before", Duration::from_secs(10), || {
        visible_has(&crash_reader, "before")
    });

    fs::write(rig.home.home.join("go"), "").unwrap();
    let log = rig.home.data.join("daemon.log");
    wait_for("daemon.log 记下 panic", Duration::from_secs(10), || {
        fs::read_to_string(&log).is_ok_and(|l| l.contains("panicked") && l.contains("故障注入"))
    });
    let calm_before = live_bytes(&calm_reader);
    wait_for(
        "panic 之后 crash 的标签页还收到输出",
        Duration::from_secs(10),
        || visible_has(&crash_reader, "after-panic"),
    );
    wait_for("别的对话照常输出", Duration::from_secs(5), || {
        live_bytes(&calm_reader) > calm_before
    });
    assert!(alive(rig.pid), "守护进程没了");
    assert!(
        crash_reader.alive() && calm_reader.alive(),
        "标签页被断开了"
    );
    let list = rig.pids();
    assert_eq!(list.len(), 2, "托管列表不对：{list:?}");
    let text = fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("换新状态机接着转发"),
        "daemon.log 没写恢复记录：{text}"
    );
    // 重连：补发画面是 panic 之后重新攒的。
    drop(crash_reader);
    let restored = rig.restored(&crash);
    assert!(
        restored
            .visible_text()
            .iter()
            .any(|l| l.contains("after-panic")),
        "重连画面不对：{:?}",
        restored.visible_text()
    );
    drop(calm_reader);
    rig.kill_all(&[crash, calm]);
}
