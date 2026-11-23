//! 标签页标题的状态前缀（方案 v2「6. 更醒目」）：状态变了托管层重发一次标题「◆等你 项目 · 厂商 · CLI 标题」，
//! 只插在输出停在完整的地方，CLI 的其余字节一个不改。

mod host_support;

use std::time::Duration;

use host_support::*;
use huitai::core::{SessionId, SessionState, Vendor};
use huitai::host::scan::Scanner;
use huitai::host::{self, AttachMsg, Request, Response, SpawnSpec};

const RAW: &str = "stty raw -echo;";

/// 从 `live` 里拿掉每一处 `seq`，返回剩下的和各处的位置（在剩下的字节里算）。
fn take_out(live: &[u8], seq: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let (mut rest, mut at) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < live.len() {
        if live[i..].starts_with(seq) {
            at.push(rest.len());
            i += seq.len();
        } else {
            rest.push(live[i]);
            i += 1;
        }
    }
    (rest, at)
}

#[test]
fn state_change_resends_the_title_only_where_the_output_is_whole() {
    let host = TestHost::start();
    // 第一段停在 CSI 中间，第二段停在一个汉字的中间，第三段收尾。
    host.file("p1.bin", b"A\x1b]2;t1\x07B\x1b[3");
    host.file("p2.bin", b"1m\xe4\xb8");
    host.file("p3.bin", b"\xad\x1b[0mEND");
    let id = host.spawn(
        "title",
        &format!("{RAW} cat p1.bin; read x; cat p2.bin; read y; cat p3.bin; sleep 60"),
    );
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    // 还没有状态：只有「项目 · 厂商」。
    assert!(
        seen.snapshot
            .ends_with("\x1b]2;demo · Claude\x07".as_bytes())
    );
    read_until(&mut att, &mut seen, "第一段", |s| {
        s.live.ends_with(b"B\x1b[3")
    });

    // 输出停在 CSI 中间时状态变了：先不插。
    host.handle.set_status(&id, SessionState::Waiting);
    att.writer().input(b"x\n").unwrap();
    read_until(&mut att, &mut seen, "第二段", |s| {
        s.live.ends_with(b"1m\xe4\xb8")
    });
    att.writer().input(b"y\n").unwrap();
    let waiting = "\x1b]2;◆等你 demo · Claude · t1\x07".as_bytes();
    read_until(&mut att, &mut seen, "补发的标题", |s| {
        contains(&s.live, waiting)
    });
    read_until(&mut att, &mut seen, "END", |s| contains(&s.live, b"END"));

    let (rest, at) = take_out(&seen.live, waiting);
    // 除了插进来的那一条，CLI 的字节原样（它自己设的标题照旧只加前缀，那时还没有状态）。
    let want = "A\x1b]2;demo · Claude · t1\x07B\x1b[31m中\x1b[0mEND".as_bytes();
    assert_eq!(rest, want, "{:?}", String::from_utf8_lossy(&rest));
    assert_eq!(at.len(), 1, "只重发一次：{:?}", seen.live_text());
    let whole = want.len() - b"\x1b[0mEND".len();
    assert!(
        at[0] >= whole,
        "标题插在了序列或字符中间（第 {} 字节）：{:?}",
        at[0],
        seen.live_text()
    );

    // 输出停在完整的地方：马上发。
    host.handle.set_status(&id, SessionState::Running);
    let running = "\x1b]2;●在跑 demo · Claude · t1\x07".as_bytes();
    read_until(&mut att, &mut seen, "马上发的标题", |s| {
        s.live.ends_with(running)
    });
    // 同一个状态再说一遍不重发。
    host.handle.set_status(&id, SessionState::Running);
    match att.recv(Some(Duration::from_millis(400))) {
        Err(_) => {}
        Ok(msg) => panic!("状态没变不该再发：{msg:?}"),
    }

    // 重新接上：补发画面里的标题带着此刻的状态。
    drop(att);
    host.wait_info(&id, "客户端断开", |i| i.is_some_and(|i| !i.attached));
    host.handle.set_status(&id, SessionState::Done);
    let mut att = host.attach(&id, COLS, ROWS);
    let mut back = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut back);
    assert!(
        back.snapshot
            .ends_with("\x1b]2;✓做完 demo · Claude · t1\x07".as_bytes()),
        "{:?}",
        String::from_utf8_lossy(&back.snapshot)
    );
}

/// tmux 透传 `ESC P tmux; …内层 ESC 写成两个… ESC \`：内层 OSC 的 BEL、内层的 ST（`ESC ESC \`）都不结束外层，
/// 每个切点都不算完整的地方，直到外层 ST；里面的标题和响铃也不认（审计 v0.1.4 第 8 条）。
#[test]
fn tmux_passthrough_is_not_a_boundary_until_the_outer_st() {
    let head = b"A\x1bPtmux;";
    let body = b"\x1b\x1b]9;hi\x07\x1b\x1b]2;t\x1b\x1b\\\x07x";
    let input = [&head[..], &body[..], b"\x1b\\B"].concat();
    // `ESC P` 放出去之后（单个 ESC 扫描器先扣着）到外层 ST 的 `\` 之前。
    let inside = 3..=head.len() + body.len() + 1;
    for cut in 0..=input.len() {
        let mut s = Scanner::new("P ");
        let (mut out, mut sig) = (Vec::new(), Vec::new());
        s.feed(&input[..cut], &mut out, &mut sig);
        assert_eq!(s.at_boundary(), !inside.contains(&cut), "切在 {cut}");
        s.feed(&input[cut..], &mut out, &mut sig);
        assert_eq!((out, sig), (input.clone(), vec![]), "切在 {cut}");
    }
}

/// 状态变的时候输出正停在 tmux 透传里（内层 OSC 刚以 BEL 结束），下一次读取还在透传里：
/// 标题等到外层 ST 之后才补，CLI 的字节一个不改。
#[test]
fn pending_title_waits_for_the_outer_st_of_a_tmux_passthrough() {
    let host = TestHost::start();
    let parts: [&[u8]; 3] = [
        b"A\x1bPtmux;\x1b\x1b]9;hi\x07",
        b"more\x1b\x1b]2;t\x1b\x1b\\",
        b"\x1b\\BEND",
    ];
    for (i, p) in parts.iter().enumerate() {
        host.file(&format!("d{i}.bin"), p);
    }
    let id = host.spawn(
        "dcs",
        &format!("{RAW} cat d0.bin; read x; cat d1.bin; read y; cat d2.bin; sleep 60"),
    );
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    read_until(&mut att, &mut seen, "第一段", |s| {
        s.live.ends_with(parts[0])
    });

    host.handle.set_status(&id, SessionState::Waiting);
    att.writer().input(b"x\n").unwrap();
    read_until(&mut att, &mut seen, "第二段", |s| {
        s.live.ends_with(parts[1])
    });
    att.writer().input(b"y\n").unwrap();
    read_until(&mut att, &mut seen, "END", |s| contains(&s.live, b"END"));
    let waiting = "\x1b]2;◆等你 demo · Claude\x07".as_bytes();
    read_until(&mut att, &mut seen, "补发的标题", |s| {
        contains(&s.live, waiting)
    });

    let (rest, at) = take_out(&seen.live, waiting);
    assert_eq!(rest, parts.concat(), "{:?}", String::from_utf8_lossy(&rest));
    assert_eq!(at.len(), 1, "只补一次：{:?}", seen.live_text());
    let outer_st = parts[0].len() + parts[1].len() + 2;
    assert!(
        at[0] >= outer_st,
        "标题插进了透传里（第 {} 字节）：{:?}",
        at[0],
        seen.live_text()
    );
}

#[test]
fn cli_titles_after_a_state_change_carry_the_new_state() {
    let host = TestHost::start();
    host.file("t.bin", b"\x1b]0;hello\x07OK");
    let id = host.spawn(
        "later",
        &format!("{RAW} printf ready; read x; cat t.bin; sleep 60"),
    );
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    read_until(&mut att, &mut seen, "ready", |s| s.live.ends_with(b"ready"));
    host.handle.set_status(&id, SessionState::Running);
    att.writer().input(b"x\n").unwrap();
    read_until(&mut att, &mut seen, "OK", |s| s.live.ends_with(b"OK"));
    assert_eq!(
        seen.live_text(),
        "ready\x1b]2;●在跑 demo · Claude\x07\x1b]0;●在跑 demo · Claude · hello\x07OK"
    );
}

/// 真起 `ai daemon`：状态由守护进程里的 Notifier 算——启动算「在跑」，响铃且标签页不在前台算「做完」，
/// 每变一次重发一次标题。
#[test]
fn daemon_notifier_drives_the_title_prefix() {
    let mut home = Home::new("ttitle");
    home.ensure_daemon();
    let id = SessionId::from("bell-title");
    let spec = SpawnSpec {
        id: id.clone(),
        vendor: Vendor::Claude,
        project: "demo".to_owned(),
        cwd: home.home.clone(),
        command: format!("{RAW} printf ready; read x; printf 'done\\007'; sleep 30"),
    };
    assert_eq!(
        host::request(&home.data, &Request::Spawn(spec)).unwrap(),
        Response::Ok
    );
    let mut att = host::attach(&home.data, &id, fake_env(&home.home), COLS, ROWS).unwrap();
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    let running = "\x1b]2;●在跑 demo · Claude\x07".as_bytes();
    // 启动的事件可能在补发画面之前或之后算完。
    let ran = |s: &Seen| contains(&s.snapshot, running) || contains(&s.live, running);
    read_until(&mut att, &mut seen, "在跑", |s| {
        s.live.ends_with(b"ready")
    });
    read_until(&mut att, &mut seen, "在跑的标题", ran);
    att.writer().input(b"x\n").unwrap();
    let done = "\x1b]2;✓做完 demo · Claude\x07".as_bytes();
    read_until(&mut att, &mut seen, "做完的标题", |s| {
        contains(&s.live, done)
    });
    let (rest, _) = take_out(&seen.live, running);
    let (rest, at) = take_out(&rest, done);
    assert_eq!(at.len(), 1, "{:?}", seen.live_text());
    assert_eq!(
        String::from_utf8_lossy(&rest),
        "readydone\x07",
        "CLI 的字节原样"
    );
    assert!(matches!(
        att.recv(Some(Duration::from_millis(1))),
        Err(_) | Ok(AttachMsg::Output(_))
    ));
}
