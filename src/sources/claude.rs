//! Claude Code：`~/.claude/projects/<编码路径>/<会话id>.jsonl`。
//! 取 `cwd`、`gitBranch`、标题（`custom-title` > `ai-title` > `summary` > 第一句话）、最后时间、消息数，
//! 以及是不是自动运行：消息行的 `entrypoint` 记着是从哪里起的，`sdk-cli` 是无头的 `claude -p`
//! （本机核对过的取值：`cli` 终端、`claude-desktop` 桌面端、`sdk-cli`）。取最后一个，
//! 无头起的对话后来在终端里接着聊过就算自己的。

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::jsonl::{self, str_field};
use super::moment::Event;
use super::time::parse_rfc3339;
use super::window::Window;
use super::{
    Command, Counted, Moment, Part, PreviewMessage, Progress, Role, Scan, Source, Transcript,
    Trash, TrashEntry, encode_path, moment, transcript,
};
use crate::core::{Session, SessionId, Vendor};

/// 包在用户消息里、但不是用户自己说的话（斜杠命令回显、后台通知等）。
const WRAPPERS: [&str; 7] = [
    "<command-name>",
    "<command-message>",
    "<command-args>",
    "<local-command-stdout>",
    "<local-command-caveat>",
    "<task-notification>",
    "<system-reminder>",
];

pub struct ClaudeSource {
    root: PathBuf,
    trash: Trash,
}

impl ClaudeSource {
    /// `root` 是 `~/.claude/projects`。
    pub fn new(root: impl Into<PathBuf>, trash: Trash) -> Self {
        Self {
            root: root.into(),
            trash,
        }
    }
}

impl Source for ClaudeSource {
    fn vendor(&self) -> Vendor {
        Vendor::Claude
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
        moment::read_jsonl(&session.path, moment_events)
    }

    fn count_messages(&self, session: &Session) -> io::Result<usize> {
        self.count_since(session, None).map(|c| c.users)
    }

    fn count_since(&self, session: &Session, prev: Option<Progress>) -> io::Result<Counted> {
        let salt = self.parse_version();
        jsonl::count_since(&session.path, br#""type":"user""#, salt, message, prev)
    }

    fn resume_cmd(&self, session: &Session) -> Command {
        Command::new("claude", &["--resume", session.id.as_str()], &session.cwd)
    }

    fn new_cmd(&self, cwd: &Path) -> Command {
        Command::new("claude", &[], cwd)
    }

    /// 会话文件和同名的附属目录（子代理、工具输出）一起移进回收站。
    fn trash(&self, session: &Session) -> io::Result<TrashEntry> {
        super::trash_files(&self.trash, session, &[session.path.with_extension("")])
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

/// 读一个会话文件的头尾。空文件、没有任何对话内容的文件返回 `None`。
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

    let head_msgs: Vec<PreviewMessage> = head.iter().filter_map(message).collect();
    let tail_msgs: Vec<PreviewMessage> = tail.iter().filter_map(message).collect();
    let users = |ms: &[PreviewMessage]| ms.iter().filter(|m| m.role == Role::User).count();
    let message_count = if w.whole {
        users(&head_msgs)
    } else {
        // 头尾不重叠，两段相加是下限；尾部有 promptIndex 时它更准。
        let prompt_index = tail
            .iter()
            .filter_map(|v| v.pointer("/turnPosition/promptIndex")?.as_u64())
            .max()
            .map_or(0, |i| i as usize + 1);
        (users(&head_msgs) + users(&tail_msgs)).max(prompt_index)
    };

    let title = last_str(tail, "custom-title", "customTitle")
        .or_else(|| last_str(&head, "custom-title", "customTitle"))
        .or_else(|| last_str(tail, "ai-title", "aiTitle"))
        .or_else(|| last_str(&head, "ai-title", "aiTitle"))
        .or_else(|| last_str(tail, "summary", "summary"))
        .or_else(|| last_str(&head, "summary", "summary"))
        .map(str::to_owned)
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

    let cwd = project_cwd(path, &head, tail);
    let git_branch = [tail, &head]
        .iter()
        .find_map(|vs| {
            vs.iter()
                .rev()
                .find_map(|v| str_field(v, "gitBranch").filter(|s| !s.is_empty()))
        })
        .map(str::to_owned);
    let automatic = [tail, &head]
        .iter()
        .find_map(|vs| vs.iter().rev().find_map(|v| str_field(v, "entrypoint")))
        == Some("sdk-cli");
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

    let id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Some(Session {
        vendor: Vendor::Claude,
        id: SessionId::from(id),
        cwd,
        path: path.to_path_buf(),
        title,
        git_branch,
        last_active,
        message_count,
        automatic,
        account: String::new(),
    }))
}

/// 一行里的对话消息：用户自己说的话或 AI 的文字回复；工具调用、元信息返回 `None`。
pub(crate) fn message(v: &Value) -> Option<PreviewMessage> {
    if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let content = v.pointer("/message/content")?;
    let role = match str_field(v, "type")? {
        "user" => {
            if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            let has_tool_result = content.as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|it| str_field(it, "type") == Some("tool_result"))
            });
            if has_tool_result {
                return None;
            }
            Role::User
        }
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let text = jsonl::content_text(content, &["text"])?;
    if role == Role::User && WRAPPERS.iter().any(|w| text.starts_with(w)) {
        return None;
    }
    Some(PreviewMessage {
        role,
        text,
        at: str_field(v, "timestamp").and_then(parse_rfc3339),
    })
}

/// 整段对话里的一行：消息（同 [`message`]），加上 AI 的 `tool_use` 块和用户行里的 `tool_result` 块。
/// 子代理的行不算（它们在附属目录里另有文件）。
fn parts(v: &Value) -> Vec<Part> {
    let mut out: Vec<Part> = message(v).map(Part::Message).into_iter().collect();
    if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return out;
    }
    let Some(items) = v.pointer("/message/content").and_then(Value::as_array) else {
        return out;
    };
    for it in items {
        match str_field(it, "type") {
            Some("tool_use") => {
                let (name, input) = tool_use(it);
                out.push(Part::ToolCall { name, input });
            }
            Some("tool_result") => out.push(Part::ToolResult {
                text: transcript::output_text(it.get("content").unwrap_or(&Value::Null)),
            }),
            _ => {}
        }
    }
    out
}

/// `tool_use` 块的工具名和参数摘要。
fn tool_use(it: &Value) -> (String, String) {
    (
        str_field(it, "name").unwrap_or("工具").to_owned(),
        transcript::input_summary(it.get("input").unwrap_or(&Value::Null)),
    )
}

/// 「此刻」用的一行：消息（同 [`message`]）和带关联 id 的工具调用、返回（`tool_use.id`、`tool_result.tool_use_id`，
/// 并行的工具按 id 收），加上 `system` 行里的总结（`away_summary`）和这一轮用时（`turn_duration`），
/// `TodoWrite` 调用里的计划。子代理的行只看消息（同 [`parts`]）。
fn moment_events(v: &Value) -> Vec<Event> {
    let mut out: Vec<Event> = message(v)
        .map(|m| Event::Part(Part::Message(m)))
        .into_iter()
        .collect();
    if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return out;
    }
    let items = v.pointer("/message/content").and_then(Value::as_array);
    for it in items.into_iter().flatten() {
        // 没有关联 id 的（旧格式）退回按先后对上。
        let id = |k: &str| {
            str_field(it, k)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        match str_field(it, "type") {
            Some("tool_use") => {
                let (name, input) = tool_use(it);
                let plan = (name == "TodoWrite")
                    .then(|| it.pointer("/input/todos"))
                    .flatten()
                    .and_then(|t| moment::plan_steps(t, &["content", "activeForm"]));
                out.push(match id("id") {
                    Some(id) => Event::ToolCall { id, name, input },
                    None => Event::Part(Part::ToolCall { name, input }),
                });
                out.extend(plan.map(Event::Plan));
            }
            Some("tool_result") => out.push(match id("tool_use_id") {
                Some(id) => Event::ToolResult { id },
                None => Event::Part(Part::ToolResult {
                    text: String::new(),
                }),
            }),
            _ => {}
        }
    }
    match (str_field(v, "type"), str_field(v, "subtype")) {
        (Some("system"), Some("away_summary")) => {
            if let Some(s) = str_field(v, "content") {
                out.push(Event::Summary(s.to_owned()));
            }
        }
        (Some("system"), Some("turn_duration")) => out.push(Event::TurnEnded {
            duration: v
                .get("durationMs")
                .and_then(Value::as_u64)
                .map(Duration::from_millis),
            failed: None,
        }),
        _ => {}
    }
    out
}

/// 最后一条 `type == kind` 的行里的字符串字段 `key`（非空）。
fn last_str<'a>(lines: &'a [Value], kind: &str, key: &str) -> Option<&'a str> {
    lines
        .iter()
        .rev()
        .filter(|v| str_field(v, "type") == Some(kind))
        .find_map(|v| str_field(v, key).map(str::trim).filter(|s| !s.is_empty()))
}

/// 对话的项目目录。对话中途会换目录（cd 到别处、进 worktree），Claude 把文件存在
/// `projects/<某个 cwd 的编码>/` 下，`claude --resume` 也只在那个目录找得到它：
/// 取编码后等于文件所在目录名的 cwd（多个取最后一个）；只读头尾时可能一个都对不上（长对话中途搬过目录、
/// 结尾在项目的工作树里），再找 cwd 的上级目录里对得上的；都没有取最后一个 cwd，没有 cwd 按目录名反推。
fn project_cwd(path: &Path, head: &[Value], tail: &[Value]) -> PathBuf {
    let cwds: Vec<&Path> = head
        .iter()
        .chain(tail)
        .filter_map(|v| str_field(v, "cwd").filter(|s| !s.is_empty()))
        .map(Path::new)
        .collect();
    let Some(dir) = path.parent().and_then(Path::file_name) else {
        return cwds
            .last()
            .map_or_else(|| decode_dir(path), |c| c.to_path_buf());
    };
    let dir = dir.to_string_lossy();
    let stored_here = |p: &&Path| encode_path(p) == dir;
    cwds.iter()
        .rev()
        .copied()
        .find(stored_here)
        .or_else(|| {
            cwds.iter()
                .rev()
                .find_map(|c| c.ancestors().skip(1).find(stored_here))
        })
        .or(cwds.last().copied())
        .map_or_else(|| decode_dir(path), Path::to_path_buf)
}

/// 文件里找不到 cwd 时，从项目目录名反推（有损：`/` 和 `.` 都编码成了 `-`）。
fn decode_dir(path: &Path) -> PathBuf {
    path.parent()
        .and_then(Path::file_name)
        .map(|n| PathBuf::from(n.to_string_lossy().replace('-', "/")))
        .unwrap_or_default()
}
