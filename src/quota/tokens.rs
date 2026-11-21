//! 统计本机会话记录里今天（本地时间）用掉的 token。只看今天改过的文件。

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

use super::rules::Channel;
use super::{QuotaReader, jsonl_files, matches_all, time};
use crate::core::QuotaValue;

pub(super) fn tokens_today(r: &QuotaReader, ch: &Channel) -> Result<QuotaValue, String> {
    let Channel::TokensToday {
        dir,
        line_contains,
        matches,
        timestamp,
        dedupe,
        count,
        cumulative,
    } = ch
    else {
        unreachable!("只给 token 统计渠道调用")
    };
    let midnight = time::local_midnight(r.now, r.utc_offset_secs);
    let mut tally = Tally::default();
    for (path, mtime) in jsonl_files(&r.dirs.resolve(&r.home, dir))? {
        if time::to_secs(mtime) < midnight {
            continue;
        }
        let mut file = FileTally::default();
        for_each_line(&path, |line| {
            if !line.contains(line_contains.as_str()) {
                return;
            }
            let Ok(doc) = serde_json::from_str::<Value>(line) else {
                return;
            };
            if !matches_all(&doc, matches) {
                return;
            }
            tally.matched += 1;
            let Some(ts) = doc.pointer(timestamp).and_then(time::value_secs) else {
                return;
            };
            let Some(tokens) = sum_fields(&doc, count) else {
                return;
            };
            tally.recognized += 1;
            if *cumulative {
                // 累计值：记下今天之前和今天各自最后一个。
                if ts < midnight {
                    file.before = Some(tokens);
                } else {
                    file.today = Some(tokens);
                }
                return;
            }
            if ts < midnight {
                return;
            }
            if !dedupe.is_empty() {
                let key: Vec<&Value> = dedupe.iter().filter_map(|p| doc.pointer(p)).collect();
                if !key.is_empty()
                    && !tally
                        .seen
                        .insert(serde_json::to_string(&key).unwrap_or_default())
                {
                    return;
                }
            }
            tally.total += tokens;
        });
        if let Some(today) = file.today {
            tally.total += today.saturating_sub(file.before.unwrap_or(0));
        }
    }
    if tally.matched > 0 && tally.recognized == 0 {
        return Err("会话记录里认不出 token 字段（格式可能变了）".to_owned());
    }
    Ok(QuotaValue::TokensToday(tally.total))
}

#[derive(Default)]
struct Tally {
    total: u64,
    matched: u64,
    recognized: u64,
    seen: HashSet<String>,
}

#[derive(Default)]
struct FileTally {
    before: Option<u64>,
    today: Option<u64>,
}

/// 这些字段相加；一个都没有时返回 `None`。
fn sum_fields(doc: &Value, fields: &[String]) -> Option<u64> {
    let values: Vec<u64> = fields
        .iter()
        .filter_map(|p| doc.pointer(p).and_then(Value::as_u64))
        .collect();
    (!values.is_empty()).then(|| values.iter().sum())
}

/// 逐行读；不是 UTF-8 的行跳过，读不了的文件跳过。
fn for_each_line(path: &Path, mut f: impl FnMut(&str)) {
    let Ok(file) = File::open(path) else { return };
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if let Ok(line) = std::str::from_utf8(&buf) {
                    f(line);
                }
            }
        }
    }
}
