//! 搜索结果行下的命中片段。正文的全文索引是 contentless（只存索引不存原文），取不回原文，
//! 片段从 `Source::load_transcript` 读回的消息里找：按空白拆词，按全文索引同样的归一化（[`text::fold`]：
//! 不分大小写、去变音）比子串，索引命中的片段也找得到（搜 `cafe` 命中 `café`）。

use super::text;
use crate::sources::{Part, Transcript};

/// 命中词前面最多留几个字，界面再按宽度裁。
const BEFORE: usize = 40;
/// 片段最多几个字（一行放不下的界面截掉）。
const CHARS: usize = 240;

/// 一段命中片段：一行字（换行、控制字符都换成了空格），和命中词在里面的字节位置（从前往后、不重叠）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    pub marks: Vec<(usize, usize)>,
}

/// 在整段对话的消息里找搜索词，取它前后一小段。从最新的消息往前找：先找每个词都在的那条，
/// 没有就取任何一个词出现的那条（几个词分在几条消息里，全文索引照样算命中）。都没有返回 `None`。
pub fn find(transcript: &Transcript, query: &str) -> Option<Snippet> {
    let terms = terms(query);
    if terms.is_empty() {
        return None;
    }
    let mut any = None;
    for part in transcript.parts.iter().rev() {
        let Part::Message(m) = part else { continue };
        let folded = Folded::of(&m.text);
        let found: Vec<usize> = terms
            .iter()
            .filter_map(|t| folded.text.find(t.as_str()).map(|i| folded.at[i]))
            .collect();
        let Some(&first) = found.iter().min() else {
            continue;
        };
        if found.len() == terms.len() {
            return Some(window(&m.text, first, &terms));
        }
        any = any.or(Some((&m.text, first)));
    }
    any.map(|(text, at)| window(text, at, &terms))
}

/// 搜索词按空白拆开、按 [`text::fold`] 归一化；归一化后空了的词不要。查看视图的高亮也用它和 [`marks`]。
pub fn terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|t| t.chars().filter_map(text::fold).collect::<String>())
        .filter(|t| !t.is_empty())
        .collect()
}

/// 一段字逐字按 [`text::fold`] 归一化后的样子，和归一化后每个字节对应原文的字节位置。
struct Folded {
    text: String,
    /// `at[i]`：归一化后第 `i` 个字节出自原文哪个字；多一项 `at[text.len()]` 是原文长度。
    /// 命中的结尾落在下一个留下的字上，紧跟着被去掉的组合变音符号也算进命中里。
    at: Vec<usize>,
}

impl Folded {
    fn of(original: &str) -> Self {
        let mut text = String::with_capacity(original.len());
        let mut at = Vec::with_capacity(original.len() + 1);
        for (i, c) in original.char_indices() {
            if let Some(f) = text::fold(c) {
                text.push(f);
                at.resize(text.len(), i);
            }
        }
        at.push(original.len());
        Self { text, at }
    }
}

/// `text` 里从 `at`（字节位置）往前 [`BEFORE`] 个字起、共 [`CHARS`] 个字，折成一行，前后截掉了就加 `…`。
fn window(text: &str, at: usize, terms: &[String]) -> Snippet {
    let start = text[..at]
        .char_indices()
        .rev()
        .take(BEFORE)
        .last()
        .map_or(at, |(i, _)| i);
    let end = text[start..]
        .char_indices()
        .nth(CHARS)
        .map_or(text.len(), |(i, _)| start + i);
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let mut space = false;
    for c in text[start..end].chars() {
        if c.is_whitespace() || c.is_control() {
            space = true;
            continue;
        }
        if space && !out.is_empty() && out != "…" {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    if end < text.len() {
        out.push('…');
    }
    let marks = marks(&out, terms);
    Snippet { text: out, marks }
}

/// 每个词在 `text` 里出现的地方（字节位置），排好序、重叠的合并。
pub fn marks(text: &str, terms: &[String]) -> Vec<(usize, usize)> {
    let folded = Folded::of(text);
    let mut found: Vec<(usize, usize)> = Vec::new();
    for term in terms {
        let mut from = 0;
        while let Some(i) = folded.text[from..].find(term.as_str()) {
            let s = from + i;
            found.push((folded.at[s], folded.at[s + term.len()]));
            from = s + term.len();
        }
    }
    found.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (s, e) in found {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::{PreviewMessage, Role};

    fn transcript(texts: &[&str]) -> Transcript {
        Transcript {
            parts: texts
                .iter()
                .map(|t| {
                    Part::Message(PreviewMessage {
                        role: Role::User,
                        text: (*t).to_owned(),
                        at: None,
                    })
                })
                .collect(),
            truncated: false,
        }
    }

    fn marked(s: &Snippet) -> Vec<&str> {
        s.marks.iter().map(|&(a, b)| &s.text[a..b]).collect()
    }

    #[test]
    fn finds_term_case_insensitively_and_marks_every_occurrence() {
        let t = transcript(&["别的", "修 Login 页，\n再看 login 接口"]);
        let s = find(&t, "login").unwrap();
        assert_eq!(s.text, "修 Login 页， 再看 login 接口");
        assert_eq!(marked(&s), ["Login", "login"]);
    }

    #[test]
    fn prefers_newest_message_with_all_terms() {
        let t = transcript(&["结算 和 优惠券", "只有结算", "新的：只有优惠券"]);
        let s = find(&t, "结算 优惠券").unwrap();
        assert_eq!(s.text, "结算 和 优惠券");
        let t = transcript(&["旧的结算", "新的结算"]);
        assert_eq!(find(&t, "结算").unwrap().text, "新的结算");
        // 没有哪条两个词都有：取最新的出现任何一个词的那条。
        let t = transcript(&["只有结算", "新的：只有优惠券"]);
        assert_eq!(find(&t, "结算 优惠券").unwrap().text, "新的：只有优惠券");
    }

    #[test]
    fn long_message_is_windowed_around_the_hit() {
        let text = format!("{}命中词{}", "前".repeat(500), "后".repeat(500));
        let s = find(&transcript(&[&text]), "命中词").unwrap();
        assert!(s.text.starts_with('…') && s.text.ends_with('…'));
        assert_eq!(s.text.chars().count(), CHARS + 2);
        let before = s.text[..s.marks[0].0].chars().count();
        assert_eq!(before, BEFORE + 1, "前面留 {BEFORE} 个字加省略号");
        assert_eq!(marked(&s), ["命中词"]);
    }

    #[test]
    fn tools_and_missing_terms_give_nothing() {
        let mut t = transcript(&["没有"]);
        t.parts.push(Part::ToolResult {
            text: "结算".into(),
        });
        assert_eq!(find(&t, "结算"), None);
        assert_eq!(find(&t, "  "), None);
        assert_eq!(find(&Transcript::default(), "结算"), None);
    }

    /// 审计 v0.1.4 第 24 条：全文索引去变音、不分大小写，搜 `cafe` 命中只含 `café` 的正文，片段也要找得到，
    /// 高亮落在原文那个词上（分解写法的组合符号一起标上）。
    #[test]
    fn folds_like_the_index() {
        let s = find(&transcript(&["去 Café 坐坐"]), "cafe").unwrap();
        assert_eq!(s.text, "去 Café 坐坐");
        assert_eq!(marked(&s), ["Café"]);
        let s = find(&transcript(&["cafe\u{301} au lait"]), "CAFÉ").unwrap();
        assert_eq!(marked(&s), ["cafe\u{301}"]);
        let s = find(&transcript(&["Über naïve Straße"]), "uber naive").unwrap();
        assert_eq!(marked(&s), ["Über", "naïve"]);
    }

    #[test]
    fn control_chars_never_reach_the_line() {
        let s = find(&transcript(&["a\x1b[31mred\tbug\r\n\n end"]), "bug").unwrap();
        assert!(!s.text.chars().any(char::is_control));
        assert_eq!(s.text, "a [31mred bug end");
    }
}
