//! Kimi Code（MoonshotAI/kimi-code，命令 `kimi`）：`$KIMI_CODE_HOME/sessions/<工作区>/<会话 id>/`（默认 `~/.kimi-code`）。
//! 每条对话一个目录：`state.json` 给 id、cwd、标题、创建和更新时间；正文在主代理的 `agents/main/wire.jsonl`。
//! 字段按 2.7.2 源码（tag `@moonshot-ai/kimi-code@2.7.2`，commit 56145f8）：`state.json` 见 agent-core-v2
//! `session/sessionMetadata/`，正文的取法照它自己的全文搜索 kap-server `search/wireExtract.ts`；本机 2.1.1 写的数据
//! 核对过是同一套结构。工作区目录名 `wd_<目录名>_<哈希>`，会话 id `session_<uuid>`，两者都看不出生成时间。
//!
//! 一条对话有两个会话文件（[`Source::session_files`] 两个都列）：标题只写 `state.json`、正文只写 `wire.jsonl`，
//! index 按文件的 mtime 和大小判断要不要重读，任何一个变了都要重读这条。
//!
//! 撤销（`/undo`）不删正文里的行，只追加一条 `context.undo`：列表、计数、预览、整段、「此刻」都按
//! [`Undo`] 重放过的样子来，撤掉的那几轮不显示、不计数、不进全文搜索。

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use super::jsonl::{self, str_field};
use super::moment::Folder;
use super::time::from_millis;
use super::window::{self, PREVIEW_MAX_BYTES, Window};
use super::{
    Command, Counted, Delivery, Moment, Part, PreviewMessage, Progress, Role, Scan, Source,
    Transcript, Trash, TrashEntry, moment, transcript,
};
use crate::core::{Session, SessionId, Vendor};

const STATE: &str = "state.json";
/// 主代理的正文，相对会话目录。子代理在 `agents/<别的 id>/`，不算这条对话的正文。
const WIRE: [&str; 3] = ["agents", "main", "wire.jsonl"];
/// 2.7.2 `isUntitled`：还没起标题时写的占位名。
const UNTITLED: &str = "New Session";
/// 用户消息的行都带它，精确计数只解析这些行。
const USER_MARKER: &[u8] = br#""context.append_message""#;
/// 撤销那一行带它。
const UNDO_MARKER: &[u8] = br#""context.undo""#;
/// 撤销重放要看的几种行（`"context.` 之后的部分）：消息、撤销、压缩、清空。别的行（AI 的输出、工具）不用解析。
const REPLAY_TYPES: [&[u8]; 4] = [
    b"append_message\"",
    b"undo\"",
    b"apply_compaction\"",
    b"clear\"",
];

pub struct KimiSource {
    root: PathBuf,
    trash: Trash,
}

impl KimiSource {
    /// `root` 是 `~/.kimi-code/sessions`。
    pub fn new(root: impl Into<PathBuf>, trash: Trash) -> Self {
        Self {
            root: root.into(),
            trash,
        }
    }
}

impl Source for KimiSource {
    fn vendor(&self) -> Vendor {
        Vendor::Kimi
    }

    /// 每个会话目录读一次（两个会话文件读出的是同一条）。
    fn scan(&self) -> Scan {
        let mut scan = Scan::default();
        let files = match list_files(&self.root) {
            Ok(f) => f,
            Err(e) => {
                scan.problems.push(e.to_string());
                return scan;
            }
        };
        let mut dirs: Vec<PathBuf> = files
            .iter()
            .filter_map(|f| session_dir(f))
            .map(Path::to_path_buf)
            .collect();
        dirs.dedup();
        for (dir, res) in dirs.iter().zip(jsonl::par_map(&dirs, read_session)) {
            match res {
                Ok(Some(s)) => scan.sessions.push(s),
                Ok(None) => {}
                Err(e) => scan.problems.push(format!("{}: {e}", dir.display())),
            }
        }
        scan
    }

    /// 同 [`jsonl::last_messages`]，读到的那段先按撤销重放。
    fn load_preview(&self, session: &Session, n: usize) -> io::Result<Vec<PreviewMessage>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut bytes = 256 * 1024;
        loop {
            let (buf, from_start) = window::read_tail_lines(&session.path, bytes)?;
            let mut out: Vec<PreviewMessage> = kept(window::lines(&buf), from_start)
                .into_iter()
                .rev()
                .filter_map(|l| jsonl::parse(l).as_ref().and_then(message))
                .take(n)
                .collect();
            if out.len() == n || from_start || bytes >= PREVIEW_MAX_BYTES {
                out.reverse();
                return Ok(out.into_iter().map(jsonl::clip).collect());
            }
            bytes = (bytes * 4).min(PREVIEW_MAX_BYTES);
        }
    }

    /// 同 [`transcript::read`]（最后 [`transcript::TRANSCRIPT_MAX_BYTES`]），读到的那段先按撤销重放。
    fn load_transcript(&self, session: &Session) -> io::Result<Transcript> {
        let (buf, from_start) =
            window::read_tail_lines(&session.path, transcript::TRANSCRIPT_MAX_BYTES)?;
        Ok(Transcript {
            parts: kept(window::lines(&buf), from_start)
                .into_iter()
                .filter_map(jsonl::parse)
                .flat_map(|v| parts(&v))
                .map(transcript::clip)
                .collect(),
            truncated: !from_start,
        })
    }

    /// 同 [`moment::read_jsonl`]，读到的那段先按撤销重放。
    fn load_moment(&self, session: &Session) -> io::Result<Moment> {
        let (buf, from_start) = window::read_tail_lines(&session.path, moment::MOMENT_TAIL_BYTES)?;
        let mut f = Folder::default();
        for v in kept(window::lines(&buf), from_start)
            .into_iter()
            .filter_map(jsonl::parse)
        {
            for e in moment::parts(parts(&v)) {
                f.push(e);
            }
        }
        Ok(f.finish())
    }

    fn count_messages(&self, session: &Session) -> io::Result<usize> {
        self.count_since(session, None).map(|c| c.users)
    }

    /// 接着数的新增那段里没有撤销时照 [`jsonl::count_since`] 接着数（撤销只撤它之前的，新增的话撤不到前面）；
    /// 有撤销、或 `prev` 对不上，从头按撤销重放着数。
    fn count_since(&self, session: &Session, prev: Option<Progress>) -> io::Result<Counted> {
        let salt = self.parse_version();
        let mut file = File::open(&session.path)?;
        let len = file.metadata()?.len();
        let resume = prev.filter(|p| {
            p.pos <= len && window::digest(&mut file, p.pos, salt).is_ok_and(|d| d == p.digest)
        });
        if let Some(p) = resume {
            let mut undone = false;
            window::for_each_line_from(&mut file, p.pos, |line, _| {
                undone |= find(line, UNDO_MARKER).is_some();
            })?;
            if !undone {
                return jsonl::count_since(&session.path, USER_MARKER, salt, message, prev);
            }
        }
        let replay = Replay::of_file(&mut file)?;
        Ok(Counted {
            users: replay.users + replay.partial,
            progress: Some(Progress {
                pos: replay.end,
                users: replay.users,
                digest: window::digest(&mut file, replay.end, salt)?,
            }),
            appended: None,
        })
    }

    /// 2.7.2 `-S <id>` 只认在对话自己的目录里恢复（换了目录报 created under a different directory），
    /// 命令在对话的项目目录里跑。
    fn resume_cmd(&self, session: &Session) -> Command {
        Command::new("kimi", &["-S", session.id.as_str()], &session.cwd)
    }

    /// Kimi Code 交互模式带不了首句话：位置参数会被当成子命令名报 unknown command，`-p` 是跑一句就退出的
    /// 非交互模式（2.7.2 `cli/commands.ts`）。只恢复对话，话由用户自己发。
    fn resume_with_message_cmd(&self, session: &Session, _message: &str) -> Command {
        self.resume_cmd(session)
    }

    fn message_delivery(&self, _session: &Session) -> Delivery {
        Delivery::Manual
    }

    fn new_cmd(&self, cwd: &Path) -> Command {
        Command::new("kimi", &[], cwd)
    }

    /// id 看不出，读它的 `state.json`（[`created_at`]）。
    fn created_at(&self, session: &Session) -> Option<SystemTime> {
        created_at(&session.path)
    }

    /// 整个会话目录移进回收站（state.json、正文、子代理、文件历史一起）。
    fn trash(&self, session: &Session) -> io::Result<TrashEntry> {
        let dir = session_dir(&session.path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} 不在会话目录里", session.path.display()),
            )
        })?;
        self.trash
            .move_in(Vendor::Kimi, session.id.as_str(), &[dir.to_path_buf()])
    }

    fn restore(&self, entry: &TrashEntry) -> io::Result<()> {
        self.trash.restore(entry)
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    /// 工作区、会话目录本身（建、删）和两个会话文件算；Kimi 自己的索引缓存（`.index-cache`、`.index-dirty`，
    /// 每发一句都写）、日志、子代理、文件历史不算。
    fn affects(&self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
        if parts.first().is_some_and(|w| w.starts_with('.')) {
            return false;
        }
        match parts.as_slice() {
            [] | [_] | [_, _] => true,
            [_, _, name] => *name == STATE,
            [_, _, a, b, c] => [*a, *b, *c] == WIRE,
            _ => false,
        }
    }

    fn session_files(&self) -> Option<io::Result<Vec<PathBuf>>> {
        Some(list_files(&self.root))
    }

    fn read_files(&self, paths: &[PathBuf]) -> Vec<io::Result<Option<Session>>> {
        jsonl::par_map(paths, |p| match session_dir(p) {
            Some(dir) => read_session(dir),
            None => Ok(None),
        })
    }
}

/// 每个会话目录的 `state.json` 和主代理正文（在的才列）。根目录不存在返回空。
fn list_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", root.display()))),
    };
    let mut out = Vec::new();
    for ws in rd.flatten() {
        let hidden = ws.file_name().to_string_lossy().starts_with('.');
        if hidden || !ws.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(sessions) = std::fs::read_dir(ws.path()) else {
            continue;
        };
        for dir in sessions.flatten().map(|d| d.path()) {
            out.extend(
                [dir.join(STATE), wire_path(&dir)]
                    .into_iter()
                    .filter(|f| f.is_file()),
            );
        }
    }
    out.sort();
    Ok(out)
}

fn wire_path(dir: &Path) -> PathBuf {
    WIRE.iter().fold(dir.to_path_buf(), |p, c| p.join(c))
}

/// 会话文件（`state.json` 或主代理正文）所在的会话目录。
fn session_dir(file: &Path) -> Option<&Path> {
    if file.file_name().is_some_and(|n| n == STATE) {
        return file.parent();
    }
    let dir = file.ancestors().nth(WIRE.len())?;
    (wire_path(dir) == file).then_some(dir)
}

/// 读一条对话：`state.json` 和正文都要在，用户一句话没说过（刚打开就退出、说过的全撤销了）的不列。
/// 大文件只读头尾；头尾里一句用户的话都没有时（开头一大段注入、结尾一大段输出），或尾段的撤销撤过了尾段开头时，
/// 从头按撤销重放着找一遍。
fn read_session(dir: &Path) -> io::Result<Option<Session>> {
    let state = match std::fs::read(dir.join(STATE)) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let state: Value = serde_json::from_slice(&state)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{STATE}: {e}")))?;
    let wire = wire_path(dir);
    let w = match Window::read(&wire) {
        Ok(w) => w,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let head: Vec<Value> = kept(w.head_lines(), true)
        .into_iter()
        .filter_map(jsonl::parse)
        .collect();
    let tail_own: Vec<Value>;
    // 尾段里的撤销撤到了尾段前面：头段里的首问、头尾数出的条数都可能是撤掉的，要从头重放。
    let mut crossed = false;
    let tail: &[Value] = if w.whole {
        &head
    } else {
        let (lines, c) = kept_crossing(w.tail_lines(), false);
        crossed = c;
        tail_own = lines.into_iter().filter_map(jsonl::parse).collect();
        &tail_own
    };

    let head_msgs: Vec<PreviewMessage> = head.iter().filter_map(message).collect();
    let users = |vs: &[Value]| {
        vs.iter()
            .filter_map(message)
            .filter(|m| m.role == Role::User)
            .count()
    };
    let mut first_user = head_msgs
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| m.text.clone());
    let mut message_count = if w.whole {
        users(&head)
    } else {
        users(&head) + users(tail)
    };
    if (message_count == 0 || crossed) && !w.whole {
        let replay = Replay::of_file(&mut File::open(&wire)?)?;
        message_count = replay.users + replay.partial;
        first_user = replay.first_user;
    }
    if message_count == 0 {
        return Ok(None);
    }

    let title = str_field(&state, "title")
        .map(str::trim)
        .filter(|t| !t.is_empty() && *t != UNTITLED)
        .map(str::to_owned)
        .or(first_user)
        .map(|t| jsonl::one_line(&t, 80));
    let id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // 2.7.2 `normalizeSessionMeta`：老版本把目录记在 `workDir`。
    let cwd = ["cwd", "workDir"]
        .iter()
        .find_map(|k| str_field(&state, k).filter(|s| !s.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_default();
    // 发话时会刷新 updatedAt，正文每条记录都带时间：取更晚的。
    let last_active = [
        tail.iter().rev().find_map(|v| v.get("time").and_then(at)),
        state.get("updatedAt").and_then(at),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(w.mtime);

    Ok(Some(Session {
        vendor: Vendor::Kimi,
        id: SessionId::from(id),
        cwd,
        path: wire,
        title,
        git_branch: None,
        last_active,
        message_count,
        // `kimi -p` 跑的对话存得和交互的一样（2.7.2 `run-v2-print.ts` 不另记来源），认不出。
        automatic: false,
        account: String::new(),
    }))
}

/// 撤销的重放（2.7.2 `wire/tree/fork.ts` 的 `foldUnpairedUndo`）：一条 `context.undo` 撤 `count` 轮，
/// 从倒数第 `count` 个轮次开头（[`undo_anchor`]）起、到这条撤销为止都不算了；往回数时先碰到压缩或清空就不撤
/// （上游这时撤销会失败，不会写出这条）。2.7.2 撤销时写三行：`agent.switched`（切到新分支，分叉点是轮次开头的
/// 前一行）、和它配对的 `context.undo`、`context.undone`；按分支切出的可见范围和按这条配对的撤销折叠出来的
/// 一样，所以只认 `context.undo`，不另外走分支。
///
/// `whole` 为假时喂的只是文件后面一段：往回数到这段开头还不够 `count` 轮，就当这段里撤销之前的全撤了。
struct Undo {
    whole: bool,
    /// `whole` 为假时有撤销数过了这段开头（撤到了这段前面没读的行）。
    crossed: bool,
    /// 还算数的行里撤销要看的那些：轮次开头、压缩/清空、用户的话（带一行的样子，取标题用）。
    marks: Vec<Mark>,
}

struct Mark {
    line: usize,
    kind: MarkKind,
    /// 是一句算数的用户发言：折成一行的前 80 字。
    user: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MarkKind {
    /// 轮次开头，撤销按它数轮。
    Turn,
    /// 压缩、清空：撤销数到这里就停。
    Stop,
    /// 只是一句用户的话（轮次中间插的话）。
    Said,
}

impl Undo {
    fn new(whole: bool) -> Self {
        Self {
            whole,
            crossed: false,
            marks: Vec::new(),
        }
    }

    /// 喂第 `line` 行（只喂 [`replay_line`] 认的行）。是撤销且撤得动时返回撤掉的第一行：从它到 `line` 都不算了。
    fn feed(&mut self, line: usize, v: &Value) -> Option<usize> {
        let kind = match str_field(v, "type")? {
            "context.append_message" => {
                let m = v.get("message")?;
                let origin = m.get("origin").filter(|o| o.is_object());
                if origin.and_then(|o| str_field(o, "kind")) == Some("compaction_summary") {
                    MarkKind::Stop
                } else if undo_anchor(m) {
                    MarkKind::Turn
                } else {
                    MarkKind::Said
                }
            }
            "context.apply_compaction" | "context.clear" => MarkKind::Stop,
            "context.undo" => return self.undo(v.get("count")?.as_u64()?),
            _ => return None,
        };
        let user = message(v)
            .filter(|m| m.role == Role::User)
            .map(|m| jsonl::one_line(&m.text, 80));
        if kind != MarkKind::Said || user.is_some() {
            self.marks.push(Mark { line, kind, user });
        }
        None
    }

    fn undo(&mut self, count: u64) -> Option<usize> {
        if count == 0 {
            return None;
        }
        let mut left = count;
        for i in (0..self.marks.len()).rev() {
            match self.marks[i].kind {
                MarkKind::Stop => return None,
                MarkKind::Said => {}
                MarkKind::Turn => {
                    left -= 1;
                    if left == 0 {
                        let from = self.marks[i].line;
                        self.marks.truncate(i);
                        return Some(from);
                    }
                }
            }
        }
        if self.whole {
            return None;
        }
        self.marks.clear();
        self.crossed = true;
        Some(0)
    }

    fn users(&self) -> impl Iterator<Item = &String> {
        self.marks.iter().filter_map(|m| m.user.as_ref())
    }
}

/// 2.7.2 `isUndoAnchor`：用户自己发的话（没有来源、来源是 user，或用户敲的技能/插件斜杠命令），轮次中间插的话
/// （`origin.inTurn`）不算。
fn undo_anchor(m: &Value) -> bool {
    if str_field(m, "role") != Some("user") {
        return false;
    }
    let Some(o) = m.get("origin").filter(|o| o.is_object()) else {
        return true;
    };
    if o.get("inTurn").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    match str_field(o, "kind") {
        Some("user") => true,
        Some("skill_activation" | "plugin_command") => {
            str_field(o, "trigger") == Some("user-slash")
        }
        _ => false,
    }
}

/// 这一行撤销重放要不要看（含 `"context.` 加 [`REPLAY_TYPES`] 之一），不解析整行。
fn replay_line(line: &[u8]) -> bool {
    const PREFIX: &[u8] = b"\"context.";
    let mut rest = line;
    while let Some(i) = find(rest, PREFIX) {
        let after = &rest[i + PREFIX.len()..];
        if REPLAY_TYPES.iter().any(|t| after.starts_with(t)) {
            return true;
        }
        rest = &rest[i + 1..];
    }
    false
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 文件的一段完整行里撤销以后还算数的（`whole`：这段从文件开头起）。
fn kept<'a>(lines: impl Iterator<Item = &'a [u8]>, whole: bool) -> Vec<&'a [u8]> {
    kept_crossing(lines, whole).0
}

/// 同 [`kept`]，另外返回有没有撤销撤过了这段开头（这段前面的行也被撤了，光看这段不知道撤到哪）。
fn kept_crossing<'a>(lines: impl Iterator<Item = &'a [u8]>, whole: bool) -> (Vec<&'a [u8]>, bool) {
    let mut out: Vec<&[u8]> = Vec::new();
    // `out` 里每行在这段里是第几行。
    let mut at: Vec<usize> = Vec::new();
    let mut undo = Undo::new(whole);
    for (i, line) in lines.enumerate() {
        if replay_line(line)
            && let Some(v) = jsonl::parse(line)
            && let Some(from) = undo.feed(i, &v)
        {
            let keep = at.partition_point(|&j| j < from);
            out.truncate(keep);
            at.truncate(keep);
            continue;
        }
        out.push(line);
        at.push(i);
    }
    (out, undo.crossed)
}

/// 从头按撤销重放整个文件的结果，精确计数和头尾里找不到用户发言时用。
struct Replay {
    /// 完整行里还算数的用户发言。
    users: usize,
    /// 最后那行还没写完换行、又是一句用户的话：算 1。
    partial: usize,
    /// 完整的行读到了哪里。
    end: u64,
    /// 还算数的第一句用户发言（一行、前 80 字）。
    first_user: Option<String>,
}

impl Replay {
    /// 逐行读，只解析 [`replay_line`] 认的行；超长的行跳过（同 [`window::for_each_line_from`]）。
    fn of_file(file: &mut File) -> io::Result<Self> {
        let mut undo = Undo::new(true);
        let mut partial = 0;
        let mut line_no = 0;
        let end = window::for_each_line_from(file, 0, |line, complete| {
            let i = line_no;
            line_no += 1;
            if !replay_line(line) {
                return;
            }
            let Some(v) = jsonl::parse(line) else {
                return;
            };
            if complete {
                undo.feed(i, &v);
            } else if message(&v).is_some_and(|m| m.role == Role::User) {
                partial = 1;
            }
        })?;
        Ok(Self {
            users: undo.users().count(),
            partial,
            end,
            first_user: undo.users().next().cloned(),
        })
    }
}

/// 新建的对话是什么时候生成的（`state.json` 的 `createdAt`，毫秒）：id 是随机 uuid 看不出时间，会话台新建、
/// 托管在临时 id 下的对话靠它认回来。`path` 是 [`Session::path`]；读不出返回 `None`。
pub fn created_at(path: &Path) -> Option<SystemTime> {
    let state = std::fs::read(session_dir(path)?.join(STATE)).ok()?;
    serde_json::from_slice::<Value>(&state)
        .ok()?
        .get("createdAt")
        .and_then(at)
}

/// 记录里的时间：毫秒数（2.7.2 `normalizeTimestampMs` 也认秒数），老版本 `state.json` 可能是 RFC 3339 字符串。
fn at(v: &Value) -> Option<SystemTime> {
    if let Some(s) = v.as_str() {
        return super::time::parse_rfc3339(s);
    }
    let n = v.as_f64().filter(|n| n.is_finite() && *n > 0.0)?;
    let ms = if n > 1e12 { n } else { n * 1000.0 };
    from_millis(ms as i64)
}

/// 用户和 AI 的文字消息。用户的是 `context.append_message`（role 为 user），去掉系统注入的
/// （`origin.kind` 为 injection、system_trigger、retry、compaction_summary；技能和插件命令只认用户敲的斜杠命令）；
/// AI 的是 `context.append_loop_event` 里的文字块。思考块、工具调用不算。
pub(crate) fn message(v: &Value) -> Option<PreviewMessage> {
    let (role, text) = match str_field(v, "type")? {
        "context.append_message" => {
            let m = v.get("message")?;
            if str_field(m, "role") != Some("user") || !user_typed(m.get("origin")) {
                return None;
            }
            (Role::User, text_of(m.get("content")?))
        }
        "context.append_loop_event" => {
            let e = v.get("event")?;
            let part = e.get("part")?;
            if str_field(e, "type") != Some("content.part")
                || str_field(part, "type") != Some("text")
            {
                return None;
            }
            (Role::Assistant, str_field(part, "text")?.trim().to_owned())
        }
        _ => return None,
    };
    if text.is_empty() {
        return None;
    }
    Some(PreviewMessage {
        role,
        text,
        at: v.get("time").and_then(at),
    })
}

/// 2.7.2 `isUserTypedOrigin`。
fn user_typed(origin: Option<&Value>) -> bool {
    let Some(o) = origin.filter(|o| o.is_object()) else {
        return true;
    };
    match str_field(o, "kind") {
        Some("skill_activation" | "plugin_command") => {
            str_field(o, "trigger") == Some("user-slash")
        }
        Some("injection" | "system_trigger" | "retry" | "compaction_summary") => false,
        _ => true,
    }
}

/// 文字块拼起来（和 2.7.2 一样直接接上）；只是一个附件标签（`<image path="…">`）的块不算字。
fn text_of(content: &Value) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| str_field(p, "type") == Some("text"))
        .filter_map(|p| str_field(p, "text"))
        .filter(|t| !media_tag_only(t))
        .collect::<String>()
        .trim()
        .to_owned()
}

/// 2.7.2 `matchSingleMediaPathTag` 的简化：整块就是一个 `<image|video|audio|file … path="…">` 标签。
fn media_tag_only(text: &str) -> bool {
    let t = text.trim();
    let Some(rest) = t.strip_prefix('<') else {
        return false;
    };
    let kind_ok = ["image", "video", "audio", "file"].iter().any(|k| {
        rest.strip_prefix(k)
            .is_some_and(|r| r.starts_with([' ', '>', '/']))
    });
    kind_ok && t.contains(" path=\"") && t.ends_with('>') && t.matches('<').count() <= 2
}

/// 整段对话里的一行：消息（同 [`message`]），加上工具调用（`tool.call` 的 `name`、`args`）和返回（`tool.result`）。
fn parts(v: &Value) -> Vec<Part> {
    if let Some(m) = message(v) {
        return vec![Part::Message(m)];
    }
    if str_field(v, "type") != Some("context.append_loop_event") {
        return Vec::new();
    }
    let Some(e) = v.get("event") else {
        return Vec::new();
    };
    match str_field(e, "type") {
        Some("tool.call") => vec![Part::ToolCall {
            name: str_field(e, "name").unwrap_or("工具").to_owned(),
            input: transcript::input_summary(e.get("args").unwrap_or(&Value::Null)),
        }],
        Some("tool.result") => vec![Part::ToolResult {
            text: transcript::output_text(e.get("result").unwrap_or(&Value::Null)),
        }],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_dir_of_both_files() {
        let d = Path::new("/h/.kimi-code/sessions/wd_app_0123456789ab/session_x");
        assert_eq!(session_dir(&d.join("state.json")), Some(d));
        assert_eq!(session_dir(&wire_path(d)), Some(d));
        assert_eq!(session_dir(&d.join("agents/sub1/wire.jsonl")), None);
        assert_eq!(session_dir(&d.join("logs/kimi-code.log")), None);
    }

    #[test]
    fn user_messages_skip_injections() {
        let user = |origin: Value| {
            json!({"type": "context.append_message", "time": 1_795_000_000_000_i64,
                   "message": {"role": "user", "content": [{"type": "text", "text": "改一下"}], "origin": origin}})
        };
        assert!(message(&user(json!({"kind": "user"}))).is_some());
        assert!(message(&user(Value::Null)).is_some());
        assert!(message(&user(json!({"kind": "injection"}))).is_none());
        assert!(message(&user(json!({"kind": "compaction_summary"}))).is_none());
        assert!(
            message(&user(
                json!({"kind": "skill_activation", "trigger": "model"})
            ))
            .is_none()
        );
        assert!(
            message(&user(
                json!({"kind": "skill_activation", "trigger": "user-slash"})
            ))
            .is_some()
        );
    }

    #[test]
    fn media_tags_are_not_text() {
        assert!(media_tag_only(r#"<image path="/tmp/a.png">"#));
        assert!(media_tag_only(
            r#"<file id="1" path="/tmp/a b.pdf"></file>"#
        ));
        assert!(!media_tag_only("<imagine> 这不是标签"));
        assert!(!media_tag_only(r#"看图 <image path="/tmp/a.png">"#));
        let content = json!([
            {"type": "text", "text": "看这张"},
            {"type": "text", "text": "<image path=\"/tmp/a.png\">"},
            {"type": "text", "text": "图"},
        ]);
        assert_eq!(text_of(&content), "看这张图");
    }

    #[test]
    fn times_in_millis_seconds_or_text() {
        let ms = from_millis(1_795_000_000_123);
        assert_eq!(at(&json!(1_795_000_000_123_i64)), ms);
        assert_eq!(at(&json!(1_795_000_000)), from_millis(1_795_000_000_000));
        assert_eq!(at(&json!(0)), None);
        assert!(at(&json!("2026-11-20T08:00:00Z")).is_some());
    }

    #[test]
    fn resume_with_message_only_resumes() {
        let src = KimiSource::new("/h/.kimi-code/sessions", Trash::new("/tmp/t"));
        let s = Session {
            vendor: Vendor::Kimi,
            id: SessionId::from("session_6b1f0c2e-4d3a-4b5c-9e8f-7a6b5c4d3e2f"),
            cwd: PathBuf::from("/work/app"),
            path: PathBuf::new(),
            title: None,
            git_branch: None,
            last_active: SystemTime::UNIX_EPOCH,
            message_count: 1,
            automatic: false,
            account: String::new(),
        };
        let cmd = src.resume_with_message_cmd(&s, "继续上次没做完的事。");
        assert_eq!(cmd, src.resume_cmd(&s));
        assert_eq!(
            src.message_delivery(&s),
            Delivery::Manual,
            "R 要报要自己输入"
        );
        assert_eq!(
            cmd.shell_line(),
            "kimi -S session_6b1f0c2e-4d3a-4b5c-9e8f-7a6b5c4d3e2f"
        );
    }
}
