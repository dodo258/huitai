//! 闲置自动收回：用 sh 脚本当假 CLI、假时钟快进，覆盖闲置+无连接→收回、有连接不收、在跑不收、
//! 收回后不进被中断、设置 0 关闭，以及退出键不管用时按步骤 SIGTERM / SIGKILL。

mod host_support;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use host_support::{Seen, TestHost, WAIT, fake_env, read_snapshot, read_until, wait_event};
use huitai::core::{Config, SessionId};
use huitai::host::idle::{Clock, QUIT_GRACE, TERM_GRACE};
use huitai::host::{
    self, Attachment, Daemon, DaemonHandle, EventStream, HostEvent, Request, Response, Snapshot,
    interrupt,
};

const MIN: Duration = Duration::from_secs(60);

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// 进程内守护进程 + 手动快进的假时钟。
struct IdleHost {
    host: TestHost,
    daemon: DaemonHandle,
    now: Arc<Mutex<Instant>>,
}

impl IdleHost {
    fn start(minutes: u32) -> Self {
        let dir = PathBuf::from(format!(
            "/tmp/hti-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock = {
            let now = Arc::clone(&now);
            Clock::new(move || *now.lock().unwrap())
        };
        let daemon = Daemon::bind_with_clock(&dir, clock).expect("守护进程监听失败");
        let handle = daemon.handle();
        thread::spawn(move || daemon.run());
        Config {
            idle_reclaim_minutes: minutes,
            ..Config::default()
        }
        .save(&dir.join("config.json"))
        .unwrap();
        Self {
            host: TestHost {
                dir,
                handle: handle.clone(),
            },
            daemon: handle,
            now,
        }
    }

    fn advance(&self, d: Duration) {
        *self.now.lock().unwrap() += d;
    }

    /// 登记并拉起一条假 CLI，等它打出 `ready`。返回还接着的标签页。
    fn launch(&self, id: &str, script: &str) -> (SessionId, Attachment) {
        let id = self.host.spawn(id, script);
        let mut att = self.host.attach(&id, 60, 12);
        let mut seen = Seen::new(60, 12);
        read_snapshot(&mut att, &mut seen);
        read_until(&mut att, &mut seen, "假 CLI 起来", |s| {
            s.live_text().contains("ready")
        });
        (id, att)
    }

    /// 拉起后关掉标签页（对话照跑），等守护进程认到没人连着。
    fn launch_detached(&self, id: &str, script: &str) -> SessionId {
        let (id, att) = self.launch(id, script);
        drop(att);
        self.host
            .wait_info(&id, "标签页断开", |i| i.is_some_and(|i| !i.attached));
        id
    }

    fn alive(&self, id: &SessionId) -> bool {
        self.host.info(id).is_some_and(|i| i.pid.is_some())
    }

    fn running_ids(&self) -> Vec<SessionId> {
        interrupt::load(&interrupt::running_path(&self.host.dir))
            .unwrap()
            .into_keys()
            .collect()
    }
}

/// 收到 Ctrl+C（SIGINT）就打一行再正常退出；平时不读输入、不输出。
const POLITE: &str = "trap 'echo bye; exit 0' INT; printf 'ready\\a'; while :; do sleep 0.05; done";

fn wait_exit(events: &mut EventStream, id: &SessionId) -> Option<i32> {
    let got = wait_event(
        events,
        "对话退出",
        |e| matches!(e, HostEvent::Exited { id: x, .. } if x == id),
    );
    match got.last() {
        Some(HostEvent::Exited { code, .. }) => *code,
        _ => unreachable!(),
    }
}

fn screen_text(h: &IdleHost, id: &SessionId) -> String {
    match host::snapshot(&h.host.dir, id, None).unwrap() {
        Snapshot::Screen(s) => s.lines.iter().flatten().map(|r| r.text.as_str()).collect(),
        Snapshot::Unchanged => unreachable!("没带版本号"),
    }
}

/// 一段真实时间内不该退出。
fn stays_alive(h: &IdleHost, id: &SessionId, d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until {
        assert!(h.alive(id), "{id} 不该被结束");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn idle_detached_session_is_reclaimed_and_not_interrupted() {
    let h = IdleHost::start(30);
    let mut events = h.host.subscribe();
    let id = h.launch_detached("idle", POLITE);
    assert_eq!(h.running_ids(), vec![id.clone()]);

    h.advance(29 * MIN);
    assert!(h.daemon.reclaim_idle().is_empty(), "不到 30 分钟不收");

    h.advance(MIN);
    assert_eq!(h.daemon.reclaim_idle(), vec![id.clone()]);
    // 没快进到 SIGTERM 那一步：靠 Ctrl+C 自己正常退出（trap 走到 exit 0）。
    assert_eq!(wait_exit(&mut events, &id), Some(0));
    assert!(
        h.host.info(&id).is_none(),
        "退出后取消登记，列表里是普通「已停」"
    );

    // 收回不算被中断：running.json 里没有它，守护进程重启接管也不会转进被中断。
    assert!(h.running_ids().is_empty());
    assert!(interrupt::take_over(&h.host.dir).unwrap().is_empty());

    // 下次回车照常接着聊：同一条对话能重新登记、拉起。
    let (again, _att) = h.launch(id.as_str(), POLITE);
    assert!(h.alive(&again));
}

#[test]
fn attached_session_is_kept() {
    let h = IdleHost::start(30);
    let (id, _att) = h.launch("attached", POLITE);
    h.advance(120 * MIN);
    assert!(h.daemon.reclaim_idle().is_empty());
    stays_alive(&h, &id, Duration::from_millis(300));
}

#[test]
fn running_session_is_kept() {
    let h = IdleHost::start(30);
    // 从没发过提醒信号：一直算在跑。
    let silent = h.launch_detached(
        "silent",
        "trap 'exit 0' INT; printf ready; while :; do sleep 0.05; done",
    );
    // 发过信号，过了收尾重画的时间又有输出（开始干活了），之后一直没输出。
    let busy = h.launch_detached(
        "busy",
        "trap 'exit 0' INT; printf 'ready\\a'; while [ ! -f go ]; do sleep 0.05; done; printf working; while :; do sleep 0.05; done",
    );
    h.advance(Duration::from_secs(5));
    h.host.file("go", b"");
    // Output 事件每秒最多一条，可能被刚才的 ready 吃掉；看画面。
    let deadline = Instant::now() + WAIT;
    while !screen_text(&h, &busy).contains("working") {
        assert!(Instant::now() < deadline, "等不到 busy 开始干活");
        thread::sleep(Duration::from_millis(20));
    }

    h.advance(120 * MIN);
    assert!(h.daemon.reclaim_idle().is_empty());
    stays_alive(&h, &silent, Duration::from_millis(300));
    assert!(h.alive(&busy));
}

#[test]
fn recent_output_is_kept() {
    let h = IdleHost::start(30);
    let id = h.launch_detached(
        "recent",
        "trap 'exit 0' INT; printf 'ready\\a'; while [ ! -f go ]; do sleep 0.05; done; printf '\\a'; while :; do sleep 0.05; done",
    );
    let mut events = h.host.subscribe();
    h.advance(20 * MIN);
    h.host.file("go", b"");
    wait_event(
        &mut events,
        "又响铃",
        |e| matches!(e, HostEvent::Bell { id: x } if *x == id),
    );
    // 距最后一次输出才 15 分钟。
    h.advance(15 * MIN);
    assert!(h.daemon.reclaim_idle().is_empty());
    h.advance(15 * MIN);
    assert_eq!(h.daemon.reclaim_idle(), vec![id]);
}

#[test]
fn zero_minutes_turns_reclaim_off() {
    let h = IdleHost::start(0);
    let id = h.launch_detached("off", POLITE);
    h.advance(10_000 * MIN);
    assert!(h.daemon.reclaim_idle().is_empty());
    stays_alive(&h, &id, Duration::from_millis(300));
}

#[test]
fn ignored_ctrl_c_escalates_to_sigterm_then_sigkill() {
    let h = IdleHost::start(30);
    let mut events = h.host.subscribe();
    let term = h.launch_detached(
        "term",
        "trap '' INT; printf 'ready\\a'; while :; do sleep 0.05; done",
    );
    let stubborn = h.launch_detached(
        "stubborn",
        "trap '' INT TERM; printf 'ready\\a'; while :; do sleep 0.05; done",
    );
    h.advance(31 * MIN);
    assert_eq!(
        h.daemon.reclaim_idle(),
        vec![stubborn.clone(), term.clone()]
    );
    // 还在收回中：再查一遍不重复收。
    assert!(h.daemon.reclaim_idle().is_empty());
    // Ctrl+C 不管用，时钟没到不发 SIGTERM。
    stays_alive(&h, &term, Duration::from_secs(1));
    assert!(h.alive(&stubborn));

    h.advance(QUIT_GRACE);
    assert_eq!(wait_exit(&mut events, &term), None, "被 SIGTERM 结束");
    stays_alive(&h, &stubborn, Duration::from_millis(500));

    h.advance(TERM_GRACE);
    assert_eq!(wait_exit(&mut events, &stubborn), None, "被 SIGKILL 结束");
    assert!(h.running_ids().is_empty());
}

#[test]
fn reattach_during_reclaim_cancels_it() {
    let h = IdleHost::start(30);
    let id = h.launch_detached(
        "back",
        "trap 'printf got-int' INT; printf 'ready\\a'; while :; do sleep 0.05; done",
    );
    h.advance(31 * MIN);
    assert_eq!(h.daemon.reclaim_idle(), vec![id.clone()]);
    // 用户在 SIGTERM 之前回来了。
    let _att = h.host.attach(&id, 60, 12);
    h.host
        .wait_info(&id, "标签页接上", |i| i.is_some_and(|i| i.attached));
    h.advance(QUIT_GRACE + TERM_GRACE);
    stays_alive(&h, &id, Duration::from_secs(1));
}

#[test]
fn redraw_after_reattach_resize_is_still_idle() {
    let h = IdleHost::start(30);
    let id = h.launch_detached(
        "peek",
        "trap 'exit 0' INT; trap 'printf redraw' WINCH; printf 'ready\\a'; while :; do sleep 0.05; done",
    );
    // 过了一阵回来看一眼：窗口大小不同，CLI 跟着重画；看完直接关标签页。
    h.advance(10 * MIN);
    let mut att = h.host.attach(&id, 80, 20);
    let mut seen = Seen::new(80, 20);
    read_snapshot(&mut att, &mut seen);
    read_until(&mut att, &mut seen, "CLI 重画", |s| {
        s.live_text().contains("redraw")
    });
    drop(att);
    h.host
        .wait_info(&id, "标签页断开", |i| i.is_some_and(|i| !i.attached));

    h.advance(29 * MIN);
    assert!(
        h.daemon.reclaim_idle().is_empty(),
        "从重画那次输出算起不到 30 分钟"
    );
    h.advance(MIN);
    assert_eq!(h.daemon.reclaim_idle(), vec![id]);
}

#[test]
fn registrations_never_attached_expire_so_an_old_daemon_can_step_aside() {
    let h = IdleHost::start(0);
    let mut events = h.host.subscribe();
    let busy = h.launch_detached("busy", POLITE);
    // 会话台登记了、标签页却没开出来（开标签页失败、拒绝了自动化授权）。
    let stale = h.host.spawn("stale", "exit 0");
    let renewed = h.host.spawn("renewed", "exit 0");
    h.advance(9 * MIN);
    assert!(h.daemon.expire_pending().is_empty(), "不到 10 分钟不撤");
    // 同一条又登记一次（重按回车）：从这次算起。
    h.host.spawn("renewed", "exit 0");
    h.advance(MIN);
    assert_eq!(h.daemon.expire_pending(), vec![stale.clone()]);
    assert!(h.host.info(&stale).is_none());
    assert!(host::attach(&h.host.dir, &stale, fake_env(&h.host.dir), 60, 12).is_err());
    assert!(h.alive(&busy), "在跑的不撤");
    assert!(h.host.info(&renewed).is_some());

    // 只剩待启动的过期登记时，旧版守护进程能让位（Retire 不再说「还托管着」）。
    assert_eq!(
        h.host.request(&Request::Kill { id: busy.clone() }),
        Response::Ok
    );
    wait_exit(&mut events, &busy);
    h.advance(10 * MIN);
    assert_eq!(h.daemon.expire_pending(), vec![renewed]);
    assert_eq!(h.host.request(&Request::Retire), Response::Ok);
}

/// 像 pi：答完不响铃，从没发过提醒信号。会话台凭厂商记录说这一轮答完了，发 `Reclaim`。
const QUIET_CLI: &str = "trap 'echo bye; exit 0' INT; printf ready; while [ ! -f go ]; do sleep 0.05; done; printf more; while :; do sleep 0.05; done";

fn reclaim(h: &IdleHost, id: &SessionId) -> Response {
    h.host.request(&Request::Reclaim { id: id.clone() })
}

#[test]
fn record_says_done_reclaims_a_quiet_cli_after_the_limit() {
    let h = IdleHost::start(30);
    let mut events = h.host.subscribe();
    let id = h.launch_detached("quiet", QUIET_CLI);
    // 守护进程自己不收没发过信号的。
    h.advance(120 * MIN);
    assert!(h.daemon.reclaim_idle().is_empty());
    assert_eq!(reclaim(&h, &id), Response::Ok);
    assert_eq!(wait_exit(&mut events, &id), Some(0), "两下 Ctrl+C 正常退出");
    assert!(h.host.info(&id).is_none());
    assert!(h.running_ids().is_empty(), "收回不算被中断");
    assert!(interrupt::take_over(&h.host.dir).unwrap().is_empty());
}

#[test]
fn record_says_done_but_pty_recent_or_attached_or_off_is_kept() {
    let h = IdleHost::start(30);
    let mut events = h.host.subscribe();
    let id = h.launch_detached("recent", QUIET_CLI);
    h.advance(29 * MIN);
    assert!(
        matches!(reclaim(&h, &id), Response::Error(_)),
        "不到 30 分钟不收"
    );

    // 之后又有输出：从那时重新算。
    h.host.file("go", b"");
    let deadline = Instant::now() + WAIT;
    while !screen_text(&h, &id).contains("more") {
        assert!(Instant::now() < deadline, "等不到新输出");
        thread::sleep(Duration::from_millis(20));
    }
    h.advance(15 * MIN);
    assert!(matches!(reclaim(&h, &id), Response::Error(_)));
    stays_alive(&h, &id, Duration::from_millis(300));

    // 有标签页连着不收。
    let att = h.host.attach(&id, 60, 12);
    h.host
        .wait_info(&id, "标签页接上", |i| i.is_some_and(|i| i.attached));
    h.advance(120 * MIN);
    assert!(matches!(reclaim(&h, &id), Response::Error(_)));
    stays_alive(&h, &id, Duration::from_millis(300));
    drop(att);
    h.host
        .wait_info(&id, "标签页断开", |i| i.is_some_and(|i| !i.attached));

    // 设置里关了（0）不收。
    Config {
        idle_reclaim_minutes: 0,
        ..Config::default()
    }
    .save(&h.host.dir.join("config.json"))
    .unwrap();
    h.advance(120 * MIN);
    assert!(matches!(reclaim(&h, &id), Response::Error(_)));
    stays_alive(&h, &id, Duration::from_millis(300));

    // 打开、没人连着、够久：收。
    Config {
        idle_reclaim_minutes: 30,
        ..Config::default()
    }
    .save(&h.host.dir.join("config.json"))
    .unwrap();
    assert_eq!(reclaim(&h, &id), Response::Ok);
    assert_eq!(wait_exit(&mut events, &id), Some(0));
}

#[test]
fn reclaim_unknown_session_is_an_error() {
    let h = IdleHost::start(30);
    assert!(matches!(
        reclaim(&h, &SessionId::from("nope")),
        Response::Error(_)
    ));
}
