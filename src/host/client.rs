//! 守护进程的客户端：发请求、拉起守护进程、attach 转发、订阅事件，以及 `ai attach` 的终端接线。
//!
//! 每条连接先握手（[`open`]）。守护进程是没有握手的旧版时记住这个 socket，之后对它按旧协议说话（不发 `Hello`），
//! 旧版本来就有的请求照常能用。

use std::borrow::Cow;
use std::fs::OpenOptions;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::daemon::{prepare_dir, socket_path};
use super::protocol::{
    self, BoardInfo, Build, EndReason, HelloReply, HostEvent, HostedInfo, KIND_END, KIND_EVENT,
    KIND_FOCUS, KIND_INPUT, KIND_OUTPUT, KIND_REQUEST, KIND_RESIZE, KIND_RESPONSE, Request,
    Response, ScreenSnapshot,
};
use super::pty;
use super::version;
use crate::core::SessionId;

/// 普通请求等应答的上限。
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// 拉起守护进程后最多等它这么久开始监听。
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(3);

/// 新起的守护进程因为锁被正在退出的旧版占着而退出时，隔这么久再起一次。
const RELAUNCH_GAP: Duration = Duration::from_millis(100);
/// 旧版守护进程退出后多久看一次它让出来没有。
const RETIRE_POLL: Duration = Duration::from_millis(20);
/// 不认识的请求旧版守护进程直接关连接，这时告诉用户为什么。
const OLD_DAEMON_REFUSED: &str =
    "后台是旧版的守护进程，不认这个操作；它托管的对话都结束后会自动换成新版";

/// 认出来是没有握手的旧版守护进程的那个 socket（文件的设备号、inode、改动时间；守护进程每次启动都新建 socket）。
static LEGACY: Mutex<Option<SocketKey>> = Mutex::new(None);

type SocketKey = (u64, u64, i64, i64);

fn socket_key(dir: &Path) -> Option<SocketKey> {
    let m = std::fs::metadata(socket_path(dir)).ok()?;
    Some((m.dev(), m.ino(), m.ctime(), m.ctime_nsec()))
}

fn is_legacy(key: Option<SocketKey>) -> bool {
    key.is_some() && *LEGACY.lock().unwrap_or_else(PoisonError::into_inner) == key
}

/// 连上并握手。守护进程不接待（协议太老）时报错，不发别的；返回 `None` 的应答表示它是没有握手的旧版。
fn open(dir: &Path) -> io::Result<(UnixStream, Option<HelloReply>)> {
    let key = socket_key(dir);
    if is_legacy(key) {
        return Ok((UnixStream::connect(socket_path(dir))?, None));
    }
    let mut stream = UnixStream::connect(socket_path(dir))?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    protocol::write_json(
        &mut stream,
        KIND_REQUEST,
        &Request::Hello(version::current().clone()),
    )?;
    match protocol::read_json::<Response>(&mut stream, KIND_RESPONSE) {
        Ok(Response::Hello(hello)) => match hello.refused {
            Some(why) => Err(io::Error::other(why)),
            None => Ok((stream, Some(hello))),
        },
        Ok(other) => Err(io::Error::other(format!("握手应答不对：{other:?}"))),
        // 旧版读不懂 Hello，什么也不回就关了连接。
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
            ) =>
        {
            *LEGACY.lock().unwrap_or_else(PoisonError::into_inner) = key;
            Ok((UnixStream::connect(socket_path(dir))?, None))
        }
        Err(e) => Err(e),
    }
}

fn connect(dir: &Path) -> io::Result<UnixStream> {
    open(dir).map(|(s, _)| s)
}

/// 发一个请求等一个应答（不用于 Attach/Events）。
pub fn request(dir: &Path, req: &Request) -> io::Result<Response> {
    let (mut stream, hello) = open(dir)?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    protocol::write_json(&mut stream, KIND_REQUEST, req)?;
    match protocol::read_json(&mut stream, KIND_RESPONSE) {
        Err(e) if hello.is_none() && e.kind() == ErrorKind::UnexpectedEof => {
            Err(io::Error::other(OLD_DAEMON_REFUSED))
        }
        other => other,
    }
}

/// 守护进程是哪一版、托管着几条对话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonInfo {
    /// `None`：没有握手的旧版。
    pub build: Option<Build>,
    pub pid: u32,
    /// 登记着的托管对话条数（含还没启动的）。
    pub hosted: usize,
}

impl DaemonInfo {
    /// 比当前程序旧（没有握手的旧版也算）。
    pub fn outdated(&self) -> bool {
        self.build
            .as_ref()
            .is_none_or(|b| version::current().newer_than(b))
    }

    /// 旧版且空着，可以直接换成新版。
    pub fn replaceable(&self) -> bool {
        self.outdated() && self.hosted == 0
    }
}

/// 问在跑的守护进程是哪一版。没在跑时返回连接错误（不拉起）。
pub fn probe(dir: &Path) -> io::Result<DaemonInfo> {
    let (_, hello) = open(dir)?;
    if let Some(h) = hello {
        return Ok(DaemonInfo {
            build: Some(h.build),
            pid: h.pid,
            hosted: h.hosted,
        });
    }
    let Response::Pong { pid } = request(dir, &Request::Ping)? else {
        return Err(io::Error::other("旧版守护进程的 Ping 应答不对"));
    };
    let Response::List(list) = request(dir, &Request::List)? else {
        return Err(io::Error::other("旧版守护进程的 List 应答不对"));
    };
    Ok(DaemonInfo {
        build: None,
        pid,
        hosted: list.len(),
    })
}

/// 让空着的旧版守护进程退出，等它让出 socket。返回它退了没有（刚好有人在它那儿开了对话就没退）。
fn retire(dir: &Path, old: &DaemonInfo) -> io::Result<bool> {
    if old.build.is_some() {
        match request(dir, &Request::Retire) {
            Ok(Response::Ok) => {}
            Ok(_) => return Ok(false),
            // 它定下退出后可能来不及回话就走了。
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {}
            Err(e) => return Err(e),
        }
    } else {
        // 旧版没有 Retire：空着时直接结束它（没有对话在跑，不会留下被中断的记录）。
        let pid = libc::pid_t::try_from(old.pid).map_err(io::Error::other)?;
        // SAFETY: 只发信号；pid 是守护进程自己在 Pong 里报的进程号。
        if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let deadline = Instant::now() + LAUNCH_TIMEOUT;
    while UnixStream::connect(socket_path(dir)).is_ok() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "旧版守护进程没有按时退出",
            ));
        }
        thread::sleep(RETIRE_POLL);
    }
    Ok(true)
}

/// [`snapshot`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snapshot {
    Screen(ScreenSnapshot),
    /// 画面还是 `since` 那一版。
    Unchanged,
}

/// 只读取一条在跑对话此刻的屏幕。`since` 传上次拿到的版本，画面没变时守护进程只回 `Unchanged`。
/// 守护进程没在跑、没有这条或它还没启动时返回错误（不会拉起守护进程）。
pub fn snapshot(dir: &Path, id: &SessionId, since: Option<u64>) -> io::Result<Snapshot> {
    let req = Request::Snapshot {
        id: id.clone(),
        since,
    };
    match request(dir, &req)? {
        Response::Snapshot(s) => Ok(Snapshot::Screen(s)),
        Response::Unchanged => Ok(Snapshot::Unchanged),
        Response::Error(msg) => Err(io::Error::other(msg)),
        other => Err(io::Error::other(format!("意外的应答：{other:?}"))),
    }
}

/// 守护进程没在跑就用 `launch`（通常是 `ai daemon`）拉起：setsid 脱离当前终端，不经 launchd，
/// 这样它沿用当前终端 App 的系统权限归属。输出写到数据目录的 `daemon.log`。
/// 在跑的是旧版：空着就让它退出、拉起当前版本；托管着对话就接着用它（它的对话结束后自己让位）。
pub fn ensure_daemon(dir: &Path, launch: Command) -> io::Result<DaemonInfo> {
    match probe(dir) {
        Ok(info) if !info.replaceable() => return Ok(info),
        Ok(old) => {
            if !retire(dir, &old)? {
                return probe(dir);
            }
        }
        Err(_) => {}
    }
    launch_daemon(dir, launch)?;
    probe(dir)
}

fn launch_daemon(dir: &Path, mut launch: Command) -> io::Result<()> {
    prepare_dir(dir)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?;
    launch
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: pre_exec 里只调 async-signal-safe 的 setsid。
    unsafe {
        launch.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = launch.spawn()?;
    let deadline = Instant::now() + LAUNCH_TIMEOUT;
    let listening = || UnixStream::connect(socket_path(dir)).is_ok();
    loop {
        if listening() {
            // 不等它：守护进程常驻；它退出时由 init 收尸（我们这边先退出的话）。
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Err(io::Error::other(format!(
                    "守护进程启动失败（{status}），看 {}",
                    dir.join("daemon.log").display()
                )));
            }
            // 锁被别的守护进程占着时它会正常退出：那个在跑就连得上；那个正在退出（换新版）就再起一次。
            if listening() {
                return Ok(());
            }
            thread::sleep(RELAUNCH_GAP);
            child = launch.spawn()?;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(ErrorKind::TimedOut, "守护进程没有按时启动"));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// 转发通道收到的东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachMsg {
    Output(Vec<u8>),
    End(EndReason),
    /// 连接断了但没收到结束原因（守护进程没了）。
    Closed,
}

/// 一条 attach 转发通道。
pub struct Attachment {
    reader: UnixStream,
    writer: AttachWriter,
}

/// 往转发通道写输入和窗口大小，可以拿到别的线程用。
#[derive(Clone)]
pub struct AttachWriter(Arc<Mutex<UnixStream>>);

impl AttachWriter {
    pub fn input(&self, bytes: &[u8]) -> io::Result<()> {
        let mut s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        protocol::write_frame(&mut *s, KIND_INPUT, bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let mut s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        protocol::write_frame(&mut *s, KIND_RESIZE, &protocol::resize_payload(cols, rows))
    }

    /// 标签页到了前台（`true`）或被切走（`false`）。
    pub fn focus(&self, front: bool) -> io::Result<()> {
        let mut s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        protocol::write_frame(&mut *s, KIND_FOCUS, &[u8::from(front)])
    }
}

/// 接上一条已登记的对话。第一次 attach 时守护进程用 `env` 启动 CLI。
pub fn attach(
    dir: &Path,
    id: &SessionId,
    env: Vec<(String, String)>,
    cols: u16,
    rows: u16,
) -> io::Result<Attachment> {
    let mut stream = connect(dir)?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    let req = Request::Attach {
        id: id.clone(),
        env,
        cols,
        rows,
    };
    protocol::write_json(&mut stream, KIND_REQUEST, &req)?;
    match protocol::read_json(&mut stream, KIND_RESPONSE)? {
        Response::Ok => {}
        Response::Error(msg) => return Err(io::Error::other(msg)),
        other => return Err(io::Error::other(format!("意外的应答：{other:?}"))),
    }
    // CLI 一启动就退出时，守护进程回完 Ok 紧接着发结束原因、关连接：这里照样往下走，结束原因由 recv 读出来。
    set_read_timeout(&stream, None)?;
    let writer = AttachWriter(Arc::new(Mutex::new(stream.try_clone()?)));
    Ok(Attachment {
        reader: stream,
        writer,
    })
}

impl Attachment {
    pub fn writer(&self) -> AttachWriter {
        self.writer.clone()
    }

    /// 等下一条消息；`timeout` 为 `None` 时一直等。超时返回 `TimedOut`/`WouldBlock` 错误。
    pub fn recv(&mut self, timeout: Option<Duration>) -> io::Result<AttachMsg> {
        set_read_timeout(&self.reader, timeout)?;
        loop {
            let Some(frame) = protocol::read_frame(&mut self.reader)? else {
                return Ok(AttachMsg::Closed);
            };
            match frame.kind {
                KIND_OUTPUT => return Ok(AttachMsg::Output(frame.payload)),
                KIND_END => return Ok(AttachMsg::End(frame.json()?)),
                _ => continue,
            }
        }
    }
}

/// 对端已关闭时 macOS 的 setsockopt 返回 EINVAL，但缓冲里剩下的帧（比如结束原因）仍然读得到，照常往下读。
fn set_read_timeout(stream: &UnixStream, timeout: Option<Duration>) -> io::Result<()> {
    match stream.set_read_timeout(timeout) {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        other => other,
    }
}

/// 订阅状态事件。
pub struct EventStream(UnixStream);

pub fn subscribe(dir: &Path) -> io::Result<EventStream> {
    open_events(dir, &Request::Events)
}

/// 以会话台的身份订阅：同 `subscribe`，另外会收到要它代弹的提醒（`HostEvent::Alert`）。
/// 连接开着守护进程就认为会话台开着。
pub fn board(dir: &Path, info: BoardInfo) -> io::Result<EventStream> {
    open_events(dir, &Request::Board(info))
}

fn open_events(dir: &Path, req: &Request) -> io::Result<EventStream> {
    let mut stream = connect(dir)?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    protocol::write_json(&mut stream, KIND_REQUEST, req)?;
    match protocol::read_json(&mut stream, KIND_RESPONSE)? {
        Response::Ok => Ok(EventStream(stream)),
        other => Err(io::Error::other(format!("订阅失败：{other:?}"))),
    }
}

/// 会话台从守护进程拿到的一条消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardItem {
    Event(HostEvent),
    /// 托管着的全部对话（轮询旧版守护进程时每隔一会儿重报一次）。
    Hosted(Vec<HostedInfo>),
}

/// 会话台看守护进程的通道：订阅得上就收事件；守护进程比当前程序旧、不认这个订阅
/// （没有握手的旧版，或以后订阅格式改了的旧版），就退而用它也认的 `List` 定期轮询。
pub struct BoardFeed(Feed);

enum Feed {
    Events(EventStream),
    Poll { dir: PathBuf, every: Duration },
}

/// 以会话台身份订阅（同 [`board`]）；订阅失败且守护进程是旧版时降级成每 `every` 轮询一次 `List`。
/// 守护进程不是旧版还订阅失败就报那个错。
pub fn board_feed(dir: &Path, info: BoardInfo, every: Duration) -> io::Result<BoardFeed> {
    let err = match board(dir, info) {
        Ok(events) => return Ok(BoardFeed(Feed::Events(events))),
        Err(e) => e,
    };
    match probe(dir) {
        Ok(d) if d.outdated() => Ok(BoardFeed(Feed::Poll {
            dir: dir.to_path_buf(),
            every,
        })),
        _ => Err(err),
    }
}

impl BoardFeed {
    /// 是降级成轮询的（守护进程是旧版）。
    pub fn polling(&self) -> bool {
        matches!(self.0, Feed::Poll { .. })
    }

    /// 等下一条消息。返回 `None`：守护进程关了连接、连不上了，或轮询着的旧版已经没有托管对话
    /// （调用方这时该把它换成新版）。
    pub fn recv(&mut self) -> io::Result<Option<BoardItem>> {
        match &mut self.0 {
            Feed::Events(events) => Ok(events.next(None)?.map(BoardItem::Event)),
            Feed::Poll { dir, every } => {
                thread::sleep(*every);
                match request(dir, &Request::List) {
                    Ok(Response::List(list)) if !list.is_empty() => {
                        Ok(Some(BoardItem::Hosted(list)))
                    }
                    Ok(Response::List(_)) => Ok(None),
                    Ok(other) => Err(io::Error::other(format!("意外的应答：{other:?}"))),
                    Err(_) => Ok(None),
                }
            }
        }
    }
}

impl EventStream {
    /// 等下一个事件；守护进程关了连接返回 `None`。认不出的事件（更新的守护进程加的种类）跳过，不断开。
    pub fn next(&mut self, timeout: Option<Duration>) -> io::Result<Option<HostEvent>> {
        set_read_timeout(&self.0, timeout)?;
        loop {
            match protocol::read_frame(&mut self.0)? {
                Some(f) if f.kind == KIND_EVENT => match f.json() {
                    Ok(event) => return Ok(Some(event)),
                    Err(_) => continue,
                },
                Some(f) => {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        format!("意外的帧 {}", f.kind as char),
                    ));
                }
                None => return Ok(None),
            }
        }
    }
}

/// `ai attach <id>`：把当前终端和守护进程里的 PTY 接起来，直到 CLI 退出或被别处接走。返回进程退出码。
pub fn attach_terminal(dir: &Path, id: &SessionId, launch: Command) -> io::Result<i32> {
    ensure_daemon(dir, launch)?;
    let stdout = io::stdout();
    let (cols, rows) = pty::size_of(&stdout).unwrap_or((80, 24));
    // 先在主线程挡住 SIGWINCH，后面开的线程继承这个屏蔽，只由专门的线程 sigwait 收它。
    let winch = block_sigwinch();
    let mut att = attach(dir, id, std::env::vars().collect(), cols, rows)?;
    let raw = RawMode::enable()?;

    // CLI 自己开没开焦点上报（?1004）：开了焦点序列照样转给它，没开就是我们要来的，从输入里去掉。
    let cli_focus = Arc::new(AtomicBool::new(false));
    let writer = att.writer();
    let input_focus = Arc::clone(&cli_focus);
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    let (bytes, focus) =
                        split_focus(&buf[..n], input_focus.load(Ordering::Acquire));
                    if !bytes.is_empty() && writer.input(&bytes).is_err() {
                        return;
                    }
                    for front in focus {
                        if writer.focus(front).is_err() {
                            return;
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    });
    if let Some(set) = winch {
        let writer = att.writer();
        thread::spawn(move || {
            while wait_signal(&set) {
                if let Ok((c, r)) = pty::size_of(&io::stdout())
                    && writer.resize(c, r).is_err()
                {
                    return;
                }
            }
        });
    }

    let end = forward(&mut att, &mut stdout.lock(), &cli_focus);
    drop(raw);
    match end? {
        Some(EndReason::Exited { code }) => Ok(code.unwrap_or(1)),
        Some(EndReason::Elsewhere) => {
            eprintln!("\r\n这条对话已在别处打开");
            Ok(0)
        }
        Some(EndReason::Detached) => Ok(0),
        None => Err(io::Error::other("和守护进程的连接断了")),
    }
}

/// 终端焦点上报：开、关，以及开着时终端发来的「到了前台」「被切走」。
const FOCUS_ON: &[u8] = b"\x1b[?1004h";
const FOCUS_OFF: &[u8] = b"\x1b[?1004l";
const FOCUS_IN: &[u8] = b"\x1b[I";
const FOCUS_OUT: &[u8] = b"\x1b[O";

/// 从一块终端输入里认出焦点上报，返回要转给 CLI 的字节和依次出现的焦点（`true` 到了前台）。
/// `cli_wants` 为假（CLI 没开 ?1004，焦点上报是我们自己要来的）时把这些序列去掉；为真时字节原样转。
/// 只认同一次读里完整的序列：终端一次写出整个序列，被拆开的极少见，拆开时原样转给 CLI。
pub fn split_focus(input: &[u8], cli_wants: bool) -> (Cow<'_, [u8]>, Vec<bool>) {
    let mut focus = Vec::new();
    let mut kept = Vec::with_capacity(if cli_wants { 0 } else { input.len() });
    let mut i = 0;
    while i < input.len() {
        let rest = &input[i..];
        if rest.starts_with(FOCUS_IN) || rest.starts_with(FOCUS_OUT) {
            focus.push(rest.starts_with(FOCUS_IN));
            i += FOCUS_IN.len();
            continue;
        }
        if !cli_wants {
            kept.push(input[i]);
        }
        i += 1;
    }
    if cli_wants || focus.is_empty() {
        (Cow::Borrowed(input), focus)
    } else {
        (Cow::Owned(kept), focus)
    }
}

/// 把转发通道的输出写到 `out`，直到通道结束。不管怎么结束，都把 CLI 经透传开在终端上、此刻还开着的模式撤掉：
/// 被别处接走或被要求脱离时 CLI 还在跑，不会自己撤，标签页会留在备用屏、鼠标上报、kitty 键盘协议里。
///
/// 焦点上报：接上时（第一块是补发画面，带着 CLI 已开的模式）和 CLI 每次开关 ?1004 之后，CLI 没要的话
/// 我们自己在真终端开 ?1004，标签页切走、切回才能告诉守护进程；CLI 要了就不重复开，结束时也不由我们关。
/// 接上时先告诉守护进程标签页在前台（刚打开的标签页就在用户眼前）。
/// `cli_focus` 告诉读输入的线程 CLI 此刻要不要焦点序列。
fn forward(
    att: &mut Attachment,
    out: &mut impl Write,
    cli_focus: &AtomicBool,
) -> io::Result<Option<EndReason>> {
    let mut modes = ModeTracker::default();
    let mut first = true;
    // 我们自己在真终端开着焦点上报。
    let mut ours = false;
    let result = loop {
        match att.recv(None) {
            Ok(AttachMsg::Output(bytes)) => {
                modes.feed(&bytes);
                let cli = modes.on.contains(&1004);
                cli_focus.store(cli, Ordering::Release);
                if first {
                    // 发不出去说明连接断了，下一次 recv 会报出来。
                    let _ = att.writer().focus(true);
                }
                let touched = std::mem::take(&mut modes.focus_touched) || first;
                first = false;
                let mut written = out.write_all(&bytes);
                if touched {
                    ours = !cli;
                    if ours {
                        written = written.and_then(|()| out.write_all(FOCUS_ON));
                    }
                }
                if let Err(e) = written.and_then(|()| out.flush()) {
                    break Err(e);
                }
            }
            Ok(AttachMsg::End(reason)) => break Ok(Some(reason)),
            Ok(AttachMsg::Closed) => break Ok(None),
            Err(e) => break Err(e),
        }
    };
    // 终端已经写不进去时撤销序列也写不进去，结果以转发本身为准。
    let mut undo = modes.undo();
    if ours {
        undo.extend_from_slice(FOCUS_OFF);
    }
    let _ = out.write_all(&undo).and_then(|()| out.flush());
    result
}

/// 跟踪的 DEC 私有模式。25（显示光标）记的是「被隐藏」。
const TRACKED_MODES: [u16; 15] = [
    1, 25, 47, 1000, 1002, 1003, 1004, 1005, 1006, 1015, 1016, 1047, 1049, 2004, 2026,
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Track {
    #[default]
    Ground,
    Esc,
    Csi,
}

/// 从输出里认出 CLI 开关的终端模式（只看 CSI 和 ESC = / ESC >），结束时生成撤销序列。
#[derive(Debug, Default)]
struct ModeTracker {
    state: Track,
    lead: u8,
    intermediate: u8,
    params: Vec<u8>,
    on: Vec<u16>,
    keypad: bool,
    kitty: bool,
    cursor_shape: bool,
    /// 上次取走之后 CLI 开关过焦点上报（?1004）。
    focus_touched: bool,
}

impl ModeTracker {
    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.state = match (self.state, b) {
                (_, 0x1b) => Track::Esc,
                (Track::Ground, _) => Track::Ground,
                (Track::Esc, b'[') => {
                    self.lead = 0;
                    self.intermediate = 0;
                    self.params.clear();
                    Track::Csi
                }
                (Track::Esc, b'=') => {
                    self.keypad = true;
                    Track::Ground
                }
                (Track::Esc, b'>') => {
                    self.keypad = false;
                    Track::Ground
                }
                (Track::Esc, _) => Track::Ground,
                (Track::Csi, b'?' | b'>' | b'=' | b'<')
                    if self.lead == 0 && self.params.is_empty() =>
                {
                    self.lead = b;
                    Track::Csi
                }
                (Track::Csi, b'0'..=b'9' | b';' | b':') => {
                    if self.params.len() < 64 {
                        self.params.push(b);
                    }
                    Track::Csi
                }
                (Track::Csi, 0x20..=0x2f) => {
                    self.intermediate = b;
                    Track::Csi
                }
                (Track::Csi, 0x40..=0x7e) => {
                    self.finish_csi(b);
                    Track::Ground
                }
                (Track::Csi, _) => Track::Ground,
            };
        }
    }

    fn params(&self) -> impl Iterator<Item = u16> + '_ {
        self.params.split(|&b| b == b';').map(|p| {
            std::str::from_utf8(p)
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(0)
        })
    }

    fn finish_csi(&mut self, last: u8) {
        match (self.lead, self.intermediate, last) {
            (b'?', 0, b'h' | b'l') => {
                let set = last == b'h';
                let modes: Vec<u16> = self
                    .params()
                    .filter(|p| TRACKED_MODES.contains(p))
                    .collect();
                self.focus_touched |= modes.contains(&1004);
                for m in modes {
                    // 25 反着记：关掉（隐藏光标）才需要撤销。
                    let on = set != (m == 25);
                    self.on.retain(|&x| x != m);
                    if on {
                        self.on.push(m);
                    }
                }
            }
            (b'>' | b'=', 0, b'u') => {
                let flags = self.params().next().unwrap_or(0);
                self.kitty |= flags != 0;
            }
            (0, b' ', b'q') => {
                let shape = self.params().next().unwrap_or(0);
                self.cursor_shape = shape != 0;
            }
            _ => {}
        }
    }

    /// 撤销此刻还开着的模式：先结束同步输出、退出备用屏，再关鼠标/焦点/粘贴/应用光标键，显示光标，
    /// 关 kitty 键盘协议（把当前标志设成 0），还原光标形状和画笔。
    fn undo(&self) -> Vec<u8> {
        let mut s = String::new();
        let order = [
            2026, 1049, 1047, 47, 1000, 1002, 1003, 1004, 1005, 1006, 1015, 1016, 2004, 1,
        ];
        for m in order.into_iter().filter(|m| self.on.contains(m)) {
            s.push_str(&format!("\x1b[?{m}l"));
        }
        if self.on.contains(&25) {
            s.push_str("\x1b[?25h");
        }
        if self.keypad {
            s.push_str("\x1b>");
        }
        if self.kitty {
            s.push_str("\x1b[=0;1u");
        }
        if self.cursor_shape {
            s.push_str("\x1b[0 q");
        }
        s.push_str("\x1b[0m");
        s.into_bytes()
    }
}

extern "C" fn ignore_signal(_: libc::c_int) {}

fn block_sigwinch() -> Option<libc::sigset_t> {
    // SAFETY: set 是可写的局部 sigset_t；sigaction 装的是什么都不做的处理函数；pthread_sigmask 只改本线程屏蔽字。
    unsafe {
        // SIGWINCH 默认动作是忽略，macOS 对这种信号即使屏蔽着也在产生时直接丢弃，sigwait 永远等不到；
        // 装一个空处理函数把它变成「会递送」的信号。
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = ignore_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut()) != 0 {
            return None;
        }
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGWINCH);
        (libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) == 0).then_some(set)
    }
}

fn wait_signal(set: &libc::sigset_t) -> bool {
    let mut sig = 0;
    // SAFETY: set 有效，sig 可写。
    unsafe { libc::sigwait(set, &mut sig) == 0 }
}

/// 标准输入是终端时切到原始模式，drop 时还原。
struct RawMode(Option<libc::termios>);

impl RawMode {
    fn enable() -> io::Result<Self> {
        // SAFETY: isatty/tcgetattr/tcsetattr 只读写传入的 termios。
        unsafe {
            if libc::isatty(0) != 1 {
                return Ok(Self(None));
            }
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut orig) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = orig;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(Some(orig)))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(orig) = &self.0 {
            // SAFETY: orig 是之前 tcgetattr 读到的有效设置。
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, orig);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Vendor;
    use crate::host::daemon::Daemon;
    use crate::host::protocol::SpawnSpec;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    /// CLI 启动时常开的一组模式。
    const CLI_INIT: &[u8] =
        b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?1004h\x1b[>1u\x1b[?2004h\x1b[?25l\x1b=\x1b[5 q";

    fn undo_of(chunks: &[&[u8]]) -> String {
        let mut t = ModeTracker::default();
        for c in chunks {
            t.feed(c);
        }
        String::from_utf8(t.undo()).unwrap()
    }

    fn assert_undoes_cli_init(undo: &str) {
        for want in [
            "\x1b[?1049l",
            "\x1b[?1000l",
            "\x1b[?1006l",
            "\x1b[?1004l",
            "\x1b[?2004l",
            "\x1b[?25h",
            "\x1b>",
            "\x1b[=0;1u",
            "\x1b[0 q",
        ] {
            assert!(undo.contains(want), "缺 {want:?}：{undo:?}");
        }
        assert!(
            undo.find("\x1b[?1049l") < undo.find("\x1b[?1000l"),
            "先退出备用屏"
        );
    }

    #[test]
    fn undoes_modes_the_cli_left_on() {
        assert_undoes_cli_init(&undo_of(&[CLI_INIT]));
        let bytewise: Vec<&[u8]> = CLI_INIT.chunks(1).collect();
        assert_eq!(undo_of(&bytewise), undo_of(&[CLI_INIT]));
    }

    #[test]
    fn leaves_alone_modes_the_cli_already_turned_off() {
        let undo = undo_of(&[
            b"\x1b[?1049h\x1b[?1000;1006h text \x1b[31m",
            b"\x1b[?1000;1006l\x1b[?1049l",
        ]);
        // 不在备用屏时发 ?1049l 会把光标跳回保存的位置，所以没开着就不能发。
        assert_eq!(undo, "\x1b[0m");
    }

    #[test]
    fn ignores_other_sequences() {
        let undo = undo_of(&[b"\x1b]2;title\x07\x1b[2J\x1b[?u\x1b[>0u\x1b[1;31mx\x1b[0 q"]);
        assert_eq!(undo, "\x1b[0m");
    }

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// 假守护进程：握手，收到 Attach 就回 Ok、一段输出和结束原因，然后马上关连接（CLI 一启动就退出时就是这样）。
    fn fake_daemon_that_hangs_up(dir: &Path) -> thread::JoinHandle<()> {
        let _ = std::fs::remove_dir_all(dir);
        prepare_dir(dir).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(socket_path(dir)).unwrap();
        thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = protocol::read_frame(&mut s).unwrap();
            let hello = HelloReply {
                build: version::current().clone(),
                pid: 1,
                hosted: 1,
                refused: None,
            };
            protocol::write_json(&mut s, KIND_RESPONSE, &Response::Hello(hello)).unwrap();
            let _ = protocol::read_frame(&mut s).unwrap();
            let mut all = Vec::new();
            protocol::write_json(&mut all, KIND_RESPONSE, &Response::Ok).unwrap();
            protocol::write_frame(&mut all, KIND_OUTPUT, b"bye").unwrap();
            let end = EndReason::Exited { code: Some(3) };
            protocol::write_json(&mut all, KIND_END, &end).unwrap();
            s.write_all(&all).unwrap();
        })
    }

    /// 守护进程回完 Ok 就关了连接（macOS 这时 setsockopt 返回 EINVAL）：attach 照样成功，读得到输出和结束原因。
    #[test]
    fn attach_succeeds_when_daemon_hangs_up_right_after_ok() {
        for i in 0..20 {
            let dir = PathBuf::from(format!("/tmp/htc-{}-hangup{i}", std::process::id()));
            let server = fake_daemon_that_hangs_up(&dir);
            let id = SessionId::from("quick");
            let mut att = attach(&dir, &id, Vec::new(), 80, 24).expect("attach 失败");
            server.join().unwrap();
            assert_eq!(att.recv(None).unwrap(), AttachMsg::Output(b"bye".to_vec()));
            assert_eq!(
                att.recv(None).unwrap(),
                AttachMsg::End(EndReason::Exited { code: Some(3) })
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// 进程内起守护进程，登记一个开了常见模式后一直跑的假 CLI，先接一次等它把模式开好再断开。
    fn host_with_cli() -> (PathBuf, SessionId) {
        let dir = PathBuf::from(format!(
            "/tmp/htc-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::bind(&dir).unwrap();
        thread::spawn(move || daemon.run());
        let id = SessionId::from("modes");
        let spec = SpawnSpec {
            id: id.clone(),
            vendor: Vendor::Claude,
            project: "p".to_owned(),
            cwd: dir.clone(),
            command: "stty raw -echo; printf '\\033[?1049h\\033[?1000h\\033[?1006h\\033[?1004h\\033[>1u\\033[?2004h\\033[?25l\\033=\\033[5 qREADY'; sleep 60".to_owned(),
        };
        assert_eq!(request(&dir, &Request::Spawn(spec)).unwrap(), Response::Ok);
        let mut first = attach(&dir, &id, env(&dir), 80, 24).unwrap();
        let mut seen = Vec::new();
        while !seen.ends_with(b"READY") {
            match first.recv(Some(Duration::from_secs(10))).unwrap() {
                AttachMsg::Output(b) => seen.extend_from_slice(&b),
                other => panic!("{other:?}"),
            }
        }
        (dir, id)
    }

    fn env(home: &Path) -> Vec<(String, String)> {
        [
            ("SHELL", "/bin/sh"),
            ("PATH", "/usr/bin:/bin"),
            ("HOME", home.to_str().unwrap()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
    }

    fn cleanup(dir: &Path, id: &SessionId) {
        let _ = request(dir, &Request::Kill { id: id.clone() });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detached_tab_is_restored_while_cli_keeps_running() {
        let (dir, id) = host_with_cli();
        let mut att = attach(&dir, &id, env(&dir), 80, 24).unwrap();
        assert_eq!(
            request(&dir, &Request::Detach { id: id.clone() }).unwrap(),
            Response::Ok
        );
        let mut out = Vec::new();
        let end = forward(&mut att, &mut out, &AtomicBool::new(false)).unwrap();
        assert_eq!(end, Some(EndReason::Detached));
        let text = String::from_utf8_lossy(&out);
        let undo = &text[text.rfind("\x1b[?1049l").expect("没撤备用屏")..];
        assert_undoes_cli_init(undo);
        cleanup(&dir, &id);
    }

    #[test]
    fn tab_taken_over_elsewhere_is_restored() {
        let (dir, id) = host_with_cli();
        let mut att = attach(&dir, &id, env(&dir), 80, 24).unwrap();
        let _other = attach(&dir, &id, env(&dir), 80, 24).unwrap();
        let mut out = Vec::new();
        assert_eq!(
            forward(&mut att, &mut out, &AtomicBool::new(false)).unwrap(),
            Some(EndReason::Elsewhere)
        );
        let text = String::from_utf8_lossy(&out);
        assert_undoes_cli_init(&text[text.rfind("\x1b[?1049l").expect("没撤备用屏")..]);
        cleanup(&dir, &id);
    }

    #[test]
    fn focus_reports_are_recognised_and_stripped_when_the_cli_did_not_ask() {
        let input = b"a\x1b[Ob\x1b[I\x1b[A";
        let (bytes, focus) = split_focus(input, false);
        assert_eq!(&*bytes, b"ab\x1b[A");
        assert_eq!(focus, vec![false, true]);
    }

    #[test]
    fn focus_reports_pass_through_when_the_cli_asked() {
        let input = b"a\x1b[Ob\x1b[I";
        let (bytes, focus) = split_focus(input, true);
        assert_eq!(&*bytes, input);
        assert_eq!(focus, vec![false, true]);
    }

    #[test]
    fn input_without_focus_reports_is_untouched() {
        for input in [&b"plain"[..], b"\x1b[1;5I", b"\x1b[", b"\x1bO"] {
            let (bytes, focus) = split_focus(input, false);
            assert_eq!(&*bytes, input);
            assert!(focus.is_empty());
        }
    }

    fn count(hay: &str, needle: &str) -> usize {
        hay.matches(needle).count()
    }

    #[test]
    fn turns_focus_reporting_on_for_a_cli_that_did_not_and_off_at_the_end() {
        let dir = PathBuf::from(format!(
            "/tmp/htc-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::bind(&dir).unwrap();
        thread::spawn(move || daemon.run());
        let id = SessionId::from("plain");
        let spec = SpawnSpec {
            id: id.clone(),
            vendor: Vendor::Claude,
            project: "p".to_owned(),
            cwd: dir.clone(),
            command: "printf READY; sleep 60".to_owned(),
        };
        assert_eq!(request(&dir, &Request::Spawn(spec)).unwrap(), Response::Ok);
        let mut att = attach(&dir, &id, env(&dir), 80, 24).unwrap();
        let cli_focus = AtomicBool::new(true);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let d = dir.clone();
        let i = id.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(500));
            let _ = request(&d, &Request::Detach { id: i });
            let _ = done_tx.send(());
        });
        let mut out = Vec::new();
        assert_eq!(
            forward(&mut att, &mut out, &cli_focus).unwrap(),
            Some(EndReason::Detached)
        );
        done_rx.recv().unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(!cli_focus.load(Ordering::Acquire));
        assert_eq!(count(&text, "\x1b[?1004h"), 1, "{text:?}");
        assert!(
            text.ends_with("\x1b[?1004l"),
            "结束时关掉自己开的：{text:?}"
        );
        cleanup(&dir, &id);
    }

    #[test]
    fn leaves_focus_reporting_to_a_cli_that_turned_it_on() {
        let (dir, id) = host_with_cli();
        let mut att = attach(&dir, &id, env(&dir), 80, 24).unwrap();
        assert_eq!(
            request(&dir, &Request::Detach { id: id.clone() }).unwrap(),
            Response::Ok
        );
        let cli_focus = AtomicBool::new(false);
        let mut out = Vec::new();
        forward(&mut att, &mut out, &cli_focus).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(cli_focus.load(Ordering::Acquire));
        // 补发画面里重放 CLI 的 ?1004h 一次，我们不再另开；结束时撤销 CLI 的模式里关一次，我们不再另关。
        assert_eq!(count(&text, "\x1b[?1004h"), 1, "{text:?}");
        assert_eq!(count(&text, "\x1b[?1004l"), 1, "{text:?}");
        cleanup(&dir, &id);
    }

    #[test]
    fn unknown_event_kinds_are_skipped() {
        let (mut daemon_side, client_side) = UnixStream::pair().unwrap();
        protocol::write_frame(
            &mut daemon_side,
            KIND_EVENT,
            br#"{"from_the_future":{"id":"a"}}"#,
        )
        .unwrap();
        let bell = HostEvent::Bell {
            id: SessionId::from("a"),
        };
        protocol::write_json(&mut daemon_side, KIND_EVENT, &bell).unwrap();
        drop(daemon_side);
        let mut events = EventStream(client_side);
        assert_eq!(
            events.next(Some(Duration::from_secs(5))).unwrap(),
            Some(bell)
        );
        assert_eq!(events.next(Some(Duration::from_secs(5))).unwrap(), None);
    }
}
