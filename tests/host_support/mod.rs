//! host 集成测试的公共部分：进程内起守护进程、用 sh 脚本当假 CLI、读转发通道直到满足条件。
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use huitai::core::{SessionId, Vendor};
use huitai::host::{
    self, AttachMsg, Attachment, Daemon, DaemonHandle, EventStream, HostEvent, HostedInfo, Request,
    Response, Screen, SpawnSpec,
};

pub const WAIT: Duration = Duration::from_secs(10);
pub const COLS: u16 = 60;
pub const ROWS: u16 = 12;

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// 进程内的守护进程，数据目录在 /tmp 下（socket 路径不能太长）。drop 时结束所有对话、删目录。
pub struct TestHost {
    pub dir: PathBuf,
    /// 进程内接口（守护进程的提醒循环在测试里不跑，状态由测试自己设）。
    pub handle: DaemonHandle,
}

impl TestHost {
    pub fn start() -> Self {
        let dir = PathBuf::from(format!(
            "/tmp/hta-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        let daemon = Daemon::bind(&dir).expect("守护进程监听失败");
        let handle = daemon.handle();
        thread::spawn(move || daemon.run());
        Self { dir, handle }
    }

    pub fn request(&self, req: &Request) -> Response {
        host::request(&self.dir, req).expect("请求失败")
    }

    /// 登记一条假 CLI。`script` 在数据目录下用 /bin/sh 执行。
    pub fn spawn(&self, id: &str, script: &str) -> SessionId {
        let id = SessionId::from(id);
        let spec = SpawnSpec {
            id: id.clone(),
            vendor: Vendor::Claude,
            project: "demo".to_owned(),
            cwd: self.dir.clone(),
            command: script.to_owned(),
        };
        assert_eq!(self.request(&Request::Spawn(spec)), Response::Ok);
        id
    }

    /// 写一个字节文件给假 CLI 用 `cat` 输出。
    pub fn file(&self, name: &str, bytes: &[u8]) {
        fs::write(self.dir.join(name), bytes).unwrap();
    }

    pub fn attach(&self, id: &SessionId, cols: u16, rows: u16) -> Attachment {
        host::attach(&self.dir, id, fake_env(&self.dir), cols, rows).expect("attach 失败")
    }

    pub fn list(&self) -> Vec<HostedInfo> {
        match self.request(&Request::List) {
            Response::List(l) => l,
            other => panic!("List 应答不对：{other:?}"),
        }
    }

    pub fn info(&self, id: &SessionId) -> Option<HostedInfo> {
        self.list().into_iter().find(|i| &i.id == id)
    }

    pub fn subscribe(&self) -> EventStream {
        host::subscribe(&self.dir).expect("订阅失败")
    }

    /// 等到 List 里这条对话满足条件。
    pub fn wait_info(
        &self,
        id: &SessionId,
        what: &str,
        pred: impl Fn(Option<&HostedInfo>) -> bool,
    ) {
        let deadline = Instant::now() + WAIT;
        while !pred(self.info(id).as_ref()) {
            assert!(Instant::now() < deadline, "等不到：{what}");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for TestHost {
    fn drop(&mut self) {
        if let Ok(Response::List(list)) = host::request(&self.dir, &Request::List) {
            for i in list {
                let _ = host::request(&self.dir, &Request::Kill { id: i.id });
            }
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// 假终端环境：sh 当登录 shell，HOME 指到测试目录，不读用户的 shell 配置。
/// 由它拉起的守护进程不注册全局呼出键、不弹系统通知（改写进 `notifications_log(home)`）。
pub fn fake_env(home: &Path) -> Vec<(String, String)> {
    let log = notifications_log(home);
    [
        ("SHELL", "/bin/sh"),
        ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"),
        ("TERM", "xterm-256color"),
        ("LANG", "en_US.UTF-8"),
        ("HOME", home.to_str().unwrap()),
        ("HUITAI_NO_HOTKEY", "1"),
        ("HUITAI_NOTIFY_TO", log.to_str().unwrap()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

/// `fake_env(home)` 拉起的守护进程把系统通知写到这里。
pub fn notifications_log(home: &Path) -> PathBuf {
    home.join("notifications.log")
}

/// 客户端这边看到的东西：原始字节（第一帧是补发画面，单独放）和喂出来的画面。
pub struct Seen {
    pub snapshot: Vec<u8>,
    pub live: Vec<u8>,
    pub screen: Screen,
    pub end: Option<AttachMsg>,
}

impl Seen {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            snapshot: Vec::new(),
            live: Vec::new(),
            screen: Screen::new(cols, rows),
            end: None,
        }
    }

    pub fn live_text(&self) -> String {
        String::from_utf8_lossy(&self.live).into_owned()
    }
}

/// 读第一帧（补发画面）。
pub fn read_snapshot(att: &mut Attachment, seen: &mut Seen) {
    match att.recv(Some(WAIT)).expect("等补发画面超时") {
        AttachMsg::Output(bytes) => {
            seen.screen.feed(&bytes);
            seen.snapshot = bytes;
        }
        other => panic!("第一帧应是补发画面，收到 {other:?}"),
    }
}

/// 一直读到 `pred` 成立；转发通道结束时把结束消息记进 `seen.end` 再判断一次。
pub fn read_until(att: &mut Attachment, seen: &mut Seen, what: &str, pred: impl Fn(&Seen) -> bool) {
    let deadline = Instant::now() + WAIT;
    while !pred(seen) {
        assert!(
            seen.end.is_none(),
            "通道已结束（{:?}），等不到：{what}",
            seen.end
        );
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "超时，等不到：{what}；已收到 {:?}",
            seen.live_text()
        );
        match att.recv(Some(left)) {
            Ok(AttachMsg::Output(bytes)) => {
                seen.screen.feed(&bytes);
                seen.live.extend_from_slice(&bytes);
            }
            Ok(msg) => seen.end = Some(msg),
            Err(e) => panic!(
                "读转发通道出错：{e}；等的是：{what}；已收到 {:?}",
                seen.live_text()
            ),
        }
    }
}

/// 等到某个事件出现，返回之前收到的全部事件（含它）。
pub fn wait_event(
    events: &mut EventStream,
    what: &str,
    pred: impl Fn(&HostEvent) -> bool,
) -> Vec<HostEvent> {
    let deadline = Instant::now() + WAIT;
    let mut got = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "等不到事件：{what}；已收到 {got:?}");
        match events.next(Some(left)) {
            Ok(Some(e)) => {
                let hit = pred(&e);
                got.push(e);
                if hit {
                    return got;
                }
            }
            Ok(None) => panic!("事件流断了；已收到 {got:?}"),
            Err(e) => panic!("读事件出错：{e}；等的是：{what}；已收到 {got:?}"),
        }
    }
}

pub fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

pub const AI: &str = env!("CARGO_BIN_EXE_ai");

/// 用真的 `ai` 二进制：HOME 指到短的临时目录，`ai` 的数据目录就落在它下面。drop 时结束守护进程、删目录。
pub struct Home {
    pub home: PathBuf,
    pub data: PathBuf,
    pub daemon_pid: Option<u32>,
}

impl Home {
    pub fn new(tag: &str) -> Self {
        let home = PathBuf::from(format!("/tmp/htl-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let data = home.join("Library/Application Support/huitai");
        Self {
            home,
            data,
            daemon_pid: None,
        }
    }

    pub fn ai(&self) -> Command {
        let mut cmd = Command::new(AI);
        cmd.env_clear().envs(fake_env(&self.home));
        cmd
    }

    pub fn ensure_daemon(&mut self) -> u32 {
        let mut launch = self.ai();
        launch.arg("daemon");
        host::ensure_daemon(&self.data, launch).expect("拉起守护进程失败");
        let Response::Pong { pid } = host::request(&self.data, &Request::Ping).unwrap() else {
            panic!("Ping 应答不对");
        };
        self.daemon_pid = Some(pid);
        pid
    }

    /// `kill -9` 守护进程（模拟断电、崩溃：来不及收尾）并等它真的没了。
    pub fn kill_daemon_hard(&mut self) {
        let pid = self.daemon_pid.take().expect("守护进程没起") as libc::pid_t;
        // SAFETY: 只给测试自己拉起的守护进程发信号；它是本进程的子进程，waitpid 收尸。
        unsafe {
            assert_eq!(libc::kill(pid, libc::SIGKILL), 0);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon_pid {
            // SAFETY: 只给测试自己拉起的守护进程发 SIGTERM。
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        let _ = fs::remove_dir_all(&self.home);
    }
}
