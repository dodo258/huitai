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
    assert!(text.contains("\x1b]2;π proj\x07"), "{text:?}");
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
