//! Codex：新版（本机 0.171）把对话记在 `~/.codex/state_<N>.sqlite` 的 `threads` 表、正文在
//! `thread_history_<N>.sqlite`，大部分对话已经没有 rollout 文件；仍在的 rollout 在
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`。两边合起来，按 id 去重，状态库优先。
//! 标题：`session_index.jsonl` 的 `thread_name` > 库里 `name` > 库里 `title` > 第一句话。
//! 子代理对话和 Codex 里已归档的不列。删除只隐藏（见 DECISIONS.md）。
//! 自动运行：库里 `source` 列、rollout 的 `session_meta.payload.source` 为 `exec`（`codex exec` 起的；
//! 本机核对过的其余取值有 `cli`、`vscode`、`unknown` 和子代理的 JSON）。
//! 归档：Codex 把闲置的会话压成 `rollout-*.jsonl.zst` 并删掉 `.jsonl`（库里的 `rollout_path` 还是 `.jsonl`）。
//! 读的时候先找 `.jsonl`，没有就流式解压 `.zst`，只在内存里读，不写回、不解压到厂商目录。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::jsonl::{self, str_field};
use super::limit;
use super::moment::{Event, Folder, MOMENT_TAIL_BYTES};
use super::sqlite::{self, Row};
use super::time::{from_millis, parse_rfc3339};
use super::window::{self, MAX_LINE_BYTES, Window};
use super::{
    Command, Counted, Moment, Part, PreviewMessage, Progress, Role, Scan, Source,
    TRANSCRIPT_MAX_BYTES, Transcript, Trash, TrashEntry, moment, transcript,
};
use crate::core::{Session, SessionId, Vendor};

pub struct CodexSource {
    root: PathBuf,
    trash: Trash,
    /// 归档 rollout 的扫描结果，按文件大小和修改时间认：归档不再变，每次扫描都解压整个文件太慢。
    archives: Mutex<HashMap<PathBuf, (Stamp, Option<Session>)>>,
}

/// 文件大小和修改时间。
type Stamp = (u64, SystemTime);

impl CodexSource {
    /// `root` 是 `~/.codex`。
    pub fn new(root: impl Into<PathBuf>, trash: Trash) -> Self {
        Self {
            root: root.into(),
            trash,
            archives: Mutex::default(),
        }
    }

    /// 同 [`read_rollout`]；归档的读过一次就记下，文件没变不再解压。
    fn read_rollout(&self, path: &Path) -> io::Result<Option<Session>> {
        if !is_archive(path) {
            return read_rollout(path);
        }
        let meta = fs::metadata(path)?;
        let stamp = (meta.len(), meta.modified()?);
        let cache = || self.archives.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((s, session)) = cache().get(path)
            && *s == stamp
        {
            return Ok(session.clone());
        }
        let session = read_archived_rollout(path, stamp.1)?;
        cache().insert(path.to_path_buf(), (stamp, session.clone()));
        Ok(session)
    }

    fn state_db(&self) -> Option<PathBuf> {
        newest_db(&self.root, "state_")
    }

    fn history_db(&self) -> Option<PathBuf> {
        newest_db(&self.root, "thread_history_")
    }

    /// 只在正文库里的对话：查库数用户发言。
    fn count_in_db(&self, session: &Session) -> io::Result<usize> {
        let (Some(db), Some(id)) = (self.history_db(), sqlite::safe_id(session.id.as_str())) else {
            return Ok(0);
        };
        let sql = format!(
            "select count(*) as n from thread_items where thread_id = '{id}' and item_type = 'userMessage'"
        );
        Ok(sqlite::query(&db, &sql)?
            .first()
            .and_then(|r| r.get("n"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize)
    }

    /// `session_index.jsonl`：id → 最新的 thread_name。
    fn thread_names(&self) -> HashMap<String, String> {
        let Ok(bytes) = fs::read(self.root.join("session_index.jsonl")) else {
            return HashMap::new();
        };
        super::window::lines(&bytes)
            .filter_map(jsonl::parse)
            .filter_map(|v| {
                let id = str_field(&v, "id")?.to_owned();
                let name = str_field(&v, "thread_name")?.trim().to_owned();
                (!name.is_empty()).then_some((id, name))
            })
            .collect()
    }

    /// 会话目录里的 rollout。同一个会话 `.jsonl` 和 `.zst` 都在（Codex 正在压）时只要 `.jsonl`。
    fn rollout_files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        collect_rollouts(&self.root.join("sessions"), &mut out);
        let plain: HashSet<PathBuf> = out.iter().filter(|p| !is_archive(p)).cloned().collect();
        out.retain(|p| !is_archive(p) || !plain.contains(&p.with_extension("")));
        out.sort();
        out
    }
}

impl Source for CodexSource {
    fn vendor(&self) -> Vendor {
        Vendor::Codex
    }

    fn scan(&self) -> Scan {
        let mut scan = Scan::default();
        let hidden = self.trash.hidden_ids(Vendor::Codex).unwrap_or_else(|e| {
            scan.problems.push(format!("回收站: {e}"));
            HashSet::new()
        });
        let names = self.thread_names();

        // 状态库里出现过的 id（含归档、子代理），对应的 rollout 不再单独列。
        let mut known: HashSet<String> = HashSet::new();
        if let Some(db) = self.state_db() {
            match sqlite::query(&db, THREADS_SQL) {
                Ok(rows) => {
                    let counts = self.history_db().map_or_else(HashMap::new, |h| {
                        user_counts(&h).unwrap_or_else(|e| {
                            scan.problems.push(format!("{}: {e}", h.display()));
                            HashMap::new()
                        })
                    });
                    for row in &rows {
                        let Some(id) = row.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        known.insert(id.to_owned());
                        if hidden.contains(id) {
                            continue;
                        }
                        if let Some(s) = session_from_row(self, row, &names, &counts) {
                            scan.sessions.push(s);
                        }
                    }
                }
                Err(e) => scan.problems.push(format!("{}: {e}", db.display())),
            }
        }

        let rollouts: Vec<PathBuf> = self
            .rollout_files()
            .into_iter()
            .filter(|p| rollout_id(p).is_none_or(|id| !known.contains(id) && !hidden.contains(id)))
            .collect();
        let read = |p: &Path| self.read_rollout(p);
        for (path, res) in rollouts.iter().zip(jsonl::par_map(&rollouts, read)) {
            match res {
                Ok(Some(mut s))
                    if !known.contains(s.id.as_str()) && !hidden.contains(s.id.as_str()) =>
                {
                    if let Some(name) = names.get(s.id.as_str()) {
                        s.title = Some(jsonl::one_line(name, 80));
                    }
                    scan.sessions.push(s);
                }
                Ok(_) => {}
                Err(e) => scan.problems.push(format!("{}: {e}", path.display())),
            }
        }
        scan
    }

    /// rollout 文件还在就从它的尾部读（最新），否则从正文库读。
    fn load_preview(&self, session: &Session, n: usize) -> io::Result<Vec<PreviewMessage>> {
        if is_archive(&session.path) {
            return archived_last_messages(&session.path, n);
        }
        if session.path.is_file() {
            return jsonl::last_messages(&session.path, n, message);
        }
        let (Some(db), Some(id)) = (self.history_db(), sqlite::safe_id(session.id.as_str())) else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "select item_json, created_at_ms from thread_items where thread_id = '{id}' \
             and item_type in ('userMessage', 'agentMessage') order by rollout_ordinal desc limit {n}"
        );
        let mut out: Vec<PreviewMessage> = sqlite::query(&db, &sql)?
            .iter()
            .filter_map(history_message)
            .map(jsonl::clip)
            .collect();
        out.reverse();
        Ok(out)
    }

    /// rollout 文件还在就读文件，否则从正文库读最后 [`TRANSCRIPT_MAX_BYTES`] 的那些项。
    fn load_transcript(&self, session: &Session) -> io::Result<Transcript> {
        if is_archive(&session.path) {
            let (lines, truncated) = archived_tail(&session.path, TRANSCRIPT_MAX_BYTES)?;
            let parts = lines
                .iter()
                .filter_map(|l| jsonl::parse(l))
                .flat_map(|v| parts(&v))
                .map(transcript::clip)
                .collect();
            return Ok(Transcript { parts, truncated });
        }
        if session.path.is_file() {
            return transcript::read(&session.path, parts);
        }
        let (Some(db), Some(id)) = (self.history_db(), sqlite::safe_id(session.id.as_str())) else {
            return Ok(Transcript::default());
        };
        let types = HISTORY_TYPES.map(|t| format!("'{t}'")).join(", ");
        let items = format!("thread_items where thread_id = '{id}' and item_type in ({types})");
        // 从最新的往前，正文累计不超过 TRANSCRIPT_MAX_BYTES 的那些（同读 rollout 文件的上限）。
        let sql = format!(
            "select item_json, created_at_ms from (select item_json, created_at_ms, rollout_ordinal, \
             sum(length(cast(item_json as blob))) over (order by rollout_ordinal desc \
             rows unbounded preceding) as upto from {items}) \
             where upto <= {TRANSCRIPT_MAX_BYTES} order by rollout_ordinal desc"
        );
        let mut rows = sqlite::query(&db, &sql)?;
        let total = sqlite::query(&db, &format!("select count(*) as n from {items}"))?
            .first()
            .and_then(|r| r.get("n")?.as_u64())
            .unwrap_or(0);
        let truncated = (rows.len() as u64) < total;
        rows.reverse();
        Ok(Transcript {
            parts: rows
                .iter()
                .flat_map(history_parts)
                .map(transcript::clip)
                .collect(),
            truncated,
        })
    }

    /// rollout 文件还在就只读它的尾部（带轮次事件和计划）；只剩正文库时从整段对话推。
    fn load_moment(&self, session: &Session) -> io::Result<Moment> {
        if is_archive(&session.path) {
            let mut f = Folder::default();
            let (lines, _) = archived_tail(&session.path, MOMENT_TAIL_BYTES)?;
            for v in lines.iter().filter_map(|l| jsonl::parse(l)) {
                moment_events(&v).into_iter().for_each(|e| f.push(e));
            }
            return Ok(f.finish());
        }
        if session.path.is_file() {
            return moment::read_jsonl(&session.path, moment_events);
        }
        Ok(moment::from_parts(self.load_transcript(session)?.parts))
    }

    fn count_messages(&self, session: &Session) -> io::Result<usize> {
        if is_archive(&session.path) {
            return archived_user_count(&session.path);
        }
        if session.path.is_file() {
            return jsonl::count_user_messages(&session.path, USER_MARKER, message);
        }
        self.count_in_db(session)
    }

    /// 有会话文件的接着数；归档的（不会再追加）和只在正文库里的每次数一遍。
    fn count_since(&self, session: &Session, prev: Option<Progress>) -> io::Result<Counted> {
        if !is_archive(&session.path) && session.path.is_file() {
            let salt = self.parse_version();
            return jsonl::count_since(&session.path, USER_MARKER, salt, message, prev);
        }
        Ok(Counted {
            users: self.count_messages(session)?,
            progress: None,
            appended: None,
        })
    }

    fn resume_cmd(&self, session: &Session) -> Command {
        Command::new("codex", &["resume", session.id.as_str()], &session.cwd)
    }

    fn new_cmd(&self, cwd: &Path) -> Command {
        Command::new("codex", &[], cwd)
    }

    fn created_at(&self, session: &Session) -> Option<SystemTime> {
        id_created(&session.id)
    }

    /// 只隐藏：对话在 Codex 自己的库里，移走 rollout 文件删不掉它（见 DECISIONS.md）。
    fn trash(&self, session: &Session) -> io::Result<TrashEntry> {
        self.trash
            .hide(Vendor::Codex, session.id.as_str(), &session.path)
    }

    fn restore(&self, entry: &TrashEntry) -> io::Result<()> {
        self.trash.restore(entry)
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    /// `~/.codex` 里日志、缓存一直在写；只有状态库、正文库、标题索引和 rollout 影响列表。
    fn affects(&self, path: &Path) -> bool {
        if path.starts_with(self.root.join("sessions")) {
            return true;
        }
        path.parent() == Some(self.root.as_path())
            && path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("state_")
                    || n.starts_with("thread_history_")
                    || n == "session_index.jsonl"
            })
    }
}

/// 用户发言所在的行都带它，整份数时只解析这些行。
const USER_MARKER: &[u8] = br#""type":"event_msg""#;
/// 会话头、轮次上下文所在的行（读归档时挑行用）。
const META_MARKER: &[u8] = br#""type":"session_meta""#;
const TURN_CONTEXT_MARKER: &[u8] = br#""type":"turn_context""#;

const THREADS_SQL: &str = "select id, rollout_path, cwd, title, name, git_branch, \
     updated_at_ms, updated_at, source, archived, first_user_message from threads";

fn user_counts(db: &Path) -> io::Result<HashMap<String, usize>> {
    let rows = sqlite::query(
        db,
        "select thread_id, count(*) as n from thread_items \
         where item_type = 'userMessage' group by thread_id",
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let id = r.get("thread_id")?.as_str()?.to_owned();
            Some((id, r.get("n")?.as_u64()? as usize))
        })
        .collect())
}

fn session_from_row(
    src: &CodexSource,
    row: &Row,
    names: &HashMap<String, String>,
    counts: &HashMap<String, usize>,
) -> Option<Session> {
    let s = |k: &str| {
        row.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let id = s("id")?;
    if row.get("archived").and_then(Value::as_i64).unwrap_or(0) != 0 {
        return None;
    }
    if s("source").is_some_and(is_subagent_source) {
        return None;
    }
    let path = on_disk(PathBuf::from(s("rollout_path").unwrap_or_default()));
    let title = names
        .get(id)
        .map(String::as_str)
        .or_else(|| s("name"))
        .or_else(|| s("title"))
        .or_else(|| s("first_user_message"))
        .map(|t| jsonl::one_line(t, 80));
    let last_active = row
        .get("updated_at_ms")
        .and_then(Value::as_i64)
        .and_then(from_millis)
        .or_else(|| {
            row.get("updated_at")
                .and_then(Value::as_i64)
                .and_then(|secs| from_millis(secs.saturating_mul(1000)))
        })
        .unwrap_or_else(|| super::mtime_or_epoch(&path));
    let message_count = match counts.get(id) {
        Some(&n) => n,
        // 正文库还没收录的对话，rollout（或它的归档）在就从文件数。
        None => src
            .read_rollout(&path)
            .ok()
            .flatten()
            .map_or(0, |r| r.message_count),
    };
    Some(Session {
        vendor: Vendor::Codex,
        id: SessionId::from(id),
        cwd: PathBuf::from(s("cwd").unwrap_or_default()),
        path,
        title,
        git_branch: s("git_branch").map(str::to_owned),
        last_active,
        message_count,
        automatic: s("source") == Some(EXEC_SOURCE),
        account: String::new(),
    })
}

/// `codex exec`（无头）起的对话的 `source`。
const EXEC_SOURCE: &str = "exec";

/// `source` 是 `{"subagent": …}` 这种 JSON 时是子代理对话。
fn is_subagent_source(source: &str) -> bool {
    source.starts_with('{')
        && serde_json::from_str::<Value>(source).is_ok_and(|v| v.get("subagent").is_some())
}

/// 读一个 rollout 文件的头尾。没有 `session_meta` 头、子代理、空文件返回 `None`。
pub(crate) fn read_rollout(path: &Path) -> io::Result<Option<Session>> {
    let w = Window::read(path)?;
    if w.len == 0 {
        return Ok(None);
    }
    let mut sum = Summary::default();
    for v in w.head_lines().filter_map(jsonl::parse) {
        sum.head(&v);
        sum.count(&v);
        if w.whole {
            sum.tail(&v);
        }
    }
    if !w.whole {
        for v in w.tail_lines().filter_map(jsonl::parse) {
            sum.tail(&v);
            sum.count(&v);
        }
    }
    Ok(sum.session(path, w.mtime))
}

/// 一个 rollout 里列表要的那些。行按文件先后喂：开头的给 [`Summary::head`]，结尾的给 [`Summary::tail`]，
/// 要数的给 [`Summary::count`]。
#[derive(Default)]
struct Summary {
    /// 第一个 `session_meta` 的 `payload`。
    meta: Option<Value>,
    /// 第一句用户发言。
    title: Option<String>,
    users: usize,
    /// 最后一个 `turn_context` 的 cwd（会话头缺 cwd 时用）。
    turn_cwd: Option<PathBuf>,
    /// 最后一条带时间的消息的时间。
    last_message_at: Option<SystemTime>,
    /// 最后一行能认的 `timestamp`（没有带时间的消息时用）。
    last_timestamp: Option<SystemTime>,
}

impl Summary {
    fn head(&mut self, v: &Value) {
        if self.meta.is_none() && str_field(v, "type") == Some("session_meta") {
            self.meta = v.get("payload").cloned();
        }
        if self.title.is_none()
            && let Some(m) = message(v).filter(|m| m.role == Role::User)
        {
            self.title = Some(jsonl::one_line(&m.text, 80));
        }
    }

    fn tail(&mut self, v: &Value) {
        if let Some(at) = message(v).and_then(|m| m.at) {
            self.last_message_at = Some(at);
        }
        if str_field(v, "type") == Some("turn_context")
            && let Some(cwd) = v.get("payload").and_then(|p| str_field(p, "cwd"))
        {
            self.turn_cwd = Some(PathBuf::from(cwd));
        }
        if let Some(t) = str_field(v, "timestamp").and_then(parse_rfc3339) {
            self.last_timestamp = Some(t);
        }
    }

    fn count(&mut self, v: &Value) {
        if message(v).is_some_and(|m| m.role == Role::User) {
            self.users += 1;
        }
    }

    fn session(self, path: &Path, mtime: SystemTime) -> Option<Session> {
        let meta = self.meta?;
        if meta
            .get("source")
            .is_some_and(|s| s.get("subagent").is_some())
        {
            return None;
        }
        let id = str_field(&meta, "id")
            .filter(|s| !s.is_empty())
            .or_else(|| rollout_id(path))?;
        let cwd = str_field(&meta, "cwd")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or(self.turn_cwd)
            .unwrap_or_default();
        let git_branch = meta
            .pointer("/git/branch")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        Some(Session {
            vendor: Vendor::Codex,
            id: SessionId::from(id),
            cwd,
            path: path.to_path_buf(),
            title: self.title,
            git_branch,
            last_active: self
                .last_message_at
                .or(self.last_timestamp)
                .unwrap_or(mtime),
            message_count: self.users,
            automatic: str_field(&meta, "source") == Some(EXEC_SOURCE),
            account: String::new(),
        })
    }
}

/// 归档的 rollout 后缀：`rollout-*.jsonl.zst`。
const ARCHIVE_EXT: &str = "zst";

fn is_archive(path: &Path) -> bool {
    path.extension().is_some_and(|x| x == ARCHIVE_EXT)
}

/// `path` 不在、旁边有它的归档 `<path>.zst` 时换成归档，否则原样。
fn on_disk(path: PathBuf) -> PathBuf {
    if path.as_os_str().is_empty() || is_archive(&path) || path.is_file() {
        return path;
    }
    let mut zst = path.clone().into_os_string();
    zst.push(".");
    zst.push(ARCHIVE_EXT);
    let zst = PathBuf::from(zst);
    if zst.is_file() { zst } else { path }
}

/// 流式解压一个归档的 rollout，逐个完整行交给 `f`（不含换行）。长过 [`MAX_LINE_BYTES`] 的行跳过，
/// 内存有上限；只读，解压出来的内容不落盘。
fn for_each_archived_line(path: &Path, mut f: impl FnMut(&[u8])) -> io::Result<()> {
    let decoder = zstd::stream::read::Decoder::new(File::open(path)?)?;
    let mut reader = BufReader::new(decoder);
    let mut line = Vec::new();
    loop {
        line.clear();
        let cap = MAX_LINE_BYTES as u64 + 1;
        if (&mut reader).take(cap).read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.len() as u64 == cap && line.last() != Some(&b'\n') {
            reader.skip_until(b'\n')?;
            continue;
        }
        window::lines(&line).for_each(&mut f);
    }
}

/// 归档里列表要的那些（同 [`read_rollout`]，但整个文件都读了，发言数是精确的）。整行解析只做
/// 会话头、消息、`turn_context` 和最后一行，工具输出那些大行只扫一眼。
fn read_archived_rollout(path: &Path, mtime: SystemTime) -> io::Result<Option<Session>> {
    let mut sum = Summary::default();
    let mut last = Vec::new();
    for_each_archived_line(path, |l| {
        let has = |m: &[u8]| l.windows(m.len()).any(|w| w == m);
        let wanted = (sum.meta.is_none() && has(META_MARKER))
            || has(USER_MARKER)
            || has(TURN_CONTEXT_MARKER);
        if let Some(v) = wanted.then(|| jsonl::parse(l)).flatten() {
            sum.head(&v);
            sum.tail(&v);
            sum.count(&v);
        }
        last.clear();
        last.extend_from_slice(l);
    })?;
    if let Some(v) = jsonl::parse(&last) {
        sum.tail(&v);
    }
    Ok(sum.session(path, mtime))
}

/// 归档里最后 `n` 条消息（按时间先后），同 [`jsonl::last_messages`]。
fn archived_last_messages(path: &Path, n: usize) -> io::Result<Vec<PreviewMessage>> {
    let mut out = VecDeque::with_capacity(n);
    if n == 0 {
        return Ok(Vec::new());
    }
    for_each_archived_line(path, |l| {
        if !l.windows(USER_MARKER.len()).any(|w| w == USER_MARKER) {
            return;
        }
        if let Some(m) = jsonl::parse(l).as_ref().and_then(message) {
            if out.len() == n {
                out.pop_front();
            }
            out.push_back(m);
        }
    })?;
    Ok(out.into_iter().map(jsonl::clip).collect())
}

/// 归档里的用户发言数（读完整个文件）。
fn archived_user_count(path: &Path) -> io::Result<usize> {
    let mut users = 0;
    for_each_archived_line(path, |l| {
        if l.windows(USER_MARKER.len()).any(|w| w == USER_MARKER)
            && jsonl::parse(l)
                .as_ref()
                .and_then(message)
                .is_some_and(|m| m.role == Role::User)
        {
            users += 1;
        }
    })?;
    Ok(users)
}

/// 归档解压后最后 `max` 字节里的完整行（同 [`window::read_tail_lines`]），和前面是否还有没留的。
fn archived_tail(path: &Path, max: u64) -> io::Result<(VecDeque<Vec<u8>>, bool)> {
    let (mut lines, mut kept, mut dropped) = (VecDeque::new(), 0u64, false);
    for_each_archived_line(path, |l| {
        kept += l.len() as u64 + 1;
        lines.push_back(l.to_vec());
        while kept > max {
            let Some(front) = lines.pop_front() else {
                break;
            };
            kept -= front.len() as u64 + 1;
            dropped = true;
        }
    })?;
    Ok((lines, dropped))
}

/// rollout 行里的对话消息：新格式 `item_completed` 的 UserMessage/AgentMessage，
/// 旧格式 `event_msg` 的 `user_message`/`agent_message`。
pub(crate) fn message(v: &Value) -> Option<PreviewMessage> {
    if str_field(v, "type") != Some("event_msg") {
        return None;
    }
    let p = v.get("payload")?;
    let (role, text) = match str_field(p, "type")? {
        "item_completed" => {
            let item = p.get("item")?;
            let role = match str_field(item, "type")? {
                "UserMessage" => Role::User,
                "AgentMessage" => Role::Assistant,
                _ => return None,
            };
            (role, item_text(item)?)
        }
        "user_message" => (Role::User, str_field(p, "message")?.trim().to_owned()),
        "agent_message" => (Role::Assistant, str_field(p, "message")?.trim().to_owned()),
        _ => return None,
    };
    if text.is_empty() {
        return None;
    }
    Some(PreviewMessage {
        role,
        text,
        at: str_field(v, "timestamp").and_then(parse_rfc3339),
    })
}

/// rollout 里的工具调用和返回都在 `response_item` 行（新旧格式都有）；新格式 `item_completed` 里的
/// `CommandExecution` 是 `exec` 工具内部跑的命令，和外面那次调用重复，不算。
fn parts(v: &Value) -> Vec<Part> {
    if let Some(m) = message(v) {
        return vec![Part::Message(m)];
    }
    if str_field(v, "type") != Some("response_item") {
        return Vec::new();
    }
    let Some(p) = v.get("payload") else {
        return Vec::new();
    };
    let null = &Value::Null;
    let field = |k: &str| p.get(k).unwrap_or(null);
    let name = || str_field(p, "name").unwrap_or("工具").to_owned();
    let part = match str_field(p, "type") {
        Some("function_call") => Part::ToolCall {
            name: name(),
            input: transcript::input_summary(field("arguments")),
        },
        Some("custom_tool_call") => Part::ToolCall {
            name: name(),
            input: transcript::input_summary(field("input")),
        },
        Some("local_shell_call") => Part::ToolCall {
            name: "shell".to_owned(),
            input: transcript::input_summary(p.pointer("/action").unwrap_or(null)),
        },
        Some("web_search_call") => Part::ToolCall {
            name: "web_search".to_owned(),
            input: transcript::input_summary(p.pointer("/action").unwrap_or(null)),
        },
        Some("function_call_output" | "custom_tool_call_output" | "local_shell_call_output") => {
            Part::ToolResult {
                text: transcript::output_text(field("output")),
            }
        }
        _ => return Vec::new(),
    };
    vec![part]
}

/// 「此刻」用的一行：整段对话的块（同 [`parts`]），加上 `event_msg` 里的轮次开始、做完（带用时）、
/// 被打断（`turn_aborted`，算失败，带原因），和 `update_plan` 调用里的计划。
fn moment_events(v: &Value) -> Vec<Event> {
    let mut out = moment::parts(parts(v));
    let Some(p) = v.get("payload") else {
        return out;
    };
    let ms = |k: &str| p.get(k).and_then(Value::as_u64).map(Duration::from_millis);
    match (str_field(v, "type"), str_field(p, "type")) {
        (Some("event_msg"), Some("task_started")) => {
            let at = p
                .get("started_at")
                .and_then(Value::as_i64)
                .and_then(|s| from_millis(s.saturating_mul(1000)))
                .or_else(|| str_field(v, "timestamp").and_then(parse_rfc3339));
            out.push(Event::TurnStarted(at));
        }
        (Some("event_msg"), Some("task_complete")) => out.push(Event::TurnEnded {
            duration: ms("duration_ms"),
            failed: None,
        }),
        (Some("event_msg"), Some("turn_aborted")) => out.push(Event::TurnEnded {
            duration: ms("duration_ms"),
            failed: Some(str_field(p, "reason").unwrap_or_default().to_owned()),
        }),
        (Some("response_item"), Some("function_call"))
            if str_field(p, "name") == Some("update_plan") =>
        {
            let args = p
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str::<Value>(s).ok());
            if let Some(steps) = args
                .as_ref()
                .and_then(|a| a.get("plan"))
                .and_then(|plan| moment::plan_steps(plan, &["step"]))
            {
                out.push(Event::Plan(steps));
            }
        }
        _ => {}
    }
    out.extend(limit::codex(v).map(Event::Limit));
    out
}

/// 正文库里整段对话读这几类：消息，和几种工具项。
const HISTORY_TYPES: [&str; 6] = [
    "userMessage",
    "agentMessage",
    "commandExecution",
    "mcpToolCall",
    "fileChange",
    "webSearch",
];

/// 正文库里整段对话的一项：消息（同 [`history_message`]），或工具调用（带返回的再跟一块返回）。
fn history_parts(row: &Row) -> Vec<Part> {
    if let Some(m) = history_message(row) {
        return vec![Part::Message(m)];
    }
    let Some(item) = row
        .get("item_json")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
    else {
        return Vec::new();
    };
    let null = &Value::Null;
    let field = |k: &str| item.get(k).unwrap_or(null);
    let (name, input, output) = match str_field(&item, "type") {
        Some("commandExecution") => (
            "shell".to_owned(),
            transcript::input_summary(&serde_json::json!({ "command": field("command") })),
            transcript::output_text(field("aggregatedOutput")),
        ),
        Some("mcpToolCall") => (
            str_field(&item, "tool").unwrap_or("工具").to_owned(),
            transcript::input_summary(field("arguments")),
            String::new(),
        ),
        Some("fileChange") => (
            "改文件".to_owned(),
            field("changes")
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| str_field(c, "path"))
                .collect::<Vec<_>>()
                .join(" "),
            String::new(),
        ),
        Some("webSearch") => (
            "web_search".to_owned(),
            transcript::input_summary(field("query")),
            String::new(),
        ),
        _ => return Vec::new(),
    };
    let mut out = vec![Part::ToolCall { name, input }];
    if !output.trim().is_empty() {
        out.push(Part::ToolResult { text: output });
    }
    out
}

/// 正文库 `thread_items` 里的一条。
fn history_message(row: &Row) -> Option<PreviewMessage> {
    let item: Value = serde_json::from_str(row.get("item_json")?.as_str()?).ok()?;
    let role = match str_field(&item, "type")? {
        "userMessage" => Role::User,
        "agentMessage" => Role::Assistant,
        _ => return None,
    };
    Some(PreviewMessage {
        role,
        text: item_text(&item)?,
        at: row
            .get("created_at_ms")
            .and_then(Value::as_i64)
            .and_then(from_millis),
    })
}

/// 消息条目的文字：`content` 块或直接的 `text`。
fn item_text(item: &Value) -> Option<String> {
    item.get("content")
        .and_then(|c| jsonl::content_text(c, &["text", "input_text", "output_text"]))
        .or_else(|| {
            str_field(item, "text")
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
        })
}

/// `rollout-2026-11-02T18-17-10-<uuid>[_<uuid>].jsonl` → 第一个 uuid。
fn rollout_id(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("rollout-")?.get(20..)?;
    let id = rest.get(..36)?;
    is_uuid(id).then_some(id)
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// `<prefix><数字>.sqlite` 里数字最大的那个。
fn newest_db(root: &Path, prefix: &str) -> Option<PathBuf> {
    fs::read_dir(root)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let n: u32 = name
                .strip_prefix(prefix)?
                .strip_suffix(".sqlite")?
                .parse()
                .ok()?;
            Some((n, e.path()))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, p)| p)
}

/// `dir` 下的 `*.jsonl` 和归档 `*.jsonl.zst`。
fn collect_rollouts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let rollout = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
            n.ends_with(".jsonl")
                || n.strip_suffix(".zst")
                    .is_some_and(|n| n.ends_with(".jsonl"))
        });
        match e.file_type() {
            Ok(t) if t.is_dir() => collect_rollouts(&p, out),
            Ok(t) if t.is_file() && rollout => out.push(p),
            _ => {}
        }
    }
}

/// Codex 会话 id 是 UUIDv7（本机 0.171 实测），开头 48 位是生成它的毫秒时间；不是 v7 的返回 `None`。
pub fn id_created(id: &SessionId) -> Option<SystemTime> {
    let b = id.native().as_bytes();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollout_id_from_file_name() {
        let p = Path::new(
            "/x/rollout-2026-11-02T18-17-10-01a1527b-f86f-75b3-b423-9b7af3f9605d_01a19bc3-5034-7e62-971b-3c879d009986.jsonl",
        );
        assert_eq!(rollout_id(p), Some("01a1527b-f86f-75b3-b423-9b7af3f9605d"));
        assert_eq!(rollout_id(Path::new("/x/rollout-bad.jsonl")), None);
        assert_eq!(rollout_id(Path::new("/x/other.jsonl")), None);
    }

    #[test]
    fn subagent_source() {
        assert!(is_subagent_source(r#"{"subagent":{"thread_spawn":{}}}"#));
        assert!(!is_subagent_source("vscode"));
        assert!(!is_subagent_source("{bad"));
    }
}
