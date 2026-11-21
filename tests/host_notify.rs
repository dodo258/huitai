//! 真起 `ai daemon`：假 CLI 响铃或发 OSC 9，提醒走到会话台（`HostEvent::Alert`）或假的系统通知
//! （`HUITAI_NOTIFY_TO` 文件）。守护进程由 `fake_env` 拉起，全局呼出键和真系统通知都关着。
//! 标签页连着且在前台时不提醒：用真的 `ai attach` 跑在一个假的外层终端里，往它的输入写焦点上报。

mod host_support;

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use host_support::{AI, Home, TestHost, WAIT, contains, fake_env, notifications_log, wait_event};
use huitai::core::{SessionId, SessionState, Vendor};
use huitai::host::{self, Attachment, BoardInfo, HostEvent, Request, Response, SpawnSpec};
use huitai::notify::Notifier;

/// 登记一条假 CLI（Claude、项目 demo），attach 一次让它跑起来。
fn start(home: &Home, id: &str, script: &str) -> (SessionId, Attachment) {
    let id = register(home, id, script);
    let att = host::attach(&home.data, &id, fake_env(&home.home), 60, 12).expect("attach 失败");
    (id, att)
}

/// 同 `start`，接上后马上断开：CLI 照跑，标签页没开着。
fn start_detached(home: &Home, id: &str, script: &str) -> SessionId {
    let (id, att) = start(home, id, script);
    assert_eq!(
        host::request(&home.data, &Request::Detach { id: id.clone() }).unwrap(),
        Response::Ok
    );
    drop(att);
    id
}

/// 只登记，不启动。
fn register(home: &Home, id: &str, script: &str) -> SessionId {
    let id = SessionId::from(id);
    let spec = SpawnSpec {
        id: id.clone(),
        vendor: Vendor::Claude,
        project: "demo".to_owned(),
        cwd: home.home.clone(),
        command: script.to_owned(),
    };
    assert_eq!(
        host::request(&home.data, &Request::Spawn(spec)).unwrap(),
        Response::Ok
    );
    id
}

fn board(alerts: bool) -> BoardInfo {
    BoardInfo {
        term_program: Some("ghostty".to_owned()),
        tty: Some("/dev/ttys999".to_owned()),
        pane: None,
        command: "ai".to_owned(),
        alerts,
    }
}

/// 等假系统通知文件里出现 `line`。
fn wait_notification(home: &Home, line: &str) {
    let path = notifications_log(&home.home);
    let deadline = Instant::now() + WAIT;
    loop {
        let text = fs::read_to_string(&path).unwrap_or_default();
        if text.lines().any(|l| l == line) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "等不到系统通知 {line:?}；文件里是 {text:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// 等到守护进程广播了这条对话的响铃，再多等过合并窗口，确认没有发出系统通知。
fn assert_bell_not_alerted(home: &Home, events: &mut host::EventStream) {
    wait_event(events, "Bell", |e| matches!(e, HostEvent::Bell { .. }));
    thread::sleep(Duration::from_millis(1000));
    let text = fs::read_to_string(notifications_log(&home.home)).unwrap_or_default();
    assert!(text.is_empty(), "标签页在前台时不该提醒：{text:?}");
}

/// 外层终端：PTY 里跑真的 `ai attach <id>`，读它写给终端的字节，往它的输入写字节（假装是终端发的）。
struct Outer {
    pty: host::pty::Pty,
    rx: Receiver<Vec<u8>>,
    seen: Vec<u8>,
}

impl Outer {
    fn attach(home: &Home, id: &SessionId) -> Self {
        let pty = host::pty::spawn(
            &format!("exec '{AI}' attach {id}"),
            &home.home,
            &fake_env(&home.home),
            80,
            24,
        )
        .expect("开外层终端失败");
        let mut reader = pty.master.try_clone().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        });
        Self {
            pty,
            rx,
            seen: Vec::new(),
        }
    }

    fn wait_for(&mut self, needle: &[u8]) {
        let deadline = Instant::now() + WAIT;
        while !contains(&self.seen, needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(chunk) => self.seen.extend_from_slice(&chunk),
                Err(_) => panic!(
                    "等不到 {:?}；已收到 {:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&self.seen)
                ),
            }
        }
    }

    fn type_in(&mut self, bytes: &[u8]) {
        (&self.pty.master).write_all(bytes).unwrap();
        thread::sleep(Duration::from_millis(100));
    }
}

impl Drop for Outer {
    fn drop(&mut self) {
        let _ = self.pty.child.kill();
        let _ = self.pty.child.wait();
    }
}

/// 每收到一行就响一次铃。
const BELL_PER_LINE: &str = "printf ready; while read l; do printf '\\007'; done";

fn daemon_log(home: &Home) -> String {
    fs::read_to_string(home.data.join("daemon.log")).unwrap_or_default()
}

#[test]
fn daemon_started_by_tests_has_hotkey_and_system_notifications_off() {
    let mut home = Home::new("nsw");
    home.ensure_daemon();
    let log = daemon_log(&home);
    assert!(log.contains("全局呼出键已关"), "daemon.log：{log:?}");
    assert!(log.contains("系统通知改写进"), "daemon.log：{log:?}");
    assert!(!log.contains("已注册"), "daemon.log：{log:?}");
}

#[test]
fn bell_without_board_goes_to_system_notification() {
    let mut home = Home::new("nbell");
    home.ensure_daemon();
    start_detached(
        &home,
        "bell-1",
        "printf ready; sleep 0.5; printf '\\007'; sleep 30",
    );
    wait_notification(&home, "✻ demo\t在等你回复");
}

#[test]
fn osc9_goes_to_open_board_not_system() {
    let mut home = Home::new("nosc");
    home.ensure_daemon();
    let mut events = host::board(&home.data, board(true)).expect("会话台订阅失败");
    // 标签页开着时 CLI 自己的 OSC 9 已原样到了那个终端，不再补；断开后才轮到会话台提醒。
    let id = start_detached(
        &home,
        "osc-1",
        "printf ready; sleep 0.8; printf '\\033]9;做完了\\007'; sleep 30",
    );
    let got = wait_event(&mut events, "Alert", |e| {
        matches!(e, HostEvent::Alert { .. })
    });
    assert_eq!(
        got.last(),
        Some(&HostEvent::Alert {
            id,
            title: "✻ demo".to_owned(),
            body: "做完了".to_owned(),
        })
    );
    assert!(
        got.iter().any(|e| matches!(e, HostEvent::Notify { .. })),
        "会话台也照常收到状态事件：{got:?}"
    );
    thread::sleep(Duration::from_millis(500));
    assert!(
        !notifications_log(&home.home).exists(),
        "交给会话台的提醒不该再发系统通知"
    );
}

#[test]
fn board_that_cannot_show_alerts_falls_back_to_system() {
    let mut home = Home::new("nnoalert");
    home.ensure_daemon();
    let mut events = host::board(&home.data, board(false)).expect("会话台订阅失败");
    start_detached(
        &home,
        "bell-2",
        "printf ready; sleep 0.5; printf '\\007'; sleep 30",
    );
    wait_notification(&home, "✻ demo\t在等你回复");
    let got = wait_event(&mut events, "Bell", |e| matches!(e, HostEvent::Bell { .. }));
    assert!(
        !got.iter().any(|e| matches!(e, HostEvent::Alert { .. })),
        "{got:?}"
    );
}

#[test]
fn closed_board_is_forgotten() {
    let mut home = Home::new("nclosed");
    home.ensure_daemon();
    drop(host::board(&home.data, board(true)).expect("会话台订阅失败"));
    start_detached(
        &home,
        "bell-3",
        "printf ready; sleep 0.8; printf '\\007'; sleep 30",
    );
    wait_notification(&home, "✻ demo\t在等你回复");
}

#[test]
fn seen_request_is_broadcast() {
    let host = TestHost::start();
    let mut events = host.subscribe();
    let id = SessionId::from("seen-1");
    assert_eq!(
        host.request(&Request::Seen { id: id.clone() }),
        Response::Ok
    );
    let got = wait_event(&mut events, "Seen", |e| matches!(e, HostEvent::Seen { .. }));
    assert_eq!(got.last(), Some(&HostEvent::Seen { id }));
}

#[test]
fn stalled_board_is_disconnected_so_it_reconnects() {
    let host = TestHost::start();
    // 会话台停住不读（被挂起、调试器停着）：事件攒满 socket 缓冲，守护进程写超时。
    let mut stalled = host::board(&host.dir, board(true)).expect("会话台订阅失败");
    let id = host.spawn(
        "titles",
        "i=0; while [ $i -lt 20000 ]; do printf '\\033]2;t%d\\007' $i; i=$((i+1)); done; sleep 60",
    );
    drop(host.attach(&id, 60, 12));
    // 写超时是 5 秒。
    thread::sleep(Duration::from_secs(8));
    // 恢复读：读完缓冲里剩下的就该看到断开（会话台据此重连、重取状态），而不是一直等不到新事件。
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "读了 {WAIT:?} 还没读完");
        match stalled.next(Some(Duration::from_secs(2))) {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                panic!("写超时之后连接还开着，会话台收不到断开、不会重连")
            }
            // 超时时可能只写出半帧。
            Err(_) => break,
        }
    }
}

#[test]
fn freshly_opened_tab_is_in_front_and_gets_no_alert_for_a_bell() {
    // 审查给的复现（第 3 轮）：标签页接着、终端没发过焦点事件，只响铃不该再发通知。
    // ai attach 接上时自己报一次「在前台」。
    let mut home = Home::new("nfront");
    home.ensure_daemon();
    let id = register(&home, "front-1", BELL_PER_LINE);
    let mut events = host::subscribe(&home.data).unwrap();
    let mut outer = Outer::attach(&home, &id);
    outer.wait_for(b"ready");
    wait_event(&mut events, "接上时的 Focus(true)", |e| {
        matches!(e, HostEvent::Focus { front: true, .. })
    });
    outer.type_in(b"b\r");
    assert_bell_not_alerted(&home, &mut events);
}

/// 会话台那边：订阅广播，用同一个映射喂自己的 `Notifier`（和 `Loop::host_event` 一样）。
struct BoardLamps {
    events: host::EventStream,
    notifier: Notifier,
}

impl BoardLamps {
    fn subscribe(home: &Home) -> Self {
        Self {
            events: host::subscribe(&home.data).unwrap(),
            notifier: Notifier::new(),
        }
    }

    /// 读到 `pred` 成立的事件为止，一路喂给 `Notifier`。
    fn until(&mut self, what: &str, pred: impl Fn(&HostEvent) -> bool) {
        for e in wait_event(&mut self.events, what, pred) {
            if let Some((id, a)) = huitai::ui::activity(e) {
                self.notifier.notify(id, a, Instant::now());
            }
        }
    }
}

#[test]
fn board_lamp_agrees_with_daemon_when_the_tab_is_switched_away() {
    let mut home = Home::new("nlampout");
    home.ensure_daemon();
    let id = register(&home, "lo-1", BELL_PER_LINE);
    let mut board = BoardLamps::subscribe(&home);
    let mut outer = Outer::attach(&home, &id);
    outer.wait_for(b"ready");
    outer.wait_for(b"\x1b[?1004h");
    board.until("Focus(true)", |e| {
        matches!(e, HostEvent::Focus { front: true, .. })
    });
    outer.type_in(b"\x1b[O");
    outer.type_in(b"b\r");
    board.until("Bell", |e| matches!(e, HostEvent::Bell { .. }));
    // 守护进程提醒了，会话台的灯是「刚做完」（未读）。
    wait_notification(&home, "✻ demo\t在等你回复");
    assert_eq!(board.notifier.state(&id), SessionState::Done);
}

#[test]
fn board_lamp_agrees_with_daemon_when_the_tab_is_in_front() {
    let mut home = Home::new("nlampin");
    home.ensure_daemon();
    let id = register(&home, "li-1", BELL_PER_LINE);
    let mut board = BoardLamps::subscribe(&home);
    let mut outer = Outer::attach(&home, &id);
    outer.wait_for(b"ready");
    outer.wait_for(b"\x1b[?1004h");
    outer.type_in(b"\x1b[O");
    outer.type_in(b"\x1b[I");
    board.until("切回前台", |e| {
        matches!(e, HostEvent::Focus { front: true, .. })
    });
    outer.type_in(b"b\r");
    board.until("Bell", |e| matches!(e, HostEvent::Bell { .. }));
    thread::sleep(Duration::from_millis(1000));
    let text = fs::read_to_string(notifications_log(&home.home)).unwrap_or_default();
    assert!(text.is_empty(), "在前台不该提醒：{text:?}");
    assert_eq!(board.notifier.state(&id), SessionState::Waiting);
}

#[test]
fn detaching_records_the_tab_as_not_in_front() {
    let mut home = Home::new("nlampdet");
    home.ensure_daemon();
    let mut events = host::subscribe(&home.data).unwrap();
    let id = start_detached(&home, "ld-1", "printf ready; sleep 30");
    let got = wait_event(&mut events, "Detached", |e| {
        matches!(e, HostEvent::Detached { .. })
    });
    let n = got.len();
    assert_eq!(
        got[n - 2],
        HostEvent::Focus {
            id: id.clone(),
            front: false
        },
        "{got:?}"
    );
}

#[test]
fn bell_after_the_tab_loses_focus_alerts() {
    let mut home = Home::new("nfout");
    home.ensure_daemon();
    let id = register(&home, "fout-1", BELL_PER_LINE);
    let mut outer = Outer::attach(&home, &id);
    outer.wait_for(b"ready");
    outer.wait_for(b"\x1b[?1004h");
    outer.type_in(b"\x1b[O");
    outer.type_in(b"b\r");
    wait_notification(&home, "✻ demo\t在等你回复");
}

#[test]
fn bell_after_the_tab_comes_back_to_front_does_not_alert() {
    let mut home = Home::new("nfin");
    home.ensure_daemon();
    let id = register(&home, "fin-1", BELL_PER_LINE);
    let mut outer = Outer::attach(&home, &id);
    outer.wait_for(b"ready");
    outer.wait_for(b"\x1b[?1004h");
    let mut events = host::subscribe(&home.data).unwrap();
    outer.type_in(b"\x1b[O");
    outer.type_in(b"\x1b[I");
    outer.type_in(b"b\r");
    assert_bell_not_alerted(&home, &mut events);
}

#[test]
fn focus_reports_do_not_reach_a_cli_that_did_not_ask_for_them() {
    let mut home = Home::new("nfstrip");
    home.ensure_daemon();
    let id = register(&home, "fs-1", "stty raw -echo; printf ready; exec cat -v");
    let mut outer = Outer::attach(&home, &id);
    outer.wait_for(b"ready");
    outer.type_in(b"\x1b[Ox\x1b[Iy\n");
    outer.wait_for(b"xy\n");
    assert!(
        !contains(&outer.seen, b"^[["),
        "焦点序列到了 CLI：{:?}",
        String::from_utf8_lossy(&outer.seen)
    );
}

#[test]
fn focus_reports_reach_a_cli_that_asked_for_them_unchanged() {
    let mut home = Home::new("nfpass");
    home.ensure_daemon();
    let id = register(
        &home,
        "fp-1",
        "stty raw -echo; printf '\\033[?1004h'; printf ready; exec cat -v",
    );
    let mut outer = Outer::attach(&home, &id);
    outer.wait_for(b"ready");
    outer.type_in(b"\x1b[Ox\x1b[Iy\n");
    outer.wait_for(b"^[[Ox^[[Iy\n");
}
