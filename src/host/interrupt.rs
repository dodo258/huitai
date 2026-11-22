//! 中断恢复的记录：`running.json` 记着有进程在跑的对话（原子写），正常退出就移除；
//! 守护进程启动时（已拿到锁，说明上一个已经不在）把里面剩下的转进 `interrupted.json`。

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::protocol::RunRecord;
use crate::core::SessionId;

pub fn running_path(dir: &Path) -> PathBuf {
    dir.join("running.json")
}

pub fn interrupted_path(dir: &Path) -> PathBuf {
    dir.join("interrupted.json")
}

/// 一组记录，按对话 id 排好、去重。
pub type Records = BTreeMap<SessionId, RunRecord>;

/// 读一份记录。文件不在算空；内容坏了报错（带路径）。
/// 文件可能是别的版本写的：不认识的字段忽略；单条读不懂（比如更新的版本加了新厂商）就跳过那条、记日志，其余照读。
pub fn load(path: &Path) -> io::Result<Records> {
    let text = match fs::read(path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Records::new()),
        Err(e) => return Err(with_path(e, path)),
    };
    let list: Vec<serde_json::Value> = serde_json::from_slice(&text).map_err(|e| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("{}: 格式不对: {e}", path.display()),
        )
    })?;
    Ok(list
        .into_iter()
        .filter_map(|v| match serde_json::from_value::<RunRecord>(v) {
            Ok(r) => Some((r.id.clone(), r)),
            Err(e) => {
                eprintln!(
                    "huitai daemon: {}: 跳过一条读不懂的记录: {e}",
                    path.display()
                );
                None
            }
        })
        .collect())
}

/// 原子写：先写同目录临时文件并落盘，再改名盖过去。没有记录时删掉文件。
pub fn save(path: &Path, records: &Records) -> io::Result<()> {
    if records.is_empty() {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(with_path(e, path)),
            _ => Ok(()),
        };
    }
    let list: Vec<&RunRecord> = records.values().collect();
    let json = serde_json::to_vec_pretty(&list).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| with_path(e, &tmp))?;
    f.write_all(&json).map_err(|e| with_path(e, &tmp))?;
    f.sync_all().map_err(|e| with_path(e, &tmp))?;
    fs::rename(&tmp, path).map_err(|e| with_path(e, path))
}

/// 守护进程启动时调用（必须已拿到锁）：上一个守护进程留在 `running.json` 里的条目并进「被中断」，
/// 清空 `running.json`，返回全部被中断的对话。`running.json` 坏了就当它是空的（记日志），不挡守护进程启动。
pub fn take_over(dir: &Path) -> io::Result<Records> {
    let running = running_path(dir);
    let left = load(&running).unwrap_or_else(|e| {
        eprintln!("huitai daemon: {e}，当作没有在跑的对话");
        Records::new()
    });
    let path = interrupted_path(dir);
    let mut interrupted = load(&path).unwrap_or_else(|e| {
        eprintln!("huitai daemon: {e}，被中断列表从空开始");
        Records::new()
    });
    if !left.is_empty() {
        interrupted.extend(left);
        save(&path, &interrupted)?;
    }
    save(&running, &Records::new())?;
    Ok(interrupted)
}

fn with_path(e: io::Error, path: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Vendor;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("huitai-int-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn rec(id: &str, at: u64) -> RunRecord {
        RunRecord {
            id: SessionId::from(id),
            vendor: Vendor::Codex,
            project: "shop".to_owned(),
            cwd: PathBuf::from("/p/shop"),
            started_at: at,
        }
    }

    fn records(list: &[RunRecord]) -> Records {
        list.iter().map(|r| (r.id.clone(), r.clone())).collect()
    }

    #[test]
    fn save_load_round_trips_and_empty_removes_file() {
        let d = dir("rt");
        let p = running_path(&d);
        let r = records(&[rec("a", 1), rec("b", 2)]);
        save(&p, &r).unwrap();
        assert_eq!(load(&p).unwrap(), r);
        assert!(!p.with_extension("json.tmp").exists());
        save(&p, &Records::new()).unwrap();
        assert!(!p.exists());
        assert!(load(&p).unwrap().is_empty());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn take_over_moves_running_into_interrupted() {
        let d = dir("take");
        save(
            &interrupted_path(&d),
            &records(&[rec("old", 1), rec("a", 1)]),
        )
        .unwrap();
        save(&running_path(&d), &records(&[rec("a", 5), rec("b", 6)])).unwrap();
        let got = take_over(&d).unwrap();
        // 同一条以新的那次为准，旧的被中断条目保留。
        assert_eq!(got, records(&[rec("old", 1), rec("a", 5), rec("b", 6)]));
        assert_eq!(load(&interrupted_path(&d)).unwrap(), got);
        assert!(!running_path(&d).exists());
        // 再启动一次（中间没有对话在跑）：不新增。
        assert_eq!(take_over(&d).unwrap(), got);
        fs::remove_dir_all(&d).unwrap();
    }

    /// 别的版本写的记录：多出来的字段忽略，读不懂的单条跳过，其余照读。
    #[test]
    fn records_from_other_versions_are_readable() {
        let d = dir("compat");
        let p = running_path(&d);
        fs::write(
            &p,
            r#"[
              {"id":"a","vendor":"claude","project":"shop","cwd":"/p/shop","started_at":7,"future_field":{"x":1}},
              {"id":"b","vendor":"not-a-vendor","project":"shop","cwd":"/p/shop","started_at":8},
              {"id":"c","vendor":"pi","project":"shop","cwd":"/p/shop","started_at":9}
            ]"#,
        )
        .unwrap();
        let got = load(&p).unwrap();
        assert_eq!(
            got.keys().map(SessionId::as_str).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(got[&SessionId::from("a")].vendor, Vendor::Claude);
        assert_eq!(got[&SessionId::from("c")].started_at, 9);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn broken_running_file_does_not_block_start() {
        let d = dir("broken");
        fs::write(running_path(&d), "{not json").unwrap();
        assert!(take_over(&d).unwrap().is_empty());
        assert!(!running_path(&d).exists());
        fs::remove_dir_all(&d).unwrap();
    }
}
