//! 文字排版小工具：按显示宽度截断和折行（中文占两格）、相对时间、`~` 缩写路径。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::index::ProjectNote;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::sources::Command;

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// 截到 `max` 格以内，截掉了就以 `…` 结尾。控制字符换成空格。
pub fn truncate(s: &str, max: usize) -> String {
    let clean: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if width(&clean) <= max {
        return clean;
    }
    let mut out = String::new();
    let mut used = 0;
    for c in clean.chars() {
        let w = char_width(c);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    if max > 0 {
        out.push('…');
    }
    out
}

/// 截到 `max` 格以内，截掉的是中间（换成 `…`），头尾都留着：路径的开头和最后一段都要看得到。
pub fn truncate_middle(s: &str, max: usize) -> String {
    let clean = truncate(s, usize::MAX);
    if width(&clean) <= max {
        return clean;
    }
    if max == 0 {
        return String::new();
    }
    let keep = max - 1;
    let tail_w = keep.div_ceil(2);
    let take = |chars: &mut dyn Iterator<Item = char>, room: usize| {
        let mut out = Vec::new();
        let mut used = 0;
        for c in chars {
            let w = char_width(c);
            if used + w > room {
                break;
            }
            out.push(c);
            used += w;
        }
        (out, used)
    };
    let (tail, used) = take(&mut clean.chars().rev(), tail_w);
    let (head, _) = take(&mut clean.chars(), keep - used);
    head.into_iter()
        .chain(['…'])
        .chain(tail.into_iter().rev())
        .collect()
}

/// 按 `max` 格折行；原文的换行保留，连续空行压成一行。
pub fn wrap(s: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut lines = Vec::new();
    for raw in s.lines() {
        let raw = raw.trim_end();
        if raw.is_empty() {
            if lines.last().is_some_and(|l: &String| !l.is_empty()) {
                lines.push(String::new());
            }
            continue;
        }
        let mut line = String::new();
        let mut used = 0;
        for c in raw.chars() {
            let c = if c == '\t' { ' ' } else { c };
            if c.is_control() {
                continue;
            }
            let w = char_width(c);
            if used + w > max {
                let carry = word_tail(&line, c);
                line.truncate(line.len() - carry.len());
                lines.push(std::mem::take(&mut line).trim_end().to_owned());
                used = width(&carry);
                line = carry;
            }
            line.push(c);
            used += w;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// 折行时要带到下一行的英文单词残段：`next` 和行尾都是 ASCII 字母数字时，取行尾这个单词
/// （整行就是一个单词时不带，硬断）。中文随处可断，不带。
fn word_tail(line: &str, next: char) -> String {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    if !word(next) || !line.chars().last().is_some_and(word) {
        return String::new();
    }
    let start = line
        .char_indices()
        .rev()
        .find(|(_, c)| !word(*c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    if start == 0 {
        return String::new();
    }
    line[start..].to_owned()
}

/// 「刚刚」「3 分钟前」「2 小时前」「昨天」「5 天前」「3 个月前」「2 年前」。
pub fn ago(then: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(then).unwrap_or(Duration::ZERO).as_secs();
    let (m, h, d) = (secs / 60, secs / 3600, secs / 86_400);
    match () {
        _ if m < 1 => "刚刚".to_owned(),
        _ if h < 1 => format!("{m} 分钟前"),
        _ if d < 1 => format!("{h} 小时前"),
        _ if d < 2 => "昨天".to_owned(),
        _ if d < 30 => format!("{d} 天前"),
        _ if d < 365 => format!("{} 个月前", d / 30),
        _ => format!("{} 年前", d / 365),
    }
}

/// 在跑了多久：「45 秒」「4 分 12 秒」「1 小时 3 分」。
pub fn clock(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s} 秒"),
        60..3600 => format!("{} 分 {} 秒", s / 60, s % 60),
        _ => format!("{} 小时 {} 分", s / 3600, s % 3600 / 60),
    }
}

/// 用了多久、等了多久：「45 秒」「6 分钟」「1 小时 3 分」。
pub fn span(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s} 秒"),
        60..3600 => format!("{} 分钟", s / 60),
        _ => format!("{} 小时 {} 分", s / 3600, s % 3600 / 60),
    }
}

/// 额度离重置还有多久：「1 分钟内重置」「20 分钟后重置」「1 小时 20 分后重置」「3 天 5 小时后重置」。
pub fn resets_in(d: Duration) -> String {
    let s = d.as_secs();
    let (d, h, m) = (s / 86_400, s % 86_400 / 3600, s % 3600 / 60);
    match () {
        _ if s < 60 => "1 分钟内重置".to_owned(),
        _ if s < 3600 => format!("{m} 分钟后重置"),
        _ if d == 0 && m == 0 => format!("{h} 小时后重置"),
        _ if d == 0 => format!("{h} 小时 {m} 分后重置"),
        _ if h == 0 => format!("{d} 天后重置"),
        _ => format!("{d} 天 {h} 小时后重置"),
    }
}

/// 家目录开头的换成 `~`。
pub fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => {
            if rest.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~/{}", rest.display())
            }
        }
        _ => path.display().to_string(),
    }
}

/// 项目显示名：目录最后一段。
pub fn project_name(cwd: &Path) -> String {
    cwd.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            if cwd.as_os_str().is_empty() {
                "未知目录".to_owned()
            } else {
                cwd.display().to_string()
            }
        })
}

/// 项目在列表、预览、标签页标题里叫什么：会话台里起的显示名，没起就是目录名。
pub fn project_label(cwd: &Path, projects: &HashMap<PathBuf, ProjectNote>) -> String {
    projects
        .get(cwd)
        .and_then(|n| n.name.clone())
        .unwrap_or_else(|| project_name(cwd))
}

/// `c` 复制的恢复命令：`cd '<目录>' && <环境变量><恢复命令>`，粘进任何终端都能接着聊。目录总是加单引号（含空格、中文也不出错）；
/// `env` 是 [`crate::sources::env_prefix`] 给的，数据目录没改过就是空串。
pub fn resume_line(cmd: &Command, env: &str) -> String {
    let dir = cmd.cwd.to_string_lossy().replace('\'', r"'\''");
    format!("cd '{dir}' && {env}{}", cmd.shell_line())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_and_span() {
        let d = Duration::from_secs;
        assert_eq!(clock(d(45)), "45 秒");
        assert_eq!(clock(d(252)), "4 分 12 秒");
        assert_eq!(clock(d(3780)), "1 小时 3 分");
        assert_eq!(span(d(360)), "6 分钟");
        assert_eq!(span(d(59)), "59 秒");
        assert_eq!(span(d(7200)), "2 小时 0 分");
    }

    #[test]
    fn reset_countdown() {
        let d = Duration::from_secs;
        assert_eq!(resets_in(d(0)), "1 分钟内重置");
        assert_eq!(resets_in(d(59)), "1 分钟内重置");
        assert_eq!(resets_in(d(20 * 60 + 30)), "20 分钟后重置");
        assert_eq!(resets_in(d(80 * 60)), "1 小时 20 分后重置");
        assert_eq!(resets_in(d(2 * 3600)), "2 小时后重置");
        assert_eq!(
            resets_in(d(3 * 86_400 + 5 * 3600 + 59)),
            "3 天 5 小时后重置"
        );
        assert_eq!(resets_in(d(6 * 86_400 + 40 * 60)), "6 天后重置");
    }

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate("购物车结算", 6), "购物…");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "ab…");
        assert_eq!(truncate("a\nb", 5), "a b");
    }

    #[test]
    fn truncates_in_the_middle() {
        assert_eq!(truncate_middle("~/projects/shop", 20), "~/projects/shop");
        assert_eq!(truncate_middle("~/projects/shop", 9), "~/pr…shop");
        assert_eq!(truncate_middle("~/项目/购物车", 8), "~/…物车");
        assert_eq!(truncate_middle("abc", 1), "…");
        assert_eq!(truncate_middle("abc", 0), "");
        assert!(width(&truncate_middle("~/很长的中文目录/子目录", 9)) <= 9);
    }

    #[test]
    fn wraps_wide_chars() {
        assert_eq!(wrap("一二三四五", 4), vec!["一二", "三四", "五"]);
        assert_eq!(wrap("a\n\n\nb\n", 10), vec!["a", "", "b"]);
        // 英文单词不从中间断。
        assert_eq!(
            wrap("我改了 useCheckout 里", 12),
            vec!["我改了", "useCheckout", "里"]
        );
        assert_eq!(wrap("abcdefgh", 4), vec!["abcd", "efgh"]);
    }

    #[test]
    fn relative_times() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000);
        let before = |s| now - Duration::from_secs(s);
        assert_eq!(ago(before(10), now), "刚刚");
        assert_eq!(ago(before(180), now), "3 分钟前");
        assert_eq!(ago(before(7200), now), "2 小时前");
        assert_eq!(ago(before(90_000), now), "昨天");
        assert_eq!(ago(before(5 * 86_400), now), "5 天前");
        assert_eq!(ago(now + Duration::from_secs(5), now), "刚刚");
    }

    #[test]
    fn tilde_paths() {
        let home = PathBuf::from("/Users/demo");
        assert_eq!(
            tilde(Path::new("/Users/demo/projects/shop"), &home),
            "~/projects/shop"
        );
        assert_eq!(tilde(Path::new("/Users/demo"), &home), "~");
        assert_eq!(tilde(Path::new("/opt/x"), &home), "/opt/x");
        assert_eq!(project_name(Path::new("/a/shop")), "shop");
        assert_eq!(project_name(Path::new("")), "未知目录");
    }
}
