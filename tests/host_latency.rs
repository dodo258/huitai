//! 本地回环转发延迟实测（手动跑）：
//! `cargo test --release --test host_latency -- --ignored --nocapture`
//! 同一个回显假 CLI，分别经托管层（客户端 → socket → 守护进程 → PTY → cat → 原路回来）和直接读写 PTY，
//! 各测 1 字节往返，差值就是托管层带来的延迟。

mod host_support;

use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use host_support::*;
use huitai::host::{AttachMsg, pty};

const ROUNDS: usize = 2000;
const ECHO: &str = "stty raw -echo; printf R; cat";

fn report(name: &str, mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    let pick = |q: f64| samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)];
    let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
    println!(
        "{name}: 平均 {:.1}µs  p50 {:.1}µs  p99 {:.1}µs  最大 {:.1}µs（{} 次）",
        mean.as_secs_f64() * 1e6,
        pick(0.5).as_secs_f64() * 1e6,
        pick(0.99).as_secs_f64() * 1e6,
        samples[samples.len() - 1].as_secs_f64() * 1e6,
        samples.len()
    );
    pick(0.5)
}

#[test]
#[ignore = "实测用，手动跑"]
fn loopback_latency() {
    let host = TestHost::start();
    let id = host.spawn("lat", ECHO);
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    read_until(&mut att, &mut seen, "R", |s| s.live == b"R");
    let writer = att.writer();
    let mut hosted = Vec::with_capacity(ROUNDS);
    for i in 0..ROUNDS {
        let b = [b'a' + (i % 26) as u8];
        let t = Instant::now();
        writer.input(&b).unwrap();
        match att.recv(Some(WAIT)).unwrap() {
            AttachMsg::Output(got) => assert_eq!(got, b),
            other => panic!("{other:?}"),
        }
        hosted.push(t.elapsed());
    }

    let env = fake_env(&host.dir);
    let mut direct_pty = pty::spawn(ECHO, Path::new("/tmp"), &env, COLS, ROWS).unwrap();
    let mut buf = [0u8; 64];
    let n = direct_pty.master.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"R");
    let mut direct = Vec::with_capacity(ROUNDS);
    for i in 0..ROUNDS {
        let b = [b'a' + (i % 26) as u8];
        let t = Instant::now();
        direct_pty.master.write_all(&b).unwrap();
        let n = direct_pty.master.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b);
        direct.push(t.elapsed());
    }
    pty::signal_group(direct_pty.child.id(), libc::SIGKILL);
    let _ = direct_pty.child.wait();

    let h = report("经托管层", hosted);
    let d = report("直接 PTY", direct);
    println!(
        "托管层增加（p50 差）：{:.1}µs",
        (h.saturating_sub(d)).as_secs_f64() * 1e6
    );
}
