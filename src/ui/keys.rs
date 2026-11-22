//! 快捷键：按键 → 动作，以及帮助页、底栏、右键菜单用的按键说明（唯一来源，改键只改这里）。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 列表界面（普通模式）里的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    NextVendor,
    PrevVendor,
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Collapse,
    Expand,
    /// 显示 / 隐藏自动运行（后台）的对话。
    ToggleBackground,
    Search,
    /// 跳到第 n 个项目（1–9）。
    Jump(u8),
    /// 跳到下一条等你回复的对话（三家轮着找）。
    NextWaiting,
    /// 新标签页打开（已开着就切过去）。
    Open,
    /// 当前窗口打开。
    OpenHere,
    New,
    NewElsewhere,
    RestoreTabs,
    /// 把选中对话的恢复命令复制到剪贴板。
    CopyResume,
    Rename,
    Pin,
    Archive,
    Trash,
    Untrash,
    Kill,
    Editor,
    Finder,
    GitStatus,
    Settings,
    Help,
    Quit,
}

/// 中文输入法打出来的全角符号当半角用：`、`→`/`，`？`→`?`，`，`→`,`，`！`→`!`，`。`→`.`。
pub fn halfwidth(c: char) -> char {
    match c {
        '、' => '/',
        '？' => '?',
        '，' => ',',
        '！' => '!',
        '。' => '.',
        _ => c,
    }
}

/// 输入法提交上来的文字（中文等非 ASCII 的字，不含全角符号）：普通模式下收到就当搜索词。
pub fn typed_text(key: KeyEvent) -> Option<char> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char(c) if !c.is_ascii() && c.is_alphanumeric() => Some(c),
        _ => None,
    }
}

/// 普通模式下这个键对应的动作。
pub fn action_for(key: KeyEvent) -> Option<Action> {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let code = match key.code {
        KeyCode::Char(c) => KeyCode::Char(halfwidth(c)),
        c => c,
    };
    let a = match code {
        KeyCode::Char('c') if ctrl => Action::Quit,
        _ if ctrl => return None,
        KeyCode::Enter if alt => Action::OpenHere,
        KeyCode::Enter => Action::Open,
        KeyCode::Char('w') => Action::OpenHere,
        KeyCode::Tab => Action::NextVendor,
        KeyCode::BackTab => Action::PrevVendor,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Home => Action::Top,
        KeyCode::End => Action::Bottom,
        KeyCode::Left | KeyCode::Char('h') => Action::Collapse,
        KeyCode::Right | KeyCode::Char('l') => Action::Expand,
        KeyCode::Char('/') => Action::Search,
        KeyCode::Char('b') => Action::ToggleBackground,
        KeyCode::Char(c @ '1'..='9') => Action::Jump(c as u8 - b'0'),
        KeyCode::Char('!') => Action::NextWaiting,
        KeyCode::Char('n') => Action::New,
        // m 是新建项目；N 是 v0.1.0 的老键，留着兼容，界面上不再提示。
        KeyCode::Char('m' | 'N') => Action::NewElsewhere,
        KeyCode::Char('R') => Action::RestoreTabs,
        KeyCode::Char('c') => Action::CopyResume,
        KeyCode::Char('e') => Action::Rename,
        KeyCode::Char('p') => Action::Pin,
        KeyCode::Char('a') => Action::Archive,
        KeyCode::Char('d') => Action::Trash,
        KeyCode::Char('u') => Action::Untrash,
        KeyCode::Char('x') => Action::Kill,
        KeyCode::Char('o') => Action::Editor,
        KeyCode::Char('f') => Action::Finder,
        KeyCode::Char('g') => Action::GitStatus,
        KeyCode::Char(',') => Action::Settings,
        KeyCode::Char('?') => Action::Help,
        KeyCode::Char('q') => Action::Quit,
        _ => return None,
    };
    Some(a)
}

/// 一条按键说明。
#[derive(Debug, Clone, Copy)]
pub struct KeyHelp {
    pub keys: &'static str,
    pub label: &'static str,
}

const fn k(keys: &'static str, label: &'static str) -> KeyHelp {
    KeyHelp { keys, label }
}

/// 选中的是什么：底栏提示和右键菜单按它换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selected {
    /// 列表空着。
    Nothing,
    /// 组标题；`project` 为假是「被中断」「置顶」「归档」这类组。
    Group { expanded: bool, project: bool },
    /// 托管着在跑（或有标签页接着）的对话。
    Running,
    /// 停着的对话。
    Stopped,
}

fn toggle(expanded: bool) -> &'static str {
    if expanded { "收起" } else { "进入" }
}

/// 底栏左边的按键提示：随选中项换，末尾始终是 `? 全部按键`。
pub fn bar(sel: Selected) -> Vec<KeyHelp> {
    let mut keys = match sel {
        Selected::Nothing => vec![
            k("/", "搜索"),
            k("n", "新对话"),
            k("m", "新项目"),
            k("Tab", "换 AI"),
        ],
        Selected::Group {
            expanded,
            project: true,
        } => vec![
            k("⏎", toggle(expanded)),
            k("n", "新对话"),
            k("m", "新项目"),
            k("e", "改名"),
        ],
        Selected::Group { expanded, .. } => vec![k("⏎", toggle(expanded))],
        Selected::Running => vec![k("⏎", "回到它"), k("x", "结束")],
        Selected::Stopped => vec![k("⏎", "接着聊"), k("e", "改名"), k("d", "删除")],
    };
    keys.push(k("?", "全部按键"));
    keys
}

/// 右键菜单的一项：点它等于按 `keys`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuItem {
    pub action: Action,
    pub keys: &'static str,
    pub label: &'static str,
}

const fn m(action: Action, keys: &'static str, label: &'static str) -> MenuItem {
    MenuItem {
        action,
        keys,
        label,
    }
}

/// 右键菜单：这一项能做的操作，每项带它的单键。
pub fn menu(sel: Selected) -> Vec<MenuItem> {
    use Action as A;
    match sel {
        Selected::Nothing => Vec::new(),
        Selected::Group {
            expanded,
            project: true,
        } => vec![
            m(A::Open, "⏎", toggle(expanded)),
            m(A::New, "n", "新对话"),
            m(A::NewElsewhere, "m", "新建项目 / 在别的目录开对话"),
            m(A::Rename, "e", "改项目显示名"),
            m(A::Archive, "a", "整组归档 / 取消"),
            m(A::Editor, "o", "用编辑器打开"),
            m(A::Finder, "f", "在 Finder 显示"),
            m(A::GitStatus, "g", "看改了哪些文件"),
        ],
        Selected::Group { expanded, .. } => vec![m(A::Open, "⏎", toggle(expanded))],
        Selected::Running => vec![
            m(A::Open, "⏎", "回到它"),
            m(A::OpenHere, "w", "在当前窗口接上"),
            m(A::Kill, "x", "结束"),
            m(A::CopyResume, "c", "复制恢复命令"),
            m(A::Rename, "e", "改名"),
            m(A::Pin, "p", "置顶 / 取消"),
            m(A::Editor, "o", "用编辑器打开项目"),
            m(A::Finder, "f", "在 Finder 显示"),
        ],
        Selected::Stopped => vec![
            m(A::Open, "⏎", "接着聊"),
            m(A::OpenHere, "w", "在当前窗口打开"),
            m(A::CopyResume, "c", "复制恢复命令"),
            m(A::Rename, "e", "改名"),
            m(A::Pin, "p", "置顶 / 取消"),
            m(A::Archive, "a", "归档 / 取消"),
            m(A::Trash, "d", "删除（进回收站）"),
            m(A::Editor, "o", "用编辑器打开项目"),
            m(A::Finder, "f", "在 Finder 显示"),
        ],
    }
}

/// 帮助页：分组列出全部按键（方案「界面」节）。
pub const HELP: &[(&str, &[KeyHelp])] = &[
    (
        "找",
        &[
            k("Tab", "换 AI（⇧Tab 反向）"),
            k("↑↓ jk", "移动"),
            k("←→ hl", "收起 / 展开"),
            k("b", "显示 / 隐藏后台对话"),
            k("/", "搜标题和正文"),
            k("1-9", "跳到第几个项目"),
            k("!", "下一条等你回复的（跨家）"),
        ],
    ),
    (
        "开",
        &[
            k("⏎", "新标签页打开"),
            k("w  ⌥⏎", "当前窗口打开"),
            k("", "⌥⏎ 要终端把 Option 当 Alt，"),
            k("", "不然用 w"),
            k("n", "在这个项目新对话"),
            k("m", "新建项目 / 换个目录开对话"),
            k("R", "被中断的全部接着做"),
            k("", "否则恢复上次所有标签"),
            k("c", "复制恢复命令"),
        ],
    ),
    (
        "整理",
        &[
            k("e", "改名（项目组上改项目显示名）"),
            k("p", "置顶 / 取消"),
            k("a", "归档 / 取消（项目组上整组）"),
            k("d", "进回收站"),
            k("u", "从回收站恢复"),
            k("x", "结束后台任务 / 移出被中断"),
        ],
    ),
    (
        "项目",
        &[
            k("o", "用编辑器打开"),
            k("f", "在 Finder 显示"),
            k("g", "看改了哪些文件"),
        ],
    ),
    (
        "其他",
        &[
            k(",", "设置"),
            k("q", "退出（后台对话不停）"),
            k("单击", "选中；组标题收起 / 展开"),
            k("", "顶栏 ◆ 跳到等你回复的"),
            k("", "底栏路径 在访达里打开"),
            k("双击", "打开"),
            k("右键", "这一项能做什么"),
            k("滚轮", "移动 / 翻预览"),
        ],
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn maps_single_keys() {
        let none = KeyModifiers::NONE;
        assert_eq!(
            action_for(key(KeyCode::Char('j'), none)),
            Some(Action::Down)
        );
        assert_eq!(
            action_for(key(KeyCode::Char('3'), none)),
            Some(Action::Jump(3))
        );
        assert_eq!(
            action_for(key(KeyCode::Enter, KeyModifiers::ALT)),
            Some(Action::OpenHere)
        );
        assert_eq!(
            action_for(key(KeyCode::Char('m'), KeyModifiers::NONE)),
            Some(Action::NewElsewhere)
        );
        // v0.1.0 的老键照样能用。
        assert_eq!(
            action_for(key(KeyCode::Char('N'), KeyModifiers::SHIFT)),
            Some(Action::NewElsewhere)
        );
        assert_eq!(
            action_for(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
        assert_eq!(
            action_for(key(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            action_for(key(KeyCode::Char('!'), KeyModifiers::SHIFT)),
            Some(Action::NextWaiting)
        );
        assert_eq!(
            action_for(key(KeyCode::Char('c'), none)),
            Some(Action::CopyResume)
        );
        assert_eq!(action_for(key(KeyCode::Char('z'), none)), None);
        assert_eq!(
            action_for(key(KeyCode::Char('w'), none)),
            Some(Action::OpenHere)
        );
    }
}
