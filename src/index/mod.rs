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
//! - 标注：数据目录的 `annotations.json`（改名、置顶、归档、项目显示名和整组归档、展开着的项目组），不写厂商文件。

mod notes;
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

use crate::core::{Config, Session, SessionId, Vendor, VendorDirs};
use crate::sources::{self, PreviewMessage, Progress, Source, Trash};

pub use notes::{Edit, Note, ProjectEdit, ProjectNote};
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
            .unwrap_or(self.session.id.as_str())
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
    /// 整家扫过、确认按文件组织的厂商：FSEvents 报来具体文件时只重读那几个。
    by_file: Mutex<HashSet<Vendor>>,
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
                by_file: Mutex::default(),
            }),
        })
    }

    /// 本机默认：数据目录 `~/Library/Application Support/huitai/`，三家默认来源。
    pub fn open_default() -> io::Result<Self> {
        let not_found = || io::Error::new(io::ErrorKind::NotFound, "取不到 HOME");
        let dir = Config::data_dir().ok_or_else(not_found)?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(not_found)?;
        let trash = Trash::default_location().ok_or_else(not_found)?;
        // 改过数据目录（CLAUDE_CONFIG_DIR 等）的列表缓存单独放，换一组目录启动不会混进另一组的对话和搜索结果。
        let dir = VendorDirs::from_env(&home).cache_dir(&dir, &home);
        Self::open(&dir, sources::default_sources(&home, trash))
    }

    /// 打开时遇到但不影响使用的问题（比如标注文件坏了已改名留底）。
    pub fn problems(&self) -> &[String] {
        &self.inner.open_problems
    }

    /// 一家的对话（缓存里的），置顶的在前，其余按最后时间新的在前。含归档的，界面自己分组。
    pub fn list(&self, vendor: Vendor) -> io::Result<Vec<Entry>> {
        let rows = store::rows(&lock(&self.inner.reader), vendor)?;
        let notes = lock(&self.inner.notes);
        let mut out: Vec<Entry> = rows
            .into_iter()
            .map(|r| self.entry(&notes, r.session))
            .collect();
        drop(notes);
        // 缓存已按时间排好，稳定排序只把置顶的提前。
        out.sort_by_key(|e| !e.note.pinned);
        Ok(out)
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
        let Some(fts) = text::fts_query(query) else {
            return Ok(Vec::new());
        };
        let reader = lock(&self.inner.reader);
        let rows = store::rows(&reader, vendor)?;
        let body_rids = store::search_body(&reader, vendor, &fts, limit)?;
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
        let body_only = body_rids
            .into_iter()
            .filter(|rid| !title_rids.contains(rid));
        let hits = title_rids
            .iter()
            .map(|rid| (*rid, true))
            .chain(body_only.map(|rid| (rid, false)))
            .filter_map(|(rid, in_title)| entries.remove(&rid).map(|entry| Hit { entry, in_title }))
            .take(limit)
            .collect();
        Ok(hits)
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

    /// 增量扫描全部三家（同步，界面别在主线程调；后台见 [`Index::start`]）。
    pub fn refresh(&self) -> Vec<RefreshReport> {
        Vendor::ALL
            .iter()
            .filter_map(|v| self.source(*v))
            .map(|src| self.refresh_source(src, None))
            .collect()
    }

    /// 只增量扫描一家（比如删除进回收站、恢复之后）。
    pub fn refresh_vendor(&self, vendor: Vendor) -> RefreshReport {
        match self.source(vendor) {
            Some(src) => self.refresh_source(src, None),
            None => RefreshReport {
                vendor: Some(vendor),
                ..RefreshReport::default()
            },
        }
    }

    /// 只看这几个变了的路径（FSEvents 报来的）。按文件组织的来源只 stat、重读这几个会话文件，
    /// 不再列整个目录；路径里有目录、或这家不按文件组织时，退回整家增量扫描。
    pub fn refresh_paths(&self, vendor: Vendor, paths: &[PathBuf]) -> RefreshReport {
        let Some(src) = self.source(vendor) else {
            return RefreshReport {
                vendor: Some(vendor),
                ..RefreshReport::default()
            };
        };
        let by_file = lock(&self.inner.by_file).contains(&vendor);
        if !by_file {
            return self.refresh_source(src, None);
        }
        // 缓存状态读不了（库坏了等）就整家扫，错误和重建在那条路上处理。
        let Ok(old) = store::stamps(&lock(&self.inner.writer), vendor) else {
            return self.refresh_source(src, None);
        };
        // 认识的会话文件（缓存里有，或现在是个文件）才单独处理；目录建删、改名要整家重列。
        let all_files = paths.iter().all(|p| old.contains_key(p) || p.is_file());
        self.refresh_source(src, all_files.then_some(paths))
    }

    fn source(&self, vendor: Vendor) -> Option<&dyn Source> {
        self.inner
            .sources
            .iter()
            .find(|s| s.vendor() == vendor)
            .map(|s| s.as_ref())
    }

    /// `only` 给了就只看这些会话文件，否则整家扫。
    fn refresh_source(&self, src: &dyn Source, only: Option<&[PathBuf]>) -> RefreshReport {
        let vendor = src.vendor();
        let mut report = RefreshReport {
            vendor: Some(vendor),
            ..RefreshReport::default()
        };
        let res = match only {
            Some(paths) => self.refresh_files(src, paths, false, &mut report),
            None => match src.session_files() {
                Some(files) => {
                    lock(&self.inner.by_file).insert(vendor);
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
        let vendor = src.vendor();
        let mine = src.parse_version();
        let old = store::stamps(&lock(&self.inner.writer), vendor)?;
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
        let gone: Vec<&PathBuf> = if full {
            old.keys()
                .filter(|p| !present.contains(p.as_path()))
                .collect()
        } else {
            files
                .iter()
                .filter(|p| !present.contains(p.as_path()) && old.contains_key(*p))
                .collect()
        };
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
            match res {
                Ok(Some(s)) => {
                    report.removed += tx.remove_file_sessions(path, Some(s.id.as_str()), mine)?;
                    tx.upsert(&s, Some(path), mine)?;
                    report.read += 1;
                }
                Ok(None) => {
                    report.removed += tx.remove_file_sessions(path, None, mine)?;
                    report.read += 1;
                }
                // 读失败不记 mtime，下次再试；缓存里原来的留着。
                Err(e) => {
                    report.problems.push(format!("{}: {e}", path.display()));
                    continue;
                }
            }
            tx.set_stamp(vendor, path, *stamp, mine)?;
        }
        tx.commit()
    }

    /// 不按文件组织的来源（Codex）：整家扫，按 id 对齐。
    fn refresh_whole(&self, src: &dyn Source, report: &mut RefreshReport) -> io::Result<()> {
        let scan = src.scan();
        report.problems.extend(scan.problems);
        let mut conn = lock(&self.inner.writer);
        // 一条都没扫到又有报错（库被锁、查询超时）：多半是暂时的，留着缓存。
        if scan.sessions.is_empty() && !report.problems.is_empty() {
            return Ok(());
        }
        let mine = src.parse_version();
        let old = store::keys(&conn, src.vendor())?;
        let tx = Tx::begin(&mut conn)?;
        for s in &scan.sessions {
            let fresh = old
                .get(s.id.as_str())
                .is_some_and(|(key, parser)| *parser > mine || *key == store::content_key(s, mine));
            if fresh {
                report.unchanged += 1;
            } else {
                tx.upsert(s, None, mine)?;
                report.read += 1;
            }
        }
        let keep: Vec<&str> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
        report.removed = tx.retain(src.vendor(), &keep, mine)?;
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
        let src = self
            .source(session.vendor)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "没有这家的来源"))?;
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
