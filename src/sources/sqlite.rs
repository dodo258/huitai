//! 只读查询 SQLite：调用系统自带的 `/usr/bin/sqlite3 -readonly -json`，不引入 SQLite 依赖。
//! WAL 库在厂商进程没开着时没有 `-shm`，只读打开会失败（错误 14），这时改用 `immutable=1` 再查一次：
//! 没有写入方，按不可变读是一致的。

use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

const TIMEOUT: Duration = Duration::from_secs(5);

/// 库路径和 SQL 前面的参数。`-init /dev/null` 顶掉 `~/.sqliterc`：用户的启动脚本可能打印东西或改输出格式，
/// 混进 `-json` 的结果就解析不了。
const ARGS: [&str; 6] = [
    "-init",
    "/dev/null",
    "-readonly",
    "-json",
    "-cmd",
    ".timeout 2000",
];

pub(crate) type Row = Map<String, Value>;

pub(crate) fn query(db: &Path, sql: &str) -> io::Result<Vec<Row>> {
    match run(&db.to_string_lossy(), sql) {
        Err(e) if e.to_string().contains("unable to open") => run(&immutable_uri(db), sql),
        other => other,
    }
}

fn run(target: &str, sql: &str) -> io::Result<Vec<Row>> {
    let bin = if Path::new("/usr/bin/sqlite3").exists() {
        "/usr/bin/sqlite3"
    } else {
        "sqlite3"
    };
    let mut child = Command::new(bin)
        .args(ARGS)
        .args([target, sql])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("启动 {bin} 失败: {e}")))?;
    let mut stdout = child.stdout.take().expect("stdout 已设为 piped");
    let mut stderr = child.stderr.take().expect("stderr 已设为 piped");
    let out = thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).map(|_| buf)
    });
    let err = thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });

    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("sqlite3 查询超过 {} 秒", TIMEOUT.as_secs()),
            ));
        }
        thread::sleep(Duration::from_millis(5));
    };
    let stdout = out
        .join()
        .map_err(|_| io::Error::other("读 sqlite3 输出的线程异常"))??;
    let stderr = err.join().unwrap_or_default();
    if !status.success() {
        return Err(io::Error::other(format!(
            "sqlite3 {target}: {}",
            stderr.trim()
        )));
    }
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    let rows: Vec<Value> = serde_json::from_slice(&stdout).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("sqlite3 输出不是 JSON: {e}"),
        )
    })?;
    Ok(rows
        .into_iter()
        .filter_map(|r| match r {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .collect())
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

/// 拼进 SQL 的 id 只允许字母数字和 `-`，其余拒绝。
pub(crate) fn safe_id(id: &str) -> Option<&str> {
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(safe_id("x' or 1=1 --"), None);
        assert_eq!(safe_id(""), None);
    }

    /// sqlite3 以最后一个 `-init` 为准：前面先塞一个会打印东西、改输出格式的启动脚本（等同 `~/.sqliterc`），
    /// 我们的参数要把它顶掉，输出还是干净的 JSON。
    #[test]
    fn startup_script_does_not_pollute_json() {
        let dir = std::env::temp_dir().join(format!("huitai-sqliterc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rc = dir.join("sqliterc");
        std::fs::write(&rc, ".print polluted\nselect 'from rc';\n.mode list\n").unwrap();
        let out = Command::new("/usr/bin/sqlite3")
            .arg("-init")
            .arg(&rc)
            .args(ARGS)
            .args([":memory:", "select 1 as a"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let rows: Vec<Value> = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
        assert_eq!(rows, vec![serde_json::json!({ "a": 1 })]);
    }

    #[test]
    fn missing_db_is_an_error() {
        let p = std::env::temp_dir().join(format!("huitai-nodb-{}/x.sqlite", std::process::id()));
        assert!(query(&p, "select 1").is_err());
    }
}
