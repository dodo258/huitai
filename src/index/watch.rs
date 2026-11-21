//! 后台线程：先增量扫一遍三家、建全文索引，之后用 FSEvents（notify 的 macOS 后端）监听三家目录，
//! 有变化就增量扫那一家。事件攒一下再扫：最后一个事件后安静 300ms，或第一个事件后已过 1 秒。
//! 全文索引最多每 5 秒补一次，正在写的对话不会每秒重建。没事时线程阻塞在通道上，不占 CPU。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use super::Index;
use crate::core::Vendor;

const QUIET: Duration = Duration::from_millis(300);
const MAX_WAIT: Duration = Duration::from_secs(1);
const BODY_INTERVAL: Duration = Duration::from_secs(5);

/// 后台线程发给界面的消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// 这一家的列表可能变了，重新 `list`。
    Listed(Vendor),
    /// 又有一批对话的正文能搜到了。
    Searchable,
    /// 扫描或监听出了问题（单个文件读不了、监听开不了等），给日志和 `ai doctor` 看。
    Problem(String),
}

enum Msg {
    Fs(Vec<PathBuf>),
    /// 监听报错（事件丢了等），整家重扫。
    Rescan,
    Stop,
}

/// 后台扫描和监听。丢掉它就停止监听并等线程退出。
pub struct Background {
    watcher: Option<RecommendedWatcher>,
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Background {
    fn drop(&mut self) {
        self.watcher.take();
        let _ = self.tx.send(Msg::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Index {
    /// 起后台线程：增量扫描、建全文索引、监听变化。变化通过返回的通道告诉界面。
    pub fn start(&self) -> (Background, Receiver<Update>) {
        let (tx, rx) = mpsc::channel::<Msg>();
        let (up_tx, up_rx) = mpsc::channel::<Update>();

        let roots: Vec<(Vendor, PathBuf, PathBuf)> = self
            .sources()
            .iter()
            .flat_map(|s| {
                s.watch_roots().into_iter().map(move |r| {
                    // FSEvents 报的是真实路径（/var → /private/var），比较时两种都认。
                    let real = r.canonicalize().unwrap_or_else(|_| r.clone());
                    (s.vendor(), r, real)
                })
            })
            .collect();

        let watcher = match watch(&roots, tx.clone()) {
            Ok(w) => Some(w),
            Err(e) => {
                let _ = up_tx.send(Update::Problem(format!(
                    "监听三家目录失败，只能手动刷新: {e}"
                )));
                None
            }
        };

        let index = self.clone();
        let problems = up_tx.clone();
        let thread = thread::Builder::new()
            .name("huitai-index".into())
            .spawn(move || run(&index, &roots, &rx, &up_tx))
            .map_err(|e| {
                let _ = problems.send(Update::Problem(format!("起不了后台扫描线程: {e}")));
            })
            .ok();
        (
            Background {
                watcher,
                tx,
                thread,
            },
            up_rx,
        )
    }
}

fn watch(
    roots: &[(Vendor, PathBuf, PathBuf)],
    tx: Sender<Msg>,
) -> notify::Result<RecommendedWatcher> {
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let msg = match res {
            Ok(ev) if ev.need_rescan() => Msg::Rescan,
            Ok(ev) => Msg::Fs(ev.paths),
            Err(_) => Msg::Rescan,
        };
        let _ = tx.send(msg);
    })?;
    for (_, root, _) in roots {
        // 没装这家（目录不存在）就不监听。
        if root.is_dir() {
            w.watch(root, RecursiveMode::Recursive)?;
        }
    }
    Ok(w)
}

/// 一家待扫的范围。
enum Dirty {
    /// 整家增量扫描（启动、丢了事件、目录变了）。
    All,
    /// 只看这些路径。
    Paths(HashSet<PathBuf>),
}

struct State {
    dirty: HashMap<Vendor, Dirty>,
    /// 这一批里第一个事件的时间。
    since: Option<Instant>,
    last_event: Instant,
    stop: bool,
}

impl State {
    /// 处理一条消息，把受影响的厂商和路径记进 `dirty`。
    fn take(&mut self, msg: Msg, roots: &[(Vendor, PathBuf, PathBuf)], index: &Index) {
        let mut hit = false;
        match msg {
            Msg::Stop => {
                self.stop = true;
                return;
            }
            Msg::Rescan => {
                for (v, _, _) in roots {
                    self.dirty.insert(*v, Dirty::All);
                    hit = true;
                }
            }
            Msg::Fs(paths) => {
                for (v, p) in paths.iter().filter_map(|p| vendor_of(p, roots, index)) {
                    hit = true;
                    match self
                        .dirty
                        .entry(v)
                        .or_insert_with(|| Dirty::Paths(HashSet::new()))
                    {
                        Dirty::All => {}
                        Dirty::Paths(set) => {
                            set.insert(p);
                        }
                    }
                }
            }
        }
        if hit {
            let now = Instant::now();
            self.since.get_or_insert(now);
            self.last_event = now;
        }
    }

    /// 缓存重建了：三家都整家重扫，马上扫。
    fn rescan_all(&mut self, roots: &[(Vendor, PathBuf, PathBuf)]) {
        for (v, _, _) in roots {
            self.dirty.insert(*v, Dirty::All);
        }
        self.last_event = Instant::now() - QUIET;
    }

    fn refresh_due(&self) -> bool {
        !self.dirty.is_empty()
            && (self.last_event.elapsed() >= QUIET
                || self.since.is_some_and(|t| t.elapsed() >= MAX_WAIT))
    }

    /// 离该扫描还有多久。
    fn refresh_wait(&self) -> Duration {
        let quiet = QUIET.saturating_sub(self.last_event.elapsed());
        let max = self
            .since
            .map_or(quiet, |t| MAX_WAIT.saturating_sub(t.elapsed()));
        quiet.min(max)
    }
}

fn run(
    index: &Index,
    roots: &[(Vendor, PathBuf, PathBuf)],
    rx: &Receiver<Msg>,
    up: &Sender<Update>,
) {
    let send = |u: Update| {
        let _ = up.send(u);
    };
    // 一开始三家都扫一遍；上次没建完的全文索引也接着建。
    let mut st = State {
        dirty: roots.iter().map(|(v, _, _)| (*v, Dirty::All)).collect(),
        since: None,
        last_event: Instant::now() - QUIET,
        stop: false,
    };
    let mut bodies_due = true;
    let mut last_body: Option<Instant> = None;
    // 先查缓存库坏没坏（截断、乱写）：坏了重建，下面三家整家扫一遍就补回来。
    if let Some(msg) = index.check_cache() {
        send(Update::Problem(msg));
    }

    while !st.stop {
        if st.refresh_due() {
            for (v, dirty) in std::mem::take(&mut st.dirty) {
                let report = match dirty {
                    Dirty::All => index.refresh_vendor(v),
                    Dirty::Paths(set) => {
                        index.refresh_paths(v, &set.into_iter().collect::<Vec<_>>())
                    }
                };
                if report.changed() {
                    send(Update::Listed(v));
                    bodies_due = true;
                }
                if report.rebuilt {
                    st.rescan_all(roots);
                }
                report
                    .problems
                    .into_iter()
                    .for_each(|p| send(Update::Problem(p)));
            }
            st.since = None;
        }

        let body_wait = last_body.map_or(Duration::ZERO, |t| {
            BODY_INTERVAL.saturating_sub(t.elapsed())
        });
        if st.dirty.is_empty() && bodies_due && body_wait.is_zero() {
            // 建索引中途也处理停止和新事件。
            let mut keep_going = || {
                while let Ok(msg) = rx.try_recv() {
                    st.take(msg, roots, index);
                }
                !st.stop
            };
            match index.index_bodies(&mut keep_going) {
                Ok(0) => {}
                Ok(_) => send(Update::Searchable),
                Err(e) => match index.recover(&e) {
                    Some(msg) => {
                        st.rescan_all(roots);
                        send(Update::Problem(msg));
                    }
                    None => send(Update::Problem(format!("全文索引: {e}"))),
                },
            }
            bodies_due = false;
            last_body = Some(Instant::now());
            continue;
        }

        // 有待扫的按攒事件的规则醒，有待建索引的按间隔醒，否则一直等（不占 CPU）。
        let wait = if !st.dirty.is_empty() {
            Some(st.refresh_wait())
        } else if bodies_due {
            Some(body_wait)
        } else {
            None
        };
        let msg = match wait {
            Some(d) => match rx.recv_timeout(d) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => break,
            },
        };
        st.take(msg, roots, index);
    }
}

/// 事件路径属于哪一家（且这家认为它影响列表），换回监听时给的写法。
fn vendor_of(
    path: &Path,
    roots: &[(Vendor, PathBuf, PathBuf)],
    index: &Index,
) -> Option<(Vendor, PathBuf)> {
    roots.iter().find_map(|(v, root, real)| {
        // 换回监听时给的写法，交给来源判断。
        let p = match path.strip_prefix(real) {
            Ok(rest) => root.join(rest),
            Err(_) if path.starts_with(root) => path.to_path_buf(),
            Err(_) => return None,
        };
        let affects = index
            .sources()
            .iter()
            .any(|s| s.vendor() == *v && s.affects(&p));
        affects.then_some((*v, p))
    })
}
