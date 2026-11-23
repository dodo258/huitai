//! Antigravity（Google，`agy` CLI 和 Antigravity App）：两者各写各的数据目录（`agy` 是 `~/.gemini/antigravity-cli`，
//! App 是 `~/.gemini/antigravity`，App 那份在 `sources::accounts` 里认成一个账号），格式相同：
//!
//! - `conversation_summaries.db`（SQLite，WAL）：`conversation_summaries` 一行一条对话（标题、工作区、
//!   最后修改时间、父对话）；`conversations_fts`（FTS5，存了原文）里每条对话一行表头（`chunk_index = -1`，
//!   带 `archived`）加若干块正文：`user_body`、`agent_body` 是各步的文字接起来、每步后面一个换行，
//!   `user_step_byte_offsets`、`agent_step_byte_offsets` 是 `[[步号, 字节偏移], …]`。正文按偏移切回一步一条，
//!   按步号排就是对话顺序。
//! - `conversations/<id>.pb` 是加密的，不读；`brain/<id>/` 放计划、任务这些产物，只拿它的建立时间认新建的对话。
//! - `annotations/<id>.pbtxt`：用户标注（`archived:true` 是在 Antigravity 里归档了），有它就以它为准。
//!
//! 只读打开库（见 `sqlite`），从不写。归档的、子代理（`parent_conversation_id` 非空）、没说过话的不列。
//! 项目目录取 `workspace_uris`（JSON 数组，`file://` URI）里第一个本机目录，没有就是空的（零散对话）。
//! 每条消息没有自己的时间。
//! 恢复 `agy --conversation <id>`（官方文档 https://antigravity.google/docs/cli/reference/ 「Resume a previous
//! conversation by ID」）；带话接着聊没有写明能和它一起用的选项，只恢复、话自己输入；新建在目录里 `agy`，定不了 id。
//! 删除只隐藏（对话在 Antigravity 自己的库里）。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use super::jsonl;
use super::sqlite::{self, Row, hex_bytes, secs, str_col};
use super::window::PREVIEW_MAX_BYTES;
use super::{
    Command, Delivery, Moment, Part, PreviewMessage, Role, Scan, Source, TRANSCRIPT_MAX_BYTES,
    Transcript, Trash, TrashEntry, moment, transcript,
};
use crate::core::{Session, SessionId, Vendor};

/// 摘要库的文件名（数据目录下）。
pub const SUMMARIES_DB: &str = "conversation_summaries.db";

/// 列表里第一句话只取第一块正文开头这么多字节：只拿来当标题（压成一行、最多 80 字），按字节截断切坏的字换成 U+FFFD。
const FIRST_BYTES: usize = 4096;

/// 整段对话最多取最后这么多条消息。
const HISTORY_MESSAGES: usize = 2000;

/// `conversation_summaries` 里要用到的列（有哪个取哪个，没有的当空）。时间列是 `2026-01-17 04:21:11.6+00:00`
/// 这样的文字，在库里换成秒；零值 `0001-01-01` 换出来是负数，当没有。
const SUMMARY_COLUMNS: [(&str, bool); 8] = [
    ("title", false),
    ("preview", false),
    ("workspace_uris", false),
    ("parent_conversation_id", false),
    ("nesting_depth", false),
    ("last_user_input_step_index", false),
    ("last_modified_time", true),
    ("last_user_input_time", true),
];

pub struct AntigravitySource {
    root: PathBuf,
    trash: Trash,
}

impl AntigravitySource {
    /// `root` 是数据目录（`~/.gemini/antigravity-cli` 或 App 的 `~/.gemini/antigravity`）。
    pub fn new(root: impl Into<PathBuf>, trash: Trash) -> Self {
        Self {
            root: root.into(),
            trash,
        }
    }

    fn db(&self) -> PathBuf {
        self.root.join(SUMMARIES_DB)
    }

    fn list(&self) -> io::Result<Vec<Session>> {
        let db = self.db();
        let have = sqlite::columns(&db, "conversation_summaries")?;
        if !have.contains("conversation_id") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "conversation_summaries 表没有 conversation_id 列，不认得这个库",
            ));
        }
        let cols: Vec<String> = SUMMARY_COLUMNS
            .iter()
            .map(|&(c, time)| match (have.contains(c), time) {
                (false, _) => format!("null as {c}"),
                (true, true) => format!("unixepoch({c}, 'subsec') as {c}"),
                (true, false) => c.to_owned(),
            })
            .collect();
        let rows = sqlite::query(
            &db,
            &format!(
                "select conversation_id as id, {} from conversation_summaries",
                cols.join(", ")
            ),
        )?;
        let stats = self.stats(None)?;
        let hidden = self.trash.hidden_ids(Vendor::Antigravity)?;
        Ok(rows
            .iter()
            .filter(|r| str_col(r, "id").is_some_and(|id| !hidden.contains(id)))
            .filter_map(|r| self.session_from_row(r, &stats))
            .collect())
    }

    /// 各条对话（`only` 给了就只那一条）的归档标记、用户发言条数和第一句话（没标题时当标题），从全文表里取；
    /// 正文只传第一块的开头 [`FIRST_BYTES`]。
    fn stats(&self, only: Option<&str>) -> io::Result<HashMap<String, Stats>> {
        let db = self.db();
        if !has_fts(&db)? {
            return Ok(HashMap::new());
        }
        let cond = only.map_or_else(String::new, |id| format!(" where conversation_id = '{id}'"));
        let rows = sqlite::query(
            &db,
            &format!(
                "select conversation_id as id, chunk_index as chunk, archived, \
                 user_step_byte_offsets as offsets, length(cast(user_body as blob)) as len, \
                 case when chunk_index = 0 then hex(substr(cast(user_body as blob), 1, {FIRST_BYTES})) end as first \
                 from conversations_fts{cond}"
            ),
        )?;
        let mut out: HashMap<String, Stats> = HashMap::new();
        for r in &rows {
            let Some(id) = str_col(r, "id") else { continue };
            let st = out.entry(id.to_owned()).or_default();
            let chunk = r.get("chunk").and_then(Value::as_i64).unwrap_or(0);
            if chunk < 0 {
                st.archived = r.get("archived").and_then(Value::as_i64).unwrap_or(0) != 0;
                continue;
            }
            let len = r.get("len").and_then(Value::as_u64).unwrap_or(0) as usize;
            let offsets = offsets(r.get("offsets"));
            st.users += segments(&offsets, len)
                .filter(|(_, a, b)| b - a > 1)
                .count();
            if let Some(body) = r.get("first").and_then(Value::as_str).and_then(hex_bytes) {
                st.first = split(&body, &offsets)
                    .into_iter()
                    .map(|(_, text)| text)
                    .next();
            }
        }
        Ok(out)
    }

    fn session_from_row(&self, row: &Row, stats: &HashMap<String, Stats>) -> Option<Session> {
        let id = str_col(row, "id")?;
        sqlite::safe_id(id)?;
        let num = |k: &str| row.get(k).and_then(Value::as_i64);
        // 子代理是主对话派出去的，跟着主对话看。
        if str_col(row, "parent_conversation_id").is_some() || num("nesting_depth").unwrap_or(0) > 0
        {
            return None;
        }
        let st = stats.get(id).cloned().unwrap_or_default();
        if self.archived(id).unwrap_or(st.archived) {
            return None;
        }
        // 一句话都没说的（开了就关）不列。
        if st.users == 0 && num("last_user_input_step_index").unwrap_or(-1) < 0 {
            return None;
        }
        let title = str_col(row, "title")
            .or_else(|| str_col(row, "preview"))
            .or(st.first.as_deref())
            .map(|t| jsonl::one_line(t, 80));
        let last_active = ["last_modified_time", "last_user_input_time"]
            .into_iter()
            .filter_map(|k| row.get(k).and_then(secs))
            .max()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        Some(Session {
            vendor: Vendor::Antigravity,
            id: SessionId::from(id),
            cwd: str_col(row, "workspace_uris")
                .and_then(workspace_dir)
                .unwrap_or_default(),
            // 库是各条对话共用的；记这条自己的产物目录：建立时间认新建、变动时间认别处在用。
            path: self.root.join("brain").join(id),
            title,
            git_branch: None,
            last_active,
            message_count: st.users,
            automatic: false,
            account: String::new(),
        })
    }

    /// `annotations/<id>.pbtxt` 里的归档标记；没有这个文件是 `None`。
    fn archived(&self, id: &str) -> Option<bool> {
        let text =
            std::fs::read_to_string(self.root.join("annotations").join(format!("{id}.pbtxt")))
                .ok()?;
        Some(text_field(&text, "archived") == Some("true"))
    }

    /// 一条对话最后 `n` 条消息（按步号先后，空的不要），和有没有更早的没读。块按步号先后切
    /// （`chunk_first_step`..`chunk_last_step`）：先只取各块的步偏移、不取正文，从最新一步往前挑；
    /// 再只取挑中的那几块正文里要的那一段。挑够 `n` 条，或正文读满 `max_bytes`（至少读最后一步）就停。
    fn last_messages(
        &self,
        session: &Session,
        n: usize,
        max_bytes: u64,
    ) -> io::Result<(Vec<PreviewMessage>, bool)> {
        let Some(id) = sqlite::safe_id(session.id.as_str()) else {
            return Ok((Vec::new(), false));
        };
        let db = self.db();
        if n == 0 || !has_fts(&db)? {
            return Ok((Vec::new(), false));
        }
        let rows = sqlite::query(
            &db,
            &format!(
                "select chunk_index as chunk, user_step_byte_offsets as uo, agent_step_byte_offsets as ao, \
                 length(cast(user_body as blob)) as ul, length(cast(agent_body as blob)) as al \
                 from conversations_fts where conversation_id = '{id}' and chunk_index >= 0 order by chunk_index"
            ),
        )?;
        let mut steps: Vec<Step> = Vec::new();
        for r in &rows {
            let chunk = r.get("chunk").and_then(Value::as_i64).unwrap_or(0);
            for (role, at, len) in [(Role::User, "uo", "ul"), (Role::Assistant, "ao", "al")] {
                let len = r.get(len).and_then(Value::as_u64).unwrap_or(0) as usize;
                let offsets = offsets(r.get(at));
                steps.extend(segments(&offsets, len).filter(|(_, a, b)| b > a).map(
                    |(step, a, b)| Step {
                        chunk,
                        role,
                        step,
                        a,
                        b,
                    },
                ));
            }
        }
        // 同一步先用户后 AI（排序是稳定的，同一步里保持读出来的先后）。
        steps.sort_by_key(|s| (s.step, s.role == Role::Assistant));

        let mut got: Vec<(usize, String)> = Vec::new();
        let mut end = steps.len();
        let mut bytes = 0u64;
        while end > 0 && got.len() < n {
            // 这一轮还差几条就往前挑几步（空白步读出来才知道，不够再挑）。
            let mut start = end;
            while start > 0 && end - start < n - got.len() {
                let len = (steps[start - 1].b - steps[start - 1].a) as u64;
                if bytes + len > max_bytes && !(got.is_empty() && start == end) {
                    break;
                }
                bytes += len;
                start -= 1;
            }
            if start == end {
                break;
            }
            got.extend(
                self.read_steps(id, &steps[start..end])?
                    .into_iter()
                    .map(|(i, t)| (start + i, t)),
            );
            end = start;
        }
        got.sort_by_key(|&(i, _)| i);
        let messages = got
            .into_iter()
            .map(|(i, text)| PreviewMessage {
                role: steps[i].role,
                text,
                at: None,
            })
            .collect();
        Ok((messages, end > 0))
    }

    /// 取这几步的文字：每块每段正文只取挑中的步所在的那一段字节。返回 `(第几步, 文字)`，空白的不要。
    /// 坏 UTF-8 换成 U+FFFD。
    fn read_steps(&self, id: &str, steps: &[Step]) -> io::Result<Vec<(usize, String)>> {
        // (块, 角色) → 要的字节范围。
        let mut ranges: HashMap<(i64, Role), (usize, usize)> = HashMap::new();
        for s in steps {
            let r = ranges.entry((s.chunk, s.role)).or_insert((s.a, s.b));
            *r = (r.0.min(s.a), r.1.max(s.b));
        }
        let mut chunks: Vec<i64> = ranges.keys().map(|&(c, _)| c).collect();
        chunks.sort_unstable();
        chunks.dedup();
        let cut = |role: Role, body: &str| {
            let (from, len): (String, String) = chunks
                .iter()
                .filter_map(|c| ranges.get(&(*c, role)).map(|&(a, b)| (c, a, b)))
                .map(|(c, a, b)| {
                    (
                        format!(" when {c} then {}", a + 1),
                        format!(" when {c} then {}", b - a),
                    )
                })
                .unzip();
            if from.is_empty() {
                // 这段正文一步都没挑中。
                return "''".to_owned();
            }
            format!(
                "hex(substr(cast({body} as blob), case chunk_index{from} else 1 end, case chunk_index{len} else 0 end))"
            )
        };
        let list: Vec<String> = chunks.iter().map(i64::to_string).collect();
        let rows = sqlite::query(
            &self.db(),
            &format!(
                "select chunk_index as chunk, {} as ub, {} as ab from conversations_fts \
                 where conversation_id = '{id}' and chunk_index in ({})",
                cut(Role::User, "user_body"),
                cut(Role::Assistant, "agent_body"),
                list.join(", ")
            ),
        )?;
        let mut bodies: HashMap<(i64, Role), Vec<u8>> = HashMap::new();
        for r in &rows {
            let chunk = r.get("chunk").and_then(Value::as_i64).unwrap_or(0);
            for (role, col) in [(Role::User, "ub"), (Role::Assistant, "ab")] {
                if let Some(b) = r.get(col).and_then(Value::as_str).and_then(hex_bytes) {
                    bodies.insert((chunk, role), b);
                }
            }
        }
        Ok(steps
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let from = ranges.get(&(s.chunk, s.role))?.0;
                let body = bodies.get(&(s.chunk, s.role))?;
                let text = String::from_utf8_lossy(body.get(s.a - from..s.b - from)?);
                let text = text.trim();
                (!text.is_empty()).then(|| (i, text.to_owned()))
            })
            .collect())
    }
}

/// 一步在哪块、哪段正文（用户或 AI）、正文里的哪个字节范围。
#[derive(Debug, Clone, Copy)]
struct Step {
    chunk: i64,
    role: Role,
    step: i64,
    a: usize,
    b: usize,
}

/// 全文表里一条对话的统计。
#[derive(Debug, Clone, Default)]
struct Stats {
    archived: bool,
    users: usize,
    first: Option<String>,
}

fn has_fts(db: &Path) -> io::Result<bool> {
    Ok(!sqlite::query(
        db,
        "select 1 from sqlite_master where name = 'conversations_fts'",
    )?
    .is_empty())
}

/// `[[步号, 字节偏移], …]`，按偏移排好；坏的、不是这个形状的当空。
fn offsets(v: Option<&Value>) -> Vec<(i64, usize)> {
    let mut out: Vec<(i64, usize)> = v
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Vec<(i64, usize)>>(s).ok())
        .unwrap_or_default();
    out.sort_by_key(|&(_, at)| at);
    out
}

/// 每步在正文里的字节范围 `(步号, 起, 止)`：到下一步的偏移为止，最后一步到 `len`。止点越界（坏库）截到 `len`，
/// 起点越界的不要。
fn segments(
    offsets: &[(i64, usize)],
    len: usize,
) -> impl Iterator<Item = (i64, usize, usize)> + '_ {
    offsets
        .iter()
        .enumerate()
        .filter_map(move |(i, &(step, a))| {
            let b = offsets.get(i + 1).map_or(len, |&(_, b)| b.min(len));
            (a <= b).then_some((step, a, b))
        })
}

/// 正文按偏移切回一步一条，去掉首尾空白，空的不要。坏 UTF-8 换成 U+FFFD。
fn split(body: &[u8], offsets: &[(i64, usize)]) -> Vec<(i64, String)> {
    segments(offsets, body.len())
        .filter_map(|(step, a, b)| {
            let text = String::from_utf8_lossy(&body[a..b]);
            let text = text.trim();
            (!text.is_empty()).then(|| (step, text.to_owned()))
        })
        .collect()
}

/// protobuf 文本格式里顶层 `key:value` 的值（`archived:true archival_status_timestamp:{…}`）。
fn text_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = text;
    while let Some(at) = rest.find(key) {
        let before = rest[..at].chars().next_back();
        let after = rest[at + key.len()..].trim_start();
        rest = &rest[at + key.len()..];
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        if let Some(v) = after.strip_prefix(':') {
            let v = v.trim_start();
            let end = v
                .find(|c: char| c.is_whitespace() || c == '{' || c == '}')
                .unwrap_or(v.len());
            return Some(&v[..end]);
        }
    }
    None
}

/// `workspace_uris`（JSON 数组的 `file://` URI，`["file:///Users/me/shop"]`）里第一个本机目录。
/// 远程的（`vscode-remote://…`、`file://主机/…`）跳过；不是 JSON 的当一个 URI 或路径。
fn workspace_dir(raw: &str) -> Option<PathBuf> {
    let uris: Vec<String> = match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Ok(Value::String(s)) => vec![s],
        _ => vec![raw.to_owned()],
    };
    uris.iter().find_map(|u| {
        let path = match u.strip_prefix("file://") {
            Some(rest) if rest.starts_with('/') => percent_decode(rest)?,
            Some(_) => return None,
            None if u.starts_with('/') => u.clone(),
            None => return None,
        };
        let path = path.trim_end_matches('/');
        Some(PathBuf::from(if path.is_empty() { "/" } else { path }))
    })
}

/// URI 里的 `%xx` 还原；还原出来不是 UTF-8 返回 `None`。
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(byte) = b
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// 对话的产物目录（[`Session::path`]）的建立时间：会话台新建的对话靠它认回来。取不到是 `None`。
pub fn created_at(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.created()).ok()
}

impl Source for AntigravitySource {
    fn vendor(&self) -> Vendor {
        Vendor::Antigravity
    }

    fn scan(&self) -> Scan {
        let mut scan = Scan::default();
        if !self.db().is_file() {
            return scan;
        }
        match self.list() {
            Ok(sessions) => scan.sessions = sessions,
            Err(e) => scan.problems.push(format!("{}: {e}", self.db().display())),
        }
        scan
    }

    fn load_preview(&self, session: &Session, n: usize) -> io::Result<Vec<PreviewMessage>> {
        let (keep, _) = self.last_messages(session, n, PREVIEW_MAX_BYTES)?;
        Ok(keep.into_iter().map(jsonl::clip).collect())
    }

    fn load_transcript(&self, session: &Session) -> io::Result<Transcript> {
        let (all, truncated) =
            self.last_messages(session, HISTORY_MESSAGES, TRANSCRIPT_MAX_BYTES)?;
        let parts = all
            .into_iter()
            .map(|m| transcript::clip(Part::Message(m)))
            .collect();
        Ok(Transcript { parts, truncated })
    }

    /// 只有消息，「此刻」就是最后一条：AI 的取第一句，用户的说明这一轮还没回。
    fn load_moment(&self, session: &Session) -> io::Result<Moment> {
        let (last, _) = self.last_messages(session, 1, PREVIEW_MAX_BYTES)?;
        Ok(moment::from_parts(last.into_iter().map(Part::Message)))
    }

    fn count_messages(&self, session: &Session) -> io::Result<usize> {
        let Some(id) = sqlite::safe_id(session.id.as_str()) else {
            return Ok(0);
        };
        Ok(self.stats(Some(id))?.get(id).map_or(0, |s| s.users))
    }

    fn resume_cmd(&self, session: &Session) -> Command {
        Command::new(
            "agy",
            &["--conversation", session.id.as_str()],
            &session.cwd,
        )
    }

    /// 官方文档只写了 `-i/--prompt-interactive`（新开对话带首句话），没写能和 `--conversation` 一起用：
    /// 只恢复，话由用户自己发。
    fn resume_with_message_cmd(&self, session: &Session, _message: &str) -> Command {
        self.resume_cmd(session)
    }

    fn message_delivery(&self, _session: &Session) -> Delivery {
        Delivery::Manual
    }

    fn new_cmd(&self, cwd: &Path) -> Command {
        Command::new("agy", &[], cwd)
    }

    /// 看它 `brain/<id>/` 目录的建立时间（[`created_at`]）。
    fn created_at(&self, session: &Session) -> Option<SystemTime> {
        created_at(&session.path)
    }

    /// 只隐藏：对话在 Antigravity 自己的库里。
    fn trash(&self, session: &Session) -> io::Result<TrashEntry> {
        self.trash
            .hide(Vendor::Antigravity, session.id.as_str(), &session.path)
    }

    fn restore(&self, entry: &TrashEntry) -> io::Result<()> {
        self.trash.restore(entry)
    }

    /// 会话路径是产物目录，正文在摘要库里：看库和它的 WAL。
    fn moment_files(&self, _session: &Session) -> Vec<PathBuf> {
        super::with_wal(&self.db())
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    /// 数据目录里还有日志、产物、缓存；影响列表的只有摘要库和它的 WAL（`-shm` 读的时候也会动，不算），
    /// 和归档标注。
    fn affects(&self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        let name = |n: &str| rel == Path::new(n);
        name(SUMMARIES_DB)
            || name(&format!("{SUMMARIES_DB}-wal"))
            || (rel.starts_with("annotations") && path.extension().is_some_and(|e| e == "pbtxt"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_split_by_byte_offsets() {
        let body = "修登录\n继续\n\n".as_bytes();
        let got = split(body, &[(0, 0), (5, 10), (23, 17)]);
        assert_eq!(got, [(0, "修登录".to_owned()), (5, "继续".to_owned())]);
        // 越界的偏移（坏库）不 panic，只丢那一段。
        assert_eq!(
            split(body, &[(0, 0), (1, 99)]),
            [(0, "修登录\n继续".to_owned())]
        );
        // 落在一个字中间的偏移：坏的那几个字节换成 U+FFFD，不丢整条。
        assert_eq!(
            split("修".as_bytes(), &[(0, 1)]),
            [(0, "\u{FFFD}\u{FFFD}".to_owned())]
        );
    }

    #[test]
    fn workspace_is_first_local_folder() {
        let w = |s: &str| workspace_dir(s);
        assert_eq!(
            w(r#"["file:///Users/me/my%20shop/"]"#),
            Some(PathBuf::from("/Users/me/my shop"))
        );
        assert_eq!(
            w(r#"["vscode-remote://ssh-remote+box/srv/app","file:///work/api"]"#),
            Some(PathBuf::from("/work/api"))
        );
        assert_eq!(w(r#"["file://server/share/x"]"#), None);
        assert_eq!(
            w("file:///work/%E9%A1%B9%E7%9B%AE"),
            Some(PathBuf::from("/work/项目"))
        );
        assert_eq!(w("/plain/path"), Some(PathBuf::from("/plain/path")));
        assert_eq!(w("[]"), None);
        assert_eq!(w("not a path"), None);
    }

    #[test]
    fn archived_flag_from_text_proto() {
        let t = "archived:true archival_status_timestamp:{seconds:1 nanos:2}";
        assert_eq!(text_field(t, "archived"), Some("true"));
        assert_eq!(text_field("archived: false\n", "archived"), Some("false"));
        assert_eq!(text_field("unarchived:true", "archived"), None);
        assert_eq!(
            text_field("last_user_view_time:{seconds:1}", "archived"),
            None
        );
    }

    #[test]
    fn offsets_parse_and_sort() {
        let v = Value::from("[[5,57],[0,0],[23,66]]");
        assert_eq!(offsets(Some(&v)), [(0, 0), (5, 57), (23, 66)]);
        assert!(offsets(Some(&Value::from("坏的"))).is_empty());
        assert!(offsets(None).is_empty());
    }
}
