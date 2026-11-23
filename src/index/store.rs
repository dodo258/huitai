//! 索引缓存：SQLite（WAL），一个库里三张表——
//! `sessions` 对话元数据（整条 `Session` 存 JSON），`files` 会话文件上次读时的 mtime 和大小，
//! `body` FTS5 全文索引（contentless，只存索引不存原文，行号 = `sessions.rid`；会话文件后来追加的部分
//! 另起一行追加进去，行号 = `rid + 第几段 << 32`，见 [`part_rowid`]）。厂商也是索引里的一列，
//! 搜索时用列过滤限定厂商：写成 `rowid in (select … from sessions)` 会让 FTS5 逐行回查，实测慢 100 倍以上。
//! 这是可重建的缓存：表结构版本不对或库坏了，删掉重建。
//!
//! `sessions`、`files` 每行记着读它的适配器解析版本（`parser`，见 `sources::parse_version`）：
//! 版本比自己旧的当没读过、重读；比自己新的（同时开着更新版的 `ai`）不覆盖。
//!
//! `sessions` 还记着精确计数数到会话文件哪里（`tally_*`，见 `sources::Progress`）和正文追加了几段
//! （`body_parts`）：文件只是变长时只读新增部分，见 `Index::index_bodies`。

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior, params};

use crate::core::{Session, Vendor};
use crate::sources::Progress;

pub(crate) const DB_NAME: &str = "index.sqlite";

/// 表结构改了就加一，旧缓存自动重建。只加列、旧版照样能用的改动不加一（同时开着新旧两版时，
/// 旧版见到版本不对会把整个库删了重建），在 [`add_columns`] 里给旧库补列。
const SCHEMA_VERSION: i64 = 1;

/// 后加的列：表、列、定义。旧版写入时不填，取默认值 0，新版当作没读过。
const ADDED_COLUMNS: [(&str, &str, &str); 9] = [
    ("sessions", "parser", "integer not null default 0"),
    ("files", "parser", "integer not null default 0"),
    ("sessions", "tally_pos", "integer"),
    ("sessions", "tally_users", "integer"),
    ("sessions", "tally_digest", "integer"),
    ("sessions", "body_parts", "integer not null default 0"),
    // 哪个账号的（`Account::key`，默认账号是空串）：同一家几个账号各扫各的，删没了的只删自己账号的。
    ("sessions", "account", "text not null default ''"),
    ("files", "account", "text not null default ''"),
    // 这个文件上次读出的对话 id（读出来不是对话时为空）：一条对话由几个文件读出时，没了其中一个，
    // 靠它找到同一条对话的其余文件重读。
    ("files", "session", "text"),
];

const SCHEMA: &str = "
create table sessions(
    rid integer primary key,
    vendor text not null,
    id text not null,
    file text,
    data text not null,
    last_active_ms integer not null,
    key text not null,
    body_key text,
    exact_count integer,
    parser integer not null default 0,
    tally_pos integer,
    tally_users integer,
    tally_digest integer,
    body_parts integer not null default 0,
    account text not null default '',
    unique(vendor, id)
);
create index sessions_file on sessions(file);
create table files(
    path text primary key,
    vendor text not null,
    mtime_ns integer not null,
    len integer not null,
    parser integer not null default 0,
    account text not null default '',
    session text
);
create virtual table body using fts5(
    vendor,
    text,
    tokenize = 'unicode61 remove_diacritics 2',
    content = '',
    contentless_delete = 1
);
insert into body(body, rank) values ('rank', 'bm25(0.0, 1.0)');
";

/// 缓存库坏了的错误前缀（种类是 `InvalidData`）：见到它就删掉重建，见 [`is_corrupt`]。
const CORRUPT: &str = "索引库坏了";

pub(crate) fn sql_err(e: rusqlite::Error) -> io::Error {
    if is_corrupt_sql(&e) {
        return io::Error::new(io::ErrorKind::InvalidData, format!("{CORRUPT}: {e}"));
    }
    io::Error::other(format!("索引库: {e}"))
}

/// 文件损坏、不是数据库、表或列没了（被别的程序改过）。
fn is_corrupt_sql(e: &rusqlite::Error) -> bool {
    match e {
        rusqlite::Error::SqliteFailure(f, msg) => {
            matches!(f.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
                || msg.as_deref().is_some_and(|m| {
                    m.starts_with("no such table") || m.starts_with("no such column")
                })
        }
        _ => false,
    }
}

/// 这个错误说明缓存库坏了，该删掉重建。
pub(crate) fn is_corrupt(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::InvalidData && e.to_string().starts_with(CORRUPT)
}

/// 完整检查一遍库（`quick_check`，12MB 的库约 50ms，放在后台线程里做）。
pub(crate) fn check(conn: &Connection) -> Result<(), String> {
    let rows: Vec<String> = conn
        .prepare("pragma quick_check")
        .and_then(|mut st| {
            st.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()
        })
        .map_err(|e| e.to_string())?;
    if rows.len() == 1 && rows[0] == "ok" {
        schema_ok(conn).map_err(|e| e.to_string())
    } else {
        Err(rows.into_iter().take(3).collect::<Vec<_>>().join("；"))
    }
}

/// 表和列都在（版本号对、表却被删了或改过时不对）。只编译语句不执行，几乎不花时间。
fn schema_ok(conn: &Connection) -> rusqlite::Result<()> {
    for sql in [
        "select rid, vendor, id, file, data, last_active_ms, key, body_key, exact_count, parser, \
         tally_pos, tally_users, tally_digest, body_parts, account from sessions limit 0",
        "select path, vendor, mtime_ns, len, parser, account, session from files limit 0",
        "select rowid, vendor, text from body limit 0",
    ] {
        conn.prepare(sql)?;
    }
    Ok(())
}

/// 文件上次读时的状态：mtime（纳秒）和大小。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub mtime_ns: i64,
    pub len: i64,
}

impl Stamp {
    pub(crate) fn of(path: &Path) -> io::Result<Self> {
        let m = fs::metadata(path)?;
        Ok(Self {
            mtime_ns: nanos(m.modified()?),
            len: i64::try_from(m.len()).unwrap_or(i64::MAX),
        })
    }
}

fn nanos(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn millis(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// 缓存里一个会话文件的记录：上次读时的状态、读它的解析版本和读出的对话 id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seen {
    pub stamp: Stamp,
    pub parser: u32,
    pub session: Option<String>,
}

/// 对话内容的「版本」：变了就要重建它的全文索引。带上解析版本（解析规则改了正文也要重取），
/// 会话文件在就带上文件的 mtime 和大小；库里的对话有正文修订号（[`crate::sources::Scan::revisions`]）就带上它，
/// 原地改了正文也算变了。
pub(crate) fn content_key(s: &Session, parser: u32, revision: Option<u64>) -> String {
    let file = Stamp::of(&s.path).map_or_else(
        |_| "-".to_owned(),
        |st| format!("{}:{}", st.mtime_ns, st.len),
    );
    let mut key = format!(
        "p{parser}:{}:{}:{file}",
        millis(s.last_active),
        s.message_count
    );
    if let Some(r) = revision {
        key += &format!(":r{r}");
    }
    key
}

/// 存进 `sessions.data` 的样子。
pub(crate) fn session_data(s: &Session) -> io::Result<String> {
    serde_json::to_string(s).map_err(io::Error::other)
}

/// 缓存里的一条对话。
pub(crate) struct Row {
    pub rid: i64,
    pub session: Session,
}

/// 等着建全文索引的一条对话。
pub(crate) struct Pending {
    pub rid: i64,
    pub key: String,
    pub session: Session,
    /// 上次精确计数数到哪了。
    pub progress: Option<Progress>,
    /// 正文已经追加了几段。
    pub parts: u32,
}

/// 写一条对话的正文：整份换掉，或在后面追加一段。
pub(crate) enum BodyWrite<'a> {
    Replace(&'a str),
    Append(&'a str),
}

/// 正文第 `part` 段在 `body` 里的行号：第 0 段就是 `rid`（旧版 `ai` 只认这一段），之后每段加 `part << 32`。
/// `rid` 是 `sessions` 的行号，到不了 2^32。
pub(crate) fn part_rowid(rid: i64, part: u32) -> i64 {
    rid + (i64::from(part) << 32)
}

/// 正文行号 → 对话行号。
const RID_MASK: i64 = (1 << 32) - 1;

/// 打开（必要时新建或重建）缓存库。
pub(crate) fn open(dir: &Path) -> io::Result<Connection> {
    let path = dir.join(DB_NAME);
    match open_checked(&path) {
        // 缓存文件坏了：删掉重建。库被锁等别的错误照常报，不删别的进程正在用的库。
        Err(rusqlite::Error::SqliteFailure(e, _))
            if matches!(e.code, ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) =>
        {
            remove_db(&path);
            open_checked(&path).map_err(sql_err)
        }
        other => other.map_err(sql_err),
    }
}

/// 只读打开缓存库（`ai list`、`ai search` 用）：不建、不重建、不补列。库还没有时报 `NotFound`，
/// 表结构版本不对、读要用的表或列不在时报 `InvalidData`，都请用户先开一次会话台。
pub(crate) fn open_read_only(dir: &Path) -> io::Result<Connection> {
    let path = dir.join(DB_NAME);
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("还没有索引（{}）", path.display()),
        ));
    }
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sql_err)?;
    conn.busy_timeout(Duration::from_secs(5)).map_err(sql_err)?;
    let version: i64 = conn
        .query_row("pragma user_version", [], |r| r.get(0))
        .map_err(sql_err)?;
    // 只查读列表、搜索要用的列：旧版 `ai` 建的库缺后加的列（[`ADDED_COLUMNS`]）照样能读。
    let readable = [
        "select rid, vendor, data, last_active_ms, key, body_key, exact_count from sessions limit 0",
        "select rowid, vendor, text from body limit 0",
    ]
    .iter()
    .all(|sql| conn.prepare(sql).is_ok());
    if version != SCHEMA_VERSION || !readable {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("索引是别的版本建的（{}）", path.display()),
        ));
    }
    Ok(conn)
}

/// 删掉 `dir` 里的缓存库（含 WAL）。调用前先关掉所有连接。
pub(crate) fn remove(dir: &Path) {
    remove_db(&dir.join(DB_NAME));
}

fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        let _ = fs::remove_file(PathBuf::from(p));
    }
}

fn open_checked(path: &Path) -> rusqlite::Result<Connection> {
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch("pragma journal_mode = wal; pragma synchronous = normal;")?;
    // 版本检查和建表在同一个写事务里，两个进程同时打开也只建一次。
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("pragma user_version", [], |r| r.get(0))?;
    if version == SCHEMA_VERSION {
        add_columns(&tx)?;
    }
    if version != SCHEMA_VERSION || schema_ok(&tx).is_err() {
        tx.execute_batch(&format!(
            "drop table if exists sessions; drop table if exists files; drop table if exists body;
             {SCHEMA}
             pragma user_version = {SCHEMA_VERSION};"
        ))?;
    }
    tx.commit()?;
    Ok(conn)
}

/// 给旧版建的库补上后加的列（表不在的不管，交给 [`schema_ok`] 判重建）。
fn add_columns(conn: &Connection) -> rusqlite::Result<()> {
    for (table, column, def) in ADDED_COLUMNS {
        let columns: Vec<String> = conn
            .prepare(&format!("pragma table_info({table})"))?
            .query_map([], |r| r.get(1))?
            .collect::<rusqlite::Result<_>>()?;
        if !columns.is_empty() && !columns.iter().any(|c| c == column) {
            conn.execute_batch(&format!("alter table {table} add column {column} {def}"))?;
        }
    }
    Ok(())
}

/// 一家的全部对话（缓存里的，按最后时间新的在前）。全文索引过的用精确消息数。
pub(crate) fn rows(conn: &Connection, vendor: Vendor) -> io::Result<Vec<Row>> {
    let mut st = conn
        .prepare_cached(
            "select rid, data, case when body_key = key then exact_count end
             from sessions where vendor = ?1 order by last_active_ms desc",
        )
        .map_err(sql_err)?;
    let rows = st
        .query_map([vendor_key(vendor)], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for row in rows {
        let (rid, data, exact) = row.map_err(sql_err)?;
        // 单条解析不了（不该发生，缓存是自己写的）就跳过，下次扫描会重写。
        let Ok(mut session) = serde_json::from_str::<Session>(&data) else {
            continue;
        };
        if let Some(n) = exact.and_then(|n| usize::try_from(n).ok()) {
            session.message_count = n;
        }
        out.push(Row { rid, session });
    }
    Ok(out)
}

/// 全文命中的对话行号，按相关度排序。`query` 是 `text::fts_query` 生成的查询（几个词用 `AND` 连起来）。
///
/// 一条对话的正文可能分成几段（几行），FTS5 的 `AND` 只在同一行里判断：每个词单独查、各自按对话合并，
/// 每个词都命中的对话才算，相关度取各词在这条对话里最好的那段相加。`keep` 为假的行（账号停用了的）
/// 在排序、限量之前去掉，不占 `limit` 的名额。`vendor` 为 `None` 时各家一起搜。
pub(crate) fn search_body(
    conn: &Connection,
    vendor: Option<Vendor>,
    query: &str,
    limit: usize,
    keep: impl Fn(i64) -> bool,
) -> io::Result<Vec<i64>> {
    let mut st = conn
        .prepare_cached(&format!(
            "select rowid & {RID_MASK} as rid, min(rank) from body where body match ?1 group by rid"
        ))
        .map_err(sql_err)?;
    let terms = and_terms(query);
    let mut found: HashMap<i64, (usize, f64)> = HashMap::new();
    for term in &terms {
        let q = match vendor {
            Some(v) => format!("vendor : {} AND text : ({term})", vendor_key(v)),
            None => format!("text : ({term})"),
        };
        let rows = st
            .query_map([q], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))
            .map_err(sql_err)?;
        for row in rows {
            let (rid, rank) = row.map_err(sql_err)?;
            let e = found.entry(rid).or_insert((0, 0.0));
            e.0 += 1;
            e.1 += rank;
        }
    }
    let mut hits: Vec<(i64, f64)> = found
        .into_iter()
        .filter(|(rid, (n, _))| *n == terms.len() && keep(*rid))
        .map(|(rid, (_, rank))| (rid, rank))
        .collect();
    // rank 越小越相关；同分按行号，结果稳定。
    hits.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    Ok(hits.into_iter().take(limit).map(|(rid, _)| rid).collect())
}

/// 把 `a AND b AND c` 拆成几个词；引号里的 ` AND `（短语里的字）不拆。引号内的 `""` 是转义的引号。
fn and_terms(query: &str) -> Vec<&str> {
    let bytes = query.as_bytes();
    let (mut out, mut start, mut quoted, mut i) = (Vec::new(), 0, false, 0);
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if quoted && bytes.get(i + 1) == Some(&b'"') {
                i += 2;
                continue;
            }
            quoted = !quoted;
        } else if !quoted && bytes[i..].starts_with(b" AND ") {
            out.push(&query[start..i]);
            i += 5;
            start = i;
            continue;
        }
        i += 1;
    }
    out.push(&query[start..]);
    out
}

/// 一家一个账号的会话文件上次读时的状态和解析版本。
pub(crate) fn stamps(
    conn: &Connection,
    vendor: Vendor,
    account: &str,
) -> io::Result<HashMap<PathBuf, Seen>> {
    let mut st = conn
        .prepare_cached(
            "select path, mtime_ns, len, parser, session from files where vendor = ?1 and account = ?2",
        )
        .map_err(sql_err)?;
    st.query_map(params![vendor_key(vendor), account], |r| {
        Ok((
            PathBuf::from(r.get::<_, String>(0)?),
            Seen {
                stamp: Stamp {
                    mtime_ns: r.get(1)?,
                    len: r.get(2)?,
                },
                parser: r.get(3)?,
                session: r.get(4)?,
            },
        ))
    })
    .map_err(sql_err)?
    .collect::<Result<_, _>>()
    .map_err(sql_err)
}

/// 缓存里一条对话上次写入的样子，见 [`known`]。
pub(crate) struct Known {
    /// 内容版本（[`content_key`]）。
    pub key: String,
    pub parser: u32,
    /// 整条 `Session` 的 JSON（[`Tx::upsert`] 写的）。
    pub data: String,
}

/// 一家一个账号现有的对话：id → 上次写入的内容版本、解析版本和元数据。
pub(crate) fn known(
    conn: &Connection,
    vendor: Vendor,
    account: &str,
) -> io::Result<HashMap<String, Known>> {
    let mut st = conn
        .prepare_cached(
            "select id, key, parser, data from sessions where vendor = ?1 and account = ?2",
        )
        .map_err(sql_err)?;
    st.query_map(params![vendor_key(vendor), account], |r| {
        Ok((
            r.get(0)?,
            Known {
                key: r.get(1)?,
                parser: r.get(2)?,
                data: r.get(3)?,
            },
        ))
    })
    .map_err(sql_err)?
    .collect::<Result<_, _>>()
    .map_err(sql_err)
}

/// 写事务里的操作。
pub(crate) struct Tx<'c>(rusqlite::Transaction<'c>);

impl<'c> Tx<'c> {
    pub(crate) fn begin(conn: &'c mut Connection) -> io::Result<Self> {
        conn.transaction().map(Tx).map_err(sql_err)
    }

    pub(crate) fn commit(self) -> io::Result<()> {
        self.0.commit().map_err(sql_err)
    }

    /// 写入或更新一条对话（`parser` 版本读的，`revision` 是库里的正文修订号）；行号不变，全文索引留着，
    /// 内容版本变了由全文索引那一步重建。缓存里这条是更新的解析版本写的就不动。
    pub(crate) fn upsert(
        &self,
        s: &Session,
        file: Option<&Path>,
        parser: u32,
        revision: Option<u64>,
    ) -> io::Result<()> {
        let data = session_data(s)?;
        let file = file.map(|f| f.to_string_lossy().into_owned());
        self.0
            .prepare_cached(
                "insert into sessions(vendor, id, file, data, last_active_ms, key, parser, account)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 on conflict(vendor, id) do update set
                   file = excluded.file, data = excluded.data,
                   last_active_ms = excluded.last_active_ms, key = excluded.key,
                   parser = excluded.parser, account = excluded.account
                 where sessions.parser <= excluded.parser",
            )
            .and_then(|mut st| {
                st.execute(params![
                    vendor_key(s.vendor),
                    s.id.as_str(),
                    file,
                    data,
                    millis(s.last_active),
                    content_key(s, parser, revision),
                    parser,
                    s.account
                ])
            })
            .map(drop)
            .map_err(sql_err)
    }

    /// 删掉来自这个文件、但 id 不是 `keep` 的对话（文件没了、不再是对话、或换了 id）。
    /// 只删解析版本不超过 `upto` 的：更新的版本读出来的结果不归旧版判断。
    pub(crate) fn remove_file_sessions(
        &self,
        file: &Path,
        keep: Option<&str>,
        upto: u32,
    ) -> io::Result<usize> {
        let file = file.to_string_lossy();
        self.delete_where(
            "file = ?1 and id is not ?2 and parser <= ?3",
            params![file.as_ref(), keep, upto],
        )
    }

    /// 删掉这一家这个账号里不在 `keep` 里、解析版本不超过 `upto` 的对话（整家重扫的来源用）。
    pub(crate) fn retain(
        &self,
        vendor: Vendor,
        account: &str,
        keep: &[&str],
        upto: u32,
    ) -> io::Result<usize> {
        let keep = serde_json::to_string(keep).map_err(io::Error::other)?;
        self.delete_where(
            "vendor = ?1 and account = ?4 and id not in (select value from json_each(?2)) \
             and parser <= ?3",
            params![vendor_key(vendor), keep, upto, account],
        )
    }

    fn delete_where(&self, cond: &str, args: impl rusqlite::Params + Copy) -> io::Result<usize> {
        let doomed: Vec<(i64, u32)> = self
            .0
            .prepare(&format!(
                "select rid, body_parts from sessions where {cond}"
            ))
            .and_then(|mut st| {
                st.query_map(args, |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()
            })
            .map_err(sql_err)?;
        for (rid, parts) in doomed {
            self.delete_body(rid, 0..=parts)?;
        }
        self.0
            .execute(&format!("delete from sessions where {cond}"), args)
            .map_err(sql_err)
    }

    /// 删掉一条对话正文的这几段。
    fn delete_body(&self, rid: i64, parts: impl Iterator<Item = u32>) -> io::Result<()> {
        let mut st = self
            .0
            .prepare_cached("delete from body where rowid = ?1")
            .map_err(sql_err)?;
        for part in parts {
            st.execute([part_rowid(rid, part)]).map_err(sql_err)?;
        }
        Ok(())
    }

    /// 记下会话文件这次读时的状态（`parser` 版本读的）和读出的对话 id；缓存里是更新的版本记的就不动。
    pub(crate) fn set_stamp(
        &self,
        vendor: Vendor,
        account: &str,
        path: &Path,
        (st, session): (Stamp, Option<&str>),
        parser: u32,
    ) -> io::Result<()> {
        self.0
            .prepare_cached(
                "insert into files(path, vendor, mtime_ns, len, parser, account, session)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 on conflict(path) do update set vendor = excluded.vendor,
                   mtime_ns = excluded.mtime_ns, len = excluded.len, parser = excluded.parser,
                   account = excluded.account, session = excluded.session
                 where files.parser <= excluded.parser",
            )
            .and_then(|mut s| {
                s.execute(params![
                    path.to_string_lossy(),
                    vendor_key(vendor),
                    st.mtime_ns,
                    st.len,
                    parser,
                    account,
                    session
                ])
            })
            .map(drop)
            .map_err(sql_err)
    }

    pub(crate) fn remove_stamp(&self, path: &Path) -> io::Result<()> {
        self.0
            .execute(
                "delete from files where path = ?1",
                [path.to_string_lossy()],
            )
            .map(drop)
            .map_err(sql_err)
    }

    /// 写一条对话的全文索引、精确消息数和数到哪了。只有内容版本还是 `p.key`、正文段数还是读的时候那样
    /// （建的过程中又变了、或另一个 `ai` 先写了）才写，否则什么都不动、留给下一轮，返回 `false`。
    pub(crate) fn set_body(
        &self,
        p: &Pending,
        write: BodyWrite<'_>,
        exact_count: Option<usize>,
        progress: Option<Progress>,
    ) -> io::Result<bool> {
        let parts = match write {
            BodyWrite::Replace(_) => 0,
            BodyWrite::Append("") => p.parts,
            BodyWrite::Append(_) => p.parts + 1,
        };
        let int = |n: usize| i64::try_from(n).ok();
        // 摘要按位存进有符号整数。
        let digest = progress.map(|t| t.digest as i64);
        let updated = self
            .0
            .prepare_cached(
                "update sessions set body_key = ?2, exact_count = ?3, tally_pos = ?4,
                   tally_users = ?5, tally_digest = ?6, body_parts = ?7
                 where rid = ?1 and key = ?2 and body_parts = ?8",
            )
            .and_then(|mut st| {
                st.execute(params![
                    p.rid,
                    p.key,
                    exact_count.and_then(int),
                    progress.and_then(|t| i64::try_from(t.pos).ok()),
                    progress.and_then(|t| int(t.users)),
                    digest,
                    parts,
                    p.parts
                ])
            })
            .map_err(sql_err)?;
        if updated == 0 {
            return Ok(false);
        }
        let text = match write {
            BodyWrite::Replace(text) => {
                self.delete_body(p.rid, 0..=p.parts)?;
                text
            }
            // 万一这一段的行号上留着东西（旧版 `ai` 删对话时不删追加的段）先清掉，不然插不进去。
            BodyWrite::Append(text) if !text.is_empty() => {
                self.delete_body(p.rid, std::iter::once(parts))?;
                text
            }
            BodyWrite::Append(_) => return Ok(true),
        };
        if !text.is_empty() {
            self.0
                .prepare_cached("insert into body(rowid, vendor, text) values (?1, ?2, ?3)")
                .and_then(|mut st| {
                    st.execute(params![
                        part_rowid(p.rid, parts),
                        vendor_key(p.session.vendor),
                        text
                    ])
                })
                .map_err(sql_err)?;
        }
        Ok(true)
    }
}

/// 内容版本变了、还没建全文索引的对话，最近活跃的在前。
pub(crate) fn pending_bodies(conn: &Connection, limit: usize) -> io::Result<Vec<Pending>> {
    let mut st = conn
        .prepare_cached(
            "select rid, key, data, tally_pos, tally_users, tally_digest, body_parts
             from sessions where body_key is not key
             order by last_active_ms desc limit ?1",
        )
        .map_err(sql_err)?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = st
        .query_map([limit], |r| {
            let tally = (
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<i64>>(5)?,
            );
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                tally,
                r.get::<_, u32>(6)?,
            ))
        })
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for row in rows {
        let (rid, key, data, tally, parts) = row.map_err(sql_err)?;
        let Ok(session) = serde_json::from_str(&data) else {
            continue;
        };
        let progress = match tally {
            (Some(pos), Some(users), Some(digest)) => Some(Progress {
                pos: u64::try_from(pos).unwrap_or(0),
                users: usize::try_from(users).unwrap_or(0),
                digest: digest as u64,
            }),
            _ => None,
        };
        out.push(Pending {
            rid,
            key,
            session,
            progress,
            parts,
        });
    }
    Ok(out)
}

pub(crate) fn vendor_key(v: Vendor) -> &'static str {
    v.key()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SessionId;

    fn session(count: usize) -> Session {
        Session {
            vendor: Vendor::Claude,
            id: SessionId::from("a"),
            cwd: PathBuf::from("/p"),
            path: PathBuf::from("/nonexistent/a.jsonl"),
            title: None,
            git_branch: None,
            last_active: UNIX_EPOCH,
            message_count: count,
            automatic: false,
            account: String::new(),
        }
    }

    #[test]
    fn and_terms_split_outside_quotes_only() {
        assert_eq!(and_terms(r#""a"*"#), [r#""a"*"#]);
        assert_eq!(
            and_terms(r#""api"* AND "登  录""#),
            [r#""api"*"#, r#""登  录""#]
        );
        // 短语里的 AND（「录AND录」切词后）和转义的引号都不拆。
        assert_eq!(
            and_terms(r#""录 AND 录" AND "say""hi AND x"*"#),
            [r#""录 AND 录""#, r#""say""hi AND x"*"#]
        );
    }

    /// 多个词分别出现在同一条对话的不同段里也算命中（FTS5 的 AND 只看同一行）；
    /// 只有一个词命中的对话不算；一条对话只出现一次。
    #[test]
    fn every_term_may_hit_a_different_part() {
        let dir = std::env::temp_dir().join(format!("huitai-store-and-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut conn = open(&dir).unwrap();
        let mut other = session(1);
        other.id = SessionId::from("b");
        let tx = Tx::begin(&mut conn).unwrap();
        tx.upsert(&session(1), None, 1, None).unwrap();
        tx.upsert(&other, None, 1, None).unwrap();
        tx.commit().unwrap();

        // a：第 0 段 alpha，追加一段 bravo；b：只有 alpha。正文建好的也重新拿出来写。
        let write = |conn: &mut Connection, id: &str, w: BodyWrite<'_>| {
            conn.execute("update sessions set body_key = null", [])
                .unwrap();
            let p = pending_bodies(conn, 10)
                .unwrap()
                .into_iter()
                .find(|p| p.session.id.as_str() == id)
                .unwrap();
            let tx = Tx::begin(conn).unwrap();
            assert!(tx.set_body(&p, w, Some(1), None).unwrap());
            tx.commit().unwrap();
        };
        write(&mut conn, "a", BodyWrite::Replace("alpha"));
        write(&mut conn, "b", BodyWrite::Replace("alpha"));
        write(&mut conn, "a", BodyWrite::Append("bravo"));

        let rid = |id: &str| {
            rows(&conn, Vendor::Claude)
                .unwrap()
                .into_iter()
                .find(|r| r.session.id.as_str() == id)
                .unwrap()
                .rid
        };
        let q = r#""alpha"* AND "bravo"*"#;
        assert_eq!(
            search_body(&conn, Some(Vendor::Claude), q, 10, |_| true).unwrap(),
            [rid("a")]
        );
        let mut both =
            search_body(&conn, Some(Vendor::Claude), r#""alpha"*"#, 10, |_| true).unwrap();
        both.sort();
        let mut want = vec![rid("a"), rid("b")];
        want.sort();
        assert_eq!(both, want);
        assert!(
            search_body(&conn, Some(Vendor::Pi), q, 10, |_| true)
                .unwrap()
                .is_empty()
        );
        drop(conn);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 旧版读文件期间新版先写进去了：旧版那一事务里的写入、删除都不生效。
    #[test]
    fn older_parser_writes_never_replace_newer_rows() {
        let dir = std::env::temp_dir().join(format!("huitai-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut conn = open(&dir).unwrap();
        let file = Path::new("/nonexistent/a.jsonl");
        let stamp = |len| Stamp { mtime_ns: 1, len };

        let tx = Tx::begin(&mut conn).unwrap();
        tx.upsert(&session(5), Some(file), 3, None).unwrap();
        tx.set_stamp(Vendor::Claude, "", file, (stamp(50), None), 3)
            .unwrap();
        tx.commit().unwrap();

        let tx = Tx::begin(&mut conn).unwrap();
        assert_eq!(tx.remove_file_sessions(file, None, 2).unwrap(), 0);
        assert_eq!(tx.retain(Vendor::Claude, "", &[], 2).unwrap(), 0);
        tx.upsert(&session(1), Some(file), 2, None).unwrap();
        tx.set_stamp(Vendor::Claude, "", file, (stamp(10), None), 2)
            .unwrap();
        tx.commit().unwrap();

        let kept = rows(&conn, Vendor::Claude).unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].session.message_count, 5);
        let seen = stamps(&conn, Vendor::Claude, "").unwrap()[file].clone();
        assert_eq!((seen.stamp, seen.parser), (stamp(50), 3));

        // 同版本、更新的版本照常覆盖。
        let tx = Tx::begin(&mut conn).unwrap();
        tx.upsert(&session(7), Some(file), 3, None).unwrap();
        tx.set_stamp(Vendor::Claude, "", file, (stamp(70), None), 4)
            .unwrap();
        tx.commit().unwrap();
        assert_eq!(
            rows(&conn, Vendor::Claude).unwrap()[0]
                .session
                .message_count,
            7
        );
        assert_eq!(stamps(&conn, Vendor::Claude, "").unwrap()[file].parser, 4);
        drop(conn);
        let _ = fs::remove_dir_all(&dir);
    }
}
