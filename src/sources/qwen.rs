//! Qwen Code：`~/.qwen/projects/<编码路径>/chats/<会话 id>.jsonl`（`QWEN_HOME` 换的是 `~/.qwen`）。
//! 格式按上游 0.28.3 的 `packages/core/src/services/chatRecordingService.ts`、`sessionService.ts` 核对：
//!
//! - 项目目录名是项目路径把非字母数字都换成 `-`（和 Claude 一样，有损：不同路径可能撞名），所以项目路径
//!   取记录里的 `cwd`，不从目录名反推。归档的对话在 `chats/archive/`，不列；同目录的 `<id>.runtime.json`、
//!   `<id>.ledger.jsonl` 这类附属文件不算会话（上游只认 `^[0-9a-fA-F-]{32,36}\.jsonl$`）。
//! - 一行一条记录 `{uuid, parentUuid, sessionId, timestamp, type: user|assistant|tool_result|system, subtype?,
//!   cwd, gitBranch?, message?: {role, parts}, systemPayload?}`；`message` 是 Gemini 的 Content，
//!   文字在 `parts[].text`（`thought: true` 是思考过程，不算），工具调用 `parts[].functionCall`，
//!   工具返回是 `tool_result` 行的 `parts[].functionResponse`。
//! - 用户发言：`type: user` 且没有 `subtype`（`mid_turn_user_message` 是 AI 干活时插的话，也算），
//!   字取 `systemPayload.displayText`（屏幕上显示的那句，`parts` 里会带上注入的上下文），没有再取 `parts`。
//!   通知、定时任务、子代理消息这些带 `subtype` 的 user 行不算。
//! - 标题：最后一条 `system`/`custom_title` 的 `customTitle`（手动 `/rename` 和自动起的都在这），没有取第一句话。
//! - `/rewind` 不删记录：之后的记录 `parentUuid` 指回回退点之前，被回退的留在文件里。所以只认从最后一条
//!   沿 `parentUuid` 走得到的那些（[`branch`]）；精确计数要读整份文件走一遍，不做增量计数。
//! - 子代理的对话在 `<项目>/subagents/<会话 id>/` 下另存，不在 `chats/` 里，不列。
//! - 托管引擎（`qwen serve` 的 managed）的会话正文另存在资源目录、`qwen -r` 接不上，不列。
//! - 不是终端里开的会写 `system`/`session_source`：定时任务、聊天频道算自动运行（[`Origin`]）；
//!   终端里开的和 `qwen -p` 都不写，看不出是不是无头跑的，都算自己的对话。

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::gemini::{part_text, result_text};
use super::jsonl::{self, str_field};
use super::time::parse_rfc3339;
use super::transcript::{self, TRANSCRIPT_MAX_BYTES};
use super::window::{self, PREVIEW_MAX_BYTES, Window};
use super::{Command, Part, PreviewMessage, Role, Scan, Source, Transcript, Trash, TrashEntry};
use crate::core::{Session, SessionId, Vendor};

pub struct QwenSource {
    root: PathBuf,
    trash: Trash,
}

impl QwenSource {
    /// `root` 是 `~/.qwen/projects`。
    pub fn new(root: impl Into<PathBuf>, trash: Trash) -> Self {
        Self {
            root: root.into(),
            trash,
        }
    }
}

impl Source for QwenSource {
    fn vendor(&self) -> Vendor {
        Vendor::Qwen
    }

    fn scan(&self) -> Scan {
        super::scan_by_files(self)
    }

    fn load_preview(&self, session: &Session, n: usize) -> io::Result<Vec<PreviewMessage>> {
        last_messages(&session.path, n)
    }

    fn load_transcript(&self, session: &Session) -> io::Result<Transcript> {
        let (buf, from_start) = window::read_tail_lines(&session.path, TRANSCRIPT_MAX_BYTES)?;
        let records: Vec<Value> = window::lines(&buf).filter_map(jsonl::parse).collect();
        Ok(Transcript {
            parts: branch(&records)
                .iter()
                .flat_map(parts)
                .map(transcript::clip)
                .collect(),
            truncated: !from_start,
        })
    }

    fn count_messages(&self, session: &Session) -> io::Result<usize> {
        count_users(&session.path)
    }

    fn resume_cmd(&self, session: &Session) -> Command {
        Command::new("qwen", &["-r", session.id.as_str()], &session.cwd)
    }

    /// 首句话用 `--prompt-interactive=<话>`（上游 `-i`：发出这句后留在交互界面，可以和 `--resume` 一起用）。
    /// 不用位置参数：以 `-` 开头的话会被当成选项。
    fn resume_with_message_cmd(&self, session: &Session, message: &str) -> Command {
        let mut cmd = self.resume_cmd(session);
        cmd.args.push(format!("--prompt-interactive={message}"));
        cmd
    }

    /// 会话台新建时 host 会加 `--session-id <uuid>`（表里 `presets_new_id`）。
    fn new_cmd(&self, cwd: &Path) -> Command {
        Command::new("qwen", &[], cwd)
    }

    /// 会话文件，加上同目录 `<id>.*` 的附属文件（运行状态、标题、目标等）和子代理目录 `<项目>/subagents/<id>/`。
    fn trash(&self, session: &Session) -> io::Result<TrashEntry> {
        let mut extra = Vec::new();
        if let (Some(chats), Some(id)) = (
            session.path.parent(),
            super::sqlite::safe_id(session.id.as_str()),
        ) {
            let prefix = format!("{id}.");
            for f in fs::read_dir(chats).into_iter().flatten().flatten() {
                let p = f.path();
                let side = f
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(&prefix));
                if side && p != session.path {
                    extra.push(p);
                }
            }
            extra.sort();
            if let Some(project) = chats.parent() {
                extra.push(project.join("subagents").join(id));
            }
        }
        super::trash_files(&self.trash, session, &extra)
    }

    fn restore(&self, entry: &TrashEntry) -> io::Result<()> {
        self.trash.restore(entry)
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    /// 项目目录、`chats` 目录的建删和其中的会话文件算；附属文件、归档、子代理、工具输出不算。
    fn affects(&self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        match rel.iter().collect::<Vec<_>>().as_slice() {
            [] | [_] => true,
            [_, chats] => *chats == "chats",
            [_, chats, name] => *chats == "chats" && is_session_name(name),
            _ => false,
        }
    }

    fn session_files(&self) -> Option<io::Result<Vec<PathBuf>>> {
        Some(list_files(&self.root))
    }

    fn read_files(&self, paths: &[PathBuf]) -> Vec<io::Result<Option<Session>>> {
        jsonl::par_map(paths, read_session)
    }
}

/// 会话文件名：上游 `SESSION_FILE_PATTERN` `^[0-9a-fA-F-]{32,36}\.jsonl$`。
fn is_session_name(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|n| n.strip_suffix(".jsonl"))
        .is_some_and(|id| {
            (32..=36).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        })
}

/// 列出 `root/<项目>/chats/` 下的会话文件（不含 `chats/archive/` 里归档的）。目录不存在返回空。
fn list_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let rd = match fs::read_dir(root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", root.display()))),
    };
    let mut out = Vec::new();
    for project in rd.flatten() {
        let Ok(files) = fs::read_dir(project.path().join("chats")) else {
            continue;
        };
        out.extend(
            files
                .flatten()
                .map(|f| f.path())
                .filter(|p| p.file_name().is_some_and(is_session_name) && p.is_file()),
        );
    }
    out.sort();
    Ok(out)
}

/// 读一个会话文件的头尾。空文件、没说过话也没有标题的返回 `None`。
fn read_session(path: &Path) -> io::Result<Option<Session>> {
    let w = Window::read(path)?;
    if w.len == 0 {
        return Ok(None);
    }
    let head: Vec<Value> = w.head_lines().filter_map(jsonl::parse).collect();
    // 托管引擎（`qwen serve`）的会话正文不在这个文件里，`qwen -r` 也接不上（上游 `assertLegacySessionExecution`）。
    if head.iter().any(is_managed) {
        return Ok(None);
    }
    // 整个读进来时按分支滤；只读了头尾时，开头那段判断不了（分支的末端在后面），照单全收。
    let (head_said, tail): (Vec<PreviewMessage>, Vec<Value>) = if w.whole {
        (said_on_branch(&head), Vec::new())
    } else {
        let tail: Vec<Value> = w.tail_lines().filter_map(jsonl::parse).collect();
        (head.iter().filter_map(said).collect(), tail)
    };
    let tail_said = said_on_branch(&tail);
    let automatic = match origin(&head, &tail) {
        Origin::Hidden => return Ok(None),
        Origin::Automatic => true,
        Origin::Own => false,
    };
    let users = |ms: &[PreviewMessage]| ms.iter().filter(|m| m.role == Role::User).count();
    // 头尾不重叠，两段相加；大文件这是下限，精确值由 count_messages 补。
    let message_count = users(&head_said) + users(&tail_said);

    let newest_first = || tail.iter().rev().chain(head.iter().rev());
    let title = newest_first()
        .find_map(custom_title)
        .map(str::to_owned)
        .or_else(|| {
            head_said
                .iter()
                .chain(&tail_said)
                .find(|m| m.role == Role::User)
                .map(|m| m.text.clone())
        })
        .map(|t| jsonl::one_line(&t, 80));
    if message_count == 0 && title.is_none() {
        return Ok(None);
    }

    // 记录里的 cwd 是项目根目录（上游 `config.getProjectRoot()`），一条对话里不变。
    let Some(cwd) = head
        .iter()
        .chain(&tail)
        .find_map(|v| str_field(v, "cwd").filter(|s| !s.is_empty()))
    else {
        return Ok(None);
    };
    let git_branch = newest_first()
        .find_map(|v| str_field(v, "gitBranch").filter(|s| !s.is_empty()))
        .map(str::to_owned);
    let last_active = head_said
        .iter()
        .chain(&tail_said)
        .rev()
        .find_map(|m| m.at)
        .or_else(|| newest_first().find_map(|v| str_field(v, "timestamp").and_then(parse_rfc3339)))
        .unwrap_or(w.mtime);

    let id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Some(Session {
        vendor: Vendor::Qwen,
        id: SessionId::from(id),
        cwd: PathBuf::from(cwd),
        path: path.to_path_buf(),
        title,
        git_branch,
        last_active,
        message_count,
        automatic,
    }))
}

fn is_managed(v: &Value) -> bool {
    match str_field(v, "subtype") {
        Some("session_execution_engine") => {
            v.pointer("/systemPayload/engine").and_then(Value::as_str) == Some("managed")
        }
        Some(s) => s.starts_with("managed_session_"),
        None => false,
    }
}

/// 对话是谁起的。
#[derive(Debug, PartialEq, Eq)]
enum Origin {
    /// 终端里自己开的（终端里起的不写 `session_source`），或编辑器插件、网页、分支出来的。
    Own,
    /// 定时任务、聊天频道机器人起的。
    Automatic,
    /// `agent`、`agent-host`：上游自己的列表也不列（`HIDDEN_CATALOG_SOURCE_TYPES`）。
    Hidden,
}

/// 看 `system`/`session_source` 行：上游取开头那几行里的第一条，没有再取结尾的最后一条。
fn origin(head: &[Value], tail: &[Value]) -> Origin {
    let source = |v: &Value| -> Option<(String, Option<String>)> {
        if str_field(v, "subtype") != Some("session_source") {
            return None;
        }
        let p = v.get("systemPayload")?;
        Some((
            str_field(p, "sourceType")?.to_owned(),
            str_field(p, "sourceId").map(str::to_owned),
        ))
    };
    let found = head
        .iter()
        .find_map(source)
        .or_else(|| tail.iter().rev().find_map(source));
    let Some((kind, id)) = found else {
        return Origin::Own;
    };
    match kind.as_str() {
        "agent" | "agent-host" => Origin::Hidden,
        "scheduled_task" | "channel" => Origin::Automatic,
        // 定时任务每次跑也记成 `default`，靠 id 前缀认（上游 `acp-bridge/src/session-source.ts`）。
        "default" if id.is_some_and(|i| i.starts_with("scheduled_task_run:")) => Origin::Automatic,
        _ => Origin::Own,
    }
}

/// `system`/`custom_title` 行里的标题。
fn custom_title(v: &Value) -> Option<&str> {
    if str_field(v, "subtype") != Some("custom_title") {
        return None;
    }
    v.pointer("/systemPayload/customTitle")
        .or_else(|| v.get("customTitle"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// 一行里的对话消息：用户自己说的话或 AI 的文字；工具、通知、系统事件返回 `None`。
fn said(v: &Value) -> Option<PreviewMessage> {
    if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let role = match (str_field(v, "type")?, str_field(v, "subtype")) {
        // 插件（Mod）替你发的话，上游显示成通知，不算。
        ("user", _) if str_field(v, "provenance") == Some("mod_prompt") => return None,
        ("user", None | Some("mid_turn_user_message")) => Role::User,
        ("assistant", None) => Role::Assistant,
        _ => return None,
    };
    let display = (role == Role::User)
        .then(|| v.pointer("/systemPayload/displayText"))
        .flatten();
    let text = match display {
        // 有 displayText 就以它为准，空串说明这句不是给人看的（只发给模型的上下文）。
        Some(d) => Some(d.as_str()?.trim())
            .filter(|s| !s.is_empty())?
            .to_owned(),
        None => v.pointer("/message/parts").and_then(part_text)?,
    };
    Some(PreviewMessage {
        role,
        text,
        at: str_field(v, "timestamp").and_then(parse_rfc3339),
    })
}

fn said_on_branch(records: &[Value]) -> Vec<PreviewMessage> {
    branch(records).iter().filter_map(said).collect()
}

/// 整段对话里的一行：消息（同 [`said`]），加上 AI 的工具调用和 `tool_result` 行里的工具返回。
fn parts(v: &Value) -> Vec<Part> {
    let mut out: Vec<Part> = said(v).map(Part::Message).into_iter().collect();
    if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return out;
    }
    let items = v.pointer("/message/parts");
    match str_field(v, "type") {
        Some("assistant") => {
            for call in items
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|p| p.get("functionCall"))
            {
                out.push(Part::ToolCall {
                    name: str_field(call, "name").unwrap_or("工具").to_owned(),
                    input: transcript::input_summary(call.get("args").unwrap_or(&Value::Null)),
                });
            }
        }
        Some("tool_result") => {
            let display = v
                .pointer("/toolCallResult/resultDisplay")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            if let Some(text) = display.or_else(|| items.and_then(result_text)) {
                out.push(Part::ToolResult { text });
            }
        }
        _ => {}
    }
    out
}

/// 一段连续记录里当前分支上的那些，按先后，同一个 `uuid` 分几行写的（流式回复的分片）合成一条
/// （`message.parts` 接起来，时间取最晚的），照上游 `prepareTranscriptRecords`。
///
/// 每条记录的 `parentUuid` 指它写下时的上一条；`/rewind` 把之后记录的父节点换回回退点之前，被回退的记录留在文件里。
/// 从最后一条对话记录沿 `parentUuid` 往回走，走到的就是当前分支。文件只追加、祖先一定在前面，
/// 所以只读了文件后面一段时，这段里没走到的也都是被回退掉的（父节点在这段前面时链在这里断开，不影响判断）。
fn branch(records: &[Value]) -> Vec<Value> {
    let links: Vec<Link<'_>> = records.iter().map(Link::of).collect();
    let keep = walk(&links);
    let mut out: Vec<Value> = Vec::new();
    let mut at: HashMap<&str, usize> = HashMap::new();
    for ((v, l), k) in records.iter().zip(&links).zip(keep) {
        if !k {
            continue;
        }
        let Some(u) = l.uuid else { continue };
        match at.get(u) {
            Some(&i) => merge_fragment(&mut out[i], v),
            None => {
                at.insert(u, out.len());
                out.push(v.clone());
            }
        }
    }
    out
}

fn merge_fragment(base: &mut Value, more: &Value) {
    if let Some(extra) = more.pointer("/message/parts").and_then(Value::as_array) {
        match base
            .pointer_mut("/message/parts")
            .and_then(Value::as_array_mut)
        {
            Some(parts) => parts.extend(extra.iter().cloned()),
            None => {
                if let Some(m) = more.get("message") {
                    base["message"] = m.clone();
                }
            }
        }
    }
    if let (Some(t), Some(obj)) = (str_field(more, "timestamp"), base.as_object_mut()) {
        let later = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .is_none_or(|b| t > b);
        if later {
            obj.insert("timestamp".into(), Value::from(t));
        }
    }
    if base.get("toolCallResult").is_none()
        && let (Some(r), Some(obj)) = (more.get("toolCallResult"), base.as_object_mut())
    {
        obj.insert("toolCallResult".into(), r.clone());
    }
}

/// 只看父子关系用的一行。
struct Link<'a> {
    uuid: Option<&'a str>,
    parent: Option<&'a str>,
    /// 能当分支末端：快照、界面状态、托管引擎日志这类不算对话的记录不行（上游 `isTranscriptConversationRecord`）。
    leaf: bool,
}

impl<'a> Link<'a> {
    fn of(v: &'a Value) -> Self {
        Self {
            uuid: str_field(v, "uuid"),
            parent: str_field(v, "parentUuid"),
            leaf: str_field(v, "subtype").is_none_or(|s| !NOT_LEAF.contains(&s)),
        }
    }
}

const NOT_LEAF: [&str; 7] = [
    "session_artifact_event",
    "session_artifact_snapshot",
    "session_sources_snapshot",
    "generated_ui_state",
    "managed_session_header_v1",
    "managed_session_event_v1",
    "managed_session_commit_v1",
];

/// 每行在不在当前分支上：从最后一条能当末端的记录沿父节点往回走，走到的 `uuid` 的各行都算。
fn walk(links: &[Link<'_>]) -> Vec<bool> {
    let mut first: HashMap<&str, usize> = HashMap::new();
    for (i, l) in links.iter().enumerate() {
        if let Some(u) = l.uuid {
            first.entry(u).or_insert(i);
        }
    }
    let mut chain: HashSet<&str> = HashSet::new();
    let mut at = links
        .iter()
        .rposition(|l| l.leaf && l.uuid.is_some())
        .and_then(|i| links[i].uuid);
    while let Some(u) = at {
        if !chain.insert(u) {
            break;
        }
        at = first
            .get(u)
            .and_then(|&i| links[i].parent)
            .filter(|p| first.contains_key(p));
    }
    links
        .iter()
        .map(|l| l.uuid.is_some_and(|u| chain.contains(u)))
        .collect()
}

/// 最后 `n` 条消息（当前分支上的）。从文件尾往回读，范围从 256KB 起每次乘 4，最多 8MB。
fn last_messages(path: &Path, n: usize) -> io::Result<Vec<PreviewMessage>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = 256 * 1024;
    loop {
        let (buf, from_start) = window::read_tail_lines(path, bytes)?;
        let records: Vec<Value> = window::lines(&buf).filter_map(jsonl::parse).collect();
        let said = said_on_branch(&records);
        if said.len() >= n || from_start || bytes >= PREVIEW_MAX_BYTES {
            let skip = said.len().saturating_sub(n);
            return Ok(said.into_iter().skip(skip).map(jsonl::clip).collect());
        }
        bytes = (bytes * 4).min(PREVIEW_MAX_BYTES);
    }
}

/// 整份文件里当前分支上的用户发言条数（同一个 `uuid` 只算一次）。每行只解出父子关系用的几个字段，
/// 像用户行的才整行解析。
fn count_users(path: &Path) -> io::Result<usize> {
    #[derive(Deserialize)]
    struct Ids {
        uuid: Option<String>,
        #[serde(rename = "parentUuid")]
        parent: Option<String>,
        subtype: Option<String>,
    }
    let mut rows: Vec<(Ids, bool)> = Vec::new();
    let mut file = fs::File::open(path)?;
    window::for_each_line_from(&mut file, 0, |line, _| {
        let Ok(ids) = serde_json::from_slice::<Ids>(line) else {
            return;
        };
        let user = contains(line, br#""type":"user""#)
            && jsonl::parse(line)
                .as_ref()
                .and_then(said)
                .is_some_and(|m| m.role == Role::User);
        rows.push((ids, user));
    })?;
    let links: Vec<Link<'_>> = rows
        .iter()
        .map(|(r, _)| Link {
            uuid: r.uuid.as_deref(),
            parent: r.parent.as_deref(),
            leaf: r.subtype.as_deref().is_none_or(|s| !NOT_LEAF.contains(&s)),
        })
        .collect();
    let users: HashSet<&str> = walk(&links)
        .into_iter()
        .zip(&rows)
        .zip(&links)
        .filter(|((k, (_, user)), _)| *k && *user)
        .filter_map(|(_, l)| l.uuid)
        .collect();
    Ok(users.len())
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(uuid: &str, parent: Option<&str>, kind: &str, text: &str) -> Value {
        let role = if kind == "assistant" { "model" } else { "user" };
        json!({"uuid": uuid, "parentUuid": parent, "sessionId": "s", "timestamp": "2026-11-20T10:00:00Z",
               "type": kind, "cwd": "/p", "message": {"role": role, "parts": [{"text": text}]}})
    }

    fn texts(records: &[Value]) -> Vec<String> {
        said_on_branch(records)
            .into_iter()
            .map(|m| m.text)
            .collect()
    }

    #[test]
    fn rewound_records_are_dropped() {
        // u1 a1 u2 a2，回退到 u2 之前（rewind 的父节点是 a1），再说 u3。
        let records = [
            rec("u1", None, "user", "一"),
            rec("a1", Some("u1"), "assistant", "答一"),
            rec("u2", Some("a1"), "user", "二"),
            rec("a2", Some("u2"), "assistant", "答二"),
            json!({"uuid": "r", "parentUuid": "a1", "type": "system", "subtype": "rewind"}),
            rec("u3", Some("r"), "user", "三"),
            json!({"uuid": "t", "parentUuid": "u3", "type": "system", "subtype": "ui_telemetry"}),
        ];
        assert_eq!(texts(&records), ["一", "答一", "三"]);
        // 只读到后面一段（u1 不在）：没走到的 u2、a2 照样算回退掉的。
        assert_eq!(texts(&records[1..]), ["答一", "三"]);
    }

    #[test]
    fn walk_survives_cycles_and_skips_non_conversation_leaf() {
        let l = |uuid, parent, leaf| Link { uuid, parent, leaf };
        let links = [
            l(Some("a"), Some("b"), true),
            l(Some("b"), Some("a"), true),
            l(None, Some("b"), true),
            // 最后一条是界面状态快照，不当末端；它自己也不在分支上。
            l(Some("ui"), Some("x"), false),
        ];
        assert_eq!(walk(&links), [true, true, false, false]);
        assert!(walk(&[]).is_empty());
    }

    #[test]
    fn fragments_with_same_uuid_are_merged() {
        let records = [
            rec("u1", None, "user", "说个笑话"),
            json!({"uuid": "a1", "parentUuid": "u1", "type": "assistant", "timestamp": "2026-11-20T10:00:01Z",
                   "message": {"role": "model", "parts": [{"text": "从前"}]}}),
            json!({"uuid": "a1", "parentUuid": "u1", "type": "assistant", "timestamp": "2026-11-20T10:00:05Z",
                   "message": {"role": "model", "parts": [{"text": "有座山"}]}}),
        ];
        let said = said_on_branch(&records);
        assert_eq!(said.len(), 2);
        assert_eq!(said[1].text, "从前有座山");
        assert_eq!(said[1].at, parse_rfc3339("2026-11-20T10:00:05Z"));
    }

    #[test]
    fn origin_from_session_source() {
        let src = |kind: &str, id: Option<&str>| {
            json!({"uuid": "s", "type": "system", "subtype": "session_source",
                   "systemPayload": {"sourceType": kind, "sourceId": id}})
        };
        assert_eq!(origin(&[], &[]), Origin::Own);
        assert_eq!(origin(&[src("vscode", None)], &[]), Origin::Own);
        assert_eq!(origin(&[src("side_task", Some("p"))], &[]), Origin::Own);
        assert_eq!(origin(&[src("channel", None)], &[]), Origin::Automatic);
        assert_eq!(
            origin(&[], &[src("scheduled_task", None)]),
            Origin::Automatic
        );
        assert_eq!(
            origin(&[src("default", Some("scheduled_task_run:42"))], &[]),
            Origin::Automatic
        );
        assert_eq!(origin(&[src("default", Some("web"))], &[]), Origin::Own);
        assert_eq!(origin(&[src("agent-host", None)], &[]), Origin::Hidden);
        // 开头的第一条算数。
        assert_eq!(
            origin(&[src("vscode", None), src("channel", None)], &[]),
            Origin::Own
        );
    }

    #[test]
    fn user_text_prefers_display_text() {
        let mut v = rec("u", None, "user", "<context>注入</context>\n修登录");
        v["systemPayload"] = json!({"displayText": "修登录"});
        assert_eq!(said(&v).unwrap().text, "修登录");
        v["systemPayload"] = json!({"displayText": ""});
        assert!(said(&v).is_none(), "displayText 是空串的不是给人看的");
        let mut n = rec("n", None, "user", "后台任务做完了");
        n["subtype"] = json!("notification");
        assert!(said(&n).is_none());
        let mut mid = rec("m", None, "user", "x");
        mid["subtype"] = json!("mid_turn_user_message");
        mid["systemPayload"] = json!({"displayText": "顺便改下文案"});
        assert_eq!(said(&mid).unwrap().text, "顺便改下文案");
    }

    #[test]
    fn thoughts_and_tool_calls_are_not_text() {
        let v = json!({"uuid": "a", "type": "assistant", "message": {"role": "model", "parts": [
            {"text": "先想想", "thought": true},
            {"functionCall": {"name": "run_shell_command", "args": {"command": "ls"}}}
        ]}});
        assert!(said(&v).is_none());
        assert_eq!(
            parts(&v),
            [Part::ToolCall {
                name: "run_shell_command".into(),
                input: "ls".into()
            }]
        );
    }

    #[test]
    fn session_names() {
        let ok = |n: &str| is_session_name(OsStr::new(n));
        assert!(ok("0b5c3c1e-1111-4222-8333-944455566677.jsonl"));
        assert!(ok("0b5c3c1e111142228333944455566677.jsonl"));
        assert!(!ok("0b5c3c1e-1111-4222-8333-944455566677.ledger.jsonl"));
        assert!(!ok("0b5c3c1e-1111-4222-8333-944455566677.runtime.json"));
        assert!(!ok("notes.jsonl"));
    }
}
