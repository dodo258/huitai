//! 只读查询厂商的 SQLite 库：进程内 rusqlite（和索引共用 bundled 的 SQLite），只读打开、从不写库，
//! 等锁最多 [`BUSY`]，一次查询最多跑 [`TIMEOUT`]（到点中断）。
//! WAL 库在厂商进程没开着时没有 `-shm`，所在目录又建不了（只读挂载、没有写权限）时，只读打开会失败
//! （`SQLITE_CANTOPEN`，或 `SQLITE_READONLY_DIRECTORY`），这时改用 `immutable=1` 再查一次：没有写入方，按不可变读是一致的。
//! 每行转成列名到值的 JSON 对象交给各家：字按原始字节取，不合法的 UTF-8 只坏那个字（见 [`text`]）；
//! 无穷大这类 JSON 表示不了的实数当 null。

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, OpenFlags};
use serde_json::{Map, Number, Value};

/// 厂商正在写库时最多等这么久。
const BUSY: Duration = Duration::from_secs(2);
/// 一次查询最多跑这么久。
const TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) type Row = Map<String, Value>;

pub(crate) fn query(db: &Path, sql: &str) -> io::Result<Vec<Row>> {
    query_within(db, sql, TIMEOUT)
}

fn query_within(db: &Path, sql: &str, timeout: Duration) -> io::Result<Vec<Row>> {
    let result = match run(db, false, sql, timeout) {
        Err(e)
            if matches!(
                e.sqlite_error_code(),
                Some(ErrorCode::CannotOpen | ErrorCode::ReadOnly)
            ) =>
        {
            run(db, true, sql, timeout)
        }
        other => other,
    };
    result.map_err(|e| match e.sqlite_error_code() {
        Some(ErrorCode::OperationInterrupted) => io::Error::new(
            io::ErrorKind::TimedOut,
            format!("SQLite 查询超过 {} 秒", timeout.as_secs_f32()),
        ),
        _ => io::Error::other(format!("SQLite: {e}")),
    })
}

fn run(db: &Path, immutable: bool, sql: &str, timeout: Duration) -> rusqlite::Result<Vec<Row>> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut conn = if immutable {
        Connection::open_with_flags(immutable_uri(db), flags | OpenFlags::SQLITE_OPEN_URI)?
    } else {
        Connection::open_with_flags(db, flags)?
    };
    conn.busy_timeout(BUSY)?;
    let deadline = Instant::now() + timeout;
    conn.progress_handler(1000, Some(move || Instant::now() >= deadline))?;
    let mut stmt = conn.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().map(str::to_owned).collect();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let mut row = Row::new();
        for (i, name) in names.iter().enumerate() {
            row.insert(name.clone(), value(r.get_ref(i)?));
        }
        out.push(row);
    }
    Ok(out)
}

fn value(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => Number::from_f64(f).map_or(Value::Null, Value::Number),
        ValueRef::Text(b) | ValueRef::Blob(b) => Value::String(text(b)),
    }
}

/// 库里的字节转成字。不是合法 UTF-8 的地方换成 U+FFFD：按字节截断的汉字，和 SQLite 把孤立代理项
/// `\ud800` 解出来的三个字节（`ED A0..BF xx`，换成一个）。
pub(crate) fn text(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_owned();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if let [0xED, 0xA0..=0xBF, 0x80..=0xBF, ..] = bytes[i..] {
            out.extend_from_slice("\u{FFFD}".as_bytes());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn immutable_uri(db: &Path) -> String {
    let mut uri = String::from("file:");
    for b in db.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri.push_str("?immutable=1");
    uri
}

/// 这张表现在有哪些列（厂商的表结构变得快，取列前先查）。
pub(crate) fn columns(db: &Path, table: &str) -> io::Result<HashSet<String>> {
    let rows = query(
        db,
        &format!("select name from pragma_table_info('{table}')"),
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| r.get("name")?.as_str().map(str::to_owned))
        .collect())
}

/// 文字列，去掉首尾空白；空的、不是文字的当没有。
pub(crate) fn str_col<'a>(row: &'a Row, key: &str) -> Option<&'a str> {
    row.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// 秒（可带小数）→ `SystemTime`；负数、非数和大到表示不了的（坏库、导入的数据）返回 `None`，不 panic：
/// 扫描在全家共用的索引线程里跑，一条坏时间不能让它退出。
pub(crate) fn secs(v: &Value) -> Option<SystemTime> {
    let s = v.as_f64().filter(|s| *s >= 0.0)?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::try_from_secs_f64(s).ok()?)
}

/// `hex()` 传出来的原始字节：按字节截的一段可能切在字中间，要拼回整段再转字时用（Antigravity）。
/// 不是十六进制返回 `None`。
pub(crate) fn hex_bytes(hex: &str) -> Option<Vec<u8>> {
    hex.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// 拼进 SQL 的 id 只允许字母数字、`-` 和 `_`（Hermes 的 id 带下划线），其余拒绝。
pub(crate) fn safe_id(id: &str) -> Option<&str> {
    (!id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn uri_escapes_special_bytes() {
        assert_eq!(
            immutable_uri(Path::new("/a b/c?#.sqlite")),
            "file:/a%20b/c%3F%23.sqlite?immutable=1"
        );
    }

    #[test]
    fn safe_id_rejects_quotes() {
        assert_eq!(safe_id("019b-AF"), Some("019b-AF"));
        assert_eq!(
            safe_id("20261122_213015_a1b2c3"),
            Some("20261122_213015_a1b2c3")
        );
        assert_eq!(safe_id("x' or 1=1 --"), None);
        assert_eq!(safe_id(""), None);
    }

    fn temp_db(name: &str, sql: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("huitai-sqlite-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("x.sqlite");
        Connection::open(&db).unwrap().execute_batch(sql).unwrap();
        db
    }

    /// WAL 库在写它的进程关掉以后没有 `-shm`，所在目录又不让建（如只读挂载）：只读打开报
    /// `SQLITE_READONLY_DIRECTORY`，退到 `immutable=1` 照样读得到。
    #[test]
    fn wal_without_shm_falls_back_to_immutable() {
        use std::os::unix::fs::PermissionsExt;
        let db = temp_db(
            "wal",
            "pragma journal_mode = wal; create table t(a); insert into t values (1);",
        );
        let dir = db.parent().unwrap().to_owned();
        assert!(!dir.join("x.sqlite-shm").exists());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let direct = run(&db, false, "select a from t", TIMEOUT).map_err(|e| e.sqlite_error_code());
        let rows = query(&db, "select a from t");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(direct, Err(Some(ErrorCode::ReadOnly)));
        assert_eq!(rows.unwrap(), vec![row(json!({ "a": 1 }))]);
    }

    fn row(v: Value) -> Row {
        v.as_object().unwrap().clone()
    }

    /// 坏 UTF-8 只坏那个字：按字节截断的汉字换成一个 U+FFFD，孤立代理项解出来的三个字节也只换一个；
    /// 无穷大当 null；二进制按字取。以前 `sqlite3 -json` 遇到前两种原样输出、整批结果解析不了，遇到无穷大输出裸 `Inf`。
    #[test]
    fn values_that_broke_the_command_line_output() {
        let db = temp_db("values", "create table t(a);");
        let rows = query(
            &db,
            "select cast(x'e68ea5e58f' as text) as cut, cast(x'61eda0bd62' as text) as lone, \
             1e999 as inf, -1e999 as ninf, x'e68ea5' as blob, 1.5 as real, 7 as int, null as none, \
             '{\"text\":\"a\\ud83db\"}' ->> '$.text' as extracted",
        )
        .unwrap();
        std::fs::remove_dir_all(db.parent().unwrap()).unwrap();
        assert_eq!(
            rows,
            vec![row(json!({
                "cut": "接\u{FFFD}",
                "lone": "a\u{FFFD}b",
                "inf": null,
                "ninf": null,
                "blob": "接",
                "real": 1.5,
                "int": 7,
                "none": null,
                "extracted": "a\u{FFFD}b",
            }))]
        );
    }

    /// 跑不完的查询到点中断，返回超时，不卡住扫描线程。
    #[test]
    fn long_query_is_interrupted() {
        let db = temp_db("slow", "create table t(a);");
        let started = Instant::now();
        let e = query_within(
            &db,
            "with recursive n(i) as (select 1 union all select i + 1 from n) select count(*) from n",
            Duration::from_millis(200),
        )
        .unwrap_err();
        std::fs::remove_dir_all(db.parent().unwrap()).unwrap();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut, "{e}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn missing_db_is_an_error() {
        let p = std::env::temp_dir().join(format!("huitai-nodb-{}/x.sqlite", std::process::id()));
        assert!(query(&p, "select 1").is_err());
    }
}
