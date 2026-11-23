//! 会话台自己的标注（改名、置顶、归档，项目的显示名和整组归档，以及列表里展开着的项目组），
//! 存在数据目录的 `annotations.json`，从不写厂商文件。
//! 0.1.3 之后加的厂商的对话标注另放 `annotations-more.json`：0.1.3 读 `annotations.json` 时遇到不认得的厂商
//! 会把整份当坏文件改名留底，这份只放它认得的（[`IN_MAIN_FILE`]）。两份都按条读，不认得的条目原样留着写回。
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
/// 别的厂商的对话标注，0.1.3 不读它。
const MORE_FILE_NAME: &str = "annotations-more.json";
/// 0.1.3 认得的厂商：它们的对话标注留在 `annotations.json`，0.1.3 和新版都读得到。
const IN_MAIN_FILE: [Vendor; 8] = [
    Vendor::Claude,
    Vendor::Codex,
    Vendor::Pi,
    Vendor::Hermes,
    Vendor::Kimi,
    Vendor::Gemini,
    Vendor::OpenCode,
    Vendor::Qwen,
];
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
    /// 按条解，解不了的（以后的版本加的厂商）原样留着，见 [`State::unknown`]。
    notes: Vec<serde_json::Value>,
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

/// 两份文件里的全部内容。
#[derive(Clone, Default)]
struct State {
    map: Map,
    expanded: BTreeSet<PathBuf>,
    projects: BTreeMap<PathBuf, ProjectNote>,
    /// 解不了的对话标注（以后的版本加的厂商）：不用，原样写回 `annotations-more.json`。
    unknown: Vec<serde_json::Value>,
}

impl State {
    /// 有放错文件的：`annotations.json` 里有 0.1.3 不认得的厂商（新版改分文件之前写的），要搬到另一份。
    fn misplaced(&self) -> bool {
        !self.unknown.is_empty() || self.map.keys().any(|(v, _)| !IN_MAIN_FILE.contains(v))
    }
}

pub(crate) struct Notes {
    /// `annotations.json`；另一份在同目录（[`MORE_FILE_NAME`]）。
    path: PathBuf,
    state: State,
}

impl Notes {
    /// 读两份标注文件。文件不存在算空；格式坏了把原文件改名留底（`annotations.json.corrupt-<秒>`）、
    /// 从空开始，并在第二个返回值里说明。`annotations.json` 里有 0.1.3 不认得的厂商（分文件之前写的）就马上
    /// 分开写回，免得 0.1.3 打开时把整份当坏文件。
    pub(crate) fn load(dir: &Path) -> io::Result<(Self, Option<String>)> {
        let path = dir.join(FILE_NAME);
        let main = read_or_set_aside(&path)?;
        let more = read_or_set_aside(&path.with_file_name(MORE_FILE_NAME))?;
        let misplaced = main.0.as_ref().is_some_and(State::misplaced);
        let problem = [main.1, more.1]
            .into_iter()
            .flatten()
            .reduce(|a, b| format!("{a}；{b}"));
        let mut notes = Self {
            path,
            state: merge(main.0.unwrap_or_default(), more.0.unwrap_or_default()),
        };
        if misplaced {
            notes.update(|_| {})?;
        }
        Ok((notes, problem))
    }

    /// 只读：同 [`Notes::load`]，但格式坏了不改名留底，当没有标注，并在第二个返回值里说明。
    pub(crate) fn load_read_only(dir: &Path) -> io::Result<(Self, Option<String>)> {
        let path = dir.join(FILE_NAME);
        let (state, problem) = match read(&path) {
            Ok(st) => (st, None),
            Err(e) if e.kind() == ErrorKind::InvalidData => (
                State::default(),
                Some(format!("{e}；这次不显示改名、置顶、归档")),
            ),
            Err(e) => return Err(e),
        };
        Ok((Self { path, state }, problem))
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

    /// 把 `old_dir` 里的标注文件并进来（升级时搬旧位置的改名置顶）：每条的 id 经 `rename` 换成新写法，
    /// 这边已有的同一条、同一个项目不覆盖。并完把旧文件改名成 `annotations.json.merged`，不再并第二次。
    /// 旧文件不在就什么都不做；返回并进来几条对话标注。
    pub(crate) fn adopt(
        &mut self,
        old_dir: &Path,
        rename: impl Fn(Vendor, &str) -> String,
    ) -> io::Result<usize> {
        let old_path = old_dir.join(FILE_NAME);
        if !old_path.is_file() {
            return Ok(0);
        }
        let old = read(&old_path)?;
        let mut added = 0;
        let unknown = old.unknown;
        self.update(|st| {
            for ((vendor, id), note) in old.map {
                st.map
                    .entry((vendor, rename(vendor, &id)))
                    .or_insert_with(|| {
                        added += 1;
                        note
                    });
            }
            st.expanded.extend(old.expanded);
            for (p, n) in old.projects {
                st.projects.entry(p).or_insert(n);
            }
            st.unknown.extend(unknown);
        })?;
        let done = old_path.with_extension("json.merged");
        fs::rename(&old_path, &done)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", old_path.display())))?;
        Ok(added)
    }

    /// 先重读文件再改、写回，别的会话台进程刚改的不会被盖掉。读到写回整段持文件锁：
    /// 两个会话台同时改，后一个等前一个写完再读，不会各自基于同一份旧内容写、互相覆盖。
    fn update(&mut self, change: impl FnOnce(&mut State)) -> io::Result<()> {
        let _lock = lock_exclusive(&self.path.with_file_name(LOCK_NAME))?;
        let more_path = self.path.with_file_name(MORE_FILE_NAME);
        // 文件被改坏了：已改名留底，以内存里的为准写回。
        let main = read_or_set_aside(&self.path)?.0;
        let more = read_or_set_aside(&more_path)?.0;
        let mut state = match (main, more) {
            (Some(main), Some(more)) => merge(main, more),
            _ => self.state.clone(),
        };
        change(&mut state);
        // 先写另一份：中途失败时搬过去的标注两边都有，不会两边都没有。
        let (main, more) = split(&state);
        if !(more.map.is_empty() && more.unknown.is_empty() && !more_path.exists()) {
            write(&more_path, &more)?;
        }
        write(&self.path, &main)?;
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
    let mut map = Map::new();
    let mut unknown = Vec::new();
    for raw in file.notes {
        match Record::deserialize(&raw) {
            Ok(r) => {
                map.insert((r.vendor, r.id), r.note);
            }
            Err(_) => unknown.push(raw),
        }
    }
    Ok(State {
        map,
        expanded: file.expanded,
        projects: file.projects,
        unknown,
    })
}

/// 两份文件合成一份：对话标注两份里同一条以 `annotations.json` 的为准（只在搬到一半时才会两边都有）。
fn merge(main: State, more: State) -> State {
    let mut out = more;
    out.map.extend(main.map);
    out.expanded.extend(main.expanded);
    out.projects.extend(main.projects);
    for raw in main.unknown {
        if !out.unknown.contains(&raw) {
            out.unknown.push(raw);
        }
    }
    out
}

/// 分成两份要写的：0.1.3 认得的厂商的对话标注、项目标注、展开的组进 `annotations.json`，别的进另一份。
fn split(state: &State) -> (State, State) {
    let (main, more): (Map, Map) = state
        .map
        .iter()
        .map(|(k, n)| (k.clone(), n.clone()))
        .partition(|((v, _), _)| IN_MAIN_FILE.contains(v));
    (
        State {
            map: main,
            expanded: state.expanded.clone(),
            projects: state.projects.clone(),
            unknown: Vec::new(),
        },
        State {
            map: more,
            unknown: state.unknown.clone(),
            ..State::default()
        },
    )
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
    let mut notes: Vec<serde_json::Value> = notes
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()
        .map_err(io::Error::other)?;
    notes.extend(state.unknown.iter().cloned());
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

    /// 旧版写的标注里有 Qwen Code（不再支持）的对话：整份照读不报坏，改别的对话时它的标注原样留在
    /// `annotations.json`（0.1.3 还认它，同时开着的旧版看得到）。
    #[test]
    fn qwen_notes_from_old_versions_load_and_are_kept() {
        let dir = std::env::temp_dir().join(format!("huitai-notes-qwen-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(FILE_NAME),
            r#"{"version":1,"notes":[{"vendor":"qwen","id":"q1","pinned":true,"title":"旧标注"},{"vendor":"claude","id":"c1","archived":true}]}"#,
        )
        .unwrap();
        let (mut notes, problem) = Notes::load(&dir).unwrap();
        assert_eq!(problem, None);
        assert!(notes.get(Vendor::Qwen, "q1").is_some_and(|n| n.pinned));
        notes.edit(Vendor::Claude, "c2", Edit::Pin(true)).unwrap();
        let text = fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            doc["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["vendor"] == "qwen" && n["id"] == "q1"),
            "{text}"
        );
        let (again, _) = Notes::load(&dir).unwrap();
        assert_eq!(
            again
                .get(Vendor::Qwen, "q1")
                .and_then(|n| n.title.as_deref()),
            Some("旧标注")
        );
        let _ = fs::remove_dir_all(&dir);
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
