//! Hermes Agent（NousResearch/hermes-agent）：对话全在 `$HERMES_HOME/state.db`（默认 `~/.hermes`）一个 SQLite 库里，
//! `sessions` 一行一条对话，`messages` 一行一条消息（`role` 为 user/assistant/tool，助手的工具调用在 `tool_calls`）。
//! 表结构以上游 `persistence/sqlite/_schema_ddl.py`（v33）为准，变得快，取列宽松：先查有哪些列，只取有的。
//!
//! - 只读打开（`sqlite3 -readonly`，带 busy 超时，见 `sqlite`），从不写库。
//! - 只列本机终端里开的对话：来源取 `COALESCE(created_source, source)`（gateway 把本机各入口改记成 `local`，
//!   真正的入口在 `created_source`），`cli`、`tui`、`desktop`、`acp`、`local` 算；Telegram、Discord 等消息平台、
//!   子代理（`subagent`）不列。`tool`、`oneshot`（`hermes chat -q` 一次性跑）和 `cron` 算自动运行。
//! - Hermes 自己归档、隐藏的不列；压缩后另起一行续写的，只列最新那行（`-r` 接着聊的也是它）。
//! - 项目目录取 `cwd`，空的退用 `git_repo_root`。
//! - 恢复 `hermes -r <id>`（它自己 cd 回记录的目录）；新建 `hermes --in <目录>`，定不了 id。
//! - 删除只隐藏（对话在 Hermes 自己的库里，和 Codex 一样）。

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::jsonl;
use super::sqlite::{self, Row};
use super::{
    Command, Part, PreviewMessage, Role, Scan, Source, Transcript, Trash, TrashEntry, transcript,
};
use crate::core::{Session, SessionId, Vendor};

/// 整段对话最多取最后这么多条消息。
const HISTORY_ROWS: usize = 2000;

/// 本机终端里开的对话的来源（`created_source` 优先，没有再看 `source`）。
const LOCAL_SOURCES: [&str; 6] = ["cli", "tui", "desktop", "gui", "acp", "local"];
/// 自动运行的来源：第三方集成、一次性跑、定时任务。
const AUTO_SOURCES: [&str; 3] = ["tool", "oneshot", "cron"];

/// `sessions` 表里要用到的列（有哪个取哪个）。
const SESSION_COLUMNS: [&str; 13] = [
    "id",
    "source",
    "created_source",
    "cwd",
    "git_repo_root",
    "git_branch",
    "title",
    "started_at",
    "last_activity_at",
    "end_reason",
    "parent_session_id",
    "archived",
    "hidden",
];

pub struct HermesSource {
    root: PathBuf,
    trash: Trash,
}

impl HermesSource {
    /// `root` 是 `$HERMES_HOME`（默认 `~/.hermes`）。
    pub fn new(root: impl Into<PathBuf>, trash: Trash) -> Self {
        Self {
            root: root.into(),
            trash,
        }
    }

    fn db(&self) -> PathBuf {
        self.root.join("state.db")
    }

    /// 这张表现在有哪些列。
    fn columns(&self, table: &str) -> io::Result<HashSet<String>> {
        let rows = sqlite::query(
            &self.db(),
            &format!("select name from pragma_table_info('{table}')"),
        )?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get("name")?.as_str().map(str::to_owned))
            .collect())
    }

    /// 只算还有效的消息：被回退（`/rewind`）的消息 `active = 0`，没有这一列的老库都算。
    fn active_filter(&self) -> io::Result<&'static str> {
        Ok(if self.columns("messages")?.contains("active") {
            " and active = 1"
        } else {
            ""
        })
    }

    fn list(&self) -> io::Result<Vec<Session>> {
        let have = self.columns("sessions")?;
        if !have.contains("id") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sessions 表没有 id 列，不认得这个库",
            ));
        }
        let cols: Vec<&str> = SESSION_COLUMNS
            .into_iter()
            .filter(|c| have.contains(*c))
            .collect();
        let db = self.db();
        let rows = sqlite::query(&db, &format!("select {} from sessions", cols.join(", ")))?;
        let active = self.active_filter()?;
        let stats: HashMap<String, (usize, Option<SystemTime>)> = sqlite::query(
            &db,
            &format!(
                "select session_id as id, sum(role = 'user') as n, max(timestamp) as last \
                 from messages where 1{active} group by session_id"
            ),
        )?
        .iter()
        .filter_map(|r| {
            let id = r.get("id")?.as_str()?.to_owned();
            let n = r.get("n").and_then(Value::as_u64).unwrap_or(0) as usize;
            Some((id, (n, r.get("last").and_then(secs))))
        })
        .collect();
        // 没有标题的用第一句话。SQLite 的 min() 聚合让同一行的其他列取最小 id 那行的值。
        let first: HashMap<String, String> = sqlite::query(
            &db,
            &format!(
                "select session_id as id, substr(content, 1, 400) as text, min(id) as first \
                 from messages where role = 'user' and content is not null and content != ''{active} \
                 group by session_id"
            ),
        )?
        .iter()
        .filter_map(|r| {
            let id = r.get("id")?.as_str()?.to_owned();
            Some((id, message_text(r.get("text")?.as_str()?)?))
        })
        .collect();
        // 压缩以后另起一行续写，父行 `end_reason = 'compression'`：有续写的父行不列。
        let continued: HashSet<&str> = rows
            .iter()
            .filter_map(|r| str_col(r, "parent_session_id"))
            .collect();
        let hidden = self.trash.hidden_ids(Vendor::Hermes)?;
        Ok(rows
            .iter()
            .filter(|r| {
                let id = str_col(r, "id").unwrap_or_default();
                let superseded =
                    str_col(r, "end_reason") == Some("compression") && continued.contains(id);
                !hidden.contains(id) && !superseded
            })
            .filter_map(|r| session_from_row(r, &stats, &first))
            .collect())
    }
}

impl Source for HermesSource {
    fn vendor(&self) -> Vendor {
        Vendor::Hermes
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
        let Some(id) = sqlite::safe_id(session.id.as_str()) else {
            return Ok(Vec::new());
        };
        let active = self.active_filter()?;
        let sql = format!(
            "select role, content, timestamp from messages where session_id = '{id}' \
             and role in ('user', 'assistant') and content is not null and content != ''{active} \
             order by timestamp desc, id desc limit {n}"
        );
        let mut out: Vec<PreviewMessage> = sqlite::query(&self.db(), &sql)?
            .iter()
            .filter_map(preview_message)
            .map(jsonl::clip)
            .collect();
        out.reverse();
        Ok(out)
    }

    fn load_transcript(&self, session: &Session) -> io::Result<Transcript> {
        let Some(id) = sqlite::safe_id(session.id.as_str()) else {
            return Ok(Transcript::default());
        };
        let active = self.active_filter()?;
        let sql = format!(
            "select role, content, tool_calls, timestamp from messages where session_id = '{id}'{active} \
             order by timestamp desc, id desc limit {}",
            HISTORY_ROWS + 1
        );
        let mut rows = sqlite::query(&self.db(), &sql)?;
        let truncated = rows.len() > HISTORY_ROWS;
        rows.truncate(HISTORY_ROWS);
        rows.reverse();
        Ok(Transcript {
            parts: rows.iter().flat_map(parts).map(transcript::clip).collect(),
            truncated,
        })
    }

    fn count_messages(&self, session: &Session) -> io::Result<usize> {
        let Some(id) = sqlite::safe_id(session.id.as_str()) else {
            return Ok(0);
        };
        let active = self.active_filter()?;
        let sql = format!(
            "select count(*) as n from messages where session_id = '{id}' and role = 'user'{active}"
        );
        Ok(sqlite::query(&self.db(), &sql)?
            .first()
            .and_then(|r| r.get("n"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize)
    }

    fn resume_cmd(&self, session: &Session) -> Command {
        Command::new("hermes", &["-r", session.id.as_str()], &session.cwd)
    }

    /// 带一句话接着聊：Hermes 没有位置参数当首句话（多出来的词会被当成子命令名），要用 `chat` 子命令的
    /// `-q/--query`：在真终端里它是「交互会话的第一句话」，不是一次性跑完就退（上游 `hermes_cli/_parser.py`、
    /// `cli_single_query.py`、`main_tui_launch.py` 同时接 resume 和 query）。写成 `--query=…`，话以 `-` 开头也不会被当成选项。
    fn resume_with_message_cmd(&self, session: &Session, message: &str) -> Command {
        let query = format!("--query={message}");
        Command::new(
            "hermes",
            &["chat", "-r", session.id.as_str(), &query],
            &session.cwd,
        )
    }

    fn new_cmd(&self, cwd: &Path) -> Command {
        Command::new("hermes", &["--in", &cwd.to_string_lossy()], cwd)
    }

    /// 只隐藏：对话在 Hermes 自己的库里。
    fn trash(&self, session: &Session) -> io::Result<TrashEntry> {
        self.trash
            .hide(Vendor::Hermes, session.id.as_str(), &session.path)
    }

    fn restore(&self, entry: &TrashEntry) -> io::Result<()> {
        self.trash.restore(entry)
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    /// `$HERMES_HOME` 里还有日志、配置、技能；只有库和它的 WAL 影响列表。`-shm` 读的时候也会动，不算，
    /// 不然自己读一次库又触发一次重读。
    fn affects(&self, path: &Path) -> bool {
        path.parent() == Some(self.root.as_path())
            && path
                .file_name()
                .is_some_and(|n| n == "state.db" || n == "state.db-wal")
    }
}

fn str_col<'a>(row: &'a Row, key: &str) -> Option<&'a str> {
    row.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// 秒（可带小数）→ `SystemTime`；负数、非数和大到表示不了的（坏库、导入的数据）返回 `None`，不 panic：
/// 扫描在全家共用的索引线程里跑，一条坏时间不能让它退出。
fn secs(v: &Value) -> Option<SystemTime> {
    let s = v.as_f64().filter(|s| *s >= 0.0)?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::try_from_secs_f64(s).ok()?)
}

fn session_from_row(
    row: &Row,
    stats: &HashMap<String, (usize, Option<SystemTime>)>,
    first: &HashMap<String, String>,
) -> Option<Session> {
    let id = str_col(row, "id")?;
    let flag = |k: &str| row.get(k).and_then(Value::as_i64).unwrap_or(0) != 0;
    if flag("archived") || flag("hidden") {
        return None;
    }
    let origin = str_col(row, "created_source")
        .filter(|s| *s != "unknown")
        .or_else(|| str_col(row, "source"))?;
    let automatic = AUTO_SOURCES.contains(&origin);
    if !automatic && !LOCAL_SOURCES.contains(&origin) {
        return None;
    }
    // 一句话都没说的（启动后没聊就退出、恢复失败留下的空行）不列。
    let &(message_count, last_message) = stats.get(id)?;
    if message_count == 0 {
        return None;
    }
    let cwd = str_col(row, "cwd")
        .or_else(|| str_col(row, "git_repo_root"))
        .unwrap_or_default();
    let title = str_col(row, "title")
        .or_else(|| first.get(id).map(String::as_str))
        .map(|t| jsonl::one_line(t, 80));
    let last_active = [
        row.get("last_activity_at").and_then(secs),
        last_message,
        row.get("started_at").and_then(secs),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(SystemTime::UNIX_EPOCH);
    Some(Session {
        vendor: Vendor::Hermes,
        id: SessionId::from(id),
        cwd: PathBuf::from(cwd),
        // 不按文件组织：库是各条对话共用的，记成会话文件会让一条在写、条条都像别处在用。
        path: PathBuf::new(),
        title,
        git_branch: str_col(row, "git_branch").map(str::to_owned),
        last_active,
        message_count,
        automatic,
    })
}

/// 消息正文：纯文本，或多模态时的 `[{type:"text", text}]` JSON。
fn message_text(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.starts_with('[')
        && let Ok(v @ Value::Array(_)) = serde_json::from_str::<Value>(trimmed)
    {
        return jsonl::content_text(&v, &["text", "input_text", "output_text"]);
    }
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn preview_message(row: &Row) -> Option<PreviewMessage> {
    let role = match str_col(row, "role")? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    Some(PreviewMessage {
        role,
        text: message_text(str_col(row, "content")?)?,
        at: row.get("timestamp").and_then(secs),
    })
}

/// 一行消息拆成整段对话里的几块：用户和助手的话、助手发起的工具调用（OpenAI 格式的 `tool_calls`）、工具返回。
fn parts(row: &Row) -> Vec<Part> {
    match str_col(row, "role") {
        Some("tool") => str_col(row, "content")
            .map(|text| {
                vec![Part::ToolResult {
                    text: text.to_owned(),
                }]
            })
            .unwrap_or_default(),
        Some("assistant") => {
            let mut out: Vec<Part> = preview_message(row)
                .map(Part::Message)
                .into_iter()
                .collect();
            let calls = str_col(row, "tool_calls")
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(Value::Null);
            for call in calls.as_array().into_iter().flatten() {
                let f = call.get("function").unwrap_or(call);
                let name = jsonl::str_field(f, "name").unwrap_or("工具").to_owned();
                let input = transcript::input_summary(f.get("arguments").unwrap_or(&Value::Null));
                out.push(Part::ToolCall { name, input });
            }
            out
        }
        _ => preview_message(row)
            .map(Part::Message)
            .into_iter()
            .collect(),
    }
}

/// Hermes 新建对话的 id 是 `YYYYMMDD_HHMMSS_<十六进制>`（本机时间，精确到秒，上游 `session_rules/session_ids.py`）：
/// 取出生成它的时刻；不是这种形状的返回 `None`。
pub fn id_created(id: &str) -> Option<SystemTime> {
    let b = id.as_bytes();
    if b.len() < 16 || b[8] != b'_' || b[15] != b'_' {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<i32> {
        let s = std::str::from_utf8(&b[r]).ok()?;
        s.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| s.parse().ok())?
    };
    let (year, month, day) = (digits(0..4)?, digits(4..6)?, digits(6..8)?);
    let (hour, min, sec) = (digits(9..11)?, digits(11..13)?, digits(13..15)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    // SAFETY: 全零的 tm 是合法值（各字段都是整数或可空指针）。
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day;
    tm.tm_hour = hour;
    tm.tm_min = min;
    tm.tm_sec = sec;
    tm.tm_isdst = -1;
    // SAFETY: 指针指向本函数里的局部变量，mktime 只读写它；失败返回 -1。
    let t = unsafe { libc::mktime(&mut tm) };
    let t = u64::try_from(t).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_range_seconds_are_skipped_not_panicking() {
        assert_eq!(
            secs(&Value::from(1.5)),
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1500))
        );
        for bad in [1e20, f64::MAX, -1.0] {
            assert_eq!(secs(&Value::from(bad)), None, "{bad}");
        }
        assert_eq!(secs(&Value::from("x")), None);
    }

    #[test]
    fn multimodal_content_keeps_text_parts() {
        assert_eq!(message_text("  修登录  ").as_deref(), Some("修登录"));
        assert_eq!(
            message_text(
                r#"[{"type":"text","text":"看这张图"},{"type":"image_url","image_url":{}}]"#
            )
            .as_deref(),
            Some("看这张图")
        );
        // 方括号开头但不是 JSON 的就是普通文字。
        assert_eq!(
            message_text("[WIP] 先这样").as_deref(),
            Some("[WIP] 先这样")
        );
        assert_eq!(message_text("   "), None);
    }

    /// R（被中断的对话带一句话接着做）：话要经 `chat -r <id> --query=…` 送进去，不能直接接在 `-r <id>` 后面
    /// （那样 Hermes 把它当子命令名，报「is not a `hermes` command」后退出）。
    #[test]
    fn resume_with_message_seeds_the_first_turn() {
        let src = HermesSource::new("/h/.hermes", Trash::new("/tmp/huitai-hermes-trash"));
        let s = Session {
            vendor: Vendor::Hermes,
            id: SessionId::from("20261120_101500_a1b2c3"),
            cwd: PathBuf::from("/work/api"),
            path: PathBuf::new(),
            title: None,
            git_branch: None,
            last_active: SystemTime::UNIX_EPOCH,
            message_count: 1,
            automatic: false,
        };
        let cmd = src.resume_with_message_cmd(&s, "继续上次没做完的事。");
        assert_eq!(
            cmd.shell_line(),
            "hermes chat -r 20261120_101500_a1b2c3 '--query=继续上次没做完的事。'"
        );
        assert_eq!(cmd.cwd, PathBuf::from("/work/api"));
        // 以 `-` 开头的话也整个是 --query 的值。
        let cmd = src.resume_with_message_cmd(&s, "-v 不是选项");
        assert_eq!(
            cmd.args.last().map(String::as_str),
            Some("--query=-v 不是选项")
        );
    }

    #[test]
    fn id_time_is_local_and_second_precise() {
        let t = id_created("20261122_213015_a1b2c3").expect("是 Hermes 的 id");
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        // 不管时区是什么，同一天的这个时刻在 UTC 前后 14 小时以内。
        let utc = 1_795_383_015u64; // 2026-11-22 21:30:15 UTC
        assert!(secs.abs_diff(utc) <= 14 * 3600, "{secs}");
        assert_eq!(
            id_created("20261122_213016_ffffff").unwrap(),
            t + Duration::from_secs(1)
        );
        assert_eq!(id_created("cron_job1_20261122_213015"), None);
        assert_eq!(id_created("20261322_213015_a1b2c3"), None);
        assert_eq!(id_created("0193a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"), None);
    }
}
