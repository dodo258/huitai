//! cmux（本机 0.65.2，内嵌 Ghostty）：`cmux new-surface` 在会话台所在的工作区开标签页，`focus-panel` 聚焦。
//!
//! cmux 只接受它自己的子孙进程、或带着它给标签页设的 `CMUX_SOCKET_CAPABILITY` 的进程连它的 socket。
//! 会话台在 cmux 的标签页里跑、调命令行时环境原样传下去，所以这里能用；从外面（登录项、呼出键）不行。
//! 下面的行为按 cmux v0.65.2 源码核对（2026-11-22）：
//! - `--json --id-format uuids` 输出 `{surface_id, pane_id, workspace_id, ...}`；`surface:N` 是序号，不记它。
//! - `--command` 的文本当键盘输入打进标签页的交互 shell（不是 `sh -c`），所以和 Ghostty 一样带 `exec`：
//!   attach 结束 shell 跟着退出、标签页关掉，记着的标签页 id 一定还在跑 attach。
//! - `focus-panel --panel <UUID>` 自己找它所在的工作区并把窗口拿到前面；标签页不在了退出码 1、
//!   标准错误 `not_found: Surface not found`。标签页 UUID 不复用。

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::exec;
use super::{GhosttyOpener, Opened, Opener, TabHandle};
use crate::sources::Command;

/// cmux 装在默认位置时的命令行工具。
const DEFAULT_CLI: &str = "/Applications/cmux.app/Contents/Resources/bin/cmux";

pub struct CmuxOpener {
    cli: PathBuf,
}

/// `new-surface --json --id-format uuids` 输出里用到的字段。
#[derive(Debug, Deserialize)]
struct Created {
    surface_id: String,
    #[serde(default)]
    workspace_id: String,
}

impl CmuxOpener {
    /// cmux 给标签页里的进程设 `CMUX_BUNDLED_CLI_PATH` 指向它的命令行工具；没有就用默认安装位置。
    pub fn new(env: impl Fn(&str) -> Option<String>) -> Self {
        let cli = env("CMUX_BUNDLED_CLI_PATH")
            .filter(|p| Path::new(p).file_name().is_some_and(|n| n == "cmux"))
            .map_or_else(|| PathBuf::from(DEFAULT_CLI), PathBuf::from);
        Self { cli }
    }

    pub fn cli(&self) -> &Path {
        &self.cli
    }

    /// 只读地问一句连不连得上（doctor 用）。
    pub fn ping_args() -> Vec<String> {
        vec!["ping".to_owned()]
    }

    /// 开新标签页的参数：开在调用者（会话台）所在的工作区（cmux 默认取 `CMUX_WORKSPACE_ID`）。
    pub fn open_args(cmd: &Command) -> Vec<String> {
        [
            "--json",
            "--id-format",
            "uuids",
            "new-surface",
            "--type",
            "terminal",
            "--working-directory",
        ]
        .map(str::to_owned)
        .into_iter()
        .chain([
            cmd.cwd.to_string_lossy().into_owned(),
            "--command".to_owned(),
            GhosttyOpener::typed_line(cmd),
            "--focus".to_owned(),
            "true".to_owned(),
        ])
        .collect()
    }

    pub fn focus_args(surface: &str) -> Vec<String> {
        ["focus-panel", "--panel", surface]
            .map(str::to_owned)
            .to_vec()
    }

    /// 从 `new-surface` 的 JSON 输出里取标签页和工作区的 UUID。
    pub fn parse_created(out: &str) -> io::Result<(String, String)> {
        let c: Created = serde_json::from_str(out.trim()).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cmux new-surface 输出看不懂（{e}）: {}", out.trim()),
            )
        })?;
        if c.surface_id.is_empty() {
            return Err(io::Error::other(format!(
                "cmux new-surface 没返回标签页 id: {}",
                out.trim()
            )));
        }
        Ok((c.surface_id, c.workspace_id))
    }

    fn cmux(&self, args: &[String]) -> io::Result<String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        exec::run(&self.cli, &args, exec::TIMEOUT)
    }
}

impl Opener for CmuxOpener {
    /// 标签页标题不设：显示的是托管层改写过的标题（已带厂商标记和项目名）。
    fn open(&self, cmd: &Command, _title: &str) -> io::Result<Opened> {
        let out = self
            .cmux(&Self::open_args(cmd))
            .map_err(|e| io::Error::new(e.kind(), format!("cmux new-surface 失败: {e}")))?;
        let (surface_id, workspace_id) = Self::parse_created(&out)?;
        Ok(Opened::Tab(TabHandle::Cmux {
            surface_id,
            workspace_id,
        }))
    }

    fn focus(&self, handle: &TabHandle) -> io::Result<bool> {
        let TabHandle::Cmux { surface_id, .. } = handle else {
            return Ok(false);
        };
        match self.cmux(&Self::focus_args(surface_id)) {
            Ok(_) => Ok(true),
            Err(e) if e.to_string().contains("not_found") => Ok(false),
            Err(e) => Err(io::Error::new(
                e.kind(),
                format!("cmux 切换标签页失败: {e}"),
            )),
        }
    }
}
