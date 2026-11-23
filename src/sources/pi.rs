//! pi：`~/.pi/agent/sessions/--<编码路径>--/<时间>_<会话id>.jsonl`。
//! 第一行 `session` 给 id、cwd；名称取最后一条 `session_info`（空名表示清除）；消息是 `message` 行。

use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::jsonl::{self, str_field};
use super::time::{from_millis, parse_rfc3339};
use super::window::{self, Window};
use super::{
    Command, Counted, Moment, Part, PreviewMessage, Progress, Role, Scan, Source, Transcript,
    Trash, TrashEntry, moment, transcript,
};
use crate::core::{Session, SessionId, Vendor};

pub struct PiSource {
    root: PathBuf,
    trash: Trash,
}

impl PiSource {
    /// `root` 是 `~/.pi/agent/sessions`。
    pub fn new(root: impl Into<PathBuf>, trash: Trash) -> Self {
        Self {
            root: root.into(),
            trash,
        }
    }
}

impl Source for PiSource {
    fn vendor(&self) -> Vendor {
        Vendor::Pi
    }

    fn scan(&self) -> Scan {
        super::scan_by_files(self)
    }

    fn load_preview(&self, session: &Session, n: usize) -> io::Result<Vec<PreviewMessage>> {
        jsonl::last_messages(&session.path, n, message)
    }

    fn load_transcript(&self, session: &Session) -> io::Result<Transcript> {
        transcript::read(&session.path, parts)
    }

    fn load_moment(&self, session: &Session) -> io::Result<Moment> {
        moment::read_jsonl(&session.path, |v| moment::parts(parts(v)))
    }

    fn turn_done(&self, session: &Session) -> io::Result<Option<bool>> {
        turn_done(&session.path)
    }

    fn count_messages(&self, session: &Session) -> io::Result<usize> {
        self.count_since(session, None).map(|c| c.users)
    }

    fn count_since(&self, session: &Session, prev: Option<Progress>) -> io::Result<Counted> {
        let salt = self.parse_version();
        jsonl::count_since(&session.path, br#""role":"user""#, salt, message, prev)
    }

    fn resume_cmd(&self, session: &Session) -> Command {
        Command::new("pi", &["--session", session.id.as_str()], &session.cwd)
    }

    /// pi 1.5.2 `pi [options] [--] [@files...] [messages...]`：消息跟在 `--` 后面，
    /// 以 `@` 或 `-` 开头也不会被当成文件或选项。
    fn resume_with_message_cmd(&self, session: &Session, message: &str) -> Command {
        let mut cmd = self.resume_cmd(session);
        cmd.args.extend(["--".to_owned(), message.to_owned()]);
        cmd
    }

    fn new_cmd(&self, cwd: &Path) -> Command {
        Command::new("pi", &[], cwd)
    }

    fn trash(&self, session: &Session) -> io::Result<TrashEntry> {
        super::trash_files(&self.trash, session, &[])
    }

    fn restore(&self, entry: &TrashEntry) -> io::Result<()> {
        self.trash.restore(entry)
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn affects(&self, path: &Path) -> bool {
        super::affects_subdir_layout(&self.root, path)
    }

    fn session_files(&self) -> Option<io::Result<Vec<PathBuf>>> {
        Some(super::jsonl_in_subdirs(&self.root))
    }

    fn read_files(&self, paths: &[PathBuf]) -> Vec<io::Result<Option<Session>>> {
        jsonl::par_map(paths, read_session)
    }
}

pub(crate) fn read_session(path: &Path) -> io::Result<Option<Session>> {
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
    let header = head
        .first()
        .filter(|v| str_field(v, "type") == Some("session"));

    let head_msgs: Vec<PreviewMessage> = head.iter().filter_map(message).collect();
    let tail_msgs: Vec<PreviewMessage> = tail.iter().filter_map(message).collect();
    let users = |ms: &[PreviewMessage]| ms.iter().filter(|m| m.role == Role::User).count();
    let message_count = if w.whole {
        users(&head_msgs)
    } else {
        users(&head_msgs) + users(&tail_msgs)
    };

    // 最新一条 session_info 说了算，空名表示清除。
    let name = [tail, &head].iter().find_map(|vs| {
        vs.iter()
            .rev()
            .find(|v| str_field(v, "type") == Some("session_info"))
            .map(|v| {
                str_field(v, "name")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
    });
    let title = name
        .flatten()
        .or_else(|| {
            head_msgs
                .iter()
                .find(|m| m.role == Role::User)
                .map(|m| m.text.clone())
        })
        .map(|t| jsonl::one_line(&t, 80));

    if message_count == 0 && title.is_none() {
        return Ok(None);
    }

    let id = header
        .and_then(|h| str_field(h, "id"))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| id_from_file_name(path));
    let cwd = header
        .and_then(|h| str_field(h, "cwd"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| decode_dir(path));
    let last_active = tail_msgs
        .iter()
        .rev()
        .find_map(|m| m.at)
        .or_else(|| header.and_then(|h| str_field(h, "timestamp").and_then(parse_rfc3339)))
        .unwrap_or(w.mtime);

    Ok(Some(Session {
        vendor: Vendor::Pi,
        id: SessionId::from(id),
        cwd,
        path: path.to_path_buf(),
        title,
        git_branch: None,
        last_active,
        message_count,
        // pi 的会话文件不记是不是无头（`pi -p`）起的：头行只有 id、cwd、时间、版本（本机 pi 核对过）。
        automatic: false,
        account: String::new(),
    }))
}

pub(crate) fn message(v: &Value) -> Option<PreviewMessage> {
    if str_field(v, "type") != Some("message") {
        return None;
    }
    let m = v.get("message")?;
    let role = match str_field(m, "role")? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let text = jsonl::content_text(m.get("content")?, &["text"])?;
    let at = m
        .get("timestamp")
        .and_then(Value::as_i64)
        .and_then(from_millis)
        .or_else(|| str_field(v, "timestamp").and_then(parse_rfc3339));
    Some(PreviewMessage { role, text, at })
}

/// 整段对话里的一行：消息（同 [`message`]），加上 AI 消息里的 `toolCall` 块和 `toolResult` 消息。
fn parts(v: &Value) -> Vec<Part> {
    let mut out: Vec<Part> = message(v).map(Part::Message).into_iter().collect();
    if str_field(v, "type") != Some("message") {
        return out;
    }
    let Some(m) = v.get("message") else {
        return out;
    };
    let content = m.get("content").unwrap_or(&Value::Null);
    match str_field(m, "role") {
        Some("assistant") => {
            for it in content.as_array().into_iter().flatten() {
                if str_field(it, "type") == Some("toolCall") {
                    out.push(Part::ToolCall {
                        name: str_field(it, "name").unwrap_or("工具").to_owned(),
                        input: transcript::input_summary(
                            it.get("arguments").unwrap_or(&Value::Null),
                        ),
                    });
                }
            }
        }
        Some("toolResult") => out.push(Part::ToolResult {
            text: transcript::output_text(content),
        }),
        _ => {}
    }
    out
}

/// 这一轮答完了没有：看文件尾部（[`moment::MOMENT_TAIL_BYTES`]）最后一条 `message`。
/// AI 的回复、停止原因不是工具调用（`stopReason` 不是 `toolUse`，也没带 `toolCall` 块）算答完：
/// pi 1.6 的 `stop`（说完了）、`aborted`（按 Esc 打断）、`error`、`length` 之后都停着等你。
/// 用户消息、工具返回（`toolResult`）和别的角色都算没答完；尾部一条消息都没有就看不出来（`None`）。
pub(crate) fn turn_done(path: &Path) -> io::Result<Option<bool>> {
    let (buf, _) = window::read_tail_lines(path, moment::MOMENT_TAIL_BYTES)?;
    let last = window::lines(&buf)
        .rev()
        .filter_map(jsonl::parse)
        .find(|v| str_field(v, "type") == Some("message"));
    Ok(last.map(|v| {
        let Some(m) = v.get("message") else {
            return false;
        };
        let calls_tool = str_field(m, "stopReason") == Some("toolUse")
            || m.get("content")
                .and_then(Value::as_array)
                .is_some_and(|c| c.iter().any(|it| str_field(it, "type") == Some("toolCall")));
        str_field(m, "role") == Some("assistant") && !calls_tool
    }))
}

/// `<时间>_<id>.jsonl` → id。
fn id_from_file_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match stem.rsplit_once('_') {
        Some((_, id)) => id.to_owned(),
        None => stem,
    }
}

/// `--Users-me-proj--` → `/Users/me/proj`（有损）。
fn decode_dir(path: &Path) -> PathBuf {
    let Some(name) = path.parent().and_then(Path::file_name) else {
        return PathBuf::new();
    };
    let name = name.to_string_lossy();
    let inner = name.trim_start_matches("--").trim_end_matches("--");
    PathBuf::from(format!("/{}", inner.replace('-', "/")))
}
