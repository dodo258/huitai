//! iTerm2：用它的 AppleScript 字典开标签页、聚焦会话。
//!
//! 用到的词（`is hotkey window`、session 的 `id` / `tty`、`create tab with default profile`、`write`、`select`）
//! 从 3.1.0 起都有；下面的行为按 3.7.9 源码核对（2026-11-24），脚本用 3.7.9 的字典编译过，没在真 iTerm2 里跑过。
//! - 值一律经 `on run argv` 传进脚本，不拼进脚本文本。
//! - 新标签页跑默认配置的 shell，再往里打命令（和 Ghostty、cmux 一样）。没用 `create tab … command`：
//!   它不经 shell，attach 拿不到用户 shell 里的环境变量（第一次接上时 CLI 用的就是它）；命令文本还会被当成
//!   插值字符串求值、按 iTerm2 自己的规则拆词，`$$…$$` 会弹框问值。
//! - AppleScript 不能指定新标签页的工作目录，所以先 `cd`。目录名是当按键打进去的，打了会出事的不打（见 [`typeable`]）。
//! - 会话 id 是 UUID，但退出 iTerm2 再恢复窗口时沿用旧 id、里面换成新 shell（`PTYSession.m` 的
//!   `SESSION_ARRANGEMENT_GUID`），所以聚焦前核对它的 tty 上确实跑着同一条命令。
//! - 热键窗口会自己藏起来，浮动的那种还不在 `windows` 里、之后找不回来：标签页开在最前面的普通窗口，没有就开新窗口。

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use super::tty::{ps_command_line, tty_runs};
use super::{GhosttyOpener, Opened, Opener, TabHandle, osascript};
use crate::sources::Command;

/// 刚开的标签页里 shell 还在启动、没换成 attach：这么久之内不核对命令行，免得连按两次回车开出两个。
const STARTING_SECS: u64 = 10;

/// 开新标签页，返回新会话的 id。`windows` 从前往后排。取第 i 项用 `item i of ws`，不用 `contents of`：
/// `contents` 在 iTerm2 的字典里是会话的画面文字。`write text` 等 shell 有输出（最多 1 秒）再打字，并补上回车。
const OPEN_SCRIPT: &str = r#"on run argv
	set cmdLine to item 1 of argv
	tell application id "com.googlecode.iterm2"
		set target to missing value
		set ws to windows
		repeat with i from 1 to count of ws
			if not (is hotkey window of (item i of ws)) then
				set target to item i of ws
				exit repeat
			end if
		end repeat
		if target is missing value then
			set s to current session of (create window with default profile)
		else
			tell target to set t to (create tab with default profile)
			set s to current session of t
		end if
		set sid to id of s
		activate
		tell s to write text cmdLine
		return sid
	end tell
end run"#;

/// 按会话 id 找它的 tty；不在了返回空串。iTerm2 没开着时不去拉起它。
const FIND_SCRIPT: &str = r#"on run argv
	if application id "com.googlecode.iterm2" is not running then return ""
	tell application id "com.googlecode.iterm2"
		repeat with w in (get windows)
			repeat with t in (get tabs of w)
				repeat with s in (get sessions of t)
					if id of s is (item 1 of argv) then return tty of s
				end repeat
			end repeat
		end repeat
		return ""
	end tell
end run"#;

/// 按会话 id 切到它所在的标签页（和分屏），把窗口拿到前面。已经不在了返回 missing。
const FOCUS_SCRIPT: &str = r#"on run argv
	tell application id "com.googlecode.iterm2"
		repeat with w in (get windows)
			repeat with t in (get tabs of w)
				repeat with s in (get sessions of t)
					if id of s is (item 1 of argv) then
						select w
						select t
						select s
						activate
						return "ok"
					end if
				end repeat
			end repeat
		end repeat
		return "missing"
	end tell
end run"#;

pub struct Iterm2Opener;

impl Iterm2Opener {
    /// 打进新标签页 shell 里的那一行：先进工作目录（进不去也照样接上），再换成 attach。
    /// 开头空格和 `exec` 的用意同 [`GhosttyOpener::typed_line`]。目录不能打就不 `cd`：
    /// CLI 的工作目录由守护进程定，不靠这个标签页在哪。
    pub fn typed_line(cmd: &Command) -> String {
        let attach = GhosttyOpener::typed_line(cmd);
        let dir = cmd.cwd.to_string_lossy();
        if !typeable(&dir) {
            return attach;
        }
        let cd = Command {
            program: "cd".to_owned(),
            args: vec![dir.into_owned()],
            cwd: cmd.cwd.clone(),
        };
        format!(" {};{attach}", cd.shell_line())
    }

    /// 开新标签页时 osascript 的参数。
    pub fn open_args(cmd: &Command) -> Vec<String> {
        script(OPEN_SCRIPT, Self::typed_line(cmd))
    }

    /// 查会话 tty 时 osascript 的参数。
    pub fn find_args(session_id: &str) -> Vec<String> {
        script(FIND_SCRIPT, session_id.to_owned())
    }

    /// 聚焦时 osascript 的参数。
    pub fn focus_args(session_id: &str) -> Vec<String> {
        script(FOCUS_SCRIPT, session_id.to_owned())
    }

    /// 这个会话里还是不是我们开的那条 attach。`tty` 是现在查到的（空串是会话不在了），
    /// `runs` 查某个 tty 上有没有跑着某条命令。
    pub fn still_ours(
        tty: &str,
        command: &str,
        opened_at: u64,
        now: u64,
        runs: impl FnOnce(&str, &str) -> io::Result<bool>,
    ) -> io::Result<bool> {
        if tty.is_empty() {
            return Ok(false);
        }
        let starting = now
            .checked_sub(opened_at)
            .is_some_and(|age| age < STARTING_SECS);
        Ok(starting || runs(tty, command)?)
    }
}

/// 这个目录名能不能加上单引号打进交互 shell。目录名来自会话文件，可能是从别人仓库克隆下来的，
/// 单引号挡不住的三种都不打：控制字符在行编辑器里是按键（^U 清行、回车执行），
/// fish 的单引号里 `\` 还是转义（能跳出引号），tcsh 的单引号里 `!` 还会展开历史。
fn typeable(dir: &str) -> bool {
    !dir.chars()
        .any(|c| c.is_control() || matches!(c, '\\' | '!'))
}

fn script(text: &str, arg: String) -> Vec<String> {
    vec!["-e".to_owned(), text.to_owned(), arg]
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Opener for Iterm2Opener {
    /// 标签页标题不设：显示的是托管层改写过的标题（已带厂商标记和项目名）。
    fn open(&self, cmd: &Command, _title: &str) -> io::Result<Opened> {
        let session_id = osascript("iTerm2", &Self::open_args(cmd))?
            .trim()
            .to_owned();
        if session_id.is_empty() {
            return Err(io::Error::other("iTerm2 开了标签页但没返回会话 id"));
        }
        Ok(Opened::Tab(TabHandle::Iterm2 {
            session_id,
            command: ps_command_line(cmd),
            opened_at: now(),
        }))
    }

    fn focus(&self, handle: &TabHandle) -> io::Result<bool> {
        let TabHandle::Iterm2 {
            session_id,
            command,
            opened_at,
        } = handle
        else {
            return Ok(false);
        };
        let tty = osascript("iTerm2", &Self::find_args(session_id))?;
        if !Self::still_ours(tty.trim(), command, *opened_at, now(), tty_runs)? {
            return Ok(false);
        }
        match osascript("iTerm2", &Self::focus_args(session_id))?.trim() {
            "ok" => Ok(true),
            "missing" => Ok(false),
            other => Err(io::Error::other(format!(
                "iTerm2 聚焦返回了没见过的结果: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::core::SessionId;
    use crate::opener::attach_command;

    fn line(dir: &str) -> String {
        Iterm2Opener::typed_line(&attach_command(
            Path::new("/bin/ai"),
            &SessionId::from("s4"),
            Path::new(dir),
        ))
    }

    /// 审查第 1 轮：目录名当按键打进去，单引号拦不住的会执行目录名里的命令（真 pty 里 zsh、bash、tcsh、fish 复现）。
    #[test]
    fn directory_names_that_break_out_of_quotes_are_not_typed() {
        for dir in [
            "/w/evil\u{15}touch PWNED\r",
            "/w/a\ntouch PWNED\n",
            "/w/a\u{3}b",
            "/w/a\u{1b}b",
            "/w/a\tb",
            "/w/a\u{7f}b",
            "/w/a\u{85}b",
            "/w/a\\';touch PWNED;#",
            "/w/a!!",
        ] {
            assert_eq!(line(dir), " exec /bin/ai attach s4", "{dir:?}");
        }
    }

    #[test]
    fn ordinary_directory_names_are_typed() {
        for (dir, quoted) in [
            ("/w/a", "/w/a"),
            ("/w/a b", "'/w/a b'"),
            ("/w/项目 \"x\" $HOME `id`;#", "'/w/项目 \"x\" $HOME `id`;#'"),
            ("/w/it's", r"'/w/it'\''s'"),
        ] {
            assert_eq!(line(dir), format!(" cd {quoted}; exec /bin/ai attach s4"));
        }
    }
}
