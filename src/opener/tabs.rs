//! 记着哪些对话开在哪个标签页：同一对话再打开时聚焦已有的，「恢复上次所有标签」时全部重开。
//! 存在数据目录的 `tabs.json`。

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::{Opened, Opener, TabHandle};
use crate::core::{Config, SessionId};
use crate::sources::Command;

/// 一条开着的对话：怎么重开（命令、标签页标题）、开在哪（标签页）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabRecord {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// 旧版记录没有这个字段，读出来是空串（重开时不设标题）。
    #[serde(default)]
    pub title: String,
    pub handle: TabHandle,
}

impl TabRecord {
    pub fn command(&self) -> Command {
        Command {
            program: self.program.clone(),
            args: self.args.clone(),
            cwd: self.cwd.clone(),
        }
    }
}

/// `open_or_focus` 做了什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 已经开着，切过去了。
    Focused,
    /// 开了新标签页。
    Opened,
    /// 在当前窗口跑完了（其他终端）。
    Inline,
}

#[derive(Debug)]
pub struct Tabs {
    path: PathBuf,
    open: BTreeMap<SessionId, TabRecord>,
}

impl Tabs {
    /// 默认位置：`~/Library/Application Support/huitai/tabs.json`。
    pub fn default_path() -> Option<PathBuf> {
        Config::data_dir().map(|d| d.join("tabs.json"))
    }

    /// 读记录；文件不存在就是空的。
    pub fn load(path: &Path) -> io::Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            open: read(path)?,
        })
    }

    /// 记着的对话（「恢复上次所有标签」用）。
    pub fn records(&self) -> &BTreeMap<SessionId, TabRecord> {
        &self.open
    }

    /// 打开一条对话：记着的标签页还在就聚焦它，不在了（或换了终端）就开新的并记下。
    /// `title` 是新开标签页的标题（见 `tab_title`），聚焦已开的不改标题。
    pub fn open_or_focus(
        &mut self,
        opener: &dyn Opener,
        session: &SessionId,
        cmd: &Command,
        title: &str,
    ) -> io::Result<Outcome> {
        if let Some(rec) = self.open.get(session)
            && opener.focus(&rec.handle)?
        {
            return Ok(Outcome::Focused);
        }
        let outcome = match opener.open(cmd, title)? {
            Opened::Tab(handle) => {
                let rec = TabRecord {
                    program: cmd.program.clone(),
                    args: cmd.args.clone(),
                    cwd: cmd.cwd.clone(),
                    title: title.to_owned(),
                    handle,
                };
                self.update(|open| {
                    open.insert(session.clone(), rec);
                    true
                })?;
                Outcome::Opened
            }
            // 当前窗口里跑的已经结束，没有标签页可记。
            Opened::Inline => {
                self.update(|open| open.remove(session).is_some())?;
                Outcome::Inline
            }
        };
        Ok(outcome)
    }

    /// 把记着的对话全部打开（还开着的只聚焦，不重复开）。每条单独给结果，一条失败不影响别的。
    /// 记录只来自能开标签页的终端；在其他终端里调用会在当前窗口一条接一条地前台跑。
    pub fn restore_all(&mut self, opener: &dyn Opener) -> Vec<(SessionId, io::Result<Outcome>)> {
        let todo: Vec<(SessionId, Command, String)> = self
            .open
            .iter()
            .map(|(id, rec)| (id.clone(), rec.command(), rec.title.clone()))
            .collect();
        todo.into_iter()
            .map(|(id, cmd, title)| {
                let result = self.open_or_focus(opener, &id, &cmd, &title);
                (id, result)
            })
            .collect()
    }

    /// 对话不再开着（CLI 退出、用户结束了它）：从记录里去掉，下次不恢复。
    pub fn forget(&mut self, session: &SessionId) -> io::Result<()> {
        self.update(|open| open.remove(session).is_some())
    }

    /// 改一条记录并落盘：先重读文件、在最新内容上改（别的会话台进程这期间记下的标签页不被整份盖掉），
    /// `change` 返回有没有改动，没改就不写。文件坏了以内存里的为准写回。
    fn update(&mut self, change: impl FnOnce(&mut Records) -> bool) -> io::Result<()> {
        let mut open = match read(&self.path) {
            Ok(open) => open,
            Err(e) if e.kind() == ErrorKind::InvalidData => self.open.clone(),
            Err(e) => return Err(e),
        };
        let changed = change(&mut open);
        if changed {
            save(&self.path, &open)?;
        }
        self.open = open;
        Ok(())
    }
}

type Records = BTreeMap<SessionId, TabRecord>;

fn read(path: &Path) -> io::Result<Records> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("{}: 标签页记录格式不对: {e}", path.display()),
            )
        }),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    }
}

/// 先写同目录下这次独占的临时文件（进程号 + 序号，排他创建）再改名，写一半断电、
/// 两个会话台同时写，都不会留下坏文件或互相删掉对方的临时文件。
fn save(path: &Path, open: &Records) -> io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let with_path =
        |e: io::Error, p: &Path| io::Error::new(e.kind(), format!("{}: {e}", p.display()));
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| with_path(e, dir))?;
    }
    let json = serde_json::to_string_pretty(open).map_err(io::Error::other)?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp-{}-{seq}", std::process::id()));
    let res = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| f.write_all(json.as_bytes()))
        .and_then(|()| fs::rename(&tmp, path));
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res.map_err(|e| with_path(e, path))
}
