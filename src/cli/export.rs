//! `ai export <对话 id> [-o 文件]`：把一条对话导出成 Markdown（排版见 [`crate::export`]），不给 `-o` 打到标准输出。
//!
//! 对话 id 认三种写法：厂商自己的 id（`c` 复制的恢复命令里那个，所有启用的账号里只有一条对得上才算）、
//! 带账号前缀的 `<账号>:<id>`（非默认账号）、`<厂商>:<id>`（那家默认账号）。几个账号都有同一个 id 时报歧义，列出后两种写法让人选。

use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::core::{Account, SessionId, Vendor};
use crate::export;
use crate::index::{Entry, Index};
use crate::sources::{self, Trash};
use crate::ui::text::project_label;

/// 导出 `id`：给了 `out` 写进那个文件（已有就整份换掉）并返回路径，否则打到标准输出。
pub fn run(id: &SessionId, out: Option<&Path>) -> io::Result<Option<PathBuf>> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "取不到 HOME"))?;
    let trash = Trash::default_location()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "取不到回收站位置"))?;
    let accounts = sources::local_accounts(&home);
    let index = Index::open_default()?;
    // 裸 id 要看全所有启用的账号才知道是不是只有一条：缓存里只有一条不算数（别的账号可能刚有了同一个 id），先扫一遍。
    // 带前缀的只认一个账号，缓存里没有（没开过会话台、或刚聊的新对话）才扫。
    let bare = matches!(Want::of(id), Want::Native);
    if bare {
        index.refresh();
    }
    let mut found = pick(all_entries(&index)?, id, &accounts)?;
    if found.is_none() && !bare {
        index.refresh();
        found = pick(all_entries(&index)?, id, &accounts)?;
    }
    let entry =
        found.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("找不到对话 {id}")))?;
    let s = &entry.session;
    let all = sources::sources_for(&accounts, trash);
    let src = sources::source_of(&all, s)
        .ok_or_else(|| io::Error::other(format!("没有 {} 这个账号的会话来源", s.vendor)))?;
    let md = export::render(
        src,
        &entry,
        &accounts,
        &project_label(&s.cwd, &index.projects()),
    )?;
    match out {
        Some(path) => {
            // 各账号的数据目录和各家监听的目录（Hermes、OpenCode 的库就在里面，会话文件不一定是 `Session.path`）。
            let roots: Vec<PathBuf> = accounts
                .iter()
                .map(|a| a.dir.clone())
                .chain(all.iter().flat_map(|src| src.watch_roots()))
                .collect();
            write_file(path, &s.path, &roots, &md)?;
            Ok(Some(path.to_path_buf()))
        }
        None => {
            // 打到终端时控制字符换成看得见的写法（正文里存着的转义序列不会改终端标题、清屏）；进管道、重定向原样。
            let stdout = io::stdout();
            let md = if stdout.is_terminal() {
                export::visible_controls(&md)
            } else {
                md
            };
            stdout.lock().write_all(md.as_bytes())?;
            Ok(None)
        }
    }
}

/// 对话 id 的三种写法（见模块说明）。
enum Want<'a> {
    /// `<厂商>:<id>`：那家默认账号。
    Default(Vendor, &'a str),
    /// `<账号>:<id>`：只认那个账号。
    Account,
    /// 厂商自己的 id：所有账号里找。
    Native,
}

impl<'a> Want<'a> {
    fn of(id: &'a SessionId) -> Self {
        let want = id.as_str();
        let default_of = Vendor::ALL.into_iter().find_map(|v| {
            want.strip_prefix(v.key())
                .and_then(|rest| rest.strip_prefix(':'))
                .map(|native| (v, native))
        });
        match default_of {
            Some((v, native)) => Self::Default(v, native),
            None if !id.split_account().0.is_empty() => Self::Account,
            None => Self::Native,
        }
    }
}

fn all_entries(index: &Index) -> io::Result<Vec<Entry>> {
    let mut all = Vec::new();
    for v in Vendor::ALL {
        all.extend(index.list(v)?);
    }
    Ok(all)
}

/// `id` 指的是 `entries` 里哪一条（写法见模块说明）。没有返回 `None`；对得上不止一条时报歧义，列出每条该用的写法。
fn pick(entries: Vec<Entry>, id: &SessionId, accounts: &[Account]) -> io::Result<Option<Entry>> {
    let want = Want::of(id);
    let mut hits: Vec<Entry> = entries
        .into_iter()
        .filter(|e| {
            let s = &e.session;
            match want {
                Want::Default(v, native) => {
                    s.vendor == v && s.account.is_empty() && s.id.as_str() == native
                }
                Want::Account => s.id == *id,
                Want::Native => s.id.native() == id.as_str(),
            }
        })
        .collect();
    if hits.len() <= 1 {
        return Ok(hits.pop());
    }
    let mut msg = format!(
        "对话 id {} 不止一个账号里有，换成下面一个再导出：",
        id.as_str()
    );
    for e in &hits {
        let s = &e.session;
        let (spelled, account) = if s.account.is_empty() {
            (format!("{}:{}", s.vendor.key(), s.id.native()), "默认")
        } else {
            let name = accounts
                .iter()
                .find(|a| a.vendor == s.vendor && a.key == s.account)
                .map_or(s.account.as_str(), |a| a.name.as_str());
            (s.id.to_string(), name)
        };
        let line = format!(
            "{spelled}（{} · {account} · {}）",
            s.vendor.name(),
            e.title()
        );
        // 标题可能取自首问，里面的终端控制字符打到标准错误也会生效：去掉。
        msg.push_str("\n  ");
        msg.extend(line.chars().filter(|c| !c.is_control()));
    }
    Err(io::Error::new(io::ErrorKind::InvalidInput, msg))
}

/// 写进 `path`。和这条对话的会话文件 `source` 是同一个文件（含软链、硬链），或落在 `roots`（账号数据目录）里就拒绝。
/// 已有的普通文件：同目录先写临时文件再改名换上，写到一半失败旧文件还在；旧文件本身写不了（只读）照样报错。
/// `path` 是软链时换掉它指向的文件，软链本身留着。不存在、或不是普通文件（命名管道、设备）的直接写。
fn write_file(path: &Path, source: &Path, roots: &[PathBuf], md: &str) -> io::Result<()> {
    let at = |e: io::Error| io::Error::new(e.kind(), format!("{}：{e}", path.display()));
    let refuse = |why: String| {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} {why}，不能覆盖，换个文件名", path.display()),
        ))
    };
    let old = match fs::metadata(path) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(at(e)),
    };
    if let (Some(o), Ok(src)) = (&old, fs::metadata(source))
        && o.dev() == src.dev()
        && o.ino() == src.ino()
    {
        return refuse(format!("就是这条对话的会话文件（{}）", source.display()));
    }
    let target = real_path(path);
    if let Some(root) = roots.iter().find(|r| target.starts_with(real_path(r))) {
        return refuse(format!("在会话数据目录 {} 里", root.display()));
    }
    let direct = match old {
        Some(o) if o.is_file() => return replace(&target, &o, md).map_err(at),
        // 不在的新建：create_new 不跟着悬空的软链跑到别处（比如数据目录里）建文件。
        None => OpenOptions::new().write(true).create_new(true).open(path),
        Some(_) => OpenOptions::new().write(true).open(path),
    };
    direct
        .and_then(|mut f| f.write_all(md.as_bytes()))
        .map_err(at)
}

/// `path` 真正指的位置：在的话解开软链；不在的话解开上一级目录。
fn real_path(path: &Path) -> PathBuf {
    if let Ok(p) = fs::canonicalize(path) {
        return p;
    }
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => fs::canonicalize(if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        })
        .map(|d| d.join(name))
        .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

/// 换掉已有的普通文件 `target`（旧的元数据 `old`）：临时文件建出来就带旧文件的权限，私有导出写的过程中别人也读不到。
fn replace(target: &Path, old: &fs::Metadata, md: &str) -> io::Result<()> {
    // 目录可写时改名能换掉只读文件：先确认旧文件本身能写。
    OpenOptions::new().write(true).open(target)?;
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "不是文件名"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = target.with_file_name(tmp_name);
    // 建不出来（名字被别的文件占着）就不碰那个文件。
    let mut f = create_tmp(&tmp, old)?;
    let written = (|| {
        f.write_all(md.as_bytes())?;
        f.set_permissions(old.permissions())?;
        f.sync_all()?;
        fs::rename(&tmp, target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// 新建临时文件，权限从一开始就不比旧文件宽（umask 只会再收紧，写完由调用方设成和旧文件一样）。
fn create_tmp(tmp: &Path, old: &fs::Metadata) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(old.mode() & 0o777)
        .open(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("huitai-cli-export-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn tmp_of(old: &Path) -> PathBuf {
        let name = old.file_name().unwrap().to_string_lossy();
        old.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
    }

    #[test]
    fn failed_write_keeps_the_old_export() {
        let d = dir("fail");
        let source = d.join("s1.jsonl");
        fs::write(&source, "原始记录\n").unwrap();
        let old = d.join("old.md");
        fs::write(&old, "旧导出\n").unwrap();
        // 临时文件名被一个目录占着：写不进去，旧文件不动。
        fs::create_dir(tmp_of(&old)).unwrap();
        assert!(write_file(&old, &source, &[], "# 新").is_err());
        assert_eq!(fs::read_to_string(&old).unwrap(), "旧导出\n");
    }

    #[test]
    fn taken_temp_name_is_left_alone() {
        let d = dir("taken");
        let old = d.join("old.md");
        fs::write(&old, "旧导出\n").unwrap();
        // 临时文件名被别人的普通文件占着：建不出临时文件就报错，不删那个文件。
        let theirs = tmp_of(&old);
        fs::write(&theirs, "别人的文件\n").unwrap();
        assert!(write_file(&old, &d.join("s1.jsonl"), &[], "# 新").is_err());
        assert_eq!(fs::read_to_string(&theirs).unwrap(), "别人的文件\n");
        assert_eq!(fs::read_to_string(&old).unwrap(), "旧导出\n");
    }

    #[test]
    fn temp_file_is_private_from_the_start() {
        let d = dir("private");
        let old = d.join("old.md");
        fs::write(&old, "旧导出\n").unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(0o600)).unwrap();
        // 常见的 022 umask 下不带权限建出来是 0644：写的过程中别人能读。
        let tmp = tmp_of(&old);
        drop(create_tmp(&tmp, &fs::metadata(&old).unwrap()).unwrap());
        assert_eq!(fs::metadata(&tmp).unwrap().mode() & 0o777, 0o600);
        fs::remove_file(&tmp).unwrap();
        write_file(&old, &d.join("s1.jsonl"), &[], "# 新").unwrap();
        assert_eq!(fs::read_to_string(&old).unwrap(), "# 新");
        assert_eq!(fs::metadata(&old).unwrap().mode() & 0o777, 0o600);
    }
}
