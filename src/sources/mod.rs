//! 各家适配器：扫描、元数据、预览、恢复/新建命令、回收站。对外接口：`trait Source`。
//!
//! 全部只读厂商数据；唯一会移动厂商文件的是 [`Source::trash`]（移进会话台回收站，可恢复），
//! 唯一会写进厂商目录的是主人明确要的 [`Source::put_back`]（把只剩备份的 Claude 对话放回原处，不覆盖）。
//! 扫描只读每个会话文件的头尾（见 `window`），不整文件解析。
//!
//! 「消息数」统一指用户发言条数。扫描时文件头尾装得下整个文件、或厂商自己记了计数（Codex 状态库）
//! 才是精确值，否则是头尾里看到的下限；要精确值调 [`Source::count_since`]（第一次读完整个文件、只解析候选行，
//! 之后文件只是变长就只读新增部分），由 index 在后台补齐并缓存。

mod account;
pub mod antigravity;
pub mod backup;
pub mod claude;
pub mod codex;
pub mod gemini;
pub mod hermes;
mod jsonl;
pub mod kimi;
pub mod limit;
pub mod live;
pub mod moment;
pub mod opencode;
pub mod pi;
mod sqlite;
mod time;
mod transcript;
mod trash;
mod window;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use crate::core::{Account, Config, EnvAssign, Session, Vendor, VendorDirs};

pub use account::{AccountSource, DEFAULT_NAME, accounts, data_root, setting_dir};
pub use antigravity::AntigravitySource;
pub use backup::{Backups, Saved};
pub use claude::ClaudeSource;
pub use codex::CodexSource;
pub use gemini::GeminiSource;
pub use hermes::HermesSource;
pub use kimi::KimiSource;
pub use limit::LimitHit;
pub use moment::{Moment, PlanStep, StepStatus};
pub use opencode::OpenCodeSource;
pub use pi::PiSource;
pub use transcript::{Part, TRANSCRIPT_MAX_BYTES, Transcript};
pub use trash::{Trash, TrashEntry, TrashedFile};

/// 带话接着聊时，那句话怎么到 CLI 里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// CLI 起来就把话发出去了。
    Sent,
    /// 话填进了输入框，要在标签页里按回车才发出去。
    Typed,
    /// 带不进话：只恢复了对话，话要自己输入。
    Manual,
}

/// 一家厂商的会话来源。
pub trait Source: Send + Sync {
    fn vendor(&self) -> Vendor;

    /// 哪个账号的（[`Account::key`]），默认账号是空串。只有 [`AccountSource`] 包过的才不是空串。
    fn account(&self) -> &str {
        ""
    }

    /// 解析版本，见 [`parse_version`]。测试里的假来源可以改它。
    fn parse_version(&self) -> u32 {
        parse_version(self.vendor())
    }

    /// 扫描全部对话。单个文件读不了不影响其余，记进 `problems`；目录不存在返回空。
    fn scan(&self) -> Scan;

    /// 最后 `n` 条消息（用户和 AI 的文字，按时间先后）。
    fn load_preview(&self, session: &Session, n: usize) -> io::Result<Vec<PreviewMessage>>;

    /// 整段对话（只读滚动视图用）：消息、工具调用和返回按时间先后，每块截短；
    /// 文件太大只读最后 [`TRANSCRIPT_MAX_BYTES`]（[`Transcript::truncated`]）。
    fn load_transcript(&self, session: &Session) -> io::Result<Transcript>;

    /// 「此刻」：在跑的工具、停下时的一句话、用时、计划、失败（见 [`moment`]）。只读文件尾部或最后几行。
    /// 默认从整段对话推（只有消息和工具）；jsonl 几家只读尾部，并补上厂商自己记的轮次事件。
    fn load_moment(&self, session: &Session) -> io::Result<Moment> {
        Ok(moment::from_parts(self.load_transcript(session)?.parts))
    }

    /// 「此刻」重读的依据：[`Source::load_moment`] 读的这些文件大小或修改时间变了才重读。
    /// 默认是会话文件和它旁边的 `-wal`；对话都在一个库里的几家给库和它的 WAL（见 [`with_wal`]）。
    fn moment_files(&self, session: &Session) -> Vec<PathBuf> {
        with_wal(&session.path)
    }

    /// 精确的用户发言条数（会读完整个文件，按需调用）。
    fn count_messages(&self, session: &Session) -> io::Result<usize>;

    /// 精确计数，接着上次数到的地方（`prev`，上次返回的 [`Counted::progress`]）往下数：
    /// 文件只是变长时只读新增部分；变短、开头或接缝处被改写、解析版本变了就整份重数。结果和整份重数一致。
    /// 默认实现不支持增量：每次整份数，不给进度。
    fn count_since(&self, session: &Session, _prev: Option<Progress>) -> io::Result<Counted> {
        Ok(Counted {
            users: self.count_messages(session)?,
            progress: None,
            appended: None,
        })
    }

    /// 接着这条对话聊的命令，在对话的项目目录里跑。
    fn resume_cmd(&self, session: &Session) -> Command;

    /// 接着这条对话聊，并把 `message` 当第一句话发进去（被中断后一键接着做）。
    /// Claude `claude --resume <id> <话>`、Codex `codex resume <id> <话>` 都是交互模式带首句话（`--help` 里的
    /// `[prompt]`/`[PROMPT]` 位置参数）；pi 不同，见 [`PiSource`]。
    fn resume_with_message_cmd(&self, session: &Session, message: &str) -> Command {
        let mut cmd = self.resume_cmd(session);
        cmd.args.push(message.to_owned());
        cmd
    }

    /// [`Source::resume_with_message_cmd`] 带的那句话到没到 CLI（R 续做按它报结果）。默认 CLI 起来就发出去了。
    fn message_delivery(&self, _session: &Session) -> Delivery {
        Delivery::Sent
    }

    /// 在 `cwd` 新开一条对话的命令。
    fn new_cmd(&self, cwd: &Path) -> Command;

    /// 这条对话是什么时候生成的（会话台认回它新建的对话用，见 [`created_at`]）。新建时定不了 id 的那几家
    /// 从 id 或会话文件看得出就给；只看这条对话自己的 id 和文件，不看来源的目录。默认看不出，返回 `None`。
    fn created_at(&self, _session: &Session) -> Option<SystemTime> {
        None
    }

    /// 删除：移进会话台回收站（或只隐藏），可用 [`Source::restore`] 恢复。
    fn trash(&self, session: &Session) -> io::Result<TrashEntry>;

    fn restore(&self, entry: &TrashEntry) -> io::Result<()>;

    /// 要监听变化的目录（index 用 FSEvents 监听，递归）。
    fn watch_roots(&self) -> Vec<PathBuf>;

    /// 监听目录里这个路径变了会不会影响对话列表（厂商目录里还有日志、缓存等无关文件）。
    /// 按文件组织的来源对会话文件返回 `true` 时，index 只重读这个文件；别的路径（目录）整家重列。
    fn affects(&self, _path: &Path) -> bool {
        true
    }

    /// 增量扫描用：一条对话对应一个会话文件的来源，列出全部会话文件（只列不读）。
    /// 不按文件组织的来源（Codex 以它自己的状态库为主）返回 `None`，index 每次整家调 [`Source::scan`]。
    fn session_files(&self) -> Option<io::Result<Vec<PathBuf>>> {
        None
    }

    /// 增量扫描用：只读这些会话文件（并行），结果和输入一一对应；`Ok(None)` 表示这个文件不是一条可列出的对话。
    /// 与 [`Source::scan`] 读单个文件的结果一致。
    fn read_files(&self, paths: &[PathBuf]) -> Vec<io::Result<Option<Session>>> {
        paths
            .iter()
            .map(|_| Err(io::ErrorKind::Unsupported.into()))
            .collect()
    }

    /// 增量扫描用：会话文件 `gone` 没了时还能读的替身（Claude：会话台给它留的备份），交给 [`Source::read_files`] 读。
    /// 默认没有。
    fn stand_in(&self, _gone: &Path) -> Option<PathBuf> {
        None
    }

    /// 给这条对话做备份或更新备份（只 Claude 有，见 [`backup`]）；`check` 为真时备份没变也完整解一遍确认没坏。
    fn back_up(&self, _session: &Session, _check: bool) -> io::Result<Saved> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "这家不做备份"))
    }

    /// 把只剩备份的对话放回厂商目录原处（主人明确要的才调），返回放回的路径；原处已有文件不覆盖。
    fn put_back(&self, _session: &Session) -> io::Result<PathBuf> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "这家没有备份可放回",
        ))
    }
}

/// 每家适配器的解析版本，见 [`crate::core::VendorInfo::parse_version`]（在 `core::vendor` 的表里改）。
pub const fn parse_version(vendor: Vendor) -> u32 {
    vendor.info().parse_version
}

/// 一个文件和它旁边的 SQLite `-wal`（写进 WAL 时主库文件不动）。
pub(crate) fn with_wal(path: &Path) -> Vec<PathBuf> {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    vec![path.to_path_buf(), PathBuf::from(wal)]
}

/// 按文件组织的来源的整家扫描：[`Source::session_files`] 加 [`Source::read_files`]。
fn scan_by_files(src: &dyn Source) -> Scan {
    let mut scan = Scan::default();
    let files = match src.session_files() {
        Some(Ok(f)) => f,
        Some(Err(e)) => {
            scan.problems.push(e.to_string());
            return scan;
        }
        None => return scan,
    };
    for (path, res) in files.iter().zip(src.read_files(&files)) {
        match res {
            Ok(Some(s)) => scan.sessions.push(s),
            Ok(None) => {}
            Err(e) => scan.problems.push(format!("{}: {e}", path.display())),
        }
    }
    scan
}

/// 精确计数数到哪了：会话文件前 `pos` 字节（到一个换行为止）里有 `users` 条用户发言。
/// `digest` 是解析版本、开头一段和 `pos` 之前一段的摘要，下次用它认文件是不是只在后面追加了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub pos: u64,
    pub users: usize,
    pub digest: u64,
}

/// [`Source::count_since`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counted {
    /// 精确的用户发言条数（含最后一行还没写完换行的）。
    pub users: usize,
    /// 这次数到哪了（只算完整的行），存起来下次接着数；来源不支持增量时为 `None`。
    pub progress: Option<Progress>,
    /// 接着 `prev` 数时，新增的完整行里的消息（按时间先后，已截短），给全文索引追加；
    /// 整份重数、或新增超过 8MB 时为 `None`，正文要整份重取。
    pub appended: Option<Vec<PreviewMessage>>,
}

/// 一次扫描的结果。
#[derive(Debug, Default)]
pub struct Scan {
    pub sessions: Vec<Session>,
    /// 读不了的文件或数据库，「路径: 原因」，给 `ai doctor` 和日志看。
    pub problems: Vec<String>,
    /// 库里的对话（Hermes）给的正文修订号：厂商自己的 id（不带账号前缀）→ 修订号。原地改了正文（脱敏、改写）
    /// 不动最后活动时间和发言数，索引靠它知道正文变了。没有修订号的不填。
    pub revisions: HashMap<String, u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    User,
    Assistant,
}

/// 预览里的一条消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewMessage {
    pub role: Role,
    pub text: String,
    pub at: Option<SystemTime>,
}

/// 要执行的命令：程序、参数、工作目录。由 host 用登录 shell 启动。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

impl Command {
    fn new(program: &str, args: &[&str], cwd: &Path) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            cwd: cwd.to_path_buf(),
        }
    }

    /// 给 `$SHELL -lc` 用的一行命令，参数按需加单引号。
    pub fn shell_line(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// 这家改过数据目录时，命令前面要带的 `CODEX_HOME='…' `（没改过是空串）：
/// 守护进程起 CLI 用的环境来自第一次拉起它的终端，复制出去的命令也可能贴进别的终端，都不一定有这些变量。
/// 这家每个改目录的变量都给定（[`VendorDirs::env_for`]）：要清的先 `env -u`，再设要设的，
/// 如 `env -u OPENCODE_DB XDG_DATA_HOME='…' `。
pub fn env_prefix(dirs: &VendorDirs, vendor: Vendor, home: &Path) -> String {
    env_line(&VendorDirs::env_for(vendor, dirs.dir(vendor), home))
}

/// 这个账号的命令前面要带的目录变量（默认位置的账号是空串），见 [`env_prefix`]。
pub fn account_env_prefix(account: &Account, home: &Path) -> String {
    env_line(&account.env(home))
}

/// 一家有几个账号时默认账号的命令前缀：这家所有改目录的变量都清掉（`env -u OPENCODE_DB -u XDG_DATA_HOME `），
/// 免得守护进程或登录 shell 里设着的哪一个把它带到别的账号。
pub fn default_account_env_prefix(vendor: Vendor) -> String {
    unset_line(vendor.info().dir_vars)
}

fn env_line(vars: &[EnvAssign]) -> String {
    let unset: Vec<&str> = vars
        .iter()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| *k)
        .collect();
    let set: String = vars
        .iter()
        .filter_map(|(k, v)| {
            let v = v.as_ref()?;
            Some(format!("{k}={} ", shell_quote(&v.to_string_lossy())))
        })
        .collect();
    format!("{}{set}", unset_line(&unset))
}

/// `env -u A -u B `；没有变量是空串。
fn unset_line(vars: &[&str]) -> String {
    if vars.is_empty() {
        return String::new();
    }
    let flags: Vec<String> = vars.iter().map(|v| format!("-u {v}")).collect();
    format!("env {} ", flags.join(" "))
}

fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
    if plain {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// 列表里自动收起来的组。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FoldGroup {
    /// agentq 的工作树（门检查、任务 worktree）。
    Agentq,
    /// 家目录、临时目录里的零散对话。
    Loose,
}

/// 这条对话该不该收进折叠组。按 Claude 的目录编码（非字母数字都换成 `-`）匹配：
/// 含 `agentq-gate-` 或以 `-wt` 结尾的是 agentq 门工作树，含 `-claude-worktrees-` 的是任务工作树；
/// 家目录本身和临时目录算零散对话。
pub fn fold_group(cwd: &Path, home: &Path) -> Option<FoldGroup> {
    let encoded = encode_path(cwd);
    if encoded.contains("agentq-gate-")
        || encoded.ends_with("-wt")
        || encoded.contains("-claude-worktrees-")
    {
        return Some(FoldGroup::Agentq);
    }
    let loose = cwd == home
        || cwd.as_os_str().is_empty()
        || [
            "/tmp",
            "/private/tmp",
            "/private/var/folders",
            "/var/folders",
        ]
        .iter()
        .any(|t| cwd.starts_with(t));
    loose.then_some(FoldGroup::Loose)
}

/// Claude 的项目目录编码：非 ASCII 字母数字都换成 `-`。
pub(crate) fn encode_path(p: &Path) -> String {
    p.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Claude、pi 的布局 `root/<项目目录>/<id>.jsonl` 里，这个路径变了会不会影响列表：
/// 项目目录本身（建、删）和其中的会话文件算；会话的附属目录（子代理、工具输出）不算。
fn affects_subdir_layout(root: &Path, path: &Path) -> bool {
    match path.strip_prefix(root).map(|r| r.components().count()) {
        Ok(0 | 1) => true,
        Ok(2) => path.extension().is_some_and(|e| e == "jsonl"),
        _ => false,
    }
}

/// 本机各家全部账号的来源（`home` 是用户家目录；账号怎么认见 [`local_accounts`]）。顺序和 [`Vendor::ALL`] 一致，
/// 每家主账号在前。
pub fn default_sources(home: &Path, trash: Trash) -> Vec<Box<dyn Source>> {
    sources_for(&local_accounts(home), trash)
}

/// 本机的账号：按当前进程的环境变量、工作目录和设置文件里手动加的账号，见 [`accounts`]。
/// 设置文件读不了就只认环境变量和自动发现的。
pub fn local_accounts(home: &Path) -> Vec<Account> {
    let settings = Config::default_path()
        .and_then(|p| Config::load(&p).ok())
        .map(|c| c.accounts)
        .unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_else(|_| home.to_path_buf());
    accounts(home, &cwd, |k| std::env::var_os(k), &settings)
}

/// 每个账号一个来源；不再支持的家（[`Vendor::listed`]）没有来源。
pub fn sources_for(accounts: &[Account], trash: Trash) -> Vec<Box<dyn Source>> {
    accounts
        .iter()
        .filter_map(|a| -> Option<Box<dyn Source>> {
            let dir = a.dir.as_path();
            let backups = backups_beside(&trash, a);
            let inner = adapter(a.vendor, dir, trash.clone().for_account(dir), backups)?;
            Some(if a.is_default() {
                inner
            } else {
                Box::new(AccountSource::new(inner, a))
            })
        })
        .collect()
}

/// 账号 `a` 的备份目录：在回收站旁边（都在会话台数据目录下，见 [`Backups::for_account`]）。只 Claude 有。
fn backups_beside(trash: &Trash, a: &Account) -> Option<Backups> {
    let data = trash
        .root()
        .parent()
        .filter(|p| !p.as_os_str().is_empty())?;
    (a.vendor == Vendor::Claude).then(|| Backups::for_account(data, &a.key))
}

/// 数据目录是 `dir` 的这家适配器（不包账号）；不再支持的家没有。
fn adapter(
    vendor: Vendor,
    dir: &Path,
    trash: Trash,
    backups: Option<Backups>,
) -> Option<Box<dyn Source>> {
    let root = data_root(vendor, dir);
    Some(match vendor {
        Vendor::Claude => {
            let src = ClaudeSource::new(root, trash);
            Box::new(match backups {
                Some(b) => src.with_backups(b),
                None => src,
            })
        }
        Vendor::Codex => Box::new(CodexSource::new(dir, trash)),
        Vendor::Pi => Box::new(PiSource::new(root, trash)),
        Vendor::Hermes => Box::new(HermesSource::new(dir, trash)),
        Vendor::Kimi => Box::new(KimiSource::new(root, trash)),
        Vendor::Gemini => Box::new(GeminiSource::new(dir, trash)),
        Vendor::OpenCode => Box::new(OpenCodeSource::new(dir, trash)),
        Vendor::Qwen => return None,
        Vendor::Antigravity => Box::new(AntigravitySource::new(root, trash)),
    })
}

/// 对话 `s` 是什么时候生成的：问它那家的适配器（[`Source::created_at`]）。各家只看这条对话自己的 id 和文件，
/// 和账号目录、回收站无关，所以问的是一份不指向任何目录的适配器，调用方（认领）不用带着来源。
pub fn created_at(s: &Session) -> Option<SystemTime> {
    static BARE: OnceLock<Vec<Box<dyn Source>>> = OnceLock::new();
    BARE.get_or_init(|| {
        Vendor::ALL
            .into_iter()
            .filter_map(|v| adapter(v, Path::new(""), Trash::new(""), None))
            .collect()
    })
    .iter()
    .find(|src| src.vendor() == s.vendor)?
    .created_at(s)
}

/// 这条对话的来源：同一家、同一个账号的那个。
pub fn source_of<'a>(sources: &'a [Box<dyn Source>], s: &Session) -> Option<&'a dyn Source> {
    sources
        .iter()
        .find(|src| src.vendor() == s.vendor && src.account() == s.account)
        .map(|src| src.as_ref())
}

/// 有文件的厂商共用的「删除 = 移进回收站」。
fn trash_files(trash: &Trash, session: &Session, extra: &[PathBuf]) -> io::Result<TrashEntry> {
    let mut files = vec![session.path.clone()];
    files.extend_from_slice(extra);
    trash.move_in(session.vendor, session.id.as_str(), &files)
}

/// 文件 `mtime`，取不到时用 1970。
fn mtime_or_epoch(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// 列出 `dir` 下一层子目录里的 `*.jsonl`（Claude、pi 的布局）。目录不存在返回空。
fn jsonl_in_subdirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", dir.display()))),
    };
    let mut out = Vec::new();
    for sub in rd.flatten() {
        if !sub.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(sub.path()) else {
            continue;
        };
        out.extend(
            files
                .flatten()
                .map(|f| f.path())
                .filter(|p| p.extension().is_some_and(|e| e == "jsonl") && p.is_file()),
        );
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_line_quotes_only_when_needed() {
        let c = Command::new("claude", &["--resume", "abc-123"], Path::new("/p"));
        assert_eq!(c.shell_line(), "claude --resume abc-123");
        let c = Command::new("pi", &["--session", "it's x"], Path::new("/p"));
        assert_eq!(c.shell_line(), r"pi --session 'it'\''s x'");
    }

    #[test]
    fn subdir_layout_ignores_side_files() {
        let root = Path::new("/h/.claude/projects");
        let a = |p: &str| affects_subdir_layout(root, Path::new(p));
        assert!(a("/h/.claude/projects/-p/s1.jsonl"));
        assert!(a("/h/.claude/projects/-p"));
        assert!(!a("/h/.claude/projects/-p/s1/subagents/x.jsonl"));
        assert!(!a("/h/.claude/projects/-p/s1.jsonl.tmp"));
        assert!(!a("/h/.codex/x.jsonl"));
    }

    #[test]
    fn fold_groups() {
        let home = Path::new("/Users/me");
        let g = |p: &str| fold_group(Path::new(p), home);
        assert_eq!(
            g("/Users/me/.agentq/gate/1106-132757-x/wt"),
            Some(FoldGroup::Agentq)
        );
        assert_eq!(g("/Users/me/agentq-gate-1-wt"), Some(FoldGroup::Agentq));
        assert_eq!(
            g("/Users/me/projects/a/.claude/worktrees/1120-x"),
            Some(FoldGroup::Agentq)
        );
        assert_eq!(g("/Users/me"), Some(FoldGroup::Loose));
        assert_eq!(g("/private/tmp/scratch"), Some(FoldGroup::Loose));
        assert_eq!(g("/Users/me/projects/huitai"), None);
        assert_eq!(g("/Users/me/projects"), None);
    }
}
