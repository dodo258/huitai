//! 会话台自己的标注（改名、置顶、归档，项目的显示名和整组归档，以及列表里展开着的项目组），
//! 存在数据目录的 `annotations.json`，
//! 从不写厂商文件。
//! 和可随时重建的索引缓存分开放：缓存库坏了或升级换表结构直接删掉重建，标注不受影响。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::core::Vendor;

pub(crate) const FILE_NAME: &str = "annotations.json";
/// 读改写标注文件期间持有的跨进程锁（同目录）。
const LOCK_NAME: &str = "annotations.lock";
/// 等别的会话台写完标注最多等多久（它只在读文件、写文件那一下持锁）。
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// 一条对话的标注。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    /// 会话台里改的名字，盖过厂商标题。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub archived: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// 一个项目（目录）的标注，三家共用。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectNote {
    /// 会话台里给项目起的显示名，盖过目录名；不动文件夹。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 整组移进归档（对话本身的标注不动）。
    #[serde(default, skip_serializing_if = "is_false")]
    pub archived: bool,
}

/// 对一个项目的一次修改。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectEdit {
    /// 起显示名；`None` 或空白恢复目录名。
    Rename(Option<String>),
    Archive(bool),
}

impl ProjectNote {
    fn apply(&mut self, edit: ProjectEdit) {
        match edit {
            ProjectEdit::Rename(n) => self.name = clean(n),
            ProjectEdit::Archive(a) => self.archived = a,
        }
    }

    fn is_empty(&self) -> bool {
        *self == ProjectNote::default()
    }
}

/// 名字去掉首尾空白，空的算没有。
fn clean(name: Option<String>) -> Option<String> {
    name.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
}

/// 对一条对话的一次修改。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// 改名；`None` 或空白恢复厂商标题。
    Rename(Option<String>),
    Pin(bool),
    Archive(bool),
}

impl Note {
    fn apply(&mut self, edit: Edit) {
        match edit {
            Edit::Rename(t) => self.title = clean(t),
            Edit::Pin(p) => self.pinned = p,
            Edit::Archive(a) => self.archived = a,
        }
    }

    fn is_empty(&self) -> bool {
        *self == Note::default()
    }
}

#[derive(Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    notes: Vec<Record>,
    /// 列表里展开着的项目组（项目目录）。项目组默认收起，只记展开的。
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    expanded: BTreeSet<PathBuf>,
    /// 项目的显示名和整组归档（项目目录 → 标注）。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    projects: BTreeMap<PathBuf, ProjectNote>,
}

#[derive(Serialize, Deserialize)]
struct Record {
    vendor: Vendor,
    id: String,
    #[serde(flatten)]
    note: Note,
}

pub(crate) type Key = (Vendor, String);
type Map = HashMap<Key, Note>;

/// 文件里的全部内容。
#[derive(Clone, Default)]
struct State {
    map: Map,
    expanded: BTreeSet<PathBuf>,
    projects: BTreeMap<PathBuf, ProjectNote>,
}

pub(crate) struct Notes {
    path: PathBuf,
    state: State,
}

impl Notes {
    /// 读标注文件。文件不存在算空；格式坏了把原文件改名留底（`annotations.json.corrupt-<秒>`）、
    /// 从空开始，并在第二个返回值里说明。
    pub(crate) fn load(dir: &Path) -> io::Result<(Self, Option<String>)> {
        let path = dir.join(FILE_NAME);
        let (state, problem) = read_or_set_aside(&path)?;
        Ok((
            Self {
                path,
                state: state.unwrap_or_default(),
            },
            problem,
        ))
    }

    pub(crate) fn get(&self, vendor: Vendor, id: &str) -> Option<&Note> {
        self.state.map.get(&(vendor, id.to_owned()))
    }

    pub(crate) fn expanded(&self) -> &BTreeSet<PathBuf> {
        &self.state.expanded
    }

    pub(crate) fn projects(&self) -> &BTreeMap<PathBuf, ProjectNote> {
        &self.state.projects
    }

    /// 改一个项目的标注并立即落盘。
    pub(crate) fn edit_project(&mut self, project: &Path, edit: ProjectEdit) -> io::Result<()> {
        self.update(|st| {
            let mut note = st.projects.remove(project).unwrap_or_default();
            note.apply(edit);
            if !note.is_empty() {
                st.projects.insert(project.to_path_buf(), note);
            }
        })
    }

    /// 改一条并立即落盘。
    pub(crate) fn edit(&mut self, vendor: Vendor, id: &str, edit: Edit) -> io::Result<()> {
        self.update(|st| {
            let key = (vendor, id.to_owned());
            let mut note = st.map.remove(&key).unwrap_or_default();
            note.apply(edit);
            if !note.is_empty() {
                st.map.insert(key, note);
            }
        })
    }

    /// 记下项目组展开（`true`）或收起，立即落盘。
    pub(crate) fn set_expanded(&mut self, project: &Path, expanded: bool) -> io::Result<()> {
        self.update(|st| {
            if expanded {
                st.expanded.insert(project.to_path_buf());
            } else {
                st.expanded.remove(project);
            }
        })
    }

    /// 先重读文件再改、写回，别的会话台进程刚改的不会被盖掉。读到写回整段持文件锁：
    /// 两个会话台同时改，后一个等前一个写完再读，不会各自基于同一份旧内容写、互相覆盖。
    fn update(&mut self, change: impl FnOnce(&mut State)) -> io::Result<()> {
        let _lock = lock_exclusive(&self.path.with_file_name(LOCK_NAME))?;
        // 文件被改坏了：已改名留底，以内存里的为准写回。
        let mut state = read_or_set_aside(&self.path)?
            .0
            .unwrap_or_else(|| self.state.clone());
        change(&mut state);
        write(&self.path, &state)?;
        self.state = state;
        Ok(())
    }
}

/// 拿 `path` 上的独占 flock（文件不在就建，0600）；返回的文件关掉时锁随之释放。最多等 [`LOCK_WAIT`]。
fn lock_exclusive(path: &Path) -> io::Result<fs::File> {
    let with_path = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(with_path)?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        // SAFETY: 描述符在 `file` 活着期间有效；flock 只作用于这个打开的文件。
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(with_path(err));
        }
        if Instant::now() >= deadline {
            return Err(with_path(io::Error::new(
                ErrorKind::TimedOut,
                "别的会话台一直占着标注文件",
            )));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// 读文件；格式坏了把它改名留底，返回 `None` 和说明。
fn read_or_set_aside(path: &Path) -> io::Result<(Option<State>, Option<String>)> {
    match read(path) {
        Ok(map) => Ok((Some(map), None)),
        Err(e) if e.kind() == ErrorKind::InvalidData => {
            let backup = path.with_extension(format!("json.corrupt-{}", unix_secs()));
            fs::rename(path, &backup)
                .map_err(|e2| io::Error::new(e2.kind(), format!("{}: {e2}", path.display())))?;
            Ok((
                None,
                Some(format!("{e}；原文件已改名为 {}", backup.display())),
            ))
        }
        Err(e) => Err(e),
    }
}

fn read(path: &Path) -> io::Result<State> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(State::default()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    };
    let file: FileFormat = serde_json::from_str(&text).map_err(|e| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("{}: 标注文件格式不对: {e}", path.display()),
        )
    })?;
    Ok(State {
        map: file
            .notes
            .into_iter()
            .map(|r| ((r.vendor, r.id), r.note))
            .collect(),
        expanded: file.expanded,
        projects: file.projects,
    })
}

/// 原子写：先写同目录临时文件、刷盘，再改名替换。
fn write(path: &Path, state: &State) -> io::Result<()> {
    let mut notes: Vec<Record> = state
        .map
        .iter()
        .map(|((vendor, id), note)| Record {
            vendor: *vendor,
            id: id.clone(),
            note: note.clone(),
        })
        .collect();
    notes.sort_by(|a, b| (a.vendor, &a.id).cmp(&(b.vendor, &b.id)));
    let file = FileFormat {
        version: 1,
        notes,
        expanded: state.expanded.clone(),
        projects: state.projects.clone(),
    };
    let json = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let res = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res.map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_trims_and_blank_clears() {
        let mut n = Note::default();
        n.apply(Edit::Rename(Some("  新名字 ".into())));
        assert_eq!(n.title.as_deref(), Some("新名字"));
        n.apply(Edit::Rename(Some("   ".into())));
        assert_eq!(n.title, None);
        assert!(n.is_empty());
    }

    #[test]
    fn default_fields_are_not_written() {
        let r = Record {
            vendor: Vendor::Pi,
            id: "x".into(),
            note: Note {
                pinned: true,
                ..Note::default()
            },
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"vendor":"pi","id":"x","pinned":true}"#
        );
    }

    /// 审计第 10 条：几个会话台同时改不同对话的标注，谁的都不能丢（读改写整段持锁）。
    #[test]
    fn concurrent_edits_from_separate_boards_are_all_kept() {
        let dir = std::env::temp_dir().join(format!("huitai-notes-race-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let threads: Vec<_> = (0..6)
            .map(|n| {
                let dir = dir.clone();
                thread::spawn(move || {
                    // 每个线程一份自己的 Notes，等于一个会话台进程。
                    let (mut notes, _) = Notes::load(&dir).unwrap();
                    for i in 0..10 {
                        notes
                            .edit(Vendor::Codex, &format!("{n}-{i}"), Edit::Pin(true))
                            .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let (notes, problem) = Notes::load(&dir).unwrap();
        assert_eq!(problem, None);
        assert_eq!(notes.state.map.len(), 60);
        let _ = fs::remove_dir_all(&dir);
    }
}
