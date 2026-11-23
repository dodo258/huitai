//! 每条对话此刻的上下文占用：读注入的 Claude 状态栏命令按 `session_id` 另存的那份数据
//! （官方状态栏字段 `context_window.used_percentage`，位置写在 `rules.json`）。

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::QuotaReader;
use super::rules::Channel;
use crate::core::Vendor;

/// 多久没更新的那份删掉：Claude 默认 30 天清理旧对话，留着也对不上了。
const KEEP: Duration = Duration::from_secs(30 * 24 * 3600);

/// 一条对话的上下文占用。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextUse {
    /// 已用百分比（0–100）。
    pub percent: f32,
    /// 读数时间（那份数据的修改时间）。
    pub at: SystemTime,
}

/// 读过的文件：路径 → 修改时间和读出的百分比（`None` 是那份数据里还没有，比如刚开、刚 /compact 完）。
/// 留着它只重读变了的文件。
#[derive(Debug, Default)]
pub struct ContextFiles(HashMap<PathBuf, (SystemTime, Option<f32>)>);

impl QuotaReader {
    /// 这个账号目录下各条对话的上下文占用：厂商自己的会话 id → 读数。没读数的不列。
    /// 只重读修改时间变了的文件；超过 [`KEEP`] 没更新的删掉。
    pub fn contexts(&self, seen: &mut ContextFiles) -> HashMap<String, ContextUse> {
        let Some((dir, pointer)) =
            self.rules
                .channels(Vendor::Claude)
                .iter()
                .find_map(|ch| match ch {
                    Channel::Statusline {
                        context_dir,
                        context_percent,
                        ..
                    } => Some((self.data_dir.join(context_dir), context_percent.as_str())),
                    _ => None,
                })
        else {
            return HashMap::new();
        };
        let mut out = HashMap::new();
        let mut kept = HashMap::new();
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
                .filter(|n| !n.starts_with('.'))
            else {
                continue;
            };
            let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if self.now.duration_since(mtime).is_ok_and(|age| age > KEEP) {
                let _ = fs::remove_file(&path);
                continue;
            }
            let percent = match seen.0.remove(&path) {
                Some((at, p)) if at == mtime => p,
                _ => read_percent(&path, pointer),
            };
            if let Some(p) = percent {
                out.insert(
                    id.to_owned(),
                    ContextUse {
                        percent: p,
                        at: mtime,
                    },
                );
            }
            kept.insert(path, (mtime, percent));
        }
        seen.0 = kept;
        out
    }
}

fn read_percent(path: &std::path::Path, pointer: &str) -> Option<f32> {
    let doc: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let p = doc.pointer(pointer)?.as_f64()?;
    Some(p.clamp(0.0, 100.0) as f32)
}
