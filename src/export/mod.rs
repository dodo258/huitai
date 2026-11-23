//! 对话导出成 Markdown：整段对话（[`Source::load_transcript`]，和查看视图同一份，各家不另写解析）排成一个 `.md`。
//! 文件头写标题、厂商、账号、项目、时间；每条消息一节标角色，工具调用每次折成一行摘要，工具返回不导出。
//! 只读厂商数据，写的只有导出的那个文件。对外接口：[`render`]（[`load`] 加 [`markdown`]）、[`file_stem`]、[`save`]、[`visible_controls`]。

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::{Account, Session};
use crate::index::Entry;
use crate::sources::{Part, Role, Source, Transcript};

/// 文件头里写的对话信息（调用方查好显示名，这里只排版）。
#[derive(Debug, Clone, Copy)]
pub struct Meta<'a> {
    /// 列表里显示的标题（会话台里改过的名字优先）。
    pub title: &'a str,
    /// 账号显示名；默认账号是 `None`，不写这一行。
    pub account: Option<&'a str>,
    /// 项目显示名（会话台里起的名字，没起就是目录名）。
    pub project: &'a str,
    pub session: &'a Session,
}

/// 列表里的一条（`project` 是项目显示名，`accounts` 用来查账号显示名）读出全文、按本机时区排成 Markdown。
pub fn render(
    src: &dyn Source,
    entry: &Entry,
    accounts: &[Account],
    project: &str,
) -> io::Result<String> {
    let s = &entry.session;
    let tr = load(src, s)?;
    let account = accounts
        .iter()
        .find(|a| !a.is_default() && a.vendor == s.vendor && a.key == s.account)
        .map(|a| a.name.as_str());
    let meta = Meta {
        title: entry.title(),
        account,
        project,
        session: s,
    };
    Ok(markdown(&meta, &tr, &local_offset))
}

/// 读一条对话的全文。读不了、或一条消息和工具调用都没有时报清楚的错。
pub fn load(src: &dyn Source, s: &Session) -> io::Result<Transcript> {
    let tr = src.load_transcript(s).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("读不了这条对话（{}）：{e}", s.path.display()),
        )
    })?;
    if tr.parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "这条对话还没有内容，没有可导出的",
        ));
    }
    Ok(tr)
}

/// 排成 Markdown。`offset` 给出某个 Unix 秒所在时刻本地时区相对 UTC 的偏移（秒），见 [`local_offset`]。
pub fn markdown(meta: &Meta, tr: &Transcript, offset: &dyn Fn(i64) -> i64) -> String {
    let s = meta.session;
    let vendor = s.vendor;
    let mut out = format!("# {}\n\n", one_line(meta.title));
    out.push_str(&format!("- 厂商：{}\n", vendor.name()));
    if let Some(a) = meta.account {
        out.push_str(&format!("- 账号：{a}\n"));
    }
    out.push_str(&format!(
        "- 项目：{}（{}）\n",
        one_line(meta.project),
        s.cwd.display()
    ));
    let end = unix(s.last_active);
    let start = tr.parts.iter().find_map(|p| match p {
        Part::Message(m) => m.at.map(unix),
        _ => None,
    });
    let zone = zone_label(offset(end));
    let last = local_minute(end, offset(end));
    // 同一天的结束只写几点几分；同一分钟只写一个。
    let time = match start.map(|a| local_minute(a, offset(a))) {
        Some(first) if first < last => match last.strip_prefix(&first[..11]) {
            Some(clock) => format!("{first} – {clock}"),
            None => format!("{first} – {last}"),
        },
        _ => last,
    };
    out.push_str(&format!("- 时间：{time}（{zone}）\n"));
    out.push_str(&format!("- 对话 id：{}\n", code(s.id.native())));
    if tr.truncated {
        // 各家截断的依据不同（文件按字节、数据库按条数），读出来的 Transcript 只说截没截，说法不提具体是哪种。
        out.push_str("\n> 这条对话太长，只导出了最近的一段，更早的消息没有导出。\n");
    }
    let mut in_tools = false;
    for p in &tr.parts {
        match p {
            Part::Message(m) => {
                let who = match m.role {
                    Role::User => "你",
                    Role::Assistant => vendor.name(),
                };
                out.push_str(&format!("\n## {who}"));
                if let Some(at) = m.at.map(unix) {
                    out.push_str(&format!(" · {}", local_minute(at, offset(at))));
                }
                out.push_str("\n\n");
                out.push_str(m.text.trim_end());
                out.push('\n');
                in_tools = false;
            }
            Part::ToolCall { name, input } => {
                if !in_tools {
                    out.push('\n');
                    in_tools = true;
                }
                out.push_str(&tool_line(name, input));
            }
            Part::ToolResult { .. } => {}
        }
    }
    out
}

/// 一次工具调用：`- 工具 Bash：`cargo test``。
fn tool_line(name: &str, input: &str) -> String {
    let input = input.trim();
    // 没有参数的调用，摘要是空对象。
    if input.is_empty() || input == "{}" {
        format!("- 工具 {}\n", one_line(name))
    } else {
        format!("- 工具 {}：{}\n", one_line(name), code(input))
    }
}

/// 行内代码：用比文字里最长一串反引号多一个的反引号包起来，开头结尾是反引号时补空格。
fn code(text: &str) -> String {
    let text = one_line(text);
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// 换行和制表换成空格，首尾空白去掉。
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 默认文件名（不含序号和扩展名）：`<标题>-<最后时间的本地日期>`。标题里文件名不能用或不好用的字符换成 `-`，
/// 最长 60 个字，去掉开头的 `.`（不变成隐藏文件），空了用「对话」。
pub fn file_stem(title: &str, last_active: SystemTime, offset: &dyn Fn(i64) -> i64) -> String {
    let cleaned: String = one_line(title)
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '-'
            } else {
                c
            }
        })
        .take(60)
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    let name = if cleaned.is_empty() {
        "对话"
    } else {
        cleaned
    };
    let at = unix(last_active);
    format!("{name}-{}", local_date(at, offset(at)))
}

/// 存进 `dir`（不存在就建）：`<stem>.md`，重名依次试 `<stem>-2.md`、`<stem>-3.md`……不覆盖已有文件。返回存到的路径。
pub fn save(dir: &Path, stem: &str, text: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .map_err(|e| io::Error::new(e.kind(), format!("建不了 {}：{e}", dir.display())))?;
    for n in 1..=999 {
        let name = if n == 1 {
            format!("{stem}.md")
        } else {
            format!("{stem}-{n}.md")
        };
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(text.as_bytes())
                    .map_err(|e| io::Error::new(e.kind(), format!("{}：{e}", path.display())))?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(io::Error::new(e.kind(), format!("{}：{e}", path.display())));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{} 里同名的文件太多（{stem}…）", dir.display()),
    ))
}

/// 打到终端用：换行、制表以外的控制字符换成看得见的写法（C0 和 DEL 用 `^[`、`^?` 这种脱字符写法，C1 写成 `<U+009B>`），
/// 正文里存着的终端转义序列不会被终端执行。存文件、进管道的不用它，保持原样。
pub fn visible_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            '\0'..='\x1f' => {
                out.push('^');
                out.push(char::from(c as u8 ^ 0x40));
            }
            '\x7f' => out.push_str("^?"),
            '\u{80}'..='\u{9f}' => out.push_str(&format!("<U+{:04X}>", u32::from(c))),
            _ => out.push(c),
        }
    }
    out
}

/// 默认存到哪：`~/Downloads`。
pub fn downloads(home: &Path) -> PathBuf {
    home.join("Downloads")
}

/// 本机时区在 `secs`（Unix 秒）那一刻相对 UTC 的偏移（秒，含夏令时）；取不到按 UTC。
pub fn local_offset(secs: i64) -> i64 {
    let t: libc::time_t = secs;
    // SAFETY: libc::tm 是纯数据结构，全零是合法值。
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: 两个指针都指向本地变量，localtime_r 只写 tm；失败时返回空指针。
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff
}

/// 时刻 → Unix 秒（1970 年以前是负数）。
pub fn unix(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// `UTC+08:00`。
fn zone_label(offset: i64) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let m = offset.abs() / 60;
    format!("UTC{sign}{:02}:{:02}", m / 60, m % 60)
}

/// 本地日期 `2026-11-20`。
fn local_date(secs: i64, offset: i64) -> String {
    let (y, m, d) = civil_from_days((secs + offset).div_euclid(86400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// 本地时间 `2026-11-20 10:00`。`offset` 见 [`local_offset`]。
pub fn local_minute(secs: i64, offset: i64) -> String {
    let in_day = (secs + offset).rem_euclid(86400);
    format!(
        "{} {:02}:{:02}",
        local_date(secs, offset),
        in_day / 3600,
        in_day % 3600 / 60
    )
}

/// 带时区的本地时间 `2026-11-20T10:00:05+08:00`（RFC 3339）。`offset` 见 [`local_offset`]。
pub fn local_rfc3339(secs: i64, offset: i64) -> String {
    let in_day = (secs + offset).rem_euclid(86400);
    let sign = if offset < 0 { '-' } else { '+' };
    let m = offset.abs() / 60;
    format!(
        "{}T{:02}:{:02}:{:02}{sign}{:02}:{:02}",
        local_date(secs, offset),
        in_day / 3600,
        in_day % 3600 / 60,
        in_day % 60,
        m / 60,
        m % 60
    )
}

/// 1970-01-01 起的天数 → 公历年月日（Howard Hinnant 的算法）。
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_zones() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // 2024-02-29 是第 19782 天。
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(local_minute(0, 8 * 3600), "1970-01-01 08:00");
        assert_eq!(local_minute(0, -3600), "1969-12-31 23:00");
        assert_eq!(zone_label(8 * 3600), "UTC+08:00");
        assert_eq!(local_rfc3339(5, 8 * 3600), "1970-01-01T08:00:05+08:00");
        assert_eq!(local_rfc3339(0, -5400), "1969-12-31T22:30:00-01:30");
        assert_eq!(zone_label(-(3 * 3600 + 1800)), "UTC-03:30");
        assert_eq!(zone_label(0), "UTC+00:00");
    }

    #[test]
    fn code_spans_survive_backticks() {
        assert_eq!(code("ls -la"), "`ls -la`");
        assert_eq!(code("echo `date`"), "`` echo `date` ``");
        assert_eq!(code("a ``b`` c"), "```a ``b`` c```");
        assert_eq!(code("多行\n命令"), "`多行 命令`");
    }

    #[test]
    fn controls_become_visible_but_newlines_and_tabs_stay() {
        assert_eq!(visible_controls("a\tb\nc"), "a\tb\nc");
        assert_eq!(
            visible_controls("\x1b]2;标题\x07\r\x00\x7f\u{9b}2J"),
            "^[]2;标题^G^M^@^?<U+009B>2J"
        );
    }

    #[test]
    fn file_stems_are_safe() {
        let utc = |_| 0;
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_795_000_000);
        assert_eq!(file_stem("修登录页", t, &utc), "修登录页-2026-11-18");
        assert_eq!(
            file_stem("a/b: c?\n第二行", t, &utc),
            "a-b- c- 第二行-2026-11-18"
        );
        assert_eq!(file_stem("..隐藏", t, &utc), "隐藏-2026-11-18");
        assert_eq!(file_stem("  ", t, &utc), "对话-2026-11-18");
        assert_eq!(file_stem(&"长".repeat(100), t, &utc).chars().count(), 71);
    }
}
