//! N（在别的目录新建）的目录输入：候选是三家出现过的项目目录加 `projects_root` 下的子目录，
//! 边打边筛、↑↓ 选、Tab 补全；⏎ 时把输入换成目录（只输一个名字就是 `projects_root/<名字>`）。
//! 候选最上面一行是「＋ 新建」（[`NewRow`]），就是 ↑↓ 的「不选」。
//! 不做 IO：`projects_root` 的子目录由运行层读好了交进来，目录在不在由调用方查。

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use crate::index::Entry;
use crate::sources::fold_group;

use super::text::tilde;

/// 候选列表最多显示几行。
pub const SHOWN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub dir: PathBuf,
    /// 显示的样子（家目录换成 `~`），也拿它筛。
    pub shown: String,
}

/// 候选列表最上面的「＋ 新建」那行写什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewRow {
    /// 还没输入：写怎么新建。
    Blank,
    /// 输入换算成的目录还不存在：⏎ 新建它。
    Dir(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirPicker {
    pub text: String,
    /// 全部候选：最近用过的项目在前，之后是 `projects_root` 下的子目录。
    pub candidates: Vec<Candidate>,
    /// ↑↓ 选中的是筛出来的第几个；`None` 是没选（停在「＋ 新建」那行），⏎ 用输入的文字。
    pub picked: Option<usize>,
    /// 「＋ 新建」那行；输入的目录已经有了就没有这行。
    pub new_row: Option<NewRow>,
    home: PathBuf,
}

impl DirPicker {
    pub fn new(home: &Path, dirs: Vec<PathBuf>) -> Self {
        let mut p = Self {
            text: String::new(),
            candidates: Vec::new(),
            picked: None,
            new_row: Some(NewRow::Blank),
            home: home.to_path_buf(),
        };
        p.add(dirs);
        p
    }

    /// 补进候选（读到的 `projects_root` 子目录），已有的不重复。
    pub fn add(&mut self, dirs: Vec<PathBuf>) {
        for dir in dirs {
            if self.candidates.iter().all(|c| c.dir != dir) {
                let shown = tilde(&dir, &self.home);
                self.candidates.push(Candidate { dir, shown });
            }
        }
    }

    /// 按输入筛出来的候选（在 `candidates` 里的下标）：目录名开头就对上的在前，其次目录名里有，
    /// 再次显示的路径（`~` 缩写）里有、或输入的绝对路径是它的开头；同一档保持原来的顺序。不分大小写，空输入列全部。
    pub fn matches(&self) -> Vec<usize> {
        let q = self.text.trim().to_lowercase();
        let mut hits: Vec<(u8, usize)> = self
            .candidates
            .iter()
            .enumerate()
            .filter_map(|(i, c)| rank(c, &q).map(|r| (r, i)))
            .collect();
        hits.sort();
        hits.into_iter().map(|(_, i)| i).collect()
    }

    pub fn edit(&mut self, change: impl FnOnce(&mut String)) {
        change(&mut self.text);
        self.picked = None;
    }

    /// ↓：没选时选第一个，到底停住。
    pub fn down(&mut self) {
        let n = self.matches().len();
        if n == 0 {
            return;
        }
        self.picked = Some(self.picked.map_or(0, |i| (i + 1).min(n - 1)));
    }

    /// ↑：回到第一个之上就是不选（「＋ 新建」那行，⏎ 用输入的文字）。
    pub fn up(&mut self) {
        self.picked = self.picked.and_then(|i| i.checked_sub(1));
    }

    /// Tab：把选中的（没选就是第一个）候选填进输入框。没有候选返回 `false`。
    pub fn complete(&mut self) -> bool {
        let m = self.matches();
        let Some(&i) = m.get(self.picked.unwrap_or(0)) else {
            return false;
        };
        self.text = self.candidates[i].shown.clone();
        self.picked = None;
        true
    }

    /// ⏎ 要去的目录：选了候选就是它，否则按 [`resolve`] 换算输入的文字。
    pub fn target(&self, root: &Path, base: &Path) -> Option<PathBuf> {
        if let Some(i) = self.picked
            && let Some(&c) = self.matches().get(i)
        {
            return Some(self.candidates[c].dir.clone());
        }
        self.typed(root, base)
    }

    /// 输入变了：重算「＋ 新建」那行，`exists` 查目录在不在。
    pub fn refresh_new_row(&mut self, root: &Path, base: &Path, exists: impl Fn(&Path) -> bool) {
        self.new_row = match self.typed(root, base) {
            None => Some(NewRow::Blank),
            Some(dir) if exists(&dir) => None,
            Some(dir) => Some(NewRow::Dir(dir)),
        };
    }

    fn typed(&self, root: &Path, base: &Path) -> Option<PathBuf> {
        resolve(&self.text, &self.home, root, base)
    }
}

/// 候选对不对得上输入、排第几档。
fn rank(c: &Candidate, q: &str) -> Option<u8> {
    if q.is_empty() {
        return Some(0);
    }
    let name = c
        .dir
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.starts_with(q) {
        Some(0)
    } else if name.contains(q) {
        Some(1)
    } else if c.shown.to_lowercase().contains(q)
        // 输入的是绝对路径时才对完整路径：不然家目录里的字母（/Users/<名字>）会让什么都对得上。
        || (q.starts_with('/') && c.dir.to_string_lossy().to_lowercase().starts_with(q))
    {
        Some(2)
    } else {
        None
    }
}

/// 输入的文字换成目录：`~`、`~/…` 在家目录下，`/…` 照原样，带 `/` 的相对路径和 `.`、`..` 相对 `base`
/// （会话台启动的目录），只有一个名字（不含 `/`）就是 `root/<名字>`。`..` 按字面消掉。空的返回 `None`。
pub fn resolve(text: &str, home: &Path, root: &Path, base: &Path) -> Option<PathBuf> {
    let t = text.trim();
    let p = if t.is_empty() {
        return None;
    } else if t == "~" || t.starts_with("~/") {
        expand_tilde(t, home)
    } else if t.starts_with('/') {
        PathBuf::from(t)
    } else if t.contains('/') || t == "." || t == ".." {
        base.join(t)
    } else {
        root.join(t)
    };
    Some(normalize(&p))
}

/// 配置里的 `projects_root` 换成目录：`~` 开头在家目录下，相对路径也算家目录下。
pub fn projects_root(setting: &str, home: &Path) -> PathBuf {
    let p = expand_tilde(setting.trim(), home);
    if p.is_absolute() { p } else { home.join(p) }
}

pub fn expand_tilde(text: &str, home: &Path) -> PathBuf {
    match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(text),
    }
}

/// 按字面去掉 `.`、消掉 `..`（不读文件系统）。
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => _ = out.pop(),
            c => out.push(c),
        }
    }
    out
}

/// 三家出现过的项目目录，最近用过的在前；agentq 工作树、临时目录、家目录这类不算项目的不列。
pub fn project_dirs<'a>(entries: impl IntoIterator<Item = &'a Entry>, home: &Path) -> Vec<PathBuf> {
    let mut latest: HashMap<&Path, SystemTime> = HashMap::new();
    for e in entries {
        let cwd = e.session.cwd.as_path();
        if fold_group(cwd, home).is_some() {
            continue;
        }
        let t = latest.entry(cwd).or_insert(e.session.last_active);
        *t = (*t).max(e.session.last_active);
    }
    let mut dirs: Vec<(&Path, SystemTime)> = latest.into_iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    dirs.into_iter().map(|(d, _)| d.to_path_buf()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tilde() {
        let home = Path::new("/Users/demo");
        assert_eq!(expand_tilde("~", home), PathBuf::from("/Users/demo"));
        assert_eq!(
            expand_tilde("~/a/b", home),
            PathBuf::from("/Users/demo/a/b")
        );
        assert_eq!(expand_tilde("/opt", home), PathBuf::from("/opt"));
        assert_eq!(expand_tilde("~x", home), PathBuf::from("~x"));
    }
}
