//! 全文索引的切词：FTS5 的 unicode61 把一串连着的汉字当成一个词，搜「登录」找不到「修登录页」。
//! 写入和查询前都把中日韩字符两边加空格、一字一词，查询时用短语（相邻）匹配，等价于子串匹配；
//! 字母数字照常按词切，查询词按前缀匹配。

/// 中日韩文字（含假名、谚文、全角符号区）。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x9FFF      // 部首、标点、假名、注音、CJK 统一汉字
        | 0xAC00..=0xD7AF    // 谚文
        | 0xF900..=0xFAFF    // 兼容汉字
        | 0xFF00..=0xFFEF    // 全角
        | 0x20000..=0x3FFFF) // 扩展 B 以后
}

/// 写进索引前的切分：汉字一字一词。
pub(crate) fn split(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 2);
    for c in text.chars() {
        if is_cjk(c) {
            out.push(' ');
            out.push(c);
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

/// 把用户输入变成 FTS5 查询：按空白分成几个词，全部都要命中（AND）；
/// 每个词是一个短语，最后一个字是字母数字时按前缀匹配。没有可搜的字时返回 `None`。
pub(crate) fn fts_query(input: &str) -> Option<String> {
    let parts: Vec<String> = input
        .split_whitespace()
        .filter_map(|term| {
            let last = term.chars().rev().find(|c| c.is_alphanumeric())?;
            let phrase = split(term).replace('"', "\"\"");
            let prefix = if is_cjk(last) { "" } else { "*" };
            Some(format!("\"{}\"{prefix}", phrase.trim()))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" AND "))
}

/// 标题匹配：每个词（不分大小写）都出现在标题里。
pub(crate) fn title_matches(title: &str, input: &str) -> bool {
    let title = title.to_lowercase();
    let mut terms = input.split_whitespace().peekable();
    terms.peek().is_some() && terms.all(|t| title.contains(&t.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_spaces_out_cjk_only() {
        assert_eq!(split("修登录bug"), " 修  登  录 bug");
        assert_eq!(split("plain text"), "plain text");
    }

    #[test]
    fn query_phrases_and_prefixes() {
        assert_eq!(fts_query("登录").as_deref(), Some("\"登  录\""));
        assert_eq!(fts_query("work").as_deref(), Some("\"work\"*"));
        assert_eq!(
            fts_query("api 登录").as_deref(),
            Some("\"api\"* AND \"登  录\"")
        );
        assert_eq!(fts_query("say\"hi").as_deref(), Some("\"say\"\"hi\"*"));
    }

    #[test]
    fn query_without_searchable_chars_is_none() {
        assert_eq!(fts_query(""), None);
        assert_eq!(fts_query("  -- ** "), None);
    }

    #[test]
    fn title_match_is_case_insensitive_and_all_terms() {
        assert!(title_matches("修 Login 页面", "login 页面"));
        assert!(!title_matches("修 Login 页面", "login 购物"));
        assert!(!title_matches("任意", "   "));
    }
}
