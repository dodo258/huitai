//! `ai export <对话 id> [-o 文件]`：把一条对话导出成 Markdown（排版见 [`crate::export`]），不给 `-o` 打到标准输出。
//!
//! 对话 id 认三种写法：厂商自己的 id（`c` 复制的恢复命令里那个，所有启用的账号里只有一条对得上才算）、
//! 带账号前缀的 `<账号>:<id>`（非默认账号）、`<厂商>:<id>`（那家默认账号）。几个账号都有同一个 id 时报歧义，列出后两种写法让人选。

use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::MetadataExt;
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
    // 缓存里没有（没开过会话台、或刚聊的新对话）就扫一遍再找。
    let entry = match pick(all_entries(&index)?, id, &accounts)? {
        Some(e) => e,
        None => {
            index.refresh();
            pick(all_entries(&index)?, id, &accounts)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("找不到对话 {id}"))
            })?
        }
    };
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
            write_file(path, &s.path, &md)?;
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

fn all_entries(index: &Index) -> io::Result<Vec<Entry>> {
    let mut all = Vec::new();
    for v in Vendor::ALL {
        all.extend(index.list(v)?);
    }
    Ok(all)
}

/// `id` 指的是 `entries` 里哪一条（写法见模块说明）。没有返回 `None`；对得上不止一条时报歧义，列出每条该用的写法。
fn pick(entries: Vec<Entry>, id: &SessionId, accounts: &[Account]) -> io::Result<Option<Entry>> {
    let want = id.as_str();
    let default_of = Vendor::ALL.into_iter().find_map(|v| {
        want.strip_prefix(v.key())
            .and_then(|rest| rest.strip_prefix(':'))
            .map(|native| (v, native))
    });
    let mut hits: Vec<Entry> = entries
        .into_iter()
        .filter(|e| {
            let s = &e.session;
            match default_of {
                Some((v, native)) => {
                    s.vendor == v && s.account.is_empty() && s.id.as_str() == native
                }
                // 带账号前缀的 id 只认那个账号；不带的在所有账号里找厂商自己的 id。
                None if !id.split_account().0.is_empty() => s.id == *id,
                None => s.id.native() == want,
            }
        })
        .collect();
    if hits.len() <= 1 {
        return Ok(hits.pop());
    }
    let mut msg = format!("对话 id {want} 不止一个账号里有，换成下面一个再导出：");
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
        msg.push_str(&format!(
            "\n  {spelled}（{} · {account} · {}）",
            s.vendor.name(),
            e.title()
        ));
    }
    Err(io::Error::new(io::ErrorKind::InvalidInput, msg))
}

/// 写进 `path`：`path` 和这条对话的会话文件 `source` 是同一个文件（含软链、硬链）就拒绝；
/// 先在同目录写临时文件再改名换上，写到一半失败旧文件还在。`path` 是软链时换掉它指向的文件，软链本身留着。
fn write_file(path: &Path, source: &Path, md: &str) -> io::Result<()> {
    let at = |e: io::Error| io::Error::new(e.kind(), format!("{}：{e}", path.display()));
    let old = match fs::metadata(path) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(at(e)),
    };
    if let (Some(o), Ok(src)) = (&old, fs::metadata(source))
        && o.dev() == src.dev()
        && o.ino() == src.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} 就是这条对话的会话文件（{}），不能覆盖，换个文件名",
                path.display(),
                source.display()
            ),
        ));
    }
    let target = match old {
        Some(_) => fs::canonicalize(path).map_err(at)?,
        None => path.to_path_buf(),
    };
    let name = target.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} 不是文件名", path.display()),
        )
    })?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = target.with_file_name(tmp_name);
    let written = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(md.as_bytes())?;
        f.sync_all()?;
        if let Some(o) = fs::metadata(&target).ok().filter(|m| m.is_file()) {
            f.set_permissions(o.permissions())?;
        }
        fs::rename(&tmp, &target)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(at(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_write_keeps_the_old_export() {
        let d = std::env::temp_dir().join(format!("huitai-cli-export-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let source = d.join("s1.jsonl");
        fs::write(&source, "原始记录\n").unwrap();
        let old = d.join("old.md");
        fs::write(&old, "旧导出\n").unwrap();
        // 临时文件名被一个目录占着：写不进去，旧文件不动。
        fs::create_dir(d.join(format!(".old.md.{}.tmp", std::process::id()))).unwrap();
        assert!(write_file(&old, &source, "# 新").is_err());
        assert_eq!(fs::read_to_string(&old).unwrap(), "旧导出\n");
    }
}
