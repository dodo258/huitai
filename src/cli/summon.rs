//! 全局呼出键按下后做什么：会话台开着就切到它所在的标签页，否则在首选终端新开一个 `ai`。

use std::io;
use std::path::Path;
use std::time::Duration;

use crate::core::exec;
use crate::host::BoardInfo;
use crate::opener::{self, TabHandle, Terminal};
use crate::sources;

/// 查 Ghostty 终端 id 最多等这么久（第一次连 Ghostty 会慢一些）。
const OSASCRIPT_TIMEOUT: Duration = Duration::from_secs(15);

/// 按 tty 找 Ghostty 里会话台所在的终端，返回它的 id；找不到返回空串。tty 经 argv 传入，不拼进脚本。
const GHOSTTY_FIND_TTY: &str = r#"on run argv
	tell application "Ghostty"
		repeat with t in terminals
			if tty of t is (item 1 of argv) then return id of t
		end repeat
		return ""
	end tell
end run"#;

/// 呼出会话台，返回做了什么（写进守护进程日志）。
pub fn summon(
    board: Option<BoardInfo>,
    preferred: Option<&str>,
    exe: &Path,
    home: &Path,
) -> io::Result<String> {
    if let Some(board) = board {
        match focus_board(&board) {
            Ok(true) => return Ok("切到了会话台所在的标签页".to_owned()),
            Ok(false) => {}
            // 聚焦失败就新开一个，按下热键总要有反应。
            Err(e) => eprintln!("huitai daemon: 切到会话台失败，改为新开: {e}"),
        }
    }
    let term = open_terminal(preferred, Terminal::current()).ok_or_else(|| {
        io::Error::other("没有能开标签页的终端：设置里把首选终端设成 ghostty 或 kaku")
    })?;
    let cmd = sources::Command {
        program: exe.to_string_lossy().into_owned(),
        args: Vec::new(),
        cwd: home.to_path_buf(),
    };
    opener::opener_for(&term, |k| std::env::var(k).ok()).open(&cmd, "会话台")?;
    Ok(format!("在 {term} 新开了会话台"))
}

/// 新开会话台用哪个终端：设置里的首选终端，其次拉起守护进程的那个终端；都开不了标签页就没有。
pub fn open_terminal(preferred: Option<&str>, current: Terminal) -> Option<Terminal> {
    preferred
        .map(Terminal::from_name)
        .into_iter()
        .chain([current])
        .find(Terminal::drivable_from_outside)
}

/// 切到会话台所在的标签页。`Ok(false)`：认不出它在哪（其他终端、缺 tty、窗格已不是它）。
fn focus_board(board: &BoardInfo) -> io::Result<bool> {
    let term = Terminal::from_name(board.term_program.as_deref().unwrap_or_default());
    let handle = match &term {
        Terminal::Ghostty => {
            let Some(tty) = board.tty.as_deref() else {
                return Ok(false);
            };
            let id = osascript(&ghostty_find_args(tty))?;
            if id.trim().is_empty() {
                return Ok(false);
            }
            TabHandle::Ghostty {
                terminal_id: id.trim().to_owned(),
            }
        }
        Terminal::Kaku => match kaku_handle(board) {
            Some(h) => h,
            None => return Ok(false),
        },
        // cmux 不接受守护进程控制它，iTerm2 还没做；认不出就新开（落到首选终端）。
        Terminal::Cmux | Terminal::Iterm2 | Terminal::Other(_) => return Ok(false),
    };
    opener::opener_for(&term, |k| std::env::var(k).ok()).focus(&handle)
}

/// Kaku 窗格：窗格 id、tty、命令行三样都有才认（opener 聚焦前会核对 tty 上跑的确实是这条命令）。
pub fn kaku_handle(board: &BoardInfo) -> Option<TabHandle> {
    Some(TabHandle::Kaku {
        pane_id: board.pane.as_deref()?.trim().parse().ok()?,
        tty: board.tty.clone()?,
        command: board.command.clone(),
    })
}

pub fn ghostty_find_args(tty: &str) -> Vec<String> {
    vec!["-e".to_owned(), GHOSTTY_FIND_TTY.to_owned(), tty.to_owned()]
}

fn osascript(args: &[String]) -> io::Result<String> {
    exec::run("/usr/bin/osascript", args, None, OSASCRIPT_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(term: &str, tty: Option<&str>, pane: Option<&str>) -> BoardInfo {
        BoardInfo {
            term_program: Some(term.to_owned()),
            tty: tty.map(str::to_owned),
            pane: pane.map(str::to_owned),
            command: "ai".to_owned(),
            alerts: true,
        }
    }

    #[test]
    fn kaku_handle_needs_pane_and_tty() {
        assert_eq!(
            kaku_handle(&board("Kaku", Some("/dev/ttys004"), Some("7"))),
            Some(TabHandle::Kaku {
                pane_id: 7,
                tty: "/dev/ttys004".to_owned(),
                command: "ai".to_owned(),
            })
        );
        assert_eq!(kaku_handle(&board("Kaku", None, Some("7"))), None);
        assert_eq!(
            kaku_handle(&board("Kaku", Some("/dev/ttys004"), None)),
            None
        );
        assert_eq!(
            kaku_handle(&board("Kaku", Some("/dev/ttys004"), Some("x"))),
            None
        );
    }

    #[test]
    fn other_terminal_board_is_not_focusable() {
        assert!(!focus_board(&board("Apple_Terminal", Some("/dev/ttys001"), None)).unwrap());
        assert!(!focus_board(&BoardInfo::default()).unwrap());
    }

    #[test]
    fn ghostty_board_without_tty_is_not_focusable() {
        // 缺 tty 时不跑 AppleScript，直接认不出。
        assert!(!focus_board(&board("ghostty", None, None)).unwrap());
    }

    #[test]
    fn ghostty_lookup_passes_tty_as_argument() {
        let tty = "/dev/ttys0\"; quit";
        let args = ghostty_find_args(tty);
        assert_eq!(args[0], "-e");
        assert!(!args[1].contains(tty));
        assert!(args[1].contains("tty of t is (item 1 of argv)"));
        assert_eq!(args[2], tty);
    }

    #[test]
    fn open_terminal_prefers_setting_then_current() {
        assert_eq!(
            open_terminal(Some("kaku"), Terminal::Ghostty),
            Some(Terminal::Kaku)
        );
        assert_eq!(
            open_terminal(None, Terminal::Ghostty),
            Some(Terminal::Ghostty)
        );
        assert_eq!(
            open_terminal(Some("iTerm.app"), Terminal::Kaku),
            Some(Terminal::Kaku)
        );
        assert_eq!(open_terminal(None, Terminal::Other(None)), None);
    }
}
