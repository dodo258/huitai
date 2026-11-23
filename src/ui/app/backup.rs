//! Claude 对话被删的提醒和只剩备份的对话：预览区标题行的「约 X 天后被 Claude 删除」、
//! 列表行的「只剩备份」、回车时的提示和菜单里的「放回 Claude 目录再接着聊」（`P`）。

use super::*;
use crate::index::NEAR_DELETION;
use crate::sources::{backup, claude};

/// 只剩备份的对话行标题后的那句。
pub const BACKUP_ONLY: &str = "只剩备份";
/// 只剩备份的对话按回车时的提示。
pub const BACKUP_ONLY_OPEN: &str =
    "Claude 已删掉原文件，接不上：右键菜单「放回 Claude 目录再接着聊」（P）· v 看整段";

impl App {
    /// 这条是 Claude 删掉了原文件、只剩会话台备份的对话。
    pub fn backup_only(e: &Entry) -> bool {
        e.session.vendor == Vendor::Claude && backup::is_backup(&e.session.path)
    }

    /// 离被 Claude 删不到 3 天的 Claude 对话：「约 2 天后被 Claude 删除」；已过保留期的说下次开 Claude 时删。
    /// 天数按这个账号设置里的 `cleanupPeriodDays`（运行层读来放进 [`App::retention`]，没读到按默认 30 天）。
    pub fn deletion_note(&self, e: &Entry) -> Option<String> {
        if e.session.vendor != Vendor::Claude || Self::backup_only(e) {
            return None;
        }
        let days = self
            .retention
            .get(&e.session.account)
            .copied()
            .unwrap_or(claude::DEFAULT_RETENTION_DAYS);
        match claude::deleted_in(e.session.last_active, self.now, days) {
            None => Some("已过 Claude 的保留期，下次开 Claude 时会被删".to_owned()),
            Some(left) if left < NEAR_DELETION => {
                let d = left.as_secs().div_ceil(86_400).max(1);
                Some(format!("约 {d} 天后被 Claude 删除"))
            }
            Some(_) => None,
        }
    }

    /// `P`：把只剩备份的对话放回 Claude 目录（原处有同名文件不覆盖），放回了就接着聊。
    pub(super) fn put_back(&mut self) -> Vec<Effect> {
        let Some(e) = self.selected_entry() else {
            return Vec::new();
        };
        if !Self::backup_only(e) {
            self.status = Some("这条对话的原文件还在，不用放回".to_owned());
            return Vec::new();
        }
        vec![Effect::PutBack(Box::new(e.clone()))]
    }
}
