//! 界面状态机：键鼠 → 改状态，需要动外部（开标签页、改标注、搜索、读预览……）的事以 [`Effect`] 交给运行层。
//! 这里不做任何 IO，测试可以直接喂数据、按键、渲染。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use crate::core::{Config, QuotaReading, SessionId, SessionState, Vendor};
use crate::host::ScreenSnapshot;
use crate::index::{Edit, Entry, ProjectEdit, ProjectNote};
use crate::sources::PreviewMessage;

use super::keys::{self, Action, Selected, action_for, typed_text};
use super::newdir::{self, DirPicker};
use super::rows::{self, Folds, Group, GroupKey, Row};
use super::text::{project_label, tilde};

/// 双击判定间隔。
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// 滚轮一格移动几行。
const WHEEL_STEP: usize = 3;
/// 设置页里可以改的项数（首选终端、回收站天数、开机自动打开、闲置收回、等你闪烁、自动切英文）。
const SETTINGS_ITEMS: usize = 7;
/// 首选终端可选值：跟随当前终端、Ghostty、Kaku。
pub const TERMINAL_CHOICES: [Option<&str>; 3] = [None, Some("ghostty"), Some("kaku")];
/// 不归会话台托管的对话，会话文件在这之内写过就算它在别处在用（桌面端、别的终端、后台任务里跑着）：
/// 会话台看不到那边的进程，只能看文件。
pub const ELSEWHERE_ACTIVE: Duration = Duration::from_secs(120);
/// 顶栏「◆N 等你」慢闪的半拍：亮 0.8 秒、暗 0.8 秒，1.6 秒一次。
pub const BLINK_HALF: Duration = Duration::from_millis(800);
/// 在跑的转圈：半圆转一圈四帧，一帧 120 毫秒（和运行层的轮询间隔一样）。
/// 和其他状态灯一样是圆点，静止时也看得出是个点（盲文转圈太细，停住像冒号）。
pub const SPIN_STEP: Duration = Duration::from_millis(120);
/// 一条对话刚停下来（跑完或要你回复）时整行亮一下，这么久淡完。
pub const FLASH: Duration = Duration::from_millis(2400);
pub const SPINNER: [char; 4] = ['◐', '◓', '◑', '◒'];
/// 设置里「闲置收回」可选的分钟数，0 是不收回。
pub const IDLE_CHOICES: [u32; 7] = [0, 10, 15, 30, 60, 120, 240];

/// 交给运行层去做的事。
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// 打开一条对话：`here` 为真在当前窗口，否则新标签页（已开着就切过去）。
    Open {
        entry: Box<Entry>,
        here: bool,
    },
    New {
        vendor: Vendor,
        cwd: PathBuf,
    },
    /// 目录还不存在、用户确认了：建好（`create_dir_all`，不 git init）再在里面新对话。
    CreateDir {
        vendor: Vendor,
        cwd: PathBuf,
    },
    /// N 打开了目录输入：读 `projects_root` 下的子目录交给 [`App::set_root_dirs`] 当候选。
    ScanProjectsRoot(PathBuf),
    RestoreTabs,
    /// 被中断的对话全部在新标签页重开，并带一句「继续上次没做完的事……」。
    ResumeInterrupted(Vec<Entry>),
    /// 把这条对话移出「被中断」（不重开）。
    Dismiss(SessionId),
    /// 开 / 关「开机自动打开会话台」。
    SetAutostart(bool),
    Annotate {
        vendor: Vendor,
        id: SessionId,
        edit: Edit,
    },
    /// 给项目起显示名、整组归档（只存会话台标注，不动文件夹和对话）。
    AnnotateProject {
        project: PathBuf,
        edit: ProjectEdit,
    },
    Trash(Box<Entry>),
    Untrash(Vendor),
    Kill(SessionId),
    Editor(PathBuf),
    Finder(PathBuf),
    GitStatus(PathBuf),
    Search {
        vendor: Vendor,
        query: String,
    },
    SaveConfig(Config),
    /// 把这条对话的恢复命令（`cd <目录> && <恢复命令>`）复制到剪贴板。
    CopyResume(Box<Entry>),
    /// 项目组展开或收起了，记进 index 标注（下次打开保持）。
    SetExpanded {
        project: PathBuf,
        expanded: bool,
    },
}

/// 底栏上的输入或确认。
#[derive(Debug, Clone, PartialEq)]
pub enum Prompt {
    Rename {
        vendor: Vendor,
        id: SessionId,
        text: String,
    },
    NewDir {
        vendor: Vendor,
        picker: DirPicker,
    },
    /// N 输入的目录还不存在：⏎ 新建它。
    ConfirmCreate {
        vendor: Vendor,
        dir: PathBuf,
    },
    /// 给项目起显示名，留空恢复目录名。
    RenameProject {
        project: PathBuf,
        text: String,
    },
    ConfirmKill {
        id: SessionId,
        title: String,
    },
    /// 这条正在别处运行，再开一份会两边同时往同一个会话文件写：⏎ 仍然打开。
    ConfirmOpen {
        entry: Box<Entry>,
        here: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    None,
    Help,
    Settings {
        cursor: usize,
    },
    /// 右键菜单：在 (`x`, `y`) 处弹出，列 `sel` 这一项能做的操作（[`keys::menu`]），`cursor` 是 ↑↓ 选中的那项。
    Menu {
        sel: Selected,
        x: u16,
        y: u16,
        cursor: usize,
    },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Search {
    pub query: String,
    pub hits: Vec<Entry>,
    pub selected: usize,
    pub offset: usize,
}

/// 一家的列表。
#[derive(Debug, Clone, Default)]
pub struct VendorList {
    pub entries: Vec<Entry>,
    pub groups: Vec<Group>,
    pub rows: Vec<Row>,
    pub selected: usize,
    pub offset: usize,
}

impl VendorList {
    /// 滚到一个展开的组中间时，钉在列表第一行的那个组标题（在 `rows` 里的下标）：
    /// 第一行是对话、它的组标题已经滚出去了，就让组标题盖住第一行，一直看得到、点得到。
    pub fn sticky(&self) -> Option<usize> {
        let Row::Item { group, .. } = *self.rows.get(self.offset)? else {
            return None;
        };
        self.rows[..self.offset]
            .iter()
            .rposition(|r| *r == Row::Header(group))
    }
}

/// 分组要看的：家目录、被中断的对话、显不显示后台对话、收起状态、项目标注。
struct Grouping<'a> {
    home: &'a Path,
    interrupted: &'a HashSet<SessionId>,
    show_background: bool,
    folds: &'a Folds,
    projects: &'a HashMap<PathBuf, ProjectNote>,
    now: SystemTime,
}

impl VendorList {
    /// 重新分组（`entries` 是新列表就换上），原来选中的那一行还在就继续选中它；
    /// 没了（比如藏起来了）停在它原来那组的标题上。
    fn regroup(&mut self, entries: Option<Vec<Entry>>, g: &Grouping) {
        let keep = self.selection_keys();
        if let Some(e) = entries {
            self.entries = e;
        }
        self.groups = rows::groups(
            &self.entries,
            g.home,
            g.interrupted,
            g.show_background,
            g.projects,
            g.now,
        );
        self.reflow(keep, g.folds);
    }

    /// 按收起状态重新摊平，`keep` 见 [`VendorList::selection_keys`]。
    fn reflow(&mut self, keep: Option<(RowKey, GroupKey)>, folds: &Folds) {
        self.rows = rows::rows(&self.groups, folds);
        let found = keep.and_then(|(row, group)| {
            let at = |k: &RowKey| self.rows.iter().position(|r| self.key_of(*r) == *k);
            at(&row).or_else(|| at(&RowKey::Header(group)))
        });
        // 第一次有数据（或原来选中的没了）时停在第一条对话上，没有展开的组就停在第一个组标题上。
        self.selected = found
            .or_else(|| self.rows.iter().position(|r| matches!(r, Row::Item { .. })))
            .unwrap_or(0);
        self.clamp();
    }

    /// 选中的那一行和它所在的组。
    fn selection_keys(&self) -> Option<(RowKey, GroupKey)> {
        let row = *self.rows.get(self.selected)?;
        let (Row::Header(g) | Row::Item { group: g, .. }) = row;
        Some((self.key_of(row), self.groups[g].key.clone()))
    }

    /// 这个项目组在第几行（选中它第一条用）。
    fn first_item_of(&self, gi: usize) -> Option<usize> {
        self.rows
            .iter()
            .position(|r| matches!(r, Row::Item { group, .. } if *group == gi))
    }

    fn clamp(&mut self) {
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    fn key_of(&self, row: Row) -> RowKey {
        match row {
            Row::Header(g) => RowKey::Header(self.groups[g].key.clone()),
            Row::Item { entry, .. } => RowKey::Item(self.entries[entry].session.id.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RowKey {
    Header(GroupKey),
    Item(SessionId),
}

/// 一条对话的预览：读到时对话的最后时间（变了要重读）和最后几条消息。
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    pub at: SystemTime,
    pub messages: Result<Vec<PreviewMessage>, String>,
}

/// 渲染时记下的区域，给鼠标定位用。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Areas {
    pub tabs: Vec<(Vendor, Rect)>,
    pub list: Rect,
    pub preview: Rect,
    /// 底栏左侧「后台 M 在跑」，单击切换显示后台对话。
    pub background: Rect,
    /// 顶栏印章后的全局状态「◐N 在跑 ◆N 等你」，单击等同 `!`。
    pub status: Rect,
    /// 顶栏「●N 别处」（桌面端、别的终端里开着的），单击跳到下一条。
    pub elsewhere: Rect,
    /// 底栏右侧「路径：…」，单击在访达里打开（等同 `f`）。
    pub path: Rect,
    /// 右键菜单各项所在的区域（第一项在最上面一行，一项一行）。
    pub menu: Rect,
}

pub struct App {
    pub vendor: Vendor,
    /// 顶栏标签里显示哪几家（按 [`Vendor::ALL`] 的顺序），`Tab` 只在它们之间切换。见 [`App::hide_missing`]。
    pub shown: Vec<Vendor>,
    pub lists: [VendorList; 3],
    pub home: PathBuf,
    /// 会话台从哪个目录启动的（N 的默认目录，列表空时 n 的目录）。
    pub launch_dir: PathBuf,
    /// 渲染相对时间用的「现在」。
    pub now: SystemTime,
    pub states: HashMap<SessionId, SessionState>,
    /// 正有标签页接着的对话。
    pub attached: HashSet<SessionId>,
    /// 最近可能在写的对话各自会话文件的 mtime（运行层用 `index::recent_writes` 查来）。
    pub written: HashMap<SessionId, SystemTime>,
    /// 本机别的进程（桌面端、别的终端、后台任务）参数里带着的会话 id：开着它的 CLI 还活着。
    pub open_elsewhere: HashSet<SessionId>,
    /// 列表里的真实 id、其实托管在会话台新建时的临时 id 下的 Codex 对话（运行层认出来的），不算别处在用。
    pub aliased: HashSet<SessionId>,
    /// 托管着的对话结束的时间：那之前写的文件是会话台自己托管时写的，不算别处在用。
    ended: HashMap<SessionId, SystemTime>,
    pub quota: HashMap<Vendor, QuotaReading>,
    pub previews: HashMap<SessionId, Preview>,
    pending_preview: Option<(SessionId, SystemTime)>,
    /// 选中的在跑对话此刻的屏幕（实时预览），由运行层按 [`App::live_target`] 定时取。
    pub live: Option<(SessionId, ScreenSnapshot)>,
    /// `g` 的结果：哪个目录、输出。
    pub git: Option<(PathBuf, Result<String, String>)>,
    pub search: Option<Search>,
    pub prompt: Option<Prompt>,
    pub overlay: Overlay,
    pub status: Option<String>,
    pub config: Config,
    /// 预览区从底部往上翻了几行。
    pub preview_scroll: usize,
    /// 被中断的对话（会话 id），列在各家最上面的「被中断」组。
    pub interrupted: HashSet<SessionId>,
    /// 第一次有被中断的对话时已经跳过去了（之后不再抢选中）。
    interrupted_shown: bool,
    /// 「开机自动打开会话台」开着没有（登录项文件在不在）。
    pub autostart: bool,
    /// 各组的收起状态（三家共用）。
    pub folds: Folds,
    /// 项目的显示名和整组归档（index 标注里存的，三家共用）。
    pub projects: HashMap<PathBuf, ProjectNote>,
    /// 各家是不是已经把启动目录所在的项目展开、选中过（一家只做一次）。
    launch_focused: [bool; 3],
    /// 用户已经按过键或点过：之后数据到了不再抢选中。
    touched: bool,
    /// 后台守护进程比当前程序旧时，它还托管着的对话（顶栏提示「N 条对话结束后自动换新」）。
    pub old_host: Option<HashSet<SessionId>>,
    /// GitHub 上有比当前新的版本（版本号）：顶栏右侧暗色提示「有新版 x.y.z · ai update」。
    pub new_version: Option<String>,
    /// 顶栏「◆N 等你」慢闪的暗半拍（运行层按时间翻转；设置里关了闪烁就一直是假）。
    pub blink_dim: bool,
    /// 转圈走到第几帧（有东西在跑时运行层按时间推进，没有就停在 0）。
    pub spin: usize,
    /// 刚从「在跑」停下来的对话：停下那一刻（会话台开了多久）和停成了什么状态；淡完就拿掉。
    flashes: HashMap<SessionId, (Duration, SessionState)>,
    /// 运行层最近一次推进动画时的时刻（会话台开了多久）。
    clock: Duration,
    pub areas: Areas,
    last_click: Option<(Position, Instant)>,
    pub quit: bool,
}

fn slot(v: Vendor) -> usize {
    match v {
        Vendor::Claude => 0,
        Vendor::Codex => 1,
        Vendor::Pi => 2,
    }
}

impl App {
    pub fn new(home: PathBuf, launch_dir: PathBuf, config: Config) -> Self {
        Self {
            vendor: Vendor::Claude,
            shown: Vendor::ALL.to_vec(),
            lists: Default::default(),
            home,
            launch_dir,
            now: SystemTime::now(),
            states: HashMap::new(),
            attached: HashSet::new(),
            written: HashMap::new(),
            open_elsewhere: HashSet::new(),
            aliased: HashSet::new(),
            ended: HashMap::new(),
            quota: HashMap::new(),
            previews: HashMap::new(),
            pending_preview: None,
            live: None,
            git: None,
            search: None,
            prompt: None,
            overlay: Overlay::None,
            status: None,
            config,
            preview_scroll: 0,
            interrupted: HashSet::new(),
            interrupted_shown: false,
            autostart: false,
            folds: Folds::default(),
            projects: HashMap::new(),
            launch_focused: [false; 3],
            touched: false,
            old_host: None,
            new_version: None,
            blink_dim: false,
            spin: 0,
            flashes: HashMap::new(),
            clock: Duration::ZERO,
            areas: Areas::default(),
            last_click: None,
            quit: false,
        }
    }

    pub fn list(&self) -> &VendorList {
        &self.lists[slot(self.vendor)]
    }

    fn list_mut(&mut self) -> &mut VendorList {
        &mut self.lists[slot(self.vendor)]
    }

    /// 没装的那家不显示标签：`installed` 说这家装没装（数据目录在不在），没装、列表里也没有它的对话就藏起来，
    /// `Tab` 切换跳过它。三家都没装就都显示（空列表里写着怎么新建）。当前这家被藏了就换到第一家显示的。
    pub fn hide_missing(&mut self, installed: impl Fn(Vendor) -> bool) {
        let shown: Vec<Vendor> = Vendor::ALL
            .into_iter()
            .filter(|&v| installed(v) || !self.list_of(v).entries.is_empty())
            .collect();
        self.shown = if shown.is_empty() {
            Vendor::ALL.to_vec()
        } else {
            shown
        };
        if !self.shown.contains(&self.vendor) {
            // 启动时调用，还没有搜索，换家不会产生要执行的动作。
            let _ = self.set_vendor(self.shown[0]);
        }
    }

    pub fn list_of(&self, v: Vendor) -> &VendorList {
        &self.lists[slot(v)]
    }

    // ---- 运行层喂数据 ----

    pub fn set_entries(&mut self, vendor: Vendor, entries: Vec<Entry>) {
        self.regroup(Some((vendor, entries)));
        self.focus_launch_project(vendor);
    }

    /// 换上展开着的项目组（index 标注里存的），在 [`App::set_entries`] 之前调。
    pub fn set_expanded_projects(&mut self, expanded: HashSet<PathBuf>) {
        self.folds.expanded = expanded;
        self.regroup(None);
    }

    /// 换上项目标注（显示名、整组归档），三家都重新分组。
    pub fn set_projects(&mut self, projects: HashMap<PathBuf, ProjectNote>) {
        self.projects = projects;
        self.regroup(None);
    }

    /// 项目在界面上叫什么：起过显示名用显示名，否则目录名。
    pub fn project_label(&self, dir: &Path) -> String {
        project_label(dir, &self.projects)
    }

    /// `projects_root` 下的子目录读到了：N 的输入框还开着就补进候选。
    pub fn set_root_dirs(&mut self, dirs: Vec<PathBuf>) {
        if let Some(Prompt::NewDir { picker, .. }) = &mut self.prompt {
            picker.add(dirs);
        }
    }

    /// 新建项目的默认上级目录（配置的 `projects_root`）。
    pub fn projects_root(&self) -> PathBuf {
        newdir::projects_root(&self.config.projects_root, &self.home)
    }

    /// 重新分组：`only` 这一家（换上它的新列表），或三家都分。
    fn regroup(&mut self, only: Option<(Vendor, Vec<Entry>)>) {
        let Self {
            lists,
            home,
            interrupted,
            config,
            folds,
            projects,
            now,
            ..
        } = self;
        let g = Grouping {
            home,
            interrupted,
            show_background: config.show_background,
            folds,
            projects,
            now: *now,
        };
        match only {
            Some((v, entries)) => lists[slot(v)].regroup(Some(entries), &g),
            None => lists.iter_mut().for_each(|l| l.regroup(None, &g)),
        }
    }

    /// 收起状态变了，三家都重新摊平（收起状态三家共用）。
    fn reflow_all(&mut self) {
        for l in &mut self.lists {
            let keep = l.selection_keys();
            l.reflow(keep, &self.folds);
        }
    }

    /// 在某个项目目录（或它下面）敲 `ai`：这家第一次列出这个项目时把它展开、选中第一条。
    /// 这种展开不记进标注（不然在哪打开过的项目都会一直展开着）。用户已经动过当前这家的选中就只展开不抢选中。
    fn focus_launch_project(&mut self, vendor: Vendor) {
        let i = slot(vendor);
        if self.launch_focused[i] {
            return;
        }
        let l = &self.lists[i];
        let Some((gi, dir)) = l
            .groups
            .iter()
            .enumerate()
            .filter_map(|(gi, g)| match &g.key {
                // 根目录 `/` 当项目的不算，否则从哪打开都会展开它。
                GroupKey::Project(p) if p.parent().is_some() && self.launch_dir.starts_with(p) => {
                    Some((gi, p.clone()))
                }
                _ => None,
            })
            .max_by_key(|(_, p)| p.components().count())
        else {
            return;
        };
        self.launch_focused[i] = true;
        // 在很久没聊的项目里打开：把「很久没聊」分区也打开，不然它藏着。
        let stale = l.groups[gi].stale && self.folds.is_collapsed(&GroupKey::Stale);
        if stale {
            self.folds.set(&GroupKey::Stale, false);
        }
        if stale || self.folds.is_collapsed(&GroupKey::Project(dir.clone())) {
            self.folds.expanded.insert(dir);
            self.reflow_all();
        }
        if self.touched && vendor == self.vendor {
            return;
        }
        let l = &mut self.lists[i];
        if let Some(row) = l.first_item_of(gi) {
            l.selected = row;
        }
    }

    /// 显示 / 隐藏自动运行的对话，记进设置。
    fn toggle_background(&mut self) -> Vec<Effect> {
        self.config.show_background = !self.config.show_background;
        self.regroup(None);
        self.status = Some(
            if self.config.show_background {
                "显示后台对话（b 隐藏）"
            } else {
                "已隐藏后台对话（b 显示）"
            }
            .to_owned(),
        );
        vec![Effect::SaveConfig(self.config.clone())]
    }

    /// 自动运行的这条是不是还在跑：被托管着，或在别处在用。
    pub fn background_busy(&self, e: &Entry) -> bool {
        self.state_of(&e.session.id) != SessionState::Stopped || self.elsewhere(e)
    }

    /// 别处在用：会话台没托管它，但本机有进程开着它（[`App::open_elsewhere`]），或会话文件 [`ELSEWHERE_ACTIVE`] 之内还写过。
    /// 最后一条消息的时间也算写过（文件不会比里面的消息旧）；会话台托管它时写的不算。
    pub fn elsewhere(&self, e: &Entry) -> bool {
        let id = &e.session.id;
        if self.state_of(id) != SessionState::Stopped
            || self.attached.contains(id)
            || self.aliased.contains(id)
        {
            return false;
        }
        // 开着它的 CLI 进程还在：在等工具、等你回复时文件可以几分钟不动，照样算在用。
        self.open_elsewhere.contains(id) || self.written_recently(e)
    }

    /// 别处在跑：别处在用，而且会话文件 [`ELSEWHERE_ACTIVE`] 之内还在写（正干着活，界面上转圈）。
    pub fn elsewhere_busy(&self, e: &Entry) -> bool {
        self.elsewhere(e) && self.written_recently(e)
    }

    /// 会话文件 [`ELSEWHERE_ACTIVE`] 之内写过（会话台自己结束它之后写的才算）。
    fn written_recently(&self, e: &Entry) -> bool {
        let id = &e.session.id;
        let written = self
            .written
            .get(id)
            .map_or(e.session.last_active, |w| (*w).max(e.session.last_active));
        if self.ended.get(id).is_some_and(|t| written <= *t) {
            return false;
        }
        self.now
            .duration_since(written)
            .is_ok_and(|d| d < ELSEWHERE_ACTIVE)
    }

    /// 托管着的对话在 `at` 结束了（进程退出、守护进程不再列它）。
    pub fn ended(&mut self, id: SessionId, at: SystemTime) {
        self.ended.insert(id, at);
    }

    /// 当前这家正在跑的自动运行对话数（底栏「后台 M 在跑」）。
    pub fn background_running(&self) -> usize {
        self.list()
            .entries
            .iter()
            .filter(|e| e.session.automatic && self.background_busy(e))
            .count()
    }

    /// 换上被中断的对话。第一次有的时候切到有被中断对话的那家、停在「被中断」组上
    /// （开机自动打开会话台时用户一眼看到，按 R 接着做）。
    pub fn set_interrupted(&mut self, ids: HashSet<SessionId>) {
        if ids == self.interrupted {
            return;
        }
        self.interrupted = ids;
        self.regroup(None);
        if self.interrupted_shown || self.search.is_some() || self.prompt.is_some() {
            return;
        }
        let Some(v) = Vendor::ALL.into_iter().find(|v| {
            self.list_of(*v)
                .groups
                .first()
                .is_some_and(|g| g.key == GroupKey::Interrupted)
        }) else {
            return;
        };
        self.interrupted_shown = true;
        self.set_vendor(v);
        self.select(0);
    }

    /// 列表里能看到的被中断对话（三家都算）。
    fn interrupted_entries(&self) -> Vec<Entry> {
        self.lists
            .iter()
            .flat_map(|l| {
                l.groups
                    .iter()
                    .filter(|g| g.key == GroupKey::Interrupted)
                    .flat_map(|g| g.items.iter().map(|&i| l.entries[i].clone()))
            })
            .collect()
    }

    /// 搜索结果；不是当前这家、或查询词已经变了的旧结果丢掉。
    pub fn set_hits(&mut self, vendor: Vendor, query: &str, hits: Vec<Entry>) {
        if vendor != self.vendor {
            return;
        }
        if let Some(s) = &mut self.search
            && s.query == query
        {
            s.hits = hits;
            s.selected = s.selected.min(s.hits.len().saturating_sub(1));
        }
    }

    pub fn set_preview(&mut self, id: SessionId, preview: Preview) {
        if self.pending_preview.as_ref().is_some_and(|(p, _)| *p == id) {
            self.pending_preview = None;
        }
        self.previews.insert(id, preview);
    }

    /// 选中的对话还没有（或有过时的）预览时返回它，一条只要一次。
    pub fn take_preview_request(&mut self) -> Option<Entry> {
        let e = self.selected_entry()?.clone();
        let at = e.session.last_active;
        let fresh = self.previews.get(&e.session.id).is_some_and(|p| p.at == at);
        let asked = self.pending_preview == Some((e.session.id.clone(), at));
        if fresh || asked {
            return None;
        }
        self.pending_preview = Some((e.session.id.clone(), at));
        Some(e)
    }

    /// 现在该实时预览哪条：选中的对话有托管进程在跑（不是「已停」），且预览区没被帮助页、设置页挡住。
    pub fn live_target(&self) -> Option<SessionId> {
        if matches!(self.overlay, Overlay::Help | Overlay::Settings { .. }) {
            return None;
        }
        let id = &self.selected_entry()?.session.id;
        (self.state_of(id) != SessionState::Stopped).then(|| id.clone())
    }

    /// 选中的对话此刻的屏幕；不在跑或还没取到时为 `None`（预览显示最后几条消息）。
    pub fn live_screen(&self) -> Option<&ScreenSnapshot> {
        let (id, snap) = self.live.as_ref()?;
        (self.live_target().as_ref() == Some(id)).then_some(snap)
    }

    pub fn state_of(&self, id: &SessionId) -> SessionState {
        self.states.get(id).copied().unwrap_or_default()
    }

    /// 三家合计、只算自己的（不是自动运行的）对话：(在跑, 等你回复) 各几条。「刚做完」也算等你回复（还没看过）。
    pub fn global_counts(&self) -> (usize, usize) {
        let (mut running, mut waiting) = (0, 0);
        for e in self.lists.iter().flat_map(|l| &l.entries) {
            if e.session.automatic {
                continue;
            }
            match self.state_of(&e.session.id) {
                SessionState::Running => running += 1,
                s if waits(s) => waiting += 1,
                _ => {}
            }
        }
        (running, waiting)
    }

    /// 三家合计、只算自己的（不是自动运行的）对话：别处在用几条、其中几条正在干活（会话文件还在写）。
    pub fn elsewhere_counts(&self) -> (usize, usize) {
        let mut open = 0;
        let mut busy = 0;
        for e in self.lists.iter().flat_map(|l| &l.entries) {
            if e.session.automatic || !self.elsewhere(e) {
                continue;
            }
            open += 1;
            if self.elsewhere_busy(e) {
                busy += 1;
            }
        }
        (open, busy)
    }

    /// 慢闪走到 `elapsed`（会话台开了多久）这一刻：开着闪烁、有等你回复的对话时每 [`BLINK_HALF`] 翻一次暗半拍。
    /// 返回要不要重画。
    /// 同时推进在跑的转圈：有自己在跑的、后台在跑的、别处在跑的就每 [`SPIN_STEP`] 换一帧，没有就停住。
    pub fn tick_blink(&mut self, elapsed: Duration) -> bool {
        let (running, waiting) = self.global_counts();
        let dim = self.config.blink_waiting
            && (elapsed.as_millis() / BLINK_HALF.as_millis()) % 2 == 1
            && waiting > 0;
        let spinning = running > 0
            || self.elsewhere_counts().1 > 0
            || self.list().entries.iter().any(|e| {
                self.elsewhere_busy(e) || (e.session.automatic && self.background_busy(e))
            });
        let spin = if spinning {
            (elapsed.as_millis() / SPIN_STEP.as_millis()) as usize % SPINNER.len()
        } else {
            0
        };
        // 正在淡的每一拍都要重画；最后一条淡完那一拍也重画一次，把底色擦干净。
        let flashing = !self.flashes.is_empty();
        self.flashes
            .retain(|_, (at, _)| elapsed.saturating_sub(*at) < FLASH);
        self.clock = elapsed;
        let changed = dim != self.blink_dim || spin != self.spin || flashing;
        self.blink_dim = dim;
        self.spin = spin;
        changed
    }

    /// 记下一条托管对话的新状态。从「在跑」停成「刚做完」或「等你回复」的那一刻整行亮一下（见 [`App::flash`]）。
    pub fn set_state(&mut self, id: SessionId, state: SessionState) {
        let was = self.state_of(&id);
        if was == SessionState::Running && waits(state) {
            self.flashes.insert(id.clone(), (self.clock, state));
        }
        self.states.insert(id, state);
    }

    /// 这条对话刚停下来的高亮还剩多少（1 刚停、往 0 淡）和停成的状态；没在亮就是 `None`。
    pub fn flash(&self, id: &SessionId) -> Option<(f32, SessionState)> {
        let (at, state) = self.flashes.get(id)?;
        let age = self.clock.saturating_sub(*at).as_secs_f32();
        let left = 1.0 - age / FLASH.as_secs_f32();
        (left > 0.0).then_some((left, *state))
    }

    /// 转圈此刻的那一帧。
    pub fn spinner(&self) -> char {
        SPINNER[self.spin % SPINNER.len()]
    }

    /// `!`：从选中处往下找下一条等你回复的自己的对话，这家找完接着找下一家（Tab 的顺序），绕一圈回来。
    /// 在别家就换过去，在收起的组里就展开它。
    fn next_waiting(&mut self) -> Vec<Effect> {
        self.next_where(
            |app, e| !e.session.automatic && waits(app.state_of(&e.session.id)),
            "没有等你回复的对话",
        )
    }

    /// 单击顶栏「别处」：跳到下一条别处在用的对话，三家轮着找。
    fn next_elsewhere(&mut self) -> Vec<Effect> {
        self.next_where(
            |app, e| !e.session.automatic && app.elsewhere(e),
            "没有别处在用的对话",
        )
    }

    /// 从选中的位置往后找下一条满足 `hit` 的对话（三家轮着找，到底了从头），切到那家、展开它的组、选中它。
    fn next_where(&mut self, hit: impl Fn(&Self, &Entry) -> bool, none: &str) -> Vec<Effect> {
        // 搜索时单击顶栏状态：先退出搜索回到列表再跳（搜索结果的行号和列表对不上）。
        // `!` 键在搜索框里是输入的字，到不了这里。
        self.search = None;
        let here = slot(self.vendor);
        // 位置按（第几家，第几组，组里第几条）排；组标题排在它第一条前面。
        let current = match self.selected_row() {
            Some(Row::Header(g)) => (0, g, None),
            Some(Row::Item { group, entry }) => {
                let at = self.list().groups[group]
                    .items
                    .iter()
                    .position(|&i| i == entry);
                (0, group, at)
            }
            None => (0, 0, None),
        };
        let mut targets = Vec::new();
        for k in 0..3 {
            let l = &self.lists[(here + k) % 3];
            for (gi, g) in l.groups.iter().enumerate() {
                for (p, &ei) in g.items.iter().enumerate() {
                    let e = &l.entries[ei];
                    if hit(self, e) {
                        targets.push(((k, gi, Some(p)), ei));
                    }
                }
            }
        }
        let Some(&((k, gi, _), ei)) = targets
            .iter()
            .find(|(at, _)| *at > current)
            .or_else(|| targets.first())
        else {
            self.status = Some(none.to_owned());
            return Vec::new();
        };
        let mut effects = self.set_vendor(Vendor::ALL[(here + k) % 3]);
        // 目标在收着的「很久没聊」分区里：先打开分区，不然它的组整个不在列表里，选不中。
        if self.list().groups[gi].stale && self.folds.is_collapsed(&GroupKey::Stale) {
            self.folds.set(&GroupKey::Stale, false);
            self.reflow_all();
        }
        if self.folds.is_collapsed(&self.list().groups[gi].key) {
            effects.extend(self.set_collapsed(gi, false));
        }
        let item = Row::Item {
            group: gi,
            entry: ei,
        };
        if let Some(i) = self.list().rows.iter().position(|r| *r == item) {
            self.select(i);
        }
        effects
    }

    // ---- 选中 ----

    fn in_hits(&self) -> bool {
        self.search.as_ref().is_some_and(|s| !s.query.is_empty())
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        if let Some(s) = self.search.as_ref().filter(|_| self.in_hits()) {
            return s.hits.get(s.selected);
        }
        let l = self.list();
        match l.rows.get(l.selected)? {
            Row::Item { entry, .. } => l.entries.get(*entry),
            Row::Header(_) => None,
        }
    }

    pub fn selected_group(&self) -> Option<&Group> {
        if self.in_hits() {
            return None;
        }
        let l = self.list();
        match l.rows.get(l.selected)? {
            Row::Header(g) | Row::Item { group: g, .. } => l.groups.get(*g),
        }
    }

    /// 选中的是组标题、在跑的对话还是停着的对话（底栏提示和右键菜单按它换）。
    pub fn selected_kind(&self) -> Selected {
        if let Some(e) = self.selected_entry() {
            let id = &e.session.id;
            if self.state_of(id) != SessionState::Stopped || self.attached.contains(id) {
                return Selected::Running;
            }
            return Selected::Stopped;
        }
        match self.selected_row() {
            Some(Row::Header(g)) => {
                let key = &self.list().groups[g].key;
                Selected::Group {
                    expanded: !self.folds.is_collapsed(key),
                    project: matches!(key, GroupKey::Project(_)),
                }
            }
            _ => Selected::Nothing,
        }
    }

    /// 选中的东西所在的项目目录。
    pub fn selected_dir(&self) -> Option<PathBuf> {
        if let Some(e) = self.selected_entry() {
            return Some(e.session.cwd.clone());
        }
        match &self.selected_group()?.key {
            GroupKey::Project(p) => Some(p.clone()),
            _ => None,
        }
    }

    fn list_len(&self) -> usize {
        match &self.search {
            Some(s) if self.in_hits() => s.hits.len(),
            _ => self.list().rows.len(),
        }
    }

    fn select(&mut self, i: usize) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let i = i.min(len - 1);
        let before = self.selected_entry().map(|e| e.session.id.clone());
        if self.in_hits() {
            if let Some(s) = &mut self.search {
                s.selected = i;
            }
        } else {
            self.list_mut().selected = i;
        }
        if self.selected_entry().map(|e| e.session.id.clone()) != before {
            self.preview_scroll = 0;
            self.git = None;
        }
    }

    fn selected_index(&self) -> usize {
        match &self.search {
            Some(s) if self.in_hits() => s.selected,
            _ => self.list().selected,
        }
    }

    fn move_by(&mut self, delta: isize) {
        let i = self.selected_index().saturating_add_signed(delta);
        self.select(i);
    }

    fn page(&self) -> isize {
        self.areas.list.height.max(2) as isize - 1
    }

    fn switch_vendor(&mut self, forward: bool) -> Vec<Effect> {
        let n = self.shown.len().max(1);
        let i = self
            .shown
            .iter()
            .position(|v| *v == self.vendor)
            .unwrap_or(0);
        let next = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        match self.shown.get(next) {
            Some(&v) => self.set_vendor(v),
            None => Vec::new(),
        }
    }

    fn set_vendor(&mut self, v: Vendor) -> Vec<Effect> {
        if v == self.vendor {
            return Vec::new();
        }
        self.vendor = v;
        self.preview_scroll = 0;
        self.git = None;
        match &mut self.search {
            Some(s) if !s.query.is_empty() => {
                s.hits.clear();
                s.selected = 0;
                vec![Effect::Search {
                    vendor: v,
                    query: s.query.clone(),
                }]
            }
            _ => Vec::new(),
        }
    }

    // ---- 键盘 ----

    pub fn key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        self.status = None;
        self.touched = true;
        if self.prompt.is_some() {
            return self.prompt_key(key);
        }
        match self.overlay {
            Overlay::Help => {
                self.overlay = Overlay::None;
                return Vec::new();
            }
            Overlay::Settings { cursor } => return self.settings_key(key, cursor),
            Overlay::Menu { sel, cursor, .. } => {
                if let Some(effects) = self.menu_key(key, sel, cursor) {
                    return effects;
                }
            }
            Overlay::None => {}
        }
        if self.search.is_some() {
            return self.search_key(key);
        }
        if let Some(a) = action_for(key) {
            return self.act(a);
        }
        // 中文输入法没切成英文时打出的字：直接当搜索词。
        if let Some(c) = typed_text(key) {
            self.search = Some(Search::default());
            return self.edit_query(|q| q.push(c));
        }
        Vec::new()
    }

    /// 右键菜单开着时按键：↑↓ 选、⏎ 执行选中项、Esc 关。别的键先关菜单，再照普通模式处理（返回 `None`），
    /// 所以按菜单上标的字母和点那一项一样。
    fn menu_key(&mut self, key: KeyEvent, sel: Selected, cursor: usize) -> Option<Vec<Effect>> {
        let Overlay::Menu { x, y, .. } = self.overlay else {
            return None;
        };
        let n = keys::menu(sel).len();
        let at = |cursor| Overlay::Menu { sel, x, y, cursor };
        match key.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Up | KeyCode::Char('k') => self.overlay = at(cursor.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => {
                self.overlay = at((cursor + 1).min(n.saturating_sub(1)));
            }
            KeyCode::Enter => return Some(self.run_menu_item(sel, cursor)),
            _ => {
                self.overlay = Overlay::None;
                return None;
            }
        }
        Some(Vec::new())
    }

    /// 执行右键菜单第 `i` 项：关菜单，对选中项做这一项的动作。
    fn run_menu_item(&mut self, sel: Selected, i: usize) -> Vec<Effect> {
        self.overlay = Overlay::None;
        match keys::menu(sel).get(i) {
            Some(item) => self.act(item.action),
            None => Vec::new(),
        }
    }

    fn act(&mut self, a: Action) -> Vec<Effect> {
        match a {
            Action::NextVendor => return self.switch_vendor(true),
            Action::PrevVendor => return self.switch_vendor(false),
            Action::Up => self.move_by(-1),
            Action::Down => self.move_by(1),
            Action::PageUp => self.move_by(-self.page()),
            Action::PageDown => self.move_by(self.page()),
            Action::Top => self.select(0),
            Action::Bottom => self.select(usize::MAX),
            Action::Collapse => return self.collapse(),
            Action::Expand => return self.expand(),
            Action::ToggleBackground => return self.toggle_background(),
            Action::Search => self.search = Some(Search::default()),
            Action::Jump(n) => return self.jump(n),
            Action::NextWaiting => return self.next_waiting(),
            Action::CopyResume => match self.selected_entry() {
                Some(e) => return vec![Effect::CopyResume(Box::new(e.clone()))],
                None => self.status = Some("先选一条对话".to_owned()),
            },
            Action::Open => return self.open(false),
            Action::OpenHere => return self.open(true),
            Action::New => {
                let cwd = self
                    .selected_dir()
                    .unwrap_or_else(|| self.launch_dir.clone());
                return vec![Effect::New {
                    vendor: self.vendor,
                    cwd,
                }];
            }
            Action::NewElsewhere => {
                let dirs =
                    newdir::project_dirs(self.lists.iter().flat_map(|l| &l.entries), &self.home);
                self.prompt = Some(Prompt::NewDir {
                    vendor: self.vendor,
                    picker: DirPicker::new(&self.home, dirs),
                });
                return vec![Effect::ScanProjectsRoot(self.projects_root())];
            }
            // 有被中断的对话时 R 先接着做它们；没有才是恢复上次所有标签。
            Action::RestoreTabs => {
                let stopped = self.interrupted_entries();
                if stopped.is_empty() {
                    return vec![Effect::RestoreTabs];
                }
                return vec![Effect::ResumeInterrupted(stopped)];
            }
            Action::Rename if self.selected_entry().is_none() => self.ask_rename_project(),
            Action::Archive if self.selected_entry().is_none() => return self.archive_project(),
            Action::Rename => {
                if let Some(e) = self.selected_entry() {
                    self.prompt = Some(Prompt::Rename {
                        vendor: e.session.vendor,
                        id: e.session.id.clone(),
                        text: e.title().to_owned(),
                    });
                }
            }
            Action::Pin => {
                return self.annotate(|e| Edit::Pin(!e.note.pinned));
            }
            Action::Archive => {
                return self.annotate(|e| Edit::Archive(!e.note.archived));
            }
            Action::Trash => {
                if let Some(e) = self.selected_entry() {
                    return vec![Effect::Trash(Box::new(e.clone()))];
                }
            }
            Action::Untrash => return vec![Effect::Untrash(self.vendor)],
            Action::Kill => return self.ask_kill(),
            Action::Editor => return self.with_dir(Effect::Editor),
            Action::Finder => return self.with_dir(Effect::Finder),
            Action::GitStatus => return self.with_dir(Effect::GitStatus),
            Action::Settings => self.overlay = Overlay::Settings { cursor: 0 },
            Action::Help => self.overlay = Overlay::Help,
            Action::Quit => self.quit = true,
        }
        Vec::new()
    }

    /// 选中的组标题是项目组时，返回它的目录；不是就在底栏说一句。
    fn selected_project_header(&mut self) -> Option<PathBuf> {
        if let Some(Row::Header(g)) = self.selected_row()
            && let GroupKey::Project(p) = &self.list().groups[g].key
        {
            return Some(p.clone());
        }
        if self.selected_row().is_some() {
            self.status = Some("选中一个项目组再按".to_owned());
        }
        None
    }

    /// 项目组标题上按 e：给项目起显示名（输入框里先放现在的名字）。
    fn ask_rename_project(&mut self) {
        if let Some(project) = self.selected_project_header() {
            self.prompt = Some(Prompt::RenameProject {
                text: self.project_label(&project),
                project,
            });
        }
    }

    /// 项目组标题上按 a：整组移进归档，已归档的就取消。对话本身的标注不动。
    fn archive_project(&mut self) -> Vec<Effect> {
        let Some(project) = self.selected_project_header() else {
            return Vec::new();
        };
        let archived = !self.projects.get(&project).is_some_and(|n| n.archived);
        let name = self.project_label(&project);
        self.status = Some(if archived {
            format!("「{name}」整组移进归档（再按 a 取消）")
        } else {
            format!("「{name}」移出归档")
        });
        vec![Effect::AnnotateProject {
            project,
            edit: ProjectEdit::Archive(archived),
        }]
    }

    fn with_dir(&mut self, make: fn(PathBuf) -> Effect) -> Vec<Effect> {
        match self.selected_dir() {
            Some(d) => vec![make(d)],
            None => {
                self.status = Some("先选一条对话或一个项目".to_owned());
                Vec::new()
            }
        }
    }

    fn annotate(&mut self, edit: impl Fn(&Entry) -> Edit) -> Vec<Effect> {
        match self.selected_entry() {
            Some(e) => vec![Effect::Annotate {
                vendor: e.session.vendor,
                id: e.session.id.clone(),
                edit: edit(e),
            }],
            None => Vec::new(),
        }
    }

    /// x：在跑的问一句再结束；被中断的（没在跑）直接移出「被中断」组。
    fn ask_kill(&mut self) -> Vec<Effect> {
        let Some(e) = self.selected_entry() else {
            return Vec::new();
        };
        let id = e.session.id.clone();
        if self.state_of(&id) == SessionState::Stopped && !self.attached.contains(&id) {
            if self.interrupted.contains(&id) {
                return vec![Effect::Dismiss(id)];
            }
            self.status = Some("这条对话没有在后台跑".to_owned());
            return Vec::new();
        }
        self.prompt = Some(Prompt::ConfirmKill {
            title: e.title().to_owned(),
            id,
        });
        Vec::new()
    }

    fn open(&mut self, here: bool) -> Vec<Effect> {
        if let Some(e) = self.selected_entry() {
            let entry = Box::new(e.clone());
            if self.elsewhere(e) {
                self.prompt = Some(Prompt::ConfirmOpen { entry, here });
                return Vec::new();
            }
            return vec![Effect::Open { entry, here }];
        }
        // 组标题上按回车：收起着就进去（展开并选中第一条），展开着就收起。
        let Some(Row::Header(g)) = self.selected_row() else {
            return Vec::new();
        };
        if !self.folds.is_collapsed(&self.list().groups[g].key) {
            return self.set_collapsed(g, true);
        }
        let effects = self.set_collapsed(g, false);
        let l = self.list_mut();
        if let Some(i) = l
            .rows
            .iter()
            .position(|r| matches!(r, Row::Item { group, .. } if *group == g))
        {
            l.selected = i;
        }
        effects
    }

    /// 选中的是组标题就收起/展开它。
    fn toggle_selected_group(&mut self) -> Vec<Effect> {
        match self.selected_row() {
            Some(Row::Header(g)) => {
                let collapsed = self.folds.is_collapsed(&self.list().groups[g].key);
                self.set_collapsed(g, !collapsed)
            }
            _ => Vec::new(),
        }
    }

    /// 列表（不是搜索结果）里选中的那一行。
    fn selected_row(&self) -> Option<Row> {
        if self.in_hits() {
            return None;
        }
        let l = self.list();
        l.rows.get(l.selected).copied()
    }

    /// 收起或展开当前这家的第 `gi` 组并选中它的标题；项目组的展开状态记进标注。
    fn set_collapsed(&mut self, gi: usize, collapsed: bool) -> Vec<Effect> {
        let key = self.list().groups[gi].key.clone();
        if self.folds.is_collapsed(&key) == collapsed {
            return Vec::new();
        }
        self.folds.set(&key, collapsed);
        self.reflow_all();
        let l = self.list_mut();
        if let Some(i) = l.rows.iter().position(|r| *r == Row::Header(gi)) {
            l.selected = i;
        }
        match key {
            GroupKey::Project(project) => vec![Effect::SetExpanded {
                project,
                expanded: !collapsed,
            }],
            _ => Vec::new(),
        }
    }

    fn collapse(&mut self) -> Vec<Effect> {
        match self.selected_row() {
            Some(Row::Header(g) | Row::Item { group: g, .. }) => self.set_collapsed(g, true),
            None => Vec::new(),
        }
    }

    fn expand(&mut self) -> Vec<Effect> {
        let Some(Row::Header(g)) = self.selected_row() else {
            return Vec::new();
        };
        if self.folds.is_collapsed(&self.list().groups[g].key) {
            return self.set_collapsed(g, false);
        }
        self.move_by(1);
        Vec::new()
    }

    fn jump(&mut self, n: u8) -> Vec<Effect> {
        if self.in_hits() {
            return Vec::new();
        }
        let Some(g) = self.list().groups.iter().position(|g| g.number == Some(n)) else {
            return Vec::new();
        };
        let effects = self.set_collapsed(g, false);
        // 选中组里第一条。
        let l = self.list_mut();
        if let Some(i) = l.rows.iter().position(|r| *r == Row::Header(g)) {
            l.selected = i;
        }
        self.move_by(1);
        effects
    }

    fn search_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Esc => self.search = None,
            KeyCode::Enter => {
                return self.open(key.modifiers.contains(KeyModifiers::ALT));
            }
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::PageUp => self.move_by(-self.page()),
            KeyCode::PageDown => self.move_by(self.page()),
            KeyCode::Tab => return self.switch_vendor(true),
            KeyCode::BackTab => return self.switch_vendor(false),
            KeyCode::Backspace => return self.edit_query(|q| _ = q.pop()),
            KeyCode::Char('u') if ctrl => return self.edit_query(String::clear),
            KeyCode::Char(c) if !ctrl => return self.edit_query(|q| q.push(c)),
            _ => {}
        }
        Vec::new()
    }

    fn edit_query(&mut self, edit: impl FnOnce(&mut String)) -> Vec<Effect> {
        let Some(s) = &mut self.search else {
            return Vec::new();
        };
        edit(&mut s.query);
        s.selected = 0;
        s.offset = 0;
        s.hits.clear();
        if s.query.trim().is_empty() {
            return Vec::new();
        }
        vec![Effect::Search {
            vendor: self.vendor,
            query: s.query.clone(),
        }]
    }

    fn prompt_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(prompt) = self.prompt.take() else {
            return Vec::new();
        };
        if let Prompt::ConfirmKill { id, .. } = prompt {
            return match key.code {
                KeyCode::Char('y') => vec![Effect::Kill(id)],
                _ => Vec::new(),
            };
        }
        if let Prompt::ConfirmOpen { entry, here } = prompt {
            return match key.code {
                KeyCode::Enter => vec![Effect::Open { entry, here }],
                _ => Vec::new(),
            };
        }
        if let Prompt::ConfirmCreate { vendor, dir } = prompt {
            return match key.code {
                KeyCode::Enter => vec![Effect::CreateDir { vendor, cwd: dir }],
                _ => Vec::new(),
            };
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let mut prompt = prompt;
        if let Prompt::NewDir { picker, .. } = &mut prompt {
            match key.code {
                KeyCode::Esc => return Vec::new(),
                KeyCode::Enter => return self.submit(prompt),
                KeyCode::Tab => _ = picker.complete(),
                KeyCode::Down => picker.down(),
                KeyCode::Up => picker.up(),
                KeyCode::Backspace => picker.edit(|t| _ = t.pop()),
                KeyCode::Char('u') if ctrl => picker.edit(String::clear),
                KeyCode::Char(c) if !ctrl => picker.edit(|t| t.push(c)),
                _ => {}
            }
            self.prompt = Some(prompt);
            return Vec::new();
        }
        let text = match &mut prompt {
            Prompt::Rename { text, .. } | Prompt::RenameProject { text, .. } => text,
            _ => unreachable!("上面已处理"),
        };
        match key.code {
            KeyCode::Esc => return Vec::new(),
            KeyCode::Enter => return self.submit(prompt),
            KeyCode::Backspace => _ = text.pop(),
            KeyCode::Char('u') if ctrl => text.clear(),
            KeyCode::Char(c) if !ctrl => text.push(c),
            _ => {}
        }
        self.prompt = Some(prompt);
        Vec::new()
    }

    fn submit(&mut self, prompt: Prompt) -> Vec<Effect> {
        match prompt {
            Prompt::Rename { vendor, id, text } => {
                let title = Some(text).filter(|t| !t.trim().is_empty());
                vec![Effect::Annotate {
                    vendor,
                    id,
                    edit: Edit::Rename(title),
                }]
            }
            Prompt::RenameProject { project, text } => vec![Effect::AnnotateProject {
                project,
                edit: ProjectEdit::Rename(Some(text).filter(|t| !t.trim().is_empty())),
            }],
            Prompt::NewDir { vendor, picker } => {
                let Some(cwd) = picker.target(&self.projects_root(), &self.launch_dir) else {
                    // 什么都没输：输入框留着。
                    self.prompt = Some(Prompt::NewDir { vendor, picker });
                    return Vec::new();
                };
                if cwd.is_dir() {
                    return vec![Effect::New { vendor, cwd }];
                }
                if cwd.exists() {
                    self.status = Some(format!("这不是目录：{}", tilde(&cwd, &self.home)));
                    return Vec::new();
                }
                self.prompt = Some(Prompt::ConfirmCreate { vendor, dir: cwd });
                Vec::new()
            }
            Prompt::ConfirmCreate { vendor, dir } => vec![Effect::CreateDir { vendor, cwd: dir }],
            Prompt::ConfirmKill { id, .. } => vec![Effect::Kill(id)],
            Prompt::ConfirmOpen { entry, here } => vec![Effect::Open { entry, here }],
        }
    }

    fn settings_key(&mut self, key: KeyEvent, cursor: usize) -> Vec<Effect> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.overlay = Overlay::Settings {
                    cursor: cursor.saturating_sub(1),
                };
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.overlay = Overlay::Settings {
                    cursor: (cursor + 1).min(SETTINGS_ITEMS - 1),
                };
            }
            KeyCode::Left | KeyCode::Char('h') => return self.change_setting(cursor, false),
            KeyCode::Right | KeyCode::Char('l') => return self.change_setting(cursor, true),
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(',' | 'q') => {
                self.overlay = Overlay::None;
                return vec![Effect::SaveConfig(self.config.clone())];
            }
            _ => {}
        }
        Vec::new()
    }

    fn change_setting(&mut self, cursor: usize, forward: bool) -> Vec<Effect> {
        match cursor {
            0 => {
                let now = self.config.preferred_terminal.as_deref();
                let i = TERMINAL_CHOICES.iter().position(|c| *c == now).unwrap_or(0);
                let n = TERMINAL_CHOICES.len();
                let next = if forward {
                    (i + 1) % n
                } else {
                    (i + n - 1) % n
                };
                self.config.preferred_terminal = TERMINAL_CHOICES[next].map(str::to_owned);
            }
            1 => {
                let d = self.config.trash_days;
                self.config.trash_days = if forward {
                    (d + 1).min(365)
                } else {
                    d.saturating_sub(1).max(1)
                };
            }
            4 => self.config.blink_waiting = !self.config.blink_waiting,
            5 => self.config.auto_ascii_input = !self.config.auto_ascii_input,
            6 => self.config.check_updates = !self.config.check_updates,
            3 => {
                let now = self.config.idle_reclaim_minutes;
                // 手改 config.json 写了列表外的数：从不小于它的那一档接着走。
                let i = IDLE_CHOICES
                    .iter()
                    .position(|m| *m >= now)
                    .unwrap_or(IDLE_CHOICES.len() - 1);
                let n = IDLE_CHOICES.len();
                let next = if forward {
                    (i + 1) % n
                } else {
                    (i + n - 1) % n
                };
                self.config.idle_reclaim_minutes = IDLE_CHOICES[next];
            }
            // 开关：←→ 都是切换。是否成功由运行层写登录项后定，失败会改回来。
            _ => {
                self.autostart = !self.autostart;
                return vec![Effect::SetAutostart(self.autostart)];
            }
        }
        Vec::new()
    }

    // ---- 鼠标 ----

    pub fn mouse(&mut self, ev: MouseEvent, now: Instant) -> Vec<Effect> {
        let pos = Position::new(ev.column, ev.row);
        match ev.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = ev.kind == MouseEventKind::ScrollDown;
                if self.overlay != Overlay::None || self.prompt.is_some() {
                    return Vec::new();
                }
                if self.areas.preview.contains(pos) {
                    self.preview_scroll = if down {
                        self.preview_scroll.saturating_sub(WHEEL_STEP)
                    } else {
                        self.preview_scroll + WHEEL_STEP
                    };
                } else {
                    let step = WHEEL_STEP as isize;
                    self.move_by(if down { step } else { -step });
                }
                Vec::new()
            }
            MouseEventKind::Down(MouseButton::Left) => self.click(pos, now),
            MouseEventKind::Down(MouseButton::Right) => self.right_click(pos),
            _ => Vec::new(),
        }
    }

    /// 列表里第几行在 `pos` 这一格（搜索时是第几条结果）；不在列表里或下面没有行为 `None`。
    fn row_at(&self, pos: Position) -> Option<usize> {
        if !self.areas.list.contains(pos) {
            return None;
        }
        let offset = match &self.search {
            Some(s) if self.in_hits() => s.offset,
            _ => self.list().offset,
        };
        let y = usize::from(pos.y - self.areas.list.y);
        // 第一行被钉住的组标题盖着时，点到的是那个组标题。
        if y == 0
            && !self.in_hits()
            && self.areas.list.height >= 2
            && let Some(h) = self.list().sticky()
        {
            return Some(h);
        }
        let i = offset + y;
        (i < self.list_len()).then_some(i)
    }

    /// 右键：选中这一行，在点的地方弹出它能做的操作。
    fn right_click(&mut self, pos: Position) -> Vec<Effect> {
        self.status = None;
        if self.prompt.is_some() || matches!(self.overlay, Overlay::Help | Overlay::Settings { .. })
        {
            return Vec::new();
        }
        self.overlay = Overlay::None;
        let Some(i) = self.row_at(pos) else {
            return Vec::new();
        };
        self.touched = true;
        self.last_click = None;
        self.select(i);
        let sel = self.selected_kind();
        if !keys::menu(sel).is_empty() {
            self.overlay = Overlay::Menu {
                sel,
                x: pos.x,
                y: pos.y,
                cursor: 0,
            };
        }
        Vec::new()
    }

    fn click(&mut self, pos: Position, now: Instant) -> Vec<Effect> {
        self.status = None;
        if let Overlay::Menu { sel, .. } = self.overlay {
            // 点在某一项上就执行它，点别处只关菜单。
            let m = self.areas.menu;
            if m.contains(pos) {
                return self.run_menu_item(sel, usize::from(pos.y - m.y));
            }
            self.overlay = Overlay::None;
            return Vec::new();
        }
        if self.overlay != Overlay::None {
            if let Overlay::Settings { .. } = self.overlay {
                self.overlay = Overlay::None;
                return vec![Effect::SaveConfig(self.config.clone())];
            }
            self.overlay = Overlay::None;
            return Vec::new();
        }
        if self.prompt.is_some() {
            return Vec::new();
        }
        self.touched = true;
        // 点的是顶栏、底栏这些地方（不是列表行）：上一次单击作废，免得接着点列表里同一屏幕行的另一条被当成双击打开。
        let bars = [
            self.areas.background,
            self.areas.status,
            self.areas.elsewhere,
            self.areas.path,
        ];
        if bars.iter().any(|r| r.contains(pos))
            || self.areas.tabs.iter().any(|(_, r)| r.contains(pos))
        {
            self.last_click = None;
        }
        if self.areas.background.contains(pos) {
            return self.toggle_background();
        }
        if self.areas.status.contains(pos) {
            return self.next_waiting();
        }
        if self.areas.elsewhere.contains(pos) {
            return self.next_elsewhere();
        }
        if self.areas.path.contains(pos) {
            return self.with_dir(Effect::Finder);
        }
        if let Some((v, _)) = self.areas.tabs.iter().find(|(_, r)| r.contains(pos)) {
            let v = *v;
            return self.set_vendor(v);
        }
        let Some(i) = self.row_at(pos) else {
            // 点在列表行以外（顶栏、分隔线、空白处）：上一次单击作废，接着点回列表不算双击。
            self.last_click = None;
            return Vec::new();
        };
        let double = self
            .last_click
            .is_some_and(|(p, t)| p.y == pos.y && now.duration_since(t) <= DOUBLE_CLICK);
        self.select(i);
        // 单击组标题就收起/展开。
        if matches!(self.selected_row(), Some(Row::Header(_))) {
            self.last_click = None;
            return self.toggle_selected_group();
        }
        if double {
            self.last_click = None;
            return self.open(false);
        }
        self.last_click = Some((pos, now));
        Vec::new()
    }
}

/// 等你回复（「刚做完」是还没看过的等你回复）。
fn waits(s: SessionState) -> bool {
    matches!(s, SessionState::Waiting | SessionState::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, cwd: &str, mins_ago: u64) -> Entry {
        Entry {
            session: crate::core::Session {
                vendor: Vendor::Claude,
                id: SessionId::from(id),
                cwd: PathBuf::from(cwd),
                path: PathBuf::from(format!("{cwd}/{id}.jsonl")),
                title: Some(id.to_owned()),
                git_branch: None,
                last_active: SystemTime::UNIX_EPOCH
                    + Duration::from_secs(1_000_000 - mins_ago * 60),
                message_count: 1,
                automatic: false,
            },
            note: crate::index::Note::default(),
        }
    }

    /// 搜索结果里单击顶栏状态：退出搜索，在列表里选中下一条等你回复的（不是按列表行号去选搜索结果）。
    #[test]
    fn clicking_status_while_searching_leaves_search_and_jumps_in_list() {
        let mut app = App::new(
            PathBuf::from("/Users/demo"),
            PathBuf::from("/Users/demo/projects/a"),
            Config::default(),
        );
        let list = vec![
            entry("a-run", "/Users/demo/projects/a", 1),
            entry("a-wait", "/Users/demo/projects/a", 2),
            entry("b-wait", "/Users/demo/projects/b", 3),
            entry("b-old", "/Users/demo/projects/b", 50),
        ];
        app.set_entries(Vendor::Claude, list.clone());
        app.states
            .insert(SessionId::from("a-run"), SessionState::Running);
        app.states
            .insert(SessionId::from("a-wait"), SessionState::Waiting);
        app.states
            .insert(SessionId::from("b-wait"), SessionState::Done);
        app.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let mut hits = list;
        hits.reverse();
        app.set_hits(Vendor::Claude, "x", hits);
        app.areas.status = Rect::new(10, 0, 12, 1);
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 11,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(click, Instant::now());
        assert!(app.search.is_none(), "退出了搜索");
        let id = |app: &App| app.selected_entry().map(|e| e.session.id.to_string());
        assert_eq!(id(&app).as_deref(), Some("a-wait"));
        app.mouse(click, Instant::now() + Duration::from_secs(1));
        assert_eq!(id(&app).as_deref(), Some("b-wait"));
    }
}
