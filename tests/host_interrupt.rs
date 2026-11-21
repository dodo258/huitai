//! 中断恢复：守护进程被 `kill -9`（断电、崩溃）后重启，能认出当时还在跑的对话是「被中断」；
//! 自己正常退出的、被结束的、登记了没启动的都不算。

mod host_support;

use std::time::{Duration, Instant};

use host_support::{Home, TestHost, WAIT, fake_env};
use huitai::core::{SessionId, Vendor};
use huitai::host::{self, Daemon, HostEvent, Request, Response, RunRecord, SpawnSpec, interrupt};

fn spawn(home: &Home, id: &str, vendor: Vendor, command: &str) -> SessionId {
    let id = SessionId::from(id);
    let spec = SpawnSpec {
        id: id.clone(),
        vendor,
        project: "proj".to_owned(),
        cwd: home.home.clone(),
        command: command.to_owned(),
    };
    assert_eq!(
        host::request(&home.data, &Request::Spawn(spec)).unwrap(),
        Response::Ok
    );
    id
}

/// attach 一次把进程拉起来，等到它的第一行输出后断开（对话照跑）。
fn start(home: &Home, id: &SessionId) {
    let mut att = host::attach(&home.data, id, fake_env(&home.home), 60, 12).unwrap();
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("ready") {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "等不到 {id} 起来");
        match att.recv(Some(left)).unwrap() {
            host::AttachMsg::Output(b) => seen.extend_from_slice(&b),
            other => panic!("{id} 提前结束：{other:?}"),
        }
    }
}

fn interrupted(home: &Home) -> Vec<RunRecord> {
    match host::request(&home.data, &Request::Interrupted).unwrap() {
        Response::Interrupted(list) => list,
        other => panic!("Interrupted 应答不对：{other:?}"),
    }
}

fn ids(list: &[RunRecord]) -> Vec<&str> {
    list.iter().map(|r| r.id.as_str()).collect()
}

/// 等这几条都退出（顺序不限）。
fn wait_exited(events: &mut host::EventStream, ids: &[&SessionId]) {
    let mut left_ids: Vec<&SessionId> = ids.to_vec();
    let deadline = Instant::now() + WAIT;
    while !left_ids.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "等不到退出：{left_ids:?}");
        if let Some(HostEvent::Exited { id, .. }) = events.next(Some(left)).unwrap() {
            left_ids.retain(|x| **x != id);
        }
    }
}

#[test]
fn kill9_then_restart_marks_running_conversations_interrupted() {
    let mut home = Home::new("int");
    home.ensure_daemon();
    let mut events = host::subscribe(&home.data).unwrap();

    let running = "printf ready; while :; do sleep 1; done";
    let a = spawn(&home, "a-busy", Vendor::Claude, running);
    let b = spawn(&home, "b-busy", Vendor::Codex, running);
    // 自己退出的（CLI /exit）和被 x 结束的：正常收尾，不算被中断。
    let quits = spawn(
        &home,
        "c-quits",
        Vendor::Pi,
        "printf ready; sleep 0.3; exit 0",
    );
    let killed = spawn(&home, "d-killed", Vendor::Claude, running);
    // 登记了但从没 attach 过：没有进程，不算。
    spawn(&home, "e-never", Vendor::Claude, running);
    for id in [&a, &b, &quits, &killed] {
        start(&home, id);
    }
    assert_eq!(
        host::request(&home.data, &Request::Kill { id: killed.clone() }).unwrap(),
        Response::Ok
    );
    wait_exited(&mut events, &[&quits, &killed]);

    // 正常跑着的守护进程没有被中断的对话；running.json 里正好是还在跑的两条。
    assert!(interrupted(&home).is_empty());
    let on_disk = host::interrupt::load(&host::interrupt::running_path(&home.data)).unwrap();
    assert_eq!(
        on_disk.keys().map(SessionId::as_str).collect::<Vec<_>>(),
        ["a-busy", "b-busy"]
    );

    home.kill_daemon_hard();
    home.ensure_daemon();

    let list = interrupted(&home);
    assert_eq!(ids(&list), ["a-busy", "b-busy"]);
    assert_eq!(list[1].vendor, Vendor::Codex);
    assert_eq!(list[1].cwd, home.home);
    assert!(list[0].started_at > 0);
    // 新守护进程里没有在跑的对话，running.json 已清空。
    assert!(!host::interrupt::running_path(&home.data).exists());

    // 再正常重启一次（中间没被中断的对话在跑）：列表原样保留，不重复、不新增。
    home.kill_daemon_hard();
    home.ensure_daemon();
    assert_eq!(ids(&interrupted(&home)), ["a-busy", "b-busy"]);

    // 重开其中一条：进程起来就从被中断里拿掉；另一条用户手动移出。
    spawn(&home, "a-busy", Vendor::Claude, running);
    start(&home, &a);
    assert_eq!(ids(&interrupted(&home)), ["b-busy"]);
    assert_eq!(
        host::request(
            &home.data,
            &Request::Dismiss {
                ids: vec![b.clone()]
            }
        )
        .unwrap(),
        Response::Ok
    );
    assert!(interrupted(&home).is_empty());

    // 重开的那条之后正常退出，守护进程再被杀：不算被中断。
    assert_eq!(
        host::request(&home.data, &Request::Kill { id: a.clone() }).unwrap(),
        Response::Ok
    );
    let deadline = Instant::now() + WAIT;
    while host::interrupt::running_path(&home.data).exists() {
        assert!(Instant::now() < deadline, "结束后 running.json 没清掉");
        std::thread::sleep(Duration::from_millis(20));
    }
    home.kill_daemon_hard();
    home.ensure_daemon();
    assert!(interrupted(&home).is_empty());
}

#[test]
fn reopened_interrupted_conversation_that_dies_at_once_stays_interrupted() {
    // 上一个守护进程断电时留下两条在跑记录，这一个启动时认成被中断（进程内起，不用真的 kill -9）。
    let dir = std::path::PathBuf::from(format!("/tmp/hti8-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let record = |id: &str| RunRecord {
        id: SessionId::from(id),
        vendor: Vendor::Claude,
        project: "proj".to_owned(),
        cwd: dir.clone(),
        started_at: 1,
    };
    let left: interrupt::Records = ["dies", "works"]
        .into_iter()
        .map(|id| (SessionId::from(id), record(id)))
        .collect();
    interrupt::save(&interrupt::running_path(&dir), &left).unwrap();
    let daemon = Daemon::bind(&dir).unwrap();
    std::thread::spawn(move || daemon.run());
    let host = TestHost { dir };
    let list = || match host.request(&Request::Interrupted) {
        Response::Interrupted(l) => l.into_iter().map(|r| r.id).collect::<Vec<_>>(),
        other => panic!("Interrupted 应答不对：{other:?}"),
    };
    assert_eq!(list(), [SessionId::from("dies"), SessionId::from("works")]);
    let mut events = host.subscribe();

    // 登录 shell 里找不到 CLI：shell 起来了，马上非正常退出。
    let dies = host.spawn("dies", "exit 127");
    let _ = host::attach(&host.dir, &dies, fake_env(&host.dir), 60, 12);
    wait_exited(&mut events, &[&dies]);
    assert_eq!(list(), [SessionId::from("dies"), SessionId::from("works")]);

    // 真起来了的照旧拿掉。
    let works = host.spawn("works", "printf ready; while :; do sleep 1; done");
    let mut att = host.attach(&works, 60, 12);
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("ready") {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "等不到 works 起来");
        if let host::AttachMsg::Output(b) = att.recv(Some(left)).unwrap() {
            seen.extend_from_slice(&b);
        }
    }
    assert_eq!(list(), [SessionId::from("dies")]);
}
