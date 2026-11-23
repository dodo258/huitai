//! 透传托管的集成测试：进程内守护进程 + sh 脚本当假 CLI。

mod host_support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::thread;
use std::time::Duration;

use host_support::*;
use huitai::core::{SessionId, SessionState};
use huitai::host::{
    AttachMsg, Daemon, EndReason, EventStream, HostEvent, Request, Response, Screen, socket_path,
};
use huitai::notify::Notifier;

/// 假 CLI 都先切原始模式（和真 TUI 一样），这样输出不经换行转换、输入不回显。
const RAW: &str = "stty raw -echo;";

#[test]
fn reattach_restores_screen_history_and_output_made_while_detached() {
    let host = TestHost::start();
    // 第一段：彩色字、宽字符、自动折行的长行、超过一屏的历史。
    let mut part_a = b"\x1b[1;32mgreen\x1b[0m \xe4\xb8\xad\xe6\x96\x87 plain\r\n".to_vec();
    for i in 0..30 {
        part_a.extend_from_slice(format!("hist {i}\r\n").as_bytes());
    }
    part_a.extend_from_slice(&[b'w'; 75]);
    part_a.extend_from_slice(b"\r\nREADY");
    // 第二段在断开期间输出，最后设个标题当「输出完了」的记号。
    let part_b = b"\r\nafter-detach \x1b[4;38;5;208munder\x1b[0m\r\n\x1b[?2004h\x1b]2;phase2\x07";
    host.file("a.bin", &part_a);
    host.file("b.bin", part_b);
    let id = host.spawn(
        "s1",
        &format!("{RAW} cat a.bin; read x; cat b.bin; sleep 60"),
    );
    let mut events = host.subscribe();

    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    read_until(&mut att, &mut seen, "第一段输出", |s| {
        s.live_text().ends_with("READY")
    });
    att.writer().input(b"x\n").unwrap();
    drop(att);
    host.wait_info(&id, "客户端断开", |i| i.is_some_and(|i| !i.attached));
    wait_event(
        &mut events,
        "断开期间的输出",
        |e| matches!(e, HostEvent::Title { title, .. } if title == "phase2"),
    );

    let mut att = host.attach(&id, COLS, ROWS);
    let mut back = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut back);

    let mut reference = Screen::new(COLS, ROWS);
    reference.feed(&part_a);
    reference.feed(part_b);
    assert_eq!(back.screen.signature(), reference.signature());
    assert_eq!(back.screen.history_text(), reference.history_text());
    assert!(
        back.screen
            .visible_text()
            .iter()
            .any(|l| l == "after-detach under")
    );
    assert_eq!(back.screen.history_text()[0], "green 中文 plain");
    assert!(back.screen.bracketed_paste());
    assert!(contains(
        &back.snapshot,
        "\x1b]2;demo · Claude · phase2\x07".as_bytes()
    ));
    assert!(host.info(&id).unwrap().attached);
}

#[test]
fn kitty_keyboard_mode_is_replayed_on_reattach() {
    let host = TestHost::start();
    let id = host.spawn(
        "kitty",
        &format!("{RAW} printf '\\033[>1u\\033[?1004hK'; sleep 60"),
    );
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    assert!(!contains(&seen.snapshot, b"\x1b[>"), "刚启动不该有键盘协议");
    read_until(&mut att, &mut seen, "K", |s| s.live_text().ends_with('K'));
    assert_eq!(seen.screen.kitty_keyboard_flags(), 1);
    drop(att);
    host.wait_info(&id, "客户端断开", |i| i.is_some_and(|i| !i.attached));

    let mut att = host.attach(&id, COLS, ROWS);
    let mut back = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut back);
    assert!(contains(&back.snapshot, b"\x1b[>1u"));
    assert!(contains(&back.snapshot, b"\x1b[?1004h"));
    assert_eq!(back.screen.kitty_keyboard_flags(), 1);
    assert_eq!(back.screen.signature(), seen.screen.signature());
}

#[test]
fn only_title_sequences_are_rewritten() {
    let host = TestHost::start();
    let raw = b"a\x1b]0;hello\x07b\x1b]8;;http://x\x07L\x1b]8;;\x07\x1b]52;c;aGk=\x07\x1b]2;two\x1b\\\x1b[31m\xe4\xb8\xad\x1b[0mEND";
    host.file("t.bin", raw);
    let id = host.spawn("title", &format!("{RAW} cat t.bin; sleep 60"));
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    // CLI 还没设标题时，attach 先设一个只有前缀的标题。
    assert!(
        seen.snapshot
            .ends_with("\x1b]2;demo · Claude\x07".as_bytes())
    );
    read_until(&mut att, &mut seen, "END", |s| {
        s.live_text().ends_with("END")
    });
    let want = "a\x1b]0;demo · Claude · hello\x07b\x1b]8;;http://x\x07L\x1b]8;;\x07\x1b]52;c;aGk=\x07\x1b]2;demo · Claude · two\x1b\\\x1b[31m中\x1b[0mEND";
    assert_eq!(seen.live_text(), want);
    assert_eq!(host.info(&id).unwrap().title.as_deref(), Some("two"));
}

#[test]
fn bell_and_notifications_become_events_and_pass_through() {
    let host = TestHost::start();
    let raw = b"\x07\x1b]9;\xe5\xae\x8c\xe6\x88\x90\x07\x1b]777;notify;Codex;wait\x1b\\\x1b]9;4;1;50\x07END";
    host.file("n.bin", raw);
    let id = host.spawn("bell", &format!("{RAW} cat n.bin; sleep 60"));
    let mut events = host.subscribe();
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    read_until(&mut att, &mut seen, "END", |s| s.live.ends_with(b"END"));
    assert_eq!(seen.live, raw);

    let got = wait_event(&mut events, "第二条通知", |e| {
        matches!(e, HostEvent::Notify { title: Some(_), .. })
    });
    let want = [
        HostEvent::Bell { id: id.clone() },
        HostEvent::Notify {
            id: id.clone(),
            title: None,
            body: "完成".to_owned(),
        },
        HostEvent::Notify {
            id,
            title: Some("Codex".to_owned()),
            body: "wait".to_owned(),
        },
    ];
    let signals: Vec<_> = got
        .into_iter()
        .filter(|e| matches!(e, HostEvent::Bell { .. } | HostEvent::Notify { .. }))
        .collect();
    assert_eq!(signals, want);
}

/// 会话台那边的状态灯：读到 `pred` 成立的事件为止，用会话台同一个映射（`ui::activity`）喂 `Notifier`。
fn feed_lamps(
    events: &mut EventStream,
    lamps: &mut Notifier,
    what: &str,
    pred: impl Fn(&HostEvent) -> bool,
) -> Vec<HostEvent> {
    let got = wait_event(events, what, pred);
    for e in got.clone() {
        if let Some((id, a)) = huitai::ui::activity(e) {
            lamps.notify(id, a, std::time::Instant::now());
        }
    }
    got
}

/// 「等你」：刚做完（没看过）或等你回复，顶栏都算等你。
fn waiting(state: SessionState) -> bool {
    matches!(state, SessionState::Done | SessionState::Waiting)
}

#[test]
fn program_status_osc7501_turns_the_lamp_to_waiting_and_passes_through() {
    let host = TestHost::start();
    let raw = b"\x1b]7501;state=working:app=hermes\x1b\\W\x1b]7501;state=blocked:kind=permission:app=hermes\x07B\x1b]7501;state=done\x1b\\D\x1b]7501;state=error:app=opencode\x07\x1b]7501;state=idle\x07END";
    host.file("s.bin", raw);
    let id = host.spawn("status", &format!("{RAW} cat s.bin; sleep 60"));
    let mut events = host.subscribe();
    let mut lamps = Notifier::new();
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    read_until(&mut att, &mut seen, "END", |s| s.live.ends_with(b"END"));
    assert_eq!(seen.live, raw, "7501 原样转给终端");

    let bells = std::cell::Cell::new(0);
    let got = feed_lamps(&mut events, &mut lamps, "三次要你回来", |e| {
        if matches!(e, HostEvent::Bell { .. }) {
            bells.set(bells.get() + 1);
        }
        bells.get() == 3
    });
    assert!(
        got.iter().all(|e| !matches!(e, HostEvent::Notify { .. })),
        "7501 不是桌面通知：{got:?}"
    );
    let state = lamps.state(&id);
    assert!(
        waiting(state),
        "发了 blocked/done/error 该是等你，实际 {state:?}"
    );
}

#[test]
fn program_status_working_after_waiting_turns_the_lamp_back_to_running() {
    let host = TestHost::start();
    host.file("b.bin", b"\x1b]7501;state=blocked:app=hermes\x1b\\ask");
    host.file("w.bin", b"\x1b]7501;state=working:app=hermes\x1b\\");
    // 两段之间隔过「提醒后的收尾重画」窗口（2 秒），之后只发 working、没有别的输出。
    let id = host.spawn(
        "working",
        &format!("{RAW} cat b.bin; sleep 3; cat w.bin; sleep 60"),
    );
    let mut events = host.subscribe();
    let mut lamps = Notifier::new();
    let _att = host.attach(&id, COLS, ROWS);
    feed_lamps(&mut events, &mut lamps, "blocked", |e| {
        matches!(e, HostEvent::Bell { .. })
    });
    assert!(waiting(lamps.state(&id)), "{:?}", lamps.state(&id));
    feed_lamps(&mut events, &mut lamps, "working 的有输出", |e| {
        matches!(e, HostEvent::Output { .. })
    });
    assert_eq!(lamps.state(&id), SessionState::Running);
}

#[test]
fn gemini_action_required_title_turns_the_lamp_to_waiting() {
    let host = TestHost::start();
    let raw = "\x1b]0;✦  Working… (demo)\x07\x1b]0;✋  Action Required (demo)\x07END";
    host.file("g.bin", raw.as_bytes());
    let id = host.spawn("gemini", &format!("{RAW} cat g.bin; sleep 60"));
    let mut events = host.subscribe();
    let mut lamps = Notifier::new();
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    read_until(&mut att, &mut seen, "END", |s| {
        s.live_text().ends_with("END")
    });
    // 标题改写规则不变：只加前缀。
    assert_eq!(
        seen.live_text(),
        "\x1b]0;demo · Claude · ✦  Working… (demo)\x07\x1b]0;demo · Claude · ✋  Action Required (demo)\x07END"
    );
    let got = feed_lamps(&mut events, &mut lamps, "等待标题", |e| {
        matches!(e, HostEvent::Bell { .. })
    });
    assert!(
        got.iter().any(
            |e| matches!(e, HostEvent::Title { title, .. } if title.contains("Action Required"))
        ),
        "{got:?}"
    );
    assert!(waiting(lamps.state(&id)), "{:?}", lamps.state(&id));
}

#[test]
fn input_bytes_pass_through_unchanged() {
    let host = TestHost::start();
    let id = host.spawn("echo", &format!("{RAW} printf R; cat"));
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    read_until(&mut att, &mut seen, "R", |s| s.live == b"R");
    // Shift+Enter（kitty CSI u）、括号粘贴的中文多行、控制字符。
    let input = "\x1b[13;2u\x1b[200~粘贴\r\n第二行\x1b[201~\x03\x1a\x7f".as_bytes();
    att.writer().input(input).unwrap();
    let mut want = b"R".to_vec();
    want.extend_from_slice(input);
    read_until(&mut att, &mut seen, "回显", |s| {
        s.live.len() >= want.len()
    });
    assert_eq!(seen.live, want);
}

#[test]
fn window_size_changes_reach_the_cli() {
    let host = TestHost::start();
    let id = host.spawn(
        "winch",
        &format!("{RAW} trap 'stty size' WINCH; printf R; while :; do sleep 0.05; done"),
    );
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    read_until(&mut att, &mut seen, "R", |s| s.live == b"R");
    att.writer().resize(100, 40).unwrap();
    read_until(&mut att, &mut seen, "新尺寸", |s| {
        s.live_text().contains("40 100")
    });
}

#[test]
fn second_attach_takes_over_and_first_is_told() {
    let host = TestHost::start();
    let id = host.spawn("twice", &format!("{RAW} printf hi; sleep 60"));
    let mut first = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut first, &mut seen);
    read_until(&mut first, &mut seen, "hi", |s| s.live == b"hi");

    let mut second = host.attach(&id, 80, 20);
    let mut seen2 = Seen::new(80, 20);
    read_snapshot(&mut second, &mut seen2);
    assert_eq!(seen2.screen.visible_text()[0], "hi");
    read_until(&mut first, &mut seen, "被接走", |s| s.end.is_some());
    assert_eq!(seen.end, Some(AttachMsg::End(EndReason::Elsewhere)));
    assert!(host.info(&id).unwrap().attached);
}

#[test]
fn cli_exit_is_reaped_and_reported() {
    let host = TestHost::start();
    let id = host.spawn("exit", &format!("{RAW} printf bye; exit 3"));
    let mut events = host.subscribe();
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    read_until(&mut att, &mut seen, "退出", |s| s.end.is_some());
    assert_eq!(seen.live, b"bye");
    assert_eq!(
        seen.end,
        Some(AttachMsg::End(EndReason::Exited { code: Some(3) }))
    );
    wait_event(&mut events, "Exited", |e| {
        *e == HostEvent::Exited {
            id: id.clone(),
            code: Some(3),
        }
    });
    assert!(host.info(&id).is_none(), "退出后应取消登记");
}

#[test]
fn detach_request_keeps_cli_running_and_kill_ends_it() {
    let host = TestHost::start();
    let id = host.spawn("detach", &format!("{RAW} printf up; sleep 60"));
    let mut events = host.subscribe();
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    read_until(&mut att, &mut seen, "up", |s| s.live == b"up");
    let pid = host.info(&id).unwrap().pid.unwrap();

    assert_eq!(
        host.request(&Request::Detach { id: id.clone() }),
        Response::Ok
    );
    read_until(&mut att, &mut seen, "被要求退出", |s| s.end.is_some());
    assert_eq!(seen.end, Some(AttachMsg::End(EndReason::Detached)));
    let info = host.info(&id).unwrap();
    assert!(!info.attached);
    assert_eq!(info.pid, Some(pid), "脱离后 CLI 照跑");

    assert_eq!(
        host.request(&Request::Kill { id: id.clone() }),
        Response::Ok
    );
    wait_event(
        &mut events,
        "Kill 后退出",
        |e| matches!(e, HostEvent::Exited { id: x, code: None } if *x == id),
    );
    assert!(host.info(&id).is_none());
}

#[test]
fn unknown_or_invalid_sessions_are_errors() {
    let host = TestHost::start();
    let missing = SessionId::from("nope");
    assert!(huitai::host::attach(&host.dir, &missing, fake_env(&host.dir), 80, 24).is_err());
    assert!(matches!(
        host.request(&Request::Kill { id: missing }),
        Response::Error(_)
    ));
    let spec = huitai::host::SpawnSpec {
        id: SessionId::from("bad"),
        vendor: huitai::core::Vendor::Codex,
        project: "x".to_owned(),
        cwd: "/definitely/not/here".into(),
        command: "true".to_owned(),
    };
    assert!(matches!(
        host.request(&Request::Spawn(spec)),
        Response::Error(_)
    ));
}

#[test]
fn data_dir_is_private_and_single_instance() {
    let host = TestHost::start();
    let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&host.dir), 0o700);
    assert_eq!(mode(&socket_path(&host.dir)), 0o600);
    let err = Daemon::bind(&host.dir)
        .err()
        .expect("第二个守护进程不该起来");
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    // 第一个还在正常服务。
    assert!(matches!(
        host.request(&Request::Ping),
        Response::Pong { .. }
    ));
    thread::sleep(Duration::from_millis(10));
}
