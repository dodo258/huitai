//! Codex：新版（本机 0.171）把对话记在 `~/.codex/state_<N>.sqlite` 的 `threads` 表、正文在
//! `thread_history_<N>.sqlite`，大部分对话已经没有 rollout 文件；仍在的 rollout 在
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`。两边合起来，按 id 去重，状态库优先。
//! 标题：`session_index.jsonl` 的 `thread_name` > 库里 `name` > 库里 `title` > 第一句话。
//! 子代理对话和 Codex 里已归档的不列。删除只隐藏（见 DECISIONS.md）。
//! 自动运行：库里 `source` 列、rollout 的 `session_meta.payload.source` 为 `exec`（`codex exec` 起的；
//! 本机核对过的其余取值有 `cli`、`vscode`、`unknown` 和子代理的 JSON）。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::jsonl::{self, str_field};
use super::sqlite::{self, Row};
use super::time::{from_millis, parse_rfc3339};
use super::window::Window;
use super::{
    Command, Counted, Part, PreviewMessage, Progress, Role, Scan, Source, Transcript, Trash,
    TrashEntry, transcript,
};
use crate::core::{Session, SessionId, Vendor};

pub struct CodexSource {
    root: PathBuf,
    trash: Trash,
}

impl CodexSource {
    /// `root` 是 `~/.codex`。
    pub fn new(root: impl Into<PathBuf>, trash: Trash) -> Self {
        Self {
            root: root.into(),
            trash,
        }
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

    fn rollout_files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        collect_jsonl(&self.root.join("sessions"), &mut out);
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
                        if let Some(s) = session_from_row(row, &names, &counts) {
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
        for (path, res) in rollouts.iter().zip(jsonl::par_map(&rollouts, read_rollout)) {
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

    /// rollout 文件还在就读文件，否则从正文库读最后 [`HISTORY_ITEMS`] 项。
    fn load_transcript(&self, session: &Session) -> io::Result<Transcript> {
        if session.path.is_file() {
            return transcript::read(&session.path, parts);
        }
        let (Some(db), Some(id)) = (self.history_db(), sqlite::safe_id(session.id.as_str())) else {
            return Ok(Transcript::default());
        };
        let types = HISTORY_TYPES.map(|t| format!("'{t}'")).join(", ");
        let sql = format!(
            "select item_json, created_at_ms from thread_items where thread_id = '{id}' \
             and item_type in ({types}) order by rollout_ordinal desc limit {}",
            HISTORY_ITEMS + 1
        );
        let mut rows = sqlite::query(&db, &sql)?;
        let truncated = rows.len() > HISTORY_ITEMS;
        rows.truncate(HISTORY_ITEMS);
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

    fn count_messages(&self, session: &Session) -> io::Result<usize> {
        if session.path.is_file() {
            return jsonl::count_user_messages(&session.path, USER_MARKER, message);
        }
        self.count_in_db(session)
    }

    /// 有会话文件的接着数；只在正文库里的每次查库（一次 count 查询）。
    fn count_since(&self, session: &Session, prev: Option<Progress>) -> io::Result<Counted> {
        if session.path.is_file() {
            let salt = self.parse_version();
            return jsonl::count_since(&session.path, USER_MARKER, salt, message, prev);
        }
        Ok(Counted {
            users: self.count_in_db(session)?,
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
    let path = PathBuf::from(s("rollout_path").unwrap_or_default());
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
        // 正文库还没收录的对话，rollout 在就从头尾数。
        None => read_rollout(&path)
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
    let head: Vec<Value> = w.head_lines().filter_map(jsonl::parse).collect();
    let tail_own: Vec<Value>;
    let tail: &[Value] = if w.whole {
        &head
    } else {
        tail_own = w.tail_lines().filter_map(jsonl::parse).collect();
        &tail_own
    };
    let Some(meta) = head
        .iter()
        .find(|v| str_field(v, "type") == Some("session_meta"))
        .and_then(|v| v.get("payload"))
    else {
        return Ok(None);
    };
    if meta
        .get("source")
        .is_some_and(|s| s.get("subagent").is_some())
    {
        return Ok(None);
    }
    let Some(id) = str_field(meta, "id")
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| rollout_id(path).map(str::to_owned))
    else {
        return Ok(None);
    };

    let head_msgs: Vec<PreviewMessage> = head.iter().filter_map(message).collect();
    let tail_msgs: Vec<PreviewMessage> = tail.iter().filter_map(message).collect();
    let users = |ms: &[PreviewMessage]| ms.iter().filter(|m| m.role == Role::User).count();
    let message_count = if w.whole {
        users(&head_msgs)
    } else {
        users(&head_msgs) + users(&tail_msgs)
    };
    let title = head_msgs
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| jsonl::one_line(&m.text, 80));
    let cwd = str_field(meta, "cwd")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            tail.iter().rev().find_map(|v| {
                (str_field(v, "type") == Some("turn_context"))
                    .then(|| v.get("payload").and_then(|p| str_field(p, "cwd")))
                    .flatten()
                    .map(PathBuf::from)
            })
        })
        .unwrap_or_default();
    let git_branch = meta
        .pointer("/git/branch")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let last_active = tail_msgs
        .iter()
        .rev()
        .find_map(|m| m.at)
        .or_else(|| {
            tail.iter()
                .rev()
                .find_map(|v| str_field(v, "timestamp").and_then(parse_rfc3339))
        })
        .unwrap_or(w.mtime);

    Ok(Some(Session {
        vendor: Vendor::Codex,
        id: SessionId::from(id),
        cwd,
        path: path.to_path_buf(),
        title,
        git_branch,
        last_active,
        message_count,
        automatic: str_field(meta, "source") == Some(EXEC_SOURCE),
    }))
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

/// 正文库里整段对话读这几类：消息，和几种工具项。
const HISTORY_TYPES: [&str; 6] = [
    "userMessage",
    "agentMessage",
    "commandExecution",
    "mcpToolCall",
    "fileChange",
    "webSearch",
];
/// 正文库里整段对话最多读最后这么多项（sqlite3 一次查询的输出有上限）。
const HISTORY_ITEMS: usize = 3000;

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

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => collect_jsonl(&p, out),
            Ok(t) if t.is_file() && p.extension().is_some_and(|x| x == "jsonl") => out.push(p),
            _ => {}
        }
    }
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
