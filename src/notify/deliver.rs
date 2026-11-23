//! 把提醒送出去：会话台开着就往它所在的标签页写 OSC 9/777 让终端自己弹（点击会聚焦那个标签页），
//! 否则用系统通知兜底。

use std::io::{self, Write};
use std::time::Duration;

use super::Alert;
use crate::core::exec;

/// 系统通知命令最多等这么久。
const SYSTEM_TIMEOUT: Duration = Duration::from_secs(5);

/// 标题、正文的长度上限（按字符），挡住 CLI 发来的超长文字。
const MAX_CHARS: usize = 200;

/// 终端认的桌面通知序列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscKind {
    /// `OSC 9 ; 正文`（iTerm2 起的写法，只有正文）。
    Osc9,
    /// `OSC 777 ; notify ; 标题 ; 正文`（rxvt 起的写法，带标题）。
    Osc777,
}

/// 按 `TERM_PROGRAM` 判断会话台所在终端认哪种通知序列；不认识的终端返回 `None`，走系统通知。
pub fn osc_kind(term_program: &str) -> Option<OscKind> {
    match term_program.to_ascii_lowercase().as_str() {
        "ghostty" | "wezterm" | "kaku" => Some(OscKind::Osc777),
        "iterm.app" => Some(OscKind::Osc9),
        _ => None,
    }
}

/// 一条要显示的通知文字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub title: String,
    pub body: String,
}

impl Message {
    /// `label` 是这条对话的显示名（如「✻ huitai · 修登录」），做通知标题；
    /// 正文用 CLI 自己的通知文字，只响铃时写「在等你回复」。
    pub fn new(alert: &Alert, label: &str) -> Self {
        let body = match (&alert.title, &alert.body) {
            (_, Some(body)) if !body.trim().is_empty() => body.clone(),
            (Some(title), _) if !title.trim().is_empty() => title.clone(),
            _ => "在等你回复".to_owned(),
        };
        Self {
            title: clean(label),
            body: clean(&body),
        }
    }
}

/// 去掉控制字符（CLI 给的文字不能夹带转义序列写进终端），截断过长的部分。
fn clean(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .take(MAX_CHARS)
        .collect();
    if text.chars().count() > MAX_CHARS {
        out.push('…');
    }
    out.trim().to_owned()
}

/// 通知序列字节。标题里的 `;` 会被当成 OSC 777 的分隔符，换成全角。
pub fn osc_sequence(kind: OscKind, msg: &Message) -> Vec<u8> {
    let text = match kind {
        OscKind::Osc9 => format!("\x1b]9;{}: {}\x07", msg.title, msg.body),
        OscKind::Osc777 => format!(
            "\x1b]777;notify;{};{}\x07",
            msg.title.replace(';', "；"),
            msg.body
        ),
    };
    text.into_bytes()
}

/// 系统通知（会话台没开时兜底）。
pub trait Desktop {
    fn show(&self, msg: &Message) -> io::Result<()>;
}

/// 用 `osascript` 的 `display notification` 发系统通知。文字作为参数传入，不拼进脚本。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemNotifier;

impl Desktop for SystemNotifier {
    fn show(&self, msg: &Message) -> io::Result<()> {
        exec::run(
            "/usr/bin/osascript",
            &[
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e",
                "end run",
                "--",
                msg.title.as_str(),
                msg.body.as_str(),
            ],
            None,
            SYSTEM_TIMEOUT,
        )
        .map(drop)
    }
}

/// 会话台所在的标签页：往它的终端写通知序列。
pub struct Tab<'a> {
    pub out: &'a mut dyn Write,
    pub kind: OscKind,
}

/// 送出一条通知：有会话台标签页就写 OSC，写失败或没有标签页就用系统通知。
pub fn deliver(msg: &Message, tab: Option<Tab<'_>>, desktop: &dyn Desktop) -> io::Result<()> {
    if let Some(tab) = tab {
        let written = tab
            .out
            .write_all(&osc_sequence(tab.kind, msg))
            .and_then(|()| tab.out.flush());
        if written.is_ok() {
            return Ok(());
        }
    }
    desktop.show(msg)
}
