//! 缓存、增量扫描（FSEvents）、全文搜索、会话台自己的标注（改名/置顶/归档）。对外接口：`Index`。
//!
//! - 启动：[`Index::open`] 只打开缓存库，[`Index::list`] 直接读缓存，不扫描。
//! - 后台：[`Index::start`] 起一个线程，先增量扫一遍三家，再建全文索引，之后用 FSEvents 监听三家目录，
//!   有变化就再增量扫那一家。增量扫描只重读 mtime 或大小变了的会话文件；Codex 以它自己的状态库为主，
//!   不按文件组织，整家重扫（一次 sqlite 查询）。
//! - 解析版本：缓存记着每个会话文件、每条对话是适配器哪个解析版本读的（[`Source::parse_version`]）。
//!   适配器版本加了一，旧版本读的文件即使没变也重读，正文也重建；同时开着旧版 `ai` 时，
//!   旧版不重读、不覆盖、不删除新版本写的结果。
//! - 全文索引：SQLite FTS5，正文取每条对话的用户和 AI 文字消息（见 [`BODY_MESSAGES`]），顺带补齐精确消息数。
//!   记着数到会话文件哪里：文件只是变长时只读新增部分，新增的消息作为一段追加进正文；变短、开头或接缝处
//!   被改写、解析版本变了、追加的段太多（[`BODY_MAX_PARTS`]）时整份重读。
//! - 标注：数据目录的 `annotations.json`（改名、置顶、归档、项目显示名和整组归档、展开着的项目组；0.1.3 之后加的厂商的
//!   对话标注在 `annotations-more.json`），不写厂商文件。

mod backup;
mod notes;
mod snippet;
mod store;
mod text;
mod watch;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime};

use rusqlite::Connection;

use crate::core::{Account, Config, Session, SessionId, Vendor, VendorDirs};
use crate::sources::{self, PreviewMessage, Progress, Source, Trash};

pub use backup::{BackupReport, NEAR_DELETION, near_deletion, wants_backup};
pub use notes::{Edit, Note, ProjectEdit, ProjectNote};
pub use snippet::{Snippet, marks as term_marks, terms as search_terms};
pub use watch::{Background, Update};

use store::{BodyWrite, Stamp, Tx};

/// 每条对话最多取多少条消息进全文索引（从最新往前；每条消息取前 2000 字，单文件最多往回读 8MB，
/// 见 sources 的 `load_preview`）。
pub const BODY_MESSAGES: usize = 5000;

/// 正文最多追加几段，再多就整份重建一次（回到最后 [`BODY_MESSAGES`] 条），免得一直在写的对话段数无限涨。
pub const BODY_MAX_PARTS: u32 = 256;

/// 读一条对话得到的：正文（已切词，整份或追加的一段）、精确消息数、数到哪了。
struct Body {
    text: String,
    append: bool,
    exact: Option<usize>,
    progress: Option<Progress>,
}

/// 一次建多少条对话的全文索引（并行读，读完一个事务写入）。
const BODY_BATCH: usize = 64;

/// 列表里的一条：厂商给的元数据加会话台自己的标注。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub session: Session,
    pub note: Note,
}

impl Entry {
    /// 显示的标题：会话台里改的名字 > 厂商标题 > id。
    pub fn title(&self) -> &str {
        self.note
            .title
            .as_deref()
            .or(self.session.title.as_deref())
            .unwrap_or(self.session.id.native())
    }
}

/// 只查最后时间在这之内的对话的会话文件：在写的对话最后时间跟着往前走，列表再长也只 stat 几个文件。
pub const WRITE_CHECK: Duration = Duration::from_secs(30 * 60);

/// 最近可能还在写的对话（最后时间在 `now` 往前 [`WRITE_CHECK`] 之内）各自会话文件的 mtime；
/// 文件读不到的不列。界面拿它判断「别处在用」。
pub fn recent_writes<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
    now: SystemTime,
) -> HashMap<SessionId, SystemTime> {
    entries
        .into_iter()
        .filter(|e| {
            now.duration_since(e.session.last_active)
                .map_or(true, |d| d < WRITE_CHECK)
        })
        .filter_map(|e| {
            let m = std::fs::metadata(&e.session.path).and_then(|m| m.modified());
            Some((e.session.id.clone(), m.ok()?))
        })
        .collect()
}

/// 搜索结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub entry: Entry,
    /// 标题里命中（否则是正文里命中）。
    pub in_title: bool,
    /// 正文里也命中（只命中标题的是 `false`）：界面在这些结果下面显示命中片段（[`Index::snippet`]）。
    pub in_body: bool,
}

/// 一家一次增量扫描的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshReport {
    pub vendor: Option<Vendor>,
    /// 重读了几个会话文件（整家重扫的来源记内容变了的对话数）。
    pub read: usize,
    /// 没变的文件数（整家重扫的来源记没变的对话数）。
    pub unchanged: usize,
    /// 从列表里去掉的对话数。
    pub removed: usize,
    pub problems: Vec<String>,
    /// 缓存库坏了、已删掉重建：三家都要整家重扫。
    pub rebuilt: bool,
}

impl RefreshReport {
    /// 同一家另一个账号的结果并进来。
    fn merge(&mut self, other: RefreshReport) {
        self.read += other.read;
        self.unchanged += other.unchanged;
        self.removed += other.removed;
        self.problems.extend(other.problems);
        self.rebuilt |= other.rebuilt;
    }

    /// 列表可能变了。
    pub fn changed(&self) -> bool {
        self.read > 0 || self.removed > 0
    }
}

pub(crate) struct Inner {
    /// 数据目录（缓存库、标注文件在这里）。
    dir: PathBuf,
    sources: Vec<Box<dyn Source>>,
    /// 界面读列表、搜索用；写在另一个连接上，WAL 下读不等写。
    reader: Mutex<Connection>,
    writer: Mutex<Connection>,
    notes: Mutex<notes::Notes>,
    open_problems: Vec<String>,
    /// 升级时搬旧标注出的问题（见 [`Index::adopt_profile_notes`]）。
    adopt_problems: Mutex<Vec<String>>,
    /// 整家扫过、确认按文件组织的来源（[`Inner::sources`] 里的下标）：FSEvents 报来具体文件时只重读那几个。
    by_file: Mutex<HashSet<usize>>,
}

/// 会话索引。可以克隆，克隆共享同一份缓存。
#[derive(Clone)]
pub struct Index {
    inner: Arc<Inner>,
}

impl Index {
    /// 打开 `dir` 里的缓存和标注（目录不存在就建，权限 0700），不扫描。
    pub fn open(dir: &Path, sources: Vec<Box<dyn Source>>) -> io::Result<Self> {
        create_private_dir(dir)?;
        let writer = store::open(dir)?;
        let reader = store::open(dir)?;
        let (notes, problem) = notes::Notes::load(dir)?;
        Ok(Self {
            inner: Arc::new(Inner {
                dir: dir.to_path_buf(),
                sources,
                reader: Mutex::new(reader),
                writer: Mutex::new(writer),
                notes: Mutex::new(notes),
                open_problems: problem.into_iter().collect(),
                adopt_problems: Mutex::default(),
                by_file: Mutex::default(),
            }),
        })
    }

    /// 只读打开 `dir` 里的缓存和标注（`ai list`、`ai search` 用）：不建目录、不建表、不搬旧标注，标注文件坏了
    /// 当没有（不改名）。缓存库还没有时报 `NotFound`，别的版本建的报 `InvalidData`（见 `store::open_read_only`）。
    /// 只用来读列表、搜索、取片段；别在它上面调扫描和标注，写会失败。
    pub fn open_read_only(dir: &Path, sources: Vec<Box<dyn Source>>) -> io::Result<Self> {
        let reader = store::open_read_only(dir)?;
        let writer = store::open_read_only(dir)?;
        let (notes, problem) = notes::Notes::load_read_only(dir)?;
        Ok(Self {
            inner: Arc::new(Inner {
                dir: dir.to_path_buf(),
                sources,
                reader: Mutex::new(reader),
                writer: Mutex::new(writer),
                notes: Mutex::new(notes),
                open_problems: problem.into_iter().collect(),
                adopt_problems: Mutex::default(),
                by_file: Mutex::default(),
            }),
        })
    }

    /// 本机默认：数据目录 `~/Library/Application Support/huitai/`，本机全部账号的来源（见 `sources::local_accounts`）。
    /// 几个账号的对话放在同一个缓存库里，每条记着账号。
    pub fn open_default() -> io::Result<Self> {
        let not_found = || io::Error::new(io::ErrorKind::NotFound, "取不到 HOME");
        let dir = Config::data_dir().ok_or_else(not_found)?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(not_found)?;
        let trash = Trash::default_location().ok_or_else(not_found)?;
        let index = Self::open(&dir, sources::default_sources(&home, trash))?;
        index.adopt_profile_notes(&VendorDirs::legacy_from_env(&home), &dir, &home);
        Ok(index)
    }

    /// 多账号之前，改过数据目录（`CLAUDE_CONFIG_DIR` 等）启动的会话台把改名、置顶、归档存在
    /// `profiles/<名>/annotations.json`（见 [`VendorDirs::cache_dir`]），id 不带账号前缀。这一组目录第一次用新版打开时
    /// 搬进来：每条 id 换成它那个账号的写法，这边已有的不覆盖；搬完把旧文件改名，不再搬第二次。
    /// `dirs` 要按旧版的规则算（[`VendorDirs::legacy`]）：旧目录名和旧 id 属于哪个账号都照旧版当时读的目录。
    /// 搬不成只记进 [`Index::problems`]，不影响打开。
    pub fn adopt_profile_notes(&self, dirs: &VendorDirs, data_dir: &Path, home: &Path) {
        let old = dirs.cache_dir(data_dir, home);
        if old == data_dir {
            return;
        }
        let rename = |v: Vendor, id: &str| {
            let key = Account::key_for(v, dirs.dir(v), home);
            SessionId::in_account(&key, id).as_str().to_owned()
        };
        if let Err(e) = lock(&self.inner.notes).adopt(&old, rename) {
            lock(&self.inner.adopt_problems).push(format!("旧的改名置顶没搬过来：{e}"));
        }
    }

    /// 打开时遇到但不影响使用的问题（比如标注文件坏了已改名留底）。
    pub fn problems(&self) -> Vec<String> {
        let mut out = self.inner.open_problems.clone();
        out.extend(lock(&self.inner.adopt_problems).iter().cloned());
        out
    }

    /// 一家的对话（缓存里的），置顶的在前，其余按最后时间新的在前。含归档的，界面自己分组。
    pub fn list(&self, vendor: Vendor) -> io::Result<Vec<Entry>> {
        let rows = store::rows(&lock(&self.inner.reader), vendor)?;
        let notes = lock(&self.inner.notes);
        let mut out: Vec<Entry> = rows
            .into_iter()
            .filter(|r| self.has_source(&r.session))
            .map(|r| self.entry(&notes, r.session))
            .collect();
        drop(notes);
        // 缓存已按时间排好，稳定排序只把置顶的提前。
        out.sort_by_key(|e| !e.note.pinned);
        Ok(out)
    }

    /// 缓存里这条的账号现在还在（设置里删掉的账号、换了环境变量没认出来的目录，它的对话留在缓存里但不列出来：
    /// 账号加回来就又有了，不用重读）。
    fn has_source(&self, s: &Session) -> bool {
        sources::source_of(&self.inner.sources, s).is_some()
    }

    fn entry(&self, notes: &notes::Notes, session: Session) -> Entry {
        let note = notes
            .get(session.vendor, session.id.as_str())
            .cloned()
            .unwrap_or_default();
        Entry { session, note }
    }

    /// 在一家里搜标题（含改过的名字）和正文。按空白分词，每个词都要命中；
    /// 标题命中的在前（顺序同列表），之后是正文命中的（按相关度）。最多 `limit` 条。
    pub fn search(&self, vendor: Vendor, query: &str, limit: usize) -> io::Result<Vec<Hit>> {
        self.search_in(Some(vendor), query, limit)
    }

    /// 各家一起搜（用户不记得是在哪家聊的）：排法同 [`Index::search`]，标题命中的按最后时间新的在前、
    /// 置顶的提前，正文命中的按相关度混排（几家在同一张全文表里，相关度能直接比）。最多 `limit` 条。
    pub fn search_all(&self, query: &str, limit: usize) -> io::Result<Vec<Hit>> {
        self.search_in(None, query, limit)
    }

    /// `vendor` 为 `None` 是各家一起搜。
    fn search_in(&self, vendor: Option<Vendor>, query: &str, limit: usize) -> io::Result<Vec<Hit>> {
        let Some(fts) = text::fts_query(query) else {
            return Ok(Vec::new());
        };
        let reader = lock(&self.inner.reader);
        let mut rows = Vec::new();
        for v in vendor.map_or(Vendor::ALL.to_vec(), |v| vec![v]) {
            rows.extend(
                store::rows(&reader, v)?
                    .into_iter()
                    .filter(|r| self.has_source(&r.session)),
            );
        }
        // 每家各自已按时间新的在前；几家合在一起再按时间排一遍（稳定排序，一家时顺序不变）。
        rows.sort_by_key(|r| std::cmp::Reverse(r.session.last_active));
        // 停用账号的缓存先去掉再限条数，不然它们能挤掉可见的结果。
        let listed: HashSet<i64> = rows.iter().map(|r| r.rid).collect();
        let body_rids =
            store::search_body(&reader, vendor, &fts, limit, |rid| listed.contains(&rid))?;
        drop(reader);

        let notes = lock(&self.inner.notes);
        let mut entries: HashMap<i64, Entry> = HashMap::with_capacity(rows.len());
        let mut titled: Vec<(i64, bool)> = Vec::new();
        // rows 按时间新的在前；标题命中的保持这个顺序，再把置顶的提前。
        for r in rows {
            let e = self.entry(&notes, r.session);
            if text::title_matches(e.title(), query) {
                titled.push((r.rid, e.note.pinned));
            }
            entries.insert(r.rid, e);
        }
        drop(notes);
        titled.sort_by_key(|(_, pinned)| !pinned);

        let title_rids: Vec<i64> = titled.into_iter().map(|(rid, _)| rid).collect();
        let in_body: HashSet<i64> = body_rids.iter().copied().collect();
        let body_only = body_rids
            .into_iter()
            .filter(|rid| !title_rids.contains(rid));
        let hits = title_rids
            .iter()
            .map(|rid| (*rid, true))
            .chain(body_only.map(|rid| (rid, false)))
            .filter_map(|(rid, in_title)| {
                entries.remove(&rid).map(|entry| Hit {
                    entry,
                    in_title,
                    in_body: in_body.contains(&rid),
                })
            })
            .take(limit)
            .collect();
        Ok(hits)
    }

    /// 搜索结果行下的命中片段：读回这条对话的整段（[`Source::load_transcript`]，大文件只读最后一段）
    /// 找搜索词。全文索引不存原文，只能这样取；要读文件，只给屏幕上看得见的几条取，别在搜索里调。
    pub fn snippet(&self, session: &Session, query: &str) -> io::Result<Option<Snippet>> {
        let transcript = self.source(session)?.load_transcript(session)?;
        Ok(snippet::find(&transcript, query))
    }

    /// 改名、置顶、归档，立即写进 `annotations.json`。
    pub fn annotate(&self, vendor: Vendor, id: &SessionId, edit: Edit) -> io::Result<()> {
        lock(&self.inner.notes).edit(vendor, id.as_str(), edit)
    }

    /// 列表里展开着的项目组（项目目录）。项目组默认收起。
    pub fn expanded_projects(&self) -> HashSet<PathBuf> {
        lock(&self.inner.notes).expanded().iter().cloned().collect()
    }

    /// 各项目的显示名和整组归档（项目目录 → 标注，三家共用）。
    pub fn projects(&self) -> HashMap<PathBuf, ProjectNote> {
        lock(&self.inner.notes)
            .projects()
            .iter()
            .map(|(p, n)| (p.clone(), n.clone()))
            .collect()
    }

    /// 给项目起显示名、整组归档，立即写进 `annotations.json`；不动文件夹和对话。
    pub fn annotate_project(&self, project: &Path, edit: ProjectEdit) -> io::Result<()> {
        lock(&self.inner.notes).edit_project(project, edit)
    }

    /// 记下项目组展开或收起，立即写进 `annotations.json`，下次打开保持。
    pub fn set_expanded(&self, project: &Path, expanded: bool) -> io::Result<()> {
        lock(&self.inner.notes).set_expanded(project, expanded)
    }

    /// 增量扫描全部来源（同步，界面别在主线程调；后台见 [`Index::start`]）。
    pub fn refresh(&self) -> Vec<RefreshReport> {
        (0..self.inner.sources.len())
            .map(|i| self.refresh_at(i, None))
            .collect()
    }

    /// 只增量扫描一家（全部账号；比如删除进回收站、恢复之后）。
    pub fn refresh_vendor(&self, vendor: Vendor) -> RefreshReport {
        let mut report = RefreshReport {
            vendor: Some(vendor),
            ..RefreshReport::default()
        };
        for i in self.sources_of(vendor) {
            report.merge(self.refresh_at(i, None));
        }
        report
    }

    /// 只看这几个变了的路径（FSEvents 报来的），按监听目录分给这家各账号的来源，见 [`Index::refresh_at_paths`]。
    pub fn refresh_paths(&self, vendor: Vendor, paths: &[PathBuf]) -> RefreshReport {
        let mut report = RefreshReport {
            vendor: Some(vendor),
            ..RefreshReport::default()
        };
        for i in self.sources_of(vendor) {
            let roots = self.inner.sources[i].watch_roots();
            let mine: Vec<PathBuf> = paths
                .iter()
                .filter(|p| roots.iter().any(|r| p.starts_with(r)))
                .cloned()
                .collect();
            if !mine.is_empty() {
                report.merge(self.refresh_at_paths(i, &mine));
            }
        }
        report
    }

    /// 第 `i` 个来源只看这几个变了的路径。按文件组织的来源只 stat、重读这几个会话文件，
    /// 不再列整个目录；路径里有目录、或这个来源不按文件组织时，退回整个来源增量扫描。
    pub(crate) fn refresh_at_paths(&self, i: usize, paths: &[PathBuf]) -> RefreshReport {
        let src = self.inner.sources[i].as_ref();
        if !lock(&self.inner.by_file).contains(&i) {
            return self.refresh_at(i, None);
        }
        // 缓存状态读不了（库坏了等）就整个扫，错误和重建在那条路上处理。
        let Ok(old) = store::stamps(&lock(&self.inner.writer), src.vendor(), src.account()) else {
            return self.refresh_at(i, None);
        };
        // 认识的会话文件（缓存里有，或现在是个文件）才单独处理；目录建删、改名要整个重列。
        let all_files = paths.iter().all(|p| old.contains_key(p) || p.is_file());
        self.refresh_at(i, all_files.then_some(paths))
    }

    /// 这家各账号的来源在 [`Inner::sources`] 里的下标。
    fn sources_of(&self, vendor: Vendor) -> Vec<usize> {
        (0..self.inner.sources.len())
            .filter(|&i| self.inner.sources[i].vendor() == vendor)
            .collect()
    }

    /// 第 `i` 个来源：`only` 给了就只看这些会话文件，否则整个扫。
    pub(crate) fn refresh_at(&self, i: usize, only: Option<&[PathBuf]>) -> RefreshReport {
        let src = self.inner.sources[i].as_ref();
        let vendor = src.vendor();
        let mut report = RefreshReport {
            vendor: Some(vendor),
            ..RefreshReport::default()
        };
        let res = match only {
            Some(paths) => self.refresh_files(src, paths, false, &mut report),
            None => match src.session_files() {
                Some(files) => {
                    lock(&self.inner.by_file).insert(i);
                    files.and_then(|f| self.refresh_files(src, &f, true, &mut report))
                }
                None => self.refresh_whole(src, &mut report),
            },
        };
        if let Err(e) = res {
            match self.recover(&e) {
                Some(msg) => {
                    report.rebuilt = true;
                    report.problems.push(msg);
                }
                None => report.problems.push(format!("{vendor}: {e}")),
            }
        }
        report
    }

    /// 完整检查一遍缓存库，坏了就删掉重建（之后要整家重扫三家）。返回发生了什么，没坏返回 `None`。
    /// 要几十毫秒，后台线程启动时调。
    pub(crate) fn check_cache(&self) -> Option<String> {
        let bad = store::check(&lock(&self.inner.writer)).err()?;
        Some(self.rebuild_with_message(&bad))
    }

    /// `e` 说明缓存库坏了时删掉重建，返回说明；别的错误返回 `None`。
    pub(crate) fn recover(&self, e: &io::Error) -> Option<String> {
        store::is_corrupt(e).then(|| self.rebuild_with_message(&e.to_string()))
    }

    fn rebuild_with_message(&self, why: &str) -> String {
        match self.rebuild() {
            Ok(()) => {
                format!("索引缓存坏了（{why}），已删掉重建；改名、置顶、归档单独存着，不受影响")
            }
            Err(e) => format!("索引缓存坏了（{why}），重建失败：{e}"),
        }
    }

    /// 关掉两个连接、删掉缓存库、重新建。标注在 `annotations.json`，不受影响。
    fn rebuild(&self) -> io::Result<()> {
        let mut writer = lock(&self.inner.writer);
        let mut reader = lock(&self.inner.reader);
        // 先换成内存库，旧文件的句柄全部关掉再删。
        *writer = Connection::open_in_memory().map_err(store::sql_err)?;
        *reader = Connection::open_in_memory().map_err(store::sql_err)?;
        store::remove(&self.inner.dir);
        *writer = store::open(&self.inner.dir)?;
        *reader = store::open(&self.inner.dir)?;
        lock(&self.inner.by_file).clear();
        Ok(())
    }

    /// 按文件组织的来源：对比 mtime 和大小，只读变了的、新的、旧解析版本读的，删掉没了的。
    /// `full` 为真时 `files` 是这家全部会话文件，缓存里有、列表里没有的算没了；
    /// 否则 `files` 只是变了的那几个，其中不存在的算没了。
    fn refresh_files(
        &self,
        src: &dyn Source,
        files: &[PathBuf],
        full: bool,
        report: &mut RefreshReport,
    ) -> io::Result<()> {
        let (vendor, account) = (src.vendor(), src.account());
        let mine = src.parse_version();
        let old = store::stamps(&lock(&self.inner.writer), vendor, account)?;
        let mut changed: Vec<(PathBuf, Stamp)> = Vec::new();
        let mut present: HashSet<&Path> = HashSet::new();
        for path in files {
            // 列完到这里之间被删掉的，按没了处理。
            let Ok(stamp) = Stamp::of(path) else {
                continue;
            };
            present.insert(path);
            // 更新的解析版本读过的归它管（它也在监听，会自己重读）。
            let fresh = old
                .get(path)
                .is_some_and(|s| s.parser > mine || (s.parser == mine && s.stamp == stamp));
            if fresh {
                report.unchanged += 1;
            } else {
                changed.push((path.clone(), stamp));
            }
        }
        let mut gone: Vec<&PathBuf> = if full {
            old.keys()
                .filter(|p| !present.contains(p.as_path()))
                .collect()
        } else {
            files
                .iter()
                .filter(|p| !present.contains(p.as_path()) && old.contains_key(*p))
                .collect()
        };
        // 一条对话由几个文件读出（Kimi 的 state.json 和正文）时，缓存里这条只记着最后读的那个文件：单单没了
        // 另一个，按文件删不到它。同一条对话的其余文件也重读，由来源重新判定这条还在不在。
        let orphaned: HashSet<&str> = gone
            .iter()
            .filter_map(|p| old.get(*p)?.session.as_deref())
            .collect();
        for (path, seen) in &old {
            let sibling = seen
                .session
                .as_deref()
                .is_some_and(|id| orphaned.contains(id));
            if !sibling || gone.contains(&path) || changed.iter().any(|(p, _)| p == path) {
                continue;
            }
            match Stamp::of(path) {
                Ok(stamp) => changed.push((path.clone(), stamp)),
                Err(_) => gone.push(path),
            }
        }
        // 没了的会话文件有替身（Claude 的会话台备份）就读替身，这条对话不从列表里消失。
        // 整家重扫时替身已经在 `files` 里了。
        if !full {
            for path in &gone {
                let Some(f) = src.stand_in(path) else {
                    continue;
                };
                if let Ok(stamp) = Stamp::of(&f)
                    && !changed.iter().any(|(p, _)| *p == f)
                {
                    changed.push((f, stamp));
                }
            }
        }
        if changed.is_empty() && gone.is_empty() {
            return Ok(());
        }

        // 读文件不占着锁。
        let paths: Vec<PathBuf> = changed.iter().map(|(p, _)| p.clone()).collect();
        let results = src.read_files(&paths);

        let mut conn = lock(&self.inner.writer);
        let tx = Tx::begin(&mut conn)?;
        for path in gone {
            // 文件没了是事实，不管哪个版本读的都删。
            report.removed += tx.remove_file_sessions(path, None, u32::MAX)?;
            tx.remove_stamp(path)?;
        }
        for ((path, stamp), res) in changed.iter().zip(results) {
            let session = match res {
                Ok(Some(s)) => {
                    report.removed += tx.remove_file_sessions(path, Some(s.id.as_str()), mine)?;
                    tx.upsert(&s, Some(path), mine, None)?;
                    report.read += 1;
                    Some(s.id)
                }
                Ok(None) => {
                    report.removed += tx.remove_file_sessions(path, None, mine)?;
                    report.read += 1;
                    None
                }
                // 读失败不记 mtime，下次再试；缓存里原来的留着。
                Err(e) => {
                    report.problems.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            let session = session.as_ref().map(|id| id.as_str());
            tx.set_stamp(vendor, account, path, (*stamp, session), mine)?;
        }
        tx.commit()
    }

    /// 不按文件组织的来源（Codex、Hermes、OpenCode 的库）：整家扫，按 id 对齐。内容版本和元数据（标题、分支……）
    /// 有一样变了就重写这条；只改了标题时内容版本不变，正文不重建。
    fn refresh_whole(&self, src: &dyn Source, report: &mut RefreshReport) -> io::Result<()> {
        let scan = src.scan();
        report.problems.extend(scan.problems);
        let mut conn = lock(&self.inner.writer);
        // 一条都没扫到又有报错（库被锁、查询超时）：多半是暂时的，留着缓存。
        if scan.sessions.is_empty() && !report.problems.is_empty() {
            return Ok(());
        }
        let mine = src.parse_version();
        let old = store::known(&conn, src.vendor(), src.account())?;
        let tx = Tx::begin(&mut conn)?;
        for s in &scan.sessions {
            // 修订号按厂商自己的 id 记（账号包装只给对话 id 加前缀）。
            let revision = scan.revisions.get(s.id.split_account().1).copied();
            let fresh = match old.get(s.id.as_str()) {
                Some(k) if k.parser > mine => true,
                Some(k) => {
                    k.key == store::content_key(s, mine, revision)
                        && k.data == store::session_data(s)?
                }
                None => false,
            };
            if fresh {
                report.unchanged += 1;
            } else {
                tx.upsert(s, None, mine, revision)?;
                report.read += 1;
            }
        }
        let keep: Vec<&str> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
        report.removed = tx.retain(src.vendor(), src.account(), &keep, mine)?;
        tx.commit()
    }

    /// 给内容变了的对话建全文索引，直到没有待建的。返回建了几条。
    /// `keep_going` 每批之前问一次，返回 `false` 就停（后台线程退出时用）。
    pub fn index_bodies(&self, keep_going: &mut dyn FnMut() -> bool) -> io::Result<usize> {
        let mut done = 0;
        let mut failed: HashSet<i64> = HashSet::new();
        while keep_going() {
            let batch: Vec<store::Pending> =
                store::pending_bodies(&lock(&self.inner.writer), BODY_BATCH + failed.len())?
                    .into_iter()
                    .filter(|p| !failed.contains(&p.rid))
                    .take(BODY_BATCH)
                    .collect();
            if batch.is_empty() {
                break;
            }
            let bodies = self.load_bodies(&batch);
            let mut conn = lock(&self.inner.writer);
            let tx = Tx::begin(&mut conn)?;
            for (p, body) in batch.iter().zip(bodies) {
                match body {
                    Ok(b) => {
                        let write = if b.append {
                            BodyWrite::Append(&b.text)
                        } else {
                            BodyWrite::Replace(&b.text)
                        };
                        // 读的过程中又变了：留给下一轮，这一轮不再试。
                        if !tx.set_body(p, write, b.exact, b.progress)? {
                            failed.insert(p.rid);
                            continue;
                        }
                        done += 1;
                    }
                    // 这一轮不再试，下次内容变了或重开时再试。
                    Err(_) => {
                        failed.insert(p.rid);
                    }
                }
            }
            tx.commit()?;
        }
        Ok(done)
    }

    /// 并行读一批对话的正文和精确消息数，结果和输入一一对应。
    fn load_bodies(&self, batch: &[store::Pending]) -> Vec<io::Result<Body>> {
        let next = AtomicUsize::new(0);
        let workers = thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(8)
            .min(batch.len());
        let mut slots: Vec<Option<io::Result<Body>>> = (0..batch.len()).map(|_| None).collect();
        thread::scope(|s| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    s.spawn(|| {
                        let mut mine = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(p) = batch.get(i) else { break };
                            mine.push((i, self.load_body(p)));
                        }
                        mine
                    })
                })
                .collect();
            for (i, r) in handles.into_iter().filter_map(|h| h.join().ok()).flatten() {
                slots[i] = Some(r);
            }
        });
        slots
            .into_iter()
            .map(|r| r.unwrap_or_else(|| Err(io::Error::other("读正文的线程出错"))))
            .collect()
    }

    fn load_body(&self, p: &store::Pending) -> io::Result<Body> {
        let session = &p.session;
        let src = self.source(session)?;
        // 会话文件在的才数；Codex 库里的计数本来就是准的。先数后取正文：两次之间追加的消息
        // 宁可下次再追加一遍（搜索不受影响），也不漏掉。
        let counted = if session.path.is_file() {
            Some(src.count_since(session, p.progress)?)
        } else {
            None
        };
        let (exact, progress, appended) = match counted {
            Some(c) => (Some(c.users), c.progress, c.appended),
            None => (None, None, None),
        };
        let appended = appended.filter(|_| p.parts < BODY_MAX_PARTS);
        let append = appended.is_some();
        let messages = match appended {
            Some(m) => m,
            None => src.load_preview(session, BODY_MESSAGES)?,
        };
        Ok(Body {
            text: body_text(&messages),
            append,
            exact,
            progress,
        })
    }

    fn source(&self, session: &Session) -> io::Result<&dyn Source> {
        sources::source_of(&self.inner.sources, session)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "没有这个账号的来源"))
    }

    pub(crate) fn sources(&self) -> &[Box<dyn Source>] {
        &self.inner.sources
    }
}

/// 消息切词后拼成正文，一条一行。
fn body_text(messages: &[PreviewMessage]) -> String {
    let mut body = String::new();
    for m in messages {
        body.push_str(&text::split(&m.text));
        body.push('\n');
    }
    body
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // 持锁的代码里没有会 panic 的地方；真 panic 了数据也只是缓存，继续用。
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 建数据目录（0700）；已经在、但别的账号能进（从备份恢复、手动改过权限），收回到 0700：
/// 缓存里有标题和正文词项。目录不属于当前用户就报错、不打开。
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let with_path = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", dir.display()));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(with_path)?;
    let meta = std::fs::metadata(dir).map_err(with_path)?;
    // SAFETY: geteuid 没有前置条件，总是成功。
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me {
        return Err(with_path(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("数据目录属于别的账号（uid {}），不打开", meta.uid()),
        )));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(with_path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// 审计第 12 条：数据目录已经在、权限过宽（备份恢复、手动改过），打开缓存前收回到 0700。
    #[test]
    fn open_tightens_an_existing_wide_open_data_dir() {
        let dir = std::env::temp_dir().join(format!("huitai-index-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let index = Index::open(&dir, Vec::new()).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        drop(index);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn data_dir_owned_by_someone_else_is_refused() {
        // 根目录属于 root：不 chmod、不打开，报错说清楚。
        let err = create_private_dir(Path::new("/")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("别的账号"), "{err}");
    }
}
