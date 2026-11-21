//! 升级不打断：客户端和守护进程版本不同的各种组合。
//!
//! 「另一个版本的构建」用真的 `ai daemon` 加环境变量伪造（`HUITAI_FAKE_VERSION` 报更老的版本号，
//! `HUITAI_FAKE_PROTOCOL=0` 装成没有握手的旧版）；测试进程自己是当前版本的客户端。

mod host_support;

use std::fs;
use std::io::{ErrorKind, Read};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use host_support::{COLS, Home, ROWS, Seen, TestHost, WAIT, fake_env, read_snapshot, read_until};
use huitai::core::{SessionId, Vendor};
use huitai::host::protocol::{
    self, KIND_REQUEST, KIND_RESPONSE, MIN_CLIENT_PROTOCOL, OLD_CLIENT, PROTOCOL,
};
use huitai::host::version::{FAKE_PROTOCOL, FAKE_VERSION};
use huitai::host::{self, AttachMsg, Build, DaemonInfo, EndReason, Request, Response, SpawnSpec};

const OLD_VERSION: &str = "0.0.1";

/// 用 `env` 伪造版本拉起守护进程，记下它的进程号（drop 时结束它）。
fn launch(home: &mut Home, env: &[(&str, &str)]) -> DaemonInfo {
    let mut cmd = home.ai();
    cmd.envs(env.iter().copied()).arg("daemon");
    let info = host::ensure_daemon(&home.data, cmd).expect("拉起守护进程失败");
    home.daemon_pid = Some(info.pid);
    info
}

/// 当前版本的客户端照常要守护进程（`ai` 打开对话时就是这样）。
fn ensure_current(home: &mut Home) -> DaemonInfo {
    launch(home, &[])
}

/// 登记一条假 CLI：读一行回一行，读到 bye 就退出。
fn spawn_echo(home: &Home, id: &str) -> SessionId {
    let id = SessionId::from(id);
    let spec = SpawnSpec {
        id: id.clone(),
        vendor: Vendor::Claude,
        project: "proj".to_owned(),
        cwd: home.home.clone(),
        command: r#"while read l; do echo "got:$l"; [ "$l" = bye ] && exit 0; done"#.to_owned(),
    };
    assert_eq!(
        host::request(&home.data, &Request::Spawn(spec)).unwrap(),
        Response::Ok
    );
    id
}

fn attach(home: &Home, id: &SessionId) -> (host::Attachment, Seen) {
    let mut att =
        host::attach(&home.data, id, fake_env(&home.home), COLS, ROWS).expect("attach 失败");
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    (att, seen)
}

/// 发一行，等它回出来。
fn echo(att: &mut host::Attachment, seen: &mut Seen, line: &str) {
    att.writer().input(format!("{line}\r").as_bytes()).unwrap();
    let want = format!("got:{line}");
    read_until(att, seen, &want, |s| s.live_text().contains(&want));
}

/// 等到守护进程不再接连接（退出了）。
fn wait_gone(home: &Home) {
    let deadline = Instant::now() + WAIT;
    while UnixStream::connect(host::socket_path(&home.data)).is_ok() {
        assert!(Instant::now() < deadline, "守护进程一直没退出");
        thread::sleep(Duration::from_millis(20));
    }
}

fn is_current(info: &DaemonInfo) -> bool {
    info.build
        .as_ref()
        .is_some_and(|b| b.version == env!("CARGO_PKG_VERSION") && b.protocol == PROTOCOL)
        && !info.outdated()
}

#[test]
fn idle_old_daemon_is_replaced_by_current_version() {
    let mut home = Home::new("up-idle");
    let old = launch(&mut home, &[(FAKE_VERSION, OLD_VERSION)]);
    assert_eq!(old.build.as_ref().unwrap().version, OLD_VERSION);
    assert!(old.replaceable(), "{old:?}");

    let report = doctor(&home);
    assert!(
        report.contains(&format!("状态  {OLD_VERSION}")) && report.contains("比当前程序旧"),
        "{report}"
    );

    let new = ensure_current(&mut home);
    assert_ne!(new.pid, old.pid);
    assert!(is_current(&new), "{new:?}");
    // 换好之后不再换。
    assert_eq!(ensure_current(&mut home).pid, new.pid);
    // 守护进程和 doctor 是同一个程序文件。
    let report = doctor(&home);
    assert!(report.contains("和当前程序一致"), "{report}");
}

/// `ai doctor` 的输出。
fn doctor(home: &Home) -> String {
    let out = home.ai().arg("doctor").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn old_daemon_keeps_its_sessions_and_steps_aside_after_the_last_one() {
    let mut home = Home::new("up-busy");
    let old = launch(&mut home, &[(FAKE_VERSION, OLD_VERSION)]);
    let id = spawn_echo(&home, "s1");
    let (mut att, mut seen) = attach(&home, &id);
    echo(&mut att, &mut seen, "before");

    // 新版客户端来了：托管着对话的旧版接着用，不换。
    let info = ensure_current(&mut home);
    assert_eq!(info.pid, old.pid);
    assert!(info.outdated() && info.hosted == 1, "{info:?}");
    // 双方都支持的操作照常：查列表、登记一条新的再结束它。
    let Response::List(list) = host::request(&home.data, &Request::List).unwrap() else {
        panic!("List 应答不对");
    };
    assert_eq!(list.len(), 1);
    let extra = spawn_echo(&home, "s2");
    assert_eq!(
        host::request(&home.data, &Request::Kill { id: extra }).unwrap(),
        Response::Ok
    );
    // 对话没被打断。
    echo(&mut att, &mut seen, "during");
    assert!(seen.end.is_none());

    // 最后一条结束：旧版自己退出，下次拉起的是新版。
    att.writer().input(b"bye\r").unwrap();
    read_until(&mut att, &mut seen, "CLI 退出", |s| s.end.is_some());
    assert_eq!(
        seen.end,
        Some(AttachMsg::End(EndReason::Exited { code: Some(0) }))
    );
    wait_gone(&home);
    let new = ensure_current(&mut home);
    assert_ne!(new.pid, old.pid);
    assert!(is_current(&new), "{new:?}");
}

/// 没有握手的旧版（升级前装的那种）：读不懂 Hello，客户端认出来后按旧协议说话。
#[test]
fn daemon_without_handshake_is_used_as_is_then_replaced_when_empty() {
    let mut home = Home::new("up-legacy");
    let legacy = launch(
        &mut home,
        &[(FAKE_PROTOCOL, "0"), (FAKE_VERSION, OLD_VERSION)],
    );
    assert_eq!(legacy.build, None);
    let id = spawn_echo(&home, "s1");
    let (mut att, mut seen) = attach(&home, &id);

    let info = ensure_current(&mut home);
    assert_eq!(info.pid, legacy.pid);
    assert!(info.outdated() && info.hosted == 1, "{info:?}");
    echo(&mut att, &mut seen, "during");

    // 旧版不会自己退出；空了之后新版客户端把它换掉。
    att.writer().input(b"bye\r").unwrap();
    read_until(&mut att, &mut seen, "CLI 退出", |s| s.end.is_some());
    let deadline = Instant::now() + WAIT;
    while host::probe(&home.data).unwrap().hosted > 0 {
        assert!(Instant::now() < deadline, "对话一直没取消登记");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(host::probe(&home.data).unwrap().pid, legacy.pid);
    let new = ensure_current(&mut home);
    assert_ne!(new.pid, legacy.pid);
    assert!(is_current(&new), "{new:?}");
}

/// 没有握手的旧客户端连到新守护进程：每种请求都回一句它能显示的「请重新打开 ai」，守护进程照常服务新客户端。
#[test]
fn current_daemon_tells_old_clients_to_reopen() {
    let host = TestHost::start();
    let old_requests = [
        Request::Ping,
        Request::List,
        Request::Attach {
            id: SessionId::from("x"),
            env: Vec::new(),
            cols: 80,
            rows: 24,
        },
        Request::Board(Default::default()),
        Request::Events,
    ];
    for req in old_requests {
        let mut s = UnixStream::connect(host::socket_path(&host.dir)).unwrap();
        s.set_read_timeout(Some(WAIT)).unwrap();
        protocol::write_json(&mut s, KIND_REQUEST, &req).unwrap();
        let reply: Response = protocol::read_json(&mut s, KIND_RESPONSE).unwrap();
        assert_eq!(reply, Response::Error(OLD_CLIENT.to_owned()), "{req:?}");
        assert!(OLD_CLIENT.contains("请重新打开 ai"));
        // 回完就关，旧客户端不会卡着等。
        assert_eq!(s.read(&mut [0u8; 1]).unwrap(), 0);
    }

    // 握手了但协议太老：握手应答里说明原因，不接着处理。
    let mut s = UnixStream::connect(host::socket_path(&host.dir)).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    let ancient = Build {
        protocol: MIN_CLIENT_PROTOCOL - 1,
        version: OLD_VERSION.to_owned(),
        ..Build::default()
    };
    protocol::write_json(&mut s, KIND_REQUEST, &Request::Hello(ancient)).unwrap();
    let Response::Hello(hello) = protocol::read_json(&mut s, KIND_RESPONSE).unwrap() else {
        panic!("应答不是 Hello");
    };
    assert_eq!(hello.refused.as_deref(), Some(OLD_CLIENT));

    // 更新的客户端发来它不认识的请求：回错误，不崩。
    let mut s = UnixStream::connect(host::socket_path(&host.dir)).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    let me = host::version::current().clone();
    protocol::write_json(&mut s, KIND_REQUEST, &Request::Hello(me)).unwrap();
    let _: Response = protocol::read_json(&mut s, KIND_RESPONSE).unwrap();
    protocol::write_frame(&mut s, KIND_REQUEST, br#"{"teleport":{"to":"mars"}}"#).unwrap();
    let reply: Response = protocol::read_json(&mut s, KIND_RESPONSE).unwrap();
    assert!(matches!(reply, Response::Error(_)), "{reply:?}");

    assert_eq!(host.list(), Vec::new());
}

/// 中断恢复记录跨版本：旧版守护进程托管着对话时没收尾就没了，新版认出被中断的对话；
/// 记录里有当前版本不认识的字段也照读。
#[test]
fn interrupted_records_survive_a_version_change() {
    let mut home = Home::new("up-rec");
    launch(&mut home, &[(FAKE_VERSION, OLD_VERSION)]);
    let id = spawn_echo(&home, "s1");
    let (mut att, mut seen) = attach(&home, &id);
    echo(&mut att, &mut seen, "hi");
    home.kill_daemon_hard();

    // 更新的版本写的记录（多一个字段）也在里面。
    let running = host::interrupt::running_path(&home.data);
    let mut list: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&running).unwrap()).unwrap();
    list.push(serde_json::json!({
        "id": "future", "vendor": "pi", "project": "proj",
        "cwd": home.home, "started_at": 1, "resume_hint": {"turn": 3}
    }));
    fs::write(&running, serde_json::to_vec(&list).unwrap()).unwrap();

    let new = ensure_current(&mut home);
    assert!(is_current(&new), "{new:?}");
    let Response::Interrupted(got) = host::request(&home.data, &Request::Interrupted).unwrap()
    else {
        panic!("Interrupted 应答不对");
    };
    let ids: Vec<_> = got.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["future", "s1"]);
    assert_eq!(got[1].vendor, Vendor::Claude);
}

/// 旧版守护进程不认的请求：客户端给出能看懂的原因，而不是「连接断了」。
#[test]
fn request_unknown_to_daemon_without_handshake_says_why() {
    let mut home = Home::new("up-unknown");
    launch(&mut home, &[(FAKE_PROTOCOL, "0")]);
    // 旧版没有 Retire 这个请求，读不懂就关连接。
    let err = host::request(&home.data, &Request::Retire).unwrap_err();
    assert_ne!(err.kind(), ErrorKind::UnexpectedEof, "{err}");
    assert!(err.to_string().contains("不认这个操作"), "{err}");
    // 守护进程还在，旧版本来就有的请求照常。
    assert!(matches!(
        host::request(&home.data, &Request::Ping),
        Ok(Response::Pong { .. })
    ));
}
