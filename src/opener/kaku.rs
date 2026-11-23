//! Kaku（WezTerm 分支，本机 0.30.0）：`kaku cli spawn` 开标签页，`kaku cli list` + `activate-pane` 聚焦。
//!
//! Kaku 重启后窗格 id 从 0 重新数，单凭 id 会聚焦到别人的窗格；所以记下窗格的 tty 和命令行，
//! 聚焦前核对这个 tty 上确实跑着同一条命令。

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::CLI_TIMEOUT;
use super::{Opened, Opener, TabHandle};
use crate::core::exec;
use crate::sources::Command;

/// Kaku 装在默认位置时的命令行工具。
const DEFAULT_CLI: &str = "/Applications/Kaku.app/Contents/MacOS/kaku";

pub struct KakuOpener {
    cli: PathBuf,
}

/// `kaku cli list --format json` 里用到的字段。
#[derive(Debug, Deserialize)]
pub struct Pane {
    pub pane_id: u64,
    #[serde(default)]
    pub tty_name: Option<String>,
}

impl KakuOpener {
    /// Kaku 给窗格里的进程设 `WEZTERM_EXECUTABLE` 指向它的命令行工具；没有就用默认安装位置。
    pub fn new(env: impl Fn(&str) -> Option<String>) -> Self {
        let cli = env("WEZTERM_EXECUTABLE")
            .filter(|p| Path::new(p).file_name().is_some_and(|n| n == "kaku"))
            .map_or_else(|| PathBuf::from(DEFAULT_CLI), PathBuf::from);
        Self { cli }
    }

    pub fn cli(&self) -> &Path {
        &self.cli
    }

    /// 开新标签页的参数。`--no-auto-start`：Kaku 界面不在时报错，不在后台另起一个看不见的 mux。
    /// spawn 不经 shell，程序和参数原样传。
    pub fn spawn_args(cmd: &Command) -> Vec<String> {
        let mut args = vec![
            "cli".to_owned(),
            "--no-auto-start".to_owned(),
            "spawn".to_owned(),
            "--cwd".to_owned(),
            cmd.cwd.to_string_lossy().into_owned(),
            "--".to_owned(),
            cmd.program.clone(),
        ];
        args.extend(cmd.args.iter().cloned());
        args
    }

    pub fn list_args() -> Vec<String> {
        ["cli", "--no-auto-start", "list", "--format", "json"]
            .map(str::to_owned)
            .to_vec()
    }

    pub fn activate_args(pane_id: u64) -> Vec<String> {
        vec![
            "cli".to_owned(),
            "--no-auto-start".to_owned(),
            "activate-pane".to_owned(),
            "--pane-id".to_owned(),
            pane_id.to_string(),
        ]
    }

    /// 解析 `kaku cli list --format json`。
    pub fn parse_list(json: &str) -> io::Result<Vec<Pane>> {
        serde_json::from_str(json).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("kaku cli list 输出看不懂: {e}"),
            )
        })
    }

    fn kaku(&self, args: &[String]) -> io::Result<String> {
        exec::run(&self.cli, args, None, CLI_TIMEOUT)
    }

    fn panes(&self) -> io::Result<Vec<Pane>> {
        Self::parse_list(&self.kaku(&Self::list_args())?)
    }
}

/// `ps` 显示的命令行：参数用空格连起来，不加引号。
pub fn ps_command_line(cmd: &Command) -> String {
    std::iter::once(cmd.program.as_str())
        .chain(cmd.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 这个 tty 上有没有一个进程的命令行正好是 `command`。
fn tty_runs(tty: &str, command: &str) -> io::Result<bool> {
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

impl Opener for KakuOpener {
    /// 标签页标题暂不设：spawn 出来的标签页显示 host 改写过的标题（已带厂商标记和项目名）。
    fn open(&self, cmd: &Command, _title: &str) -> io::Result<Opened> {
        let out = self
            .kaku(&Self::spawn_args(cmd))
            .map_err(|e| io::Error::new(e.kind(), format!("kaku cli spawn 失败: {e}")))?;
        let pane_id: u64 = out.trim().parse().map_err(|_| {
            io::Error::other(format!("kaku cli spawn 没返回窗格 id: {}", out.trim()))
        })?;
        // 标签页已经开了，查不到 tty 也算打开成功；只是以后认不出它，再打开时会开新的。
        let tty = self
            .panes()
            .ok()
            .and_then(|panes| panes.into_iter().find(|p| p.pane_id == pane_id))
            .and_then(|p| p.tty_name)
            .unwrap_or_default();
        Ok(Opened::Tab(TabHandle::Kaku {
            pane_id,
            tty,
            command: ps_command_line(cmd),
        }))
    }

    fn focus(&self, handle: &TabHandle) -> io::Result<bool> {
        let TabHandle::Kaku {
            pane_id,
            tty,
            command,
        } = handle
        else {
            return Ok(false);
        };
        let same_pane = self
            .panes()?
            .into_iter()
            .any(|p| p.pane_id == *pane_id && p.tty_name.as_deref() == Some(tty.as_str()));
        if !same_pane || tty.is_empty() || !tty_runs(tty, command)? {
            return Ok(false);
        }
        self.kaku(&Self::activate_args(*pane_id))
            .map_err(|e| io::Error::new(e.kind(), format!("kaku cli activate-pane 失败: {e}")))?;
        Ok(true)
    }
}
