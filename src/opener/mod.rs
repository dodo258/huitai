//! 识别终端、开新标签页、聚焦已开标签页。对外接口：`trait Opener`、`Tabs`、`opener_for`。
//!
//! 用法：`Terminal::current()` 认出终端，`opener_for` 拿到开法，
//! `Tabs::open_or_focus` 打开一条对话（已开着就聚焦），`Tabs::restore_all` 恢复上次所有标签；
//! [`LoginItem`] 是「开机自动打开会话台」的登录项。

mod cmux;
mod ghostty;
mod inline;
mod kaku;
pub mod login;
mod tabs;
mod terminal;

use std::io;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use cmux::CmuxOpener;
pub use ghostty::GhosttyOpener;
pub use inline::InlineOpener;
pub use kaku::{KakuOpener, Pane, ps_command_line};
pub use login::LoginItem;
pub use tabs::{Outcome, TabRecord, Tabs};
pub use terminal::Terminal;

use crate::core::{SessionId, Vendor};
use crate::sources::Command;

/// 辅助命令（osascript、kaku cli、cmux、ps）的超时。AppleScript 第一次连 Ghostty 会慢一些，给足。
pub const CLI_TIMEOUT: Duration = Duration::from_secs(15);

/// 一个开着的标签页在终端里的位置。存进 `tabs.json`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "terminal", rename_all = "lowercase")]
pub enum TabHandle {
    /// Ghostty 终端（标签页里的那块画面）的 id。
    Ghostty { terminal_id: String },
    /// Kaku 窗格 id，加上开它时的 tty 和命令行，用来确认还是同一个窗格。
    Kaku {
        pane_id: u64,
        tty: String,
        command: String,
    },
    /// cmux 标签页（surface）的 UUID 和它所在工作区的 UUID。
    Cmux {
        surface_id: String,
        workspace_id: String,
    },
}

/// `Opener::open` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// 开在了新标签页。
    Tab(TabHandle),
    /// 在当前窗口跑完了。
    Inline,
}

/// 一种开对话的方式。
pub trait Opener {
    /// 在新标签页（或当前窗口）里、在 `cmd.cwd` 下跑 `cmd`。
    /// `title` 是标签页标题（见 `tab_title`），空串表示不设；终端不支持设标题时忽略。
    fn open(&self, cmd: &Command, title: &str) -> io::Result<Opened>;
    /// 切到这个标签页。`Ok(false)` 表示它已经不在了（或不是这个终端的）。
    fn focus(&self, handle: &TabHandle) -> io::Result<bool>;
}

/// 这个终端用哪种开法。`env` 取环境变量（Kaku、cmux 要找它们的命令行工具）。
pub fn opener_for(terminal: &Terminal, env: impl Fn(&str) -> Option<String>) -> Box<dyn Opener> {
    match terminal {
        Terminal::Ghostty => Box::new(GhosttyOpener),
        Terminal::Kaku => Box::new(KakuOpener::new(env)),
        Terminal::Cmux => Box::new(CmuxOpener::new(env)),
        Terminal::Other(_) => Box::new(InlineOpener),
    }
}

/// 标签页里要跑的命令：`<ai 的路径> attach <id>`，工作目录是项目目录。
pub fn attach_command(ai: &Path, session: &SessionId, cwd: &Path) -> Command {
    Command {
        program: ai.to_string_lossy().into_owned(),
        args: vec!["attach".to_owned(), session.to_string()],
        cwd: cwd.to_path_buf(),
    }
}

/// 标签页标题：「厂商标记 项目名 · 对话标题」；对话还没有标题时只有「厂商标记 项目名」。
pub fn tab_title(vendor: Vendor, project: &str, title: Option<&str>) -> String {
    match title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!("{} {project} · {t}", vendor.mark()),
        None => format!("{} {project}", vendor.mark()),
    }
}
