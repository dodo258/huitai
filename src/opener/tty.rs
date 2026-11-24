//! 某个 tty 上跑着什么：终端的标签页 id 会换主人时（Kaku 重启后窗格号从 0 数、iTerm2 恢复窗口沿用会话 id），
//! 聚焦前靠它核对还是同一条命令。

use std::io;
use std::path::Path;

use super::CLI_TIMEOUT;
use crate::core::exec;
use crate::sources::Command;

/// `ps` 显示的命令行：参数用空格连起来，不加引号。
pub fn ps_command_line(cmd: &Command) -> String {
    std::iter::once(cmd.program.as_str())
        .chain(cmd.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 这个 tty 上有没有一个进程的命令行正好是 `command`。
pub(super) fn tty_runs(tty: &str, command: &str) -> io::Result<bool> {
    let tty = tty.strip_prefix("/dev/").unwrap_or(tty);
    // `-ww` 不截断长命令行。tty 上一个进程都没有时 ps 退出码是 1，当作没跑。
    let out = match exec::run(
        Path::new("/bin/ps"),
        &["-ww", "-o", "args=", "-t", tty],
        None,
        CLI_TIMEOUT,
    ) {
        Ok(out) => out,
        Err(e) if e.kind() == io::ErrorKind::Other => return Ok(false),
        Err(e) => return Err(e),
    };
    Ok(out.lines().any(|l| l.trim() == command))
}
