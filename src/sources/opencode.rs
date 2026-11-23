//! OpenCode（anomalyco/opencode）：对话全在一个 SQLite 库里，默认 `~/.local/share/opencode/opencode.db`，
//! `OPENCODE_DB` 改库的路径。同一个库里有两套表：
//!
//! - 1.x（v1.19.6 `packages/core/src/session/sql.ts`）：`session` 一行一条对话，`message` 一行一条消息
//!   （`data` 是 JSON，`role` 为 user/assistant），`part` 一行一块内容（`data.type` 为 text/tool/reasoning…）。
//! - 2.x（`session/sql.ts`）：`session_v2`、`session_message`（`type` 列为 user/assistant/…，`data` 里是其余字段）、
//!   `session_part`（助手消息拆出来的内容块，按 `ordinal` 排；没拆的还在 `session_message.data.content` 里）。
//!   2.x 第一次启动把 1.x 的对话按原 id 复制进 `session_v2`，原表原样留着，之后 1.x 再聊只写原表。
//!
//! 两套按 id 去重：同一个 id 取 `time_updated` 更晚的那套（一样晚取 2.x），预览、正文也从那套读。
//! 子代理开的对话（`parent_id` 不空）、OpenCode 自己归档的（`time_archived`）、一句话都没说的不列；
//! 这些按去重后胜出的那一行判断，新的那套里归档了，另一套里的旧副本也不列。
//! 1.x 撤销（`/undo`）不删消息，只在 `session.revert` 记下撤到哪条消息（`messageID`），这条和之后的都不显示
//! （v1.19.6 `tui/src/routes/session/index.tsx`）：列表、计数、预览、整段都只看它之前的（[`live_v1`]）。
//! 库里可能有按字节截断的汉字、JavaScript 截在 emoji 中间留下的孤立代理项：读出来只坏那个字（[`sqlite::text`]）。
//! 时间都是毫秒。只读打开（见 `sqlite`），从不写库。
//!
//! - 恢复 `opencode <目录> -s <id>`（两版都认：1.x 的位置参数叫 project，2.x 叫 directory）。
//! - 新建 `opencode <目录>`，不先定 id：1.x 的 `-s` 遇到不存在的 id 报错退出（`cli/tui/validate-session.ts`），
//!   托管在临时 id 下，等它写出对话再按 [`id_created`] 认回来。
//! - 删除只隐藏（对话在 OpenCode 自己的库里，和 Hermes 一样）。

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::jsonl;
use super::sqlite::{self, Row, str_col};
use super::{
    Command, Delivery, Part, PreviewMessage, Role, Scan, Source, Transcript, Trash, TrashEntry,
    transcript,
};
use crate::core::{Session, SessionId, Vendor};

/// 整段对话最多取最后这么多条消息。
const HISTORY_ROWS: usize = 2000;

/// 一条对话在哪套表里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Schema {
    V1,
    V2,
}

pub struct OpenCodeSource {
    db: PathBuf,
    trash: Trash,
}

impl OpenCodeSource {
    /// `db` 是库文件（`$OPENCODE_DB`，默认 `~/.local/share/opencode/opencode.db`）。
    pub fn new(db: impl Into<PathBuf>, trash: Trash) -> Self {
        Self {
            db: db.into(),
            trash,
        }
    }

    fn query(&self, sql: &str) -> io::Result<Vec<Row>> {
        sqlite::query(&self.db, sql)
    }

    /// 库里的表名，加上 1.x `session` 表的列（`session.<列名>`，老库没有 `revert` 列）。
    fn tables(&self) -> io::Result<HashSet<String>> {
        Ok(self
            .query(
                "select name from sqlite_master where type = 'table' \
                 union all select 'session.' || name as name from pragma_table_info('session')",
            )?
            .iter()
            .filter_map(|r| str_col(r, "name").map(str::to_owned))
            .collect())
    }

    fn list(&self) -> io::Result<Vec<Session>> {
        let tables = self.tables()?;
        // 先按 id 去重（只比 time_updated，和 `locate` 一样），再看胜出的那一行列不列：
        // 在新的那套里归档了的，不能让另一套里的旧副本顶上来。
        let mut best: HashMap<String, Listed> = HashMap::new();
        for schema in [Schema::V1, Schema::V2] {
            let Some(sql) = list_sql(schema, &tables) else {
                continue;
            };
            for row in self.query(&sql)? {
                let Some(listed) = session_from_row(&row) else {
                    continue;
                };
                // V2 后读：一样晚时 2.x 覆盖 1.x。
                let newer = best
                    .get(listed.session.id.as_str())
                    .is_none_or(|seen| listed.updated >= seen.updated);
                if newer {
                    best.insert(listed.session.id.as_str().to_owned(), listed);
                }
            }
        }
        let hidden = self.trash.hidden_ids(Vendor::OpenCode)?;
        Ok(best
            .into_values()
            .filter(|l| l.listable)
            .map(|l| l.session)
            .filter(|s| !hidden.contains(s.id.as_str()))
            .collect())
    }

    /// 这条对话该从哪套表读（和列表去重的规则一样）；两套都没有返回 `None`。
    fn locate(&self, id: &str) -> io::Result<Option<Schema>> {
        let tables = self.tables()?;
        let pick = |table: &str| {
            if tables.contains(table) {
                format!("(select time_updated from {table} where id = '{id}')")
            } else {
                "null".to_owned()
            }
        };
        let sql = format!(
            "select {} as v1, {} as v2",
            pick("session"),
            pick("session_v2")
        );
        let rows = self.query(&sql)?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let v1 = row.get("v1").and_then(Value::as_i64);
        let v2 = row.get("v2").and_then(Value::as_i64);
        Ok(match (v1, v2) {
            (Some(a), Some(b)) if a > b => Some(Schema::V1),
            (_, Some(_)) => Some(Schema::V2),
            (Some(_), None) => Some(Schema::V1),
            (None, None) => None,
        })
    }

    /// 最后 `limit` 条消息（新的在前），各自拆成内容块。
    fn messages(&self, id: &str, limit: usize) -> io::Result<Vec<Message>> {
        let Some(schema) = self.locate(id)? else {
            return Ok(Vec::new());
        };
        match schema {
            Schema::V1 => self.messages_v1(id, limit),
            Schema::V2 => self.messages_v2(id, limit),
        }
    }

    /// 最后 `n` 条有字的消息（新的在前），只带字：一轮里只调了工具的那几步再多也跳过去，
    /// 工具的输入输出不读出来。
    fn previews(&self, id: &str, n: usize) -> io::Result<Vec<Message>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        match self.locate(id)? {
            Some(Schema::V1) => self.previews_v1(id, n),
            Some(Schema::V2) => self.previews_v2(id, n),
            None => Ok(Vec::new()),
        }
    }

    fn previews_v1(&self, id: &str, n: usize) -> io::Result<Vec<Message>> {
        let msgs = self.query(&format!(
            "select id, time_created, role from (select m.id, m.time_created, {role} as role from message m \
             where m.session_id = '{id}' and {live} \
             and exists (select 1 from part p where p.message_id = m.id and {text})) \
             where role in ('user', 'assistant') order by time_created desc, id desc limit {n}",
            role = field("m.data", "$.role"),
            live = live_v1(&self.tables()?, "m"),
            text = text_part_v1(),
        ))?;
        let ids = quoted_ids(&msgs);
        let mut texts: HashMap<String, Vec<String>> = HashMap::new();
        if !ids.is_empty() {
            add_texts(
                &mut texts,
                self.query(&format!(
                    "select p.message_id, {} as text from part p \
                     where p.message_id in ({ids}) and {} order by p.message_id, p.id",
                    field("p.data", "$.text"),
                    text_part_v1(),
                ))?,
            );
        }
        Ok(text_messages(&msgs, "role", |r| {
            texts.remove(str_col(r, "id")?)
        }))
    }

    fn previews_v2(&self, id: &str, n: usize) -> io::Result<Vec<Message>> {
        let split = self.tables()?.contains("session_part");
        let split_text = if split {
            format!(
                "exists (select 1 from session_part p where p.message_id = m.id and {}) or ",
                text_part_v2()
            )
        } else {
            String::new()
        };
        let msgs = self.query(&format!(
            "select m.id, m.type, m.time_created, case when m.type = 'user' then {user} end as text \
             from session_message m where m.session_id = '{id}' and ( \
             (m.type = 'user' and {user_said}) or \
             (m.type = 'assistant' and ({split_text}exists (select 1 from {INLINE_V2} where {inline})))) \
             order by m.seq desc limit {n}",
            user = field("m.data", "$.text"),
            user_said = nonblank(&field("m.data", "$.text")),
            inline = inline_text_v2(),
        ))?;
        let assistant: Vec<Row> = msgs
            .iter()
            .filter(|r| str_col(r, "type") == Some("assistant"))
            .cloned()
            .collect();
        let ids = quoted_ids(&assistant);
        let mut texts: HashMap<String, Vec<String>> = HashMap::new();
        if !ids.is_empty() {
            // 拆出来的块在前，库里还留着的内联块接在后面（同 `messages_v2`）。
            if split {
                add_texts(
                    &mut texts,
                    self.query(&format!(
                        "select p.message_id, {} as text from session_part p \
                         where p.message_id in ({ids}) and {} order by p.message_id, p.ordinal",
                        field("p.data", "$.text"),
                        text_part_v2(),
                    ))?,
                );
            }
            add_texts(
                &mut texts,
                self.query(&format!(
                    "select m.id as message_id, {} as text from session_message m, {INLINE_V2} \
                     where m.id in ({ids}) and {} order by m.id, cast(j.key as integer)",
                    field("j.value", "$.text"),
                    inline_text_v2(),
                ))?,
            );
        }
        Ok(text_messages(&msgs, "type", |r| {
            match str_col(r, "type")? {
                "user" => Some(vec![str_col(r, "text")?.to_owned()]),
                _ => texts.remove(str_col(r, "id")?),
            }
        }))
    }

    fn messages_v1(&self, id: &str, limit: usize) -> io::Result<Vec<Message>> {
        let msgs = self.query(&format!(
            "select m.id, m.time_created, {role} as role from message m \
             where m.session_id = '{id}' and {live} order by m.time_created desc, m.id desc limit {limit}",
            role = field("m.data", "$.role"),
            live = live_v1(&self.tables()?, "m"),
        ))?;
        let ids = quoted_ids(&msgs);
        let mut parts: HashMap<String, Vec<Value>> = HashMap::new();
        if !ids.is_empty() {
            for r in self.query(&format!(
                "select message_id, data from part where message_id in ({ids}) order by message_id, id"
            ))? {
                if let (Some(m), Some(d)) = (str_col(&r, "message_id"), json_col(&r, "data")) {
                    parts.entry(m.to_owned()).or_default().push(d);
                }
            }
        }
        Ok(msgs
            .iter()
            .filter_map(|r| {
                let role = role_of(str_col(r, "role")?)?;
                let blocks = parts
                    .remove(str_col(r, "id")?)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(block_v1)
                    .collect();
                Some(Message {
                    role,
                    at: millis(r.get("time_created")?),
                    blocks,
                })
            })
            .collect())
    }

    fn messages_v2(&self, id: &str, limit: usize) -> io::Result<Vec<Message>> {
        let msgs = self.query(&format!(
            "select id, type, time_created, data from session_message \
             where session_id = '{id}' and type in ('user', 'assistant') \
             order by seq desc limit {limit}"
        ))?;
        let ids = quoted_ids(&msgs);
        let mut parts: HashMap<String, Vec<Value>> = HashMap::new();
        if !ids.is_empty() && self.tables()?.contains("session_part") {
            for r in self.query(&format!(
                "select message_id, type, data from session_part where message_id in ({ids}) \
                 order by message_id, ordinal"
            ))? {
                let (Some(m), Some(mut d)) = (str_col(&r, "message_id"), json_col(&r, "data"))
                else {
                    continue;
                };
                // 拆出来的块把 type 放在列里，data 里可能没有。
                if let (Some(obj), Some(t)) = (d.as_object_mut(), str_col(&r, "type")) {
                    obj.entry("type").or_insert_with(|| Value::from(t));
                }
                parts.entry(m.to_owned()).or_default().push(d);
            }
        }
        Ok(msgs
            .iter()
            .filter_map(|r| {
                let role = role_of(str_col(r, "type")?)?;
                let data = json_col(r, "data").unwrap_or(Value::Null);
                let blocks = match role {
                    Role::User => user_text_v2(&data).map(Block::Text).into_iter().collect(),
                    // 拆出来的块在前，库里还留着的内联块接在后面（上游 `session/history.ts` withParts）。
                    Role::Assistant => parts
                        .remove(str_col(r, "id")?)
                        .unwrap_or_default()
                        .iter()
                        .chain(
                            data.get("content")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten(),
                        )
                        .filter_map(block_v2)
                        .collect(),
                };
                Some(Message {
                    role,
                    at: millis(r.get("time_created")?),
                    blocks,
                })
            })
            .collect())
    }
}

impl Source for OpenCodeSource {
    fn vendor(&self) -> Vendor {
        Vendor::OpenCode
    }

    fn scan(&self) -> Scan {
        let mut scan = Scan::default();
        if !self.db.is_file() {
            return scan;
        }
        match self.list() {
            Ok(sessions) => scan.sessions = sessions,
            Err(e) => scan.problems.push(format!("{}: {e}", self.db.display())),
        }
        scan
    }

    fn load_preview(&self, session: &Session, n: usize) -> io::Result<Vec<PreviewMessage>> {
        let Some(id) = sqlite::safe_id(session.id.as_str()) else {
            return Ok(Vec::new());
        };
        let mut out: Vec<PreviewMessage> = self
            .previews(id, n)?
            .into_iter()
            .filter_map(Message::preview)
            .map(jsonl::clip)
            .collect();
        out.reverse();
        Ok(out)
    }

    fn load_transcript(&self, session: &Session) -> io::Result<Transcript> {
        let Some(id) = sqlite::safe_id(session.id.as_str()) else {
            return Ok(Transcript::default());
        };
        let mut msgs = self.messages(id, HISTORY_ROWS + 1)?;
        let truncated = msgs.len() > HISTORY_ROWS;
        msgs.truncate(HISTORY_ROWS);
        msgs.reverse();
        Ok(Transcript {
            parts: msgs
                .into_iter()
                .flat_map(Message::parts)
                .map(transcript::clip)
                .collect(),
            truncated,
        })
    }

    fn count_messages(&self, session: &Session) -> io::Result<usize> {
        let Some(id) = sqlite::safe_id(session.id.as_str()) else {
            return Ok(0);
        };
        let sql = match self.locate(id)? {
            Some(Schema::V1) => format!(
                "select count(*) as n from message m where m.session_id = '{id}' \
                 and {} = 'user' and {}",
                field("m.data", "$.role"),
                live_v1(&self.tables()?, "m"),
            ),
            Some(Schema::V2) => format!(
                "select count(*) as n from session_message where session_id = '{id}' and type = 'user'"
            ),
            None => return Ok(0),
        };
        Ok(self
            .query(&sql)?
            .first()
            .and_then(|r| r.get("n"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize)
    }

    fn resume_cmd(&self, session: &Session) -> Command {
        let dir = session.cwd.to_string_lossy();
        let id = session.id.as_str();
        if dir.is_empty() {
            Command::new("opencode", &["-s", id], &session.cwd)
        } else {
            Command::new("opencode", &[&dir, "-s", id], &session.cwd)
        }
    }

    /// 带一句话接着聊：位置参数是目录，话要走 `--prompt`。写成 `--prompt=…`，话以 `-` 开头也不会被当成选项。
    fn resume_with_message_cmd(&self, session: &Session, message: &str) -> Command {
        let mut cmd = self.resume_cmd(session);
        cmd.args.push(format!("--prompt={message}"));
        cmd
    }

    /// `--prompt` 两版不一样：2.x 模型就绪后自己提交，1.x（v1.19.6 `routes/session/index.tsx`）只填进输入框、
    /// 要按回车。装的是哪版看这条对话在哪套表里（和读正文一样，见 [`OpenCodeSource::locate`]）：
    /// 在 2.x 的表里就是 2.x 在写它；只在 1.x 的表里、或 1.x 那份更新，就是 1.x 在用。读不了库按 1.x 报。
    fn message_delivery(&self, session: &Session) -> Delivery {
        let schema =
            sqlite::safe_id(session.id.as_str()).and_then(|id| self.locate(id).ok().flatten());
        match schema {
            Some(Schema::V2) => Delivery::Sent,
            Some(Schema::V1) | None => Delivery::Typed,
        }
    }

    fn new_cmd(&self, cwd: &Path) -> Command {
        Command::new("opencode", &[&cwd.to_string_lossy()], cwd)
    }

    fn created_at(&self, session: &Session) -> Option<SystemTime> {
        id_created(session.id.native(), session.last_active)
    }

    /// 只隐藏：对话在 OpenCode 自己的库里。
    fn trash(&self, session: &Session) -> io::Result<TrashEntry> {
        self.trash
            .hide(Vendor::OpenCode, session.id.as_str(), &session.path)
    }

    fn restore(&self, entry: &TrashEntry) -> io::Result<()> {
        self.trash.restore(entry)
    }

    /// 会话路径是空的：对话都在这个库里，看库和它的 WAL。
    fn moment_files(&self, _session: &Session) -> Vec<PathBuf> {
        super::with_wal(&self.db)
    }

    /// 监听库所在的目录（库文件可能还没建）。
    fn watch_roots(&self) -> Vec<PathBuf> {
        self.db
            .parent()
            .map(Path::to_path_buf)
            .into_iter()
            .collect()
    }

    /// 目录里还有日志、快照、别的频道的库；只有这个库和它的 WAL 影响列表。`-shm` 读的时候也会动，不算。
    fn affects(&self, path: &Path) -> bool {
        let (Some(dir), Some(name)) = (self.db.parent(), self.db.file_name()) else {
            return false;
        };
        let mut wal = name.to_os_string();
        wal.push("-wal");
        path.parent() == Some(dir) && path.file_name().is_some_and(|n| n == name || n == wal)
    }
}

/// 列出一套表里的对话：每行带 id、目录、标题、时间、用户发言数、第一句话。这套表不在返回 `None`。
fn list_sql(schema: Schema, tables: &HashSet<String>) -> Option<String> {
    let has = |t: &str| tables.contains(t);
    // 目录空（老对话）时退用项目根目录。
    let dir = |s: &str| {
        if has("project") {
            format!(
                "coalesce(nullif({s}.directory, ''), \
                 (select worktree from project where project.id = {s}.project_id), '')"
            )
        } else {
            format!("{s}.directory")
        }
    };
    match schema {
        Schema::V1 if has("session") && has("message") && has("part") => Some(format!(
            "select s.id, {dir} as directory, s.title, s.time_created, s.time_updated, \
             (select count(*) from message m where m.session_id = s.id \
              and {role} = 'user' and {live}) as users, \
             (select {text} from message m join part p on p.message_id = m.id \
              where m.session_id = s.id and {role} = 'user' and {live} and {text_part} \
              order by m.time_created, m.id, p.id limit 1) as first, \
             s.parent_id, s.time_archived from session s",
            dir = dir("s"),
            role = field("m.data", "$.role"),
            live = live_v1(tables, "m"),
            text = field("p.data", "$.text"),
            text_part = text_part_v1(),
        )),
        // 第一句取第一条有字的用户消息：第一条坏了（截断的 JSON）时用后面的。
        Schema::V2 if has("session_v2") && has("session_message") => Some(format!(
            "select s.id, {dir} as directory, s.title, s.time_created, s.time_updated, \
             (select count(*) from session_message m where m.session_id = s.id and m.type = 'user') as users, \
             (select {text} from session_message m \
              where m.session_id = s.id and m.type = 'user' and {said} order by m.seq limit 1) as first, \
             s.parent_id, s.time_archived from session_v2 s",
            dir = dir("s"),
            text = field("m.data", "$.text"),
            said = nonblank(&field("m.data", "$.text")),
        )),
        _ => None,
    }
}

/// 1.x 消息 `m` 没被撤销（在它那条对话 `session.revert.messageID` 之前；上游按 id 排先后）。老库没有这一列时总是真。
fn live_v1(tables: &HashSet<String>, m: &str) -> String {
    if !tables.contains("session.revert") {
        return "1".to_owned();
    }
    format!(
        "not exists (select 1 from session r where r.id = {m}.session_id and {m}.id >= {})",
        field("r.revert", "$.messageID")
    )
}

/// `json` 这列（坏的 JSON 当 null，不让整句报错：SQLite 的 `and` 不保证先判 `json_valid`）里 `path` 的值。
fn field(json: &str, path: &str) -> String {
    format!("json_extract(case when json_valid({json}) then {json} end, '{path}')")
}

/// `expr` 去掉空白后不是空的。
fn nonblank(expr: &str) -> String {
    format!("trim(coalesce({expr}, ''), ' ' || char(9, 10, 13)) <> ''")
}

/// 1.x 里算字的块（同 [`block_v1`]：系统塞的、被忽略的、空白的不算），`p` 是 `part`。
fn text_part_v1() -> String {
    format!(
        "{} = 'text' and coalesce({}, 0) = 0 and coalesce({}, 0) = 0 and {}",
        field("p.data", "$.type"),
        field("p.data", "$.synthetic"),
        field("p.data", "$.ignored"),
        nonblank(&field("p.data", "$.text"))
    )
}

/// 2.x 拆出来的字块（data 里有 type 以它为准，没有看 `type` 列，同 `messages_v2`），`p` 是 `session_part`。
fn text_part_v2() -> String {
    format!(
        "coalesce({}, p.type) = 'text' and {}",
        field("p.data", "$.type"),
        nonblank(&field("p.data", "$.text"))
    )
}

/// 2.x 助手消息 `m` 里还内联在 `data.content` 的块，`j` 是一块。
const INLINE_V2: &str = "json_each(case when json_valid(m.data) then m.data end, '$.content') j";

fn inline_text_v2() -> String {
    format!(
        "{} = 'text' and {}",
        field("j.value", "$.type"),
        nonblank(&field("j.value", "$.text"))
    )
}

/// 按 `message_id` 收下预览查询取出的字（`text` 列）。
fn add_texts(texts: &mut HashMap<String, Vec<String>>, rows: Vec<Row>) {
    for r in &rows {
        if let (Some(m), Some(t)) = (str_col(r, "message_id"), str_col(r, "text")) {
            texts.entry(m.to_owned()).or_default().push(t.to_owned());
        }
    }
}

/// 预览查询的结果拼成只有字块的消息：`role_col` 是角色列，`texts` 给每行的几段字（没有就不要这条）。
fn text_messages(
    rows: &[Row],
    role_col: &str,
    mut texts: impl FnMut(&Row) -> Option<Vec<String>>,
) -> Vec<Message> {
    rows.iter()
        .filter_map(|r| {
            Some(Message {
                role: role_of(str_col(r, role_col)?)?,
                at: millis(r.get("time_created")?),
                blocks: texts(r)?.iter().filter_map(|t| text_block(t)).collect(),
            })
        })
        .collect()
}

/// 一套表里的一行对话，去重以后再看 `listable`。
struct Listed {
    session: Session,
    /// `time_updated`，去重比新旧用。
    updated: i64,
    /// 列不列：子代理开的（`parent_id`）、归档的（`time_archived`）、一句没说的不列。
    listable: bool,
}

/// 一行列表结果 → [`Listed`]。没有 id 的返回 `None`。
fn session_from_row(row: &Row) -> Option<Listed> {
    let id = str_col(row, "id")?;
    let users = row.get("users").and_then(Value::as_u64).unwrap_or(0) as usize;
    let listable = users > 0
        && str_col(row, "parent_id").is_none()
        && row.get("time_archived").is_none_or(Value::is_null);
    let updated = row.get("time_updated").and_then(Value::as_i64).unwrap_or(0);
    let title = str_col(row, "title")
        .filter(|t| !is_fallback_title(t))
        .or_else(|| str_col(row, "first"))
        .map(|t| jsonl::one_line(t, 80));
    let last_active = [row.get("time_updated"), row.get("time_created")]
        .into_iter()
        .flatten()
        .filter_map(millis)
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let session = Session {
        vendor: Vendor::OpenCode,
        id: SessionId::from(id),
        cwd: PathBuf::from(str_col(row, "directory").unwrap_or_default()),
        // 不按文件组织：库是各条对话共用的。
        path: PathBuf::new(),
        title,
        git_branch: None,
        last_active,
        message_count: users,
        automatic: false,
        account: String::new(),
    };
    Some(Listed {
        session,
        updated,
        listable,
    })
}

/// OpenCode 没起标题前的占位标题 `New session - 2026-11-22T13:30:15.123Z`（子代理是 `Child session - …`），
/// 认法照上游 `cli-rs/crates/tui/src/features/title.rs` 的 is_fallback。
fn is_fallback_title(title: &str) -> bool {
    let Some(stamp) = title
        .strip_prefix("New session - ")
        .or_else(|| title.strip_prefix("Child session - "))
    else {
        return false;
    };
    let shape = b"dddd-dd-ddTdd:dd:dd.dddZ";
    stamp.len() == shape.len()
        && stamp.bytes().zip(shape).all(|(b, want)| match want {
            b'd' => b.is_ascii_digit(),
            other => b == *other,
        })
}

/// 一条消息：角色、时间、按先后的内容块。
struct Message {
    role: Role,
    at: Option<SystemTime>,
    blocks: Vec<Block>,
}

enum Block {
    Text(String),
    Tool {
        name: String,
        input: String,
        result: Option<String>,
    },
}

impl Message {
    /// 预览只要字：几块字连起来，没有字的（只调了工具）返回 `None`。
    fn preview(self) -> Option<PreviewMessage> {
        let text = self
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                Block::Tool { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = text.trim();
        (!text.is_empty()).then(|| PreviewMessage {
            role: self.role,
            text: text.to_owned(),
            at: self.at,
        })
    }

    fn parts(self) -> Vec<Part> {
        let (role, at) = (self.role, self.at);
        self.blocks
            .into_iter()
            .flat_map(|b| match b {
                Block::Text(text) => vec![Part::Message(PreviewMessage { role, text, at })],
                Block::Tool {
                    name,
                    input,
                    result,
                } => std::iter::once(Part::ToolCall { name, input })
                    .chain(result.map(|text| Part::ToolResult { text }))
                    .collect(),
            })
            .collect()
    }
}

/// 1.x 的一块内容：字（系统塞进去的 `synthetic`、被忽略的 `ignored` 不算）或工具调用（`state.output`/`state.error` 是返回）。
fn block_v1(part: &Value) -> Option<Block> {
    let flag = |k: &str| part.get(k).and_then(Value::as_bool).unwrap_or(false);
    match jsonl::str_field(part, "type")? {
        "text" if !flag("synthetic") && !flag("ignored") => {
            text_block(jsonl::str_field(part, "text")?)
        }
        "tool" => {
            let state = part.get("state").unwrap_or(&Value::Null);
            Some(Block::Tool {
                name: jsonl::str_field(part, "tool").unwrap_or("工具").to_owned(),
                input: transcript::input_summary(state.get("input").unwrap_or(&Value::Null)),
                result: ["output", "error"]
                    .into_iter()
                    .find_map(|k| jsonl::str_field(state, k))
                    .map(str::to_owned),
            })
        }
        _ => None,
    }
}

/// 2.x 助手消息的一块内容：`text`，或 `tool`（`state.content` 是返回，出错时 `state.error`）。
fn block_v2(part: &Value) -> Option<Block> {
    match jsonl::str_field(part, "type")? {
        "text" => text_block(jsonl::str_field(part, "text")?),
        "tool" => {
            let state = part.get("state").unwrap_or(&Value::Null);
            let result = state
                .get("content")
                .map(transcript::output_text)
                .filter(|t| !t.is_empty())
                .or_else(|| {
                    let e = state.get("error")?;
                    jsonl::str_field(e, "message")
                        .map(str::to_owned)
                        .or_else(|| e.as_str().map(str::to_owned))
                });
            Some(Block::Tool {
                name: jsonl::str_field(part, "name").unwrap_or("工具").to_owned(),
                input: transcript::input_summary(state.get("input").unwrap_or(&Value::Null)),
                result,
            })
        }
        _ => None,
    }
}

fn text_block(text: &str) -> Option<Block> {
    let t = text.trim();
    (!t.is_empty()).then(|| Block::Text(t.to_owned()))
}

/// 2.x 用户消息的字：`data.text`。
fn user_text_v2(data: &Value) -> Option<String> {
    let t = jsonl::str_field(data, "text")?.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

fn role_of(s: &str) -> Option<Role> {
    match s {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        _ => None,
    }
}

/// 拼进 `in (…)` 的消息 id 列表（只收安全的 id）。
fn quoted_ids(rows: &[Row]) -> String {
    rows.iter()
        .filter_map(|r| sqlite::safe_id(str_col(r, "id")?))
        .map(|id| format!("'{id}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// JSON 文本列（坏的 JSON 这一块跳过）。带孤立代理项转义（`\ud83d`）的 serde 不认，
/// 把那个转义换成 U+FFFD 再解，其余正文照留（同 [`sqlite::text`] 的换法）。
fn json_col(row: &Row, key: &str) -> Option<Value> {
    let text = row.get(key)?.as_str()?;
    serde_json::from_str(text)
        .or_else(|_| serde_json::from_str(&mend_surrogates(text)))
        .ok()
}

/// JSON 文本里不成对的 `\uD800`–`\uDFFF` 转义换成 `\ufffd`。`\\` 整个跳过，不会把字面的 `\u` 当转义。
fn mend_surrogates(json: &str) -> String {
    let b = json.as_bytes();
    let hex4 = |at: usize| {
        let h = std::str::from_utf8(b.get(at..at + 4)?).ok()?;
        u16::from_str_radix(h, 16).ok()
    };
    let high = |u: u16| (0xD800..0xDC00).contains(&u);
    let low = |u: u16| (0xDC00..0xE000).contains(&u);
    let mut out = String::with_capacity(json.len());
    let (mut last, mut i) = (0, 0);
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        let Some(u) = (b.get(i + 1) == Some(&b'u')).then(|| hex4(i + 2)).flatten() else {
            i += 2;
            continue;
        };
        let paired = high(u) && b.get(i + 6..i + 8) == Some(b"\\u") && hex4(i + 8).is_some_and(low);
        if paired {
            i += 12;
        } else if high(u) || low(u) {
            out.push_str(&json[last..i]);
            out.push_str("\\ufffd");
            i += 6;
            last = i;
        } else {
            i += 6;
        }
    }
    out.push_str(&json[last..]);
    out
}

/// 毫秒 → `SystemTime`。
fn millis(v: &Value) -> Option<SystemTime> {
    let ms = u64::try_from(v.as_i64()?).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
}

/// 低 36 位毫秒一圈约 795 天，见 [`id_created`]。
const ID_TIME_WRAP_MS: u64 = 1 << 36;

/// OpenCode 对话 id 是 `ses_` + 12 位十六进制 + 14 位 base62（上游 `schema/src/identifier.ts` 的 descending）：
/// 十六进制是 `!(毫秒 * 4096 + 计数)` 的低 48 位，只留下了毫秒的低 36 位（约 795 天一圈）。
/// 取不大于 `not_after`（这条对话的最后活动时间，id 生成一定在它之前）的那一圈，还原出生成它的毫秒。
/// 不是这种形状的返回 `None`。
pub fn id_created(id: &str, not_after: SystemTime) -> Option<SystemTime> {
    let hex = id.strip_prefix("ses_")?.get(..12)?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = u64::from_str_radix(hex, 16).ok()?;
    let low = (!value & ((1 << 48) - 1)) >> 12;
    let limit = u64::try_from(
        not_after
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()?;
    let behind = limit.checked_sub(low)? % ID_TIME_WRAP_MS;
    Some(SystemTime::UNIX_EPOCH + Duration::from_millis(limit - behind))
}

/// 认得出的 OpenCode 对话 id（`ses_` 开头，后面是字母数字）。
pub fn is_session_id(s: &str) -> bool {
    s.strip_prefix("ses_")
        .is_some_and(|rest| rest.len() >= 12 && rest.bytes().all(|b| b.is_ascii_alphanumeric()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按上游 identifier.ts 的算法生成 descending id 的时间部分。
    fn descending_hex(ms: u64, counter: u64) -> String {
        let value = !(ms * 0x1000 + counter);
        (0..6)
            .map(|i| format!("{:02x}", (value >> (40 - 8 * i)) & 0xff))
            .collect()
    }

    #[test]
    fn id_time_roundtrips() {
        let ms = 1_795_383_015_123u64; // 2026-11-22
        let id = format!("ses_{}AbCdEfGhIjKlMn", descending_hex(ms, 1));
        let at = |m: u64| SystemTime::UNIX_EPOCH + Duration::from_millis(m);
        assert_eq!(id_created(&id, at(ms + 60_000)), Some(at(ms)));
        assert_eq!(id_created(&id, at(ms)), Some(at(ms)));
        // 隔了一年多才看（不到一圈）也还原得对。
        assert_eq!(id_created(&id, at(ms + 400 * 86_400_000)), Some(at(ms)));
        assert_eq!(id_created("ses_zzzz", at(ms)), None);
        assert_eq!(id_created("20261122_213015_a1b2c3", at(ms)), None);
        assert!(is_session_id(&id));
        assert!(!is_session_id("ses_x' or 1"));
        assert!(!is_session_id("msg_0123456789abcdef"));
    }

    /// 用 tests/fixtures/opencode/opencode.sql 建库，再跑 `extra` 改几行。
    fn fixture_db(name: &str, extra: &str) -> (PathBuf, OpenCodeSource) {
        use std::io::Write;
        use std::process::Stdio;
        let dir = std::env::temp_dir().join(format!("huitai-oc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let sql = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode/opencode.sql"),
        )
        .unwrap();
        let mut child = std::process::Command::new("/usr/bin/sqlite3")
            .arg(&db)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(sql.as_bytes()).unwrap();
        stdin.write_all(extra.as_bytes()).unwrap();
        drop(stdin);
        assert!(child.wait().unwrap().success());
        let src = OpenCodeSource::new(&db, Trash::new(dir.join("trash")));
        (dir, src)
    }

    /// 两套都有的对话在更新的那套里被归档（或成了子代理的）：整条不列，另一套里的旧副本不能顶上来。
    #[test]
    fn archived_in_newer_schema_hides_the_older_copy() {
        const DUP_V2_NEWER: &str = "ses_e115d56bfffeDupV2newerAbCd";
        const DUP_V1_NEWER: &str = "ses_e114e147fffeDupV1newerAbCd";
        let (dir, src) = fixture_db(
            "archived",
            &format!(
                "update session_v2 set time_archived = time_updated + 1, time_updated = time_updated + 1 \
                 where id = '{DUP_V2_NEWER}';\n\
                 update session set time_archived = time_updated + 1, time_updated = time_updated + 1 \
                 where id = '{DUP_V1_NEWER}';\n"
            ),
        );
        let scan = src.scan();
        assert!(scan.problems.is_empty(), "{:?}", scan.problems);
        let ids: HashSet<&str> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(!ids.contains(DUP_V2_NEWER), "{ids:?}");
        assert!(!ids.contains(DUP_V1_NEWER), "{ids:?}");
        assert_eq!(ids.len(), 3, "别的对话照列：{ids:?}");

        // 旧的那套被归档、新的那套没有：照列，取新的那套。
        let (dir2, src) = fixture_db(
            "archived-old",
            &format!(
                "update session set time_archived = 1 where id = '{DUP_V2_NEWER}';\n\
                 update session_v2 set parent_id = 'ses_x' where id = '{DUP_V1_NEWER}';\n"
            ),
        );
        let scan = src.scan();
        let d2 = scan
            .sessions
            .iter()
            .find(|s| s.id.as_str() == DUP_V2_NEWER)
            .expect("新的那套没归档，照列");
        assert_eq!(d2.title.as_deref(), Some("修结算按钮（2.x）"));
        assert!(scan.sessions.iter().any(|s| s.id.as_str() == DUP_V1_NEWER));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    /// 审计 v0.1.3 第 04 条：`--prompt` 在 2.x 自己提交，1.x 只填进输入框。按这条对话在哪套表里报。
    #[test]
    fn message_delivery_follows_the_schema_that_owns_the_session() {
        let (dir, src) = fixture_db("delivery", "");
        let at = |id: &str| Session {
            vendor: Vendor::OpenCode,
            id: SessionId::from(id),
            cwd: PathBuf::from("/work/api"),
            path: PathBuf::new(),
            title: None,
            git_branch: None,
            last_active: SystemTime::UNIX_EPOCH,
            message_count: 1,
            automatic: false,
            account: String::new(),
        };
        for (id, want) in [
            ("ses_e113ed23fffeV2onlyAbCdEfGh", Delivery::Sent),
            ("ses_e115d56bfffeDupV2newerAbCd", Delivery::Sent),
            ("ses_e114e147fffeDupV1newerAbCd", Delivery::Typed),
            ("ses_e11a9a1ffffeV1onlyAbCdEfGh", Delivery::Typed),
            ("ses_x' or 1", Delivery::Typed),
        ] {
            assert_eq!(src.message_delivery(&at(id)), want, "{id}");
        }
        // 读不了库（没装、路径不对）：按 1.x 报，宁可多叫人按一次回车。
        let missing = OpenCodeSource::new(dir.join("nope/opencode.db"), Trash::new(dir.join("t")));
        assert_eq!(
            missing.message_delivery(&at("ses_e113ed23fffeV2onlyAbCdEfGh")),
            Delivery::Typed
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fallback_titles() {
        assert!(is_fallback_title("New session - 2026-11-22T13:30:15.123Z"));
        assert!(is_fallback_title(
            "Child session - 2026-11-22T13:30:15.123Z"
        ));
        assert!(!is_fallback_title("New session - 整理接口"));
        assert!(!is_fallback_title("修登录"));
    }

    #[test]
    fn resume_and_new_commands() {
        let src = OpenCodeSource::new("/h/opencode.db", Trash::new("/tmp/huitai-oc-trash"));
        let s = Session {
            vendor: Vendor::OpenCode,
            id: SessionId::from("ses_7f3a1b2c3d4eAbCdEfGhIjKlMn"),
            cwd: PathBuf::from("/work/api"),
            path: PathBuf::new(),
            title: None,
            git_branch: None,
            last_active: SystemTime::UNIX_EPOCH,
            message_count: 1,
            automatic: false,
            account: String::new(),
        };
        let cmd = src.resume_cmd(&s);
        assert_eq!(
            cmd.shell_line(),
            "opencode /work/api -s ses_7f3a1b2c3d4eAbCdEfGhIjKlMn"
        );
        assert_eq!(cmd.cwd, PathBuf::from("/work/api"));
        let cmd = src.resume_with_message_cmd(&s, "-继续上次没做完的事。");
        assert_eq!(
            cmd.args.last().map(String::as_str),
            Some("--prompt=-继续上次没做完的事。")
        );
        assert_eq!(
            src.new_cmd(Path::new("/work/my app")).shell_line(),
            "opencode '/work/my app'"
        );
        let no_dir = Session {
            cwd: PathBuf::new(),
            ..s
        };
        assert_eq!(
            src.resume_cmd(&no_dir).shell_line(),
            "opencode -s ses_7f3a1b2c3d4eAbCdEfGhIjKlMn"
        );
    }
}
