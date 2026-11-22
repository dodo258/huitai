//! 守护进程：持有每条对话的 PTY，接 socket 请求，转发字节，广播状态事件。
//!
//! 进程内的调用方（`ai daemon` 的通知和热键）用 [`DaemonHandle`] 订阅事件、找会话台、请它代弹提醒。
//! 另有一个线程每隔 [`idle::SWEEP_EVERY`] 收回闲置的对话（见 [`idle`]）。
//!
//! 升级：每条连接先握手（见 [`protocol`]）。比自己新的客户端连过之后，最后一条托管对话结束时守护进程自己退出
//! （`run` 返回），下次由新版客户端拉起新版；新版客户端也可以发 `Retire` 让没有托管对话的它马上退出。
//!
//! 线程模型：每个连接一个线程；每条已启动的对话一个读线程（PTY 输出 → 扫描 → 状态机 → 当前客户端）、
//! 一个写线程（排队的输入 → PTY）和一个等待线程（回收子进程）；每个 attach 的客户端一个写线程（排队的输出 → socket）。
//! 持着单条对话的 `state` 锁时只往 [`Outbox`] 里放，不直接写 PTY 或 socket（对面卡住时会一直阻塞）。锁的顺序：先 `sessions` 再单条对话的 `state`，不反过来。

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use super::failpoint;
use super::idle::{self, Activity, Clock};
use super::interrupt::{self, Records};
use super::outbox::Outbox;
use super::power::PowerAssertion;
use super::protocol::{
    self, BoardInfo, Build, EndReason, HelloReply, HostEvent, HostedInfo, KIND_END, KIND_EVENT,
    KIND_FOCUS, KIND_INPUT, KIND_OUTPUT, KIND_REQUEST, KIND_RESIZE, KIND_RESPONSE,
    MIN_CLIENT_PROTOCOL, OLD_CLIENT, Request, Response, RunRecord, SpawnSpec,
};
use super::pty;
use super::scan::{Scanner, Signal, Status};
use super::screen::Screen;
use super::version;
use crate::core::{Config, SessionId};

/// 写给客户端卡住超过这么久就断开它（对话不受影响）。
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// 客户端读得慢、积压的实时输出超过这么多（补发画面另算）就断开它：CLI 和别的对话不被一个标签页拖住，
/// 守护进程的内存也有上限。断开后重新 attach 靠补发画面恢复。
const CLIENT_BACKLOG: usize = 16 << 20;
/// 排队的输入超过这么多字节，客户端就先等 CLI 读走一些；终端查询的代答直接丢掉。
const INPUT_BACKLOG: usize = 1 << 20;
/// 同一条对话的 Output 事件最短间隔。
const OUTPUT_EVENT_GAP: Duration = Duration::from_secs(1);
/// 子进程退出后等读线程把剩余输出读完的时间；超时就给进程组发 SIGHUP 收掉占着从端的后台进程。
const DRAIN_GRACE: Duration = Duration::from_millis(500);
/// Kill 先发 SIGHUP，这么久还没退就 SIGKILL。
const KILL_GRACE: Duration = Duration::from_secs(3);
/// 没有事件时每隔这么久看一眼订阅方是不是已经关了连接。
const SUBSCRIBER_CHECK: Duration = Duration::from_secs(1);
/// 收回时多久看一眼进程退了没有、时钟到了没有。
const RECLAIM_POLL: Duration = Duration::from_millis(50);
/// 补发画面按这个大小分帧发：1 万行历史、每格一种颜色时整段会超过单帧上限。
const RESTORE_CHUNK: usize = 1 << 20;
/// 连上之后这么久还没发完握手和请求就断开：连上不说话、发半帧的连接不一直占着线程和描述符。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// 接受连接遇到描述符/内存暂时用光时，歇这么久再接（不退出：退出会带走所有托管的对话）。
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// 窗口大小上限。客户端报的大小直接决定状态机分配多少格，坏数据（65535×65535）会把守护进程撑爆；
/// 真实终端远小于它（5K 屏小字号也就几百列）。
const MAX_COLS: u16 = 1000;
const MAX_ROWS: u16 = 500;
/// 登记了这么久还没有标签页接上（开标签页失败、被拒绝自动化授权）就撤掉：不一直占着登记表、挡着旧版守护进程让位。
/// 留够人在系统弹窗里点「允许」、登录 shell 慢慢起来的时间。
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
/// 重开的被中断对话起来不到这么久就非正常退出（找不到 CLI、一启动就崩），放回「被中断」，用户还能再试。
const RESUME_GRACE: Duration = Duration::from_secs(10);

/// 数据目录下的文件。
pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("host.sock")
}

fn lock_path(dir: &Path) -> PathBuf {
    dir.join("host.lock")
}

/// 建数据目录并把权限收成 0700。
pub fn prepare_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir).map_err(|e| with_path(e, dir))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| with_path(e, dir))
}

fn with_path(e: io::Error, path: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 起一个带名字的线程（panic 信息里带名字，好查）。线程体 panic 只结束这条线程：写一行日志说明是哪条，
/// 不往外传；系统起不了线程时返回错误（`thread::spawn` 会直接 panic）。
fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    let label = name.to_owned();
    thread::Builder::new().name(label.clone()).spawn(move || {
        if let Err(p) = panic::catch_unwind(AssertUnwindSafe(f)) {
            eprintln!(
                "huitai daemon: 线程 {label} panic 退出：{}",
                panic_message(&*p)
            );
        }
    })?;
    Ok(())
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> &str {
    p.downcast_ref::<&str>()
        .copied()
        .or_else(|| p.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("（不是文字）")
}

/// 客户端报的窗口大小，过大的截到上限。
fn sane_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.min(MAX_COLS), rows.min(MAX_ROWS))
}

/// 接受连接时这些错误是暂时的（描述符或内存用光、对面连上又断了），歇一下接着接。
fn transient_accept_error(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM | libc::ECONNABORTED)
    )
}

pub struct Daemon {
    listener: UnixListener,
    shared: Arc<Shared>,
    /// 持有期间别的守护进程起不来（flock）。
    _lock: File,
}

#[derive(Default)]
struct Shared {
    /// 数据目录（`running.json`、`interrupted.json` 放这里）。
    dir: PathBuf,
    /// 有进程在跑的对话，每次变化都原子写进 `running.json`。锁的顺序：可以在持有 `sessions`、单条对话的 `state` 时拿，反过来不行。
    running: Mutex<Records>,
    /// 上一个守护进程没收尾时还在跑的对话，存 `interrupted.json`。锁的顺序同 `running`，且在它之后拿。
    interrupted: Mutex<Records>,
    sessions: Mutex<HashMap<SessionId, Arc<Hosted>>>,
    subscribers: Mutex<Vec<Sender<HostEvent>>>,
    /// 开着的会话台，后连上的在后面。
    boards: Mutex<Vec<Board>>,
    /// 在跑的对话数和防休眠断言。
    power: Mutex<(usize, Option<PowerAssertion>)>,
    next_client: AtomicU64,
    /// 闲置收回用的时间（测试换成假时钟）。
    clock: Clock,
    /// 自己是哪一版（握手时报给客户端）。
    build: Build,
    /// 有比自己新的客户端连过：最后一条托管对话结束时退出。
    retire_when_idle: AtomicBool,
    /// 已决定退出：不再接新对话，接收循环醒来就返回。只在持有 `sessions` 锁时置位。
    retired: AtomicBool,
}

/// 一个开着的会话台连接。
struct Board {
    id: u64,
    info: BoardInfo,
    out: Arc<Mutex<UnixStream>>,
}

struct Hosted {
    spec: SpawnSpec,
    /// 标题前缀：`<厂商标记> <项目名> · `。
    prefix: String,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    run: Option<Running>,
    client: Option<Client>,
    title: Option<String>,
    last_output_event: Option<Instant>,
    /// 最近一次登记（Spawn）的时间，判断待启动的登记是否过期。
    registered: Option<Instant>,
    /// 已从登记表注销（进程结束、取消、过期）：拿着旧引用的 Attach 不再启动它。只在同时持有 `sessions` 锁时置位。
    ended: bool,
}

struct Running {
    master: Arc<File>,
    /// 发往 PTY 的输入都走这里，不直接写 `master`（CLI 不读输入时直接写会卡住）。
    input: Arc<Outbox>,
    pid: u32,
    screen: Screen,
    scanner: Scanner,
    /// 最近的输出和提醒信号，判断闲置用。
    activity: Activity,
    /// 正在收回（已经按了退出键），不再重复收；有标签页接上时清掉，收回随之停手。
    reclaiming: bool,
    /// 用户要结束这条对话（Kill）。
    killed: bool,
    /// 这次启动从「被中断」里拿掉的记录，启动失败时放回去。
    resumed: Option<RunRecord>,
    started: Instant,
}

impl Running {
    /// 正在结束这条对话（结束或收回）：CLI 退出后，它留下的不理 SIGHUP 的进程也一并 SIGKILL。
    fn ending(&self) -> bool {
        self.killed || self.reclaiming
    }
}

/// 正接着一条对话的客户端。发给它的帧都进 `out`，由它自己的写线程写进 socket；
/// 写线程结束（写不进去、超时、被结束）时关掉 socket，客户端那边的输入线程随之退出。
struct Client {
    id: u64,
    out: Arc<Outbox>,
}

impl Client {
    /// `restore` 是要先发的补发画面字节数，积压上限在它之上再加 [`CLIENT_BACKLOG`]。
    fn new(id: u64, stream: &UnixStream, restore: usize) -> io::Result<Self> {
        let writer = stream.try_clone()?;
        writer.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))?;
        let closer = stream.try_clone()?;
        let out = Outbox::start(writer, restore + CLIENT_BACKLOG, move || {
            let _ = closer.shutdown(Shutdown::Both);
        });
        Ok(Self { id, out })
    }

    /// 实时输出：积压超过上限时不收，返回 `false`（调用方断开它）。
    fn send(&self, kind: u8, payload: &[u8]) -> bool {
        frame(kind, payload).is_some_and(|f| self.out.try_push(f))
    }

    /// 应答、补发画面、结束原因：不看上限。
    fn send_all(&self, kind: u8, payload: &[u8]) {
        if let Some(f) = frame(kind, payload) {
            self.out.force_push(f);
        }
    }

    /// 告诉客户端为什么结束，发完就断开。
    fn end(&self, reason: &EndReason) {
        if let Ok(json) = serde_json::to_vec(reason) {
            self.send_all(KIND_END, &json);
        }
        self.out.finish();
    }
}

fn frame(kind: u8, payload: &[u8]) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    protocol::write_frame(&mut buf, kind, payload).ok()?;
    Some(buf)
}

impl Drop for Client {
    /// 不管怎么不再接着（结束、断开、被换掉），写线程都要收尾，不留线程和描述符。
    fn drop(&mut self) {
        self.out.finish();
    }
}

/// 守护进程的进程内接口，可以拿到别的线程用。
#[derive(Clone)]
pub struct DaemonHandle(Arc<Shared>);

impl DaemonHandle {
    /// 订阅状态事件（和 socket 上的 `Events` 一样）。
    pub fn subscribe(&self) -> Receiver<HostEvent> {
        let (tx, rx) = mpsc::channel();
        lock(&self.0.subscribers).push(tx);
        rx
    }

    /// 最近打开、还连着的会话台。
    pub fn board(&self) -> Option<BoardInfo> {
        lock(&self.0.boards).last().map(|b| b.info.clone())
    }

    /// 托管中的对话。
    pub fn list(&self) -> Vec<HostedInfo> {
        self.0.list()
    }

    /// 马上查一遍闲置对话（平时由守护进程定时查），返回这次开始收回的对话。
    pub fn reclaim_idle(&self) -> Vec<SessionId> {
        self.0.reclaim_idle()
    }

    /// 马上撤掉过期的待启动登记（平时随闲置检查定时做），返回撤掉的对话。
    pub fn expire_pending(&self) -> Vec<SessionId> {
        self.0.expire_pending()
    }

    /// 请最近打开、能弹提醒的会话台代弹一条提醒。送到了返回 `true`；没有这样的会话台或都写不进去返回
    /// `false`（调用方改用系统通知）。写不进去的会话台从名单里去掉。
    pub fn alert(&self, id: &SessionId, title: &str, body: &str) -> bool {
        let event = HostEvent::Alert {
            id: id.clone(),
            title: title.to_owned(),
            body: body.to_owned(),
        };
        loop {
            let target = lock(&self.0.boards)
                .iter()
                .rev()
                .find(|b| b.info.alerts)
                .map(|b| (b.id, Arc::clone(&b.out)));
            let Some((board, out)) = target else {
                return false;
            };
            if protocol::write_json(&mut *lock(&out), KIND_EVENT, &event).is_ok() {
                return true;
            }
            let _ = lock(&out).shutdown(Shutdown::Both);
            lock(&self.0.boards).retain(|b| b.id != board);
        }
    }
}

impl Daemon {
    /// 占住数据目录并监听 socket。已有守护进程时返回 `AddrInUse`。
    pub fn bind(dir: &Path) -> io::Result<Self> {
        Self::bind_with_clock(dir, Clock::default())
    }

    /// 同 [`Daemon::bind`]，闲置收回用给定的时钟。
    pub fn bind_with_clock(dir: &Path, clock: Clock) -> io::Result<Self> {
        prepare_dir(dir)?;
        // 终端默认只给 256 个描述符，每条对话、每个标签页都要占几个，并发连接一多就接不了。
        pty::raise_fd_limit();
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(lock_path(dir))?;
        // SAFETY: 描述符有效；flock 只作用于这个打开的文件。
        if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                io::Error::new(ErrorKind::AddrInUse, "守护进程已经在跑")
            } else {
                e
            });
        }
        let sock = socket_path(dir);
        // 拿到锁说明之前的守护进程已经不在了，留下的 socket 文件是旧的。
        match fs::remove_file(&sock) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(with_path(e, &sock)),
            _ => {}
        }
        let listener = UnixListener::bind(&sock).map_err(|e| with_path(e, &sock))?;
        fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;
        // 拿到锁说明上一个守护进程已经不在：它留在 running.json 里的对话是被中断的。
        let interrupted = interrupt::take_over(dir)?;
        Ok(Self {
            listener,
            shared: Arc::new(Shared {
                dir: dir.to_path_buf(),
                interrupted: Mutex::new(interrupted),
                clock,
                build: version::current().clone(),
                ..Shared::default()
            }),
            _lock: lock_file,
        })
    }

    pub fn handle(&self) -> DaemonHandle {
        DaemonHandle(Arc::clone(&self.shared))
    }

    /// 一直接连接，直到监听出错，或者（升级时）托管对话都结束了要让位给新版：这时返回 `Ok`，调用方退出进程。
    /// 同时定时收回闲置对话。
    pub fn run(self) -> io::Result<()> {
        let shared = Arc::clone(&self.shared);
        spawn("huitai-sweep", move || {
            loop {
                thread::sleep(idle::SWEEP_EVERY);
                // 这一轮出了意外也接着查下一轮，不让收回从此停掉。
                let sweep = || {
                    shared.expire_pending();
                    shared.reclaim_idle();
                };
                if let Err(p) = panic::catch_unwind(AssertUnwindSafe(sweep)) {
                    eprintln!("huitai daemon: 查闲置对话时 panic：{}", panic_message(&*p));
                }
            }
        })?;
        loop {
            let (stream, _) = match self.listener.accept() {
                Ok(c) => c,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if transient_accept_error(&e) => {
                    eprintln!("huitai daemon: 暂时接不了新连接，稍后再接: {e}");
                    thread::sleep(ACCEPT_BACKOFF);
                    continue;
                }
                Err(e) => return Err(e),
            };
            if self.shared.retired.load(Ordering::Acquire) {
                // 先不再接连接（监听随 self 关掉），再断开订阅方，它们重连时就连不上这个旧的了。
                drop(stream);
                self.shared.hang_up_subscribers();
                return Ok(());
            }
            let shared = Arc::clone(&self.shared);
            // 起不了线程时这条连接随闭包一起关掉，客户端看到连接断开；接收循环照常。
            if let Err(e) = spawn("huitai-conn", move || {
                if let Err(e) = handle(&shared, stream) {
                    eprintln!("huitai daemon: 连接处理出错: {e}");
                }
            }) {
                eprintln!("huitai daemon: 起不了线程，这条连接不处理: {e}");
            }
        }
    }
}

fn reply(stream: &mut UnixStream, r: Response) -> io::Result<()> {
    protocol::write_json(stream, KIND_RESPONSE, &r)
}

/// 一条连接：先握手，再处理一个请求。握手和请求要在 [`REQUEST_TIMEOUT`] 内发完；
/// 应答写不出去（客户端不读）也最多卡 [`CLIENT_WRITE_TIMEOUT`]。
fn handle(shared: &Arc<Shared>, mut stream: UnixStream) -> io::Result<()> {
    let timeouts = stream
        .set_read_timeout(Some(REQUEST_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT)));
    match timeouts {
        // 对面已经关了（macOS 的 setsockopt 这时返回 EINVAL）：没什么要处理的。
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => return Ok(()),
        other => other?,
    }
    let Some(first) = read_request(&mut stream)? else {
        return Ok(());
    };
    let request = if shared.build.protocol == 0 {
        // 装成没有握手的旧版（测试用）：和旧版一样读不懂 Hello 和 Retire，直接关连接。
        if matches!(first, Request::Hello(_) | Request::Retire) {
            return Err(io::Error::new(ErrorKind::InvalidData, "不认识的请求"));
        }
        first
    } else {
        let Request::Hello(peer) = first else {
            // 没有握手的旧客户端：回一句它能显示的话。
            return reply(&mut stream, Response::Error(OLD_CLIENT.to_owned()));
        };
        if !shared.greet(&mut stream, &peer)? {
            return Ok(());
        }
        match read_request(&mut stream)? {
            Some(r) => r,
            None => return Ok(()),
        }
    };
    serve(shared, stream, request)
}

/// 读一个请求。连接关了返回 `None`；不是请求帧或读不懂（更新的客户端加的请求）时回一句错误，也返回 `None`。
fn read_request(stream: &mut UnixStream) -> io::Result<Option<Request>> {
    let Some(frame) = protocol::read_frame(stream)? else {
        return Ok(None);
    };
    let parsed = (frame.kind == KIND_REQUEST)
        .then(|| frame.json::<Request>())
        .and_then(Result::ok);
    if parsed.is_none() {
        reply(
            stream,
            Response::Error("后台的守护进程不认这个请求（版本不同），重新打开 ai 试试".to_owned()),
        )?;
    }
    Ok(parsed)
}

fn serve(shared: &Arc<Shared>, mut stream: UnixStream, request: Request) -> io::Result<()> {
    if matches!(
        request,
        Request::Attach { .. } | Request::Board(_) | Request::Events
    ) {
        // 长连接：之后客户端可以一直不说话（标签页没人打字、会话台只收不发）。
        stream.set_read_timeout(None)?;
    }
    match request {
        Request::Hello(peer) => {
            shared.greet(&mut stream, &peer)?;
            Ok(())
        }
        Request::Retire => {
            let (r, retire) = shared.retire_if_idle();
            reply(&mut stream, r)?;
            if retire {
                shared.wake();
            }
            Ok(())
        }
        Request::Ping => reply(
            &mut stream,
            Response::Pong {
                pid: std::process::id(),
            },
        ),
        Request::Spawn(spec) => {
            let r = shared.register(spec);
            reply(&mut stream, r)
        }
        Request::Attach {
            id,
            env,
            cols,
            rows,
        } => attach(shared, stream, &id, &env, cols, rows),
        Request::Detach { id } => {
            let r = shared.detach(&id);
            reply(&mut stream, r)
        }
        Request::Kill { id } => {
            let r = shared.kill(&id);
            reply(&mut stream, r)
        }
        Request::List => {
            let r = Response::List(shared.list());
            reply(&mut stream, r)
        }
        Request::Snapshot { id, since } => {
            let r = shared.snapshot(&id, since);
            reply(&mut stream, r)
        }
        Request::Interrupted => {
            let list = lock(&shared.interrupted).values().cloned().collect();
            reply(&mut stream, Response::Interrupted(list))
        }
        Request::Dismiss { ids } => {
            let r = shared.dismiss(&ids);
            reply(&mut stream, r)
        }
        Request::Events => {
            // 订阅方卡住不读时断开它（会话台会重连），免得事件在这边越攒越多（写超时在 handle 里设好了）。
            let (tx, rx) = mpsc::channel();
            lock(&shared.subscribers).push(tx);
            reply(&mut stream, Response::Ok)?;
            forward_events(&mut stream, &rx);
            Ok(())
        }
        Request::Board(info) => board(shared, stream, info),
        Request::Seen { id } => {
            shared.emit(HostEvent::Seen { id });
            reply(&mut stream, Response::Ok)
        }
    }
}

/// 会话台的订阅：登记它，转发状态事件，直到它关掉连接。
fn board(shared: &Arc<Shared>, mut stream: UnixStream, info: BoardInfo) -> io::Result<()> {
    let out = Arc::new(Mutex::new(stream.try_clone()?));
    let id = shared.next_client.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    {
        // 先登记再应答，应答之后的事件一个不漏；持着写锁，事件和提醒都排在应答后面。
        let mut w = lock(&out);
        lock(&shared.subscribers).push(tx);
        lock(&shared.boards).push(Board {
            id,
            info,
            out: Arc::clone(&out),
        });
        protocol::write_json(&mut *w, KIND_RESPONSE, &Response::Ok)?;
    }
    let events_out = Arc::clone(&out);
    let gone = Arc::new(AtomicBool::new(false));
    let left = Arc::clone(&gone);
    // 会话台关掉后，没有新事件时也在 [`SUBSCRIBER_CHECK`] 内收掉这条线程和它拿着的描述符。
    let forwarded = spawn("huitai-board", move || {
        loop {
            match rx.recv_timeout(SUBSCRIBER_CHECK) {
                Ok(event) => {
                    let mut out = lock(&events_out);
                    if protocol::write_json(&mut *out, KIND_EVENT, &event).is_err() {
                        // 写超时（会话台停住不读）或写了半帧：关掉连接，等对端输入的连接线程随之收尾，
                        // 会话台读到断开会重连并重新取全量状态，不会一直停在旧值上。
                        let _ = out.shutdown(Shutdown::Both);
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) if left.load(Ordering::Acquire) => return,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    if forwarded.is_ok() {
        // 会话台不发东西，读到 EOF 就是它关了。
        while let Ok(Some(_)) = protocol::read_frame(&mut stream) {}
    }
    gone.store(true, Ordering::Release);
    lock(&shared.boards).retain(|b| b.id != id);
    let _ = stream.shutdown(Shutdown::Both);
    forwarded
}

/// 把事件写给订阅方，直到它关了连接（下一次 emit 会把它从名单里去掉）。
/// 没有事件时也每隔一会儿看一眼连接还在不在，免得关掉的订阅一直占着线程和描述符等下一个事件。
fn forward_events(stream: &mut UnixStream, rx: &Receiver<HostEvent>) {
    loop {
        match rx.recv_timeout(SUBSCRIBER_CHECK) {
            Ok(event) => {
                if protocol::write_json(stream, KIND_EVENT, &event).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if peer_closed(stream) {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// 对端关了连接（不取走数据，不阻塞）。
fn peer_closed(stream: &UnixStream) -> bool {
    let mut byte = 0u8;
    // SAFETY: 描述符有效，缓冲是 1 字节的局部变量；MSG_PEEK 不取走数据，MSG_DONTWAIT 不阻塞。
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&raw mut byte).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    n == 0 || (n < 0 && io::Error::last_os_error().kind() != ErrorKind::WouldBlock)
}

fn log_err(r: io::Result<()>) {
    if let Err(e) = r {
        eprintln!("huitai daemon: {e}");
    }
}

/// 标签页断开：先记它不在前台了（和客户端发的焦点同一种事件），再记断开。
fn detached_events(id: &SessionId) -> [HostEvent; 2] {
    [
        HostEvent::Focus {
            id: id.clone(),
            front: false,
        },
        HostEvent::Detached { id: id.clone() },
    ]
}

fn not_found(id: &SessionId) -> Response {
    Response::Error(format!("没有这条托管对话：{id}"))
}

/// 标记这条对话结束并从登记表注销。调用方同时持有 `sessions` 和这条的 `state`：
/// 拿着旧引用、还没启动它的 Attach 看到 `ended` 就不再启动，不会多出一条登记表里找不到的进程。
fn end_entry(sessions: &mut HashMap<SessionId, Arc<Hosted>>, st: &mut State, hosted: &Arc<Hosted>) {
    st.ended = true;
    if sessions
        .get(&hosted.spec.id)
        .is_some_and(|h| Arc::ptr_eq(h, hosted))
    {
        sessions.remove(&hosted.spec.id);
    }
}

/// 去掉控制字符（C0、DEL、C1），用于拼进终端序列的文字。
fn strip_controls(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

impl Shared {
    /// 回答握手，返回接不接待这个客户端。比自己新的客户端连过，就记下「托管对话都结束后让位」。
    fn greet(&self, stream: &mut UnixStream, peer: &Build) -> io::Result<bool> {
        let welcome = peer.protocol >= MIN_CLIENT_PROTOCOL;
        if welcome && peer.newer_than(&self.build) {
            self.retire_when_idle.store(true, Ordering::Release);
        }
        let hello = HelloReply {
            build: self.build.clone(),
            pid: std::process::id(),
            hosted: lock(&self.sessions).len(),
            refused: (!welcome).then(|| OLD_CLIENT.to_owned()),
        };
        reply(stream, Response::Hello(hello))?;
        Ok(welcome)
    }

    /// `Retire`：没有托管对话就定下退出（调用方回完话再 [`Shared::wake`]），返回应答和退没退。
    /// 还有对话就等最后一条结束（[`Shared::unregister`]）。
    fn retire_if_idle(&self) -> (Response, bool) {
        let sessions = lock(&self.sessions);
        self.retire_when_idle.store(true, Ordering::Release);
        if sessions.is_empty() {
            self.retired.store(true, Ordering::Release);
            return (Response::Ok, true);
        }
        let n = sessions.len();
        (
            Response::Error(format!("还托管着 {n} 条对话，都结束后再退出")),
            false,
        )
    }

    /// 叫醒接收循环让它看到 `retired` 返回。
    fn wake(&self) {
        let _ = UnixStream::connect(socket_path(&self.dir));
    }

    /// 退出前断开事件订阅和会话台，让它们发现守护进程没了。
    fn hang_up_subscribers(&self) {
        lock(&self.subscribers).clear();
        for b in lock(&self.boards).drain(..) {
            let _ = lock(&b.out).shutdown(Shutdown::Both);
        }
    }

    fn find(&self, id: &SessionId) -> Option<Arc<Hosted>> {
        lock(&self.sessions).get(id).cloned()
    }

    fn emit(&self, event: HostEvent) {
        lock(&self.subscribers).retain(|tx| tx.send(event.clone()).is_ok());
    }

    fn register(&self, mut spec: SpawnSpec) -> Response {
        if !spec.cwd.is_absolute() || !spec.cwd.is_dir() {
            return Response::Error(format!("项目目录不存在：{}", spec.cwd.display()));
        }
        if spec.command.trim().is_empty() {
            return Response::Error("启动命令是空的".to_owned());
        }
        // 项目名会拼进标题序列（OSC）和提醒：控制字符会提前结束序列，后面的字节被终端当成输出或控制序列。
        spec.project = strip_controls(&spec.project);
        let mut sessions = lock(&self.sessions);
        if self.retired.load(Ordering::Acquire) {
            return Response::Error("后台正在换新版，再打开一次就好".to_owned());
        }
        let hosted = sessions.entry(spec.id.clone()).or_insert_with(|| {
            Arc::new(Hosted {
                prefix: format!("{} {} · ", spec.vendor.mark(), spec.project),
                spec,
                state: Mutex::default(),
            })
        });
        lock(&hosted.state).registered = Some(self.clock.now());
        Response::Ok
    }

    fn detach(&self, id: &SessionId) -> Response {
        let Some(hosted) = self.find(id) else {
            return not_found(id);
        };
        let client = lock(&hosted.state).client.take();
        if let Some(client) = client {
            client.end(&EndReason::Detached);
            for e in detached_events(id) {
                self.emit(e);
            }
        }
        Response::Ok
    }

    fn kill(self: &Arc<Self>, id: &SessionId) -> Response {
        let Some(hosted) = self.find(id) else {
            return not_found(id);
        };
        let pid = {
            let mut sessions = lock(&self.sessions);
            let mut st = lock(&hosted.state);
            let pid = st.run.as_mut().map(|r| {
                r.killed = true;
                r.pid
            });
            if pid.is_none() {
                // 还没启动：直接取消登记（和标记结束一起做，同时到的 Attach 不会再把它启动起来）。
                end_entry(&mut sessions, &mut st, &hosted);
            }
            pid
        };
        let Some(pid) = pid else {
            self.retire_if_done();
            return Response::Ok;
        };
        pty::signal_group(pid, libc::SIGHUP);
        let escalate = spawn("huitai-kill", move || {
            thread::sleep(KILL_GRACE);
            if lock(&hosted.state).run.as_ref().map(|r| r.pid) == Some(pid) {
                pty::signal_group(pid, libc::SIGKILL);
            }
        });
        if escalate.is_err() {
            // 起不了线程等它：不等了，直接 SIGKILL。
            pty::signal_group(pid, libc::SIGKILL);
        }
        Response::Ok
    }

    fn list(&self) -> Vec<HostedInfo> {
        let sessions: Vec<_> = lock(&self.sessions).values().cloned().collect();
        let mut infos: Vec<_> = sessions
            .iter()
            .map(|h| {
                let st = lock(&h.state);
                HostedInfo {
                    id: h.spec.id.clone(),
                    vendor: h.spec.vendor,
                    project: h.spec.project.clone(),
                    cwd: h.spec.cwd.clone(),
                    pid: st.run.as_ref().map(|r| r.pid),
                    attached: st.client.is_some(),
                    title: st.title.clone(),
                }
            })
            .collect();
        infos.sort_by(|a, b| a.id.cmp(&b.id));
        infos
    }

    /// 只读当前画面：只在持锁时导出状态机，不写 PTY、不改大小、不碰正接着的客户端。
    fn snapshot(&self, id: &SessionId, since: Option<u64>) -> Response {
        let Some(hosted) = self.find(id) else {
            return not_found(id);
        };
        let st = lock(&hosted.state);
        let Some(run) = st.run.as_ref() else {
            return Response::Error(format!("这条对话还没启动：{id}"));
        };
        if since == Some(run.screen.version()) {
            return Response::Unchanged;
        }
        Response::Snapshot(run.screen.snapshot())
    }

    /// 撤掉登记了 [`PENDING_TTL`] 还没启动的对话，返回撤掉的。撤完登记表空了、又有新版客户端连过，就让位。
    fn expire_pending(&self) -> Vec<SessionId> {
        let now = self.clock.now();
        let mut expired = Vec::new();
        lock(&self.sessions).retain(|id, hosted| {
            let mut st = lock(&hosted.state);
            let stale = st.run.is_none()
                && st
                    .registered
                    .is_some_and(|t| now.saturating_duration_since(t) >= PENDING_TTL);
            if stale {
                st.ended = true;
                expired.push(id.clone());
            }
            !stale
        });
        if !expired.is_empty() {
            self.retire_if_done();
        }
        expired.sort();
        expired
    }

    /// 一条对话收完尾之后调用：这是最后一条、又有新版客户端连过，就让位给新版（定下退出，叫醒接收循环）。
    fn retire_if_done(&self) {
        let sessions = lock(&self.sessions);
        let retire = sessions.is_empty()
            && self.retire_when_idle.load(Ordering::Acquire)
            && !self.retired.swap(true, Ordering::AcqRel);
        drop(sessions);
        if retire {
            self.wake();
        }
    }

    /// 进程起来了：记进 `running.json`；它要是在「被中断」里，已经重开了，拿掉，返回拿掉的记录。
    fn record_started(&self, spec: &SpawnSpec) -> Option<RunRecord> {
        let started_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let record = RunRecord {
            id: spec.id.clone(),
            vendor: spec.vendor,
            project: spec.project.clone(),
            cwd: spec.cwd.clone(),
            started_at,
        };
        let mut running = lock(&self.running);
        running.insert(spec.id.clone(), record);
        log_err(interrupt::save(
            &interrupt::running_path(&self.dir),
            &running,
        ));
        drop(running);
        let resumed = lock(&self.interrupted).get(&spec.id).cloned();
        self.dismiss(std::slice::from_ref(&spec.id));
        resumed
    }

    /// 重开的被中断对话没能起来：放回「被中断」。
    fn restore_interrupted(&self, record: RunRecord) {
        let mut interrupted = lock(&self.interrupted);
        interrupted.insert(record.id.clone(), record);
        log_err(interrupt::save(
            &interrupt::interrupted_path(&self.dir),
            &interrupted,
        ));
    }

    /// 进程退出（自己退出或被结束）：从 `running.json` 移除，不算被中断。
    fn record_finished(&self, id: &SessionId) {
        let mut running = lock(&self.running);
        if running.remove(id).is_some() {
            log_err(interrupt::save(
                &interrupt::running_path(&self.dir),
                &running,
            ));
        }
    }

    fn dismiss(&self, ids: &[SessionId]) -> Response {
        let mut interrupted = lock(&self.interrupted);
        let before = interrupted.len();
        interrupted.retain(|id, _| !ids.contains(id));
        if interrupted.len() == before {
            return Response::Ok;
        }
        match interrupt::save(&interrupt::interrupted_path(&self.dir), &interrupted) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error(format!("被中断列表没存上：{e}")),
        }
    }

    /// 查一遍：闲置的对话标上「正在收回」，各起一个线程按步骤结束它。返回这次开始收回的对话。
    /// 设置每次现读，会话台里改了不用重启守护进程；读不到就这一轮不收（宁可不收也不误收）。
    fn reclaim_idle(self: &Arc<Self>) -> Vec<SessionId> {
        let limit = match Config::load(&self.dir.join("config.json")) {
            Ok(cfg) => cfg.idle_reclaim(),
            Err(e) => {
                eprintln!("huitai daemon: 设置没读到，这一轮不收回闲置对话: {e}");
                return Vec::new();
            }
        };
        if limit.is_none() {
            return Vec::new();
        }
        let now = self.clock.now();
        let sessions: Vec<_> = lock(&self.sessions).values().cloned().collect();
        let mut picked = Vec::new();
        for hosted in sessions {
            let target = {
                let mut st = lock(&hosted.state);
                let attached = st.client.is_some();
                st.run.as_mut().and_then(|run| {
                    let idle = !run.reclaiming && run.activity.should_reclaim(attached, now, limit);
                    run.reclaiming |= idle;
                    idle.then(|| (run.pid, Arc::clone(&run.input)))
                })
            };
            let Some((pid, input)) = target else { continue };
            let shared = Arc::clone(self);
            let id = hosted.spec.id.clone();
            match spawn("huitai-reclaim", move || {
                shared.reclaim(&hosted, pid, &input)
            }) {
                Ok(()) => picked.push(id),
                Err(e) => eprintln!("huitai daemon: 起不了线程收回 {id}: {e}"),
            }
        }
        picked.sort();
        picked
    }

    /// 按厂商正常退出方式结束一条闲置对话：两下 Ctrl+C → 等 → SIGTERM 进程组 → 等 → SIGKILL。
    /// 中途有标签页接上就停手（对话照跑）。退出后的收尾和自己退出一样（`finish`），不算被中断。
    fn reclaim(&self, hosted: &Hosted, pid: u32, input: &Outbox) {
        for i in 0..2 {
            if i > 0 {
                thread::sleep(idle::QUIT_GAP);
            }
            if !still_reclaiming(hosted, pid) {
                return;
            }
            input.try_push(vec![idle::QUIT_KEY]);
        }
        for (grace, then) in [
            (idle::QUIT_GRACE, libc::SIGTERM),
            (idle::TERM_GRACE, libc::SIGKILL),
        ] {
            if !self.wait_reclaim(hosted, pid, grace) {
                return;
            }
            pty::signal_group(pid, then);
        }
    }

    /// 等收回中的进程退出，最多等时钟上的 `grace`。还在收回、时间到了返回 `true`（该下一步）；
    /// 进程已经退出或有标签页接上了（收回取消）返回 `false`。
    fn wait_reclaim(&self, hosted: &Hosted, pid: u32, grace: Duration) -> bool {
        let deadline = self.clock.now() + grace;
        loop {
            if !still_reclaiming(hosted, pid) {
                return false;
            }
            if self.clock.now() >= deadline {
                return true;
            }
            thread::sleep(RECLAIM_POLL);
        }
    }

    /// 在跑的对话数变化时更新防休眠断言。
    fn running_changed(&self, started: bool) {
        let mut power = lock(&self.power);
        if started {
            power.0 += 1;
        } else {
            power.0 = power.0.saturating_sub(1);
        }
        if power.0 == 0 {
            power.1 = None;
        } else if power.1.is_none() {
            power.1 = PowerAssertion::acquire();
            if power.1.is_none() {
                eprintln!("huitai daemon: 防休眠断言没拿到");
            }
        }
    }
}

/// 这个进程还在、还在收回（中途有标签页接上会清掉标记）。
fn still_reclaiming(hosted: &Hosted, pid: u32) -> bool {
    lock(&hosted.state)
        .run
        .as_ref()
        .is_some_and(|r| r.pid == pid && r.reclaiming)
}

fn attach(
    shared: &Arc<Shared>,
    stream: UnixStream,
    id: &SessionId,
    env: &[(String, String)],
    cols: u16,
    rows: u16,
) -> io::Result<()> {
    let mut stream = stream;
    let Some(hosted) = shared.find(id) else {
        return protocol::write_json(&mut stream, KIND_RESPONSE, &not_found(id));
    };
    attach_to(shared, stream, &hosted, env, cols, rows)
}

/// attach 已经在登记表里找到的这条。找到之后它可能已经结束、注销了（`ended`），这时不再启动。
fn attach_to(
    shared: &Arc<Shared>,
    mut stream: UnixStream,
    hosted: &Arc<Hosted>,
    env: &[(String, String)],
    cols: u16,
    rows: u16,
) -> io::Result<()> {
    let id = &hosted.spec.id;
    let (cols, rows) = sane_size(cols, rows);
    let client_id = shared.next_client.fetch_add(1, Ordering::Relaxed);

    let mut started = None;
    let input = {
        let mut st = lock(&hosted.state);
        if st.ended {
            drop(st);
            return protocol::write_json(&mut stream, KIND_RESPONSE, &not_found(id));
        }
        match st.run.as_mut() {
            Some(run) => {
                run.reclaiming = false;
                if run.screen.size() != (cols, rows) {
                    pty::resize(&run.master, cols, rows)?;
                    run.screen.resize(cols, rows);
                    run.activity.nudge(shared.clock.now());
                }
            }
            None => match start(shared, hosted, env, cols, rows) {
                Ok(run) => {
                    started = Some(run.pid);
                    st.run = Some(run);
                }
                Err(e) => {
                    let msg = format!("启动对话失败：{e}");
                    return protocol::write_json(&mut stream, KIND_RESPONSE, &Response::Error(msg));
                }
            },
        }
        if let Some(old) = st.client.take() {
            old.end(&EndReason::Elsewhere);
        }
        let run = st.run.as_ref().expect("上面刚确认或启动过");
        let mut snapshot = run.screen.restore_sequence();
        snapshot.extend_from_slice(title_sequence(hosted, st.title.as_deref()).as_bytes());
        let client = Client::new(client_id, &stream, snapshot.len())?;
        client.send_all(KIND_RESPONSE, &serde_json::to_vec(&Response::Ok)?);
        for part in snapshot.chunks(RESTORE_CHUNK) {
            client.send_all(KIND_OUTPUT, part);
        }
        let input = Arc::clone(&run.input);
        st.client = Some(client);
        input
    };
    if let Some(pid) = started {
        shared.emit(HostEvent::Started {
            id: id.clone(),
            pid,
        });
    }
    shared.emit(HostEvent::Attached { id: id.clone() });

    pump_client_input(shared, hosted, &mut stream, &input, client_id);

    let mut st = lock(&hosted.state);
    if st.client.as_ref().is_some_and(|c| c.id == client_id) {
        st.client = None;
        drop(st);
        for e in detached_events(id) {
            shared.emit(e);
        }
    }
    Ok(())
}

/// 客户端发来的输入原样排进 PTY 的输入队列，窗口大小变化转给 PTY 和状态机，标签页焦点交给进程内订阅。
/// 连接断开、进程已退出，或输入排满时这个客户端已被脱离/接走，就返回。
fn pump_client_input(
    shared: &Shared,
    hosted: &Hosted,
    stream: &mut UnixStream,
    input: &Outbox,
    client_id: u64,
) {
    let still_attached = || {
        lock(&hosted.state)
            .client
            .as_ref()
            .is_some_and(|c| c.id == client_id)
    };
    while let Ok(Some(frame)) = protocol::read_frame(stream) {
        match frame.kind {
            KIND_INPUT => {
                // CLI 自己开了焦点上报时，焦点切换原样到这里；它会重画，闲置判断不当开跑。
                if matches!(&frame.payload[..], b"\x1b[I" | b"\x1b[O")
                    && let Some(run) = lock(&hosted.state).run.as_mut()
                {
                    run.activity.nudge(shared.clock.now());
                }
                if !input.push(frame.payload, still_attached) {
                    return;
                }
            }
            KIND_RESIZE => {
                let Some((cols, rows)) = protocol::parse_resize(&frame.payload) else {
                    continue;
                };
                let (cols, rows) = sane_size(cols, rows);
                let mut st = lock(&hosted.state);
                if let Some(run) = st.run.as_mut() {
                    let _ = pty::resize(&run.master, cols, rows);
                    run.screen.resize(cols, rows);
                    run.activity.nudge(shared.clock.now());
                }
            }
            KIND_FOCUS => {
                if let [b] = frame.payload[..] {
                    shared.emit(HostEvent::Focus {
                        id: hosted.spec.id.clone(),
                        front: b != 0,
                    });
                }
            }
            _ => {}
        }
    }
}

/// attach 时主动设一次标题：CLI 设过就用加了前缀的那个，没设过就只用前缀。
fn title_sequence(hosted: &Hosted, title: Option<&str>) -> String {
    let text = match title {
        Some(t) => format!("{}{t}", hosted.prefix),
        None => hosted.prefix.trim_end_matches(" · ").to_owned(),
    };
    format!("\x1b]2;{text}\x07")
}

fn start(
    shared: &Arc<Shared>,
    hosted: &Arc<Hosted>,
    env: &[(String, String)],
    cols: u16,
    rows: u16,
) -> io::Result<Running> {
    let spec = &hosted.spec;
    let pty = pty::spawn(&spec.command, &spec.cwd, env, cols, rows)?;
    let pid = pty.child.id();
    let mut child = pty.child;
    let reader = pty.master.try_clone()?;
    let input = Outbox::start(pty.master.try_clone()?, INPUT_BACKLOG, || {});
    let master = Arc::new(pty.master);
    shared.running_changed(true);
    let resumed = shared.record_started(spec);

    let (done_tx, done_rx) = mpsc::channel::<()>();
    let waiter = {
        let (shared, hosted) = (Arc::clone(shared), Arc::clone(hosted));
        spawn("huitai-wait", move || {
            let code = match child.wait() {
                Ok(status) => status.code(),
                Err(e) => {
                    eprintln!("huitai daemon: 等子进程 {pid} 出错: {e}");
                    None
                }
            };
            // 用户要结束（或收回）这条对话：CLI 退了，进程组里剩下的子孙（不理 SIGHUP/SIGTERM 的）一并 SIGKILL，
            // 不留孤儿。CLI 一退内核就收回会话的终端，它们不一定还占着 PTY，所以不能等读线程来判断。
            if lock(&hosted.state)
                .run
                .as_ref()
                .is_some_and(Running::ending)
            {
                pty::signal_group(pid, libc::SIGKILL);
            }
            // 读线程读到 EOF 要等所有拿着从端的进程都关掉。CLI 自己退了、留下的后台进程还占着从端时，
            // 像终端关标签页一样给进程组发 SIGHUP。
            if done_rx.recv_timeout(DRAIN_GRACE).is_err() {
                pty::signal_group(pid, libc::SIGHUP);
                let _ = done_rx.recv_timeout(DRAIN_GRACE);
            }
            finish(&shared, &hosted, code);
        })
    };
    if let Err(e) = waiter {
        // 没人等它：直接结束、自己收尸，撤掉上面记下的「在跑」。
        pty::signal_group(pid, libc::SIGKILL);
        // SAFETY: pid 是刚启动、还没被回收的子进程；waitpid 只回收它。
        unsafe {
            libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), 0);
        }
        shared.record_finished(&spec.id);
        shared.running_changed(false);
        return Err(e);
    }
    let pump = {
        let (shared, hosted) = (Arc::clone(shared), Arc::clone(hosted));
        spawn("huitai-output", move || {
            pump_output(&shared, &hosted, reader);
            let _ = done_tx.send(());
        })
    };
    if let Err(e) = pump {
        // 没人读输出：结束它，等待线程收尾（done_tx 已随闭包丢掉，它不会多等）。
        pty::signal_group(pid, libc::SIGKILL);
        return Err(e);
    }
    Ok(Running {
        master,
        input,
        pid,
        screen: Screen::new(cols, rows),
        scanner: Scanner::new(&hosted.prefix),
        activity: Activity::new(shared.clock.now()),
        reclaiming: false,
        killed: false,
        resumed,
        started: shared.clock.now(),
    })
}

/// PTY 输出 → 扫描改写 → 状态机 → 当前客户端，并发出状态事件。读到 EOF/EIO（从端全关）就返回。
/// 处理某一段时 panic（状态机遇到没见过的输入之类）：写日志，换一个新的状态机和扫描器接着转发，
/// 这条对话和它的标签页照常（补发画面从这之后重新攒）。
fn pump_output(shared: &Shared, hosted: &Hosted, mut reader: File) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let chunk = &buf[..n];
        match panic::catch_unwind(AssertUnwindSafe(|| forward_output(shared, hosted, chunk))) {
            Ok(Some(events)) => {
                for e in events {
                    shared.emit(e);
                }
            }
            Ok(None) => return,
            Err(p) => {
                eprintln!(
                    "huitai daemon: 处理 {} 的输出时 panic，换新状态机接着转发：{}",
                    hosted.spec.id,
                    panic_message(&*p)
                );
                reset_screen(hosted);
            }
        }
    }
}

/// 处理一段输出，返回要发的事件；这条对话已经回收（`run` 没了）时返回 `None`。
fn forward_output(shared: &Shared, hosted: &Hosted, chunk: &[u8]) -> Option<Vec<HostEvent>> {
    let id = &hosted.spec.id;
    let mut out = Vec::with_capacity(chunk.len());
    let mut signals = Vec::new();
    let mut events = Vec::new();
    let mut guard = lock(&hosted.state);
    let State {
        run,
        client,
        title,
        last_output_event,
        ..
    } = &mut *guard;
    // attach 持锁启动进程并放入 run，读线程拿到锁时 run 一定已就位；为 None 只会是已经回收。
    let run = run.as_mut()?;
    failpoint::output(chunk);
    run.scanner.feed(chunk, &mut out, &mut signals);
    let now = shared.clock.now();
    run.activity.output(now);
    if signals.iter().any(needs_you) {
        run.activity.signal(now);
    }
    // CLI 明说在跑（OSC 7501 working）：不等节流，马上报一次有输出。
    let working = signals
        .iter()
        .any(|s| matches!(s, Signal::Status(Status::Working)));
    run.screen.feed(chunk);
    let replies = run.screen.take_replies();
    // 持锁时不能写 PTY：CLI 不读输入时会一直卡住，整个守护进程跟着卡在这条对话的锁上。
    if client.is_none() && !replies.is_empty() {
        run.input.try_push(replies);
    }
    if let Some(c) = client.as_ref()
        && !c.send(KIND_OUTPUT, &out)
    {
        // 积压太多：丢掉没发的，断开它（写线程收尾时关 socket）。
        c.out.close();
        *client = None;
        events.extend(detached_events(id));
    }
    if working || last_output_event.is_none_or(|t| t.elapsed() >= OUTPUT_EVENT_GAP) {
        *last_output_event = Some(Instant::now());
        events.push(HostEvent::Output { id: id.clone() });
    }
    for s in signals {
        events.push(match s {
            // 程序状态不加新事件：要你回来的和响铃走同一条路（旧版会话台也认得），在跑的已经报过有输出。
            Signal::Bell => HostEvent::Bell { id: id.clone() },
            Signal::Status(status) if status.needs_you() => HostEvent::Bell { id: id.clone() },
            Signal::Status(_) => continue,
            Signal::Notify { title, body } => HostEvent::Notify {
                id: id.clone(),
                title,
                body,
            },
            Signal::Title(t) => {
                *title = Some(t.clone());
                HostEvent::Title {
                    id: id.clone(),
                    title: t,
                }
            }
        });
    }
    Some(events)
}

/// 这个信号是不是 CLI 停下来要你回来：响铃、桌面通知、OSC 7501 的 blocked/done/error、Gemini 等待标题。
fn needs_you(signal: &Signal) -> bool {
    match signal {
        Signal::Bell | Signal::Notify { .. } => true,
        Signal::Status(status) => status.needs_you(),
        Signal::Title(_) => false,
    }
}

/// panic 之后状态机和扫描器可能停在半截，换成新的（大小不变）。
fn reset_screen(hosted: &Hosted) {
    if let Some(run) = lock(&hosted.state).run.as_mut() {
        let (cols, rows) = run.screen.size();
        run.screen = Screen::new(cols, rows);
        run.scanner = Scanner::new(&hosted.prefix);
    }
}

/// 子进程退出：回收 PTY，通知正接着的客户端，取消登记，更新防休眠。
/// 取走 `run`、注销、删 `running.json` 记录、发 Exited 都在同时持有 `sessions` 和 `state` 时做：
/// 期间到的 Attach 启动不了这条，之后同一条对话新登记、新启动的进程，它的记录和事件也不会被这里删掉或盖住。
fn finish(shared: &Shared, hosted: &Arc<Hosted>, code: Option<i32>) {
    let id = &hosted.spec.id;
    let client = {
        let mut sessions = lock(&shared.sessions);
        let mut st = lock(&hosted.state);
        if let Some(mut run) = st.run.take() {
            run.input.close();
            // 重开的被中断对话一起来就非正常退出（找不到 CLI、启动就崩）：放回「被中断」，先于 Exited，
            // 收到 Exited 再去取列表的会话台能看到它。
            let failed = !run.ending()
                && code != Some(0)
                && shared.clock.now().saturating_duration_since(run.started) < RESUME_GRACE;
            if let Some(record) = run.resumed.take().filter(|_| failed) {
                shared.restore_interrupted(record);
            }
        }
        end_entry(&mut sessions, &mut st, hosted);
        shared.record_finished(id);
        shared.emit(HostEvent::Exited {
            id: id.clone(),
            code,
        });
        st.client.take()
    };
    if let Some(c) = client {
        c.end(&EndReason::Exited { code });
    }
    shared.running_changed(false);
    shared.retire_if_done();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Vendor;

    fn shared_in(tag: &str) -> Arc<Shared> {
        let dir = PathBuf::from(format!("/tmp/htd-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Arc::new(Shared {
            dir,
            ..Shared::default()
        })
    }

    fn register(shared: &Shared, id: &str) -> Arc<Hosted> {
        let spec = SpawnSpec {
            id: SessionId::from(id),
            vendor: Vendor::Claude,
            project: "demo".to_owned(),
            cwd: shared.dir.clone(),
            command: "sleep 5".to_owned(),
        };
        assert_eq!(shared.register(spec), Response::Ok);
        shared.find(&SessionId::from(id)).expect("刚登记")
    }

    /// 拿着 `find` 时的旧引用 attach（找到之后、锁住之前它被收尾或取消了），返回应答。
    fn late_attach(shared: &Arc<Shared>, hosted: &Arc<Hosted>) -> Response {
        let (near, mut far) = UnixStream::pair().unwrap();
        let (s, h) = (Arc::clone(shared), Arc::clone(hosted));
        let t = thread::spawn(move || {
            let env = [
                ("SHELL".to_owned(), "/bin/sh".to_owned()),
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ];
            attach_to(&s, near, &h, &env, 80, 24)
        });
        far.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let r = protocol::read_json(&mut far, KIND_RESPONSE).unwrap();
        drop(far);
        t.join().unwrap().unwrap();
        r
    }

    fn assert_not_started(shared: &Arc<Shared>, hosted: &Arc<Hosted>, r: &Response) {
        let pid = lock(&hosted.state).run.as_ref().map(|r| r.pid);
        if let Some(pid) = pid {
            pty::signal_group(pid, libc::SIGKILL);
        }
        assert!(matches!(r, Response::Error(_)), "应答：{r:?}");
        assert_eq!(pid, None, "启动了一条登记表里找不到的进程");
        assert!(lock(&shared.running).is_empty());
        let _ = fs::remove_dir_all(&shared.dir);
    }

    #[test]
    fn attach_racing_with_exit_does_not_start_an_unmanaged_process() {
        let shared = shared_in("exit");
        let hosted = register(&shared, "a");
        // 进程退出的收尾抢在这个 Attach 锁住之前做完。
        finish(&shared, &hosted, Some(0));
        let r = late_attach(&shared, &hosted);
        assert_not_started(&shared, &hosted, &r);
    }

    #[test]
    fn attach_racing_with_kill_of_unstarted_does_not_start_it() {
        let shared = shared_in("kill");
        let hosted = register(&shared, "a");
        assert_eq!(shared.kill(&hosted.spec.id), Response::Ok);
        let r = late_attach(&shared, &hosted);
        assert_not_started(&shared, &hosted, &r);
    }

    #[test]
    fn project_name_control_characters_never_reach_the_title_sequence() {
        let shared = shared_in("title");
        let spec = SpawnSpec {
            id: SessionId::from("t"),
            vendor: Vendor::Claude,
            project: "a\x07b\x1b\\c\u{9c}d\ne".to_owned(),
            cwd: shared.dir.clone(),
            command: "true".to_owned(),
        };
        assert_eq!(shared.register(spec), Response::Ok);
        let hosted = shared.find(&SessionId::from("t")).unwrap();
        assert_eq!(hosted.spec.project, "ab\\cde");
        let mark = Vendor::Claude.mark();
        assert_eq!(
            title_sequence(&hosted, None),
            format!("\x1b]2;{mark} ab\\cde\x07")
        );
        assert_eq!(
            title_sequence(&hosted, Some("x")),
            format!("\x1b]2;{mark} ab\\cde · x\x07")
        );
        // CLI 自己设的标题走扫描器改写，前缀同样是干净的。
        let (mut out, mut signals) = (Vec::new(), Vec::new());
        Scanner::new(&hosted.prefix).feed(b"\x1b]0;x\x07", &mut out, &mut signals);
        assert_eq!(out, format!("\x1b]0;{mark} ab\\cde · x\x07").into_bytes());
        let _ = fs::remove_dir_all(&shared.dir);
    }
}
