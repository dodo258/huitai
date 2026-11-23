//! 会话台给 Claude 对话留的备份：Claude Code 按 `cleanupPeriodDays`（默认 30 天）删掉没动过的对话，
//! 会话台把要留的对话压成 `<备份目录>/<项目目录>/<id>.jsonl.zst` 存在自己的数据目录里。
//!
//! - 备份目录：`<会话台数据目录>/backups/claude/`，别的账号在它下面再分一层 `<账号短名>/`；目录都是 0700，文件 0600。
//! - 只读厂商文件：备份是复制出来压缩，原文件不改不删。备份的修改时间设成原文件的，两边不一样就是原文件变了，重写。
//! - 原文件没了只剩备份：读的时候解到备份目录下的 `.open/` 临时文件里读，读完就删（[`Plain`]），不往厂商目录写。
//! - 唯一会写进厂商目录的是主人明确要的「放回」（[`put_back`]），原处已有同名文件不覆盖。

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// 会话台数据目录下放 Claude 备份的地方。
pub const BACKUP_DIR: &str = "backups/claude";
/// 备份文件的后缀（接在 `<id>.jsonl` 后面）。
const EXT: &str = "zst";
/// 解出来给读的临时文件放这里（备份目录下，同样 0700）。
const OPEN_DIR: &str = ".open";
/// 读完没删掉的临时文件（进程被杀）过了这么久就清掉。
const OPEN_STALE: Duration = Duration::from_secs(60 * 60);
/// zstd 压缩级别：默认的 3，快，文字对话压到原来的十分之一上下。
const LEVEL: i32 = 3;

/// 一个账号的备份目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backups {
    root: PathBuf,
}

/// [`Backups::save`] 做了什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Saved {
    /// 备份和原文件一样新，没动。
    Fresh,
    /// 新写了或更新了备份。
    Written,
}

impl Backups {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 会话台数据目录 `data_dir` 下账号短名是 `key` 的备份目录：默认账号（空串）是 `backups/claude/`，
    /// 别的账号是 `backups/claude/<短名>/`（短名是 8 位十六进制，不会和 Claude 的项目目录名撞：那些都以 `-` 开头）。
    pub fn for_account(data_dir: &Path, key: &str) -> Self {
        let root = data_dir.join(BACKUP_DIR);
        Self::new(if key.is_empty() { root } else { root.join(key) })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 原文件 `<projects>/<项目目录>/<id>.jsonl` 的备份位置；不在 `projects` 下一层的不备。
    pub fn path_for(&self, projects: &Path, original: &Path) -> Option<PathBuf> {
        let rel = original.strip_prefix(projects).ok()?;
        if rel.components().count() != 2 || original.extension()? != "jsonl" {
            return None;
        }
        let mut p = self.root.join(rel).into_os_string();
        p.push(".");
        p.push(EXT);
        Some(PathBuf::from(p))
    }

    /// 备份 `<备份目录>/<项目目录>/<id>.jsonl.zst` 原来在 `projects` 下的哪里。
    pub fn original_of(&self, projects: &Path, backup: &Path) -> Option<PathBuf> {
        let rel = backup.strip_prefix(&self.root).ok()?;
        if rel.components().count() != 2 || !is_backup(backup) {
            return None;
        }
        Some(projects.join(rel).with_extension(""))
    }

    /// 备份或更新 `original`：备份的修改时间和原文件一样就不动；否则压缩写进临时文件、改名换上，
    /// 修改时间设成读之前原文件的（读的时候原文件又变了，下一轮再更新）。原文件只读。
    pub fn save(&self, original: &Path, backup: &Path) -> io::Result<Saved> {
        let mtime = fs::metadata(original)?.modified()?;
        if fs::metadata(backup).and_then(|m| m.modified()).ok() == Some(mtime) {
            return Ok(Saved::Fresh);
        }
        let dir = backup
            .parent()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "备份路径没有上级目录"))?;
        make_dir(&self.root)?;
        make_dir(dir)?;
        let tmp = temp_name(dir, ".saving");
        let written = compress(original, &tmp)
            .and_then(|f| f.set_modified(mtime))
            .and_then(|()| fs::rename(&tmp, backup));
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written.map(|()| Saved::Written)
    }

    /// 原文件没了的备份：`<备份目录>/<项目目录>/<id>.jsonl.zst`，而 `<projects>/<项目目录>/<id>.jsonl` 不在。
    /// 备份目录不在返回空。
    pub fn orphans(&self, projects: &Path) -> Vec<PathBuf> {
        let Ok(rd) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for sub in rd.flatten() {
            let is_dir = sub.file_type().is_ok_and(|t| t.is_dir());
            if !is_dir || sub.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(files) = fs::read_dir(sub.path()) else {
                continue;
            };
            out.extend(files.flatten().map(|f| f.path()).filter(|p| {
                is_backup(p)
                    && p.is_file()
                    && self
                        .original_of(projects, p)
                        .is_some_and(|o| fs::symlink_metadata(o).is_err())
            }));
        }
        out.sort();
        out
    }

    /// 解开备份给读：解到 `.open/` 里的临时文件（0600，修改时间同备份），[`Plain`] 丢掉时删掉。
    pub fn open(&self, backup: &Path) -> io::Result<Plain> {
        let dir = self.root.join(OPEN_DIR);
        make_dir(&self.root)?;
        make_dir(&dir)?;
        clear_stale(&dir, SystemTime::now());
        let path = temp_name(&dir, ".jsonl");
        let plain = Plain { path, temp: true };
        let mtime = fs::metadata(backup)?.modified()?;
        decompress(backup, &plain.path)?.set_modified(mtime)?;
        Ok(plain)
    }
}

/// 这个路径是不是会话台的备份（`<id>.jsonl.zst`）。
pub fn is_backup(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == EXT)
        && path
            .file_stem()
            .is_some_and(|s| s.to_string_lossy().ends_with(".jsonl"))
}

/// 完整解一遍备份，坏了（截断、不是 zstd）报错。
pub fn verify(backup: &Path) -> io::Result<()> {
    let mut d = zstd::stream::read::Decoder::new(File::open(backup)?)?;
    io::copy(&mut d, &mut io::sink()).map(drop)
}

/// 把备份解回 `original`（主人明确要放回时）。原处已有东西（文件、目录、断掉的链接）报错不覆盖；
/// 先解到同目录的临时文件再一步改名（`RENAME_EXCL`），不会半截写进去，也不会在检查和改名之间被抢先写进同名文件。
/// 放回的文件修改时间是现在：Claude 按修改时间算多久没动，用备份的旧时间放回去，下次一开 Claude 就又被删了。
pub fn put_back(backup: &Path, original: &Path) -> io::Result<()> {
    if fs::symlink_metadata(original).is_ok() {
        return Err(io::Error::new(
            ErrorKind::AlreadyExists,
            format!("{} 已存在，不覆盖", original.display()),
        ));
    }
    let dir = original
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "原路径没有上级目录"))?;
    fs::create_dir_all(dir)?;
    let tmp = temp_name(dir, ".restoring");
    let done = decompress(backup, &tmp)
        .and_then(|f| f.sync_all())
        .and_then(|()| {
            super::trash::rename_excl(&tmp, original).map_err(|e| match e.kind() {
                ErrorKind::AlreadyExists => io::Error::new(
                    ErrorKind::AlreadyExists,
                    format!("{} 已存在，不覆盖", original.display()),
                ),
                _ => e,
            })
        });
    if done.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    done
}

/// 备份目录（含各账号的子目录）里的备份个数和占用字节数；目录不在是 0。
pub fn usage(dir: &Path) -> (usize, u64) {
    let mut n = 0;
    let mut bytes = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(t) = e.file_type() else {
                continue;
            };
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() && is_backup(&e.path()) {
                n += 1;
                bytes += e.metadata().map_or(0, |m| m.len());
            }
        }
    }
    (n, bytes)
}

/// 读备份时用的明文文件：原文件还在时就是原文件；只剩备份时是解出来的临时文件，丢掉时删掉。
#[derive(Debug)]
pub struct Plain {
    path: PathBuf,
    temp: bool,
}

impl Plain {
    /// 原文件本身，不用解。
    pub fn as_is(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            temp: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Plain {
    fn drop(&mut self) {
        if self.temp {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// 建目录（含上级），新建的都是 0700；已有的目录收紧到 0700。
fn make_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", dir.display())))?;
    let m = fs::metadata(dir)?;
    if m.permissions().mode() & 0o777 != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// 新建一个 0600 的文件，已存在报错。
fn create_private(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// 压缩 `from` 写进新文件 `to`，返回写好的文件（修改时间由调用方设）。
fn compress(from: &Path, to: &Path) -> io::Result<File> {
    let mut input = BufReader::new(File::open(from)?);
    let out = create_private(to)?;
    let mut enc = zstd::stream::write::Encoder::new(BufWriter::new(out), LEVEL)?;
    io::copy(&mut input, &mut enc)?;
    let out = enc.finish()?.into_inner().map_err(|e| e.into_error())?;
    out.sync_all()?;
    Ok(out)
}

/// 解压 `from` 写进新文件 `to`。
fn decompress(from: &Path, to: &Path) -> io::Result<File> {
    let mut d = zstd::stream::read::Decoder::new(File::open(from)?)?;
    let mut out = BufWriter::new(create_private(to)?);
    io::copy(&mut d, &mut out)?;
    out.flush()?;
    out.into_inner().map_err(|e| e.into_error())
}

/// 同目录下不会撞名的临时文件名（以 `.` 开头，扫描和列备份都不认）。
fn temp_name(dir: &Path, suffix: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".huitai-{}-{n}{suffix}", std::process::id()))
}

/// 删掉 `.open/` 里放了超过 [`OPEN_STALE`] 的临时文件（读到一半进程被杀留下的）。
fn clear_stale(dir: &Path, now: SystemTime) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.accessed().or_else(|_| m.modified()))
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > OPEN_STALE);
        if old {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// 读 `path`：备份就解出来，别的原样。
pub(crate) fn readable(backups: Option<&Backups>, path: &Path) -> io::Result<Plain> {
    match backups {
        Some(b) if is_backup(path) => b.open(path),
        _ => Ok(Plain::as_is(path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("huitai-backup-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn paths_map_both_ways() {
        let b = Backups::new("/d/backups/claude");
        let projects = Path::new("/h/.claude/projects");
        let orig = Path::new("/h/.claude/projects/-p/s1.jsonl");
        let bk = b.path_for(projects, orig).unwrap();
        assert_eq!(bk, Path::new("/d/backups/claude/-p/s1.jsonl.zst"));
        assert_eq!(b.original_of(projects, &bk).unwrap(), orig);
        assert!(is_backup(&bk) && !is_backup(orig));
        // 附属目录里的、不是 .jsonl 的不备。
        assert!(
            b.path_for(
                projects,
                Path::new("/h/.claude/projects/-p/s1/subagents/a.jsonl")
            )
            .is_none()
        );
        assert!(
            b.path_for(projects, Path::new("/h/.claude/projects/-p/s1.txt"))
                .is_none()
        );
        assert!(
            b.path_for(projects, Path::new("/elsewhere/-p/s1.jsonl"))
                .is_none()
        );
    }

    #[test]
    fn open_reads_a_temp_copy_and_removes_it() {
        let d = temp("open");
        let orig = d.join("p/-x/s.jsonl");
        fs::create_dir_all(orig.parent().unwrap()).unwrap();
        fs::write(&orig, b"{\"a\":1}\n").unwrap();
        let b = Backups::new(d.join("bk"));
        let bk = b.path_for(&d.join("p"), &orig).unwrap();
        assert_eq!(b.save(&orig, &bk).unwrap(), Saved::Written);
        let plain = b.open(&bk).unwrap();
        let p = plain.path().to_path_buf();
        assert_eq!(fs::read(&p).unwrap(), b"{\"a\":1}\n");
        drop(plain);
        assert!(!p.exists());
    }
}
