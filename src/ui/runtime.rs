//! 真终端里跑会话台：进出备用屏、事件循环、后台线程（预览、额度、托管事件、开标签页等动作），
//! 把 [`Effect`] 落到 index / opener / host / quota / sources 的接口上。

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CStr, OsStr};
use std::io::{self, Read, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};

use crate::clis;
use crate::core::{Account, Config, QuotaReading, Session, SessionId, SessionState, Vendor, exec};
use crate::export;
use crate::host::client::BoardItem;
use crate::host::{
    self, BoardInfo, HostEvent, HostedInfo, Request, Response, RunRecord, ScreenSnapshot, Snapshot,
    SpawnSpec,
};
use crate::ime::{Switcher, Tis};
use crate::index::{self, Entry, Index, Snippet, Update};
use crate::notify::{self, Activity, Desktop, Message, Notifier, OscKind, SystemNotifier};
use crate::opener::{
    self, InlineOpener, KakuOpener, LoginItem, Opened, Opener, Outcome, Tabs, Terminal as Term,
};
use crate::quota;
use crate::sources::{self, Command, Delivery, Moment, Source, Transcript, Trash};

use super::app::{App, CliReport, CliResult, CliUpgrade, Effect, Preview};
use super::claims::{
    Bound, Claims, ClaimsStamp, Hosted, NewSession, TEMP_PREFIX, hosted_alias, is_new_since,
    load_claims, native_hosted, native_owners, save_claims, temp_started, time_pairs,
};
use super::text::{project_label, resume_line, tilde, truncate};
use super::theme::Theme;
use super::view;

mod jobs;
mod reclaim;
mod workers;

use jobs::{Job, job_worker};
use reclaim::reclaim_worker;
use workers::{
    CliRequest, backup_worker, balance_worker, cli_versions_worker, context_worker, forward,
    host_watcher, live_worker, moment_worker, preview_worker, procs_worker, quota_worker,
    seen_worker, snippet_worker, system_alert, transcript_worker,
};

/// 当前窗口接上对话前，底栏「⌃\ 回到会话台」亮多久再把屏幕交给 CLI。
const BACK_KEY_HINT: Duration = Duration::from_secs(1);
/// 预览读几条消息。
const PREVIEW_MESSAGES: usize = 8;
/// 搜索最多列几条。
const SEARCH_LIMIT: usize = 300;
/// 当前那家额度多久重读一次。
const QUOTA_EVERY: Duration = Duration::from_secs(60);
/// 余额多久读一次（和每把密钥的缓存时长一样，见 `quota/rules.json` 的 `balance.cache_minutes`）。
const BALANCE_EVERY: Duration = Duration::from_secs(5 * 60);
/// 多久读一次各条对话的上下文占用（只重读变了的文件）。
const CONTEXT_EVERY: Duration = Duration::from_secs(5);
/// 多久看一次本机进程（认出桌面端、别的终端开着的对话）。
const PROCS_EVERY: Duration = Duration::from_secs(5);
/// 多久备一轮 Claude 对话（原文件变了就更新备份；改了标注、设置会马上备一轮）。
const BACKUP_EVERY: Duration = Duration::from_secs(10 * 60);
/// 守护进程没在跑时多久再试着连一次。
const HOST_RETRY: Duration = Duration::from_secs(2);
/// 守护进程是不认订阅的旧版时，多久用 `List` 重读一次托管对话。
const HOSTED_POLL: Duration = Duration::from_secs(5);
/// 没有按键时事件循环多久醒一次，收后台线程的消息。
const TICK: Duration = Duration::from_millis(120);
/// 相对时间多久刷新一次。
const CLOCK: Duration = Duration::from_secs(30);
/// 实时预览多久取一次画面。
const LIVE_EVERY: Duration = Duration::from_millis(500);
/// 一条对话的「此刻」最多多久读一次（会话文件变了才读）。
const MOMENT_EVERY: Duration = Duration::from_secs(15);
/// 按 R 重开被中断的对话时带进去的第一句话（方案「中断恢复」节）。
pub const CONTINUE_MESSAGE: &str =
    "继续上次没做完的事。先检查现场（比如有没有执行到一半的命令），再接着做。";
/// 撞额度的对话到了重置时间自动送进去的那句话。
pub const AUTO_CONTINUE_MESSAGE: &str = "继续";
/// 自动继续前等守护进程拿掉卡在上限上的旧进程最多多久（它 SIGHUP 后 3 秒不退就 SIGKILL）。
const GONE_WAIT: Duration = Duration::from_secs(6);
/// git / open 这类子进程的超时。
const CHILD_TIMEOUT: Duration = Duration::from_secs(5);

/// 后台线程发给界面的消息。
enum Msg {
    Index(Update),
    Preview(SessionId, Preview),
    /// 一条对话的「此刻」读到了。
    Moment(SessionId, Moment),
    /// 整段对话读到了：哪条、读的时候它的最后时间、全文。
    Transcript(SessionId, SystemTime, Result<Transcript, String>),
    /// 一条搜索结果的命中片段（查询词、哪条、片段；没找到或读不了是 `None`）。
    Snippet(String, SessionId, Option<Snippet>),
    /// 实时预览取到的新画面；`None` 表示这条取不到了（已退出、守护进程不在）。
    Live(SessionId, Option<ScreenSnapshot>),
    /// 一个账号（厂商、账号短名）的额度。
    Quota(Vendor, String, QuotaReading),
    /// 余额和订阅额度（全部密钥）。
    Balance(quota::BalanceReport),
    /// 各条对话的上下文占用（全部重报，见 [`context_worker`]）。
    Context(HashMap<SessionId, quota::ContextUse>),
    Hosted(Vec<HostedInfo>),
    /// 收到没见过的 id 起了进程（别的会话台新建的）后补取的托管列表：只补认得的，不重新同步状态。
    Learned(Vec<HostedInfo>),
    /// 守护进程记着的被中断对话（全部重报）。
    Interrupted(Vec<RunRecord>),
    /// 守护进程的事件和收到它的时刻（会话台交出终端时主循环晚些才处理，认领要用真实时刻）。
    Host(HostEvent, SystemTime),
    /// 连上的守护进程比当前程序旧（之后的 `Hosted` 记成它托管着的对话）。
    OldHost(bool),
    HostGone,
    Status(String),
    Git(PathBuf, Result<String, String>),
    /// 查到有新版（版本号）。
    Update(String),
    /// 各家 CLI 有新版的、能一键升的，每次是完整的一份。
    CliUpdates(CliReport),
    /// 一键升级升完了几家、一共几家。
    CliProgress(usize, usize),
    /// 一键升级全部跑完。
    CliDone(Vec<CliResult>),
    /// 自动升级跑完的几家。
    CliAuto(Vec<CliResult>),
    /// 本机进程参数里带着的会话 id（见 [`procs_worker`]）。
    Procs(HashSet<SessionId>),
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
    let accounts = sources::local_accounts(&home);
    let mut app = App::new(home.clone(), launch_dir, config);
    app.set_accounts(accounts.clone());
    app.retention = sources::claude::retention_by_account(&accounts);
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
    // 没装的那家（哪个账号的数据目录都不在）不显示标签。
    app.hide_missing(|v| installed(&accounts, v));
    app.status = config_problem
        .or(list_problem)
        .or_else(|| index.problems().first().cloned());

    let theme = Theme::detect();
    let ctx = Ctx::new(home, data_dir, index.clone())?.with_accounts(accounts);
    match load_claims(&ctx.data_dir, SystemTime::now()) {
        Ok(bound) => *lock(&ctx.claims) = Claims::restore(bound),
        Err(e) => app.status = Some(format!("认领记录没读到（新建的对话先不认领）：{e}")),
    }
    let alert_kind = std::env::var("TERM_PROGRAM")
        .ok()
        .and_then(|t| notify::osc_kind(&t));
    let board = board_info(alert_kind.is_some());
    let (tx, rx) = mpsc::channel();
    let (_background, updates) = index.start();
    forward(updates, tx.clone(), Msg::Index);
    let previews = preview_worker(ctx.clone(), tx.clone());
    let moments = moment_worker(ctx.clone(), tx.clone());
    let reclaims = reclaim_worker(ctx.clone());
    let transcripts = transcript_worker(ctx.clone(), tx.clone());
    let snippets = snippet_worker(index.clone(), tx.clone());
    let live = live_worker(ctx.data_dir.clone(), tx.clone());
    let quotas = quota_worker(tx.clone());
    let balances = balance_worker(ctx.accounts.clone(), tx.clone());
    procs_worker(tx.clone());
    context_worker(ctx.accounts.clone(), tx.clone());
    let seen = seen_worker(ctx.data_dir.clone());
    let backups = backup_worker(
        index.clone(),
        ctx.accounts.clone(),
        ctx.data_dir.clone(),
        tx.clone(),
    );
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
    let cli = app
        .config
        .check_updates
        .then(|| cli_versions_worker(ctx.home.clone(), ctx.data_dir.clone(), tx.clone()));
    let jobs = job_worker(ctx.clone(), index.clone(), tx);
    let _ = quotas.send(app.quota_account());

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
        previews,
        moments,
        moment_sent: Vec::new(),
        reclaims,
        reclaim_sent: (None, Vec::new()),
        frame: None,
        transcripts,
        snippets,
        live,
        live_sent: None,
        quotas,
        balances,
        seen,
        backups,
        jobs,
        cli,
        rx,
        stopped: Vec::new(),
        hosted: Vec::new(),
        claims_stamp: ClaimsStamp::default(),
    };
    let result = lp.run(&mut tui);
    // 正常退出和出错退出都切回原来的输入源。
    let _ = lp.ime.leave();
    drop(tui);
    result
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
    /// 临时 id 认领到列表里的哪条（主循环攒证据、动作线程打开时查）。
    claims: Arc<Mutex<Claims>>,
    /// 列表里的被中断对话 → 守护进程记它用的 id（会话台新建的 Codex 对话是临时 id，其余相同）。
    interrupted: Arc<Mutex<HashMap<SessionId, SessionId>>>,
    /// 取项目显示名（标签页标题前缀）。
    index: Index,
    /// 本机各家的账号（每家主账号在前），见 `sources::local_accounts`。
    accounts: Arc<Vec<Account>>,
}

impl Ctx {
    fn new(home: PathBuf, data_dir: PathBuf, index: Index) -> io::Result<Self> {
        Ok(Self {
            accounts: Arc::new(sources::local_accounts(&home)),
            home,
            data_dir,
            index,
            exe: std::env::current_exe()?,
            terminal: Term::current(),
            known: Arc::default(),
            claims: Arc::default(),
            interrupted: Arc::default(),
        })
    }

    fn with_accounts(mut self, accounts: Vec<Account>) -> Self {
        self.accounts = Arc::new(accounts);
        self
    }

    fn sources(&self) -> io::Result<Vec<Box<dyn Source>>> {
        let trash = Trash::default_location()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "取不到回收站位置"))?;
        Ok(sources::sources_for(&self.accounts, trash))
    }

    /// 这家这个账号（[`Account::key`]）。
    fn account(&self, vendor: Vendor, key: &str) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|a| a.vendor == vendor && a.key == key)
    }

    /// 托管 id 是 `id` 的对话的命令前面要带的目录变量（账号从 id 的前缀认），见 [`env_prefix_of`]。
    fn env_prefix(&self, vendor: Vendor, id: &SessionId) -> String {
        env_prefix_of(&self.accounts, vendor, id.split_account().0, &self.home)
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
            && let Some(account) = self.account(vendor, id.split_account().0)
            && let Some(settings) = quota::claude_settings(account, cwd)
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
            command: format!("{}{}", self.env_prefix(vendor, id), cmd.shell_line()),
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

/// 这家的主账号的来源（新建对话用）：来源按账号排，每家主账号在前。
fn source_for(sources: &[Box<dyn Source>], v: Vendor) -> io::Result<&dyn Source> {
    sources
        .iter()
        .find(|s| s.vendor() == v)
        .map(|s| s.as_ref())
        .ok_or_else(|| io::Error::other(format!("没有 {v} 的会话来源")))
}

/// 这条对话所在账号的来源。
fn source_of<'a>(sources: &'a [Box<dyn Source>], s: &Session) -> io::Result<&'a dyn Source> {
    sources::source_of(sources, s).ok_or_else(|| {
        io::Error::other(format!(
            "没有 {} 这个账号的会话来源（账号删掉了？）",
            s.vendor
        ))
    })
}

/// 本机这家有没有装（哪个账号的数据目录在）。
fn installed(accounts: &[Account], v: Vendor) -> bool {
    accounts.iter().any(|a| a.vendor == v && a.dir.exists())
}

/// 账号 `key` 的命令前面要带的目录变量：非默认账号带 `VAR=目录`；默认账号在这家只有一个账号时不带
/// （老样子，命令短），有几个账号时把这家所有改目录的变量都 `env -u` 清掉（OpenCode 连 `XDG_DATA_HOME`），
/// 免得守护进程或登录 shell 里设着的哪一个把它带到别的账号。
/// 找不到账号（设置里删了）就按默认账号算。
fn env_prefix_of(accounts: &[Account], vendor: Vendor, key: &str, home: &Path) -> String {
    match accounts.iter().find(|a| a.vendor == vendor && a.key == key) {
        Some(a) if !a.is_default() => sources::account_env_prefix(a, home),
        _ if accounts.iter().filter(|a| a.vendor == vendor).count() > 1 => {
            sources::default_account_env_prefix(vendor)
        }
        _ => String::new(),
    }
}

/// 恢复一条已有对话：登记托管，返回 attach 命令。
fn register_resume(ctx: &Ctx, sources: &[Box<dyn Source>], s: &Session) -> io::Result<Command> {
    let cmd = source_of(sources, s)?.resume_cmd(s);
    ctx.register(&s.id, s.vendor, &s.cwd, cmd)
}

/// 守护进程记的被中断对话对到列表里的哪条：返回「列表里的会话 id → 守护进程用的 id」。
/// 多数就是同一个 id；会话台新建、记的是临时 id 的，只认回它认领过的那条（[`Claims`]，光按时间对得上不算），
/// 且那条还得是同一项目、id 在那次进程启动之后才生成、不是别的被中断对话（见 [`is_new_since`]）。
/// 没认领过、或同一条被几条记录认领，都不列：宁可不列，也不能把「继续」发给别的对话。
/// v0.1.3 记的原生 id 按 [`native_owners`] 对：工作账号下那条带前缀的 id 也认得回来，对不上唯一一条不列。
fn resolve_interrupted<'a>(
    records: &[RunRecord],
    entries_of: impl Fn(Vendor) -> &'a [Entry],
    bound: &HashMap<SessionId, Bound>,
) -> HashMap<SessionId, SessionId> {
    let recorded: HashSet<&SessionId> = records.iter().map(|r| &r.id).collect();
    let mut out = HashMap::new();
    // 列表里的会话 id → 认它的临时 id 记录。
    let mut claims: HashMap<SessionId, Vec<SessionId>> = HashMap::new();
    for r in records {
        let entries = entries_of(r.vendor);
        if native_hosted(&r.id) {
            // 自己（带账号前缀）的 id 另有记录的另有其主，不算。
            let others = entries
                .iter()
                .map(|e| &e.session)
                .filter(|s| s.id == r.id || !recorded.contains(&s.id));
            if let [one] = native_owners(&r.id, r.vendor, &r.cwd, others).as_slice() {
                out.insert(one.id.clone(), r.id.clone());
            }
            continue;
        }
        if entries.iter().any(|e| e.session.id == r.id) {
            out.insert(r.id.clone(), r.id.clone());
            continue;
        }
        let Some(started) = temp_started(&r.id) else {
            continue;
        };
        // 记录里的进程启动时间（秒，向下取整）比新建时刻更紧：这次跑的那条 id 一定在它之后生成。
        let since = started.max(SystemTime::UNIX_EPOCH + Duration::from_secs(r.started_at));
        let Some(b) = bound.get(&r.id) else {
            continue;
        };
        // 不带账号前缀的临时 id 可能是旧版守护进程托管的工作账号对话（见 `NewSession::any_account`）：
        // 认领本身有证据，认到哪个账号都算。
        let any_account = r.id.split_account().0.is_empty();
        if let Some(e) = entries.iter().find(|e| e.session.id == b.real)
            && !recorded.contains(&e.session.id)
            && is_new_since(&e.session, &r.id, &r.cwd, since, None, any_account)
        {
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

/// 带一句话重开对话（R 接着做被中断的、撞额度到点自动继续）：登记托管（命令带 `message`），
/// 返回 attach 命令和那句话到没到 CLI。
fn register_continue(
    ctx: &Ctx,
    sources: &[Box<dyn Source>],
    s: &Session,
    message: &str,
) -> io::Result<(Command, Delivery)> {
    let source = source_of(sources, s)?;
    let cmd = source.resume_with_message_cmd(s, message);
    let delivery = source.message_delivery(s);
    Ok((ctx.register(&s.id, s.vendor, &s.cwd, cmd)?, delivery))
}

/// R 的结果：开了几个、话到没到（按家说要按回车还是要自己输入）、没打开的第一条原因。
fn resume_report(opened: &[(Vendor, Delivery)], failed: &[String]) -> String {
    let vendors = |want: Delivery| {
        let mut names: Vec<&str> = Vec::new();
        for (v, d) in opened {
            if *d == want && !names.contains(&v.name()) {
                names.push(v.name());
            }
        }
        (
            opened.iter().filter(|(_, d)| *d == want).count(),
            names.join("、"),
        )
    };
    let (sent, _) = vendors(Delivery::Sent);
    let (typed, typed_by) = vendors(Delivery::Typed);
    let (manual, manual_by) = vendors(Delivery::Manual);
    let n = opened.len();
    let head = if typed + manual == 0 {
        match failed {
            [] => return format!("已在 {n} 个新标签页接着做"),
            _ => format!("接着做了 {n} 个"),
        }
    } else {
        let mut parts = Vec::new();
        if sent > 0 {
            parts.push(format!("{sent} 个接着做了"));
        }
        if typed > 0 {
            parts.push(format!("{typed} 个话已填好、要按回车（{typed_by}）"));
        }
        if manual > 0 {
            parts.push(format!("{manual} 个带不进话、要自己输入（{manual_by}）"));
        }
        format!("开了 {n} 个新标签页：{}", parts.join("，"))
    };
    match failed.first() {
        None => head,
        Some(first) => format!("{head}，{} 个没打开：{first}", failed.len()),
    }
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
    // 新对话开在主账号里：托管 id 带上主账号的前缀，和它将来在列表里的 id 一致；临时 id 也带，
    // 几个账号同时新建的临时 id 不会撞，也只认回同一个账号里的对话（见 [`is_new_since`]）。
    let primary = ctx.accounts.iter().find(|a| a.vendor == v);
    let id = SessionId::in_account(primary.map_or("", |a| a.key.as_str()), id.as_str());
    if !v.info().presets_new_id {
        let new = NewSession {
            vendor: v,
            cwd: cwd.to_path_buf(),
            existing,
            any_account: false,
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

/// 打开一条已有对话：返回托管 id、标签页里要跑的 attach 命令，以及这次是不是新登记的托管
/// （之前守护进程里没有这条；标签页没开成时要撤掉，见 [`Worker::open_tab`]）。
fn attach_existing(
    ctx: &Ctx,
    sources: &[Box<dyn Source>],
    s: &Session,
) -> io::Result<(SessionId, Command, bool)> {
    let hosted = hosted_list(ctx);
    let listed = ctx.index.list(s.vendor)?;
    // 先 known 后 claims，和主循环同一个顺序拿锁。
    let alias = || {
        let known = lock(&ctx.known);
        hosted_alias(s, &hosted, &known, &lock(&ctx.claims), &listed)
    };
    let mut found = alias();
    // 认不准：可能是别的会话台新建、已经认领了的（审计 v0.1.4 第 13 条），接上它落盘的认领再看一次。
    if found == Hosted::Ambiguous {
        let saved = load_claims(&ctx.data_dir, SystemTime::now())?;
        lock(&ctx.claims).adopt(saved);
        found = alias();
    }
    match found {
        Hosted::Own => {
            let fresh = !hosted.iter().any(|h| h.id == s.id);
            Ok((s.id.clone(), register_resume(ctx, sources, s)?, fresh))
        }
        Hosted::Alias(id) => {
            let cmd = opener::attach_command(&ctx.exe, &id, &s.cwd);
            Ok((id, cmd, false))
        }
        Hosted::Ambiguous => Err(io::Error::other(format!(
            "这个项目里有会话台托管的 {} 对话还在跑，确认不了是不是这条；请到那个标签页里继续，或等它结束再打开",
            s.vendor
        ))),
    }
}

/// 守护进程此刻托管着、CLI 进程在跑的对话和它的状态灯（`None` 是守护进程没报），给 `ai list`、`ai search`：
/// 认领到临时 id 下的换回那条对话的 id（进程号对得上才算）。守护进程没在跑就是空的；不拉起守护进程、不改认领记录。
pub fn hosted_now(dir: &Path) -> HashMap<SessionId, Option<SessionState>> {
    let hosted = match host::client::request(dir, &Request::List) {
        Ok(Response::List(h)) => h,
        _ => return HashMap::new(),
    };
    let claims = load_claims(dir, SystemTime::now()).unwrap_or_default();
    hosted
        .into_iter()
        .filter_map(|h| {
            let pid = h.pid?;
            let id = match claims.get(&h.id) {
                Some(b) if b.pid == pid => b.real.clone(),
                // 没认领到的临时 id 还不知道是哪条对话。
                _ if temp_started(&h.id).is_some() => return None,
                _ => h.id,
            };
            Some((id, h.status))
        })
        .collect()
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
    previews: Sender<Entry>,
    /// 读「此刻」的线程：要读哪些对话（变了才发）。
    moments: Sender<Vec<Entry>>,
    /// 上次发给它的那批（id 和会话文件）。
    moment_sent: Vec<(SessionId, PathBuf)>,
    /// 答完不发提醒的 CLI 闲置收回的线程（[`reclaim`]）：闲置时长和要看的对话（变了才发）。
    reclaims: Sender<reclaim::Targets>,
    /// 上次发给它的闲置时长、守护进程里的 id 和会话文件。
    reclaim_sent: (Option<Duration>, Vec<(SessionId, PathBuf)>),
    /// 上一次整帧画出来的样子：只有转圈换帧时拿它改那几格再交给终端（见 [`view::respin`]）。
    frame: Option<Buffer>,
    /// 读整段对话的线程。
    transcripts: Sender<Entry>,
    /// 取搜索结果命中片段的线程：查询词和看得见的那几条。
    snippets: Sender<(String, Vec<Entry>)>,
    live: Sender<Option<SessionId>>,
    /// 上次告诉实时预览线程的目标。
    live_sent: Option<SessionId>,
    quotas: Sender<Account>,
    /// 叫余额线程马上读一遍（`$` 打开时）。
    balances: Sender<()>,
    seen: Sender<SessionId>,
    /// 叫备份线程马上备一轮（改了标注、存了设置）。
    backups: Sender<()>,
    jobs: Sender<Job>,
    /// 各家 CLI 线程（开会话台时「检查新版本」关着就没有）。
    cli: Option<Sender<CliRequest>>,
    rx: Receiver<Msg>,
    /// 守护进程记着的被中断对话。
    stopped: Vec<RunRecord>,
    /// 守护进程托管着的对话（上次 `List` 的结果，之后按事件增减）：认 v0.1.3 以原生 id 托管的那些用。
    hosted: Vec<HostedInfo>,
    /// 认领记录文件上次看到的样子：别的会话台改了就读回来接上（[`Claims::adopt`]）。
    claims_stamp: ClaimsStamp,
}

impl Loop {
    /// 顶栏额度现在该显示哪个账号的（厂商、账号短名）。
    fn quota_key(&self) -> (Vendor, String) {
        let a = self.app.quota_account();
        (a.vendor, a.key)
    }

    fn run(&mut self, tui: &mut Tui) -> io::Result<()> {
        let mut dirty = true;
        let mut last_draw = Instant::now();
        let started = Instant::now();
        while !self.app.quit {
            let ticked = self.app.tick_blink(started.elapsed());
            let spun = ticked && self.app.only_spun;
            dirty |= ticked && !spun;
            if dirty || last_draw.elapsed() >= CLOCK || (spun && !self.respin(tui)?) {
                self.app.now = SystemTime::now();
                let done = tui
                    .terminal
                    .draw(|f| view::draw(f, &mut self.app, &self.theme))?;
                self.frame = Some(done.buffer.clone());
                last_draw = Instant::now();
                dirty = false;
            }
            if event::poll(TICK)? {
                let quota_was = self.quota_key();
                let effects = match event::read()? {
                    Event::Key(k) => self.app.key(k),
                    Event::Mouse(m) => self.app.mouse(m, Instant::now()),
                    Event::FocusGained => {
                        self.app.board_front = true;
                        let r = self.ime.focus_gained();
                        self.ime_result(r);
                        Vec::new()
                    }
                    Event::FocusLost => {
                        self.app.board_front = false;
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
                // 换了家、或选中了另一个账号的对话：顶栏额度换成那个账号的。
                if self.quota_key() != quota_was {
                    let _ = self.quotas.send(self.app.quota_account());
                }
            }
            while let Ok(msg) = self.rx.try_recv() {
                self.receive(msg);
                dirty = true;
            }
            for e in self.app.auto_continue_due(SystemTime::now()) {
                self.apply(e, tui)?;
                dirty = true;
            }
            for id in self.notifier.settle(Instant::now()) {
                self.app.set_state(id.clone(), self.notifier.state(&id));
                dirty = true;
            }
            if lock(&self.ctx.claims).due(SystemTime::now()) {
                self.refresh_aliases();
                dirty = true;
            }
            if let Some(e) = self.app.take_preview_request() {
                let _ = self.previews.send(e);
            }
            if let Some(e) = self.app.take_transcript_request() {
                let _ = self.transcripts.send(e);
            }
            if let Some(req) = self.app.take_snippet_request() {
                let _ = self.snippets.send(req);
            }
            self.send_moment_targets();
            self.send_reclaim_targets();
            self.set_live(self.app.live_target());
            dirty |= self.mark_seen();
        }
        self.set_live(None);
        Ok(())
    }

    /// 只有转圈换了一帧：拿上一帧改转圈那几格交给终端，终端只收到这几格。没有上一帧、窗口大小变了
    /// 返回 `false`，要整帧重画。
    fn respin(&mut self, tui: &mut Tui) -> io::Result<bool> {
        let size = tui.terminal.size()?;
        let Some(last) = self
            .frame
            .as_mut()
            .filter(|b| (b.area.width, b.area.height) == (size.width, size.height))
        else {
            return Ok(false);
        };
        view::respin(last, &self.app.areas.spin, self.app.spinner());
        tui.terminal.draw(|f| f.buffer_mut().clone_from(last))?;
        Ok(true)
    }

    /// 要读「此刻」的对话变了才告诉读的线程。
    fn send_moment_targets(&mut self) {
        let targets = self.app.moment_targets();
        let keys: Vec<(SessionId, PathBuf)> = targets
            .iter()
            .map(|e| (e.session.id.clone(), e.session.path.clone()))
            .collect();
        if keys != self.moment_sent {
            let _ = self.moments.send(targets);
            self.moment_sent = keys;
        }
    }

    /// 要看记录能不能收回的对话、闲置时长变了才告诉收回线程。
    fn send_reclaim_targets(&mut self) {
        let limit = self.app.config.idle_reclaim();
        let targets = self.app.reclaim_targets();
        let keys = (
            limit,
            targets
                .iter()
                .map(|(id, e)| (id.clone(), e.session.path.clone()))
                .collect(),
        );
        if keys != self.reclaim_sent {
            let _ = self.reclaims.send((limit, targets));
            self.reclaim_sent = keys;
        }
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
        // 回来时主人就在这个标签页里（交出去期间收不到焦点事件）。
        self.app.board_front = true;
        let r = self.ime.back();
        self.ime_result(r);
        result
    }

    /// 选着「刚做完」的对话看了一会儿（[`App::take_seen`]）就算看过：本地状态灯变成不闪的「等你回复」，
    /// 再告诉守护进程，它那份提醒判断不再为这一轮弹提醒，别的会话台窗口也收到广播跟着停闪。返回要不要重画。
    fn mark_seen(&mut self) -> bool {
        let Some(id) = self.app.take_seen() else {
            return false;
        };
        self.notifier
            .notify(id.clone(), Activity::Seen, Instant::now());
        self.app.set_state(id.clone(), self.notifier.state(&id));
        let _ = self.seen.send(id);
        true
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
            Ok(entries) => {
                // 新建时定不了 id 的几家：消息数变了的记下来，等对证是不是会话台新建的那个进程写的。
                if !v.info().presets_new_id {
                    let mut claims = lock(&self.ctx.claims);
                    for e in entries.iter().filter(|e| !e.session.automatic) {
                        let s = &e.session;
                        claims.entry_seen(&s.id, s.message_count, s.last_active);
                    }
                }
                self.app.set_entries(v, entries);
            }
            Err(e) => self.app.status = Some(format!("列表没读到：{e}")),
        }
        // 会话台开着时第一次用这家（之前没有数据目录、标签藏着）：读到它的对话就把标签补回来。
        if !self.app.shown.contains(&v) && !self.app.list_of(v).entries.is_empty() {
            let accounts = Arc::clone(&self.ctx.accounts);
            self.app.hide_missing(|x| installed(&accounts, x));
        }
        // 会话文件有变化才会走到这里，顺带重查最近在写的文件（判断「别处在用」）。
        let lists = &self.app.lists;
        self.app.written =
            index::recent_writes(lists.iter().flat_map(|l| &l.entries), SystemTime::now());
        self.refresh_interrupted();
    }

    /// 托管在临时 id 下的新对话认回列表里的真实 id：认领记录文件变了（别的会话台认领、撤销过，审计 v0.1.4
    /// 第 13 条）先读回来接上，再对证攒下的变化（[`Claims::settle`]），认领到的交给界面（状态、实时预览、已读、
    /// x 结束按临时 id），认不准的界面也不当成别处在用。
    fn refresh_aliases(&mut self) {
        match self
            .claims_stamp
            .reload(&self.ctx.data_dir, SystemTime::now())
        {
            Ok(Some(saved)) => lock(&self.ctx.claims).adopt(saved),
            Ok(None) => {}
            Err(e) => self.app.status = Some(format!("认领记录没读到：{e}")),
        }
        let app = &self.app;
        let known = lock(&self.ctx.known);
        let mut claims = lock(&self.ctx.claims);
        let temps: Vec<HostedInfo> = claims
            .running()
            .filter_map(|(id, pid)| {
                let n = known.get(id)?;
                Some(HostedInfo {
                    id: id.clone(),
                    vendor: n.vendor,
                    project: String::new(),
                    cwd: n.cwd.clone(),
                    pid: Some(pid),
                    attached: false,
                    title: None,
                    status: None,
                })
            })
            .collect();
        let vendors: Vec<Vendor> = temps.iter().map(|h| h.vendor).collect();
        let lists = Vendor::ALL
            .into_iter()
            .filter(|v| vendors.contains(v))
            .map(|v| app.list_of(v).entries.as_slice());
        let mut pairs = Vec::new();
        for listed in lists.clone() {
            // 以自己的 id 托管着、在跑的（不经认领看原始状态，认领到的真实 id 不算）。
            pairs.extend(time_pairs(listed, &temps, &known, |id| {
                app.states
                    .get(id)
                    .is_some_and(|s| *s != SessionState::Stopped)
            }));
        }
        let changed = claims.settle(SystemTime::now(), &pairs);
        let mut aliases = HashMap::new();
        let mut aliased = HashSet::new();
        let mut note = |id: &SessionId, h: Hosted| match h {
            Hosted::Alias(temp) => {
                aliased.insert(id.clone());
                aliases.insert(id.clone(), temp);
            }
            Hosted::Ambiguous => {
                aliased.insert(id.clone());
            }
            Hosted::Own => {}
        };
        for listed in lists {
            for e in listed {
                if pairs.iter().any(|(r, _)| *r == e.session.id) {
                    note(
                        &e.session.id,
                        hosted_alias(&e.session, &temps, &known, &claims, listed),
                    );
                }
            }
        }
        // v0.1.3 以原生 id 托管、还在跑的：对到的那条（工作账号下带前缀的 id）状态、预览、结束都按它。
        // 这家只有默认账号时列表里没有带前缀的 id，不用对。
        let accounts = &self.ctx.accounts;
        let prefixed = |v: Vendor| accounts.iter().any(|a| a.vendor == v && !a.is_default());
        for h in self
            .hosted
            .iter()
            .filter(|h| h.pid.is_some() && native_hosted(&h.id) && prefixed(h.vendor))
        {
            let listed = app.list_of(h.vendor).entries.as_slice();
            for e in listed
                .iter()
                .filter(|e| e.session.id.native() == h.id.as_str())
            {
                note(
                    &e.session.id,
                    hosted_alias(&e.session, &self.hosted, &known, &claims, listed),
                );
            }
        }
        let saved = if changed.is_empty() {
            Ok(())
        } else {
            save_claims(&self.ctx.data_dir, &changed, claims.bound())
        };
        drop(claims);
        drop(known);
        if let Err(e) = saved {
            self.app.status = Some(format!("认领记录没写成：{e}"));
        }
        self.app.set_aliases(aliases);
        self.app.aliased = aliased;
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
        let map = resolve_interrupted(
            &self.stopped,
            |v| app.list_of(v).entries.as_slice(),
            lock(&self.ctx.claims).bound(),
        );
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

    /// `v` 为 `None` 是各家一起搜。
    fn search(&mut self, v: Option<Vendor>, query: &str) {
        let hits = match v {
            Some(v) => self.index.search(v, query, SEARCH_LIMIT),
            None => self.index.search_all(query, SEARCH_LIMIT),
        };
        match hits {
            Ok(hits) => self.app.set_search_hits(v, query, hits),
            Err(e) => self.app.status = Some(format!("搜索出错：{e}")),
        }
    }

    fn apply(&mut self, effect: Effect, tui: &mut Tui) -> io::Result<()> {
        let job = match effect {
            Effect::RefreshBalances => {
                let _ = self.balances.send(());
                return Ok(());
            }
            Effect::UpgradeClis => {
                let sent = self
                    .cli
                    .as_ref()
                    .is_some_and(|c| c.send(CliRequest::UpgradeAll).is_ok());
                if !sent {
                    self.app.cli_upgrade = CliUpgrade::Idle;
                    self.app.status = Some(
                        "各家 CLI 的后台线程没在跑（开会话台时关着检查新版本），重开会话台再试"
                            .to_owned(),
                    );
                }
                return Ok(());
            }
            Effect::Search { vendor, query } => {
                self.search(vendor, &query);
                return Ok(());
            }
            Effect::Annotate { vendor, id, edit } => {
                match self.index.annotate(vendor, &id, edit) {
                    // 置顶、改名、归档过的 Claude 对话要备份：马上备一轮。
                    Ok(()) => _ = self.backups.send(()),
                    Err(e) => self.app.status = Some(format!("没存上：{e}")),
                }
                self.reload(vendor);
                if let Some(q) = self.app.search.as_ref().map(|s| s.query.clone())
                    && !q.is_empty()
                {
                    self.search(self.app.search_scope(), &q);
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
                // 可能刚开了「备份全部 Claude 对话」。
                let _ = self.backups.send(());
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
            // 开不了新标签页的终端不自动继续：只能在当前窗口接，会把会话台换下去。
            Effect::AutoContinue { entry, .. } if !self.ctx.terminal.has_tabs() => {
                self.app.status = Some(format!(
                    "「{}」到了额度重置时间，{} 开不了新标签页，没自动继续：选中按 ⏎ 接着做",
                    truncate(entry.title(), 16),
                    self.ctx.terminal
                ));
                return Ok(());
            }
            Effect::AutoContinue {
                entry,
                hosted,
                resume_at,
            } => Job::AutoContinue(entry, hosted, resume_at),
            Effect::Trash(e) => Job::Trash(e),
            Effect::Untrash(v) => Job::Untrash(v),
            // 放回了就在新标签页接着聊；开不了新标签页的终端只放回。
            Effect::PutBack(e) if self.ctx.terminal.has_tabs() => self.leave_for(Job::PutBack(e)),
            Effect::PutBack(e) => Job::PutBack(e),
            Effect::Finder(d) => Job::Finder(d),
            Effect::GitStatus(d) => Job::Git(d),
            Effect::CopyResume(e) => Job::Copy(e),
            Effect::Export(e) => Job::Export(e),
        };
        let _ = self.jobs.send(job);
        Ok(())
    }

    /// 在当前窗口接上对话，CLI 退出、被别处接走或按回到会话台的键脱开后回到会话台。
    /// 回来时告诉守护进程看过了：接着期间它就在这个窗口里，答完的最后一轮主人看着呢，不该回来还闪。
    /// 走守护进程再广播回来，排在接着期间的状态事件后面，不会被它们盖掉。
    fn open_here(&mut self, tui: &mut Tui, s: &Session) -> io::Result<String> {
        let (id, cmd) = match self
            .ctx
            .sources()
            .and_then(|src| attach_existing(&self.ctx, &src, s))
        {
            Ok((id, c, _)) => (id, c),
            Err(e) => return Ok(format!("没打开：{e}")),
        };
        self.hint_back_key(tui)?;
        let result = self.suspend(tui, || InlineOpener.open(&cmd, ""))?;
        Ok(self.back_here(id, result))
    }

    fn new_here(&mut self, tui: &mut Tui, v: Vendor, cwd: &Path) -> io::Result<String> {
        let existing = ids_in(&self.index, v, cwd);
        let (id, cmd) = match self
            .ctx
            .sources()
            .and_then(|src| register_new(&self.ctx, &src, v, cwd, existing))
        {
            Ok(x) => x,
            Err(e) => return Ok(format!("没新建：{e}")),
        };
        self.hint_back_key(tui)?;
        let result = self.suspend(tui, || InlineOpener.open(&cmd, ""))?;
        Ok(self.back_here(id, result))
    }

    /// 屏幕交给当前窗口里的 CLI 之前，底栏先亮 [`BACK_KEY_HINT`]「⌃\ 回到会话台」：交出去以后会话台说不上话了。
    /// 设置里关了回到会话台的键就不亮、不等。
    fn hint_back_key(&mut self, tui: &mut Tui) -> io::Result<()> {
        let Some(hint) = self.app.back_key_hint() else {
            return Ok(());
        };
        self.app.status = Some(hint);
        tui.terminal
            .draw(|f| view::draw(f, &mut self.app, &self.theme))?;
        thread::sleep(BACK_KEY_HINT);
        Ok(())
    }

    /// 当前窗口里接的对话交回会话台之后：告诉守护进程看过了（见 `open_here`），给底栏一句话。
    /// CLI 还在跑说明是按回到会话台的键脱开的（CLI 退出时守护进程先注销再结束 attach），之后回车或 w 接回去。
    fn back_here(&self, id: SessionId, result: io::Result<Opened>) -> String {
        let running = result.is_ok()
            && hosted_list(&self.ctx)
                .iter()
                .any(|h| h.id == id && h.pid.is_some());
        let _ = self.seen.send(id);
        match result {
            Ok(_) if running => "已回到会话台，对话在后台继续".to_owned(),
            Ok(_) => "已回到会话台".to_owned(),
            Err(e) => format!("对话结束：{e}"),
        }
    }

    fn receive(&mut self, msg: Msg) {
        let now = Instant::now();
        match msg {
            Msg::Index(Update::Listed(v)) => self.reload(v),
            Msg::Index(Update::Searchable) => {
                if let Some(q) = self.app.search.as_ref().map(|s| s.query.clone())
                    && !q.is_empty()
                {
                    self.search(self.app.search_scope(), &q);
                }
            }
            Msg::Index(Update::Problem(_)) => {}
            Msg::Preview(id, p) => self.app.set_preview(id, p),
            Msg::Moment(id, m) => self.app.set_moment(id, m),
            Msg::Transcript(id, at, t) => self.app.set_transcript(&id, at, t),
            Msg::Snippet(query, id, snip) => self.app.set_snippet(&query, id, snip),
            Msg::Live(id, snap) => {
                // 目标已经换了的旧画面丢掉。
                if self.live_sent.as_ref() == Some(&id) {
                    self.app.live = snap.map(|s| (id, s));
                }
            }
            Msg::Quota(v, key, r) => {
                self.app.quota.insert((v, key), r);
            }
            Msg::Balance(report) => self.app.balance = Some(report),
            Msg::Context(c) => self.app.contexts = c,
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
                // 重新同步之前漏没漏事件不知道：重新开始看。
                lock(&self.ctx.claims).unwatch_all();
                self.hosted.clear();
                self.learn_hosted(infos.clone());
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
            Msg::Learned(infos) => self.learn_hosted(infos),
            Msg::Interrupted(list) => {
                self.stopped = list;
                self.refresh_interrupted();
            }
            Msg::Host(ev, at) => self.host_event(ev, at, now),
            Msg::OldHost(old) => self.app.old_host = old.then(HashSet::new),
            Msg::HostGone => {
                lock(&self.ctx.claims).unwatch_all();
                self.hosted.clear();
                self.app.old_host = None;
                self.notifier = Notifier::new();
                self.forget_hosted(&HashSet::new());
            }
            Msg::Status(s) => self.app.status = Some(s),
            Msg::Git(dir, out) => self.app.git = Some((dir, out)),
            Msg::Update(v) => self.app.new_version = Some(v),
            Msg::CliUpdates(r) => self.app.set_cli_report(r),
            Msg::CliProgress(done, total) => self.app.cli_progress(done, total),
            Msg::CliDone(results) => self.app.cli_upgrade_done(results),
            Msg::CliAuto(results) => self.app.cli_auto_done(results),
        }
        self.refresh_aliases();
    }

    /// 记下托管列表（按 id 更新或补上，没列出的留着：补取失败时是空的）：临时 id 记进 `known`
    /// （别的会话台新建的也认得厂商和项目，审计 v0.1.4 第 13 条）、看着在跑的进程，再接上别的会话台落盘的认领（[`Claims::adopt`]）。
    fn learn_hosted(&mut self, infos: Vec<HostedInfo>) {
        let (known, claims) = (Arc::clone(&self.ctx.known), Arc::clone(&self.ctx.claims));
        let mut known = lock(&known);
        let mut claims = lock(&claims);
        let mut changed = Vec::new();
        // 旧版守护进程托管的临时 id 不带账号前缀时，分不出是哪个账号新建的。
        let old_host = self.app.old_host.is_some();
        for i in infos.iter().filter(|i| temp_started(&i.id).is_some()) {
            known.entry(i.id.clone()).or_insert_with(|| NewSession {
                vendor: i.vendor,
                cwd: i.cwd.clone(),
                existing: HashSet::new(),
                any_account: old_host && i.id.split_account().0.is_empty(),
            });
            if let Some(pid) = i.pid
                && claims.watch(&i.id, pid, SystemTime::now())
            {
                changed.push(i.id.clone());
            }
        }
        self.save_claims(&claims, &changed);
        match load_claims(&self.ctx.data_dir, SystemTime::now()) {
            Ok(saved) => claims.adopt(saved),
            Err(e) => self.app.status = Some(format!("认领记录没读到：{e}")),
        }
        drop(claims);
        drop(known);
        for i in infos {
            match self.hosted.iter_mut().find(|h| h.id == i.id) {
                Some(h) => *h = i,
                None => self.hosted.push(i),
            }
        }
    }

    /// 临时 id 那个进程的动静记进认领证据：起了（换了）进程、有输出、退出。
    /// 起进程的 id 没见过（别的会话台新建的，`Started` 里没有厂商和项目）就补取一次托管列表。
    fn watch_temp(&mut self, ev: &HostEvent, at: SystemTime) {
        let pid = match ev {
            HostEvent::Started { pid, .. } => Some(*pid),
            _ => None,
        };
        if let HostEvent::Started { id, .. } | HostEvent::Exited { id, .. } = ev {
            match self.hosted.iter_mut().find(|h| h.id == *id) {
                Some(h) => h.pid = pid,
                None if pid.is_some() => {
                    let _ = self.jobs.send(Job::Learn);
                }
                None => {}
            }
        }
        let claims = Arc::clone(&self.ctx.claims);
        let mut claims = lock(&claims);
        match ev {
            HostEvent::Started { id, pid } if temp_started(id).is_some() => {
                if claims.watch(id, *pid, at) {
                    self.save_claims(&claims, std::slice::from_ref(id));
                }
            }
            HostEvent::Output { id }
            | HostEvent::Bell { id }
            | HostEvent::Notify { id, .. }
            | HostEvent::Title { id, .. } => claims.output(id, at),
            HostEvent::Exited { id, .. } => claims.unwatch(id),
            _ => {}
        }
    }

    /// 认领变了的临时 id 落盘；没写成在底栏说一声。
    fn save_claims(&mut self, claims: &Claims, changed: &[SessionId]) {
        if changed.is_empty() {
            return;
        }
        if let Err(e) = save_claims(&self.ctx.data_dir, changed, claims.bound()) {
            self.app.status = Some(format!("认领记录没写成：{e}"));
        }
    }

    fn host_event(&mut self, ev: HostEvent, at: SystemTime, now: Instant) {
        self.watch_temp(&ev, at);
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

#[cfg(test)]
mod tests {
    use super::jobs::{Worker, open_or_undo};
    use super::workers::{Stamp, moment_due, pass_host_event, stamp};
    use super::*;
    use crate::sources::codex::id_created as codex_id_created;
    use crate::sources::{antigravity, created_at as id_created, hermes};
    use crate::ui::claims::CLAIMS_FILE;

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
            Ok(Msg::Host(HostEvent::Alert { .. }, _))
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
            Ok(Msg::Host(HostEvent::Bell { .. }, _))
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
        // 按时间对得上只算可能是它（没有证据不认领，见 `Claims`）。
        let alias = |h: HostedInfo| {
            hosted_alias(
                &s,
                std::slice::from_ref(&h),
                &HashMap::new(),
                &Claims::default(),
                &[],
            )
        };
        let temp = temp_at(created - Duration::from_secs(5));
        assert_eq!(alias(temp), Hosted::Ambiguous);
        // id 只到秒：同一秒里稍早一点新建的也对得上。
        let same_second = temp_at(created + Duration::from_millis(500));
        assert_eq!(alias(same_second), Hosted::Ambiguous);
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
                &Claims::default(),
                &[],
            )
        };
        let temp = temp_at(created - Duration::from_secs(3));
        assert_eq!(alias(temp), Hosted::Ambiguous);
        // 新建之前就有的不认（精确到毫秒）。
        assert_eq!(
            alias(temp_at(created + Duration::from_millis(1))),
            Hosted::Own
        );
    }

    #[test]
    fn moment_is_read_at_most_every_15s_and_only_when_the_file_changed() {
        let t = Instant::now();
        let a: Stamp = vec![Some((10, SystemTime::UNIX_EPOCH)), None];
        let b: Stamp = vec![Some((20, SystemTime::UNIX_EPOCH)), None];
        assert!(moment_due(None, &a, t), "没读过的马上读");
        let last = (a.clone(), t);
        assert!(
            !moment_due(Some(&last), &a, t + MOMENT_EVERY * 4),
            "文件没变不读"
        );
        assert!(
            !moment_due(Some(&last), &b, t + Duration::from_secs(14)),
            "变了也等满 15 秒"
        );
        assert!(moment_due(Some(&last), &b, t + MOMENT_EVERY));
        // SQLite 几家写进 -wal 时主库不动，也算变了。
        let wal: Stamp = vec![a[0], Some((4096, SystemTime::UNIX_EPOCH))];
        assert!(moment_due(Some(&last), &wal, t + MOMENT_EVERY));
        // 上次没读成记的是空版本：文件没变也过 15 秒再试。
        let failed = (Stamp::new(), t);
        assert!(!moment_due(Some(&failed), &a, t + Duration::from_secs(14)));
        assert!(moment_due(Some(&failed), &a, t + MOMENT_EVERY));
    }

    #[test]
    fn stamp_sees_wal_next_to_the_file() {
        let dir = std::env::temp_dir().join(format!("huitai-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");
        std::fs::write(&db, b"x").unwrap();
        let before = stamp(&sources::with_wal(&db));
        assert!(before[0].is_some() && before[1].is_none());
        std::fs::write(dir.join("state.db-wal"), b"yy").unwrap();
        assert_ne!(stamp(&sources::with_wal(&db)), before);
        assert_eq!(
            stamp(&sources::with_wal(&dir.join("gone.jsonl"))),
            [None, None]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 对话都在一个库里的几家：会话路径是空的（Hermes、OpenCode）或是产物目录（Antigravity），
    /// 「此刻」要看库和它的 WAL 才知道有新回复。
    #[test]
    fn database_sessions_moment_follows_their_database() {
        let dir = std::env::temp_dir().join(format!("huitai-moment-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let trash = || Trash::new(dir.join("trash"));
        let cases: [(Box<dyn Source>, PathBuf, PathBuf); 3] = [
            (
                Box::new(sources::HermesSource::new(dir.join("hermes"), trash())),
                dir.join("hermes/state.db"),
                PathBuf::new(),
            ),
            (
                Box::new(sources::OpenCodeSource::new(
                    dir.join("opencode/opencode.db"),
                    trash(),
                )),
                dir.join("opencode/opencode.db"),
                PathBuf::new(),
            ),
            (
                Box::new(sources::AntigravitySource::new(dir.join("agy"), trash())),
                dir.join(format!("agy/{}", antigravity::SUMMARIES_DB)),
                dir.join("agy/brain/c1"),
            ),
        ];
        for (src, db, path) in cases {
            std::fs::create_dir_all(db.parent().unwrap()).unwrap();
            std::fs::write(&db, b"x").unwrap();
            if !path.as_os_str().is_empty() {
                std::fs::create_dir_all(&path).unwrap();
            }
            let session = Session {
                vendor: src.vendor(),
                id: SessionId::from("c1"),
                cwd: PathBuf::from("/p"),
                path,
                title: None,
                git_branch: None,
                last_active: SystemTime::UNIX_EPOCH,
                message_count: 1,
                automatic: false,
                account: String::new(),
            };
            let t = Instant::now();
            let last = (stamp(&src.moment_files(&session)), t);
            let mut wal = db.into_os_string();
            wal.push("-wal");
            std::fs::write(&wal, b"new reply").unwrap();
            let now = stamp(&src.moment_files(&session));
            assert!(
                moment_due(Some(&last), &now, t + MOMENT_EVERY),
                "{}：库的 WAL 写了新回复要重读",
                src.vendor()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
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
            account: String::new(),
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
            status: None,
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
            hosted_alias(s, h, known, &Claims::default(), &[])
        };
        // 新建之后才生成的同项目对话：可能是它，没认领前谁也不接。
        assert_eq!(alias(&real, &h, &none), Hosted::Ambiguous);
        // 认领过、认领时的进程还在：接到临时 id 那个进程上。
        let claimed = |pid| {
            Claims::restore(HashMap::from([(
                SessionId::from(TEMP),
                Bound {
                    real: real.id.clone(),
                    pid,
                },
            )]))
        };
        assert_eq!(
            hosted_alias(&real, &h, &none, &claimed(42), &[]),
            Hosted::Alias(SessionId::from(TEMP))
        );
        // 临时 id 下已经换了个进程（CLI 退出后又接上，起的是新对话）：认领不算数，也不能当成它自己。
        assert_eq!(
            hosted_alias(&real, &h, &none, &claimed(7), &[]),
            Hosted::Ambiguous
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
                any_account: false,
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

    /// 恢复/新建命令前带的目录变量：非默认账号带自己的目录；默认账号只有一个账号时不带，几个账号时清掉变量。
    #[test]
    fn env_prefix_follows_the_account_in_the_id() {
        let home = Path::new("/h");
        let default = Account::new(Vendor::Codex, "默认", PathBuf::from("/h/.codex"), home);
        let work = Account::new(
            Vendor::Codex,
            "工作号",
            PathBuf::from("/h/codex work"),
            home,
        );
        let claude = Account::new(Vendor::Claude, "默认", PathBuf::from("/h/.claude"), home);
        let all = [default.clone(), work.clone(), claude];
        assert_eq!(
            env_prefix_of(&all, Vendor::Codex, &work.key, home),
            "CODEX_HOME='/h/codex work' "
        );
        assert_eq!(
            env_prefix_of(&all, Vendor::Codex, "", home),
            "env -u CODEX_HOME "
        );
        assert_eq!(env_prefix_of(&all, Vendor::Claude, "", home), "");
        // 账号在设置里删掉了：按默认账号算。
        assert_eq!(
            env_prefix_of(&[default], Vendor::Codex, &work.key, home),
            ""
        );
        // 改目录的变量不止一个的（OpenCode）：默认账号全清掉（继承的 XDG_DATA_HOME 不能把它带走）。
        let two = |vendor: Vendor, other: &str| {
            let default = crate::core::VendorDirs::defaults(home)
                .dir(vendor)
                .to_path_buf();
            [
                Account::new(vendor, "默认", default, home),
                Account::new(vendor, "别处", PathBuf::from(other), home),
            ]
        };
        assert_eq!(
            env_prefix_of(
                &two(Vendor::OpenCode, "/x/opencode/opencode.db"),
                Vendor::OpenCode,
                "",
                home
            ),
            "env -u OPENCODE_DB -u XDG_DATA_HOME "
        );
    }

    /// DECISIONS「Codex 第三轮审计第 1 条」：两个账号（两个 `CODEX_HOME`）同一时刻在同一项目各自新建，
    /// 临时 id 带着账号前缀不撞；每条真实对话只认回自己账号的那个临时 id。
    #[test]
    fn temp_ids_of_two_accounts_only_map_back_within_their_account() {
        let other = "0123abcd";
        let temp_b = format!("{other}:{TEMP}");
        let h = [
            hosted(TEMP, "/p/shop", true),
            hosted(&temp_b, "/p/shop", true),
        ];
        let none = HashMap::new();
        let real_a = session(&v7(1_000_050, 1), "/p/shop", 1_000_100);
        let mut real_b = session(
            &format!("{other}:{}", v7(1_000_051, 2)),
            "/p/shop",
            1_000_100,
        );
        real_b.account = other.to_owned();
        let both = entries(&[real_a.clone(), real_b.clone()]);
        assert_eq!(
            time_pairs(&both, &h, &none, |_| false),
            vec![
                (real_a.id.clone(), SessionId::from(TEMP)),
                (real_b.id.clone(), SessionId::from(temp_b.as_str())),
            ]
        );
        assert_eq!(
            temp_started(&SessionId::from(temp_b.as_str())),
            temp_started(&SessionId::from(TEMP))
        );
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
        let empty = Claims::default();
        assert_eq!(hosted_alias(&old, &h, &none, &empty, &[]), Hosted::Own);
        assert!(time_pairs(&only_old, &h, &none, |_| false).is_empty());
        // 都在列表里：旧对话还是它自己，只有新建那条可能是临时 id 那条。
        let both = entries(&[old.clone(), real.clone()]);
        assert_eq!(hosted_alias(&old, &h, &none, &empty, &[]), Hosted::Own);
        assert_eq!(
            hosted_alias(&real, &h, &none, &empty, &[]),
            Hosted::Ambiguous
        );
        assert_eq!(
            time_pairs(&both, &h, &none, |_| false),
            vec![(real.id.clone(), SessionId::from(TEMP))]
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
        let empty = Claims::default();
        assert_eq!(hosted_alias(&a, &h, &none, &empty, &[]), Hosted::Ambiguous);
        assert_eq!(hosted_alias(&b, &h, &none, &empty, &[]), Hosted::Ambiguous);
        assert_eq!(time_pairs(&list, &h, &none, |_| false).len(), 2);
        // 另一条托管在它自己的 id 下（肯定不是临时 id 那条）：不算对手。
        assert_eq!(
            time_pairs(&list, &h, &none, |id| *id == b.id),
            vec![(a.id.clone(), SessionId::from(TEMP))]
        );
    }

    #[test]
    fn two_new_codex_conversations_in_one_project_are_ambiguous() {
        let h = [
            hosted(TEMP, "/p/shop", true),
            hosted("new-codex-1000020000", "/p/shop", true),
        ];
        let real = session(&v7(1_000_050, 1), "/p/shop", 1_000_100);
        assert_eq!(
            hosted_alias(&real, &h, &HashMap::new(), &Claims::default(), &[]),
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
                any_account: false,
            },
        )]);
        let real = v7(1_000_050, 1);
        let list = entries(&[
            session(&real, "/p/shop", 1_000_100),
            session(&busy, "/p/shop", 1_000_100),
            session(&v7(1_000_050, 3), "/p/blog", 1_000_100),
            session(&v7(999_000, 4), "/p/shop", 1_000_100),
        ]);
        // 临时 id 那个进程在跑：新建之后才生成、同项目、新建时还没有的那条可能是它。
        let running = [hosted(TEMP, "/p/shop", true)];
        assert_eq!(
            time_pairs(&list, &running, &known, |_| false),
            vec![(SessionId::from(real.as_str()), temp)]
        );
        // 进程退出了：谁也不是。
        let stopped = [hosted(TEMP, "/p/shop", false)];
        assert!(time_pairs(&list, &stopped, &known, |_| false).is_empty());
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

    /// 临时 id → 认领到的那条（进程号随便，中断恢复不看进程号）。
    fn bound(pairs: &[(&str, &str)]) -> HashMap<SessionId, Bound> {
        pairs
            .iter()
            .map(|(temp, real)| {
                (
                    SessionId::from(*temp),
                    Bound {
                        real: SessionId::from(*real),
                        pid: 42,
                    },
                )
            })
            .collect()
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
            // 会话台新建的 Codex 对话（临时 id）：认回它认领过的那条。
            record(TEMP, Vendor::Codex, "/p/shop"),
            // 列表里没有这条（从没写出会话文件）：不列。
            record("ghost", Vendor::Codex, "/p/shop"),
        ];
        let lists = |v| match v {
            Vendor::Codex => codex.as_slice(),
            _ => &[],
        };
        // 没认领过：光按时间对得上（同项目、之后才生成的只有一条）也不列（审计 01）。
        assert_eq!(
            resolve_interrupted(&records, lists, &HashMap::new()),
            HashMap::from([(SessionId::from("plain"), SessionId::from("plain"))])
        );
        let map = resolve_interrupted(&records, lists, &bound(&[(TEMP, &real)]));
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
        let old = v7(999_000, 1);
        let codex = entries(&[session(&old, "/p/shop", 1_000_200)]);
        let records = [record(TEMP, Vendor::Codex, "/p/shop")];
        assert!(resolve_interrupted(&records, |_| codex.as_slice(), &HashMap::new()).is_empty());
        // 认领记录写错了（指着新建之前就有的对话）也不认。
        assert!(
            resolve_interrupted(&records, |_| codex.as_slice(), &bound(&[(TEMP, &old)])).is_empty()
        );
    }

    #[test]
    fn ambiguous_temp_interrupted_codex_is_not_listed() {
        let codex = entries(&[
            session(&v7(1_000_050, 1), "/p/shop", 1_000_100),
            session(&v7(1_000_060, 2), "/p/shop", 1_000_200),
        ]);
        let records = [record(TEMP, Vendor::Codex, "/p/shop")];
        assert!(resolve_interrupted(&records, |_| codex.as_slice(), &HashMap::new()).is_empty());
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
        let records = [r];
        assert_eq!(
            resolve_interrupted(&records, |_| codex.as_slice(), &bound(&[(TEMP, &latest)])),
            HashMap::from([(SessionId::from(latest.as_str()), SessionId::from(TEMP))])
        );
        // 认领的还是上一次跑的那条（换进程时会话台没开着）：不认。
        assert!(
            resolve_interrupted(&records, |_| codex.as_slice(), &bound(&[(TEMP, &earlier)]))
                .is_empty()
        );
    }

    #[test]
    fn one_entry_claimed_by_two_interrupted_records_is_not_listed() {
        let real = v7(1_000_050, 1);
        let codex = entries(&[session(&real, "/p/shop", 1_000_100)]);
        let other = "new-codex-1000010000";
        let records = [
            record(TEMP, Vendor::Codex, "/p/shop"),
            record(other, Vendor::Codex, "/p/shop"),
        ];
        let both = bound(&[(TEMP, &real), (other, &real)]);
        assert!(resolve_interrupted(&records, |_| codex.as_slice(), &both).is_empty());
    }

    /// 工作账号（短名 `0badcafe`）下原生 id 是 `native` 的对话。
    fn in_work(native: &str, cwd: &str) -> Session {
        in_account("0badcafe", native, cwd)
    }

    fn in_account(key: &str, native: &str, cwd: &str) -> Session {
        let mut s = session(&format!("{key}:{native}"), cwd, 1_000_100);
        s.account = key.to_owned();
        s
    }

    /// 审计 v0.1.4 第 7 条：v0.1.3 在自定义目录（现在的工作账号）下托管的对话记的是原生 id，新版列表里它带了账号前缀。
    /// 回车要接到那个还在跑的进程上，不再起第二个 CLI；两个工作账号里同项目都有这个 id 时认不准，两条都不接。
    /// 默认账号有这个 id 时照旧是默认账号那条（新版托管默认账号的对话也记原生 id）。
    #[test]
    fn native_id_hosted_by_v013_maps_to_the_prefixed_entry() {
        let none = HashMap::new();
        let claims = Claims::default();
        let h = [hosted("x-1", "/p/shop", true)];
        let work = in_work("x-1", "/p/shop");
        let listed = entries(&[work.clone(), session("other", "/p/shop", 5)]);
        assert_eq!(
            hosted_alias(&work, &h, &none, &claims, &listed),
            Hosted::Alias(SessionId::from("x-1"))
        );
        // 别的项目里的同名对话不是它。
        let elsewhere = in_work("x-1", "/p/blog");
        let listed = entries(std::slice::from_ref(&elsewhere));
        assert_eq!(
            hosted_alias(&elsewhere, &h, &none, &claims, &listed),
            Hosted::Own
        );
        // 两个工作账号同项目都有这个 id（默认账号没有）：哪条都认不准，不接也不另起。
        let other = in_account("0deadbee", "x-1", "/p/shop");
        let listed = entries(&[work.clone(), other.clone()]);
        assert_eq!(
            hosted_alias(&work, &h, &none, &claims, &listed),
            Hosted::Ambiguous
        );
        assert_eq!(
            hosted_alias(&other, &h, &none, &claims, &listed),
            Hosted::Ambiguous
        );
        // 新版托管的默认账号对话（原生 id），工作账号里同项目有同 id 的副本：默认账号那条是它自己，
        // 工作账号那条没托管，各开各的（和改之前一样）。
        let default = session("x-1", "/p/shop", 1_000_100);
        let listed = entries(&[default.clone(), work.clone()]);
        assert_eq!(
            hosted_alias(&default, &h, &none, &claims, &listed),
            Hosted::Own
        );
        assert_eq!(
            hosted_alias(&work, &h, &none, &claims, &listed),
            Hosted::Own
        );
        // 只在别的项目有同名的另一条：同项目那条就是它。
        let far = in_work("x-1", "/p/blog");
        let listed = entries(&[default.clone(), far.clone()]);
        assert_eq!(
            hosted_alias(&default, &h, &none, &claims, &listed),
            Hosted::Own
        );
        assert_eq!(hosted_alias(&far, &h, &none, &claims, &listed), Hosted::Own);
        // 新版托管的工作账号那条（带前缀的 id）是它自己，默认账号那条照旧是原生 id 那个。
        let both = [
            hosted("x-1", "/p/shop", true),
            hosted("0badcafe:x-1", "/p/shop", true),
        ];
        let listed = entries(&[default.clone(), work.clone()]);
        assert_eq!(
            hosted_alias(&work, &both, &none, &claims, &listed),
            Hosted::Own
        );
        assert_eq!(
            hosted_alias(&default, &both, &none, &claims, &listed),
            Hosted::Own
        );
    }

    /// 审计 v0.1.4 第 7 条复审：默认账号有同 id 副本、但在别的项目；v0.1.3 托管的是工作账号同项目那条。
    /// 以前先认给默认账号那条（不看项目），工作账号那条又能再起一个 CLI、中断记录也挂到默认账号那条。
    /// 现在要项目也相同：接到工作账号那条；默认账号那条的 id 被占着、又对不上，认不准，不另起。
    /// 旧版守护进程托管、不带账号前缀的临时 id（v0.1.3 新建的工作账号对话）：工作账号里按时间对得上的也参与对证，
    /// 没认领时是认不准（不另起），认领了就接回那个进程。
    #[test]
    fn v013_hosts_match_by_project_and_across_accounts() {
        let none = HashMap::new();
        let claims = Claims::default();
        let h = [hosted("x-1", "/p/shop", true)];
        let default_far = session("x-1", "/p/blog", 1_000_100);
        let work = in_work("x-1", "/p/shop");
        let listed = entries(&[default_far.clone(), work.clone()]);
        assert_eq!(
            hosted_alias(&work, &h, &none, &claims, &listed),
            Hosted::Alias(SessionId::from("x-1"))
        );
        assert_eq!(
            hosted_alias(&default_far, &h, &none, &claims, &listed),
            Hosted::Ambiguous
        );
        let records = [record("x-1", Vendor::Codex, "/p/shop")];
        assert_eq!(
            resolve_interrupted(&records, |_| listed.as_slice(), &HashMap::new()),
            HashMap::from([(work.id.clone(), SessionId::from("x-1"))])
        );

        // 不带前缀的临时 id，工作账号里同项目、之后才生成的那条。
        let temp = [hosted(TEMP, "/p/shop", true)];
        let fresh = in_work(&v7(1_000_050, 1), "/p/shop");
        let listed = entries(std::slice::from_ref(&fresh));
        let known = |any_account| {
            HashMap::from([(
                SessionId::from(TEMP),
                NewSession {
                    vendor: Vendor::Codex,
                    cwd: PathBuf::from("/p/shop"),
                    existing: HashSet::new(),
                    any_account,
                },
            )])
        };
        // 新版自己新建的（知道是默认账号）：工作账号那条不是它。
        assert_eq!(
            hosted_alias(&fresh, &temp, &known(false), &claims, &listed),
            Hosted::Own
        );
        // 旧版守护进程托管的：各账号都可能，没认领就认不准，不另起。
        let old = known(true);
        assert_eq!(
            hosted_alias(&fresh, &temp, &old, &claims, &listed),
            Hosted::Ambiguous
        );
        assert_eq!(
            time_pairs(&listed, &temp, &old, |_| false),
            vec![(fresh.id.clone(), SessionId::from(TEMP))]
        );
        let claimed = Claims::restore(HashMap::from([(
            SessionId::from(TEMP),
            Bound {
                real: fresh.id.clone(),
                pid: 42,
            },
        )]));
        assert_eq!(
            hosted_alias(&fresh, &temp, &old, &claimed, &listed),
            Hosted::Alias(SessionId::from(TEMP))
        );
        // 中断记录：认领到工作账号那条的，认回它。
        let records = [record(TEMP, Vendor::Codex, "/p/shop")];
        let fresh_id = fresh.id.to_string();
        assert_eq!(
            resolve_interrupted(
                &records,
                |_| listed.as_slice(),
                &bound(&[(TEMP, fresh_id.as_str())])
            ),
            HashMap::from([(fresh.id.clone(), SessionId::from(TEMP))])
        );
    }

    /// 审计 v0.1.4 第 13 条复审：别的会话台改了认领记录（认领、撤销），这边看文件变了就读回来交给 adopt，
    /// 不用等下次补取托管列表；没变不重读。
    #[test]
    fn claims_file_change_is_read_once_and_adopted() {
        let dir = tabs_dir("claims-stamp");
        let now = SystemTime::now();
        let mut stamp = ClaimsStamp::default();
        let mut mine = Claims::default();
        assert!(
            stamp.reload(&dir, now).unwrap().is_none(),
            "没有记录，没得读"
        );
        let ms = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let temp = SessionId::from(format!("{TEMP_PREFIX}codex-{ms}"));
        let theirs = HashMap::from([(
            temp.clone(),
            Bound {
                real: SessionId::from("real-a"),
                pid: 42,
            },
        )]);
        save_claims(&dir, std::slice::from_ref(&temp), &theirs).unwrap();
        mine.adopt(
            stamp
                .reload(&dir, now)
                .unwrap()
                .expect("别的会话台写了，要读"),
        );
        assert_eq!(mine.bound(), &theirs);
        assert!(stamp.reload(&dir, now).unwrap().is_none(), "没变不再读");
        // 别的会话台撤了那条认领。
        save_claims(&dir, std::slice::from_ref(&temp), &HashMap::new()).unwrap();
        mine.adopt(stamp.reload(&dir, now).unwrap().expect("撤了也是变了"));
        assert!(mine.bound().is_empty(), "跟着撤：{:?}", mine.bound());
        // 文件删了：读到空的。
        save_claims(&dir, std::slice::from_ref(&temp), &theirs).unwrap();
        assert!(stamp.reload(&dir, now).unwrap().is_some());
        std::fs::remove_file(dir.join(CLAIMS_FILE)).unwrap();
        assert_eq!(stamp.reload(&dir, now).unwrap(), Some(HashMap::new()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 审计 v0.1.4 第 7 条（中断记录那半）：v0.1.3 记的原生 id 对到工作账号那条带前缀的；对不上唯一一条不列。
    #[test]
    fn native_id_interrupted_by_v013_maps_to_the_prefixed_entry() {
        let work = in_work("x-1", "/p/shop");
        let records = [record("x-1", Vendor::Codex, "/p/shop")];
        let codex = entries(std::slice::from_ref(&work));
        assert_eq!(
            resolve_interrupted(&records, |_| codex.as_slice(), &HashMap::new()),
            HashMap::from([(work.id.clone(), SessionId::from("x-1"))])
        );
        // 两个工作账号同项目都有：对不上唯一一条，不列。
        let codex = entries(&[work.clone(), in_account("0deadbee", "x-1", "/p/shop")]);
        assert!(resolve_interrupted(&records, |_| codex.as_slice(), &HashMap::new()).is_empty());
        // 默认账号有这个 id：是默认账号那条（和改之前一样）。
        let codex = entries(&[session("x-1", "/p/shop", 1_000_100), work.clone()]);
        assert_eq!(
            resolve_interrupted(&records, |_| codex.as_slice(), &HashMap::new()),
            HashMap::from([(SessionId::from("x-1"), SessionId::from("x-1"))])
        );
        // 工作账号那条自己另有（新版的）记录：原生 id 的记录是默认账号那条的。
        let records = [
            record("x-1", Vendor::Codex, "/p/shop"),
            record("0badcafe:x-1", Vendor::Codex, "/p/shop"),
        ];
        assert_eq!(
            resolve_interrupted(&records, |_| codex.as_slice(), &HashMap::new()),
            HashMap::from([
                (SessionId::from("x-1"), SessionId::from("x-1")),
                (work.id.clone(), work.id.clone()),
            ])
        );
    }

    /// 第 `n` 条认领：临时 id 的新建时间在 `ms` 之前（新建在将来的记录读的时候会扔掉）。
    fn claim_at(ms: u128, n: usize) -> (SessionId, Bound) {
        (
            SessionId::from(format!("new-codex-{}", ms - 10_000 + n as u128)),
            Bound {
                real: SessionId::from(format!("real-{n}")),
                pid: 42,
            },
        )
    }

    /// 审计 v0.1.4 第 12 条：几个会话台同时各存各的认领，一条都不丢；旧记录读不出来就报错、原文件不动。
    #[test]
    fn concurrent_claim_saves_keep_every_entry_and_bad_file_is_not_overwritten() {
        let dir = tabs_dir("claims-lock");
        let ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let writers: Vec<_> = (0..8)
            .map(|w| {
                let dir = dir.clone();
                thread::spawn(move || {
                    for i in 0..20 {
                        let (temp, b) = claim_at(ms, w * 20 + i);
                        let bound = HashMap::from([(temp.clone(), b)]);
                        save_claims(&dir, &[temp], &bound).unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        let all = load_claims(&dir, SystemTime::now()).unwrap();
        assert_eq!(all.len(), 160, "丢了认领");

        std::fs::write(dir.join(CLAIMS_FILE), "{坏了").unwrap();
        let (temp, b) = claim_at(ms, 999);
        let err = save_claims(
            &dir,
            std::slice::from_ref(&temp),
            &HashMap::from([(temp.clone(), b)]),
        );
        assert!(err.is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join(CLAIMS_FILE)).unwrap(),
            "{坏了"
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    /// 审计 v0.1.3 第 04 条：Kimi 带不进话、OpenCode 1.x 只填进输入框，不能算进「已在 N 个新标签页接着做」。
    #[test]
    fn resume_reports_what_each_vendor_did_with_the_message() {
        let dir = PathBuf::from(format!("/tmp/htr-{}-deliver", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let w = worker_in(&dir, false);
        let with = |vendor: Vendor, id: &str| {
            let mut e = pi_entry(id, &dir);
            e.session.vendor = vendor;
            e
        };
        let msg = w
            .resume_interrupted(&[
                pi_entry("p1", &dir),
                with(Vendor::Kimi, "session_k1"),
                // 测试里没有 OpenCode 的库：认不出是 2.x，按 1.x 报要按回车。
                with(Vendor::OpenCode, "ses_e113ed23fffeV2onlyAbCdEfGh"),
            ])
            .unwrap();
        assert_eq!(
            msg,
            "开了 3 个新标签页：1 个接着做了，1 个话已填好、要按回车（OpenCode），\
             1 个带不进话、要自己输入（Kimi Code）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 撞额度到点自动继续：还托管着的先结束、等它从守护进程里没了，再带「继续」重新登记、开标签页，记下这次上限。
    #[test]
    fn auto_continue_replaces_the_stuck_process_and_records_it() {
        let (w, dir) = test_worker("auto", false);
        let spec = SpawnSpec {
            id: SessionId::from("p1"),
            vendor: Vendor::Pi,
            project: "demo".to_owned(),
            cwd: dir.clone(),
            command: "旧的进程".to_owned(),
        };
        assert_eq!(
            host::client::request(&dir, &Request::Spawn(spec)).unwrap(),
            Response::Ok
        );
        let e = pi_entry("p1", &dir);
        let msg = w
            .auto_continue(&e, &SessionId::from("p1"), 1_795_410_060)
            .unwrap();
        assert!(msg.contains("已自动继续"), "{msg}");
        // 守护进程里还是一条 p1：换成了带「继续」的新登记，没有两个。
        assert_eq!(hosted_ids(&w), vec![SessionId::from("p1")]);
        let notes = std::fs::read_to_string(dir.join("index/annotations.json")).unwrap();
        assert!(notes.contains(r#""continued": 1795410060"#), "{notes}");

        let _ = std::fs::remove_dir_all(&dir);

        // 不托管的（闲置收回过）直接开；标签页没开成：报错、撤掉登记、不记「继续过」，下次还能再试。
        let (w, dir) = test_worker("auto-fail", true);
        assert!(
            w.auto_continue(&pi_entry("p1", &dir), &SessionId::from("p1"), 1)
                .is_err()
        );
        assert_eq!(hosted_ids(&w), Vec::<SessionId>::new());
        let notes = std::fs::read_to_string(dir.join("index/annotations.json")).unwrap_or_default();
        assert!(!notes.contains("continued"), "{notes}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_report_wording() {
        use Delivery::{Manual, Sent, Typed};
        let fail = vec!["「修登录」开不了".to_owned()];
        assert_eq!(
            resume_report(&[(Vendor::Claude, Sent)], &[]),
            "已在 1 个新标签页接着做"
        );
        assert_eq!(
            resume_report(&[(Vendor::Claude, Sent)], &fail),
            "接着做了 1 个，1 个没打开：「修登录」开不了"
        );
        assert_eq!(
            resume_report(&[(Vendor::Kimi, Manual), (Vendor::Kimi, Manual)], &[]),
            "开了 2 个新标签页：2 个带不进话、要自己输入（Kimi Code）"
        );
        assert_eq!(
            resume_report(&[(Vendor::OpenCode, Typed)], &fail),
            "开了 1 个新标签页：1 个话已填好、要按回车（OpenCode），1 个没打开：「修登录」开不了"
        );
        assert_eq!(
            resume_report(&[], &fail),
            "接着做了 0 个，1 个没打开：「修登录」开不了"
        );
    }
}
