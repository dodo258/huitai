//! 实测：会话台实时预览（每 0.5 秒取一次画面）会不会让空闲的守护进程 CPU 上升。
//! 起真的 `ai daemon`（HOME 指到临时目录），托管一条输出完就空闲的假 CLI，分三段各 20 秒量守护进程的 CPU 时间：
//! 不预览、按会话台的做法预览（带上次的版本号，画面没变只回「没变」）、每次都要整屏（最坏情况）。
//! 手动跑：cargo test --release --test host_preview_cpu -- --ignored --nocapture

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use huitai::core::{SessionId, Vendor};
use huitai::host::{self, Request, Response, Snapshot, SpawnSpec};

const AI: &str = env!("CARGO_BIN_EXE_ai");
const WINDOW: Duration = Duration::from_secs(20);
const EVERY: Duration = Duration::from_millis(500);

/// 进程累计 CPU 时间（用户 + 系统），`ps` 精度 0.01 秒。
fn cpu_time(pid: u32) -> Duration {
    let out = Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .expect("跑 ps");
    let text = String::from_utf8_lossy(&out.stdout);
    let secs = text.trim().split(':').fold(0.0, |acc, part| {
        acc * 60.0 + part.parse::<f64>().expect("ps 输出")
    });
    Duration::from_secs_f64(secs)
}

struct Daemon {
    child: Child,
    home: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.home);
    }
}

fn start_daemon() -> (Daemon, PathBuf) {
    let home = PathBuf::from(format!("/tmp/htcpu-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).unwrap();
    let child = Command::new(AI)
        .arg("daemon")
        .env("HOME", &home)
        // 不抢全局呼出键、不弹系统通知。
        .env("HUITAI_NO_HOTKEY", "1")
        .env("HUITAI_NOTIFY_TO", "/dev/null")
        .spawn()
        .expect("起守护进程");
    let dir = home.join("Library/Application Support/huitai");
    let deadline = Instant::now() + Duration::from_secs(5);
    while host::request(&dir, &Request::Ping).is_err() {
        assert!(Instant::now() < deadline, "守护进程没起来");
        thread::sleep(Duration::from_millis(20));
    }
    (Daemon { child, home }, dir)
}

fn env(home: &Path) -> Vec<(String, String)> {
    [
        ("SHELL", "/bin/sh"),
        ("PATH", "/usr/bin:/bin"),
        ("TERM", "xterm-256color"),
        ("LANG", "en_US.UTF-8"),
        ("HOME", home.to_str().unwrap()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

/// 在 `WINDOW` 内每 `EVERY` 调一次 `poll`，返回守护进程这段时间用的 CPU 占单核的百分比。
fn measure(pid: u32, mut poll: impl FnMut()) -> f64 {
    let before = cpu_time(pid);
    let start = Instant::now();
    while start.elapsed() < WINDOW {
        poll();
        thread::sleep(EVERY);
    }
    let used = cpu_time(pid).saturating_sub(before);
    used.as_secs_f64() / start.elapsed().as_secs_f64() * 100.0
}

#[test]
#[ignore = "实测 60 秒：cargo test --release --test host_preview_cpu -- --ignored --nocapture"]
fn idle_daemon_cpu_with_and_without_live_preview() {
    let (daemon, dir) = start_daemon();
    let pid = daemon.child.id();
    let id = SessionId::from("cpu-idle");
    let spec = SpawnSpec {
        id: id.clone(),
        vendor: Vendor::Claude,
        project: "demo".to_owned(),
        cwd: daemon.home.clone(),
        // 画一屏 40 行彩色字后空闲。
        command: "stty raw -echo; i=1; while [ $i -le 40 ]; do printf '\\033[3%dm line %d %s\\033[0m\\r\\n' $((i % 8)) $i 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; i=$((i + 1)); done; sleep 600".to_owned(),
    };
    assert_eq!(
        host::request(&dir, &Request::Spawn(spec)).unwrap(),
        Response::Ok
    );
    // attach 一次把它启动起来，然后关掉标签页（对话照跑）。
    drop(host::attach(&dir, &id, env(&daemon.home), 120, 40).unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Snapshot::Screen(s)) = host::snapshot(&dir, &id, None)
            && s.lines
                .iter()
                .any(|l| l.iter().any(|r| r.text.contains("line 40")))
        {
            break;
        }
        assert!(Instant::now() < deadline, "假 CLI 没画完");
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(Duration::from_secs(1));

    let idle = measure(pid, || {});
    let mut since = None;
    let mut sent = 0;
    let preview = measure(pid, || match host::snapshot(&dir, &id, since).unwrap() {
        Snapshot::Screen(s) => {
            since = Some(s.version);
            sent += 1;
        }
        Snapshot::Unchanged => {}
    });
    let full = measure(pid, || {
        host::snapshot(&dir, &id, None).unwrap();
    });
    println!(
        "守护进程 CPU（占单核，{} 秒一段，每 0.5 秒一次请求）：",
        WINDOW.as_secs()
    );
    println!("  不预览              {idle:.3}%");
    println!("  会话台预览（带版本）{preview:.3}%（整屏只发了 {sent} 次）");
    println!("  每次整屏（最坏）    {full:.3}%");
    assert_eq!(sent, 1, "画面没变时不该重发整屏");
    assert!(
        preview - idle < 0.5,
        "预览让空闲守护进程 CPU 上升太多：{idle:.3}% → {preview:.3}%"
    );
}
