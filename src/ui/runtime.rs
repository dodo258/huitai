//! 真终端里跑会话台：进出备用屏、事件循环、后台线程（预览、额度、托管事件、开标签页等动作），
//! 把 [`Effect`] 落到 index / opener / host / quota / sources 的接口上。

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::io::{self, Read, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};

use crate::core::{Config, QuotaReading, Session, SessionId, SessionState, Vendor, VendorDirs};
use crate::host::client::BoardItem;
use crate::host::{
    self, BoardInfo, HostEvent, HostedInfo, Request, Response, RunRecord, ScreenSnapshot, Snapshot,
    SpawnSpec,
};
use crate::ime::{Switcher, Tis};
use crate::index::{self, Entry, Index, Update};
use crate::notify::{self, Activity, Desktop, Message, Notifier, OscKind, SystemNotifier};
use crate::opener::{
    self, InlineOpener, KakuOpener, LoginItem, Opener, Outcome, Tabs, Terminal as Term,
};
use crate::quota;
use crate::sources::{self, Command, Source, Transcript, Trash, hermes, kimi, opencode};

use super::app::{App, Effect, Preview};
use super::text::{project_label, resume_line, truncate};
use super::theme::Theme;
use super::view;

/// 预览读几条消息。
const PREVIEW_MESSAGES: usize = 8;
/// 搜索最多列几条。
const SEARCH_LIMIT: usize = 300;
/// 当前那家额度多久重读一次。
const QUOTA_EVERY: Duration = Duration::from_secs(60);
/// 多久看一次本机进程（认出桌面端、别的终端开着的对话）。
const PROCS_EVERY: Duration = Duration::from_secs(5);
/// 守护进程没在跑时多久再试着连一次。
const HOST_RETRY: Duration = Duration::from_secs(2);
/// 守护进程是不认订阅的旧版时，多久用 `List` 重读一次托管对话。
const HOSTED_POLL: Duration = Duration::from_secs(5);
/// 没有按键时事件循环多久醒一次，收后台线程的消息。
const TICK: Duration = Duration::from_millis(120);
/// 相对时间多久刷新一次。
const CLOCK: Duration = Duration::from_secs(30);
/// 新建时定不了 id 的那几家（Codex、Hermes），新对话的临时托管 id 是 `new-<存储名>-<毫秒时间戳>`。
const TEMP_PREFIX: &str = "new-";
/// 实时预览多久取一次画面。
const LIVE_EVERY: Duration = Duration::from_millis(500);
/// 按 R 重开被中断的对话时带进去的第一句话（方案「中断恢复」节）。
pub const CONTINUE_MESSAGE: &str =
    "继续上次没做完的事。先检查现场（比如有没有执行到一半的命令），再接着做。";
/// git / open 这类子进程的超时。
const CHILD_TIMEOUT: Duration = Duration::from_secs(5);

/// 后台线程发给界面的消息。
enum Msg {
    Index(Update),
    Preview(SessionId, Preview),
    /// 整段对话读到了：哪条、读的时候它的最后时间、全文。
    Transcript(SessionId, SystemTime, Result<Transcript, String>),
    /// 实时预览取到的新画面；`None` 表示这条取不到了（已退出、守护进程不在）。
    Live(SessionId, Option<ScreenSnapshot>),
    Quota(Vendor, QuotaReading),
    Hosted(Vec<HostedInfo>),
    /// 守护进程记着的被中断对话（全部重报）。
    Interrupted(Vec<RunRecord>),
    Host(HostEvent),
    /// 连上的守护进程比当前程序旧（之后的 `Hosted` 记成它托管着的对话）。
    OldHost(bool),
    HostGone,
    Status(String),
    Git(PathBuf, Result<String, String>),
    /// 查到有新版（版本号）。
    Update(String),
    /// 本机进程参数里带着的会话 id（见 [`procs_worker`]）。
    Procs(HashSet<SessionId>),
}

/// 交给动作线程的活。
enum Job {
    Open(Box<Entry>),
    New(Vendor, PathBuf),
    RestoreTabs,
    ResumeInterrupted(Vec<Entry>),
    Dismiss(SessionId),
    /// 列表里被中断的这条已经在它自己的 id 下重新跑起来了：把守护进程用别的 id（临时 id）记的那条被中断记录清掉。
    Restarted(SessionId),
    Kill(SessionId),
    Trash(Box<Entry>),
    Untrash(Vendor),
    Finder(PathBuf),
    Git(PathBuf),
    Copy(Box<Entry>),
}

/// 查有没有新版：返回要提示的版本号。在后台线程里调，可能联网（由 `cli` 给）。
pub type UpdateCheck = Box<dyn FnOnce() -> Option<String> + Send>;

/// `ai`：打开会话台，直到按 q。设置里开着「检查新版本」时在后台调一次 `update_check`。
pub fn run(update_check: UpdateCheck) -> io::Result<()> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "取不到 HOME"))?;
    let data_dir = Config::data_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "取不到数据目录"))?;
    let launch_dir = std::env::current_dir().unwrap_or_else(|_| home.clone());
    let (config, config_problem) = match Config::default_path().map(|p| Config::load(&p)) {
        Some(Ok(c)) => (c, None),
        Some(Err(e)) => (
            Config::default(),
            Some(format!("设置没读到，先用默认值：{e}")),
        ),
        None => (Config::default(), None),
    };
    let index = Index::open_default()?;
    let mut app = App::new(home.clone(), launch_dir, config);
    app.autostart = LoginItem::default_location().is_some_and(|l| l.is_enabled());
    app.set_expanded_projects(index.expanded_projects());
    app.set_projects(index.projects());
    // 缓存读不了（坏了）也照常起来：先空着，后台查出来重建、扫完再填上。
    let mut list_problem = None;
    for v in Vendor::ALL {
        match index.list(v) {
            Ok(entries) => app.set_entries(v, entries),
            Err(e) => list_problem = Some(format!("列表缓存没读到，后台正在重建：{e}")),
        }
    }
    app.written = index::recent_writes(app.lists.iter().flat_map(|l| &l.entries), app.now);
    // 没装的那家（数据目录不在）不显示标签。
    let dirs = VendorDirs::from_env(&home);
    app.hide_missing(|v| dirs.of(v).1.exists());
    app.status = config_problem
        .or(list_problem)
        .or_else(|| index.problems().first().cloned());

    let theme = Theme::detect();
    let ctx = Ctx::new(home, data_dir, index.clone())?;
    let alert_kind = std::env::var("TERM_PROGRAM")
        .ok()
        .and_then(|t| notify::osc_kind(&t));
    let board = board_info(alert_kind.is_some());
    let (tx, rx) = mpsc::channel();
    let (_background, updates) = index.start();
    forward(updates, tx.clone(), Msg::Index);
    let previews = preview_worker(ctx.clone(), tx.clone());
    let transcripts = transcript_worker(ctx.clone(), tx.clone());
    let live = live_worker(ctx.data_dir.clone(), tx.clone());
    let quotas = quota_worker(tx.clone());
    procs_worker(tx.clone());
    let seen = seen_worker(ctx.data_dir.clone());
    let away = Arc::new(Mutex::new(false));
    host_watcher(ctx.clone(), board, Arc::clone(&away), tx.clone());
    if app.config.check_updates {
        let tx = tx.clone();
        thread::spawn(move || {
            if let Some(v) = update_check() {
                let _ = tx.send(Msg::Update(v));
            }
        });
    }
    let jobs = job_worker(ctx.clone(), index.clone(), tx);
    let _ = quotas.send(app.vendor);

    let mut ime = Switcher::new(Tis, app.config.auto_ascii_input);
    let mut tui = Tui::enter()?;
    // Ghostty、Kaku、cmux（内嵌 Ghostty）会报焦点：会话台是在前台的标签页里敲 `ai`（或呼出键）起来的，当作已在前台；
    // 别的终端等它报来焦点再切，不报焦点的终端就一直不切（离开时没人告诉我们切回）。
    if matches!(ctx.terminal, Term::Ghostty | Term::Kaku | Term::Cmux)
        && let Err(e) = ime.focus_gained()
    {
        app.status = Some(format!("输入法没切成：{e}"));
    }
    let mut lp = Loop {
        ime,
        app,
        index,
        ctx,
        theme,
        notifier: Notifier::new(),
        alert_kind,
        away,
        last_selected: None,
        previews,
        transcripts,
        live,
        live_sent: None,
        quotas,
        seen,
        jobs,
        rx,
        stopped: Vec::new(),
    };
    let result = lp.run(&mut tui);
    // 正常退出和出错退出都切回原来的输入源。
    let _ = lp.ime.leave();
    drop(tui);
    result
}

/// 一条托管在临时 id 下的新对话（新建时定不了 id 的那几家，见 `VendorInfo::presets_new_id`）。
#[derive(Debug, Clone)]
struct NewSession {
    vendor: Vendor,
    cwd: PathBuf,
    /// 新建那一刻这个项目里已有的这家对话（这些肯定不是新建的那条）；守护进程报来的旧对话不知道，是空的。
    existing: HashSet<SessionId>,
}

/// 动作要用的环境：会话来源（拼恢复/新建命令、读预览、回收站）、数据目录、自己的路径。
#[derive(Clone)]
struct Ctx {
    home: PathBuf,
    data_dir: PathBuf,
    exe: PathBuf,
    terminal: Term,
    /// 会话台新建、托管在临时 id 下的对话：临时 id → 厂商、项目目录和新建那一刻这个项目里已有的这家对话。
    known: Arc<Mutex<HashMap<SessionId, NewSession>>>,
    /// 列表里的被中断对话 → 守护进程记它用的 id（会话台新建的 Codex 对话是临时 id，其余相同）。
    interrupted: Arc<Mutex<HashMap<SessionId, SessionId>>>,
    /// 取项目显示名（标签页标题前缀）。
    index: Index,
}

impl Ctx {
    fn new(home: PathBuf, data_dir: PathBuf, index: Index) -> io::Result<Self> {
        Ok(Self {
            home,
            data_dir,
            index,
            exe: std::env::current_exe()?,
            terminal: Term::current(),
            known: Arc::default(),
            interrupted: Arc::default(),
        })
    }

    fn sources(&self) -> io::Result<Vec<Box<dyn Source>>> {
        let trash = Trash::default_location()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "取不到回收站位置"))?;
        Ok(sources::default_sources(&self.home, trash))
    }

    /// 项目在标签页标题、托管标题前缀里的名字：起过显示名用显示名。
    fn label(&self, cwd: &Path) -> String {
        project_label(cwd, &self.index.projects())
    }

    fn launch_daemon(&self) -> Process {
        ctx_launch(&self.exe)
    }

    /// 登记给守护进程托管（守护进程没在跑就拉起），返回标签页里要跑的 `ai attach` 命令。
    fn register(
        &self,
        id: &SessionId,
        vendor: Vendor,
        cwd: &Path,
        mut cmd: Command,
    ) -> io::Result<Command> {
        if vendor == Vendor::Claude
            && let Some(settings) = quota::claude_settings(cwd)
        {
            // 会话台启动的 Claude 带上状态栏注入，额度读数才有第一渠道（见 quota 模块）。
            // 放在最前面：带话恢复时那句话要留在最后当位置参数。
            cmd.args.splice(0..0, ["--settings".to_owned(), settings]);
        }
        host::client::ensure_daemon(&self.data_dir, self.launch_daemon())?;
        let spec = SpawnSpec {
            id: id.clone(),
            vendor,
            project: self.label(cwd),
            cwd: cwd.to_path_buf(),
            command: format!(
                "{}{}",
                sources::env_prefix(&VendorDirs::from_env(&self.home), vendor, &self.home),
                cmd.shell_line()
            ),
        };
        match host::client::request(&self.data_dir, &Request::Spawn(spec))? {
            Response::Ok => Ok(opener::attach_command(&self.exe, id, cwd)),
            Response::Error(e) => Err(io::Error::other(e)),
            other => Err(io::Error::other(format!("守护进程回答不对：{other:?}"))),
        }
    }
}

/// `root` 下的子目录（不含隐藏的），按名字排。N 的候选用。
fn subdirs(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(root)?
        .filter_map(Result::ok)
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        // 跟着符号链接看：链到别处的项目目录也算。
        .filter(|p| p.is_dir())
        .collect();
    out.sort();
    Ok(out)
}

/// `ai daemon`。
fn ctx_launch(exe: &Path) -> Process {
    let mut c = Process::new(exe);
    c.arg("daemon");
    c
}

/// 问守护进程要被中断的对话。
fn interrupted_list(dir: &Path) -> io::Result<Vec<RunRecord>> {
    match host::client::request(dir, &Request::Interrupted)? {
        Response::Interrupted(list) => Ok(list),
        other => Err(io::Error::other(format!("守护进程回答不对：{other:?}"))),
    }
}

fn source_for(sources: &[Box<dyn Source>], v: Vendor) -> io::Result<&dyn Source> {
    sources
        .iter()
        .find(|s| s.vendor() == v)
        .map(|s| s.as_ref())
        .ok_or_else(|| io::Error::other(format!("没有 {v} 的会话来源")))
}

/// 恢复一条已有对话：登记托管，返回 attach 命令。
fn register_resume(ctx: &Ctx, sources: &[Box<dyn Source>], s: &Session) -> io::Result<Command> {
    let cmd = source_for(sources, s.vendor)?.resume_cmd(s);
    ctx.register(&s.id, s.vendor, &s.cwd, cmd)
}

/// 守护进程记的被中断对话对到列表里的哪条：返回「列表里的会话 id → 守护进程用的 id」。
/// 多数就是同一个 id；会话台新建的 Codex 对话记的是临时 id，只认回「同一项目、id 在那次进程启动之后才生成、
/// 不是别的被中断对话」的唯一一条（见 [`is_new_since`]）。认出不止一条、一条都没有、或同一条被几条记录认领，
/// 都不列：宁可不列，也不能把「继续」发给别的对话。
fn resolve_interrupted<'a>(
    records: &[RunRecord],
    entries_of: impl Fn(Vendor) -> &'a [Entry],
) -> HashMap<SessionId, SessionId> {
    let recorded: HashSet<&SessionId> = records.iter().map(|r| &r.id).collect();
    let mut out = HashMap::new();
    // 列表里的会话 id → 认它的临时 id 记录。
    let mut claims: HashMap<SessionId, Vec<SessionId>> = HashMap::new();
    for r in records {
        let entries = entries_of(r.vendor);
        if entries.iter().any(|e| e.session.id == r.id) {
            out.insert(r.id.clone(), r.id.clone());
            continue;
        }
        let Some(started) = temp_started(&r.id) else {
            continue;
        };
        // 记录里的进程启动时间（秒，向下取整）比新建时刻更紧：这次跑的那条 id 一定在它之后生成。
        let since = started.max(SystemTime::UNIX_EPOCH + Duration::from_secs(r.started_at));
        let mut found = entries.iter().filter(|e| {
            !recorded.contains(&e.session.id) && is_new_since(&e.session, &r.cwd, since, None)
        });
        if let (Some(e), None) = (found.next(), found.next()) {
            claims
                .entry(e.session.id.clone())
                .or_default()
                .push(r.id.clone());
        }
    }
    for (entry, recs) in claims {
        if let [one] = recs.as_slice() {
            out.entry(entry).or_insert_with(|| one.clone());
        }
    }
    out
}

/// 被中断的对话带那句话重开：登记托管（命令带 [`CONTINUE_MESSAGE`]），返回 attach 命令。
fn register_continue(ctx: &Ctx, sources: &[Box<dyn Source>], s: &Session) -> io::Result<Command> {
    let cmd = source_for(sources, s.vendor)?.resume_with_message_cmd(s, CONTINUE_MESSAGE);
    ctx.register(&s.id, s.vendor, &s.cwd, cmd)
}

/// 新对话：Claude、pi 用 `--session-id` 指定会话 id，托管 id 就是将来列表里那条的 id，
/// 再从列表打开它时守护进程里已经有这条，不会起第二个进程。Codex 不能指定 id，用临时 id 托管，
/// 打开时由 [`hosted_alias`] 认回来。`existing` 是这个项目里已有的这家对话的 id。
fn register_new(
    ctx: &Ctx,
    sources: &[Box<dyn Source>],
    v: Vendor,
    cwd: &Path,
    existing: HashSet<SessionId>,
) -> io::Result<(SessionId, Command)> {
    let base = source_for(sources, v)?.new_cmd(cwd);
    let millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let (id, cmd) = new_session_command(v, base, millis, random_uuid()?);
    if !v.info().presets_new_id {
        let new = NewSession {
            vendor: v,
            cwd: cwd.to_path_buf(),
            existing,
        };
        lock(&ctx.known).insert(id.clone(), new);
    }
    let attach = ctx.register(&id, v, cwd, cmd)?;
    Ok((id, attach))
}

/// 新对话的托管 id 和命令（纯函数，便于测试）。
fn new_session_command(
    v: Vendor,
    mut cmd: Command,
    millis: u128,
    uuid: String,
) -> (SessionId, Command) {
    if v.info().presets_new_id {
        cmd.args.push("--session-id".to_owned());
        cmd.args.push(uuid.clone());
        (SessionId::from(uuid), cmd)
    } else {
        (
            SessionId::from(format!("{TEMP_PREFIX}{}-{millis}", v.key())),
            cmd,
        )
    }
}

/// 随机 UUID v4（读 /dev/urandom）。
fn random_uuid() -> io::Result<String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    ))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 打开一条对话时它实际托管在哪个 id 下。
#[derive(Debug, PartialEq, Eq)]
enum Hosted {
    /// 用它自己的 id（没托管就新登记）。
    Own,
    /// 它就是会话台新建、托管在这个临时 id 下的 Codex 对话。
    Alias(SessionId),
    /// 它可能是会话台新建、还在跑的 Codex 对话，但确认不了是哪条（或是不是）。
    Ambiguous,
}

/// Codex 会话 id 是 UUIDv7（本机 0.171 实测），开头 48 位是生成它的毫秒时间；不是 v7 的返回 `None`。
fn codex_id_created(id: &SessionId) -> Option<SystemTime> {
    let b = id.as_str().as_bytes();
    let dashes = [8, 13, 18, 23].iter().all(|&i| b.get(i) == Some(&b'-'));
    if b.len() != 36 || !dashes || b[14] != b'7' {
        return None;
    }
    let hex: Vec<u8> = b[..13].iter().copied().filter(|&c| c != b'-').collect();
    if !hex.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let ms = u64::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
}

/// 对话生成时刻（新建时定不了 id 的那几家）：Codex、Hermes 从 id 看出，Kimi Code 的 id 看不出、读它的
/// `state.json`。Hermes 的 id 只精确到秒，取那一秒的最后一刻，免得同一秒里稍晚新建的那条被当成新建之前就有的。
/// 看不出的返回 `None`。
fn id_created(s: &Session) -> Option<SystemTime> {
    match s.vendor {
        Vendor::Codex => codex_id_created(&s.id),
        Vendor::Hermes => hermes::id_created(s.id.as_str()).map(|t| t + Duration::from_millis(999)),
        Vendor::Kimi => kimi::created_at(&s.path),
        Vendor::OpenCode => opencode::id_created(s.id.as_str(), s.last_active),
        Vendor::Claude | Vendor::Pi | Vendor::Gemini | Vendor::Qwen => None,
    }
}

/// `s` 会不会是 `since` 那一刻在项目 `cwd` 里新建的对话：同一项目、id 是那之后才生成的
/// （新建之前就有的对话，不管后来在别处聊没聊，都不可能是它），且不在新建那一刻已有的对话 `existing` 里。
/// id 看不出生成时间的一律不认。
fn is_new_since(
    s: &Session,
    cwd: &Path,
    since: SystemTime,
    existing: Option<&HashSet<SessionId>>,
) -> bool {
    s.cwd == cwd
        && id_created(s).is_some_and(|c| c >= since)
        && !existing.is_some_and(|e| e.contains(&s.id))
}

/// Codex 新对话托管在临时 id 下；它写出会话记录后列表里出现的是真实 id。
/// 一条不在托管里的对话，只有同时满足这两条才认作那个临时 id、接到那个进程上（不再起第二个 `codex resume`）：
/// 在跑的临时 id 里只有一个可能是它（见 [`is_new_since`]），且列表里（`listed`，这家的全部对话）
/// 除它以外没有别的对话也可能是那个临时 id。可能是它但确认不了时是 [`Hosted::Ambiguous`]：绝不接错。
fn hosted_alias(
    s: &Session,
    hosted: &[HostedInfo],
    known: &HashMap<SessionId, NewSession>,
    listed: &[Entry],
) -> Hosted {
    let own = |id: &SessionId| hosted.iter().any(|h| h.id == *id);
    if own(&s.id) {
        return Hosted::Own;
    }
    let may_be = |h: &HostedInfo, x: &Session| {
        h.vendor == x.vendor
            && h.pid.is_some()
            && temp_started(&h.id)
                .is_some_and(|t| is_new_since(x, &h.cwd, t, known.get(&h.id).map(|n| &n.existing)))
    };
    let temps: Vec<&HostedInfo> = hosted.iter().filter(|h| may_be(h, s)).collect();
    match temps.as_slice() {
        [] => Hosted::Own,
        [h] if !listed
            .iter()
            .any(|e| e.session.id != s.id && !own(&e.session.id) && may_be(h, &e.session)) =>
        {
            Hosted::Alias(h.id.clone())
        }
        _ => Hosted::Ambiguous,
    }
}

/// 会话台新建、托管在临时 id 下（`running` 为真）的对话写出会话记录后，列表里是真实 id、看起来没托管；
/// 用打开时同一套认法（[`hosted_alias`]）找出这些真实 id（含认不准的，界面不把它们当成别处在用）。
/// `listed` 是 `vendor` 这家的全部对话。
fn aliased(
    vendor: Vendor,
    listed: &[Entry],
    known: &HashMap<SessionId, NewSession>,
    running: impl Fn(&SessionId) -> bool,
) -> HashSet<SessionId> {
    let hosted: Vec<HostedInfo> = known
        .iter()
        .filter(|(id, n)| n.vendor == vendor && running(id))
        .map(|(id, n)| HostedInfo {
            id: id.clone(),
            vendor,
            project: String::new(),
            cwd: n.cwd.clone(),
            pid: Some(0),
            attached: false,
            title: None,
        })
        .collect();
    if hosted.is_empty() {
        return HashSet::new();
    }
    listed
        .iter()
        .filter(|e| hosted_alias(&e.session, &hosted, known, listed) != Hosted::Own)
        .map(|e| e.session.id.clone())
        .collect()
}

/// 临时 id 里记的新建时间。
fn temp_started(id: &SessionId) -> Option<SystemTime> {
    let (key, ms) = id.as_str().strip_prefix(TEMP_PREFIX)?.rsplit_once('-')?;
    Vendor::ALL
        .iter()
        .any(|v| v.key() == key && !v.info().presets_new_id)
        .then_some(())?;
    let ms: u64 = ms.parse().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
}

/// 打开一条已有对话：返回托管 id、标签页里要跑的 attach 命令，以及这次是不是新登记的托管
/// （之前守护进程里没有这条；标签页没开成时要撤掉，见 [`Worker::open_tab`]）。
fn attach_existing(
    ctx: &Ctx,
    sources: &[Box<dyn Source>],
    s: &Session,
) -> io::Result<(SessionId, Command, bool)> {
    let hosted = hosted_list(ctx);
    let listed = ctx.index.list(s.vendor).unwrap_or_default();
    match hosted_alias(s, &hosted, &lock(&ctx.known), &listed) {
        Hosted::Own => {
            let fresh = !hosted.iter().any(|h| h.id == s.id);
            Ok((s.id.clone(), register_resume(ctx, sources, s)?, fresh))
        }
        Hosted::Alias(id) => {
            let cmd = opener::attach_command(&ctx.exe, &id, &s.cwd);
            Ok((id, cmd, false))
        }
        Hosted::Ambiguous => Err(io::Error::other(format!(
            "这个项目里有会话台新建的 {} 对话还在跑，确认不了是不是这条；请到那个标签页里继续，或等它结束再打开",
            s.vendor
        ))),
    }
}

/// 守护进程托管着的对话；守护进程没在跑就没有。
fn hosted_list(ctx: &Ctx) -> Vec<HostedInfo> {
    match host::client::request(&ctx.data_dir, &Request::List) {
        Ok(Response::List(h)) => h,
        _ => Vec::new(),
    }
}

/// 这个项目里已有的这家对话 id。
fn ids_in(index: &Index, v: Vendor, cwd: &Path) -> HashSet<SessionId> {
    index
        .list(v)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.session.cwd == cwd)
        .map(|e| e.session.id)
        .collect()
}

// ---- 终端 ----

type Backend = CrosstermBackend<Stdout>;

struct Tui {
    terminal: Terminal<Backend>,
}

impl Tui {
    fn enter() -> io::Result<Self> {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            leave_screen();
            hook(info);
        }));
        enter_screen()?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        terminal.hide_cursor()?;
        terminal.clear()?;
        Ok(Self { terminal })
    }

    /// 暂时交出终端（在当前窗口跑 attach、编辑器），回来后整屏重画。
    fn suspend<T>(&mut self, f: impl FnOnce() -> T) -> io::Result<T> {
        leave_screen();
        let out = f();
        enter_screen()?;
        self.terminal.clear()?;
        Ok(out)
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        leave_screen();
    }
}

/// 终端认不认 kitty 键盘增强（第一次进屏时问一次）。认的话开 DISAMBIGUATE，⌥⏎ 这类组合键分得清。
static KEYBOARD_ENHANCED: OnceLock<bool> = OnceLock::new();

fn enter_screen() -> io::Result<()> {
    enable_raw_mode()?;
    let enhanced =
        *KEYBOARD_ENHANCED.get_or_init(|| supports_keyboard_enhancement().unwrap_or(false));
    let mut out = io::stdout();
    if enhanced {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableFocusChange,
        ratatui::crossterm::cursor::Hide
    )
}

/// 交出终端、退出、崩溃时都走这里：关掉自己开的终端模式（交给对话的终端要干净）。
fn leave_screen() {
    if KEYBOARD_ENHANCED.get() == Some(&true) {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        io::stdout(),
        DisableFocusChange,
        DisableMouseCapture,
        LeaveAlternateScreen,
        ratatui::crossterm::cursor::Show
    );
    let _ = disable_raw_mode();
}

// ---- 事件循环 ----

struct Loop {
    app: App,
    /// 进会话台切英文输入、离开切回（只在主线程调）。
    ime: Switcher<Tis>,
    index: Index,
    ctx: Ctx,
    theme: Theme,
    /// 只用来算状态灯；提醒由守护进程算（只算一份），要弹时发 `HostEvent::Alert` 过来。
    notifier: Notifier,
    /// 会话台所在终端认的通知序列；`None` 时守护进程不会把提醒交给会话台。
    alert_kind: Option<OscKind>,
    /// 交出终端（当前窗口里接对话、开编辑器）期间为 `true`：主循环停着，守护进程送来的提醒
    /// 写不进自己标签页，`host_watcher` 改发系统通知。和 `host_watcher` 转发事件共用这把锁，切换时不漏不压。
    away: Arc<Mutex<bool>>,
    /// 上一轮选中的对话，选中换了才告诉守护进程「看过了」。
    last_selected: Option<SessionId>,
    previews: Sender<Entry>,
    /// 读整段对话的线程。
    transcripts: Sender<Entry>,
    live: Sender<Option<SessionId>>,
    /// 上次告诉实时预览线程的目标。
    live_sent: Option<SessionId>,
    quotas: Sender<Vendor>,
    seen: Sender<SessionId>,
    jobs: Sender<Job>,
    rx: Receiver<Msg>,
    /// 守护进程记着的被中断对话。
    stopped: Vec<RunRecord>,
}

impl Loop {
    fn run(&mut self, tui: &mut Tui) -> io::Result<()> {
        let mut dirty = true;
        let mut last_draw = Instant::now();
        let started = Instant::now();
        while !self.app.quit {
            dirty |= self.app.tick_blink(started.elapsed());
            if dirty || last_draw.elapsed() >= CLOCK {
                self.app.now = SystemTime::now();
                tui.terminal
                    .draw(|f| view::draw(f, &mut self.app, &self.theme))?;
                last_draw = Instant::now();
                dirty = false;
            }
            if event::poll(TICK)? {
                let vendor = self.app.vendor;
                let effects = match event::read()? {
                    Event::Key(k) => self.app.key(k),
                    Event::Mouse(m) => self.app.mouse(m, Instant::now()),
                    Event::FocusGained => {
                        let r = self.ime.focus_gained();
                        self.ime_result(r);
                        Vec::new()
                    }
                    Event::FocusLost => {
                        let r = self.ime.focus_lost();
                        self.ime_result(r);
                        Vec::new()
                    }
                    _ => Vec::new(),
                };
                dirty = true;
                for e in effects {
                    self.apply(e, tui)?;
                }
                let r = self.ime.set_enabled(self.app.config.auto_ascii_input);
                self.ime_result(r);
                if self.app.vendor != vendor {
                    let _ = self.quotas.send(self.app.vendor);
                }
            }
            while let Ok(msg) = self.rx.try_recv() {
                self.receive(msg);
                dirty = true;
            }
            for id in self.notifier.settle(Instant::now()) {
                self.app.set_state(id.clone(), self.notifier.state(&id));
                dirty = true;
            }
            if let Some(e) = self.app.take_preview_request() {
                let _ = self.previews.send(e);
            }
            if let Some(e) = self.app.take_transcript_request() {
                let _ = self.transcripts.send(e);
            }
            self.set_live(self.app.live_target());
            self.mark_seen();
        }
        self.set_live(None);
        Ok(())
    }

    /// 输入源没切成：在底栏说一句，不挡别的。
    fn ime_result(&mut self, r: io::Result<()>) {
        if let Err(e) = r {
            self.app.status = Some(format!("输入法没切成：{e}"));
        }
    }

    /// 要去对话标签页了：先把输入源切回去（中文输入法留给对话用）。
    fn leave_for(&mut self, job: Job) -> Job {
        let r = self.ime.leave();
        self.ime_result(r);
        job
    }

    /// 换实时预览的目标；`None` 让预览线程停下不再取画面。
    fn set_live(&mut self, target: Option<SessionId>) {
        if target == self.live_sent {
            return;
        }
        if self.app.live.as_ref().map(|(id, _)| id) != target.as_ref() {
            self.app.live = None;
        }
        let _ = self.live.send(target.clone());
        self.live_sent = target;
    }

    /// 交出终端（当前窗口里接对话、开编辑器）期间会话台看不见，停掉实时预览；回来后主循环会重新设目标。
    /// 这段时间主循环不收消息：先标记「不在」，之后的提醒由 `host_watcher` 直接发系统通知；
    /// 标记前已排进队列的消息当场处理掉（其中的提醒同样改发系统通知），不压到回来以后。
    fn suspend<T>(&mut self, tui: &mut Tui, f: impl FnOnce() -> T) -> io::Result<T> {
        self.set_live(None);
        let r = self.ime.leave();
        self.ime_result(r);
        *lock(&self.away) = true;
        while let Ok(msg) = self.rx.try_recv() {
            self.receive(msg);
        }
        let result = tui.suspend(f);
        *lock(&self.away) = false;
        let r = self.ime.back();
        self.ime_result(r);
        result
    }

    /// 选中「刚做完」的对话，本地状态灯就算看过了。用户把选中挪到一条托管对话上（右侧开始预览它），
    /// 再告诉守护进程看过了，它那份提醒判断不再为这一轮弹提醒。选中不动、对话在那时做完的不算：
    /// 会话台可能正放在后台没人看，提醒照发。
    fn mark_seen(&mut self) {
        let selected = self.app.selected_entry().map(|e| e.session.id.clone());
        let changed = selected != self.last_selected;
        self.last_selected.clone_from(&selected);
        let Some(id) = selected else {
            return;
        };
        if self.app.state_of(&id) == SessionState::Done {
            self.notifier
                .notify(id.clone(), Activity::Seen, Instant::now());
            self.app.set_state(id.clone(), self.notifier.state(&id));
        }
        if changed && self.app.states.contains_key(&id) {
            let _ = self.seen.send(id);
        }
    }

    /// 守护进程请会话台代弹的提醒：往自己标签页写 OSC 9/777，终端自己弹（点它回到会话台）。
    /// 交出终端期间写不进去，改发系统通知。
    fn show_alert(&self, title: String, body: String) {
        let Some(kind) = self.alert_kind else {
            return;
        };
        let msg = Message { title, body };
        if *lock(&self.away) {
            system_alert(msg);
            return;
        }
        let seq = notify::osc_sequence(kind, &msg);
        let mut out = io::stdout();
        let _ = out.write_all(&seq).and_then(|()| out.flush());
    }

    fn reload(&mut self, v: Vendor) {
        match self.index.list(v) {
            Ok(entries) => self.app.set_entries(v, entries),
            Err(e) => self.app.status = Some(format!("列表没读到：{e}")),
        }
        // 会话台开着时第一次用这家（之前没有数据目录、标签藏着）：读到它的对话就把标签补回来。
        if !self.app.shown.contains(&v) && !self.app.list_of(v).entries.is_empty() {
            let dirs = VendorDirs::from_env(&self.app.home);
            self.app.hide_missing(|x| dirs.of(x).1.exists());
        }
        // 会话文件有变化才会走到这里，顺带重查最近在写的文件（判断「别处在用」）。
        let lists = &self.app.lists;
        self.app.written =
            index::recent_writes(lists.iter().flat_map(|l| &l.entries), SystemTime::now());
        self.refresh_interrupted();
    }

    /// 托管在临时 id 下的 Codex 新对话认回列表里的真实 id（见 [`aliased`]），界面不把它们当成别处在用。
    fn refresh_aliases(&mut self) {
        let app = &self.app;
        let known = lock(&self.ctx.known);
        let ids = Vendor::ALL
            .into_iter()
            .filter(|v| !v.info().presets_new_id)
            .flat_map(|v| {
                aliased(v, &app.list_of(v).entries, &known, |id| {
                    app.state_of(id) != SessionState::Stopped
                })
            })
            .collect();
        drop(known);
        self.app.aliased = ids;
    }

    /// 托管列表整个换掉（重新同步、守护进程没了）之前：原来托管着、之后不在的，记下结束时间。
    fn forget_hosted(&mut self, keep: &HashSet<SessionId>) {
        let now = SystemTime::now();
        let gone: Vec<SessionId> = self
            .app
            .states
            .keys()
            .filter(|id| !keep.contains(*id))
            .cloned()
            .collect();
        for id in gone {
            self.app.ended(id, now);
        }
        self.app.states.clear();
        self.app.attached.clear();
    }

    /// 被中断记录或列表变了：重新对到列表里，更新「被中断」组和动作线程用的对照表。
    fn refresh_interrupted(&mut self) {
        let app = &self.app;
        let map = resolve_interrupted(&self.stopped, |v| app.list_of(v).entries.as_slice());
        self.app.set_interrupted(map.keys().cloned().collect());
        *lock(&self.ctx.interrupted) = map;
    }

    /// 开 / 关「开机自动打开会话台」：写或删登录项。没做成就把开关改回去。
    fn set_autostart(&mut self, on: bool) {
        let result = LoginItem::default_location()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "取不到 HOME"))
            .and_then(|item| {
                if !on {
                    return item.disable().map(|()| "已关闭开机自动打开".to_owned());
                }
                let terminal = match self.app.config.preferred_terminal.as_deref() {
                    Some(name) => Term::from_name(name),
                    None => self.ctx.terminal.clone(),
                };
                let shell = std::env::var("SHELL")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "/bin/zsh".to_owned());
                let kaku = KakuOpener::new(|k| std::env::var(k).ok());
                let args = opener::login::launch_args(
                    &terminal,
                    &self.ctx.exe,
                    &shell,
                    &self.ctx.home,
                    kaku.cli(),
                )?;
                item.enable(&args)?;
                Ok(format!("已开启：下次登录用 {terminal} 打开会话台"))
            });
        self.app.status = Some(match result {
            Ok(msg) => msg,
            Err(e) => {
                self.app.autostart = !on;
                format!("没改成：{e}")
            }
        });
    }

    fn search(&mut self, v: Vendor, query: &str) {
        match self.index.search(v, query, SEARCH_LIMIT) {
            Ok(hits) => self
                .app
                .set_hits(v, query, hits.into_iter().map(|h| h.entry).collect()),
            Err(e) => self.app.status = Some(format!("搜索出错：{e}")),
        }
    }

    fn apply(&mut self, effect: Effect, tui: &mut Tui) -> io::Result<()> {
        let job = match effect {
            Effect::Search { vendor, query } => {
                self.search(vendor, &query);
                return Ok(());
            }
            Effect::Annotate { vendor, id, edit } => {
                if let Err(e) = self.index.annotate(vendor, &id, edit) {
                    self.app.status = Some(format!("没存上：{e}"));
                }
                self.reload(vendor);
                if let Some(q) = self.app.search.as_ref().map(|s| s.query.clone())
                    && !q.is_empty()
                {
                    self.search(vendor, &q);
                }
                return Ok(());
            }
            Effect::AnnotateProject { project, edit } => {
                if let Err(e) = self.index.annotate_project(&project, edit) {
                    self.app.status = Some(format!("没存上：{e}"));
                }
                self.app.set_projects(self.index.projects());
                return Ok(());
            }
            Effect::ScanProjectsRoot(root) => {
                // 目录还没有就只列三家出现过的项目；别的读不了也不挡输入。
                self.app.set_root_dirs(subdirs(&root).unwrap_or_default());
                return Ok(());
            }
            Effect::CreateDir { vendor, cwd } => {
                if let Err(e) = std::fs::create_dir_all(&cwd) {
                    self.app.status = Some(format!("目录没建成：{}: {e}", cwd.display()));
                    return Ok(());
                }
                return self.apply(Effect::New { vendor, cwd }, tui);
            }
            Effect::SetExpanded { project, expanded } => {
                if let Err(e) = self.index.set_expanded(&project, expanded) {
                    self.app.status = Some(format!("展开状态没存上：{e}"));
                }
                return Ok(());
            }
            Effect::SaveConfig(cfg) => {
                if let Some(p) = Config::default_path()
                    && let Err(e) = cfg.save(&p)
                {
                    self.app.status = Some(format!("设置没存上：{e}"));
                }
                return Ok(());
            }
            Effect::Open { entry, here } if here || !self.ctx.terminal.has_tabs() => {
                self.app.status = Some(self.open_here(tui, &entry.session)?);
                return Ok(());
            }
            Effect::Editor(dir) => {
                self.app.status = self.suspend(tui, || editor(&dir))?.err();
                return Ok(());
            }
            Effect::Open { entry, .. } => self.leave_for(Job::Open(entry)),
            Effect::New { vendor, cwd } if !self.ctx.terminal.has_tabs() => {
                self.app.status = Some(self.new_here(tui, vendor, &cwd)?);
                return Ok(());
            }
            Effect::New { vendor, cwd } => self.leave_for(Job::New(vendor, cwd)),
            Effect::RestoreTabs => self.leave_for(Job::RestoreTabs),
            Effect::ResumeInterrupted(_) if !self.ctx.terminal.has_tabs() => {
                self.app.status = Some(format!(
                    "{} 开不了新标签页：请逐条选中按 ⏎ 接着做",
                    self.ctx.terminal
                ));
                return Ok(());
            }
            Effect::ResumeInterrupted(entries) => self.leave_for(Job::ResumeInterrupted(entries)),
            Effect::Dismiss(id) => Job::Dismiss(id),
            Effect::SetAutostart(on) => {
                self.set_autostart(on);
                return Ok(());
            }
            Effect::Kill(id) => Job::Kill(id),
            Effect::Trash(e) => Job::Trash(e),
            Effect::Untrash(v) => Job::Untrash(v),
            Effect::Finder(d) => Job::Finder(d),
            Effect::GitStatus(d) => Job::Git(d),
            Effect::CopyResume(e) => Job::Copy(e),
        };
        let _ = self.jobs.send(job);
        Ok(())
    }

    /// 在当前窗口接上对话，CLI 退出或被别处接走后回到会话台。
    fn open_here(&mut self, tui: &mut Tui, s: &Session) -> io::Result<String> {
        let cmd = match self
            .ctx
            .sources()
            .and_then(|src| attach_existing(&self.ctx, &src, s))
        {
            Ok((_, c, _)) => c,
            Err(e) => return Ok(format!("没打开：{e}")),
        };
        let result = self.suspend(tui, || InlineOpener.open(&cmd, ""))?;
        Ok(match result {
            Ok(_) => "已回到会话台".to_owned(),
            Err(e) => format!("对话结束：{e}"),
        })
    }

    fn new_here(&mut self, tui: &mut Tui, v: Vendor, cwd: &Path) -> io::Result<String> {
        let existing = ids_in(&self.index, v, cwd);
        let cmd = match self
            .ctx
            .sources()
            .and_then(|src| register_new(&self.ctx, &src, v, cwd, existing))
        {
            Ok((_, c)) => c,
            Err(e) => return Ok(format!("没新建：{e}")),
        };
        let result = self.suspend(tui, || InlineOpener.open(&cmd, ""))?;
        Ok(match result {
            Ok(_) => "已回到会话台".to_owned(),
            Err(e) => format!("对话结束：{e}"),
        })
    }

    fn receive(&mut self, msg: Msg) {
        let now = Instant::now();
        match msg {
            Msg::Index(Update::Listed(v)) => self.reload(v),
            Msg::Index(Update::Searchable) => {
                if let Some(q) = self.app.search.as_ref().map(|s| s.query.clone())
                    && !q.is_empty()
                {
                    self.search(self.app.vendor, &q);
                }
            }
            Msg::Index(Update::Problem(_)) => {}
            Msg::Preview(id, p) => self.app.set_preview(id, p),
            Msg::Transcript(id, at, t) => self.app.set_transcript(&id, at, t),
            Msg::Live(id, snap) => {
                // 目标已经换了的旧画面丢掉。
                if self.live_sent.as_ref() == Some(&id) {
                    self.app.live = snap.map(|s| (id, s));
                }
            }
            Msg::Quota(v, r) => {
                self.app.quota.insert(v, r);
            }
            Msg::Procs(ids) => {
                self.app.open_elsewhere = ids;
            }
            Msg::Hosted(infos) => {
                if let Some(old) = self.app.old_host.as_mut() {
                    *old = infos.iter().map(|i| i.id.clone()).collect();
                }
                self.notifier = Notifier::new();
                let alive: HashSet<SessionId> = infos
                    .iter()
                    .filter(|i| i.pid.is_some())
                    .map(|i| i.id.clone())
                    .collect();
                self.forget_hosted(&alive);
                let mut known = lock(&self.ctx.known);
                for i in infos.iter().filter(|i| temp_started(&i.id).is_some()) {
                    known.entry(i.id.clone()).or_insert_with(|| NewSession {
                        vendor: i.vendor,
                        cwd: i.cwd.clone(),
                        existing: HashSet::new(),
                    });
                }
                drop(known);
                for i in infos.into_iter().filter(|i| i.pid.is_some()) {
                    self.notifier.notify(i.id.clone(), Activity::Started, now);
                    if i.attached {
                        self.notifier.notify(i.id.clone(), Activity::Attached, now);
                        self.app.attached.insert(i.id.clone());
                    }
                    self.app
                        .states
                        .insert(i.id.clone(), self.notifier.state(&i.id));
                }
            }
            Msg::Interrupted(list) => {
                self.stopped = list;
                self.refresh_interrupted();
            }
            Msg::Host(ev) => self.host_event(ev, now),
            Msg::OldHost(old) => self.app.old_host = old.then(HashSet::new),
            Msg::HostGone => {
                self.app.old_host = None;
                self.notifier = Notifier::new();
                self.forget_hosted(&HashSet::new());
            }
            Msg::Status(s) => self.app.status = Some(s),
            Msg::Git(dir, out) => self.app.git = Some((dir, out)),
            Msg::Update(v) => self.app.new_version = Some(v),
        }
        self.refresh_aliases();
    }

    fn host_event(&mut self, ev: HostEvent, now: Instant) {
        if let HostEvent::Alert { title, body, .. } = ev {
            self.show_alert(title, body);
            return;
        }
        let Some((id, activity)) = activity(ev) else {
            return;
        };
        // 旧版守护进程上新开、结束的对话，顶栏提示的条数跟着变。
        if let Some(old) = self.app.old_host.as_mut() {
            match activity {
                Activity::Started => {
                    old.insert(id.clone());
                }
                Activity::Exited => {
                    old.remove(&id);
                }
                _ => {}
            }
        }
        match activity {
            // 守护进程那边重开就从被中断里拿掉了，这边跟着拿掉。
            Activity::Started if self.stopped.iter().any(|r| r.id == id) => {
                self.stopped.retain(|r| r.id != id);
                self.refresh_interrupted();
            }
            // 守护进程用临时 id 记的被中断 Codex 对话，在真实 id 下重新跑起来了：守护进程对不上，由这边清掉。
            Activity::Started
                if lock(&self.ctx.interrupted)
                    .get(&id)
                    .is_some_and(|host| *host != id) =>
            {
                let _ = self.jobs.send(Job::Restarted(id.clone()));
            }
            Activity::Attached => {
                self.app.attached.insert(id.clone());
            }
            Activity::Detached => {
                self.app.attached.remove(&id);
            }
            Activity::Exited => {
                self.app.attached.remove(&id);
                self.app.ended(id.clone(), SystemTime::now());
            }
            _ => {}
        }
        self.notifier.notify(id.clone(), activity, now);
        self.app.set_state(id.clone(), self.notifier.state(&id));
    }
}

/// 托管事件 → 通知模块的 `Activity`（会话台的状态灯和守护进程的提醒判断共用）；
/// 和状态无关的事件（标题、代弹提醒）返回 `None`。
pub fn activity(ev: HostEvent) -> Option<(SessionId, Activity)> {
    Some(match ev {
        HostEvent::Started { id, .. } => (id, Activity::Started),
        HostEvent::Output { id } => (id, Activity::Output),
        HostEvent::Bell { id } => (id, Activity::Bell),
        HostEvent::Notify { id, title, body } => (id, Activity::Notify { title, body }),
        HostEvent::Attached { id } => (id, Activity::Attached),
        HostEvent::Detached { id } => (id, Activity::Detached),
        HostEvent::Exited { id, .. } => (id, Activity::Exited),
        HostEvent::Seen { id } => (id, Activity::Seen),
        HostEvent::Focus { id, front } => (id, Activity::Focus(front)),
        HostEvent::Title { .. } | HostEvent::Alert { .. } => return None,
    })
}

/// 会话台登记给守护进程的自己：终端、标签页的 tty、Kaku 窗格、命令行（全局呼出键靠这些聚焦它）。
fn board_info(alerts: bool) -> BoardInfo {
    let var = |k| std::env::var(k).ok().filter(|v: &String| !v.is_empty());
    BoardInfo {
        // 记认出来的终端，不记原样的 TERM_PROGRAM：cmux 里它是 ghostty，呼出键会去找另一个 Ghostty。
        term_program: Term::current().name().or_else(|| var("TERM_PROGRAM")),
        tty: own_tty(),
        pane: var("WEZTERM_PANE"),
        command: std::env::args().collect::<Vec<_>>().join(" "),
        alerts,
    }
}

/// 标准输入所在的 tty（会话台所在的标签页）。
fn own_tty() -> Option<String> {
    let mut buf = [0 as libc::c_char; 128];
    // SAFETY: 缓冲区有效、长度如实传入；ttyname_r 成功时写入以 NUL 结尾的路径。
    let rc = unsafe { libc::ttyname_r(0, buf.as_mut_ptr(), buf.len()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: 上面成功返回，buf 里是以 NUL 结尾的字符串。
    let path = unsafe { CStr::from_ptr(buf.as_ptr()) };
    Some(path.to_string_lossy().into_owned())
}

/// `$VISUAL`（其次 `$EDITOR`）在项目目录打开项目。
fn editor(dir: &Path) -> Result<(), String> {
    let set = |k| {
        std::env::var(k)
            .ok()
            .filter(|v: &String| !v.trim().is_empty())
    };
    if set("VISUAL").or_else(|| set("EDITOR")).is_none() {
        return Err("没设 $VISUAL 或 $EDITOR，不知道用哪个编辑器".to_owned());
    }
    let status = Process::new("/bin/sh")
        .arg("-c")
        .arg(r#"${VISUAL:-$EDITOR} "$1""#)
        .arg("sh")
        .arg(dir)
        .current_dir(dir)
        .status()
        .map_err(|e| format!("编辑器没启动：{e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("编辑器退出码 {status}"))
    }
}

// ---- 后台线程 ----

fn forward<T: Send + 'static>(rx: Receiver<T>, tx: Sender<Msg>, wrap: fn(T) -> Msg) {
    thread::spawn(move || {
        for m in rx {
            if tx.send(wrap(m)).is_err() {
                return;
            }
        }
    });
}

/// 读预览：只读最新要的那条，前面没来得及读的跳过。
fn preview_worker(ctx: Ctx, tx: Sender<Msg>) -> Sender<Entry> {
    let (req, rx) = mpsc::channel::<Entry>();
    thread::spawn(move || {
        let sources = ctx.sources();
        while let Ok(mut e) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                e = newer;
            }
            let s = &e.session;
            let messages = sources
                .as_ref()
                .map_err(|err| io::Error::new(err.kind(), err.to_string()))
                .and_then(|src| source_for(src, s.vendor)?.load_preview(s, PREVIEW_MESSAGES))
                .map_err(|err| err.to_string());
            let p = Preview {
                at: s.last_active,
                messages,
            };
            if tx.send(Msg::Preview(s.id.clone(), p)).is_err() {
                return;
            }
        }
    });
    req
}

/// 读整段对话：和预览一样只读最新要的那条。大文件只读最后 32MB，几百毫秒，不挡界面。
fn transcript_worker(ctx: Ctx, tx: Sender<Msg>) -> Sender<Entry> {
    let (req, rx) = mpsc::channel::<Entry>();
    thread::spawn(move || {
        let sources = ctx.sources();
        while let Ok(mut e) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                e = newer;
            }
            let s = &e.session;
            let t = sources
                .as_ref()
                .map_err(|err| io::Error::new(err.kind(), err.to_string()))
                .and_then(|src| source_for(src, s.vendor)?.load_transcript(s))
                .map_err(|err| err.to_string());
            if tx
                .send(Msg::Transcript(s.id.clone(), s.last_active, t))
                .is_err()
            {
                return;
            }
        }
    });
    req
}

/// 实时预览：有目标时每 [`LIVE_EVERY`] 向守护进程取一次那条对话的屏幕，画面没变守护进程只回「没变」，
/// 不发给界面；目标为 `None` 时阻塞等下一个目标，不碰守护进程。
fn live_worker(dir: PathBuf, tx: Sender<Msg>) -> Sender<Option<SessionId>> {
    let (req, rx) = mpsc::channel::<Option<SessionId>>();
    thread::spawn(move || {
        let mut target: Option<SessionId> = None;
        // 上次发给界面的画面版本；`None` 表示还没发过或上次没取到。
        let mut shown: Option<u64> = None;
        let mut failed = false;
        loop {
            let next = match target {
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(_) => rx.recv_timeout(LIVE_EVERY),
            };
            match next {
                Ok(mut t) => {
                    while let Ok(newer) = rx.try_recv() {
                        t = newer;
                    }
                    if t != target {
                        (target, shown, failed) = (t, None, false);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let Some(id) = target.as_ref() else { continue };
            let msg = match host::client::snapshot(&dir, id, shown) {
                Ok(Snapshot::Unchanged) => continue,
                Ok(Snapshot::Screen(s)) => {
                    (shown, failed) = (Some(s.version), false);
                    Msg::Live(id.clone(), Some(s))
                }
                // 取不到只告诉界面一次，之后照样定时再试（守护进程可能马上回来）。
                Err(_) if failed => continue,
                Err(_) => {
                    (shown, failed) = (None, true);
                    Msg::Live(id.clone(), None)
                }
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    });
    req
}

/// 额度：切到哪家就读哪家，之后每分钟重读当前那家。
fn quota_worker(tx: Sender<Msg>) -> Sender<Vendor> {
    let (req, rx) = mpsc::channel::<Vendor>();
    thread::spawn(move || {
        let Ok(mut current) = rx.recv() else {
            return;
        };
        loop {
            if tx.send(Msg::Quota(current, quota::quota(current))).is_err() {
                return;
            }
            match rx.recv_timeout(QUOTA_EVERY) {
                Ok(v) => current = v,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    req
}

/// 每 [`PROCS_EVERY`] 看一次本机进程，把参数里带着会话 id 的报上来；`ps` 跑不了这一轮就跳过。
fn procs_worker(tx: Sender<Msg>) {
    thread::spawn(move || {
        loop {
            if let Ok(out) = run_child(Process::new("/bin/ps").args(["-axww", "-o", "command="])) {
                let ids = sources::live::session_ids(&out)
                    .into_iter()
                    .map(SessionId::from)
                    .collect();
                if tx.send(Msg::Procs(ids)).is_err() {
                    return;
                }
            }
            thread::sleep(PROCS_EVERY);
        }
    });
}

/// 告诉守护进程用户看过了哪条对话。守护进程没在跑就算了（它没在跑也就没有要提醒的）。
fn seen_worker(dir: PathBuf) -> Sender<SessionId> {
    let (req, rx) = mpsc::channel::<SessionId>();
    thread::spawn(move || {
        for id in rx {
            let _ = host::client::request(&dir, &Request::Seen { id });
        }
    });
    req
}

/// 以会话台身份订阅守护进程的状态事件和代弹提醒；守护进程没在跑就每隔一会儿再试。
/// 守护进程是不认订阅的旧版时照样报「后台是旧版」，改为每隔 `HOSTED_POLL` 用 `List` 重读托管对话；
/// 它的对话都结束后轮询停下，下一轮把它换成新版。
/// 只有上一个守护进程留下了在跑记录（可能是断电、重启后第一次打开）时才主动拉起它，让它认出被中断的对话。
fn host_watcher(ctx: Ctx, board: BoardInfo, away: Arc<Mutex<bool>>, tx: Sender<Msg>) {
    thread::spawn(move || {
        let dir = ctx.data_dir.clone();
        let left = host::interrupt::load(&host::interrupt::running_path(&dir));
        if left.is_ok_and(|l| !l.is_empty())
            && let Err(e) = host::client::ensure_daemon(&dir, ctx_launch(&ctx.exe))
        {
            let _ = tx.send(Msg::Status(format!(
                "守护进程没起来，认不出被中断的对话：{e}"
            )));
        }
        loop {
            let old = old_daemon(&ctx, &tx);
            if let Ok(mut feed) = host::client::board_feed(&dir, board.clone(), HOSTED_POLL) {
                if tx.send(Msg::OldHost(old || feed.polling())).is_err() {
                    return;
                }
                if let Ok(Response::List(infos)) = host::client::request(&dir, &Request::List)
                    && tx.send(Msg::Hosted(infos)).is_err()
                {
                    return;
                }
                if let Ok(list) = interrupted_list(&dir)
                    && tx.send(Msg::Interrupted(list)).is_err()
                {
                    return;
                }
                while let Ok(Some(item)) = feed.recv() {
                    let sent = match item {
                        BoardItem::Event(ev) => pass_host_event(ev, &away, &tx, &system_alert),
                        BoardItem::Hosted(infos) => tx.send(Msg::Hosted(infos)).map_err(|_| ()),
                    };
                    if sent.is_err() {
                        return;
                    }
                }
                if tx.send(Msg::HostGone).is_err() {
                    return;
                }
            }
            thread::sleep(HOST_RETRY);
        }
    });
}

/// 在跑的守护进程比当前程序旧吗。旧版且空着就顺手换成新版（换不成在底栏说一声）。没在跑算不旧。
fn old_daemon(ctx: &Ctx, tx: &Sender<Msg>) -> bool {
    match host::probe(&ctx.data_dir) {
        Ok(info) if info.replaceable() => {
            match host::ensure_daemon(&ctx.data_dir, ctx_launch(&ctx.exe)) {
                Ok(info) => info.outdated(),
                Err(e) => {
                    let _ = tx.send(Msg::Status(format!("后台换新版没成功：{e}")));
                    true
                }
            }
        }
        Ok(info) => info.outdated(),
        Err(_) => false,
    }
}

/// 托管事件交给主循环；会话台交出终端期间（`away`）主循环收不到，代弹提醒改交 `fallback`（系统通知）。
/// 判断和转发持着同一把锁，`Loop::suspend` 置位后再清队列，两边不会漏掉或压住提醒。主循环没了返回错误。
fn pass_host_event(
    ev: HostEvent,
    away: &Mutex<bool>,
    tx: &Sender<Msg>,
    fallback: &dyn Fn(Message),
) -> Result<(), ()> {
    let away = lock(away);
    match ev {
        HostEvent::Alert { title, body, .. } if *away => {
            fallback(Message { title, body });
            Ok(())
        }
        ev => tx.send(Msg::Host(ev)).map_err(|_| ()),
    }
}

/// 系统通知，放到别的线程发（osascript 最多等 5 秒，不卡住调用方）。
fn system_alert(msg: Message) {
    thread::spawn(move || {
        if let Err(e) = SystemNotifier.show(&msg) {
            eprintln!("huitai: 系统通知没发出去: {e}");
        }
    });
}

/// 开标签页（已开着就切过去）；既没开出也没切到任何标签页、却失败了，就调 `undo` 撤掉为它做的准备，
/// 并把撤销没成的原因接在错误后面。标签页开了、只是记录没存下的失败不撤：那个标签页里的 attach 还要用这条登记。
fn open_or_undo(
    tabs: io::Result<Tabs>,
    opener: &dyn Opener,
    id: &SessionId,
    cmd: &Command,
    title: &str,
    undo: impl FnOnce() -> io::Result<()>,
) -> io::Result<Outcome> {
    let watched = Watched {
        inner: opener,
        reached: Cell::new(false),
    };
    let result = tabs.and_then(|mut t| t.open_or_focus(&watched, id, cmd, title));
    let Err(e) = result else {
        return result;
    };
    if watched.reached.get() {
        return Err(e);
    }
    match undo() {
        Ok(()) => Err(e),
        Err(u) => Err(io::Error::new(
            e.kind(),
            format!("{e}（托管登记没撤掉：{u}）"),
        )),
    }
}

/// 记下有没有真的开出或切到一个标签页。
struct Watched<'a> {
    inner: &'a dyn Opener,
    reached: Cell<bool>,
}

impl Opener for Watched<'_> {
    fn open(&self, cmd: &Command, title: &str) -> io::Result<opener::Opened> {
        let r = self.inner.open(cmd, title);
        if r.is_ok() {
            self.reached.set(true);
        }
        r
    }

    fn focus(&self, handle: &opener::TabHandle) -> io::Result<bool> {
        let r = self.inner.focus(handle);
        if matches!(r, Ok(true)) {
            self.reached.set(true);
        }
        r
    }
}

/// 开标签页、结束、回收站这类可能慢的动作，排队在一个线程里做，结果用一句话报回。
fn job_worker(ctx: Ctx, index: Index, tx: Sender<Msg>) -> Sender<Job> {
    let (req, rx) = mpsc::channel::<Job>();
    thread::spawn(move || {
        let opener = opener::opener_for(&ctx.terminal, |k| std::env::var(k).ok());
        let mut worker = Worker {
            ctx,
            index,
            opener,
            tx,
        };
        for job in rx {
            let msg = worker.run(job);
            if worker.tx.send(msg).is_err() {
                return;
            }
        }
    });
    req
}

struct Worker {
    ctx: Ctx,
    index: Index,
    opener: Box<dyn Opener>,
    tx: Sender<Msg>,
}

impl Worker {
    fn tabs(&self) -> io::Result<Tabs> {
        Tabs::load(&self.ctx.data_dir.join("tabs.json"))
    }

    fn run(&mut self, job: Job) -> Msg {
        let result = match job {
            Job::Git(dir) => {
                let out = run_child(
                    Process::new("git")
                        .arg("-C")
                        .arg(&dir)
                        .args(["status", "--short"]),
                )
                .map_err(|e| e.to_string());
                return Msg::Git(dir, out);
            }
            Job::Open(e) => self.open(&e),
            Job::New(v, cwd) => self.new_session(v, &cwd),
            Job::RestoreTabs => self.restore_tabs(),
            Job::ResumeInterrupted(entries) => self.resume_interrupted(&entries),
            Job::Dismiss(id) => self.dismiss(&[id]).map(|()| "已移出「被中断」".to_owned()),
            Job::Restarted(id) => match self.dismiss_quietly(&[id]) {
                Ok(list) => return Msg::Interrupted(list),
                Err(e) => Err(e),
            },
            Job::Kill(id) => self.kill(&id),
            Job::Trash(e) => self.trash(&e),
            Job::Untrash(v) => self.untrash(v),
            Job::Finder(dir) => {
                run_child(Process::new("open").arg(&dir)).map(|_| "已在 Finder 打开".to_owned())
            }
            Job::Copy(e) => self.copy_resume(&e),
        };
        Msg::Status(result.unwrap_or_else(|e| format!("没做成：{e}")))
    }

    /// `c`：恢复命令（厂商 CLI 自己的，不经会话台托管）写进剪贴板。
    fn copy_resume(&self, e: &Entry) -> io::Result<String> {
        let s = &e.session;
        let cmd = source_for(&self.ctx.sources()?, s.vendor)?.resume_cmd(s);
        let home = &self.ctx.home;
        let env = sources::env_prefix(&VendorDirs::from_env(home), s.vendor, home);
        pbcopy(&resume_line(&cmd, &env))?;
        Ok("已复制恢复命令".to_owned())
    }

    fn open(&self, e: &Entry) -> io::Result<String> {
        let s = &e.session;
        let (id, cmd, fresh) = attach_existing(&self.ctx, &self.ctx.sources()?, s)?;
        let tab = opener::tab_title(s.vendor, &self.ctx.label(&s.cwd), Some(e.title()));
        // 被中断的单条 ⏎ 只打开不发话；CLI 真起来了守护进程才把它移出被中断（见 `Loop::host_event`）。
        let outcome = self.open_tab(&id, &cmd, &tab, fresh)?;
        let title = truncate(e.title(), 24);
        Ok(match outcome {
            Outcome::Focused => format!("已切到「{title}」"),
            Outcome::Opened => format!("已在新标签页打开「{title}」"),
            Outcome::Inline => "已回到会话台".to_owned(),
        })
    }

    fn new_session(&self, v: Vendor, cwd: &Path) -> io::Result<String> {
        let existing = ids_in(&self.index, v, cwd);
        let (id, cmd) = register_new(&self.ctx, &self.ctx.sources()?, v, cwd, existing)?;
        let tab = opener::tab_title(v, &self.ctx.label(cwd), None);
        self.open_tab(&id, &cmd, &tab, true)?;
        Ok(format!("已在 {} 新开 {v} 对话", self.ctx.label(cwd)))
    }

    /// 把上次开着的标签全部重开。守护进程可能重启过，先把每条重新登记托管。
    fn restore_tabs(&self) -> io::Result<String> {
        let records = self.tabs()?.records().clone();
        if records.is_empty() {
            return Ok("没有记着的标签页".to_owned());
        }
        let sources = self.ctx.sources()?;
        let hosted: HashSet<SessionId> = hosted_list(&self.ctx).into_iter().map(|h| h.id).collect();
        for v in Vendor::ALL {
            for e in self.index.list(v)? {
                if records.contains_key(&e.session.id) {
                    register_resume(&self.ctx, &sources, &e.session)?;
                }
            }
        }
        // 一条一条开，一条失败不影响别的；没开成的撤掉这次新登记的托管。
        let failed = records
            .iter()
            .filter(|(id, rec)| {
                let fresh = !hosted.contains(*id);
                self.open_tab(id, &rec.command(), &rec.title, fresh)
                    .is_err()
            })
            .count();
        Ok(match failed {
            0 => format!("已恢复 {} 个标签页", records.len()),
            n => format!("恢复了 {} 个，{n} 个没打开", records.len() - n),
        })
    }

    /// R：被中断的对话全部在新标签页重开，命令带那句「继续上次没做完的事……」。
    /// 开了标签页不算恢复：CLI 真起来了，守护进程才把它移出被中断（见 `Loop::host_event`）；
    /// attach 没接上、CLI 没起来的，留在被中断里还能再按 R。
    fn resume_interrupted(&self, entries: &[Entry]) -> io::Result<String> {
        let sources = self.ctx.sources()?;
        let hosted: HashSet<SessionId> = hosted_list(&self.ctx).into_iter().map(|h| h.id).collect();
        let mut opened = 0;
        let mut failed = Vec::new();
        for e in entries {
            let s = &e.session;
            let tab = opener::tab_title(s.vendor, &self.ctx.label(&s.cwd), Some(e.title()));
            let result = register_continue(&self.ctx, &sources, s)
                .and_then(|cmd| self.open_tab(&s.id, &cmd, &tab, !hosted.contains(&s.id)));
            match result {
                Ok(_) => opened += 1,
                Err(err) => failed.push(format!("「{}」{err}", truncate(e.title(), 16))),
            }
        }
        Ok(match failed.first() {
            None => format!("已在 {opened} 个新标签页接着做"),
            Some(first) => format!("接着做了 {opened} 个，{} 个没打开：{first}", failed.len()),
        })
    }

    /// 开标签页（已开着就切过去）。`fresh`：这次才登记的托管；标签页没开成时撤掉它，
    /// 不留一条永远没人来 attach 的登记（它会挡住旧守护进程换新版退出）。
    fn open_tab(
        &self,
        id: &SessionId,
        cmd: &Command,
        title: &str,
        fresh: bool,
    ) -> io::Result<Outcome> {
        open_or_undo(self.tabs(), self.opener.as_ref(), id, cmd, title, || {
            if fresh {
                self.cancel_unstarted(id)
            } else {
                Ok(())
            }
        })
    }

    /// 撤掉一条还没起过进程的托管登记；已经起了进程的（标签页其实开了、attach 接上了）不动。
    fn cancel_unstarted(&self, id: &SessionId) -> io::Result<()> {
        let unstarted = hosted_list(&self.ctx)
            .iter()
            .any(|h| h.id == *id && h.pid.is_none());
        if !unstarted {
            return Ok(());
        }
        lock(&self.ctx.known).remove(id);
        match host::client::request(&self.ctx.data_dir, &Request::Kill { id: id.clone() })? {
            Response::Ok => Ok(()),
            Response::Error(e) => Err(io::Error::other(e)),
            other => Err(io::Error::other(format!("守护进程回答不对：{other:?}"))),
        }
    }

    /// 把这些对话（列表里的会话 id）移出被中断，再把最新的被中断列表报给界面。
    fn dismiss(&self, ids: &[SessionId]) -> io::Result<()> {
        let list = self.dismiss_quietly(ids)?;
        let _ = self.tx.send(Msg::Interrupted(list));
        Ok(())
    }

    /// 把这些对话（列表里的会话 id）移出被中断，返回最新的被中断列表。
    fn dismiss_quietly(&self, ids: &[SessionId]) -> io::Result<Vec<RunRecord>> {
        let host_ids: Vec<SessionId> = {
            let map = lock(&self.ctx.interrupted);
            ids.iter()
                .map(|id| map.get(id).unwrap_or(id).clone())
                .collect()
        };
        match host::client::request(&self.ctx.data_dir, &Request::Dismiss { ids: host_ids })? {
            Response::Ok => {}
            Response::Error(e) => return Err(io::Error::other(e)),
            other => return Err(io::Error::other(format!("守护进程回答不对：{other:?}"))),
        }
        interrupted_list(&self.ctx.data_dir)
    }

    fn kill(&self, id: &SessionId) -> io::Result<String> {
        match host::client::request(&self.ctx.data_dir, &Request::Kill { id: id.clone() })? {
            Response::Ok => {
                self.tabs()?.forget(id)?;
                Ok("已结束后台进程".to_owned())
            }
            Response::Error(e) => Err(io::Error::other(e)),
            other => Err(io::Error::other(format!("守护进程回答不对：{other:?}"))),
        }
    }

    fn trash(&self, e: &Entry) -> io::Result<String> {
        let sources = self.ctx.sources()?;
        let entry = source_for(&sources, e.session.vendor)?.trash(&e.session)?;
        self.relist(e.session.vendor);
        let title = truncate(e.title(), 24);
        Ok(if entry.hide_only {
            format!("已隐藏「{title}」，u 恢复")
        } else {
            format!("「{title}」已进回收站，u 恢复")
        })
    }

    /// 恢复这家最近进回收站的一条。
    fn untrash(&self, v: Vendor) -> io::Result<String> {
        let trash = Trash::default_location()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "取不到回收站位置"))?;
        let Some(entry) = trash.list()?.into_iter().find(|t| t.vendor == v) else {
            return Ok(format!("回收站里没有 {v} 的对话"));
        };
        let sources = self.ctx.sources()?;
        source_for(&sources, v)?.restore(&entry)?;
        self.relist(v);
        let title = self
            .index
            .list(v)?
            .into_iter()
            .find(|e| e.session.id.as_str() == entry.id)
            .map(|e| truncate(e.title(), 24))
            .unwrap_or(entry.id);
        Ok(format!("已恢复「{title}」"))
    }

    fn relist(&self, v: Vendor) {
        self.index.refresh_vendor(v);
        let _ = self.tx.send(Msg::Index(Update::Listed(v)));
    }
}

/// 跑一个短命令拿标准输出，超时就杀掉。
fn run_child(cmd: &mut Process) -> io::Result<String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("没有标准输出"))?;
    let err = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("没有错误输出"))?;
    // 另起线程读，输出多时不会把管道塞满卡住子进程。
    let out = read_all(out);
    let err = read_all(err);
    let status = wait_child(&mut child)?;
    let stdout = out.join().unwrap_or_default();
    if status.success() {
        return Ok(stdout);
    }
    let msg = err.join().unwrap_or_default();
    Err(io::Error::other(
        msg.lines().next().unwrap_or("命令失败").trim().to_owned(),
    ))
}

/// 等子进程结束，超过 [`CHILD_TIMEOUT`] 就杀掉。
fn wait_child(child: &mut std::process::Child) -> io::Result<std::process::ExitStatus> {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok(st);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "超时"));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// 把文字放进系统剪贴板。
fn pbcopy(text: &str) -> io::Result<()> {
    let mut child = Process::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let written = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("没有标准输入"))
        .and_then(|mut stdin| stdin.write_all(text.as_bytes()));
    // 写完关掉标准输入（上面 take 出来的随闭包结束释放），pbcopy 读到结尾才退出。
    let status = wait_child(&mut child)?;
    written?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("pbcopy 失败"))
    }
}

fn read_all(mut r: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(program: &str) -> Command {
        Command {
            program: program.to_owned(),
            args: Vec::new(),
            cwd: PathBuf::from("/p/shop"),
        }
    }

    #[test]
    fn alerts_go_to_system_notification_while_board_is_away() {
        let away = Mutex::new(false);
        let (tx, rx) = mpsc::channel();
        let fallen = Mutex::new(Vec::new());
        let fallback = |m: Message| lock(&fallen).push(m.body);
        let alert = |body: &str| HostEvent::Alert {
            id: SessionId::from("b"),
            title: "✻ demo".to_owned(),
            body: body.to_owned(),
        };

        // 会话台在：提醒交给主循环写进自己标签页。
        pass_host_event(alert("一"), &away, &tx, &fallback).unwrap();
        assert!(matches!(
            rx.try_recv(),
            Ok(Msg::Host(HostEvent::Alert { .. }))
        ));
        assert!(lock(&fallen).is_empty());

        // 交出终端期间：提醒直接发系统通知，不压在队列里；别的事件照常排队。
        *lock(&away) = true;
        pass_host_event(alert("二"), &away, &tx, &fallback).unwrap();
        pass_host_event(
            HostEvent::Bell {
                id: SessionId::from("b"),
            },
            &away,
            &tx,
            &fallback,
        )
        .unwrap();
        assert_eq!(*lock(&fallen), vec!["二".to_owned()]);
        assert!(matches!(
            rx.try_recv(),
            Ok(Msg::Host(HostEvent::Bell { .. }))
        ));
        assert!(rx.try_recv().is_err());

        // 主循环没了：报错让 host_watcher 退出。
        drop(rx);
        *lock(&away) = false;
        assert!(pass_host_event(alert("三"), &away, &tx, &fallback).is_err());
    }

    #[test]
    fn host_events_map_to_activities() {
        let id = SessionId::from("a");
        let ev = |e: HostEvent| activity(e).map(|(i, a)| (i.to_string(), a));
        assert_eq!(
            ev(HostEvent::Seen { id: id.clone() }),
            Some(("a".to_owned(), Activity::Seen))
        );
        assert_eq!(
            ev(HostEvent::Notify {
                id: id.clone(),
                title: None,
                body: "x".to_owned()
            }),
            Some((
                "a".to_owned(),
                Activity::Notify {
                    title: None,
                    body: "x".to_owned()
                }
            ))
        );
        assert_eq!(
            ev(HostEvent::Exited {
                id: id.clone(),
                code: Some(0)
            }),
            Some(("a".to_owned(), Activity::Exited))
        );
        assert_eq!(
            ev(HostEvent::Focus {
                id: id.clone(),
                front: false
            }),
            Some(("a".to_owned(), Activity::Focus(false)))
        );
        // 代弹提醒和标题不影响状态。
        assert_eq!(
            ev(HostEvent::Alert {
                id: id.clone(),
                title: "t".to_owned(),
                body: "b".to_owned()
            }),
            None
        );
        assert_eq!(
            ev(HostEvent::Title {
                id,
                title: "t".to_owned()
            }),
            None
        );
    }

    #[test]
    fn claude_and_pi_new_sessions_are_hosted_under_their_real_id() {
        for (v, program) in [(Vendor::Claude, "claude"), (Vendor::Pi, "pi")] {
            let uuid = "0b7c2f4e-1a2b-4c3d-8e9f-0123456789ab".to_owned();
            let (id, cmd) = new_session_command(v, base(program), 1, uuid.clone());
            // 托管 id 就是 CLI 写出的会话 id：之后从列表打开同一条时守护进程里已有，不会再起一个进程。
            assert_eq!(id.as_str(), uuid);
            assert_eq!(cmd.args, vec!["--session-id".to_owned(), uuid]);
        }
    }

    #[test]
    fn codex_new_session_gets_temp_id_with_start_time() {
        let (id, cmd) = new_session_command(
            Vendor::Codex,
            base("codex"),
            1_795_176_000_123,
            String::new(),
        );
        assert_eq!(id.as_str(), "new-codex-1795176000123");
        assert!(cmd.args.is_empty());
        assert_eq!(
            temp_started(&id),
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_795_176_000_123))
        );
        assert_eq!(temp_started(&SessionId::from("abc")), None);
    }

    #[test]
    fn hermes_new_session_is_hosted_under_temp_id_and_maps_back() {
        let (id, cmd) =
            new_session_command(Vendor::Hermes, base("hermes"), 1_000_000_000, String::new());
        assert_eq!(id.as_str(), "new-hermes-1000000000");
        assert!(cmd.args.is_empty());
        assert_eq!(temp_started(&id), Some(at(1_000_000)));
        // 能先定 id 的那几家不会有临时 id；不认得的厂商名也不算。
        assert_eq!(
            temp_started(&SessionId::from("new-claude-1000000000")),
            None
        );
        assert_eq!(
            temp_started(&SessionId::from("new-gemini-1000000000")),
            None
        );

        let real = "20261122_213015_a1b2c3";
        let created = hermes::id_created(real).expect("是 Hermes 的 id");
        let ms = |t: SystemTime| {
            t.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        };
        let temp_at = |t: SystemTime| HostedInfo {
            vendor: Vendor::Hermes,
            ..hosted(&format!("new-hermes-{}", ms(t)), "/p/shop", true)
        };
        let s = Session {
            vendor: Vendor::Hermes,
            ..session(real, "/p/shop", 1_000_100)
        };
        let alias = |h: HostedInfo| {
            hosted_alias(
                &s,
                std::slice::from_ref(&h),
                &HashMap::new(),
                &entries(std::slice::from_ref(&s)),
            )
        };
        let temp = temp_at(created - Duration::from_secs(5));
        assert_eq!(alias(temp.clone()), Hosted::Alias(temp.id));
        // id 只到秒：同一秒里稍早一点新建的也认得出。
        let same_second = temp_at(created + Duration::from_millis(500));
        assert_eq!(alias(same_second.clone()), Hosted::Alias(same_second.id));
        // 新建之前就有的不认。
        assert_eq!(
            alias(temp_at(created + Duration::from_secs(2))),
            Hosted::Own
        );
    }

    /// OpenCode 1.x 的 `-s` 不认不存在的 id，新建也走临时 id；真实 id 里带着毫秒级的生成时间，靠它认回来。
    #[test]
    fn opencode_new_session_is_hosted_under_temp_id_and_maps_back() {
        let (id, cmd) = new_session_command(
            Vendor::OpenCode,
            base("opencode"),
            1_000_000_000,
            String::new(),
        );
        assert_eq!(id.as_str(), "new-opencode-1000000000");
        assert!(cmd.args.is_empty());
        assert_eq!(temp_started(&id), Some(at(1_000_000)));

        // 2026-11-22 13:30:15.123 UTC 生成的 id（上游 descending 算法）。
        let real = "ses_dfad5492cffeAbCdEfGhIjKlMn";
        let created = SystemTime::UNIX_EPOCH + Duration::from_millis(1_795_383_015_123);
        let s = Session {
            vendor: Vendor::OpenCode,
            last_active: created + Duration::from_secs(90),
            ..session(real, "/p/shop", 0)
        };
        assert_eq!(id_created(&s), Some(created));
        let ms = |t: SystemTime| {
            t.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        };
        let temp_at = |t: SystemTime| HostedInfo {
            vendor: Vendor::OpenCode,
            ..hosted(&format!("new-opencode-{}", ms(t)), "/p/shop", true)
        };
        let alias = |h: HostedInfo| {
            hosted_alias(
                &s,
                std::slice::from_ref(&h),
                &HashMap::new(),
                &entries(std::slice::from_ref(&s)),
            )
        };
        let temp = temp_at(created - Duration::from_secs(3));
        assert_eq!(alias(temp.clone()), Hosted::Alias(temp.id));
        // 新建之前就有的不认（精确到毫秒）。
        assert_eq!(
            alias(temp_at(created + Duration::from_millis(1))),
            Hosted::Own
        );
    }

    #[test]
    fn random_uuid_is_v4() {
        let u = random_uuid().unwrap();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!("89ab".contains(&u[19..20]));
        assert_ne!(u, random_uuid().unwrap());
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// Codex 的 UUIDv7 会话 id，生成时间是 `secs` 秒，`n` 区分同一秒里的几条。
    fn v7(secs: u64, n: u16) -> String {
        let ms = secs * 1000;
        format!("{:08x}-{:04x}-7000-8000-{n:012x}", ms >> 16, ms & 0xffff)
    }

    fn session(id: &str, cwd: &str, last: u64) -> Session {
        Session {
            vendor: Vendor::Codex,
            id: SessionId::from(id),
            cwd: PathBuf::from(cwd),
            path: PathBuf::new(),
            title: None,
            git_branch: None,
            last_active: at(last),
            message_count: 1,
            automatic: false,
        }
    }

    fn hosted(id: &str, cwd: &str, running: bool) -> HostedInfo {
        HostedInfo {
            id: SessionId::from(id),
            vendor: Vendor::Codex,
            project: "shop".to_owned(),
            cwd: PathBuf::from(cwd),
            pid: running.then_some(42),
            attached: true,
            title: None,
        }
    }

    /// 新建时刻（1_000_000 秒）的临时 id。
    const TEMP: &str = "new-codex-1000000000";

    #[test]
    fn codex_v7_id_carries_its_creation_time() {
        // 本机 Codex 0.171 的真实格式：id 开头 48 位就是状态库 created_at_ms 前一两秒。
        let id = SessionId::from("01a1fd7b-97e7-7db0-a209-f508e20bc82b");
        assert_eq!(
            codex_id_created(&id),
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_795_254_097_895))
        );
        assert_eq!(
            codex_id_created(&SessionId::from(v7(1_000_050, 1))),
            Some(at(1_000_050))
        );
        // v4、乱写的、非 ASCII 的：看不出生成时间。
        for bad in [
            "0b7c2f4e-1a2b-4c3d-8e9f-0123456789ab",
            "real",
            "+1a1fd7b-97e7-7db0-a209-f508e20bc82b",
            "０1a1fd7b-97e7-7db0-a209-f508e20bc8",
        ] {
            assert_eq!(codex_id_created(&SessionId::from(bad)), None, "{bad}");
        }
    }

    #[test]
    fn new_codex_conversation_maps_back_to_its_temp_host() {
        let h = [hosted(TEMP, "/p/shop", true)];
        let none = HashMap::new();
        let real = session(&v7(1_000_050, 1), "/p/shop", 1_000_100);
        let alias = |s: &Session, h: &[HostedInfo], known: &HashMap<SessionId, NewSession>| {
            hosted_alias(s, h, known, &entries(std::slice::from_ref(s)))
        };
        // 新建之后才生成的同项目对话：接到临时 id 那个进程上。
        assert_eq!(
            alias(&real, &h, &none),
            Hosted::Alias(SessionId::from(TEMP))
        );
        // 别的项目的、临时对话已经退出的、id 看不出生成时间的：都是它自己。
        let blog = session(&v7(1_000_050, 1), "/p/blog", 1_000_100);
        assert_eq!(alias(&blog, &h, &none), Hosted::Own);
        let stopped = [hosted(TEMP, "/p/shop", false)];
        assert_eq!(alias(&real, &stopped, &none), Hosted::Own);
        let v4 = session("0b7c2f4e-1a2b-4c3d-8e9f-0123456789ab", "/p/shop", 1_000_100);
        assert_eq!(alias(&v4, &h, &none), Hosted::Own);
        // 新建那一刻已有的对话不认。
        let busy = v7(1_000_060, 2);
        let known = HashMap::from([(
            SessionId::from(TEMP),
            NewSession {
                vendor: Vendor::Codex,
                cwd: PathBuf::from("/p/shop"),
                existing: HashSet::from([SessionId::from(busy.as_str())]),
            },
        )]);
        assert_eq!(
            alias(&session(&busy, "/p/shop", 1_000_100), &h, &known),
            Hosted::Own
        );
        // 自己就在托管里：用自己的 id。
        let own = [
            hosted(real.id.as_str(), "/p/shop", true),
            hosted(TEMP, "/p/shop", true),
        ];
        assert_eq!(alias(&real, &own, &none), Hosted::Own);
    }

    /// 审计第 1 条：会话台重开后不知道新建那一刻已有哪些对话，同项目的旧对话在新建之后又在别处聊过，
    /// 不能被认成临时 id 那条（以前按「之后有动静」认，会接到新建的那个进程上）。
    #[test]
    fn old_conversation_active_after_new_is_not_taken_for_it_after_restart() {
        let h = [hosted(TEMP, "/p/shop", true)];
        let none = HashMap::new();
        let old = session(&v7(999_000, 1), "/p/shop", 1_000_200);
        let real = session(&v7(1_000_050, 2), "/p/shop", 1_000_100);
        // 新建那条还没进列表时：旧对话是它自己。
        let only_old = entries(std::slice::from_ref(&old));
        assert_eq!(hosted_alias(&old, &h, &none, &only_old), Hosted::Own);
        // 都在列表里：旧对话还是它自己，新建那条认回临时 id。
        let both = entries(&[old.clone(), real.clone()]);
        assert_eq!(hosted_alias(&old, &h, &none, &both), Hosted::Own);
        assert_eq!(
            hosted_alias(&real, &h, &none, &both),
            Hosted::Alias(SessionId::from(TEMP))
        );
        assert_eq!(
            aliased(
                Vendor::Codex,
                &both,
                &HashMap::from([(
                    SessionId::from(TEMP),
                    NewSession {
                        vendor: Vendor::Codex,
                        cwd: PathBuf::from("/p/shop"),
                        existing: HashSet::new(),
                    }
                )]),
                |_| true
            ),
            HashSet::from([real.id.clone()])
        );
    }

    #[test]
    fn two_conversations_that_could_be_the_new_one_are_ambiguous() {
        // 新建之后同一项目里又有一条（比如别处也新开了 Codex）：两条都确认不了，谁也不接。
        let h = [hosted(TEMP, "/p/shop", true)];
        let a = session(&v7(1_000_050, 1), "/p/shop", 1_000_100);
        let b = session(&v7(1_000_070, 2), "/p/shop", 1_000_100);
        let list = entries(&[a.clone(), b.clone()]);
        let none = HashMap::new();
        assert_eq!(hosted_alias(&a, &h, &none, &list), Hosted::Ambiguous);
        assert_eq!(hosted_alias(&b, &h, &none, &list), Hosted::Ambiguous);
        // 另一条托管在它自己的 id 下（肯定不是临时 id 那条）：不算对手。
        let h2 = [
            hosted(TEMP, "/p/shop", true),
            hosted(b.id.as_str(), "/p/shop", true),
        ];
        assert_eq!(
            hosted_alias(&a, &h2, &none, &list),
            Hosted::Alias(SessionId::from(TEMP))
        );
    }

    #[test]
    fn two_new_codex_conversations_in_one_project_are_ambiguous() {
        let h = [
            hosted(TEMP, "/p/shop", true),
            hosted("new-codex-1000020000", "/p/shop", true),
        ];
        let real = session(&v7(1_000_050, 1), "/p/shop", 1_000_100);
        let list = entries(std::slice::from_ref(&real));
        assert_eq!(
            hosted_alias(&real, &h, &HashMap::new(), &list),
            Hosted::Ambiguous
        );
    }

    #[test]
    fn running_temp_codex_host_claims_its_real_entry() {
        let temp = SessionId::from(TEMP);
        let busy = v7(1_000_060, 2);
        let known = HashMap::from([(
            temp.clone(),
            NewSession {
                vendor: Vendor::Codex,
                cwd: PathBuf::from("/p/shop"),
                existing: HashSet::from([SessionId::from(busy.as_str())]),
            },
        )]);
        let real = v7(1_000_050, 1);
        let list = entries(&[
            session(&real, "/p/shop", 1_000_100),
            session(&busy, "/p/shop", 1_000_100),
            session(&v7(1_000_050, 3), "/p/blog", 1_000_100),
            session(&v7(999_000, 4), "/p/shop", 1_000_100),
        ]);
        // 临时 id 那个进程在跑：新建之后才生成、同项目、新建时还没有的那条是它。
        assert_eq!(
            aliased(Vendor::Codex, &list, &known, |id| *id == temp),
            HashSet::from([SessionId::from(real.as_str())])
        );
        // 进程退出了：谁也不是。
        assert!(aliased(Vendor::Codex, &list, &known, |_| false).is_empty());
    }

    fn record(id: &str, vendor: Vendor, cwd: &str) -> RunRecord {
        RunRecord {
            id: SessionId::from(id),
            vendor,
            project: "shop".to_owned(),
            cwd: PathBuf::from(cwd),
            started_at: 1_000_000,
        }
    }

    fn entries(list: &[Session]) -> Vec<Entry> {
        list.iter()
            .map(|s| Entry {
                session: s.clone(),
                note: crate::index::Note::default(),
            })
            .collect()
    }

    #[test]
    fn interrupted_records_map_to_list_entries() {
        let real = v7(1_000_050, 1);
        let codex = entries(&[
            session(&real, "/p/shop", 1_000_100),
            session(&v7(999_000, 2), "/p/shop", 999_000),
            session(&v7(1_000_050, 3), "/p/blog", 1_000_100),
            session("plain", "/p/blog", 5),
        ]);
        let records = [
            // 本来就是真实 id 的：原样对上。
            record("plain", Vendor::Codex, "/p/blog"),
            // 会话台新建的 Codex 对话（临时 id）：认回同项目、之后才生成的那条。
            record(TEMP, Vendor::Codex, "/p/shop"),
            // 列表里没有这条（从没写出会话文件）：不列。
            record("ghost", Vendor::Codex, "/p/shop"),
        ];
        let map = resolve_interrupted(&records, |v| match v {
            Vendor::Codex => codex.as_slice(),
            _ => &[],
        });
        assert_eq!(
            map,
            HashMap::from([
                (SessionId::from("plain"), SessionId::from("plain")),
                (SessionId::from(real.as_str()), SessionId::from(TEMP)),
            ])
        );
    }

    /// 审计第 1 条（断电恢复那半）：新建那条从没写出会话记录时，同项目里新建之后又聊过的旧对话
    /// 不能被当成它列进被中断，否则 R 会把「继续」发给那条旧对话。
    #[test]
    fn old_conversation_is_not_taken_for_an_interrupted_new_one() {
        let codex = entries(&[session(&v7(999_000, 1), "/p/shop", 1_000_200)]);
        let records = [record(TEMP, Vendor::Codex, "/p/shop")];
        assert!(resolve_interrupted(&records, |_| codex.as_slice()).is_empty());
    }

    #[test]
    fn ambiguous_temp_interrupted_codex_is_not_listed() {
        let codex = entries(&[
            session(&v7(1_000_050, 1), "/p/shop", 1_000_100),
            session(&v7(1_000_060, 2), "/p/shop", 1_000_200),
        ]);
        let records = [record(TEMP, Vendor::Codex, "/p/shop")];
        assert!(resolve_interrupted(&records, |_| codex.as_slice()).is_empty());
    }

    #[test]
    fn interrupted_temp_codex_uses_its_process_start_time() {
        // 同一个临时 id 先后跑过两次（CLI 退出后又接上，起了第二条新对话）：记录里的启动时间之后生成的才是这次的。
        let earlier = v7(1_000_050, 1);
        let latest = v7(1_000_500, 2);
        let codex = entries(&[
            session(&earlier, "/p/shop", 1_000_100),
            session(&latest, "/p/shop", 1_000_600),
        ]);
        let mut r = record(TEMP, Vendor::Codex, "/p/shop");
        r.started_at = 1_000_400;
        assert_eq!(
            resolve_interrupted(&[r], |_| codex.as_slice()),
            HashMap::from([(SessionId::from(latest.as_str()), SessionId::from(TEMP))])
        );
    }

    #[test]
    fn one_entry_claimed_by_two_interrupted_records_is_not_listed() {
        let codex = entries(&[session(&v7(1_000_050, 1), "/p/shop", 1_000_100)]);
        let records = [
            record(TEMP, Vendor::Codex, "/p/shop"),
            record("new-codex-1000010000", Vendor::Codex, "/p/shop"),
        ];
        assert!(resolve_interrupted(&records, |_| codex.as_slice()).is_empty());
    }

    /// 假终端：`open` 按 `fail` 成败，`focus` 一律说标签页不在了。
    struct FakeOpener {
        fail: bool,
    }

    impl Opener for FakeOpener {
        fn open(&self, _cmd: &Command, _title: &str) -> io::Result<opener::Opened> {
            if self.fail {
                return Err(io::Error::other("用户拒绝了自动化授权"));
            }
            Ok(opener::Opened::Tab(opener::TabHandle::Ghostty {
                terminal_id: "t1".to_owned(),
            }))
        }

        fn focus(&self, _handle: &opener::TabHandle) -> io::Result<bool> {
            Ok(false)
        }
    }

    fn tabs_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("huitai-rt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 审计第 6 条（会话台那半）：开标签页失败要撤掉这次的托管登记；开成了、或开了只是记录没存下，都不撤。
    #[test]
    fn failed_tab_open_undoes_the_registration() {
        use std::os::unix::fs::PermissionsExt;
        let id = SessionId::from("s1");
        let cmd = base("ai");
        let dir = tabs_dir("undo");
        let path = dir.join("tabs.json");
        let undone = Cell::new(0);
        let undo = || {
            undone.set(undone.get() + 1);
            Ok(())
        };

        // 开不了：撤，错误原样报。
        let err = open_or_undo(
            Tabs::load(&path),
            &FakeOpener { fail: true },
            &id,
            &cmd,
            "",
            undo,
        )
        .unwrap_err();
        assert!(err.to_string().contains("自动化授权"), "{err}");
        assert_eq!(undone.get(), 1);

        // 标签页记录读不了（还没开）：也撤。
        let bad = Err(io::Error::other("tabs.json 坏了"));
        assert!(open_or_undo(bad, &FakeOpener { fail: false }, &id, &cmd, "", undo).is_err());
        assert_eq!(undone.get(), 2);

        // 开成了：不撤。
        let ok = open_or_undo(
            Tabs::load(&path),
            &FakeOpener { fail: false },
            &id,
            &cmd,
            "",
            undo,
        );
        assert_eq!(ok.unwrap(), Outcome::Opened);
        assert_eq!(undone.get(), 2);

        // 标签页开了、记录没存下（目录不可写）：报错但不撤，标签页里的 attach 还要用这条登记。
        let tabs = Tabs::load(&path).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let r = open_or_undo(Ok(tabs), &FakeOpener { fail: false }, &id, &cmd, "", undo);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(r.is_err());
        assert_eq!(undone.get(), 2);

        // 撤也没撤成：两个原因都报。
        let err = open_or_undo(
            Tabs::load(&path),
            &FakeOpener { fail: true },
            &id,
            &cmd,
            "",
            || Err(io::Error::other("守护进程不在")),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("自动化授权") && err.contains("守护进程不在"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 进程内起一个守护进程（数据目录在 /tmp 下，socket 路径不能太长），配一个开标签页用假终端的动作线程。
    fn test_worker(name: &str, fail_open: bool) -> (Worker, PathBuf) {
        let dir = PathBuf::from(format!("/tmp/htr-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (worker_in(&dir, fail_open), dir)
    }

    fn worker_in(dir: &Path, fail_open: bool) -> Worker {
        let daemon = host::Daemon::bind(dir).unwrap();
        thread::spawn(move || daemon.run());
        let index = Index::open(&dir.join("index"), Vec::new()).unwrap();
        let ctx = Ctx::new(dir.join("home"), dir.to_path_buf(), index.clone()).unwrap();
        let (tx, _rx) = mpsc::channel();
        Worker {
            ctx,
            index,
            opener: Box::new(FakeOpener { fail: fail_open }),
            tx,
        }
    }

    fn pi_entry(id: &str, cwd: &Path) -> Entry {
        let mut s = session(id, "/", 1);
        s.vendor = Vendor::Pi;
        s.cwd = cwd.to_path_buf();
        Entry {
            session: s,
            note: crate::index::Note::default(),
        }
    }

    fn hosted_ids(w: &Worker) -> Vec<SessionId> {
        hosted_list(&w.ctx).into_iter().map(|h| h.id).collect()
    }

    /// 审计第 6 条（会话台那半）：n、⏎ 开标签页失败时撤掉本次登记；之前就托管着的不动。
    #[test]
    fn failed_tab_leaves_no_registration_behind() {
        let (w, dir) = test_worker("tab", true);
        assert!(w.new_session(Vendor::Pi, &dir).is_err());
        assert!(w.open(&pi_entry("p1", &dir)).is_err());
        assert_eq!(hosted_ids(&w), Vec::<SessionId>::new());

        // 已经托管着的（比如另一个标签页开着它）：这次没开成也不撤。
        let spec = SpawnSpec {
            id: SessionId::from("p2"),
            vendor: Vendor::Pi,
            project: "demo".to_owned(),
            cwd: dir.clone(),
            command: "true".to_owned(),
        };
        assert_eq!(
            host::client::request(&dir, &Request::Spawn(spec)).unwrap(),
            Response::Ok
        );
        assert!(w.open(&pi_entry("p2", &dir)).is_err());
        assert_eq!(hosted_ids(&w), vec![SessionId::from("p2")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 审计第 8 条（会话台那半）：R 开出标签页不等于恢复了；CLI 没真起来前，被中断记录要留着还能再按 R。
    #[test]
    fn resume_keeps_interrupted_record_until_the_cli_starts() {
        let dir = PathBuf::from(format!("/tmp/htr-{}-resume", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rec = RunRecord {
            id: SessionId::from("p1"),
            vendor: Vendor::Pi,
            project: "demo".to_owned(),
            cwd: dir.clone(),
            started_at: 1,
        };
        let records = host::interrupt::Records::from([(rec.id.clone(), rec.clone())]);
        host::interrupt::save(&host::interrupt::running_path(&dir), &records).unwrap();
        let w = worker_in(&dir, false);
        assert_eq!(interrupted_list(&dir).unwrap(), vec![rec.clone()]);
        lock(&w.ctx.interrupted).insert(rec.id.clone(), rec.id.clone());

        let msg = w.resume_interrupted(&[pi_entry("p1", &dir)]).unwrap();
        assert_eq!(msg, "已在 1 个新标签页接着做");
        // 标签页「开了」，但没有 attach 来、CLI 没起来：还在被中断里。
        assert_eq!(interrupted_list(&dir).unwrap(), vec![rec]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
