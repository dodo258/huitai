//! 界面状态机：键鼠 → 改状态，需要动外部（开标签页、改标注、搜索、读预览……）的事以 [`Effect`] 交给运行层。
//! 这里不做任何 IO，测试可以直接喂数据、按键、渲染。

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use crate::core::{Account, Config, QuotaReading, Session, SessionId, SessionState, Vendor};
use crate::host::ScreenSnapshot;
use crate::index::{Edit, Entry, Hit, ProjectEdit, ProjectNote, Snippet};
use crate::quota::{Balance, BalanceReport, BalanceRow};
use crate::sources::{Moment, PreviewMessage, Transcript};

// 认领在 `claims` 里；从这里也引得到（原先就在这个文件）。
pub use super::claims::{Bound, CLAIM_WINDOW, CLAIM_WITNESSES, Claims};
use super::keys::{self, Action, Selected, action_for, typed_text};
use super::newdir::{self, DirPicker};
use super::reader::{Outcome, Reader};
use super::rows::{self, Folds, Group, GroupKey, Row};
use super::text::{project_label, tilde};

mod board;
mod input;
mod menu;
mod mouse;
mod nav;
mod search;
mod settings;

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
/// 顶栏「◆N 等你」慢闪的半拍：亮 0.8 秒、暗 0.8 秒，1.6 秒一次。只在有没看过的新结果（✓ 刚做完）时闪。
pub const BLINK_HALF: Duration = Duration::from_millis(800);
/// 在会话台里选中一条「刚做完」的对话、看它的预览满这么久，就算看过了（方向键路过不算）。
pub const SEEN_AFTER: Duration = Duration::from_secs(2);
/// 在跑的转圈：半圆转一圈四帧，一帧 120 毫秒（和运行层的轮询间隔一样）。
/// 和其他状态灯一样是圆点，静止时也看得出是个点（盲文转圈太细，停住像冒号）。
pub const SPIN_STEP: Duration = Duration::from_millis(120);
/// 一条对话刚停下来（跑完或要你回复）时整行亮一下，这么久淡完。
pub const FLASH: Duration = Duration::from_millis(2400);
pub const SPINNER: [char; 4] = ['◐', '◓', '◑', '◒'];
/// 没有本机项目目录的对话回车时的提示，预览里目录那行也写它。
pub const VIEW_ONLY: &str = "只能查看：这条对话的工作区不在本机，接不上 · v 看整段";
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
    /// 搜索；`vendor` 为 `None` 是各家一起搜（搜索里按 ⌃A 切换）。
    Search {
        vendor: Option<Vendor>,
        query: String,
    },
    SaveConfig(Config),
    /// 把这条对话的恢复命令（`cd <目录> && <恢复命令>`）复制到剪贴板。
    CopyResume(Box<Entry>),
    /// 把这条对话存成 Markdown（`~/Downloads/<标题>-<日期>.md`，重名加序号）。
    Export(Box<Entry>),
    /// 项目组展开或收起了，记进 index 标注（下次打开保持）。
    SetExpanded {
        project: PathBuf,
        expanded: bool,
    },
    /// `$` 打开了额度面板：把余额（有 5 分钟缓存）和全部账号的额度都读一遍。
    RefreshBalances,
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

/// 列表里对话行下面那一行动态（[`App::doing`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Doing {
    /// 在跑的工具：工具名、参数摘要。
    Tool { name: String, input: String },
    /// 停下时的一句话（厂商的总结或最后一句回复）；在跑但没有在跑的工具时也是它。
    Said(String),
    /// 这一轮失败或被打断：厂商写的原因（可能是空串）。
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    None,
    Help,
    Settings {
        cursor: usize,
    },
    /// `$` 额度面板：全部账号的额度、各 provider 的余额和订阅额度；`scroll` 是往下翻了几行。
    Balance {
        scroll: usize,
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
    /// 各家一起搜（⌃A 切换）；默认只搜当前这家。
    pub all: bool,
    pub hits: Vec<Entry>,
    pub selected: usize,
    pub offset: usize,
    /// 正文里也命中的结果：标题下多一行命中片段（只命中标题的不加）。
    pub in_body: HashSet<SessionId>,
    /// 要过、取到的命中片段，都是 [`Search::snippets_for`] 这个查询词的。
    pub snippets: HashMap<SessionId, SnippetState>,
    pub snippets_for: String,
}

/// 一条结果的命中片段取到哪了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnippetState {
    /// 已经叫读的线程去取了，还没回来：那一行先空着占位，回来时结果不跳。
    Asked,
    Found(Snippet),
    /// 读回的正文里没找到（比如命中在大文件没读到的前段）、或读不了：不占那一行。
    Missing,
}

/// `s` 打开的「等你回复」汇总：三家自己的对话（不算自动运行的）里等你回复的排前、在跑的排后，
/// 各自最近聊过的在前（[`App::board_entries`]）。列表每次现算，状态一变就跟着变；选中的那条按 id 跟着走。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Board {
    /// 上次选中的位置：选中的那条不在了（比如结束了）就停在这个位置上。
    pub selected: usize,
    pub offset: usize,
    id: Option<SessionId>,
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
    /// 这一帧转圈所在的格子：只有转圈换帧时运行层只改这几格（见 `view::draw`）。
    pub spin: Vec<Position>,
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
    pub lists: [VendorList; Vendor::SLOTS],
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
    /// 列表里的真实 id、可能托管在会话台新建时的临时 id 下的对话（运行层认出来的，含认不准的），不算别处在用。
    pub aliased: HashSet<SessionId>,
    /// 认领到的对话：列表里的真实 id → 托管它的临时 id（见 [`Claims`]）。状态、实时预览、已读、x 结束都按临时 id。
    aliases: HashMap<SessionId, SessionId>,
    /// 托管着的对话结束的时间：那之前写的文件是会话台自己托管时写的，不算别处在用。
    ended: HashMap<SessionId, SystemTime>,
    /// 各账号的额度：（厂商，账号短名）→ 读数。
    pub quota: HashMap<(Vendor, String), QuotaReading>,
    /// API 余额和订阅额度（运行层每 5 分钟读一次，`$` 打开时再读）；还没读到是 `None`。
    pub balance: Option<BalanceReport>,
    /// 本机各家的账号（每家主账号在前），列表里标账号名、顶栏按账号显示额度用。
    pub accounts: Vec<Account>,
    pub previews: HashMap<SessionId, Preview>,
    /// 各条对话的「此刻」（在跑的工具、停下的一句话、用时、计划、失败），运行层按 [`App::moment_targets`] 读来。
    pub moments: HashMap<SessionId, Moment>,
    pending_preview: Option<(SessionId, SystemTime)>,
    /// 选中的在跑对话此刻的屏幕（实时预览），由运行层按 [`App::live_target`] 定时取。
    pub live: Option<(SessionId, ScreenSnapshot)>,
    /// `g` 的结果：哪个目录、输出。
    pub git: Option<(PathBuf, Result<String, String>)>,
    pub search: Option<Search>,
    /// 开着的整段对话（只读滚动视图），盖住列表和预览。
    pub reader: Option<Reader>,
    /// 「等你回复」汇总开着（和搜索互斥）。
    pub board: Option<Board>,
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
    launch_focused: [bool; Vendor::SLOTS],
    /// 用户已经按过键或点过：之后数据到了不再抢选中。
    touched: bool,
    /// 后台守护进程比当前程序旧时，它还托管着的对话（顶栏提示「N 条对话结束后自动换新」）。
    pub old_host: Option<HashSet<SessionId>>,
    /// GitHub 上有比当前新的版本（版本号）：顶栏右侧暗色提示「有新版 x.y.z · ai update」。
    pub new_version: Option<String>,
    /// 顶栏「◆N 等你」和「刚做完」那几行慢闪的暗半拍（运行层按时间翻转；没有没看过的、或设置里关了闪烁就一直是假）。
    pub blink_dim: bool,
    /// 转圈走到第几帧（有东西在跑时运行层按时间推进，没有就停在 0）。
    pub spin: usize,
    /// 上一次 [`App::tick_blink`] 要重画只是因为转圈换了一帧（运行层只改转圈那几格，不整帧重画）。
    pub only_spun: bool,
    /// 刚从「在跑」停下来的对话：停下那一刻（会话台开了多久）和停成了什么状态；淡完就拿掉。
    flashes: HashMap<SessionId, (Duration, SessionState)>,
    /// 运行层最近一次推进动画时的时刻（会话台开了多久）。
    clock: Duration,
    /// 选中着的「刚做完」对话（托管 id）和从哪一刻起一直选着它（会话台开了多久），满 [`SEEN_AFTER`] 算看过。
    looking: Option<(SessionId, Duration)>,
    /// 会话台自己的标签页在不在前台（终端报焦点才知道，不报就一直当在）：不在时选着也不算看过。
    pub board_front: bool,
    pub areas: Areas,
    last_click: Option<(Position, Instant)>,
    pub quit: bool,
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
            aliases: HashMap::new(),
            ended: HashMap::new(),
            quota: HashMap::new(),
            balance: None,
            accounts: Vec::new(),
            previews: HashMap::new(),
            moments: HashMap::new(),
            pending_preview: None,
            live: None,
            git: None,
            search: None,
            reader: None,
            board: None,
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
            launch_focused: [false; Vendor::SLOTS],
            touched: false,
            old_host: None,
            new_version: None,
            blink_dim: false,
            spin: 0,
            only_spun: false,
            flashes: HashMap::new(),
            clock: Duration::ZERO,
            looking: None,
            board_front: true,
            areas: Areas::default(),
            last_click: None,
            quit: false,
        }
    }

    pub fn set_accounts(&mut self, accounts: Vec<Account>) {
        self.accounts = accounts;
    }

    /// 列表里这条后面写的账号名：这家只有一个账号时不写（`None`）。
    pub fn account_label(&self, s: &Session) -> Option<&str> {
        let mut mine = self.accounts.iter().filter(|a| a.vendor == s.vendor);
        // 只有一个账号：不写。
        mine.clone().nth(1)?;
        mine.find(|a| a.key == s.account).map(|a| a.name.as_str())
    }

    /// 顶栏额度显示哪个账号的：当前这家选中的那条对话的账号，没选中对话就是主账号。
    /// 这家一个账号都没有（测试里没设）时按默认位置的账号。
    pub fn quota_account(&self) -> Account {
        // 没选中对话（选着项目标题、列表空）是主账号，不按空串去找：空串是默认位置那个账号的短名，
        // 主账号是工作号时会找成默认号（审计 v0.1.4 第 16 条）。
        let key = self.selected_entry().map(|e| e.session.account.as_str());
        let of_vendor = || self.accounts.iter().filter(|a| a.vendor == self.vendor);
        key.and_then(|key| of_vendor().find(|a| a.key == key))
            .or_else(|| of_vendor().next())
            .cloned()
            .unwrap_or_else(|| {
                let dir = self.home.join(self.vendor.info().default_dir);
                Account::new(self.vendor, crate::sources::DEFAULT_NAME, dir, &self.home)
            })
    }

    /// 顶栏要显示的余额：pi / Hermes 账号默认 provider 的读数（见 [`BalanceReport::for_account`]）。
    pub fn balance_shown(&self) -> Option<(&BalanceRow, &Balance)> {
        if !matches!(self.vendor, Vendor::Pi | Vendor::Hermes) {
            return None;
        }
        let a = self.quota_account();
        self.balance.as_ref()?.for_account(a.vendor, &a.key)
    }

    /// 顶栏的额度读数（[`App::quota_account`] 那个账号的）。
    pub fn quota_shown(&self) -> Option<&QuotaReading> {
        let a = self.quota_account();
        self.quota.get(&(a.vendor, a.key))
    }

    pub fn list(&self) -> &VendorList {
        &self.lists[self.vendor.index()]
    }

    fn list_mut(&mut self) -> &mut VendorList {
        &mut self.lists[self.vendor.index()]
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
        &self.lists[v.index()]
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
            Some((v, entries)) => lists[v.index()].regroup(Some(entries), &g),
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
        let i = vendor.index();
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
        // 进程参数里是厂商自己的 id，不带账号前缀。
        self.open_elsewhere.contains(id)
            || self.open_elsewhere.contains(&SessionId::from(id.native()))
            || self.written_recently(e)
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

    /// 托管着的对话在 `at` 结束了（进程退出、守护进程不再列它）。认领到临时 id 下的那条跟着算结束。
    pub fn ended(&mut self, id: SessionId, at: SystemTime) {
        let reals: Vec<SessionId> = self
            .aliases
            .iter()
            .filter(|(_, t)| **t == id)
            .map(|(r, _)| r.clone())
            .collect();
        for r in reals {
            self.ended.insert(r, at);
        }
        self.ended.insert(id, at);
    }

    /// 换上认领到的对话（真实 id → 临时 id），并按临时 id 有没有标签页接着标出真实 id。
    /// 运行层每次托管状态变了都重新给一遍。
    pub fn set_aliases(&mut self, aliases: HashMap<SessionId, SessionId>) {
        for real in self.aliases.keys() {
            self.attached.remove(real);
        }
        for (real, temp) in &aliases {
            if self.attached.contains(temp) {
                self.attached.insert(real.clone());
            }
        }
        self.aliases = aliases;
    }

    /// 这条对话在守护进程里用的 id：认领到临时 id 下的是那个临时 id，其余就是它自己。
    pub fn hosted_id<'a>(&'a self, id: &'a SessionId) -> &'a SessionId {
        self.aliases.get(id).unwrap_or(id)
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
        if self.interrupted_shown
            || self.search.is_some()
            || self.board.is_some()
            || self.prompt.is_some()
        {
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

    pub fn set_preview(&mut self, id: SessionId, preview: Preview) {
        if self.pending_preview.as_ref().is_some_and(|(p, _)| *p == id) {
            self.pending_preview = None;
        }
        self.previews.insert(id, preview);
    }

    // ---- 此刻 ----

    /// 「此刻」要看的对话：托管着没停的（在跑、等你、刚做完），别处在跑的。
    pub fn active(&self, e: &Entry) -> bool {
        self.state_of(&e.session.id) != SessionState::Stopped || self.elsewhere_busy(e)
    }

    /// 要读「此刻」的对话：列表里所有 [`App::active`] 的，加上选中的那条（预览标题和计划块用）。
    pub fn moment_targets(&self) -> Vec<Entry> {
        let mut out: Vec<Entry> = self
            .lists
            .iter()
            .flat_map(|l| &l.entries)
            .filter(|e| self.active(e))
            .cloned()
            .collect();
        if let Some(e) = self.selected_entry()
            && !out.iter().any(|x| x.session.id == e.session.id)
        {
            out.push(e.clone());
        }
        out
    }

    pub fn set_moment(&mut self, id: SessionId, m: Moment) {
        self.moments.insert(id, m);
    }

    /// 失败：停下了（等你回复、刚做完），而且这一轮记着失败或被打断。
    pub fn failed(&self, e: &Entry) -> bool {
        waits(self.state_of(&e.session.id))
            && self
                .moments
                .get(&e.session.id)
                .is_some_and(|m| m.failed.is_some())
    }

    /// 在跑：托管着在跑，或别处在跑。
    pub fn busy(&self, e: &Entry) -> bool {
        self.state_of(&e.session.id) == SessionState::Running || self.elsewhere_busy(e)
    }

    /// 列表里对话行下面那一行压暗的动态（只给 [`App::active`] 的）：在跑写在跑的工具，
    /// 停下写失败原因或那一句话；没有数据就没有这一行。
    pub fn doing(&self, e: &Entry) -> Option<Doing> {
        if !self.active(e) {
            return None;
        }
        let m = self.moments.get(&e.session.id)?;
        if self.busy(e) {
            if let Some(t) = &m.tool {
                return Some(Doing::Tool {
                    name: t.name.clone(),
                    input: t.input.clone(),
                });
            }
        } else if let Some(reason) = m.failed.as_ref().filter(|_| self.failed(e)) {
            return Some(Doing::Failed(reason.clone()));
        }
        m.summary.clone().map(Doing::Said)
    }

    /// 这条对话在列表里占几行：有动态的两行，其余一行。
    pub fn entry_lines(&self, e: &Entry) -> usize {
        1 + usize::from(self.doing(e).is_some())
    }

    /// 列表此刻每一行（汇总、搜索结果、分组列表里的一项）各占几行屏幕。
    pub fn line_heights(&self) -> Vec<usize> {
        if self.board.is_some() {
            return self
                .board_entries()
                .iter()
                .map(|e| self.entry_lines(e))
                .collect();
        }
        if let Some(s) = self.search.as_ref().filter(|_| self.in_hits()) {
            return s
                .hits
                .iter()
                .map(|e| 1 + usize::from(self.snippet_line(&e.session.id).is_some()))
                .collect();
        }
        let l = self.list();
        l.rows
            .iter()
            .enumerate()
            .map(|(i, r)| match *r {
                Row::Item { entry, .. } => self.entry_lines(&l.entries[entry]),
                Row::Header(_) => 1 + usize::from(self.gap_before(i)),
            })
            .collect()
    }

    /// 分组列表第 `i` 行是组标题、上面要空一行：展开的组和前后的组之间空一行，一眼分出一组到哪儿为止。
    /// 收起的组挨着收起的组不空，列表照样紧凑。
    pub fn gap_before(&self, i: usize) -> bool {
        let l = self.list();
        let Some(Row::Header(g)) = l.rows.get(i) else {
            return false;
        };
        i > 0
            && (matches!(l.rows[i - 1], Row::Item { .. })
                || !self.folds.is_collapsed(&l.groups[*g].key))
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

    /// 开着整段对话、它还没有（或有过时的）全文时返回要读的对话，一个时间只要一次。
    /// 对话的最后时间取最新列表里的：对话还在写，读进来的全文跟着更新。
    pub fn take_transcript_request(&mut self) -> Option<Entry> {
        let r = self.reader.as_ref()?;
        let s = &r.entry.session;
        let at = self
            .list_of(s.vendor)
            .entries
            .iter()
            .find(|e| e.session.id == s.id)
            .map_or(s.last_active, |e| e.session.last_active);
        self.reader.as_mut()?.take_request(at)
    }

    /// 读到了 `id` 在 `at` 时的全文；整段对话已经关了或换了一条就丢掉。
    pub fn set_transcript(
        &mut self,
        id: &SessionId,
        at: SystemTime,
        data: Result<Transcript, String>,
    ) {
        if let Some(r) = &mut self.reader
            && r.entry.session.id == *id
        {
            r.set(at, data);
        }
    }

    /// 现在该实时预览哪条：选中的对话有托管进程在跑（不是「已停」），且预览区没被帮助页、设置页、整段对话挡住。
    pub fn live_target(&self) -> Option<SessionId> {
        if self.reader.is_some()
            || matches!(
                self.overlay,
                Overlay::Help | Overlay::Settings { .. } | Overlay::Balance { .. }
            )
        {
            return None;
        }
        let id = &self.selected_entry()?.session.id;
        (self.state_of(id) != SessionState::Stopped).then(|| self.hosted_id(id).clone())
    }

    /// 选中的对话此刻的屏幕；不在跑或还没取到时为 `None`（预览显示最后几条消息）。
    pub fn live_screen(&self) -> Option<&ScreenSnapshot> {
        let (id, snap) = self.live.as_ref()?;
        (self.live_target().as_ref() == Some(id)).then_some(snap)
    }

    pub fn state_of(&self, id: &SessionId) -> SessionState {
        self.states
            .get(self.hosted_id(id))
            .copied()
            .unwrap_or_default()
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

    /// 慢闪走到 `elapsed`（会话台开了多久）这一刻：开着闪烁、有没看过的新结果（自己的对话里有「刚做完」）时
    /// 每 [`BLINK_HALF`] 翻一次暗半拍；都看过了（只剩「等你回复」）就不闪。返回要不要重画。
    /// 同时推进在跑的转圈：有自己在跑的、后台在跑的、别处在跑的就每 [`SPIN_STEP`] 换一帧，没有就停住。
    pub fn tick_blink(&mut self, elapsed: Duration) -> bool {
        let (running, _) = self.global_counts();
        let unseen =
            self.lists.iter().flat_map(|l| &l.entries).any(|e| {
                !e.session.automatic && self.state_of(&e.session.id) == SessionState::Done
            });
        let dim = self.config.blink_waiting
            && (elapsed.as_millis() / BLINK_HALF.as_millis()) % 2 == 1
            && unseen;
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
        self.only_spun = changed && dim == self.blink_dim && !flashing;
        self.blink_dim = dim;
        self.spin = spin;
        changed
    }

    /// 选中的「刚做完」对话已经一直选着看了 [`SEEN_AFTER`]（按 [`App::tick_blink`] 推进的时刻算）：返回它的托管 id，
    /// 运行层据此告诉通知模块和守护进程看过了。每次选中只报一次；选中换了、它不是「刚做完」了、
    /// 会话台的标签页退到后台了就重新计时。
    pub fn take_seen(&mut self) -> Option<SessionId> {
        let id = self
            .selected_entry()
            .filter(|_| self.board_front)
            .map(|e| self.hosted_id(&e.session.id).clone())
            .filter(|id| self.state_of(id) == SessionState::Done);
        let Some(id) = id else {
            self.looking = None;
            return None;
        };
        let since = match &self.looking {
            Some((was, at)) if *was == id => *at,
            _ => {
                self.looking = Some((id, self.clock));
                return None;
            }
        };
        if self.clock.saturating_sub(since) < SEEN_AFTER {
            return None;
        }
        self.looking = None;
        Some(id)
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
        let (at, state) = self.flashes.get(self.hosted_id(id))?;
        let age = self.clock.saturating_sub(*at).as_secs_f32();
        let left = 1.0 - age / FLASH.as_secs_f32();
        (left > 0.0).then_some((left, *state))
    }

    /// 转圈此刻的那一帧。
    pub fn spinner(&self) -> char {
        SPINNER[self.spin % SPINNER.len()]
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
                account: String::new(),
            },
            note: crate::index::Note::default(),
        }
    }

    /// 审计 v0.1.4 第 16 条：工作号是主账号、默认位置也有账号时，没选中对话（列表空、选着项目标题）
    /// 顶栏是主账号（工作号）的额度，不是按空串找到的默认号；选中了哪条就是哪条的账号。
    #[test]
    fn quota_account_without_selection_is_the_primary_account() {
        let home = PathBuf::from("/Users/demo");
        let mut app = App::new(
            home.clone(),
            PathBuf::from("/Users/demo/projects/a"),
            Config::default(),
        );
        let work = Account::new(
            Vendor::Claude,
            "工作号",
            PathBuf::from("/data/claude-work"),
            &home,
        );
        let default = Account::new(Vendor::Claude, "默认", home.join(".claude"), &home);
        assert!(default.is_default() && !work.is_default());
        app.set_accounts(vec![work.clone(), default.clone()]);
        assert!(app.selected_entry().is_none());
        assert_eq!(app.quota_account(), work, "没选中对话：主账号");
        app.set_entries(
            Vendor::Claude,
            vec![entry("a-1", "/Users/demo/projects/a", 1)],
        );
        for _ in 0..5 {
            if app.selected_entry().is_some() {
                break;
            }
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(app.quota_account(), default, "选中默认号的对话：默认号");
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
