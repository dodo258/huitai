//! Codex：最新会话记录（rollout）里带额度的事件。只读文件末尾。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use super::rules::Channel;
use super::{QuotaReader, WindowsError, jsonl_files, matches_all, time, windows_from};
use crate::core::QuotaReading;

/// 按修改时间从新到旧找，第一个带额度窗口的事件就是读数；读数时间取事件自己的时间戳。
pub(super) fn read_rollout(r: &QuotaReader, ch: &Channel) -> Result<QuotaReading, String> {
    let Channel::Rollout {
        dir,
        line_contains,
        matches,
        timestamp,
        max_files,
        tail_bytes,
        windows,
    } = ch
    else {
        unreachable!("只给 rollout 渠道调用")
    };
    let mut files = jsonl_files(&r.dirs.resolve(&r.home, dir))?;
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    let mut last_err = "最近的会话记录里没有额度事件".to_owned();
    for (path, mtime) in files.iter().take(*max_files) {
        let Ok(tail) = read_tail(path, *tail_bytes) else {
            continue;
        };
        for line in tail
            .lines()
            .rev()
            .filter(|l| l.contains(line_contains.as_str()))
        {
            let Ok(doc) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if !matches_all(&doc, matches) {
                continue;
            }
            match windows_from(&doc, windows, r.now) {
                Ok(value) => {
                    let at = doc
                        .pointer(timestamp)
                        .and_then(time::value_secs)
                        .map(time::from_secs)
                        .unwrap_or(*mtime);
                    return Ok(r.reading(ch, value, at));
                }
                // 最新一条带额度的事件窗口都过期了，更早的只会更旧。
                Err(e @ WindowsError::AllExpired) => return Err(e.into()),
                // 这条没带额度（或格式变了），继续往前找。
                Err(e) => last_err = e.into(),
            }
        }
    }
    Err(last_err)
}

/// 读文件最后 `n` 字节；从中间开始时丢掉第一行残片。
fn read_tail(path: &Path, n: u64) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(n);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    Ok(if start > 0 {
        text.split_once('\n')
            .map_or("", |(_, rest)| rest)
            .to_owned()
    } else {
        text.into_owned()
    })
}
