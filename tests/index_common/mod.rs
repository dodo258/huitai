//! index 集成测试共用：在临时目录里手写 Claude 格式的会话文件（脱敏假数据），包一层能数读了哪些文件的来源。

#![allow(dead_code)]

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use huitai::core::{Session, Vendor};
use huitai::index::Index;
use huitai::sources::{
    ClaudeSource, Command, Counted, PreviewMessage, Progress, Scan, Source, Trash, TrashEntry,
};
use serde_json::json;

pub const PROJECT: &str = "-Users-demo-projects-shop";

/// 每个测试自己的空临时目录。
pub fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("huitai-index-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// 一个测试环境：`root/claude/projects` 放会话文件，`root/data` 是会话台数据目录。
pub struct Env {
    pub root: PathBuf,
    pub reads: Arc<Mutex<Vec<PathBuf>>>,
    pub calls: Calls,
}

impl Env {
    pub fn new(name: &str) -> Self {
        let root = temp_dir(name);
        fs::create_dir_all(root.join("claude/projects").join(PROJECT)).unwrap();
        Self {
            root,
            reads: Arc::default(),
            calls: Calls::default(),
        }
    }

    pub fn projects(&self) -> PathBuf {
        self.root.join("claude/projects")
    }

    pub fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    pub fn session_path(&self, id: &str) -> PathBuf {
        self.projects().join(PROJECT).join(format!("{id}.jsonl"))
    }

    pub fn open(&self) -> Index {
        self.open_with(None, |_| {})
    }

    /// 用解析版本 `version`（`None` 用适配器自己的）打开，读出来的每条对话再交给 `tweak` 改一下
    /// （模拟那个版本的解析规则读出的结果）。
    pub fn open_with(&self, version: Option<u32>, tweak: fn(&mut Session)) -> Index {
        let src = Counting {
            inner: ClaudeSource::new(self.projects(), Trash::new(self.root.join("trash"))),
            reads: self.reads.clone(),
            calls: self.calls.clone(),
            version,
            tweak,
        };
        Index::open(&self.data(), vec![Box::new(src)]).unwrap()
    }

    /// 写一个会话：一问一答。
    pub fn write_session(&self, id: &str, user: &str, assistant: &str) {
        let mut f = fs::File::create(self.session_path(id)).unwrap();
        writeln!(f, "{}", user_line(id, user, "2026-11-01T09:00:00.000Z")).unwrap();
        writeln!(
            f,
            "{}",
            assistant_line(assistant, "2026-11-01T09:00:05.000Z")
        )
        .unwrap();
    }

    /// 在会话末尾追加一行。
    pub fn append(&self, id: &str, line: &str) {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(self.session_path(id))
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    pub fn take_reads(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.reads.lock().unwrap())
    }
}

pub fn user_line(id: &str, text: &str, at: &str) -> String {
    json!({
        "type": "user",
        "message": {"role": "user", "content": text},
        "timestamp": at,
        "cwd": "/Users/demo/projects/shop",
        "sessionId": id,
        "gitBranch": "main"
    })
    .to_string()
}

pub fn assistant_line(text: &str, at: &str) -> String {
    json!({
        "type": "assistant",
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
        "timestamp": at,
        "cwd": "/Users/demo/projects/shop"
    })
    .to_string()
}

pub fn title_line(title: &str) -> String {
    json!({"type": "custom-title", "customTitle": title}).to_string()
}

pub fn ids(entries: &[huitai::index::Entry]) -> Vec<String> {
    let mut v: Vec<String> = entries
        .iter()
        .map(|e| e.session.id.as_str().to_owned())
        .collect();
    v.sort();
    v
}

/// 一次 `count_since`：传进来的进度，和有没有交出新增的消息（接着数）。
pub type CountCall = (Option<Progress>, bool);

/// 建全文索引时调了来源哪些读法。
#[derive(Clone, Default)]
pub struct Calls {
    pub counts: Arc<Mutex<Vec<CountCall>>>,
    /// `load_preview` 调了几次（整份重取正文）。
    pub previews: Arc<Mutex<usize>>,
}

impl Calls {
    pub fn take_counts(&self) -> Vec<CountCall> {
        std::mem::take(&mut *self.counts.lock().unwrap())
    }

    pub fn take_previews(&self) -> usize {
        std::mem::take(&mut *self.previews.lock().unwrap())
    }
}

/// 代理 ClaudeSource，记下 `read_files` 读了哪些文件、建正文时怎么读的；可以换解析版本、改读出的结果。
pub struct Counting<S> {
    pub inner: S,
    pub reads: Arc<Mutex<Vec<PathBuf>>>,
    pub calls: Calls,
    pub version: Option<u32>,
    pub tweak: fn(&mut Session),
}

impl<S: Source> Source for Counting<S> {
    fn vendor(&self) -> Vendor {
        self.inner.vendor()
    }
    fn parse_version(&self) -> u32 {
        self.version.unwrap_or_else(|| self.inner.parse_version())
    }
    fn scan(&self) -> Scan {
        self.inner.scan()
    }
    fn load_preview(&self, s: &Session, n: usize) -> io::Result<Vec<PreviewMessage>> {
        *self.calls.previews.lock().unwrap() += 1;
        self.inner.load_preview(s, n)
    }
    fn count_messages(&self, s: &Session) -> io::Result<usize> {
        self.inner.count_messages(s)
    }
    fn count_since(&self, s: &Session, prev: Option<Progress>) -> io::Result<Counted> {
        let c = self.inner.count_since(s, prev)?;
        let resumed = c.appended.is_some();
        self.calls.counts.lock().unwrap().push((prev, resumed));
        Ok(c)
    }
    fn resume_cmd(&self, s: &Session) -> Command {
        self.inner.resume_cmd(s)
    }
    fn new_cmd(&self, cwd: &Path) -> Command {
        self.inner.new_cmd(cwd)
    }
    fn trash(&self, s: &Session) -> io::Result<TrashEntry> {
        self.inner.trash(s)
    }
    fn restore(&self, e: &TrashEntry) -> io::Result<()> {
        self.inner.restore(e)
    }
    fn watch_roots(&self) -> Vec<PathBuf> {
        self.inner.watch_roots()
    }
    fn session_files(&self) -> Option<io::Result<Vec<PathBuf>>> {
        self.inner.session_files()
    }
    fn read_files(&self, paths: &[PathBuf]) -> Vec<io::Result<Option<Session>>> {
        self.reads.lock().unwrap().extend_from_slice(paths);
        let mut out = self.inner.read_files(paths);
        for s in out.iter_mut().flatten().flatten() {
            (self.tweak)(s);
        }
        out
    }
}
