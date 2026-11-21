//! Ghostty：用它的 AppleScript 字典（1.3 起，本机 1.4.0）开标签页、聚焦终端。
//!
//! 值一律经 `on run argv` 传进脚本，不拼进脚本文本，项目路径、标题里有引号也不会出事。

use std::io;
use std::path::Path;

use super::exec;
use super::{Opened, Opener, TabHandle};
use crate::sources::Command;

const OSASCRIPT: &str = "/usr/bin/osascript";

/// 开新标签页，返回新终端的 id（UUID，Ghostty 进程内不复用）。没有窗口时开新窗口。
///
/// 新标签页只带工作目录、跑用户默认的 shell，再往里输入命令行、按回车：surface configuration
/// 里设 `command` 会让 Ghostty 每次弹「Allow Ghostty to execute …」确认框（2026-11-20 实测反馈）。
/// 输入的文本不带换行（带换行会触发 Ghostty 的粘贴保护确认框），回车用 `send key` 单独按。
/// 标题非空时用 `name override` 固定标签页标题。
const OPEN_SCRIPT: &str = r#"on run argv
	set {cmdLine, workDir, tabTitle} to {item 1 of argv, item 2 of argv, item 3 of argv}
	tell application "Ghostty"
		set cfg to new surface configuration
		set initial working directory of cfg to workDir
		if (count of windows) is 0 then
			set t to selected tab of (new window with configuration cfg)
		else
			set t to new tab in front window with configuration cfg
		end if
		if tabTitle is not "" then set name override of t to tabTitle
		set tm to focused terminal of t
		input text cmdLine to tm
		send key "enter" to tm
		activate
		return id of tm
	end tell
end run"#;

/// 按终端 id 聚焦：切到它所在的标签页并把窗口拿到前面。终端已经不在时返回 missing。
const FOCUS_SCRIPT: &str = r#"on run argv
	tell application "Ghostty"
		if not (exists terminal id (item 1 of argv)) then return "missing"
		focus terminal id (item 1 of argv)
		activate
		return "ok"
	end tell
end run"#;

pub struct GhosttyOpener;

impl GhosttyOpener {
    /// 输进新标签页 shell 里的那一行。
    /// - `exec`：attach 结束时 shell 跟着退出、标签页关掉，留下的终端 id 一定还在跑 attach（聚焦靠这一点）。
    /// - 开头空格：开了 `HIST_IGNORE_SPACE`（zsh）或 `HISTCONTROL=ignorespace`（bash）时不进历史。
    /// - 参数按 POSIX 单引号规则加引号（`Command::shell_line`），sh、bash、zsh、fish 都认。
    pub fn typed_line(cmd: &Command) -> String {
        format!(" exec {}", cmd.shell_line())
    }

    /// 开新标签页时 osascript 的参数：脚本、输入的命令行、工作目录、标签页标题（空串表示不设）。
    pub fn open_args(cmd: &Command, title: &str) -> Vec<String> {
        vec![
            "-e".to_owned(),
            OPEN_SCRIPT.to_owned(),
            Self::typed_line(cmd),
            cmd.cwd.to_string_lossy().into_owned(),
            title.to_owned(),
        ]
    }

    /// 聚焦时 osascript 的参数。
    pub fn focus_args(terminal_id: &str) -> Vec<String> {
        vec![
            "-e".to_owned(),
            FOCUS_SCRIPT.to_owned(),
            terminal_id.to_owned(),
        ]
    }
}

fn osascript(args: &[String]) -> io::Result<String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    exec::run(Path::new(OSASCRIPT), &args, exec::TIMEOUT)
        .map_err(|e| io::Error::new(e.kind(), format!("Ghostty AppleScript 失败: {e}")))
}

impl Opener for GhosttyOpener {
    fn open(&self, cmd: &Command, title: &str) -> io::Result<Opened> {
        let id = osascript(&Self::open_args(cmd, title))?.trim().to_owned();
        if id.is_empty() {
            return Err(io::Error::other("Ghostty 开了标签页但没返回终端 id"));
        }
        Ok(Opened::Tab(TabHandle::Ghostty { terminal_id: id }))
    }

    fn focus(&self, handle: &TabHandle) -> io::Result<bool> {
        let TabHandle::Ghostty { terminal_id } = handle else {
            return Ok(false);
        };
        match osascript(&Self::focus_args(terminal_id))?.trim() {
            "ok" => Ok(true),
            "missing" => Ok(false),
            other => Err(io::Error::other(format!(
                "Ghostty 聚焦返回了没见过的结果: {other}"
            ))),
        }
    }
}
