//! 其他终端：在当前窗口跑 attach，attach 结束（CLI 退出，或按 ⌃] 脱开、CLI 留在后台）后回到会话台。

use std::io;
use std::process;

use super::{Opened, Opener, TabHandle};
use crate::sources::Command;

pub struct InlineOpener;

impl Opener for InlineOpener {
    /// 前台跑命令，继承当前终端的输入输出，一直等到它结束。不设标题：标题由 host 改写。
    /// 调用方要先退出备用屏和原始模式，返回后再恢复界面。attach 是交互程序，不设超时。
    fn open(&self, cmd: &Command, _title: &str) -> io::Result<Opened> {
        let status = process::Command::new(&cmd.program)
            .args(&cmd.args)
            .current_dir(&cmd.cwd)
            .status()
            .map_err(|e| io::Error::new(e.kind(), format!("{} 启动失败: {e}", cmd.program)))?;
        if !status.success() {
            return Err(io::Error::other(format!("{} 退出码 {status}", cmd.program)));
        }
        Ok(Opened::Inline)
    }

    /// 当前窗口没有可以切过去的标签页。
    fn focus(&self, _handle: &TabHandle) -> io::Result<bool> {
        Ok(false)
    }
}
