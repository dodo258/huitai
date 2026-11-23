//! 给 Claude 对话做备份的那一轮：挑哪些备（[`wants_backup`]），交给各账号的来源去备（[`Source::back_up`]）。
//! 什么时候跑由界面的后台线程定（起来时、改了标注或设置时、之后每隔一会儿）。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use super::{Entry, Index};
use crate::core::{SessionId, Vendor};
use crate::sources::{Saved, backup, claude};

/// 离被 Claude 删不到这么久算「快删了」：界面上提醒，备份这一轮再完整解一遍确认没坏。
pub const NEAR_DELETION: Duration = Duration::from_secs(3 * 86_400);

/// 一轮备份做了什么。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BackupReport {
    /// 新写或更新的。
    pub written: usize,
    /// 已经是最新的。
    pub fresh: usize,
    /// 「对话标题: 原因」。
    pub problems: Vec<String>,
}

/// 这条该不该备：Claude 的、原文件还在（只剩备份的就是备份本身），并且在会话台里置顶、改名、归档过，
/// 或者设置里开了「备份全部 Claude 对话」（`all`）。
pub fn wants_backup(e: &Entry, all: bool) -> bool {
    let n = &e.note;
    e.session.vendor == Vendor::Claude
        && !backup::is_backup(&e.session.path)
        && (all || n.pinned || n.archived || n.title.is_some())
}

/// 这条离被 Claude 删不到 [`NEAR_DELETION`]（或已经过了保留期）。`days` 是这个账号的保留天数。
pub fn near_deletion(e: &Entry, now: SystemTime, days: u32) -> bool {
    claude::deleted_in(e.session.last_active, now, days).is_none_or(|left| left < NEAR_DELETION)
}

impl Index {
    /// 备一轮：该备的对话备份不是最新就更新。`retention` 是各 Claude 账号（短名）留对话的天数；
    /// 快删了的每条在 `checked` 里没记过就完整解一遍备份确认没坏，确认过记进去（同一个进程只确认一次）。
    pub fn back_up_claude(
        &self,
        all: bool,
        retention: &HashMap<String, u32>,
        now: SystemTime,
        checked: &mut HashSet<SessionId>,
    ) -> BackupReport {
        let mut report = BackupReport::default();
        let entries = match self.list(Vendor::Claude) {
            Ok(e) => e,
            Err(e) => {
                report.problems.push(format!("读列表: {e}"));
                return report;
            }
        };
        for e in entries.iter().filter(|e| wants_backup(e, all)) {
            let s = &e.session;
            let days = retention
                .get(&s.account)
                .copied()
                .unwrap_or(claude::DEFAULT_RETENTION_DAYS);
            let check = near_deletion(e, now, days) && !checked.contains(&s.id);
            match self.source(s).and_then(|src| src.back_up(s, check)) {
                Ok(saved) => {
                    if check {
                        checked.insert(s.id.clone());
                    }
                    match saved {
                        Saved::Written => report.written += 1,
                        Saved::Fresh => report.fresh += 1,
                    }
                }
                Err(err) => report.problems.push(format!("{}: {err}", e.title())),
            }
        }
        report
    }
}
