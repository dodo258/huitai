//! 三家 jsonl 会话文件共用的读法：逐行解析（坏行跳过）、往回找最后几条消息、按需精确计数、并行扫描。

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;

use serde_json::Value;

use super::window::{self, PREVIEW_MAX_BYTES};
use super::{Counted, PreviewMessage, Progress, Role};

/// 一行 JSON；坏行、非对象返回 `None`。
pub(crate) fn parse(line: &[u8]) -> Option<Value> {
    serde_json::from_slice::<Value>(line)
        .ok()
        .filter(Value::is_object)
}

/// 取字符串字段。
pub(crate) fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// 把 `[{type:"text", text}]` 或纯字符串内容拼成文本；`kinds` 是认作文本的块类型。
pub(crate) fn content_text(content: &Value, kinds: &[&str]) -> Option<String> {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|it| str_field(it, "type").is_some_and(|t| kinds.contains(&t)))
            .filter_map(|it| str_field(it, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// 折成一行、最多 `max` 个字符，用作列表标题。
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

/// 预览里每条消息最多保留的字符数。
const PREVIEW_CHARS: usize = 2000;

pub(crate) fn clip(mut m: PreviewMessage) -> PreviewMessage {
    if let Some((i, _)) = m.text.char_indices().nth(PREVIEW_CHARS) {
        m.text.truncate(i);
        m.text.push('…');
    }
    m
}

/// 从文件尾往回读，取最后 `n` 条消息（按时间先后）。读的范围从 256KB 起每次乘 4，最多 8MB。
pub(crate) fn last_messages(
    path: &Path,
    n: usize,
    message: impl Fn(&Value) -> Option<PreviewMessage>,
) -> io::Result<Vec<PreviewMessage>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = 256 * 1024;
    loop {
        let (buf, from_start) = window::read_tail_lines(path, bytes)?;
        let mut out: Vec<PreviewMessage> = window::lines(&buf)
            .rev()
            .filter_map(|l| parse(l).as_ref().and_then(&message))
            .take(n)
            .collect();
        if out.len() == n || from_start || bytes >= PREVIEW_MAX_BYTES {
            out.reverse();
            return Ok(out.into_iter().map(clip).collect());
        }
        bytes = (bytes * 4).min(PREVIEW_MAX_BYTES);
    }
}

/// 读完整个文件精确数用户发言。只解析含 `marker` 的行，其余行不解析。
pub(crate) fn count_user_messages(
    path: &Path,
    marker: &[u8],
    message: impl Fn(&Value) -> Option<PreviewMessage>,
) -> io::Result<usize> {
    count_since(path, marker, 0, message, None).map(|c| c.users)
}

/// 接着 `prev` 精确数用户发言，见 [`Source::count_since`](super::Source::count_since)。
/// `prev` 对不上这个文件（变短、开头或接缝处变了、`salt` 也就是解析版本变了）就从头数，只解析含 `marker` 的行；
/// 接着数时新增那段每行都解析，顺带把其中的消息交出去（新增超过 [`PREVIEW_MAX_BYTES`] 时不交）。
pub(crate) fn count_since(
    path: &Path,
    marker: &[u8],
    salt: u32,
    message: impl Fn(&Value) -> Option<PreviewMessage>,
    prev: Option<Progress>,
) -> io::Result<Counted> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    // 读不出那两段（文件刚被截短）也算对不上。
    let resume = prev.filter(|p| {
        p.pos <= len && window::digest(&mut file, p.pos, salt).is_ok_and(|d| d == p.digest)
    });
    let mut appended = resume
        .filter(|p| len - p.pos <= PREVIEW_MAX_BYTES)
        .map(|_| Vec::new());
    // 完整行里的、最后那行还没写完换行的。
    let (mut done, mut partial) = (resume.map_or(0, |p| p.users), 0);
    let start = resume.map_or(0, |p| p.pos);
    let end = window::for_each_line_from(&mut file, start, |line, complete| {
        if appended.is_none() && !contains(line, marker) {
            return;
        }
        let Some(m) = parse(line).as_ref().and_then(&message) else {
            return;
        };
        if m.role == Role::User {
            *(if complete { &mut done } else { &mut partial }) += 1;
        }
        if let (true, Some(a)) = (complete, appended.as_mut()) {
            a.push(clip(m));
        }
    })?;
    Ok(Counted {
        users: done + partial,
        progress: Some(Progress {
            pos: end,
            users: done,
            digest: window::digest(&mut file, end, salt)?,
        }),
        appended,
    })
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
}

/// 每个文件交给 `f`，多线程并行；结果顺序与输入一致。
pub(crate) fn par_map<T: Send>(paths: &[PathBuf], f: impl Fn(&Path) -> T + Sync) -> Vec<T> {
    let workers = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16);
    let chunk = paths.len().div_ceil(workers).max(1);
    thread::scope(|s| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|part| s.spawn(|| part.iter().map(|p| f(p)).collect::<Vec<T>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| {
                h.join()
                    .expect("扫描线程不会 panic：解析函数只返回 Option/Result")
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bad_lines_are_none() {
        assert!(parse(b"{\"a\":1}").is_some());
        assert!(parse(b"{\"a\":").is_none());
        assert!(parse(b"[1,2]").is_none());
        assert!(parse(b"").is_none());
    }

    #[test]
    fn content_text_handles_string_and_blocks() {
        assert_eq!(
            content_text(&json!("  hi "), &["text"]).as_deref(),
            Some("hi")
        );
        let blocks = json!([
            {"type": "text", "text": "a"},
            {"type": "tool_use", "text": "x"},
            {"type": "text", "text": "b"}
        ]);
        assert_eq!(content_text(&blocks, &["text"]).as_deref(), Some("a\nb"));
        assert_eq!(content_text(&json!([]), &["text"]), None);
        assert_eq!(content_text(&json!(3), &["text"]), None);
    }

    #[test]
    fn one_line_flattens_and_truncates() {
        assert_eq!(one_line("修 登录\n页面", 80), "修 登录 页面");
        assert_eq!(one_line("一二三四五", 3), "一二三…");
    }

    #[test]
    fn par_map_keeps_order() {
        let paths: Vec<PathBuf> = (0..50).map(|i| PathBuf::from(i.to_string())).collect();
        let out = par_map(&paths, |p| p.to_string_lossy().parse::<u32>().unwrap());
        assert_eq!(out, (0..50).collect::<Vec<_>>());
        assert!(par_map(&[], |_| 0).is_empty());
    }
}
