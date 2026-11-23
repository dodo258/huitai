//! 压力测试（docs/specs/huitai-v1.md「测试」节）：手动跑，不进 verify。一键跑：`tests/stress/run.sh`。
//!
//! 起真的 `ai daemon`（HOME 指到 /tmp 下的临时目录，不碰真实数据），假 CLI 是 `fake_cli.pl`，
//! 从外面用 `ps`/`lsof` 量守护进程的内存（RSS）、CPU 时间、文件描述符和线程数，每项前后对比。
//! 五项都标了 `#[ignore]`，`cargo test` 只编译不跑；数据行以 `STRESS|` 开头，抄进 PROGRESS.md。

#[path = "../host_support/mod.rs"]
mod host_support;
mod soak;
mod stability;

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use host_support::{Home, fake_env};
use huitai::core::{SessionId, Vendor};
use huitai::host::{
    self, AttachMsg, Attachment, EndReason, EventStream, HostEvent, Request, Response, Screen,
    Snapshot, SpawnSpec,
};

const COLS: u16 = 120;
const ROWS: u16 = 40;
const SESSIONS: usize = 20;
/// 方案「性能指标」：每条托管对话额外内存 ≤ 15 MB（含 1 万行历史）。
const PER_SESSION_LIMIT_MB: f64 = 15.0;
/// 方案「性能指标」：守护进程空闲 CPU ≈ 0。这里取单核 1% 当上限。
const IDLE_CPU_LIMIT: f64 = 1.0;
const FAKE_CLI: &str = include_str!("fake_cli.pl");

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn mb(kb: u64) -> f64 {
    kb as f64 / 1024.0
}

// ---------- 量守护进程 ----------

#[derive(Debug, Clone, Copy)]
struct Sample {
    rss_kb: u64,
    cpu: Duration,
    fds: usize,
    threads: usize,
}

/// 跑一个外部命令（ps、lsof、heap）取标准输出。限时 60 秒：系统忙时 lsof 这类要扫全机进程的命令可能卡住，
/// 卡住就直接失败并说清是哪条命令，不让整个测试一直挂着。
fn run(cmd: &str, args: &[&str]) -> String {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect(cmd);
    let mut stdout = child.stdout.take().expect("标准输出是管道");
    let reader = thread::spawn(move || {
        let mut s = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().expect(cmd).is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{cmd} {} 60 秒没结束", args.join(" "));
        }
        thread::sleep(Duration::from_millis(20));
    }
    String::from_utf8_lossy(&reader.join().expect("读输出的线程")).into_owned()
}

/// `ps` 的 CPU 时间（`分:秒.百分秒`，可能带小时）。
fn parse_cpu(text: &str) -> Duration {
    let secs = text.trim().split(':').fold(0.0, |acc, part| {
        acc * 60.0 + part.parse::<f64>().expect("ps 的 CPU 时间")
    });
    Duration::from_secs_f64(secs)
}

fn sample(pid: u32) -> Sample {
    let p = pid.to_string();
    let ps = run("ps", &["-o", "rss=,time=", "-p", &p]);
    let mut it = ps.split_whitespace();
    let rss_kb = it
        .next()
        .expect("ps 没输出：守护进程没了？")
        .parse()
        .unwrap();
    let cpu = parse_cpu(it.next().unwrap());
    let threads = run("ps", &["-M", "-p", &p]).lines().count() - 1;
    // 只数编号的描述符（cwd、txt 这类不算）。
    let fds = run("lsof", &["-n", "-P", "-p", &p])
        .lines()
        .skip(1)
        .filter(|l| {
            l.split_whitespace()
                .nth(3)
                .is_some_and(|fd| fd.starts_with(|c: char| c.is_ascii_digit()))
        })
        .count();
    Sample {
        rss_kb,
        cpu,
        fds,
        threads,
    }
}

/// 守护进程此刻还在用的堆内存（macOS `heap` 工具的 `All zones: N nodes (B bytes)`）。
/// RSS 包含分配器留着复用的空闲内存，判断泄漏用这个。
fn heap_bytes(pid: u32) -> u64 {
    let out = run("heap", &[&pid.to_string()]);
    let line = out
        .lines()
        .find(|l| l.starts_with("All zones:") && l.ends_with("bytes)"))
        .unwrap_or_else(|| panic!("heap 输出里没有总数：{out}"));
    let num = line.rsplit('(').next().unwrap().trim_end_matches(" bytes)");
    num.parse().expect("heap 的字节数")
}

/// 泄漏判断：全部对话结束后，存活堆内存回到基线附近（留 2 MB 给日志缓冲、哈希表扩容这类一次性的）。
const HEAP_SLACK: u64 = 2 << 20;

fn cpu_percent(a: &Sample, b: &Sample, wall: Duration) -> f64 {
    b.cpu.saturating_sub(a.cpu).as_secs_f64() / wall.as_secs_f64() * 100.0
}

/// 等到描述符和线程数回到基线（最多 10 秒），返回最后一次的读数。
fn back_to_baseline(pid: u32, base: &Sample, what: &str) -> Sample {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = sample(pid);
        if s.fds <= base.fds && s.threads <= base.threads {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "{what}：描述符/线程没回到基线：基线 fds={} threads={}，现在 fds={} threads={}",
            base.fds,
            base.threads,
            s.fds,
            s.threads
        );
        thread::sleep(Duration::from_millis(200));
    }
}

/// 后台每 50ms 读一次 RSS，记峰值。
struct PeakRss {
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

impl PeakRss {
    fn start(pid: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let (s, p) = (Arc::clone(&stop), Arc::clone(&peak));
        let handle = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let out = run("ps", &["-o", "rss=", "-p", &pid.to_string()]);
                if let Ok(kb) = out.trim().parse::<u64>() {
                    p.fetch_max(kb, Ordering::Relaxed);
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        Self {
            stop,
            peak,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take().unwrap().join().unwrap();
        self.peak.load(Ordering::Relaxed)
    }
}

// ---------- 守护进程和假 CLI ----------

struct Rig {
    home: Home,
    pid: u32,
    script: PathBuf,
    stop: PathBuf,
}

impl Rig {
    fn start(tag: &str) -> Self {
        let mut home = Home::new(&format!("st-{tag}"));
        let pid = home.ensure_daemon();
        let script = home.home.join("fake_cli.pl");
        fs::write(&script, FAKE_CLI).unwrap();
        let stop = home.home.join("stop");
        Self {
            home,
            pid,
            script,
            stop,
        }
    }

    /// `kill -9` 守护进程再拉起一个新的。
    fn restart_hard(&mut self) {
        self.home.kill_daemon_hard();
        self.pid = self.home.ensure_daemon();
    }

    fn spawn(&self, id: &str, command: &str) -> SessionId {
        let id = SessionId::from(id);
        let spec = SpawnSpec {
            id: id.clone(),
            vendor: Vendor::Claude,
            project: "stress".to_owned(),
            cwd: self.home.home.clone(),
            command: format!("stty raw -echo; {command}"),
        };
        assert_eq!(self.request(&Request::Spawn(spec)), Response::Ok);
        id
    }

    /// 按 `rate` 字节/秒持续输出的假 CLI，停止文件出现后画 FINAL 画面。
    fn spawn_fast(&self, id: &str, rate: u64) -> SessionId {
        let cmd = format!(
            "exec perl '{}' fast {rate} '{}' {id} {ROWS}",
            self.script.display(),
            self.stop.display()
        );
        self.spawn(id, &cmd)
    }

    /// 请求 5 秒没应答说明守护进程卡住了（死锁）。
    fn request(&self, req: &Request) -> Response {
        host::request(&self.home.data, req)
            .unwrap_or_else(|e| panic!("{req:?} 没应答（守护进程卡住？）：{e}"))
    }

    fn attach(&self, id: &SessionId) -> Attachment {
        host::attach(&self.home.data, id, fake_env(&self.home.home), COLS, ROWS)
            .unwrap_or_else(|e| panic!("attach {id} 失败：{e}"))
    }

    fn pids(&self) -> Vec<(SessionId, u32)> {
        match self.request(&Request::List) {
            Response::List(l) => l.into_iter().filter_map(|i| Some((i.id, i.pid?))).collect(),
            other => panic!("List 应答不对：{other:?}"),
        }
    }

    fn subscribe(&self) -> EventStream {
        host::subscribe(&self.home.data).expect("订阅事件")
    }

    fn sample(&self) -> Sample {
        sample(self.pid)
    }

    /// 等这条对话此刻的画面里出现 `needle`。
    fn wait_screen(&self, id: &SessionId, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Snapshot::Screen(s)) = host::snapshot(&self.home.data, id, None)
                && s.lines.iter().any(|l| {
                    l.iter()
                        .map(|r| r.text.as_str())
                        .collect::<String>()
                        .contains(needle)
                })
            {
                return;
            }
            assert!(Instant::now() < deadline, "{id} 的画面等不到 {needle:?}");
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// attach 一次，拿补发画面喂出来的屏幕，然后断开。只用在 CLI 已经不再输出的时候。
    fn restored(&self, id: &SessionId) -> Screen {
        let mut att = self.attach(id);
        let mut screen = Screen::new(COLS, ROWS);
        screen.feed(&read_restore(&mut att, id));
        screen
    }

    fn kill_all(&self, ids: &[SessionId]) {
        let mut events = self.subscribe();
        for id in ids {
            assert_eq!(
                self.request(&Request::Kill { id: id.clone() }),
                Response::Ok
            );
        }
        let mut left: HashSet<&SessionId> = ids.iter().collect();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !left.is_empty() {
            let rest = deadline.saturating_duration_since(Instant::now());
            assert!(!rest.is_zero(), "Kill 后等不到退出：{left:?}");
            if let Ok(Some(HostEvent::Exited { id, .. })) = events.next(Some(rest)) {
                left.remove(&id);
            }
        }
    }
}

fn first_frame(att: &mut Attachment, id: &SessionId) -> Vec<u8> {
    match att.recv(Some(Duration::from_secs(10))) {
        Ok(AttachMsg::Output(bytes)) => bytes,
        other => panic!("{id} 的第一帧应是补发画面，收到 {other:?}"),
    }
}

/// 补发画面可能分成多帧：读到 300ms 没有新帧为止（CLI 已不再输出时用）。
fn read_restore(att: &mut Attachment, id: &SessionId) -> Vec<u8> {
    let mut all = first_frame(att, id);
    loop {
        match att.recv(Some(Duration::from_millis(300))) {
            Ok(AttachMsg::Output(more)) => all.extend_from_slice(&more),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return all;
            }
            other => panic!("{id} 的补发画面没收完：{other:?}"),
        }
    }
}

/// 去掉所有 `ESC ] 2 ; … BEL` 标题序列。
fn without_title_osc(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some(i) = rest.windows(4).position(|w| w == b"\x1b]2;") {
        out.extend_from_slice(&rest[..i]);
        match rest[i..].iter().position(|&b| b == 0x07) {
            Some(end) => rest = &rest[i + end + 1..],
            None => {
                rest = &rest[i..];
                break;
            }
        }
    }
    out.extend_from_slice(rest);
    out
}

fn assert_same_screen(a: &Screen, b: &Screen, what: &str) {
    assert_eq!(a.signature(), b.signature(), "{what}：画面不一致");
    assert_eq!(a.history_text(), b.history_text(), "{what}：历史不一致");
}

fn final_rows(tag: &str) -> Vec<String> {
    let mut rows: Vec<String> = (0..ROWS - 1)
        .map(|i| format!("FINAL {tag} row {i}"))
        .collect();
    rows.push(String::new());
    rows
}

/// 一直接着一条对话读输出的客户端：画面喂进自己的状态机，可选留下全部字节。
struct Reader {
    live: Arc<Mutex<Live>>,
    handle: JoinHandle<Option<AttachMsg>>,
}

struct Live {
    screen: Screen,
    bytes: u64,
    kept: Option<Vec<u8>>,
}

impl Reader {
    fn start(mut att: Attachment, id: &SessionId, keep: bool) -> Self {
        let mut screen = Screen::new(COLS, ROWS);
        screen.feed(&first_frame(&mut att, id));
        let live = Arc::new(Mutex::new(Live {
            screen,
            bytes: 0,
            kept: keep.then(Vec::new),
        }));
        let l = Arc::clone(&live);
        let handle = thread::spawn(move || {
            loop {
                match att.recv(None) {
                    Ok(AttachMsg::Output(b)) => {
                        let mut l = l.lock().unwrap();
                        l.screen.feed(&b);
                        l.bytes += b.len() as u64;
                        if let Some(k) = l.kept.as_mut() {
                            k.extend_from_slice(&b);
                        }
                    }
                    Ok(msg) => return Some(msg),
                    Err(_) => return None,
                }
            }
        });
        Self { live, handle }
    }

    fn visible(&self) -> Vec<String> {
        self.live.lock().unwrap().screen.visible_text()
    }

    fn alive(&self) -> bool {
        !self.handle.is_finished()
    }

    /// 等转发通道结束，返回结束消息和最后的状态。
    fn join(self) -> (Option<AttachMsg>, Live) {
        let end = self.handle.join().expect("读线程崩了");
        let live = Arc::into_inner(self.live).unwrap().into_inner().unwrap();
        (end, live)
    }
}

/// 后台一直读事件（像会话台那样），数一共收到多少。
fn drain_events(mut events: EventStream) -> Arc<AtomicU64> {
    let count = Arc::new(AtomicU64::new(0));
    let c = Arc::clone(&count);
    thread::spawn(move || {
        while let Ok(Some(_)) = events.next(None) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    });
    count
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

// ---------- 五项压力测试 ----------

/// 20 条假 CLI 同时持续高速输出（默认 30 分钟、每条 1 MB/s）：其中 4 条一直有客户端接着，其余没人接；
/// 期间不停 List、取快照、attach/detach，量响应时间；结束后比重连画面、量每条对话的内存、空闲 CPU，
/// 全部结束后描述符和线程回到基线。
#[test]
#[ignore = "压力测试，手动跑：tests/stress/run.sh"]
fn twenty_sessions_fast_output() {
    let minutes = env_f64("STRESS_MINUTES", 30.0);
    let rate = (env_f64("STRESS_RATE_KB", 1024.0) * 1024.0) as u64;
    let duration = Duration::from_secs_f64(minutes * 60.0);
    let every = (duration / 15).clamp(Duration::from_secs(10), Duration::from_secs(120));
    let rig = Rig::start("long");
    let events = drain_events(rig.subscribe());
    thread::sleep(Duration::from_millis(500));
    let base = rig.sample();
    let base_heap = heap_bytes(rig.pid);
    println!(
        "STRESS|20 条高速输出|开始：{minutes} 分钟，每条 {} KB/s；基线 RSS {:.1} MB fds {} 线程 {}",
        rate / 1024,
        mb(base.rss_kb),
        base.fds,
        base.threads
    );

    let ids: Vec<SessionId> = (0..SESSIONS)
        .map(|i| rig.spawn_fast(&format!("s{i:02}"), rate))
        .collect();
    let mut readers = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let mut att = rig.attach(id);
        if i < 4 {
            readers.push(Reader::start(att, id, false));
        } else {
            first_frame(&mut att, id);
        }
    }
    let unattached = &ids[4..];

    let start = Instant::now();
    let mut last = (start, rig.sample());
    let mut samples = Vec::new();
    let (mut max_list, mut max_snap, mut max_attach) = (0.0f64, 0.0f64, 0.0f64);
    let mut round = 0usize;
    while start.elapsed() < duration {
        thread::sleep(Duration::from_secs(2));
        let t = Instant::now();
        rig.request(&Request::List);
        max_list = max_list.max(ms(t.elapsed()));
        let id = &unattached[round % unattached.len()];
        let t = Instant::now();
        host::snapshot(&rig.home.data, id, None).expect("取快照");
        max_snap = max_snap.max(ms(t.elapsed()));
        let t = Instant::now();
        let mut att = rig.attach(id);
        first_frame(&mut att, id);
        drop(att);
        max_attach = max_attach.max(ms(t.elapsed()));
        round += 1;
        for (i, r) in readers.iter().enumerate() {
            assert!(r.alive(), "s{i:02} 的客户端被断开了");
        }
        if last.0.elapsed() >= every {
            let s = rig.sample();
            let cpu = cpu_percent(&last.1, &s, last.0.elapsed());
            println!(
                "STRESS|20 条高速输出|{:>5.1} 分|RSS {:>6.1} MB|CPU {:>5.1}%|fds {}|线程 {}|List 最慢 {max_list:.1} ms|快照最慢 {max_snap:.1} ms|attach 最慢 {max_attach:.1} ms|事件 {}",
                start.elapsed().as_secs_f64() / 60.0,
                mb(s.rss_kb),
                cpu,
                s.fds,
                s.threads,
                events.load(Ordering::Relaxed)
            );
            samples.push((s, cpu));
            last = (Instant::now(), s);
        }
    }

    // 停止输出：每条画一屏 FINAL 后空闲。
    fs::write(&rig.stop, b"").unwrap();
    for id in &ids {
        rig.wait_screen(id, &format!("FINAL {id} row 0"), Duration::from_secs(30));
    }
    for (i, r) in readers.iter().enumerate() {
        let deadline = Instant::now() + Duration::from_secs(10);
        while r.visible() != final_rows(&ids[i].to_string()) {
            assert!(
                Instant::now() < deadline,
                "s{i:02} 的客户端没收到 FINAL 画面"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
    thread::sleep(Duration::from_secs(1));
    let loaded = rig.sample();
    let t = Instant::now();
    thread::sleep(Duration::from_secs(10));
    let idle = rig.sample();
    let idle_cpu = cpu_percent(&loaded, &idle, t.elapsed());

    // 重连画面一致：一直接着的客户端看到的 = 重新 attach 补发的；没人接的两次 attach 一致且就是 FINAL 画面。
    for (r, id) in readers.into_iter().zip(&ids) {
        let restored = rig.restored(id);
        let (end, live) = r.join();
        assert_eq!(end, Some(AttachMsg::End(EndReason::Elsewhere)));
        assert_same_screen(&live.screen, &restored, &format!("{id} 一直接着 vs 重连"));
    }
    for id in unattached {
        let a = rig.restored(id);
        let b = rig.restored(id);
        assert_same_screen(&a, &b, &format!("{id} 两次重连"));
        assert_eq!(
            a.visible_text(),
            final_rows(&id.to_string()),
            "{id} 重连画面"
        );
    }

    let per_session = (mb(loaded.rss_kb) - mb(base.rss_kb)) / SESSIONS as f64;
    let half = samples.len() / 2;
    let first_half = samples[..half.max(1)]
        .iter()
        .map(|s| s.0.rss_kb)
        .max()
        .unwrap();
    let second_half = samples[half..].iter().map(|s| s.0.rss_kb).max().unwrap();
    let busy_cpu = samples.iter().map(|s| s.1).sum::<f64>() / samples.len() as f64;
    println!(
        "STRESS|20 条高速输出|结果：每条额外内存 {per_session:.1} MB（上限 {PER_SESSION_LIMIT_MB}）；前半程 RSS 峰值 {:.1} MB、后半程 {:.1} MB；输出时 CPU 平均 {busy_cpu:.1}%；停下后空闲 CPU {idle_cpu:.2}%；List/快照/attach 最慢 {max_list:.1}/{max_snap:.1}/{max_attach:.1} ms；重连画面 20/20 一致",
        mb(first_half),
        mb(second_half)
    );
    assert!(per_session <= PER_SESSION_LIMIT_MB, "每条对话内存超标");
    assert!(idle_cpu <= IDLE_CPU_LIMIT, "空闲 CPU 超标");
    assert!(
        second_half as f64 <= first_half as f64 * 1.10 + 8.0 * 1024.0,
        "后半程内存还在涨（可能泄漏）"
    );

    rig.kill_all(&ids);
    let after = back_to_baseline(rig.pid, &base, "全部结束后");
    let after_heap = heap_bytes(rig.pid);
    println!(
        "STRESS|20 条高速输出|全部结束后：存活堆 {:.2} MB（基线 {:.2} MB），RSS {:.1} MB fds {} 线程 {}（基线 {:.1} MB / {} / {}）",
        after_heap as f64 / 1048576.0,
        base_heap as f64 / 1048576.0,
        mb(after.rss_kb),
        after.fds,
        after.threads,
        mb(base.rss_kb),
        base.fds,
        base.threads
    );
    assert!(
        after_heap <= base_heap + HEAP_SLACK,
        "全部结束后堆内存没回到基线（泄漏）"
    );

    // 同一个守护进程再来一轮（20 条新对话输出 30 秒后全部结束），存活堆照样回到基线。
    fs::remove_file(&rig.stop).unwrap();
    let again: Vec<SessionId> = (0..SESSIONS)
        .map(|i| rig.spawn_fast(&format!("r{i:02}"), rate))
        .collect();
    for id in &again {
        let mut att = rig.attach(id);
        first_frame(&mut att, id);
    }
    thread::sleep(Duration::from_secs(30));
    fs::write(&rig.stop, b"").unwrap();
    for id in &again {
        rig.wait_screen(id, &format!("FINAL {id} row 0"), Duration::from_secs(30));
    }
    rig.kill_all(&again);
    let second = back_to_baseline(rig.pid, &base, "第二轮全部结束后");
    let second_heap = heap_bytes(rig.pid);
    println!(
        "STRESS|20 条高速输出|第二轮全部结束后：存活堆 {:.2} MB，RSS {:.1} MB fds {} 线程 {}（第一轮结束后 {:.1} MB）",
        second_heap as f64 / 1048576.0,
        mb(second.rss_kb),
        second.fds,
        second.threads,
        mb(after.rss_kb)
    );
    assert!(
        second_heap <= base_heap + HEAP_SLACK,
        "第二轮后堆内存没回到基线（泄漏）"
    );
}

/// 单条对话瞬间输出 50 MB：一条有客户端接着（收到的字节必须和文件逐字节一致），
/// 一条没人接、每个字一种真彩色（补发画面最大的情况）。量处理时间和内存峰值，最后比重连画面。
#[test]
#[ignore = "压力测试，手动跑：tests/stress/run.sh"]
fn burst_50mb() {
    const SIZE: usize = 50 << 20;
    let rig = Rig::start("burst");
    let base = rig.sample();

    let mut plain = Vec::with_capacity(SIZE + 256);
    let mut n = 0u64;
    while plain.len() < SIZE {
        n += 1;
        plain.extend_from_slice(
            format!(
                "\x1b[3{}m{n:>8}\x1b[0m 中文 burst line {}\r\n",
                n % 7 + 1,
                "y".repeat((n % 130) as usize)
            )
            .as_bytes(),
        );
    }
    let mut rainbow = Vec::with_capacity(SIZE + 256);
    while rainbow.len() < SIZE {
        n += 1;
        for c in 0..COLS as u64 {
            let v = (n * 7 + c * 13) % 256;
            rainbow.extend_from_slice(
                format!("\x1b[38;2;{v};{};{}m#", 255 - v, (v * 3) % 256).as_bytes(),
            );
        }
        rainbow.extend_from_slice(b"\x1b[0m\r\n");
    }
    fs::write(rig.home.home.join("plain.bin"), &plain).unwrap();
    fs::write(rig.home.home.join("rainbow.bin"), &rainbow).unwrap();
    let cat = |file: &str| format!("read go; cat {file}; printf '\\r\\nDONE'; exec sleep 3600");

    // 有客户端接着的那条。
    let a = rig.spawn("b-plain", &cat("plain.bin"));
    let att = rig.attach(&a);
    let writer = att.writer();
    let reader = Reader::start(att, &a, true);
    let peak = PeakRss::start(rig.pid);
    let t = Instant::now();
    writer.input(b"\n").unwrap();
    rig.wait_screen(&a, "DONE", Duration::from_secs(120));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !reader
        .live
        .lock()
        .unwrap()
        .screen
        .visible_text()
        .iter()
        .any(|l| l == "DONE")
    {
        assert!(Instant::now() < deadline, "客户端没收完");
        thread::sleep(Duration::from_millis(20));
    }
    let took = t.elapsed();
    let peak_a = peak.finish();
    let restored = rig.restored(&a);
    let (_, live) = reader.join();
    let mut expect = plain.clone();
    expect.extend_from_slice(b"\r\nDONE");
    // 守护进程在状态变化时补发带「状态 项目 · 厂商」前缀的标题（OSC 2，c5993ca 起）；
    // 文件本身没有 OSC，去掉它们后应和输出逐字节一致。
    let got = without_title_osc(&live.kept.unwrap());
    assert!(
        got == expect,
        "客户端收到的字节和输出不一致：{} vs {}",
        got.len(),
        expect.len()
    );
    assert_same_screen(&live.screen, &restored, "50 MB 后重连");
    println!(
        "STRESS|50 MB 瞬间输出|有客户端接着：{:.2} s 全部转发完（{:.0} MB/s），字节逐字节一致，RSS 峰值 {:.1} MB（基线 {:.1}），重连画面一致",
        took.as_secs_f64(),
        SIZE as f64 / 1048576.0 / took.as_secs_f64(),
        mb(peak_a),
        mb(base.rss_kb)
    );

    // 没人接、每个字一种颜色的那条。
    let b = rig.spawn("b-rainbow", &cat("rainbow.bin"));
    let mut att = rig.attach(&b);
    first_frame(&mut att, &b);
    let peak = PeakRss::start(rig.pid);
    let t = Instant::now();
    // 发完回车就断开（读写两头都关），之后没人接着。
    att.writer().input(b"\n").unwrap();
    drop(att);
    rig.wait_screen(&b, "DONE", Duration::from_secs(120));
    let took = t.elapsed();
    let peak_b = peak.finish();
    let t = Instant::now();
    let mut att = rig.attach(&b);
    let frame = read_restore(&mut att, &b);
    let restore_ms = ms(t.elapsed()) - 300.0;
    drop(att);
    let mut one = Screen::new(COLS, ROWS);
    one.feed(&frame);
    let two = rig.restored(&b);
    assert_same_screen(&one, &two, "真彩色 50 MB 两次重连");
    assert_eq!(
        one.history_text().len(),
        huitai::host::screen::HISTORY_LINES
    );
    println!(
        "STRESS|50 MB 瞬间输出|没人接、每字一色：{:.2} s 处理完（{:.0} MB/s），RSS 峰值 {:.1} MB；补发画面 {:.1} MB、{restore_ms:.0} ms，两次重连一致",
        took.as_secs_f64(),
        SIZE as f64 / 1048576.0 / took.as_secs_f64(),
        mb(peak_b),
        frame.len() as f64 / 1048576.0
    );

    rig.kill_all(&[a, b]);
    let after = back_to_baseline(rig.pid, &base, "50 MB 两条结束后");
    println!(
        "STRESS|50 MB 瞬间输出|结束后 RSS {:.1} MB fds {} 线程 {}（基线 {:.1} MB / {} / {}）",
        mb(after.rss_kb),
        after.fds,
        after.threads,
        mb(base.rss_kb),
        base.fds,
        base.threads
    );
}

/// 1 秒内对同一条正在输出（历史已满 1 万行）的对话 attach/detach 200 次，三种断开方式轮流：
/// 客户端直接断开、Detach 请求、被下一次 attach 接走。之后描述符和线程回到基线，重连画面一致。
#[test]
#[ignore = "压力测试，手动跑：tests/stress/run.sh"]
fn attach_detach_200_in_a_second() {
    let rig = Rig::start("churn");
    let id = rig.spawn_fast("churn", 256 * 1024);
    let mut att = rig.attach(&id);
    first_frame(&mut att, &id);
    drop(att);
    // 等历史攒满 1 万行（每行约 140 字节）。
    thread::sleep(Duration::from_secs(8));
    let base = rig.sample();

    let t = Instant::now();
    let mut prev: Option<Attachment> = None;
    let mut restore_bytes = 0usize;
    for i in 0..200 {
        let mut att = rig.attach(&id);
        restore_bytes += first_frame(&mut att, &id).len();
        if let Some(mut p) = prev.take() {
            // 上一个被这一次接走：它应收到 Elsewhere。
            let end = loop {
                match p.recv(Some(Duration::from_secs(5))) {
                    Ok(AttachMsg::Output(_)) => continue,
                    other => break other,
                }
            };
            assert_eq!(
                end.ok(),
                Some(AttachMsg::End(EndReason::Elsewhere)),
                "第 {i} 次"
            );
        }
        match i % 3 {
            0 => drop(att),
            1 => {
                assert_eq!(
                    rig.request(&Request::Detach { id: id.clone() }),
                    Response::Ok
                );
                drop(att);
            }
            _ => prev = Some(att),
        }
    }
    drop(prev);
    let took = t.elapsed();

    fs::write(&rig.stop, b"").unwrap();
    rig.wait_screen(&id, &format!("FINAL {id} row 0"), Duration::from_secs(30));
    let after = back_to_baseline(rig.pid, &base, "200 次 attach/detach 后");
    let a = rig.restored(&id);
    let b = rig.restored(&id);
    assert_same_screen(&a, &b, "200 次后两次重连");
    assert_eq!(a.visible_text(), final_rows(&id.to_string()));
    println!(
        "STRESS|attach/detach 200 次|{:.0} ms（平均每次 {:.2} ms，补发画面平均 {:.0} KB）；之后 fds {} 线程 {}（基线 {} / {}），RSS {:.1} MB（基线 {:.1}），重连画面一致",
        ms(took),
        ms(took) / 200.0,
        restore_bytes as f64 / 200.0 / 1024.0,
        after.fds,
        after.threads,
        base.fds,
        base.threads,
        mb(after.rss_kb),
        mb(base.rss_kb)
    );
    assert!(
        took <= Duration::from_secs(1),
        "200 次 attach/detach 超过 1 秒"
    );
    rig.kill_all(&[id]);
}

/// 20 条在高速输出时守护进程被 `kill -9`，重启，连做 3 轮：假 CLI 全部跟着退出（不留孤儿进程），
/// 新守护进程认出 20 条被中断，描述符和线程与刚启动时一样。
#[test]
#[ignore = "压力测试，手动跑：tests/stress/run.sh"]
fn daemon_kill9_then_restart() {
    let mut rig = Rig::start("kill9");
    let fresh = rig.sample();
    for round in 1..=3 {
        let ids: Vec<SessionId> = (0..SESSIONS)
            .map(|i| rig.spawn_fast(&format!("k{i:02}"), 1024 * 1024))
            .collect();
        for id in &ids {
            let mut att = rig.attach(id);
            first_frame(&mut att, id);
        }
        thread::sleep(Duration::from_secs(3));
        let pids = rig.pids();
        assert_eq!(pids.len(), SESSIONS);
        let t = Instant::now();
        rig.restart_hard();
        let restart_ms = ms(t.elapsed());

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // SAFETY: kill 信号 0 只查进程在不在。
            let alive: Vec<u32> = pids
                .iter()
                .map(|(_, p)| *p)
                .filter(|p| unsafe { libc::kill(*p as libc::pid_t, 0) } == 0)
                .collect();
            if alive.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "守护进程被 kill -9 后假 CLI 没退出：{alive:?}"
            );
            thread::sleep(Duration::from_millis(100));
        }
        let orphans_ms = ms(t.elapsed());

        let list = match rig.request(&Request::Interrupted) {
            Response::Interrupted(l) => l,
            other => panic!("Interrupted 应答不对：{other:?}"),
        };
        let got: HashSet<_> = list.iter().map(|r| r.id.clone()).collect();
        assert_eq!(
            got,
            ids.iter().cloned().collect(),
            "第 {round} 轮被中断的对话"
        );
        thread::sleep(Duration::from_millis(300));
        let s = rig.sample();
        assert!(
            s.fds <= fresh.fds && s.threads <= fresh.threads,
            "第 {round} 轮重启后：fds {} 线程 {}，刚启动时 {} / {}",
            s.fds,
            s.threads,
            fresh.fds,
            fresh.threads
        );
        println!(
            "STRESS|kill -9 后重启|第 {round} 轮：kill 到新守护进程能连上 {restart_ms:.0} ms，20 个假 CLI 全部退出用时 {orphans_ms:.0} ms，认出被中断 {}/20；新守护进程 RSS {:.1} MB fds {} 线程 {}（刚启动时 {:.1} MB / {} / {}）",
            got.len(),
            mb(s.rss_kb),
            s.fds,
            s.threads,
            mb(fresh.rss_kb),
            fresh.fds,
            fresh.threads
        );
    }
}

/// 子进程卡死不读输入：
/// 一条什么都不读的，连续 20 次 attach、塞 256 KB 输入、Detach；
/// 一条不读输入却一直发终端查询（守护进程要代答）的，没人接着。
/// 期间守护进程照常应答，attach/detach 不卡，线程和描述符不涨，Kill 后能回收。
#[test]
#[ignore = "压力测试，手动跑：tests/stress/run.sh"]
fn stuck_child_not_reading_input() {
    let rig = Rig::start("stuck");
    let base = rig.sample();
    let stuck = rig.spawn("stuck", "printf READY; exec sleep 3600");
    let queries = rig.spawn(
        "queries",
        &format!("exec perl '{}' queries", rig.script.display()),
    );
    for id in [&stuck, &queries] {
        let mut att = rig.attach(id);
        first_frame(&mut att, id);
    }
    rig.wait_screen(&stuck, "READY", Duration::from_secs(10));

    let mut worst_detach = 0.0f64;
    let mut mid = None;
    for round in 0..20 {
        let mut att = rig.attach(&stuck);
        let screen = {
            let mut s = Screen::new(COLS, ROWS);
            s.feed(&first_frame(&mut att, &stuck));
            s
        };
        assert_eq!(screen.visible_text()[0], "READY", "第 {round} 次重连画面");
        let writer = att.writer();
        let paste = thread::spawn(move || {
            for _ in 0..4 {
                if writer.input(&[b'z'; 64 * 1024]).is_err() {
                    return;
                }
            }
        });
        thread::sleep(Duration::from_millis(100));
        let t = Instant::now();
        assert_eq!(
            rig.request(&Request::Detach { id: stuck.clone() }),
            Response::Ok
        );
        let end = loop {
            match att.recv(Some(Duration::from_secs(5))) {
                Ok(AttachMsg::Output(_)) => continue,
                other => break other,
            }
        };
        worst_detach = worst_detach.max(ms(t.elapsed()));
        assert_eq!(
            end.ok(),
            Some(AttachMsg::End(EndReason::Detached)),
            "第 {round} 次 Detach"
        );
        drop(att);
        paste.join().unwrap();
        rig.request(&Request::List);
        if round == 0 {
            thread::sleep(Duration::from_millis(300));
            mid = Some(rig.sample());
        }
    }
    let t = Instant::now();
    match rig.request(&Request::Snapshot {
        id: queries.clone(),
        since: None,
    }) {
        Response::Snapshot(_) => {}
        other => panic!("发查询的那条取不到快照：{other:?}"),
    }
    let snap_ms = ms(t.elapsed());
    thread::sleep(Duration::from_millis(500));
    let after_rounds = rig.sample();
    let mid = mid.unwrap();
    println!(
        "STRESS|子进程不读输入|20 次 attach+塞 256 KB+Detach：Detach 最慢 {worst_detach:.0} ms；第 1 次后 fds {} 线程 {}，第 20 次后 fds {} 线程 {}；一直发查询的那条取快照 {snap_ms:.0} ms",
        mid.fds, mid.threads, after_rounds.fds, after_rounds.threads
    );
    assert!(
        after_rounds.fds <= mid.fds && after_rounds.threads <= mid.threads,
        "卡住的对话每次 attach 都留下线程/描述符"
    );

    let t = Instant::now();
    rig.kill_all(&[stuck, queries]);
    let kill_ms = ms(t.elapsed());
    let after = back_to_baseline(rig.pid, &base, "卡死的两条结束后");
    println!(
        "STRESS|子进程不读输入|Kill 到两条都回收 {kill_ms:.0} ms；之后 fds {} 线程 {}（基线 {} / {}）",
        after.fds, after.threads, base.fds, base.threads
    );
}

// ---------- 压力测试发现的问题的回归测试（很快，不标 ignore，跟着 cargo test 跑） ----------

/// 不读输入的 CLI：塞进去的输入写不进 PTY 时，Detach 仍能让这个客户端的连接关掉（守护进程不留线程和描述符）。
#[test]
fn regress_detach_while_cli_does_not_read_input() {
    let rig = Rig::start("rg-stuck");
    let id = rig.spawn("stuck", "printf READY; exec sleep 60");
    drop(rig.attach(&id));
    rig.wait_screen(&id, "READY", Duration::from_secs(10));
    thread::sleep(Duration::from_millis(300));
    let base = rig.sample();

    let mut att = rig.attach(&id);
    first_frame(&mut att, &id);
    let writer = att.writer();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for _ in 0..64 {
            if writer.input(&[b'z'; 64 * 1024]).is_err() {
                break;
            }
        }
        let _ = done_tx.send(());
    });
    thread::sleep(Duration::from_millis(500));
    assert_eq!(
        rig.request(&Request::Detach { id: id.clone() }),
        Response::Ok
    );
    drop(att);
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("Detach 后连接没关：守护进程的输入线程卡在写 PTY 上");
    back_to_baseline(rig.pid, &base, "Detach 卡住的对话后");
    rig.kill_all(&[id]);
}

/// 不读输入却一直发终端查询的 CLI（没人接着时守护进程代答）：守护进程不能卡死在它的锁上。
#[test]
fn regress_cli_sending_queries_without_reading_does_not_hang_daemon() {
    let rig = Rig::start("rg-query");
    let id = rig.spawn(
        "queries",
        &format!("exec perl '{}' queries", rig.script.display()),
    );
    drop(rig.attach(&id));
    // 代答把 PTY 的输入缓冲写满只要零点几秒。
    thread::sleep(Duration::from_secs(2));
    for _ in 0..3 {
        rig.request(&Request::List);
        rig.request(&Request::Snapshot {
            id: id.clone(),
            since: None,
        });
    }
    rig.kill_all(&[id]);
}

/// 补发画面超过单帧上限（16 MB）时照样能 attach：每格样式都不同的宽终端，补发序列比原始输出大十几倍。
#[test]
fn regress_restore_larger_than_one_frame() {
    const WIDE: u16 = 1000;
    let rig = Rig::start("rg-big");
    let mut bytes = b"\x1b[1;3;4;9;38;2;101;102;103;48;2;201;202;203m".to_vec();
    for _ in 0..400 {
        for _ in 0..WIDE / 2 {
            bytes.extend_from_slice(b"\x1b[7m#\x1b[27m#");
        }
    }
    bytes.extend_from_slice(b"\x1b[0m\r\nDONE");
    fs::write(rig.home.home.join("big.bin"), &bytes).unwrap();
    let id = rig.spawn("big", "cat big.bin; exec sleep 60");
    let attach = || {
        host::attach(&rig.home.data, &id, fake_env(&rig.home.home), WIDE, 10)
            .unwrap_or_else(|e| panic!("attach 失败：{e}"))
    };
    drop(attach());
    rig.wait_screen(&id, "DONE", Duration::from_secs(60));
    let mut att = attach();
    let restore = read_restore(&mut att, &id);
    assert!(
        restore.len() > 16 << 20,
        "补发画面 {} 字节，没超过单帧上限",
        restore.len()
    );
    let mut screen = Screen::new(WIDE, 10);
    screen.feed(&restore);
    assert!(screen.visible_text().iter().any(|l| l == "DONE"));
    rig.kill_all(&[id]);
}

/// 关掉的事件订阅在没有新事件时也会被释放。
#[test]
fn regress_closed_subscription_is_released_when_idle() {
    let rig = Rig::start("rg-sub");
    thread::sleep(Duration::from_millis(300));
    let base = rig.sample();
    for _ in 0..5 {
        drop(rig.subscribe());
    }
    back_to_baseline(rig.pid, &base, "关掉 5 个订阅后");
}

/// 1 万行历史的 120 列对话，守护进程多用的内存在方案指标（15 MB）内。
#[test]
fn regress_full_history_fits_memory_budget() {
    let rig = Rig::start("rg-mem");
    thread::sleep(Duration::from_millis(300));
    let base = rig.sample();
    let mut bytes = Vec::new();
    for i in 0..12_000u32 {
        bytes.extend_from_slice(
            format!("\x1b[3{}m{i:>6}\x1b[0m 中文 {}\r\n", i % 7, "m".repeat(100)).as_bytes(),
        );
    }
    bytes.extend_from_slice(b"DONE");
    fs::write(rig.home.home.join("hist.bin"), &bytes).unwrap();
    let id = rig.spawn("hist", "cat hist.bin; exec sleep 60");
    drop(rig.attach(&id));
    rig.wait_screen(&id, "DONE", Duration::from_secs(60));
    let restored = rig.restored(&id);
    assert_eq!(
        restored.history_text().len(),
        huitai::host::screen::HISTORY_LINES
    );
    let used = mb(rig.sample().rss_kb) - mb(base.rss_kb);
    assert!(
        used <= PER_SESSION_LIMIT_MB,
        "一条满历史的对话多用 {used:.1} MB"
    );
    rig.kill_all(&[id]);
}

/// attach 了却不读的客户端（补发画面远大于 socket 缓冲）：守护进程照常应答 List、Detach，不被它堵住。
#[test]
fn regress_client_not_reading_does_not_block_daemon() {
    let rig = Rig::start("rg-slow");
    let mut bytes = Vec::new();
    for i in 0..12_000u32 {
        bytes.extend_from_slice(
            format!("\x1b[3{}m{i:>6}\x1b[0m {}\r\n", i % 7, "s".repeat(100)).as_bytes(),
        );
    }
    bytes.extend_from_slice(b"DONE");
    fs::write(rig.home.home.join("hist.bin"), &bytes).unwrap();
    let id = rig.spawn("slow", "cat hist.bin; exec sleep 60");
    drop(rig.attach(&id));
    rig.wait_screen(&id, "DONE", Duration::from_secs(60));

    let att = rig.attach(&id);
    thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    rig.request(&Request::List);
    assert_eq!(
        rig.request(&Request::Detach { id: id.clone() }),
        Response::Ok
    );
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "不读的客户端把守护进程堵了 {:?}",
        t.elapsed()
    );
    drop(att);
    rig.kill_all(&[id]);
}

/// 接着却不读的标签页：CLI 照样往前输出（不被它拖住），积压超过上限后这个客户端被断开，守护进程内存有上限。
#[test]
fn regress_stalled_client_does_not_stall_cli() {
    let rig = Rig::start("rg-stall");
    thread::sleep(Duration::from_millis(300));
    let base = rig.sample();
    let id = rig.spawn_fast("stall", 4 << 20);
    let mut att = rig.attach(&id);
    first_frame(&mut att, &id);
    // 画面多久没变：CLI 一直在输出，没被不读的标签页拖住时，画面隔一会儿就变；
    // 守护进程持锁卡在写这个标签页上时，取快照拿不到锁（或超时），这段时间画面就算没变。
    let screen_text = || match host::snapshot(&rig.home.data, &id, None) {
        Ok(Snapshot::Screen(s)) => Some(
            s.lines
                .iter()
                .map(|l| l.iter().map(|r| r.text.as_str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    };
    let start = Instant::now();
    let mut last = screen_text();
    let mut changed_at = start;
    let mut longest = Duration::ZERO;
    while start.elapsed() < Duration::from_secs(6) {
        let now = screen_text();
        if now.is_some() && now != last {
            longest = longest.max(changed_at.elapsed());
            changed_at = Instant::now();
            last = now;
        }
        thread::sleep(Duration::from_millis(100));
    }
    longest = longest.max(changed_at.elapsed());
    assert!(
        longest < Duration::from_millis(3500),
        "标签页不读时 CLI 的画面停了 {longest:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let attached = || match rig.request(&Request::List) {
        Response::List(l) => l.iter().any(|i| i.id == id && i.attached),
        other => panic!("List 应答不对：{other:?}"),
    };
    while attached() {
        assert!(Instant::now() < deadline, "积压超限后客户端没被断开");
        thread::sleep(Duration::from_millis(100));
    }
    let used = mb(rig.sample().rss_kb) - mb(base.rss_kb);
    assert!(used < 64.0, "守护进程替不读的客户端攒了 {used:.1} MB");
    drop(att);
    rig.kill_all(&[id]);
}
