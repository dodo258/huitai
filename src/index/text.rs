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

/// 一个字按全文索引（`unicode61 remove_diacritics 2`）的规则归一化：转小写、去掉变音（é → e），
/// 单独的组合变音符号（U+0301 这类）返回 `None`（索引里也不留）。别的字原样。命中片段用它找词，
/// 和 FTS 认的同一套：搜 `cafe` 能命中 `café`，片段也得找得到。
/// 去变音照搬 SQLite `fts5_remove_diacritic` 的表；大小写用 Rust 的小写，加上 SQLite 折叠得不一样的几个拉丁字
/// （`fold_matches_the_fts_tokenizer` 逐字对过拉丁各区）。
pub(crate) fn fold(c: char) -> Option<char> {
    if c.is_ascii() {
        return Some(c.to_ascii_lowercase());
    }
    // 本来就是小写、但 SQLite 按大小写折叠换成别的字的几个。
    let c = match c {
        'µ' => 'μ',
        'ſ' | 'ẛ' => 's',
        _ => {
            let mut lower = c.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some(l), None) => l,
                _ => c,
            }
        }
    };
    remove_diacritic(c)
}

/// SQLite `fts5_remove_diacritic(c, 1)`：`DIA` 每项是「起始码位 << 3 | 区间长度」，同一区间的字去变音后都是
/// `DIA_CHAR` 里对应那个字母（0：整个字去掉）。高位 `HI` 标的是只在 `remove_diacritics 2` 才去的。
fn remove_diacritic(c: char) -> Option<char> {
    let Ok(code) = u16::try_from(u32::from(c)) else {
        return Some(c);
    };
    let Some(key) = code.checked_mul(8).map(|k| k | 7) else {
        return Some(c);
    };
    let i = DIA.partition_point(|&d| d <= key).saturating_sub(1);
    if code > (DIA[i] >> 3) + (DIA[i] & 7) {
        return Some(c);
    }
    match DIA_CHAR[i] & !HI {
        0 => None,
        b => Some(char::from(b)),
    }
}

const HI: u8 = 0x80;
const DIA: [u16; 126] = [
    0, 1797, 1848, 1859, 1891, 1928, 1940, 1995, 2024, 2040, 2060, 2110, 2168, 2206, 2264, 2286,
    2344, 2383, 2472, 2488, 2516, 2596, 2668, 2732, 2782, 2842, 2894, 2954, 2984, 3000, 3028, 3336,
    3456, 3696, 3712, 3728, 3744, 3766, 3832, 3896, 3912, 3928, 3944, 3968, 4008, 4040, 4056, 4106,
    4138, 4170, 4202, 4234, 4266, 4296, 4312, 4344, 4408, 4424, 4442, 4472, 4488, 4504, 6148, 6198,
    6264, 6280, 6360, 6429, 6505, 6529, 61448, 61468, 61512, 61534, 61592, 61610, 61642, 61672,
    61688, 61704, 61726, 61784, 61800, 61816, 61836, 61880, 61896, 61914, 61948, 61998, 62062,
    62122, 62154, 62184, 62200, 62218, 62252, 62302, 62364, 62410, 62442, 62478, 62536, 62554,
    62584, 62604, 62640, 62648, 62656, 62664, 62730, 62766, 62830, 62890, 62924, 62974, 63032,
    63050, 63082, 63118, 63182, 63242, 63274, 63310, 63368, 63390,
];
const DIA_CHAR: [u8; 126] = [
    0,
    b'a',
    b'c',
    b'e',
    b'i',
    b'n',
    b'o',
    b'u',
    b'y',
    b'y',
    b'a',
    b'c',
    b'd',
    b'e',
    b'e',
    b'g',
    b'h',
    b'i',
    b'j',
    b'k',
    b'l',
    b'n',
    b'o',
    b'r',
    b's',
    b't',
    b'u',
    b'u',
    b'w',
    b'y',
    b'z',
    b'o',
    b'u',
    b'a',
    b'i',
    b'o',
    b'u',
    b'u' | HI,
    b'a' | HI,
    b'g',
    b'k',
    b'o',
    b'o' | HI,
    b'j',
    b'g',
    b'n',
    b'a' | HI,
    b'a',
    b'e',
    b'i',
    b'o',
    b'r',
    b'u',
    b's',
    b't',
    b'h',
    b'a',
    b'e',
    b'o' | HI,
    b'o',
    b'o' | HI,
    b'y',
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    b'a',
    b'b',
    b'c' | HI,
    b'd',
    b'd',
    b'e' | HI,
    b'e',
    b'e' | HI,
    b'f',
    b'g',
    b'h',
    b'h',
    b'i',
    b'i' | HI,
    b'k',
    b'l',
    b'l' | HI,
    b'l',
    b'm',
    b'n',
    b'o' | HI,
    b'p',
    b'r',
    b'r' | HI,
    b'r',
    b's',
    b's' | HI,
    b't',
    b'u',
    b'u' | HI,
    b'v',
    b'w',
    b'w',
    b'x',
    b'y',
    b'z',
    b'h',
    b't',
    b'w',
    b'y',
    b'a',
    b'a' | HI,
    b'a' | HI,
    b'a' | HI,
    b'e',
    b'e' | HI,
    b'e' | HI,
    b'i',
    b'o',
    b'o' | HI,
    b'o' | HI,
    b'o' | HI,
    b'u',
    b'u' | HI,
    b'u' | HI,
    b'y',
];

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

    /// [`fold`] 和 FTS5 `unicode61 remove_diacritics 2` 逐字一致：拉丁字母各区（含越南文那一区）和组合变音符号，
    /// 每个字两边垫上 a、b 单独写进一个同样分词的表，读回它被切成的词。被切成两个词的是分隔符，不比。
    #[test]
    fn fold_matches_the_fts_tokenizer() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "create virtual table t using fts5(x, tokenize = 'unicode61 remove_diacritics 2');
             create virtual table v using fts5vocab(t, 'instance');",
        )
        .unwrap();
        let chars: Vec<char> = (0x41..0x250)
            .chain(0x300..0x370)
            .chain(0x1E00..0x1F00)
            .filter_map(char::from_u32)
            .collect();
        {
            let mut ins = db
                .prepare("insert into t(rowid, x) values (?1, ?2)")
                .unwrap();
            for &c in &chars {
                ins.execute((i64::from(u32::from(c)), format!("a{c}b")))
                    .unwrap();
            }
        }
        let mut st = db.prepare("select doc, term from v").unwrap();
        let mut terms: std::collections::HashMap<i64, Vec<String>> = Default::default();
        for row in st
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get(1)?)))
            .unwrap()
        {
            let (doc, term) = row.unwrap();
            terms.entry(doc).or_default().push(term);
        }
        let mut wrong = Vec::new();
        for c in chars {
            let [term] = terms[&i64::from(u32::from(c))].as_slice() else {
                continue;
            };
            let ours: String = format!("a{c}b").chars().filter_map(fold).collect();
            if *term != ours {
                wrong.push(format!(
                    "U+{:04X} 索引 {term:?} 我们 {ours:?}",
                    u32::from(c)
                ));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
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
