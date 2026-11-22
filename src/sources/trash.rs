//! 会话台回收站：删除 = 把厂商文件移进回收站（记下原路径），恢复 = 移回原处。
//! 不能移文件的厂商（Codex，见 DECISIONS.md）只记一条「隐藏」，扫描时跳过。
//! 每条一个目录：`<根>/<厂商>-<id>-<毫秒>/entry.json` + `files/` 里移进来的文件。
//! 进回收站超过保留天数（设置里的 `trash_days`）的条目连同 `files/` 一起清掉；只隐藏的不清（清了对话会冒回来）。

use std::collections::HashSet;
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::core::{Config, Vendor};

const ENTRY_FILE: &str = "entry.json";
const FILES_DIR: &str = "files";
/// 正在清的条目先改成这个前缀的名字（一步改名），清到一半被打断也不会再被当成条目列出来，下次接着删。
const PURGING_PREFIX: &str = ".purging-";
/// 清理出错记在回收站目录里，不打扰界面。
const PURGE_LOG: &str = "purge.log";

#[derive(Debug, Clone)]
pub struct Trash {
    root: PathBuf,
    /// 保留天数；0 表示永不清理。
    keep_days: u32,
}

/// 回收站里的一条。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashEntry {
    pub vendor: Vendor,
    pub id: String,
    pub trashed_at: SystemTime,
    /// 只隐藏，没有移动任何厂商文件。
    pub hide_only: bool,
    /// 上次恢复时原处已被占：排到最后，`u` 先恢复更早的，不被这一条卡住。
    #[serde(default)]
    pub blocked: bool,
    pub files: Vec<TrashedFile>,
    /// 这一条在回收站里的目录。
    #[serde(skip)]
    pub dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashedFile {
    pub original: PathBuf,
    pub stored: PathBuf,
}

impl Trash {
    /// 不自动清理的回收站；要按天数清用 [`Trash::keep_days`]。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            keep_days: 0,
        }
    }

    /// 进回收站超过 `days` 天的条目在回收站操作时清掉；0 表示永不清理。
    pub fn keep_days(mut self, days: u32) -> Self {
        self.keep_days = days;
        self
    }

    /// `~/Library/Application Support/huitai/trash`，保留天数取设置（读不了用默认）。
    /// 每个进程第一次取它时（会话台启动）清一次过期的。
    pub fn default_location() -> Option<Self> {
        let dir = Config::data_dir()?;
        let days = Config::default_path()
            .and_then(|p| Config::load(&p).ok())
            .unwrap_or_default()
            .trash_days;
        let trash = Self::new(dir.join("trash")).keep_days(days);
        static STARTUP: Once = Once::new();
        STARTUP.call_once(|| trash.purge_expired_logged(SystemTime::now()));
        Some(trash)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 把 `files`（文件或目录，不存在的跳过）移进回收站。
    /// 中途失败把已移的移回原处，原文件都在；连移回都失败的（磁盘出错）留在回收站、清单里记着，可恢复。
    pub fn move_in(&self, vendor: Vendor, id: &str, files: &[PathBuf]) -> io::Result<TrashEntry> {
        let now = SystemTime::now();
        let entry = self.move_in_with(vendor, id, files, now, &rename_excl)?;
        self.purge_expired_logged(now);
        Ok(entry)
    }

    fn move_in_with(
        &self,
        vendor: Vendor,
        id: &str,
        files: &[PathBuf],
        now: SystemTime,
        mv: &Mover,
    ) -> io::Result<TrashEntry> {
        let present: Vec<&PathBuf> = files.iter().filter(|p| present(p)).collect();
        if present.is_empty() {
            return Err(io::Error::new(
                ErrorKind::NotFound,
                format!("{vendor} 对话 {id} 的文件不存在"),
            ));
        }
        let mut entry = self.new_entry(vendor, id, false, now)?;
        let store = entry.dir.join(FILES_DIR);
        entry.files = present
            .iter()
            .enumerate()
            .map(|(i, p)| TrashedFile {
                original: (*p).clone(),
                stored: store.join(format!("{i}-{}", file_name(p))),
            })
            .collect();
        // 先写清单再移动：任何时候都能从清单找回原路径。
        let ready = fs::create_dir(&store)
            .map_err(|e| ctx(e, &store))
            .and_then(|()| write_entry(&entry));
        if let Err(e) = ready {
            let _ = remove_entry_dir(&entry.dir);
            return Err(e);
        }
        let pairs: Vec<(&Path, &Path)> = entry
            .files
            .iter()
            .map(|f| (f.original.as_path(), f.stored.as_path()))
            .collect();
        let Err(fail) = move_all(&pairs, mv) else {
            return Ok(entry);
        };
        let err = moved_error("移进回收站", &fail, pairs[fail.at].0);
        if fail.stuck.is_empty() {
            let _ = remove_entry_dir(&entry.dir);
            return Err(err);
        }
        // 移不回去的留在回收站，清单只记它们。
        let stuck_names = fail
            .stuck
            .iter()
            .map(|&i| entry.files[i].original.display().to_string())
            .collect::<Vec<_>>()
            .join("、");
        entry.files = keep_only(entry.files, &fail.stuck);
        let _ = write_entry(&entry);
        Err(io::Error::new(
            err.kind(),
            format!("{err}；{stuck_names} 没能移回原处，在回收站里，按 u 可恢复"),
        ))
    }

    /// 只记一条隐藏，不动厂商文件。
    pub fn hide(&self, vendor: Vendor, id: &str, original: &Path) -> io::Result<TrashEntry> {
        let mut entry = self.new_entry(vendor, id, true, SystemTime::now())?;
        entry.files = vec![TrashedFile {
            original: original.to_path_buf(),
            stored: PathBuf::new(),
        }];
        if let Err(e) = write_entry(&entry) {
            let _ = remove_entry_dir(&entry.dir);
            return Err(e);
        }
        Ok(entry)
    }

    /// 移回原处并删掉这一条。按两边实况逐个处理：还在回收站、原处空着的移回去；回收站里已经没有的
    /// （删除或恢复到一半被打断时已在原处的）跳过。回收站里有、原处也被占（含目录、断掉的链接）的报错不覆盖，
    /// 这一条记为卡住、排到最后；中途失败把已移回的再放回回收站，可再试。
    pub fn restore(&self, entry: &TrashEntry) -> io::Result<()> {
        self.restore_with(entry, &rename_excl)
    }

    fn restore_with(&self, entry: &TrashEntry, mv: &Mover) -> io::Result<()> {
        if entry.hide_only {
            return remove_entry_dir(&entry.dir);
        }
        let todo: Vec<&TrashedFile> = entry.files.iter().filter(|f| present(&f.stored)).collect();
        if let Some(f) = todo.iter().find(|f| present(&f.original)) {
            let mut blocked = entry.clone();
            blocked.blocked = true;
            let _ = write_entry(&blocked);
            return Err(io::Error::new(
                ErrorKind::AlreadyExists,
                format!(
                    "{} 已存在，不覆盖；这一条排到最后，再按 u 先恢复更早的",
                    f.original.display()
                ),
            ));
        }
        for f in &todo {
            if let Some(parent) = f.original.parent() {
                fs::create_dir_all(parent).map_err(|e| ctx(e, parent))?;
            }
        }
        let pairs: Vec<(&Path, &Path)> = todo
            .iter()
            .map(|f| (f.stored.as_path(), f.original.as_path()))
            .collect();
        match move_all(&pairs, mv) {
            Ok(()) => remove_entry_dir(&entry.dir),
            // 回到原处又放不回回收站的（stuck），下次恢复按实况跳过，清单不用改。
            Err(fail) => Err(moved_error("从回收站恢复", &fail, pairs[fail.at].0)),
        }
    }

    /// 清掉进回收站超过保留天数的条目，出错记进 `purge.log`，不影响使用。
    fn purge_expired_logged(&self, now: SystemTime) {
        let problems = match self.purge_expired(now) {
            Ok(p) => p,
            Err(e) => vec![e.to_string()],
        };
        if problems.is_empty() {
            return;
        }
        let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let lines: String = problems.iter().map(|p| format!("{secs} {p}\n")).collect();
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join(PURGE_LOG))
            .and_then(|mut f| f.write_all(lines.as_bytes()));
    }

    /// 清掉进回收站超过保留天数的条目（只删回收站自己目录里的东西，原路径一概不碰），返回清不掉的原因。
    /// 只隐藏的条目不清：清了那条对话就冒回来了。
    fn purge_expired(&self, now: SystemTime) -> io::Result<Vec<String>> {
        if self.keep_days == 0 {
            return Ok(Vec::new());
        }
        let keep = Duration::from_secs(u64::from(self.keep_days) * 86_400);
        let mut problems = Vec::new();
        let expired = self
            .list()?
            .into_iter()
            .filter(|e| !e.hide_only && now.duration_since(e.trashed_at).is_ok_and(|a| a > keep));
        for e in expired {
            let Some(name) = e.dir.file_name() else {
                continue;
            };
            let to = self
                .root
                .join(format!("{PURGING_PREFIX}{}", name.to_string_lossy()));
            if let Err(err) = fs::rename(&e.dir, &to) {
                problems.push(ctx(err, &e.dir).to_string());
            }
        }
        // 改过名的连同上次清到一半的一起删。
        let rd = match fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(problems),
            Err(e) => return Err(ctx(e, &self.root)),
        };
        for d in rd.flatten() {
            if !d.file_name().to_string_lossy().starts_with(PURGING_PREFIX) {
                continue;
            }
            // remove_dir_all 不跟随符号链接：files/ 里的链接只删链接本身。
            if let Err(e) = fs::remove_dir_all(d.path()) {
                problems.push(ctx(e, &d.path()).to_string());
            }
        }
        Ok(problems)
    }

    /// 回收站里全部条目，新的在前（恢复时卡住的排最后）。清单坏了的条目跳过。
    pub fn list(&self) -> io::Result<Vec<TrashEntry>> {
        let rd = match fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(ctx(e, &self.root)),
        };
        let mut out: Vec<TrashEntry> = rd
            .flatten()
            .filter(|d| !d.file_name().to_string_lossy().starts_with(PURGING_PREFIX))
            .filter_map(|d| read_entry(&d.path()))
            .collect();
        out.sort_by_key(|e| (e.blocked, std::cmp::Reverse(e.trashed_at)));
        Ok(out)
    }

    /// 某家被隐藏（只隐藏不移动）的对话 id。
    pub fn hidden_ids(&self, vendor: Vendor) -> io::Result<HashSet<String>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|e| e.vendor == vendor && e.hide_only)
            .map(|e| e.id)
            .collect())
    }

    /// 建这一条的目录。同一毫秒删同一条（或 id 清洗后同名）时目录名加序号，绝不和已有的条目共用目录。
    fn new_entry(
        &self,
        vendor: Vendor,
        id: &str,
        hide_only: bool,
        now: SystemTime,
    ) -> io::Result<TrashEntry> {
        let ms = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let safe: String = id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        fs::create_dir_all(&self.root).map_err(|e| ctx(e, &self.root))?;
        let base = format!("{}-{safe}-{ms}", vendor_key(vendor));
        let mut n = 0;
        let dir = loop {
            let name = if n == 0 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            let dir = self.root.join(name);
            match fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == ErrorKind::AlreadyExists && n < 1000 => n += 1,
                Err(e) => return Err(ctx(e, &dir)),
            }
        };
        Ok(TrashEntry {
            vendor,
            id: id.to_owned(),
            trashed_at: now,
            hide_only,
            blocked: false,
            files: Vec::new(),
            dir,
        })
    }
}

/// 改名的做法；测试时换成会在指定一步失败的假实现（模拟磁盘满、跨卷、权限不足）。
type Mover = dyn Fn(&Path, &Path) -> io::Result<()>;

/// 原子改名，`to` 已存在（文件、目录、断掉的链接）时报 `AlreadyExists`，绝不覆盖。
/// macOS 的 `renamex_np(RENAME_EXCL)`：检查和改名是一步，不会在两者之间被别的进程抢先写进同名文件。
fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "路径里有 NUL"))
    };
    let (f, t) = (c(from)?, c(to)?);
    // SAFETY: 两个指针都指向活着的、以 NUL 结尾的 CString。
    let rc = unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// 一组改名中途失败。
struct MoveFailure {
    /// 第几个失败了。
    at: usize,
    error: io::Error,
    /// 之前已经改过名、又没能改回去的（还在目标位置）。
    stuck: Vec<usize>,
}

/// 依次改名 `from → to`；第 k 个失败时把前面 k 个倒着改回去，全成或全不成（改回也失败的记在 `stuck`）。
fn move_all(pairs: &[(&Path, &Path)], mv: &Mover) -> Result<(), MoveFailure> {
    for (i, (from, to)) in pairs.iter().enumerate() {
        if let Err(error) = mv(from, to) {
            let stuck = (0..i)
                .rev()
                .filter(|&j| mv(pairs[j].1, pairs[j].0).is_err())
                .collect();
            return Err(MoveFailure {
                at: i,
                error,
                stuck,
            });
        }
    }
    Ok(())
}

fn moved_error(what: &str, fail: &MoveFailure, path: &Path) -> io::Error {
    let e = &fail.error;
    let why = match e.kind() {
        ErrorKind::CrossesDevices => "对话文件和回收站不在同一个磁盘上，没有移动".to_owned(),
        ErrorKind::StorageFull => "磁盘满了".to_owned(),
        ErrorKind::PermissionDenied => "没有权限".to_owned(),
        _ => e.to_string(),
    };
    io::Error::new(
        e.kind(),
        format!(
            "{what}失败（第 {} 个文件 {}）：{why}",
            fail.at + 1,
            path.display()
        ),
    )
}

fn keep_only(files: Vec<TrashedFile>, idx: &[usize]) -> Vec<TrashedFile> {
    files
        .into_iter()
        .enumerate()
        .filter(|(i, _)| idx.contains(i))
        .map(|(_, f)| f)
        .collect()
}

/// 路径上有东西（含断掉的符号链接）。
fn present(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn vendor_key(v: Vendor) -> &'static str {
    v.key()
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map_or_else(|| "item".to_owned(), |n| n.to_string_lossy().into_owned())
}

/// 清单先写临时文件再改名替换：写到一半磁盘满了，旧清单还在。
fn write_entry(entry: &TrashEntry) -> io::Result<()> {
    let path = entry.dir.join(ENTRY_FILE);
    let tmp = entry.dir.join(format!("{ENTRY_FILE}.tmp"));
    let json = serde_json::to_vec_pretty(entry).map_err(io::Error::other)?;
    let written = fs::write(&tmp, json).and_then(|()| fs::rename(&tmp, &path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.map_err(|e| ctx(e, &path))
}

fn read_entry(dir: &Path) -> Option<TrashEntry> {
    let bytes = fs::read(dir.join(ENTRY_FILE)).ok()?;
    let mut entry: TrashEntry = serde_json::from_slice(&bytes).ok()?;
    entry.dir = dir.to_path_buf();
    Some(entry)
}

/// 只删回收站自己的清单和空目录。`files/` 里还有东西就报错且清单留着，绝不连带删厂商文件、也不丢清单。
fn remove_entry_dir(dir: &Path) -> io::Result<()> {
    let files = dir.join(FILES_DIR);
    match fs::remove_dir(&files) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(ctx(e, &files)),
        _ => {}
    }
    for name in [ENTRY_FILE.to_owned(), format!("{ENTRY_FILE}.tmp")] {
        let p = dir.join(name);
        match fs::remove_file(&p) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(ctx(e, &p)),
            _ => {}
        }
    }
    fs::remove_dir(dir).map_err(|e| ctx(e, dir))
}

fn ctx(e: io::Error, path: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("huitai-trash-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn move_in_and_restore_round_trip() {
        let d = temp("roundtrip");
        let file = d.join("vendor/proj/s1.jsonl");
        let side = d.join("vendor/proj/s1");
        fs::create_dir_all(&side).unwrap();
        fs::write(&file, b"{}\n").unwrap();
        fs::write(side.join("sub.jsonl"), b"x").unwrap();

        let trash = Trash::new(d.join("trash"));
        let entry = trash
            .move_in(
                Vendor::Claude,
                "s1",
                &[file.clone(), side.clone(), d.join("missing")],
            )
            .unwrap();
        assert!(!file.exists() && !side.exists());
        assert_eq!(entry.files.len(), 2);
        assert_eq!(trash.list().unwrap(), vec![entry.clone()]);

        trash.restore(&entry).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"{}\n");
        assert_eq!(fs::read(side.join("sub.jsonl")).unwrap(), b"x");
        assert!(trash.list().unwrap().is_empty());
        assert!(!entry.dir.exists());
    }

    #[test]
    fn restore_refuses_to_overwrite() {
        let d = temp("overwrite");
        let file = d.join("s.jsonl");
        fs::write(&file, b"old").unwrap();
        let trash = Trash::new(d.join("trash"));
        let entry = trash
            .move_in(Vendor::Pi, "s", std::slice::from_ref(&file))
            .unwrap();
        fs::write(&file, b"new").unwrap();
        let err = trash.restore(&entry).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&file).unwrap(), b"new");
        assert_eq!(trash.list().unwrap().len(), 1);
    }

    #[test]
    fn missing_files_is_not_found() {
        let d = temp("missing");
        let trash = Trash::new(d.join("trash"));
        let err = trash
            .move_in(Vendor::Claude, "x", &[d.join("nope.jsonl")])
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(trash.list().unwrap().is_empty());
    }

    #[test]
    fn hide_only_keeps_file_and_lists_id() {
        let d = temp("hide");
        let file = d.join("rollout.jsonl");
        fs::write(&file, b"{}").unwrap();
        let trash = Trash::new(d.join("trash"));
        let entry = trash.hide(Vendor::Codex, "t1", &file).unwrap();
        assert!(file.exists());
        assert!(trash.hidden_ids(Vendor::Codex).unwrap().contains("t1"));
        assert!(trash.hidden_ids(Vendor::Pi).unwrap().is_empty());
        trash.restore(&entry).unwrap();
        assert!(trash.hidden_ids(Vendor::Codex).unwrap().is_empty());
        assert!(file.exists());
    }

    use std::cell::Cell;
    use std::os::unix::fs::PermissionsExt;

    /// 假的改名：第 `fail_at` 次调用起（含）依次报 `errs` 里的错误码，用完后照常改名。
    fn flaky(fail_at: usize, errs: Vec<i32>) -> impl Fn(&Path, &Path) -> io::Result<()> {
        let calls = Cell::new(0);
        let errs = std::cell::RefCell::new(errs.into_iter());
        move |from: &Path, to: &Path| {
            let n = calls.get();
            calls.set(n + 1);
            if n >= fail_at
                && let Some(code) = errs.borrow_mut().next()
            {
                return Err(io::Error::from_raw_os_error(code));
            }
            rename_excl(from, to)
        }
    }

    fn two_files(d: &Path) -> Vec<PathBuf> {
        let file = d.join("vendor/proj/s1.jsonl");
        let side = d.join("vendor/proj/s1");
        fs::create_dir_all(&side).unwrap();
        fs::write(&file, b"{\"a\":1}\n").unwrap();
        fs::write(side.join("sub.jsonl"), b"sub").unwrap();
        vec![file, side]
    }

    fn assert_originals(files: &[PathBuf]) {
        assert_eq!(fs::read(&files[0]).unwrap(), b"{\"a\":1}\n");
        assert_eq!(fs::read(files[1].join("sub.jsonl")).unwrap(), b"sub");
    }

    /// 第二个文件移动时磁盘满、跨卷、没权限：第一个移回原处，回收站里不留半条。
    #[test]
    fn failure_midway_puts_everything_back() {
        for code in [libc::ENOSPC, libc::EXDEV, libc::EACCES, libc::EIO] {
            let d = temp(&format!("midway-{code}"));
            let files = two_files(&d);
            let trash = Trash::new(d.join("trash"));
            let err = trash
                .move_in_with(
                    Vendor::Claude,
                    "s1",
                    &files,
                    SystemTime::now(),
                    &flaky(1, vec![code]),
                )
                .unwrap_err();
            assert!(err.to_string().contains("第 2 个文件"), "{err}");
            assert_originals(&files);
            assert!(trash.list().unwrap().is_empty());
            assert_eq!(fs::read_dir(trash.root()).unwrap().count(), 0, "不留空目录");
        }
        let d = temp("exdev-msg");
        let files = two_files(&d);
        let err = Trash::new(d.join("trash"))
            .move_in_with(
                Vendor::Claude,
                "s1",
                &files,
                SystemTime::now(),
                &flaky(0, vec![libc::EXDEV]),
            )
            .unwrap_err();
        assert!(err.to_string().contains("不在同一个磁盘上"), "{err}");
        assert_originals(&files);
    }

    /// 移第二个失败、把第一个移回去也失败（磁盘出错）：第一个留在回收站且清单记着，能恢复，不丢。
    #[test]
    fn failed_rollback_stays_restorable() {
        let d = temp("rollback");
        let files = two_files(&d);
        let trash = Trash::new(d.join("trash"));
        let err = trash
            .move_in_with(
                Vendor::Claude,
                "s1",
                &files,
                SystemTime::now(),
                &flaky(1, vec![libc::ENOSPC, libc::EIO]),
            )
            .unwrap_err();
        assert!(err.to_string().contains("按 u 可恢复"), "{err}");
        assert!(!files[0].exists(), "第一个没能移回");
        assert!(files[1].exists(), "第二个没动");
        let list = trash.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].files.len(), 1);
        assert_eq!(list[0].files[0].original, files[0]);
        trash.restore(&list[0]).unwrap();
        assert_originals(&files);
        assert!(trash.list().unwrap().is_empty());
    }

    /// 恢复到一半失败：已移回的再放回回收站，这一条原样留着，之后能再恢复。
    #[test]
    fn restore_failure_midway_keeps_entry_intact() {
        let d = temp("restore-midway");
        let files = two_files(&d);
        let trash = Trash::new(d.join("trash"));
        let entry = trash.move_in(Vendor::Claude, "s1", &files).unwrap();
        let err = trash
            .restore_with(&entry, &flaky(1, vec![libc::ENOSPC]))
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::StorageFull);
        assert!(!files[0].exists() && !files[1].exists());
        assert_eq!(trash.list().unwrap(), vec![entry.clone()]);
        trash.restore(&entry).unwrap();
        assert_originals(&files);
    }

    /// 同一毫秒删同一条（或 id 清洗后同名的两条）：各占一个目录，互不覆盖，都能恢复。
    #[test]
    fn same_name_same_millisecond_never_collide() {
        let d = temp("collide");
        let trash = Trash::new(d.join("trash"));
        let now = SystemTime::now();
        let a = d.join("a/x.jsonl");
        let b = d.join("b/x.jsonl");
        for (p, body) in [(&a, "A"), (&b, "B")] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        let ea = trash
            .move_in_with(
                Vendor::Claude,
                "a.b",
                std::slice::from_ref(&a),
                now,
                &rename_excl,
            )
            .unwrap();
        let eb = trash
            .move_in_with(
                Vendor::Claude,
                "a_b",
                std::slice::from_ref(&b),
                now,
                &rename_excl,
            )
            .unwrap();
        assert_ne!(ea.dir, eb.dir);
        assert_eq!(trash.list().unwrap().len(), 2);
        trash.restore(&ea).unwrap();
        trash.restore(&eb).unwrap();
        assert_eq!(fs::read_to_string(&a).unwrap(), "A");
        assert_eq!(fs::read_to_string(&b).unwrap(), "B");
    }

    /// 原处已有同名的东西（文件、空目录、断掉的链接）：恢复报错，两边都不动。
    #[test]
    fn restore_never_overwrites_anything() {
        let d = temp("noclobber");
        let trash = Trash::new(d.join("trash"));
        let file = d.join("s.jsonl");
        for kind in ["dir", "dangling"] {
            fs::write(&file, b"old").unwrap();
            let entry = trash
                .move_in(Vendor::Pi, "s", std::slice::from_ref(&file))
                .unwrap();
            match kind {
                "dir" => fs::create_dir(&file).unwrap(),
                _ => std::os::unix::fs::symlink(d.join("nowhere"), &file).unwrap(),
            }
            let err = trash.restore(&entry).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::AlreadyExists, "{kind}");
            assert_eq!(fs::read(&entry.files[0].stored).unwrap(), b"old");
            match kind {
                "dir" => fs::remove_dir(&file).unwrap(),
                _ => fs::remove_file(&file).unwrap(),
            }
            trash.restore(&entry).unwrap();
            assert_eq!(fs::read(&file).unwrap(), b"old");
            fs::remove_file(&file).unwrap();
        }
        // 检查之后、改名之前有人抢先写进同名文件：改名这一步本身也不覆盖。
        let (src, dst) = (d.join("race-src"), d.join("race-dst"));
        fs::write(&src, b"mine").unwrap();
        fs::write(&dst, b"theirs").unwrap();
        let err = rename_excl(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&dst).unwrap(), b"theirs");
        assert_eq!(fs::read(&src).unwrap(), b"mine");
    }

    /// 真实的权限不足：对话所在目录只读，移不走，原文件在，回收站不留东西。
    #[test]
    fn permission_denied_leaves_original() {
        // SAFETY: geteuid 没有前置条件。
        if unsafe { libc::geteuid() } == 0 {
            return; // root 不受目录权限限制，测不了。
        }
        let d = temp("perm");
        let dir = d.join("vendor/proj");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.jsonl");
        fs::write(&file, b"keep").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let trash = Trash::new(d.join("trash"));
        let res = trash.move_in(Vendor::Claude, "s", std::slice::from_ref(&file));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(res.unwrap_err().kind(), ErrorKind::PermissionDenied);
        assert_eq!(fs::read(&file).unwrap(), b"keep");
        assert!(trash.list().unwrap().is_empty());
    }

    /// 清单写不进去（回收站目录只读，模拟写满）：不移动任何文件。
    #[test]
    fn unwritable_trash_moves_nothing() {
        // SAFETY: geteuid 没有前置条件。
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let d = temp("ro-trash");
        let file = d.join("s.jsonl");
        fs::write(&file, b"keep").unwrap();
        let root = d.join("trash");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();
        let res = Trash::new(&root).move_in(Vendor::Claude, "s", std::slice::from_ref(&file));
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(res.is_err());
        assert_eq!(fs::read(&file).unwrap(), b"keep");
    }

    /// 删除或恢复到一半进程被杀：清单列着两个，一个在原处、一个在回收站。恢复按实况补齐，不卡住。
    #[test]
    fn interrupted_move_still_restores() {
        for moved_back in [0, 1] {
            let d = temp(&format!("interrupted-{moved_back}"));
            let files = two_files(&d);
            let trash = Trash::new(d.join("trash"));
            let entry = trash.move_in(Vendor::Claude, "s1", &files).unwrap();
            // 1：删除时第二个还没移走；0：恢复时第一个已经移回。
            let f = &entry.files[moved_back];
            rename_excl(&f.stored, &f.original).unwrap();
            trash.restore(&trash.list().unwrap()[0]).unwrap();
            assert_originals(&files);
            assert!(trash.list().unwrap().is_empty());
            assert!(!entry.dir.exists());
        }
    }

    /// 最新一条原处被占恢复不了：报错不覆盖，它排到最后，u 接着能恢复更早的；腾出原处后它也能恢复。
    #[test]
    fn blocked_entry_does_not_hold_up_older_ones() {
        let d = temp("blocked");
        let trash = Trash::new(d.join("trash"));
        let (old, new) = (d.join("old.jsonl"), d.join("new.jsonl"));
        let t0 = SystemTime::now() - Duration::from_secs(60);
        for (p, t) in [(&old, t0), (&new, t0 + Duration::from_secs(1))] {
            fs::write(p, p.to_string_lossy().as_bytes()).unwrap();
            trash
                .move_in_with(Vendor::Pi, "s", std::slice::from_ref(p), t, &rename_excl)
                .unwrap();
        }
        fs::write(&new, b"someone else").unwrap();
        let newest = trash.list().unwrap().remove(0);
        assert_eq!(newest.files[0].original, new);
        let err = trash.restore(&newest).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&new).unwrap(), b"someone else");

        let next = trash.list().unwrap().remove(0);
        assert_eq!(next.files[0].original, old, "卡住的那条排到最后");
        trash.restore(&next).unwrap();
        assert_eq!(fs::read_to_string(&old).unwrap(), old.to_string_lossy());

        fs::remove_file(&new).unwrap();
        let last = trash.list().unwrap().remove(0);
        assert!(last.blocked);
        trash.restore(&last).unwrap();
        assert_eq!(fs::read_to_string(&new).unwrap(), new.to_string_lossy());
        assert!(trash.list().unwrap().is_empty());
    }

    const DAY: Duration = Duration::from_secs(86_400);

    /// 过了保留天数的条目连同回收站里的文件一起清掉；原路径上的东西、链接指向的文件、没过期的、只隐藏的都不动。
    #[test]
    fn expired_entries_are_purged_inside_trash_only() {
        let d = temp("purge");
        let trash = Trash::new(d.join("trash")).keep_days(30);
        let now = SystemTime::now();
        let put = |name: &str, age: Duration| {
            let p = d.join(format!("vendor/{name}.jsonl"));
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, name).unwrap();
            trash
                .move_in_with(
                    Vendor::Claude,
                    name,
                    std::slice::from_ref(&p),
                    now - age,
                    &rename_excl,
                )
                .unwrap();
            p
        };
        // 过期的：原处后来又有了同名文件（不能被删）。
        let expired = put("expired", 31 * DAY);
        fs::write(&expired, b"new one").unwrap();
        // 过期的：删除到一半被打断，第二个还在原处（不能被删）。
        let files = two_files(&d);
        let half = trash
            .move_in_with(Vendor::Claude, "half", &files, now - 40 * DAY, &rename_excl)
            .unwrap();
        rename_excl(&half.files[1].stored, &half.files[1].original).unwrap();
        // 过期的：移进来的是个符号链接，指向的文件不能被删。
        let target = d.join("link-target");
        fs::write(&target, b"target").unwrap();
        let link = d.join("vendor/link.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        trash
            .move_in_with(
                Vendor::Claude,
                "link",
                &[link],
                now - 31 * DAY,
                &rename_excl,
            )
            .unwrap();
        // 没过期的、过期但只隐藏的留着。
        put("recent", 29 * DAY);
        let mut hidden = trash.hide(Vendor::Codex, "t1", &d.join("rollout")).unwrap();
        hidden.trashed_at = now - 90 * DAY;
        write_entry(&hidden).unwrap();
        // 上次清到一半留下的。
        fs::create_dir_all(trash.root().join(".purging-x/files/0-a")).unwrap();
        assert_eq!(trash.list().unwrap().len(), 5);

        // 0 天：永不清理。
        let never = Trash::new(trash.root()).keep_days(0);
        assert!(never.purge_expired(now).unwrap().is_empty());
        assert_eq!(never.list().unwrap().len(), 5);

        assert_eq!(trash.purge_expired(now).unwrap(), Vec::<String>::new());
        let left: Vec<String> = trash.list().unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(left, vec!["recent".to_owned(), "t1".to_owned()]);
        assert_eq!(fs::read(&expired).unwrap(), b"new one");
        assert_eq!(fs::read(files[1].join("sub.jsonl")).unwrap(), b"sub");
        assert!(!files[0].exists(), "进了回收站又过期的才清");
        assert_eq!(fs::read(&target).unwrap(), b"target");
        assert_eq!(
            fs::read_dir(trash.root()).unwrap().count(),
            2,
            "不留改名中的目录"
        );
    }

    /// 删除（回收站操作）时顺手清过期的。
    #[test]
    fn move_in_purges_expired() {
        let d = temp("purge-on-move");
        let trash = Trash::new(d.join("trash")).keep_days(30);
        let (a, b) = (d.join("a.jsonl"), d.join("b.jsonl"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let old = SystemTime::now() - 31 * DAY;
        trash
            .move_in_with(Vendor::Pi, "a", std::slice::from_ref(&a), old, &rename_excl)
            .unwrap();
        trash
            .move_in(Vendor::Pi, "b", std::slice::from_ref(&b))
            .unwrap();
        let left: Vec<String> = trash.list().unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(left, vec!["b".to_owned()]);
    }

    #[test]
    fn empty_or_missing_root_lists_nothing() {
        let d = temp("emptyroot");
        assert!(Trash::new(d.join("nope")).list().unwrap().is_empty());
        fs::create_dir_all(d.join("t/garbage")).unwrap();
        assert!(Trash::new(d.join("t")).list().unwrap().is_empty());
    }
}
