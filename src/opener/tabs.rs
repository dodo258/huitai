//! 记着哪些对话开在哪个标签页：同一对话再打开时聚焦已有的，「恢复上次所有标签」时全部重开。
//! 存在数据目录的 `tabs.json`。
//!
//! 新旧版本同时开着时（升级过渡期）都读写这里：旧版（v0.1.2 及以前）把整个文件一次解码，出现它不认识的终端
//! 就整份读不了。所以 `tabs.json` 只放旧版认识的终端（Ghostty、Kaku）的记录，别的终端的记录放旁边的
//! `tabs-more.json`（旧版不读它）。两份都按单条解析：认不出的条目（更新的版本写的）跳过，原样留着写回原处；
//! 同一条对话两份里都有时以 `tabs.json` 的为准（只有旧版会在新版记过之后再往 `tabs.json` 里记它）。

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    open: Records,
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
        &self.open.known
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
        if let Some(rec) = self.open.known.get(session)
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
                self.update(|open| open.remove(session))?;
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
            .known
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
        self.update(|open| open.remove(session))
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

/// 两份文件合起来的记录。
#[derive(Debug, Clone, Default)]
struct Records {
    known: BTreeMap<SessionId, TabRecord>,
    /// 认不出的条目，按来自哪份文件（[`tabs.json`, `tabs-more.json`]）原样留着。
    raw: [BTreeMap<SessionId, Value>; 2],
}

impl Records {
    fn insert(&mut self, id: SessionId, rec: TabRecord) {
        for raw in &mut self.raw {
            raw.remove(&id);
        }
        self.known.insert(id, rec);
    }

    fn remove(&mut self, id: &SessionId) -> bool {
        let mut hit = self.known.remove(id).is_some();
        for raw in &mut self.raw {
            hit |= raw.remove(id).is_some();
        }
        hit
    }
}

/// 旧版（v0.1.2 及以前）认不认识这种标签页。新加一种终端写 `false`。
fn old_versions_read(handle: &TabHandle) -> bool {
    match handle {
        TabHandle::Ghostty { .. } | TabHandle::Kaku { .. } => true,
        TabHandle::Cmux { .. } => false,
    }
}

/// 旧版不认识的终端的记录放哪：`tabs.json` 旁边的 `tabs-more.json`。
fn more_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map_or_else(|| "tabs".into(), |s| s.to_string_lossy());
    path.with_file_name(format!("{stem}-more.json"))
}

fn read(path: &Path) -> io::Result<Records> {
    let (main, main_raw) = read_file(path)?;
    let (mut known, mut more_raw) = read_file(&more_path(path))?;
    for id in main.keys().chain(main_raw.keys()) {
        known.remove(id);
        more_raw.remove(id);
    }
    known.extend(main);
    Ok(Records {
        known,
        raw: [main_raw, more_raw],
    })
}

/// 读一份文件，逐条解析：认得的和认不出的分开。整份不是 JSON 对象才算格式不对。
type Parsed = (BTreeMap<SessionId, TabRecord>, BTreeMap<SessionId, Value>);

fn read_file(path: &Path) -> io::Result<Parsed> {
    let entries: BTreeMap<SessionId, Value> = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("{}: 标签页记录格式不对: {e}", path.display()),
            )
        })?,
        Err(e) if e.kind() == ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    };
    let mut parsed = Parsed::default();
    for (id, value) in entries {
        match TabRecord::deserialize(&value) {
            Ok(rec) => {
                parsed.0.insert(id, rec);
            }
            Err(_) => {
                parsed.1.insert(id, value);
            }
        }
    }
    Ok(parsed)
}

/// 按旧版认不认识分到两份文件里写。先写 `tabs-more.json`：两次改名之间断电，
/// 同一条可能两份里都有（以 `tabs.json` 为准），不会两份里都没有。
fn save(path: &Path, open: &Records) -> io::Result<()> {
    let [mut main, mut more] = open.raw.clone();
    for (id, rec) in &open.known {
        let value = serde_json::to_value(rec).map_err(io::Error::other)?;
        let file = if old_versions_read(&rec.handle) {
            &mut main
        } else {
            &mut more
        };
        file.insert(id.clone(), value);
    }
    let more_path = more_path(path);
    if more.is_empty() {
        match fs::remove_file(&more_path) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("{}: {e}", more_path.display()),
                ));
            }
            _ => {}
        }
    } else {
        write_file(&more_path, &more)?;
    }
    write_file(path, &main)
}

/// 先写同目录下这次独占的临时文件（进程号 + 序号，排他创建）再改名，写一半断电、
/// 两个会话台同时写，都不会留下坏文件或互相删掉对方的临时文件。
fn write_file(path: &Path, open: &BTreeMap<SessionId, Value>) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// v0.1.2 原样的类型（`git show 42a4c75:src/opener/tabs.rs`、`src/opener/mod.rs`）：整份一次解码。
    #[allow(dead_code)]
    mod v012 {
        use std::collections::BTreeMap;
        use std::path::PathBuf;

        use serde::Deserialize;

        #[derive(Debug, Deserialize)]
        pub struct TabRecord {
            pub program: String,
            pub args: Vec<String>,
            pub cwd: PathBuf,
            #[serde(default)]
            pub title: String,
            pub handle: TabHandle,
        }

        #[derive(Debug, Deserialize)]
        #[serde(tag = "terminal", rename_all = "lowercase")]
        pub enum TabHandle {
            Ghostty {
                terminal_id: String,
            },
            Kaku {
                pane_id: u64,
                tty: String,
                command: String,
            },
        }

        pub fn read(path: &std::path::Path) -> BTreeMap<String, TabRecord> {
            let text = std::fs::read_to_string(path).unwrap();
            serde_json::from_str(&text).expect("v0.1.2 读不了 tabs.json")
        }
    }

    /// 依次交出这些标签页。
    struct Handing(RefCell<Vec<TabHandle>>);

    impl Opener for Handing {
        fn open(&self, _: &Command, _: &str) -> io::Result<Opened> {
            Ok(Opened::Tab(self.0.borrow_mut().remove(0)))
        }
        fn focus(&self, _: &TabHandle) -> io::Result<bool> {
            Ok(false)
        }
    }

    fn ghostty(id: &str) -> TabHandle {
        TabHandle::Ghostty {
            terminal_id: id.to_owned(),
        }
    }

    fn cmux(id: &str) -> TabHandle {
        TabHandle::Cmux {
            surface_id: id.to_owned(),
            workspace_id: "w".to_owned(),
        }
    }

    fn cmd() -> Command {
        Command {
            program: "ai".to_owned(),
            args: vec!["attach".to_owned()],
            cwd: PathBuf::from("/tmp"),
        }
    }

    fn record(handle: TabHandle) -> TabRecord {
        TabRecord {
            program: "ai".to_owned(),
            args: vec![],
            cwd: PathBuf::from("/tmp"),
            title: String::new(),
            handle,
        }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("huitai-tabs-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.join("tabs.json")
    }

    fn ids<V>(m: &BTreeMap<SessionId, V>) -> Vec<&str> {
        m.keys().map(SessionId::as_str).collect()
    }

    /// v0.1.3 把 cmux 记录直接写进了 `tabs.json`：照样读得出，下次写的时候挪到旁边那份。
    #[test]
    fn cmux_records_in_main_file_move_out_on_next_save() {
        let path = temp("migrate");
        let old: BTreeMap<SessionId, TabRecord> =
            [(SessionId::from("c"), record(cmux("s-1")))].into();
        fs::write(&path, serde_json::to_string(&old).unwrap()).unwrap();
        let mut tabs = Tabs::load(&path).unwrap();
        assert_eq!(ids(tabs.records()), ["c"]);
        let term = Handing(RefCell::new(vec![ghostty("g-1")]));
        tabs.open_or_focus(&term, &SessionId::from("g"), &cmd(), "")
            .unwrap();
        assert_eq!(v012::read(&path).keys().collect::<Vec<_>>(), ["g"]);
        assert_eq!(ids(Tabs::load(&path).unwrap().records()), ["c", "g"]);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// 更新的版本写了认不出的终端：只跳过那一条，别的照读；写回时原样留着。
    #[test]
    fn unknown_entries_are_skipped_one_by_one_and_kept() {
        let path = temp("unknown");
        let future = serde_json::json!({
            "program": "ai", "args": [], "cwd": "/tmp",
            "handle": {"terminal": "warp", "tab": 7}
        });
        let text = serde_json::json!({
            "f": future,
            "g": serde_json::to_value(record(ghostty("g-1"))).unwrap(),
        });
        fs::write(&path, text.to_string()).unwrap();
        let mut tabs = Tabs::load(&path).unwrap();
        assert_eq!(ids(tabs.records()), ["g"]);
        let term = Handing(RefCell::new(vec![ghostty("g-2")]));
        tabs.open_or_focus(&term, &SessionId::from("h"), &cmd(), "")
            .unwrap();
        let saved: BTreeMap<String, Value> =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["f"], future);
        assert_eq!(saved.keys().collect::<Vec<_>>(), ["f", "g", "h"]);
        // 这条对话再开（或忘掉）时，认不出的那条换掉。
        tabs.forget(&SessionId::from("f")).unwrap();
        let saved: BTreeMap<String, Value> =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!saved.contains_key("f"));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// 新版记在旁边那份之后，旧版又在 `tabs.json` 里记了同一条：以 `tabs.json` 的为准。
    #[test]
    fn main_file_wins_when_both_have_a_session() {
        let path = temp("both");
        let id = SessionId::from("x");
        let main: BTreeMap<SessionId, TabRecord> = [(id.clone(), record(ghostty("g-1")))].into();
        let more: BTreeMap<SessionId, TabRecord> = [(id.clone(), record(cmux("s-1")))].into();
        fs::write(&path, serde_json::to_string(&main).unwrap()).unwrap();
        fs::write(more_path(&path), serde_json::to_string(&more).unwrap()).unwrap();
        let tabs = Tabs::load(&path).unwrap();
        assert_eq!(tabs.records()[&id].handle, ghostty("g-1"));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
