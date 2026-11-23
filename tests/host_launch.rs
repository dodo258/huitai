//! 用真的 `ai` 二进制：客户端 setsid 拉起守护进程，`ai attach` 接上对话并带回退出码。

mod host_support;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;

use host_support::{AI, Home, fake_env};
use huitai::core::{SessionId, Vendor};
use huitai::host::{self, Request, Response, SpawnSpec};

fn mode(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn client_launches_detached_daemon_once() {
    let mut home = Home::new("launch");
    let pid = home.ensure_daemon();
    // setsid 之后守护进程自己是会话首进程，不挂在任何终端上。
    // SAFETY: getsid 只读进程信息。
    assert_eq!(
        unsafe { libc::getsid(pid as libc::pid_t) },
        pid as libc::pid_t
    );
    assert_ne!(pid, std::process::id());
    assert_eq!(mode(&home.data), 0o700);
    assert_eq!(mode(&host::socket_path(&home.data)), 0o600);
    // 再要一次不会起第二个。
    assert_eq!(home.ensure_daemon(), pid);
}

#[test]
fn ai_attach_forwards_output_and_returns_cli_exit_code() {
    let mut home = Home::new("attach");
    home.ensure_daemon();
    let spec = SpawnSpec {
        id: SessionId::from("cli-1"),
        vendor: Vendor::Pi,
        project: "proj".to_owned(),
        cwd: home.home.clone(),
        command: "read line; printf 'got:%s' \"$line\"; exit 5".to_owned(),
    };
    assert_eq!(
        host::request(&home.data, &Request::Spawn(spec)).unwrap(),
        Response::Ok
    );

    let mut child = home
        .ai()
        .args(["attach", "cli-1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"hello\r").unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(5), "输出：{text:?}");
    assert!(text.contains("got:hello"), "{text:?}");
    // 前面可能已经带上守护进程算的状态（「●在跑 」），看 attach 是在它算出来之前还是之后。
    assert!(text.contains("proj · pi\x07"), "{text:?}");
}

/// `ai attach` 跑在真终端（PTY）里：外层窗口改大小时 CLI 要收到新尺寸。
/// 回归：macOS 会丢掉默认动作是忽略的 SIGWINCH，没装处理函数时 sigwait 永远等不到。
#[test]
fn ai_attach_in_a_real_terminal_forwards_window_size() {
    use std::io::Read;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let mut home = Home::new("tty");
    home.ensure_daemon();
    let spec = SpawnSpec {
        id: SessionId::from("tty-1"),
        vendor: Vendor::Codex,
        project: "proj".to_owned(),
        cwd: home.home.clone(),
        command: "trap 'stty size' WINCH; printf ready; while :; do sleep 0.05; done".to_owned(),
    };
    assert_eq!(
        host::request(&home.data, &Request::Spawn(spec)).unwrap(),
        Response::Ok
    );

    let env = fake_env(&home.home);
    let mut outer = host::pty::spawn(
        &format!("exec '{AI}' attach tty-1"),
        &home.home,
        &env,
        90,
        30,
    )
    .expect("开外层终端失败");
    let mut reader = outer.master.try_clone().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                return;
            }
        }
    });
    let mut seen = Vec::new();
    let mut wait_for = |needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(&seen).contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(chunk) => seen.extend_from_slice(&chunk),
                Err(_) => panic!(
                    "等不到 {needle:?}；已收到 {:?}",
                    String::from_utf8_lossy(&seen)
                ),
            }
        }
    };
    wait_for("ready");
    host::pty::resize(&outer.master, 120, 40).unwrap();
    wait_for("40 120");

    assert_eq!(
        host::request(
            &home.data,
            &Request::Kill {
                id: SessionId::from("tty-1")
            }
        )
        .unwrap(),
        Response::Ok
    );
    assert_eq!(
        outer.child.wait().unwrap().code(),
        Some(1),
        "CLI 被信号结束时 attach 退出码为 1"
    );
}

/// 守护进程不留着继承来的描述符：macOS 上 Rust 建管道和标「执行时关闭」是两步，别的线程刚好在中间 fork
/// 时，管道写端会漏进新拉起的守护进程。守护进程常驻，漏进去的写端就永远不关，等这根管道的那一方
/// （正在 spawn 的线程、读 `ps` 输出的线程）会一直卡住。这里故意让守护进程继承一根管道的写端，
/// 父进程关掉自己那份后应该马上读到结束。
#[test]
fn daemon_drops_inherited_descriptors() {
    let mut home = Home::new("fds");
    let mut fds = [0; 2];
    // SAFETY: fds 是两个 int 的数组；pipe 不带 CLOEXEC，子进程会继承两端。
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (read_end, write_end) = (fds[0], fds[1]);
    home.ensure_daemon();
    // SAFETY: 关自己这份写端；读端设成非阻塞，下面用 poll 等。
    unsafe {
        libc::close(write_end);
        libc::fcntl(read_end, libc::F_SETFL, libc::O_NONBLOCK);
    }
    let mut pfd = libc::pollfd {
        fd: read_end,
        events: libc::POLLIN,
        revents: 0,
    };
    // 别的测试线程同时起的短命子进程也可能暂时带着写端，给 5 秒。
    // SAFETY: pfd 有效。
    let ready = unsafe { libc::poll(&mut pfd, 1, 5000) };
    let mut buf = [0u8; 1];
    // SAFETY: buf 有效。
    let n = unsafe { libc::read(read_end, buf.as_mut_ptr().cast(), 1) };
    unsafe { libc::close(read_end) };
    assert!(
        ready == 1 && n == 0,
        "守护进程还拿着继承来的管道写端（poll={ready}, read={n}）"
    );
}

/// 起一条假 CLI（把收到的字节写进 `in.bin`）并用真的 `ai attach` 接上；`config` 是 config.json 的内容。
fn attach_recording(name: &str, config: Option<&str>) -> (Home, SessionId, std::process::Child) {
    use std::io::Read;
    use std::sync::mpsc;
    use std::time::Duration;

    let mut home = Home::new(name);
    home.ensure_daemon();
    if let Some(config) = config {
        fs::write(home.data.join("config.json"), config).unwrap();
    }
    let id = SessionId::from(format!("{name}-1"));
    let spec = SpawnSpec {
        id: id.clone(),
        vendor: Vendor::Claude,
        project: "proj".to_owned(),
        cwd: home.home.clone(),
        command: "stty raw -echo; printf READY; exec cat > in.bin".to_owned(),
    };
    assert_eq!(
        host::request(&home.data, &Request::Spawn(spec)).unwrap(),
        Response::Ok
    );

    let mut child = home
        .ai()
        .args(["attach", id.as_str()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = out.read(&mut buf) {
            if n == 0 {
                return;
            }
            seen.extend_from_slice(&buf[..n]);
            if String::from_utf8_lossy(&seen).contains("READY") {
                let _ = tx.send(());
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("等不到假 CLI 起来");
    (home, id, child)
}

/// 等假 CLI 收满 `want` 那么多字节（最多 10 秒），返回收到的。
fn received(home: &Home, want: usize) -> Vec<u8> {
    use std::time::{Duration, Instant};
    let file = home.home.join("in.bin");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = fs::read(&file).unwrap_or_default();
        if got.len() >= want || Instant::now() >= deadline {
            return got;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 按了脱离键：`delivered` 是它前面该到 CLI 的，`key` 和后面的 `zz` 不该到。attach 以 0 退出，
/// CLI 留在守护进程里接着跑（还在 List 里、有进程号、没人接着）。
fn assert_detaches(name: &str, config: Option<&str>, delivered: &[u8], key: &[u8]) {
    use std::time::{Duration, Instant};

    let (home, id, mut child) = attach_recording(name, config);
    let mut input = delivered.to_vec();
    input.extend_from_slice(key);
    input.extend_from_slice(b"zz");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(&input).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = host::request(&home.data, &Request::Kill { id: id.clone() });
            panic!("按了脱离键 {key:?} 10 秒 attach 还没结束");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(stdin);
    assert_eq!(status.code(), Some(0), "按脱离键脱开时 attach 退出码为 0");

    let got = received(&home, delivered.len());
    assert_eq!(
        got,
        delivered,
        "CLI 收到 {:?}",
        String::from_utf8_lossy(&got)
    );

    let Response::List(list) = host::request(&home.data, &Request::List).unwrap() else {
        panic!("List 应答不对");
    };
    let info = list
        .iter()
        .find(|h| h.id == id)
        .expect("脱开后对话应还在托管");
    assert!(info.pid.is_some(), "CLI 应还在跑：{info:?}");
    assert!(!info.attached, "脱开后不该还有人接着：{info:?}");
    // 再等一会儿：脱离键后面的字也不会迟到。
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fs::read(home.home.join("in.bin")).unwrap(), delivered);

    assert_eq!(
        host::request(&home.data, &Request::Kill { id }).unwrap(),
        Response::Ok
    );
}

/// `ai attach` 里按默认的脱离键 ⌃\（没有 config.json）：前面打的字照常到 CLI，⌃\ 本身和后面的不到；
/// 括号粘贴里的 0x1c 是粘贴内容照转，旧键 ⌃] 不再脱开、照转给 CLI。
#[test]
fn ai_attach_detach_key_leaves_the_cli_running() {
    assert_detaches("detach", None, b"ab\x1d\x1b[200~p\x1cq\x1b[201~", b"\x1c");
}

/// 设置里把回到会话台的键换成 ⌃Q 或 ⌃]：按它脱开，默认的 ⌃\ 照转给 CLI。
#[test]
fn ai_attach_detach_key_follows_the_setting() {
    assert_detaches(
        "detach-q",
        Some(r#"{"back_key": "ctrl_q"}"#),
        b"ab\x1c\x1d",
        b"\x11",
    );
    assert_detaches(
        "detach-br",
        Some(r#"{"back_key": "ctrl_bracket"}"#),
        b"ab\x1c\x11",
        b"\x1d",
    );
}

/// 设置里关掉回到会话台的键：⌃\、⌃Q、⌃] 都照转给 CLI，attach 一直接着，直到对话结束。
#[test]
fn ai_attach_detach_key_off_forwards_both_keys() {
    use std::time::{Duration, Instant};

    let (home, id, mut child) = attach_recording("detach-off", Some(r#"{"back_key": "off"}"#));
    let sent = b"a\x1cb\x11c\x1dd";
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(sent).unwrap();
    let got = received(&home, sent.len());
    assert_eq!(got, sent, "CLI 收到 {:?}", String::from_utf8_lossy(&got));
    assert!(child.try_wait().unwrap().is_none(), "关掉后不该脱开");

    assert_eq!(
        host::request(&home.data, &Request::Kill { id }).unwrap(),
        Response::Ok
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "对话结束 10 秒 attach 还没退出");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(stdin);
}
