//! 整段对话（只读滚动视图用）：按时间先后列出消息、工具调用和工具返回。
//! 三家 jsonl 共用一种读法：从文件尾往前最多读 [`TRANSCRIPT_MAX_BYTES`]，逐行交给各家的拆块函数；
//! 每块文字截短，内存有上限。

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use super::PreviewMessage;
use super::jsonl::{self, str_field};
use super::window;

/// 只读文件最后这么多字节：几百 MB 的会话文件也几百毫秒内读完。更早的部分回车进 CLI 看。
pub const TRANSCRIPT_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// 一条消息最多保留的字符数。
const MESSAGE_CHARS: usize = 20_000;
/// 工具参数摘要最多保留的字符数（折成一行）。
const INPUT_CHARS: usize = 300;
/// 工具返回最多保留的字符数。
const OUTPUT_CHARS: usize = 1_500;

/// 整段对话里的一块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Message(PreviewMessage),
    /// 工具调用：工具名、参数摘要（一行）。
    ToolCall {
        name: String,
        input: String,
    },
    /// 工具返回的文字。
    ToolResult {
        text: String,
    },
}

/// 一条对话的全文。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transcript {
    pub parts: Vec<Part>,
    /// 文件比 [`TRANSCRIPT_MAX_BYTES`] 大：只读了最后那段，前面的没读。
    pub truncated: bool,
}

/// 读一个 jsonl 会话文件的全文，`parts` 把一行拆成几块（坏行、和对话无关的行返回空）。
pub(crate) fn read(path: &Path, parts: impl Fn(&Value) -> Vec<Part>) -> io::Result<Transcript> {
    read_capped(path, TRANSCRIPT_MAX_BYTES, parts)
}

fn read_capped(
    path: &Path,
    max: u64,
    parts: impl Fn(&Value) -> Vec<Part>,
) -> io::Result<Transcript> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max);
    let mut out = Transcript {
        parts: Vec::new(),
        truncated: start > 0,
    };
    // 从中间开始读、前一个字节又不是换行时，第一行是半截，跳过。
    let mut skip = start > 0 && {
        let mut b = [0u8];
        file.seek(SeekFrom::Start(start - 1))?;
        file.read_exact(&mut b)?;
        b[0] != b'\n'
    };
    window::for_each_line_from(&mut file, start, |line, _| {
        if std::mem::take(&mut skip) {
            return;
        }
        if let Some(v) = jsonl::parse(line) {
            out.parts.extend(parts(&v).into_iter().map(clip));
        }
    })?;
    Ok(out)
}

/// 每块截短（消息、工具参数、工具返回各有上限）。
pub(crate) fn clip(p: Part) -> Part {
    match p {
        Part::Message(mut m) => {
            m.text = cut(&m.text, MESSAGE_CHARS);
            Part::Message(m)
        }
        Part::ToolCall { name, input } => Part::ToolCall {
            name,
            input: jsonl::one_line(&input, INPUT_CHARS),
        },
        Part::ToolResult { text } => Part::ToolResult {
            text: cut(text.trim(), OUTPUT_CHARS),
        },
    }
}

fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

/// 工具参数的摘要：常见的命令、路径、搜索词字段取它的值，否则整个参数压成一行 JSON。
/// 参数是 JSON 字符串（Codex `function_call.arguments`）先解开再取。
pub(crate) fn input_summary(input: &Value) -> String {
    const KEYS: [&str; 11] = [
        "command",
        "cmd",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "description",
        "title",
        "prompt",
        "message",
    ];
    if let Value::String(s) = input {
        return match serde_json::from_str::<Value>(s) {
            Ok(v) if v.is_object() => input_summary(&v),
            _ => s.clone(),
        };
    }
    for k in KEYS {
        match input.get(k) {
            Some(Value::String(s)) if !s.trim().is_empty() => return s.clone(),
            Some(Value::Array(items)) if !items.is_empty() => {
                return items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            _ => {}
        }
    }
    match input {
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// 工具返回的文字：纯字符串，或 `[{type:"text"|"input_text"|"output_text", text}]`。
pub(crate) fn output_text(output: &Value) -> String {
    jsonl::content_text(output, &["text", "input_text", "output_text"])
        .or_else(|| str_field(output, "output").map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::Role;
    use serde_json::json;
    use std::io::Write;
    use std::path::PathBuf;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("huitai-tr-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("a.jsonl")
    }

    fn lines_parts(v: &Value) -> Vec<Part> {
        match str_field(v, "t") {
            Some("m") => vec![Part::Message(PreviewMessage {
                role: Role::User,
                text: str_field(v, "x").unwrap_or_default().to_owned(),
                at: None,
            })],
            _ => Vec::new(),
        }
    }

    #[test]
    fn reads_whole_small_file_and_skips_bad_lines() {
        let p = temp_file("small");
        let mut f = File::create(&p).unwrap();
        writeln!(f, r#"{{"t":"m","x":"一"}}"#).unwrap();
        writeln!(f, "不是 json").unwrap();
        writeln!(f, r#"{{"t":"other"}}"#).unwrap();
        write!(f, r#"{{"t":"m","x":"二"}}"#).unwrap();
        let t = read(&p, lines_parts).unwrap();
        assert!(!t.truncated);
        let texts: Vec<_> = t
            .parts
            .iter()
            .map(|p| match p {
                Part::Message(m) => m.text.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(texts, ["一", "二"]);
    }

    #[test]
    fn big_file_reads_only_the_tail_and_drops_the_half_line() {
        let p = temp_file("big");
        let mut f = File::create(&p).unwrap();
        for i in 0..100 {
            writeln!(f, r#"{{"t":"m","x":"{i:03}"}}"#).unwrap();
        }
        // 每行 20 字节（含换行）：最后 100 字节正好从行首开始，5 行都在；
        // 最后 99 字节从行中间开始，开头的半截行丢掉，剩 4 行。
        let t = read_capped(&p, 100, lines_parts).unwrap();
        assert!(t.truncated);
        assert_eq!(t.parts.len(), 5);
        assert_eq!(read_capped(&p, 99, lines_parts).unwrap().parts.len(), 4);
        assert_eq!(
            t.parts.last(),
            Some(&Part::Message(PreviewMessage {
                role: Role::User,
                text: "099".into(),
                at: None
            }))
        );
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(read(Path::new("/nonexistent/x.jsonl"), lines_parts).is_err());
    }

    #[test]
    fn long_parts_are_clipped() {
        let long = "长".repeat(MESSAGE_CHARS + 10);
        let Part::Message(m) = clip(Part::Message(PreviewMessage {
            role: Role::Assistant,
            text: long,
            at: None,
        })) else {
            unreachable!()
        };
        assert_eq!(m.text.chars().count(), MESSAGE_CHARS + 1);
        assert!(m.text.ends_with('…'));
        let Part::ToolCall { input, .. } = clip(Part::ToolCall {
            name: "Bash".into(),
            input: "a\n  b".into(),
        }) else {
            unreachable!()
        };
        assert_eq!(input, "a b");
    }

    #[test]
    fn input_summary_picks_the_useful_field() {
        assert_eq!(input_summary(&json!({"command": "ls -la"})), "ls -la");
        assert_eq!(
            input_summary(&json!({"file_path": "/x/a.rs", "limit": 3})),
            "/x/a.rs"
        );
        assert_eq!(
            input_summary(&json!({"cmd": ["git", "status"]})),
            "git status"
        );
        assert_eq!(input_summary(&json!(r#"{"cmd":"pwd"}"#)), "pwd");
        assert_eq!(input_summary(&json!("print(1)")), "print(1)");
        assert_eq!(input_summary(&json!({"n": 1})), r#"{"n":1}"#);
        assert_eq!(input_summary(&Value::Null), "");
    }

    #[test]
    fn output_text_handles_string_blocks_and_objects() {
        assert_eq!(output_text(&json!("ok")), "ok");
        assert_eq!(
            output_text(
                &json!([{"type": "input_text", "text": "a"}, {"type": "text", "text": "b"}])
            ),
            "a\nb"
        );
        assert_eq!(output_text(&json!({"output": "done"})), "done");
        assert_eq!(output_text(&json!(3)), "");
    }
}
