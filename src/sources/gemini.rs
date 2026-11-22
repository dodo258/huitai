//! Gemini CLI：`~/.gemini/tmp/<项目>/chats/session-<时间>-<id 前 8 位>.jsonl`（`GEMINI_CLI_HOME` 换的是家目录）。
//! 格式按上游 0.70.0 的 `packages/core/src/services/chatRecordingService.ts`、`chatRecordingTypes.ts` 核对：
//!
//! - 第一行元数据 `{sessionId, projectHash, startTime, lastUpdated, kind}`；之后一行一条消息
//!   `{id, timestamp, type: user|gemini|info|error|warning, content, displayContent?, toolCalls?}`，中间夹控制行：
//!   `$set` 改元数据（标题 `summary` 在这里；更早的版本带整份 `messages`）、`$rewindTo` 回退（这条和之后的都作废，
//!   id 找不到就全作废）、`$patch` 改内容、补工具结果、删、重排。同一个 id 再出现时替换原来那条、位置不变。
//!   读法照上游 `createJsonlRecordAccumulator`（[`Replay`]）。
//! - 0.40 以前整份写成一个对象 `session-*.json`，上游现在还认；恢复时它迁成同名 `.jsonl`、旧文件留着，有 `.jsonl` 就只认它。
//! - 项目目录名：0.30 起是短名，目录里的 `.project_root` 写着项目路径，`~/.gemini/projects.json` 也记着
//!   `{"projects": {路径: 短名}}`。以前是项目路径的 sha256：新版在那个项目里启动时把它复制成短名目录（旧的不删），
//!   没复制的那些（短名目录先有了内容）新版 `--resume` 也找不到，所以 sha256 目录认出来就跳过，不重复列。
//! - 子代理的会话在 `chats/<父会话 id>/` 下、元数据 `kind: "subagent"`，不列。
//! - 用户发言：`/`、`?` 开头的命令和 `<session_context>`、`<hook_context>` 注入不算（上游 `isIgnoredUserContent`）；
//!   工具返回也记成 user 消息，只有 `functionResponse`、没有字，也不算。
//! - 看不出是不是 `gemini -p` 无头跑的（会话文件里没有这一项），都算自己的对话。
//!
//! 精确计数要按控制行回放整份文件（回退会删掉前面的消息），不能只数新增的行，所以不做增量计数。

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Map, Value};

use super::jsonl::{self, str_field};
use super::time::parse_rfc3339;
use super::transcript::{self, TRANSCRIPT_MAX_BYTES};
use super::window::{self, PREVIEW_MAX_BYTES, Window};
use super::{Command, Part, PreviewMessage, Role, Scan, Source, Transcript, Trash, TrashEntry};
use crate::core::{Session, SessionId, Vendor};

/// 旧版整份 `.json` 会话最多读多大（要整个读进内存解析）。
const LEGACY_MAX_BYTES: u64 = 32 * 1024 * 1024;

pub struct GeminiSource {
    root: PathBuf,
    trash: Trash,
}

impl GeminiSource {
    /// `root` 是 `~/.gemini/tmp`，`projects.json` 在它上一层。
    pub fn new(root: impl Into<PathBuf>, trash: Trash) -> Self {
        Self {
            root: root.into(),
            trash,
        }
    }
}

impl Source for GeminiSource {
    fn vendor(&self) -> Vendor {
        Vendor::Gemini
    }

    fn scan(&self) -> Scan {
        super::scan_by_files(self)
    }

    fn load_preview(&self, session: &Session, n: usize) -> io::Result<Vec<PreviewMessage>> {
        last_messages(&session.path, n)
    }

    fn load_transcript(&self, session: &Session) -> io::Result<Transcript> {
        let (r, truncated) = if is_legacy(&session.path) {
            (replay_file(&session.path, true)?, false)
        } else {
            let (buf, from_start) = window::read_tail_lines(&session.path, TRANSCRIPT_MAX_BYTES)?;
            (replay_lines(window::lines(&buf), true), !from_start)
        };
        let mut parts = Vec::new();
        for m in r.msgs.iter().map(|(_, m)| m) {
            parts.extend(m.said().map(Part::Message));
            for t in &m.tools {
                parts.push(Part::ToolCall {
                    name: t.name.clone(),
                    input: t.input.clone(),
                });
                if let Some(text) = &t.output {
                    parts.push(Part::ToolResult { text: text.clone() });
                }
            }
        }
        Ok(Transcript {
            parts: parts.into_iter().map(transcript::clip).collect(),
            truncated,
        })
    }

    fn count_messages(&self, session: &Session) -> io::Result<usize> {
        Ok(replay_file(&session.path, false)?.users())
    }

    fn resume_cmd(&self, session: &Session) -> Command {
        Command::new("gemini", &["--resume", session.id.as_str()], &session.cwd)
    }

    /// 首句话用 `--prompt-interactive=<话>`：位置参数在真终端里也当交互首句，但以 `-` 开头会被当成选项，
    /// 第一个词是 `config` 这类会被当成子命令报错（上游 `config.ts` 的 `isInvalidConfigSubcommand`）。
    fn resume_with_message_cmd(&self, session: &Session, message: &str) -> Command {
        let mut cmd = self.resume_cmd(session);
        cmd.args.push(format!("--prompt-interactive={message}"));
        cmd
    }

    /// 会话台新建时 host 会加 `--session-id <uuid>`（表里 `presets_new_id`，上游 0.45 起有这个选项）。
    fn new_cmd(&self, cwd: &Path) -> Command {
        Command::new("gemini", &[], cwd)
    }

    /// 会话文件，加上迁移前的同名 `.json`（不一起移走它会重新冒出来）和子代理目录 `chats/<id>/`。
    fn trash(&self, session: &Session) -> io::Result<TrashEntry> {
        let mut extra = Vec::new();
        if session.path.extension() == Some(OsStr::new("jsonl")) {
            extra.push(session.path.with_extension("json"));
        }
        if let (Some(chats), Some(id)) = (
            session.path.parent(),
            super::sqlite::safe_id(session.id.as_str()),
        ) {
            extra.push(chats.join(id));
        }
        super::trash_files(&self.trash, session, &extra)
    }

    fn restore(&self, entry: &TrashEntry) -> io::Result<()> {
        self.trash.restore(entry)
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    /// 项目目录、`chats` 目录的建删和其中的会话文件算；日志、工具输出、计划、子代理会话不算。
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
        let projects = projects(&self.root);
        jsonl::par_map(paths, |p| read_session(&self.root, &projects, p))
    }
}

/// `session-*.jsonl`，或旧版的 `session-*.json`。
fn is_session_name(name: &OsStr) -> bool {
    name.to_str().is_some_and(|n| {
        n.starts_with("session-") && (n.ends_with(".jsonl") || n.ends_with(".json"))
    })
}

fn is_legacy(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("json"))
}

/// 旧版 `.json` 恢复时迁成了同名 `.jsonl`：只认新的。
fn superseded(path: &Path) -> bool {
    is_legacy(path) && path.with_extension("jsonl").exists()
}

/// 0.30 以前的项目目录名：项目路径的 sha256（64 位小写十六进制）。
fn is_hash_dir(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 列出 `root/<项目>/chats/` 下的会话文件，跳过旧版 sha256 目录和已迁成 `.jsonl` 的旧文件。目录不存在返回空。
fn list_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let rd = match fs::read_dir(root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", root.display()))),
    };
    let mut out = Vec::new();
    for project in rd.flatten() {
        if project.file_name().to_str().is_none_or(is_hash_dir) {
            continue;
        }
        let Ok(files) = fs::read_dir(project.path().join("chats")) else {
            continue;
        };
        out.extend(files.flatten().map(|f| f.path()).filter(|p| {
            p.file_name().is_some_and(is_session_name) && !superseded(p) && p.is_file()
        }));
    }
    out.sort();
    Ok(out)
}

/// 项目目录名 → 项目路径：目录里的 `.project_root` 优先（上游拿它认目录归谁），没有再查 `projects.json`。
fn projects(root: &Path) -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    let registry = root
        .parent()
        .and_then(|g| fs::read(g.join("projects.json")).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    if let Some(map) = registry
        .as_ref()
        .and_then(|v| v.get("projects"))
        .and_then(Value::as_object)
    {
        for (path, slug) in map {
            if let Some(slug) = slug.as_str() {
                out.insert(slug.to_owned(), PathBuf::from(path));
            }
        }
    }
    for d in fs::read_dir(root).into_iter().flatten().flatten() {
        let Ok(owner) = fs::read_to_string(d.path().join(".project_root")) else {
            continue;
        };
        let owner = owner.trim();
        if let (Some(name), false) = (d.file_name().to_str(), owner.is_empty()) {
            out.insert(name.to_owned(), PathBuf::from(owner));
        }
    }
    out
}

/// `path` 是 `root/<项目>/chats/<会话文件>` 时返回项目目录名。
fn project_dir_name<'a>(root: &Path, path: &'a Path) -> Option<&'a str> {
    let rel = path.strip_prefix(root).ok()?;
    match rel.iter().collect::<Vec<_>>().as_slice() {
        [dir, chats, name] if *chats == "chats" && is_session_name(name) => dir.to_str(),
        _ => None,
    }
}

/// 读一个会话文件（大文件只读头尾）。不是可列出的对话（子代理、旧目录里的副本、没说过话的）返回 `None`。
fn read_session(
    root: &Path,
    projects: &HashMap<String, PathBuf>,
    path: &Path,
) -> io::Result<Option<Session>> {
    let Some(dir) = project_dir_name(root, path) else {
        return Ok(None);
    };
    if is_hash_dir(dir) || superseded(path) {
        return Ok(None);
    }
    let cwd = projects.get(dir).cloned().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "认不出项目目录：没有 .project_root，projects.json 里也没有",
        )
    })?;

    let (head, tail, mtime) = if is_legacy(path) {
        (replay_file(path, false)?, None, super::mtime_or_epoch(path))
    } else {
        let w = Window::read(path)?;
        if w.len == 0 {
            return Ok(None);
        }
        let head = replay_lines(w.head_lines(), false);
        let tail = (!w.whole).then(|| replay_lines(w.tail_lines(), false));
        (head, tail, w.mtime)
    };
    let parts: Vec<&Replay> = tail.iter().chain([&head]).collect();
    // 元数据后写的覆盖先写的：先看尾部。
    let meta = |k: &str| parts.iter().find_map(|r| r.meta_str(k));

    let Some(id) = meta("sessionId") else {
        return Ok(None);
    };
    if meta("kind") == Some("subagent") {
        return Ok(None);
    }
    // 头尾不重叠，两段相加；大文件这是下限，精确值由 count_messages 补。
    let message_count = parts.iter().map(|r| r.users()).sum();
    let title = meta("summary")
        .map(str::to_owned)
        .or_else(|| {
            [Some(&head), tail.as_ref()]
                .into_iter()
                .flatten()
                .flat_map(|r| r.msgs.iter())
                .flat_map(|(_, m)| m.said())
                .find(|m| m.role == Role::User)
                .map(|m| m.text)
        })
        .map(|t| jsonl::one_line(&t, 80));
    if message_count == 0 && title.is_none() {
        return Ok(None);
    }
    // CLI 自己的提示（切模型等）不算动静。
    let last_message = parts
        .iter()
        .find_map(|r| r.msgs.iter().rev().find_map(|(_, m)| m.role.and(m.at)));
    let last_active = [meta("lastUpdated").and_then(parse_rfc3339), last_message]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(mtime);

    Ok(Some(Session {
        vendor: Vendor::Gemini,
        id: SessionId::from(id),
        cwd,
        path: path.to_path_buf(),
        title,
        git_branch: None,
        last_active,
        message_count,
        automatic: false,
    }))
}

/// 最后 `n` 条消息。从文件尾往回读，读的范围从 256KB 起每次乘 4，最多 8MB；读到的那段照控制行回放。
fn last_messages(path: &Path, n: usize) -> io::Result<Vec<PreviewMessage>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let take_last = |r: Replay| {
        let said: Vec<PreviewMessage> = r.msgs.into_iter().flat_map(|(_, m)| m.said()).collect();
        let skip = said.len().saturating_sub(n);
        said.into_iter().skip(skip).map(jsonl::clip).collect()
    };
    if is_legacy(path) {
        return Ok(take_last(replay_file(path, false)?));
    }
    let mut bytes = 256 * 1024;
    loop {
        let (buf, from_start) = window::read_tail_lines(path, bytes)?;
        let r = replay_lines(window::lines(&buf), false);
        let found = r.msgs.iter().filter(|(_, m)| m.said().is_some()).count();
        if found >= n || from_start || bytes >= PREVIEW_MAX_BYTES {
            return Ok(take_last(r));
        }
        bytes = (bytes * 4).min(PREVIEW_MAX_BYTES);
    }
}

fn replay_lines<'a>(lines: impl Iterator<Item = &'a [u8]>, body: bool) -> Replay {
    let mut r = Replay::new(body);
    for v in lines.filter_map(jsonl::parse) {
        r.line(&v);
    }
    r
}

/// 整份文件回放。
fn replay_file(path: &Path, body: bool) -> io::Result<Replay> {
    let mut r = Replay::new(body);
    if is_legacy(path) {
        if fs::metadata(path)?.len() > LEGACY_MAX_BYTES {
            return Err(io::Error::other("旧版会话文件超过 32MB，不读"));
        }
        let v: Value = serde_json::from_slice(&fs::read(path)?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        r.line(&v);
        return Ok(r);
    }
    let mut file = fs::File::open(path)?;
    window::for_each_line_from(&mut file, 0, |line, _| {
        if let Some(v) = jsonl::parse(line) {
            r.line(&v);
        }
    })?;
    Ok(r)
}

/// 按上游的规则把一行行记录放成「现在的」元数据和消息。只读了文件一段时也照放：回退到这段里没有的 id，
/// 说明那条在这段前面，这段里它之后的都作废，和找不到时全作废结果一样。
struct Replay {
    meta: Map<String, Value>,
    msgs: Vec<(String, Msg)>,
    pos: HashMap<String, usize>,
    /// 留工具调用（整段对话视图要）；扫描、计数、预览不留。
    body: bool,
}

impl Replay {
    fn new(body: bool) -> Self {
        Self {
            meta: Map::new(),
            msgs: Vec::new(),
            pos: HashMap::new(),
            body,
        }
    }

    /// 判断顺序同上游：回退、补丁、消息（有字符串 `id`）、`$set`、元数据行（有 `sessionId` 和 `projectHash`）。
    fn line(&mut self, v: &Value) {
        if let Some(id) = str_field(v, "$rewindTo") {
            self.rewind(id);
        } else if let Some(p) = v.get("$patch").filter(|p| p.is_object()) {
            self.patch(p);
        } else if let Some(id) = str_field(v, "id") {
            self.put(id, v);
        } else if let Some(set) = v.get("$set").and_then(Value::as_object) {
            if let Some(all) = set.get("messages").and_then(Value::as_array) {
                self.msgs.clear();
                self.pos.clear();
                self.put_all(all);
            }
            self.merge(set);
        } else if let (Some(_), Some(_), Some(obj)) = (
            str_field(v, "sessionId"),
            str_field(v, "projectHash"),
            v.as_object(),
        ) {
            if let Some(all) = obj.get("messages").and_then(Value::as_array) {
                self.put_all(all);
            }
            self.merge(obj);
        }
    }

    fn merge(&mut self, m: &Map<String, Value>) {
        for (k, v) in m.iter().filter(|(k, _)| *k != "messages") {
            self.meta.insert(k.clone(), v.clone());
        }
    }

    fn meta_str(&self, key: &str) -> Option<&str> {
        self.meta
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    fn put_all(&mut self, all: &[Value]) {
        for m in all {
            if let Some(id) = str_field(m, "id") {
                self.put(id, m);
            }
        }
    }

    /// 新 id 接在最后；已有的 id 整条替换、位置不变。
    fn put(&mut self, id: &str, v: &Value) {
        let m = Msg::new(v, self.body);
        match self.pos.get(id) {
            Some(&i) => self.msgs[i].1 = m,
            None => {
                self.pos.insert(id.to_owned(), self.msgs.len());
                self.msgs.push((id.to_owned(), m));
            }
        }
    }

    fn rewind(&mut self, id: &str) {
        let at = self.pos.get(id).copied().unwrap_or(0);
        self.msgs.truncate(at);
        self.reindex();
    }

    fn patch(&mut self, p: &Value) {
        if str_field(p, "id").is_some() {
            self.patch_one(p);
        }
        for u in p
            .get("updates")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            self.patch_one(u);
        }
        if let Some(ids) = p.get("removeIds").and_then(Value::as_array) {
            let gone: HashSet<&str> = ids.iter().filter_map(Value::as_str).collect();
            self.msgs.retain(|(id, _)| !gone.contains(id.as_str()));
            self.reindex();
        }
        if let Some(ids) = p.get("orderIds").and_then(Value::as_array) {
            // 列出的按给的顺序排到最后，没列出的按原来的顺序留在前面。
            let mut listed: Vec<&str> = Vec::new();
            for id in ids.iter().filter_map(Value::as_str) {
                if self.pos.contains_key(id) && !listed.contains(&id) {
                    listed.push(id);
                }
            }
            let listed: Vec<String> = listed.into_iter().map(str::to_owned).collect();
            let mut by_id: HashMap<String, Msg> = HashMap::new();
            let mut kept = Vec::new();
            for (id, m) in self.msgs.drain(..) {
                if listed.contains(&id) {
                    by_id.insert(id, m);
                } else {
                    kept.push((id, m));
                }
            }
            kept.extend(
                listed
                    .into_iter()
                    .filter_map(|id| by_id.remove(&id).map(|m| (id, m))),
            );
            self.msgs = kept;
            self.reindex();
        }
    }

    /// 改一条：`content` 换掉（`null` 也算），`toolCalls[{id, result}]` 补到同 id 的工具调用上。
    fn patch_one(&mut self, u: &Value) {
        let Some(&i) = str_field(u, "id").and_then(|id| self.pos.get(id)) else {
            return;
        };
        let m = &mut self.msgs[i].1;
        if let Some(c) = u.get("content") {
            m.content = part_text(c);
        }
        for tc in u
            .get("toolCalls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(id), Some(result)) = (str_field(tc, "id"), tc.get("result")) else {
                continue;
            };
            if let Some(t) = m.tools.iter_mut().find(|t| t.id == id) {
                t.output = result_text(result).or(t.output.take());
            }
        }
    }

    fn reindex(&mut self) {
        self.pos = self
            .msgs
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (id.clone(), i))
            .collect();
    }

    /// 用户发言条数。
    fn users(&self) -> usize {
        self.msgs
            .iter()
            .filter(|(_, m)| m.said().is_some_and(|p| p.role == Role::User))
            .count()
    }
}

/// 回放后的一条消息。
struct Msg {
    /// `user` / `gemini`；`info`、`error`、`warning` 是 CLI 自己的提示，`None`。
    role: Option<Role>,
    content: Option<String>,
    /// 用户消息界面上显示的那份（`@文件` 展开前），有就优先用。
    display: Option<String>,
    at: Option<SystemTime>,
    tools: Vec<Tool>,
}

struct Tool {
    id: String,
    name: String,
    input: String,
    output: Option<String>,
}

impl Msg {
    fn new(v: &Value, body: bool) -> Self {
        let role = match str_field(v, "type") {
            Some("user") => Some(Role::User),
            Some("gemini") => Some(Role::Assistant),
            _ => None,
        };
        let tools = match v.get("toolCalls").and_then(Value::as_array) {
            Some(calls) if body => calls.iter().map(Tool::new).collect(),
            _ => Vec::new(),
        };
        Self {
            role,
            content: v.get("content").and_then(part_text),
            display: v.get("displayContent").and_then(part_text),
            at: str_field(v, "timestamp").and_then(parse_rfc3339),
            tools,
        }
    }

    /// 算作对话的消息：用户说的话（命令、注入、工具返回不算）或 AI 的文字。
    fn said(&self) -> Option<PreviewMessage> {
        let role = self.role?;
        let text = match role {
            Role::User => self.display.as_ref().or(self.content.as_ref())?,
            Role::Assistant => self.content.as_ref()?,
        };
        if role == Role::User && is_ignored(text) {
            return None;
        }
        Some(PreviewMessage {
            role,
            text: text.clone(),
            at: self.at,
        })
    }
}

impl Tool {
    fn new(t: &Value) -> Self {
        let output = str_field(t, "resultDisplay")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| t.get("result").and_then(result_text));
        Self {
            id: str_field(t, "id").unwrap_or_default().to_owned(),
            name: str_field(t, "displayName")
                .or_else(|| str_field(t, "name"))
                .unwrap_or("工具")
                .to_owned(),
            input: transcript::input_summary(t.get("args").unwrap_or(&Value::Null)),
            output,
        }
    }
}

/// 上游 `isIgnoredUserContent`：斜杠命令、`?` 帮助、注入的上下文。
fn is_ignored(text: &str) -> bool {
    ["/", "?", "<session_context>", "<hook_context>"]
        .iter()
        .any(|p| text.starts_with(p))
}

/// `PartListUnion`（字符串、一个 Part、或它们的数组）里的文字，思考过程不算；没有字返回 `None`。
pub(crate) fn part_text(v: &Value) -> Option<String> {
    let text = match v {
        Value::Array(items) => items.iter().filter_map(part_str).collect::<String>(),
        v => part_str(v)?.to_owned(),
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn part_str(p: &Value) -> Option<&str> {
    match p {
        Value::String(s) => Some(s),
        _ if p.get("thought").and_then(Value::as_bool) == Some(true) => None,
        _ => str_field(p, "text"),
    }
}

/// 工具结果：`[{functionResponse: {response: {output | error}}}]`，或直接是文字。
pub(crate) fn result_text(r: &Value) -> Option<String> {
    let items: Vec<&Value> = match r {
        Value::Array(a) => a.iter().collect(),
        Value::Null => return None,
        v => vec![v],
    };
    let text = items
        .iter()
        .filter_map(|p| {
            p.pointer("/functionResponse/response")
                .and_then(|resp| str_field(resp, "output").or_else(|| str_field(resp, "error")))
                .or_else(|| part_str(p))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn replay(lines: &[Value]) -> Replay {
        let mut r = Replay::new(true);
        for l in lines {
            r.line(l);
        }
        r
    }

    fn texts(r: &Replay) -> Vec<String> {
        r.msgs
            .iter()
            .filter_map(|(_, m)| m.said())
            .map(|m| m.text)
            .collect()
    }

    fn user(id: &str, text: &str) -> Value {
        json!({"id": id, "timestamp": "2026-11-20T10:00:00Z", "type": "user", "content": [{"text": text}]})
    }

    fn model(id: &str, text: &str) -> Value {
        json!({"id": id, "timestamp": "2026-11-20T10:00:01Z", "type": "gemini", "content": text})
    }

    #[test]
    fn rewind_drops_target_and_later_or_everything() {
        let base = [user("u1", "一"), model("g1", "二"), user("u2", "三")];
        let mut lines = base.to_vec();
        lines.push(json!({"$rewindTo": "g1"}));
        lines.push(user("u3", "四"));
        assert_eq!(texts(&replay(&lines)), ["一", "四"]);

        let mut lines = base.to_vec();
        lines.push(json!({"$rewindTo": "没有这条"}));
        assert!(texts(&replay(&lines)).is_empty());
    }

    #[test]
    fn same_id_replaces_in_place() {
        let r = replay(&[user("u1", "旧"), model("g1", "答"), user("u1", "新")]);
        assert_eq!(texts(&r), ["新", "答"]);
    }

    #[test]
    fn set_merges_metadata_and_legacy_messages_replace_all() {
        let r = replay(&[
            json!({"sessionId": "s", "projectHash": "h", "startTime": "2026-11-20T10:00:00Z"}),
            user("u1", "一"),
            json!({"$set": {"summary": "标题", "lastUpdated": "2026-11-20T11:00:00Z"}}),
            json!({"$set": {"messages": [user("u9", "整份")]}}),
        ]);
        assert_eq!(r.meta_str("summary"), Some("标题"));
        assert_eq!(r.meta_str("sessionId"), Some("s"));
        assert_eq!(texts(&r), ["整份"]);
    }

    #[test]
    fn patch_content_tool_result_remove_and_order() {
        let call = json!({"id": "g1", "type": "gemini", "content": "",
            "toolCalls": [{"id": "c1", "name": "run_shell_command", "displayName": "Shell",
                "args": {"command": "cargo test"}, "status": "executing"}]});
        let r = replay(&[
            user("u1", "一"),
            call,
            user("u2", "二"),
            user("u3", "三"),
            json!({"$patch": {"id": "u1", "content": [{"text": "改过"}]}}),
            json!({"$patch": {"updates": [{"id": "g1", "toolCalls": [{"id": "c1",
                "result": [{"functionResponse": {"id": "c1", "name": "run_shell_command",
                    "response": {"output": "ok"}}}]}]}]}}),
            json!({"$patch": {"removeIds": ["u2"]}}),
            json!({"$patch": {"orderIds": ["u1", "g1"]}}),
        ]);
        assert_eq!(texts(&r), ["三", "改过"]);
        let tool = &r.msgs[2].1.tools[0];
        assert_eq!(
            (tool.name.as_str(), tool.input.as_str()),
            ("Shell", "cargo test")
        );
        assert_eq!(tool.output.as_deref(), Some("ok"));
    }

    #[test]
    fn commands_injections_and_tool_responses_are_not_speech() {
        let r = replay(&[
            user("u1", "/resume"),
            user("u2", "?"),
            user("u3", "<session_context>x</session_context>"),
            json!({"id": "u4", "type": "user", "content": [{"functionResponse": {"name": "x", "response": {}}}]}),
            json!({"id": "u5", "type": "user", "content": "看 @a.md 里写了什么 展开后的文件",
                "displayContent": "看 @a.md 里写了什么"}),
            json!({"id": "i1", "type": "info", "content": "提示"}),
            json!({"id": "g1", "type": "gemini", "content": [{"text": "想一想", "thought": true}, {"text": "答"}]}),
        ]);
        assert_eq!(r.users(), 1);
        assert_eq!(texts(&r), ["看 @a.md 里写了什么", "答"]);
    }

    #[test]
    fn names_and_dirs() {
        assert!(is_session_name(OsStr::new(
            "session-2026-11-20T10-00-1a2b3c4d.jsonl"
        )));
        assert!(is_session_name(OsStr::new(
            "session-2026-03-01T10-00-1a2b3c4d.json"
        )));
        assert!(!is_session_name(OsStr::new("logs.json")));
        assert!(!is_session_name(OsStr::new("session-x.jsonl.tmp-12")));
        assert!(is_hash_dir(&"ab".repeat(32)));
        assert!(!is_hash_dir("shop"));
        assert!(!is_hash_dir(&"AB".repeat(32)));
    }

    #[test]
    fn resume_with_message_keeps_dashes_as_text() {
        let src = GeminiSource::new("/g/tmp", Trash::new("/t"));
        let s = Session {
            vendor: Vendor::Gemini,
            id: SessionId::from("1a2b3c4d-0000-4000-8000-000000000001"),
            cwd: PathBuf::from("/p"),
            path: PathBuf::new(),
            title: None,
            git_branch: None,
            last_active: SystemTime::UNIX_EPOCH,
            message_count: 0,
            automatic: false,
        };
        let cmd = src.resume_with_message_cmd(&s, "-继续");
        assert_eq!(
            cmd.args,
            [
                "--resume",
                "1a2b3c4d-0000-4000-8000-000000000001",
                "--prompt-interactive=-继续"
            ]
        );
        assert_eq!(cmd.cwd, Path::new("/p"));
    }
}
