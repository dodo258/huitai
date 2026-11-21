//! 列表分组：被中断 / 置顶 / 按项目 / 折叠组（agentq 工作树、零散对话）/ 归档 / 整组归档的项目，
//! 再按收起状态摊成一行行。项目组用会话台里起的显示名（[`project_label`]）。
//! 不显示后台对话时，自动运行的对话不进组里的条目，记在组的 `background` 里（组标题写「另有 N 条后台」）；
//! 只剩自动运行对话的组整组不显示。置顶的和被中断的总是显示。
//! 最近一条对话超过 [`STALE_AFTER`] 的项目排进「很久没聊」分区：分区标题默认收起，收起时下面这些项目组都不显示。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::core::SessionId;
use crate::index::{Entry, ProjectNote};
use crate::sources::{FoldGroup, fold_group};

use super::text::{project_label, tilde};

/// 组的身份，用来记收起状态。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GroupKey {
    /// 上次守护进程没收尾（断电、重启、崩溃）时还在跑的对话，排在最上面。
    Interrupted,
    Pinned,
    Project(PathBuf),
    Fold(FoldGroup),
    /// 「很久没聊」分区标题：自己没有对话，收起时把后面 `stale` 的项目组一起藏起来。
    Stale,
    Archived,
}

/// 项目最近一条对话超过这么久，就排进「很久没聊」分区。
pub const STALE_AFTER: Duration = Duration::from_secs(30 * 24 * 3600);

/// 各组的收起状态，三家共用。项目组默认收起，展开着的记在 `expanded`（运行层存进 index 标注，
/// 下次打开保持）；其他组只在这次运行里记手动改过的，折叠组和归档默认收起。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Folds {
    pub expanded: HashSet<PathBuf>,
    overrides: HashMap<GroupKey, bool>,
}

impl Folds {
    pub fn is_collapsed(&self, key: &GroupKey) -> bool {
        match key {
            GroupKey::Project(p) => !self.expanded.contains(p),
            k => self.overrides.get(k).copied().unwrap_or(matches!(
                k,
                GroupKey::Fold(_) | GroupKey::Stale | GroupKey::Archived
            )),
        }
    }

    pub fn set(&mut self, key: &GroupKey, collapsed: bool) {
        match key {
            GroupKey::Project(p) if collapsed => _ = self.expanded.remove(p),
            GroupKey::Project(p) => _ = self.expanded.insert(p.clone()),
            k => _ = self.overrides.insert(k.clone(), collapsed),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub key: GroupKey,
    pub label: String,
    /// 同名项目在组名后补的路径（`~` 缩写）。
    pub hint: Option<String>,
    /// 1–9 跳转用的编号（只给项目组）。
    pub number: Option<u8>,
    /// 组里对话在 entries 里的下标，顺序同 entries（置顶在前、新的在前）。
    pub items: Vec<usize>,
    /// 没显示出来的自动运行对话（不显示后台对话时），顺序同 entries。
    pub background: Vec<usize>,
    /// 排在「很久没聊」分区下面的项目组。
    pub stale: bool,
}

impl Group {
    /// 组里的对话来自不同项目（置顶、折叠组、归档），行里要补项目名。
    pub fn mixed(&self) -> bool {
        !matches!(self.key, GroupKey::Project(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Header(usize),
    Item { group: usize, entry: usize },
}

/// 一个组里显示的和藏起来的对话。
#[derive(Default)]
struct Bucket {
    items: Vec<usize>,
    background: Vec<usize>,
}

impl Bucket {
    fn push(&mut self, i: usize, hidden: bool) {
        if hidden {
            self.background.push(i);
        } else {
            self.items.push(i);
        }
    }
}

impl From<Vec<usize>> for Bucket {
    fn from(items: Vec<usize>) -> Self {
        Self {
            items,
            background: Vec::new(),
        }
    }
}

/// 把一家的对话分组。entries 已按「置顶在前、新的在前」排好（index 的 list 顺序）。
/// `interrupted` 里的对话只出现在最上面的「被中断」组；`show_background` 为假时自动运行的对话藏进组的 `background`。
/// `projects` 里标了整组归档的项目排在「归档」之后，组标题提示「已归档」、不编号。
pub fn groups(
    entries: &[Entry],
    home: &Path,
    interrupted: &HashSet<SessionId>,
    show_background: bool,
    projects: &HashMap<PathBuf, ProjectNote>,
    now: SystemTime,
) -> Vec<Group> {
    let mut stopped = Vec::new();
    let mut pinned = Vec::new();
    let mut archived = Bucket::default();
    let mut folds: HashMap<FoldGroup, Bucket> = HashMap::new();
    let mut buckets: HashMap<&Path, Bucket> = HashMap::new();
    // 项目组按组里最新一条显示出来的对话排（entries 新的在前，所以先有显示条目的组更新）。
    let mut order: Vec<&Path> = Vec::new();
    let mut archived_order: Vec<&Path> = Vec::new();

    for (i, e) in entries.iter().enumerate() {
        let cwd = e.session.cwd.as_path();
        let hidden = e.session.automatic && !show_background && !e.note.pinned;
        if interrupted.contains(&e.session.id) {
            stopped.push(i);
        } else if e.note.archived {
            archived.push(i, hidden);
        } else if e.note.pinned {
            pinned.push(i);
        } else if let Some(f) = fold_group(cwd, home) {
            folds.entry(f).or_default().push(i, hidden);
        } else {
            let b = buckets.entry(cwd).or_default();
            if !hidden && b.items.is_empty() {
                if projects.get(cwd).is_some_and(|n| n.archived) {
                    archived_order.push(cwd);
                } else {
                    order.push(cwd);
                }
            }
            b.push(i, hidden);
        }
    }

    // 最近一条（显示出来的）超过 STALE_AFTER 的项目放到后面的分区里，不编号。
    let newest = |cwd: &Path| {
        buckets[cwd]
            .items
            .iter()
            .map(|&i| entries[i].session.last_active)
            .max()
    };
    let (recent, stale): (Vec<&Path>, Vec<&Path>) = order.into_iter().partition(|cwd| {
        newest(cwd).is_none_or(|t| now.duration_since(t).unwrap_or_default() < STALE_AFTER)
    });
    let mut name_count: HashMap<String, usize> = HashMap::new();
    for cwd in recent.iter().chain(&stale).chain(&archived_order) {
        *name_count.entry(project_label(cwd, projects)).or_default() += 1;
    }
    let mut project_group = |cwd: &Path, number: Option<u8>, archived: bool| {
        let bucket = buckets.remove(cwd).unwrap_or_default();
        let label = project_label(cwd, projects);
        let path = (name_count[&label] > 1).then(|| tilde(cwd, home));
        let hint = if archived {
            Some(path.map_or_else(|| "已归档".to_owned(), |p| format!("已归档 · {p}")))
        } else {
            path
        };
        Group {
            number,
            hint,
            ..group(GroupKey::Project(cwd.to_path_buf()), &label, bucket)
        }
    };

    let mut out = Vec::new();
    if !stopped.is_empty() {
        out.push(Group {
            hint: Some("R 全部接着做".to_owned()),
            ..group(GroupKey::Interrupted, "被中断", stopped.into())
        });
    }
    if !pinned.is_empty() {
        out.push(group(GroupKey::Pinned, "置顶", pinned.into()));
    }
    for (n, cwd) in recent.into_iter().enumerate() {
        let number = u8::try_from(n + 1).ok().filter(|n| *n <= 9);
        out.push(project_group(cwd, number, false));
    }
    let stale_groups: Vec<Group> = stale
        .into_iter()
        .map(|cwd| Group {
            stale: true,
            ..project_group(cwd, None, false)
        })
        .collect();
    for (f, label) in [
        (FoldGroup::Agentq, "agentq 工作树"),
        (FoldGroup::Loose, "零散对话"),
    ] {
        // 只有后台对话的 agentq 工作树、零散对话也留组标题（「另有 N 条后台」），底栏的后台计数才找得到出处。
        if let Some(b) = folds
            .remove(&f)
            .filter(|b| !b.items.is_empty() || !b.background.is_empty())
        {
            out.push(group(GroupKey::Fold(f), label, b));
        }
    }
    if !stale_groups.is_empty() {
        out.push(Group {
            hint: Some(format!("{} 个项目 · 一个月以上", stale_groups.len())),
            ..group(GroupKey::Stale, "很久没聊", Bucket::default())
        });
        out.extend(stale_groups);
    }
    if !archived.items.is_empty() {
        out.push(group(GroupKey::Archived, "归档", archived));
    }
    for cwd in archived_order {
        out.push(project_group(cwd, None, true));
    }
    out
}

fn group(key: GroupKey, label: &str, b: Bucket) -> Group {
    Group {
        key,
        label: label.to_owned(),
        hint: None,
        number: None,
        items: b.items,
        background: b.background,
        stale: false,
    }
}

/// 按收起状态摊平。
pub fn rows(groups: &[Group], folds: &Folds) -> Vec<Row> {
    let mut out = Vec::new();
    let stale_hidden = folds.is_collapsed(&GroupKey::Stale);
    for (gi, g) in groups.iter().enumerate() {
        if g.stale && stale_hidden {
            continue;
        }
        out.push(Row::Header(gi));
        if !folds.is_collapsed(&g.key) {
            out.extend(g.items.iter().map(|&entry| Row::Item { group: gi, entry }));
        }
    }
    out
}
