//! 撞用量上限：认厂商写进会话记录的那条（不读屏幕），解析里面的重置时间。
//!
//! 字样和解析照抄 agentq：Claude 是 `internal/backend/claude/claude.go` 的 `limitRe`、`ParseResetTime`，
//! Codex 是 `internal/backend/codex/events.go` 的 `usageLimitRe`、`parseResetTime`。
//! 解析不出、或不在合理范围里，就当没有重置时间：不自动继续。

use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};
use regex::Regex;
use serde_json::Value;

use super::jsonl::str_field;
use super::time::parse_rfc3339;

/// 一条撞了用量上限的记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitHit {
    /// 可以接着做的时刻：重置时间再过 1 分钟。解析不出重置时间为 `None`。
    pub resume_at: Option<SystemTime>,
}

/// 到了提示的重置时刻再多等一分钟：本机时钟可能比服务端快一点。
const RESET_SLACK: Duration = Duration::from_secs(60);
/// 最长的窗口是一周：重置时间离写下记录超过 8 天就当解析错了。
const MAX_AHEAD: i64 = 8 * 24 * 3600;
const HOUR: i64 = 3600;

/// Claude Code 撞上限时说的话（官方 errors 文档「Usage limits」：「You've hit your session limit · resets 3:45pm」；
/// 旧版「Claude AI usage limit reached|<unix 秒>」）。
static CLAUDE_LIMIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)usage limit|rate limit|limit reached|hit your (\w+ ){0,3}limit|request rejected \(429\)|api error: 429")
        .expect("常量正则，编译得过")
});
/// 「resets 3:45pm」「resets Mon 12:00am」「resets Oct 14, 9am」「resets Sep 29 at 9pm (Asia/Shanghai)」。
static CLAUDE_RESETS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bresets\s+(?:(mon|tue|wed|thu|fri|sat|sun)[a-z]*\.?\s+|(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\.?\s+(\d{1,2}),?\s+(?:at\s+)?)?(\d{1,2})(?::(\d{2}))?\s*([ap]m)\b(?:\s*\(([^)]+)\))?")
        .expect("常量正则，编译得过")
});
static CLAUDE_EPOCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"limit reached\|(\d{10})\b").expect("常量正则，编译得过"));

/// Codex 撞上限的错误文字（codex-rs/protocol/src/error.rs）。撇号是弯的（You’ve），不写进正则。
static CODEX_LIMIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)hit your usage limit|usage limit reached|out of credits|hit your spend cap|quota exceeded|rate limit exceeded")
        .expect("常量正则，编译得过")
});
/// 「Try again at 3:05 PM.」或「try again at Mar 2nd, 2026 3:05 PM.」（本机时区，同一天不带日期）。
static CODEX_RESET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)try again at (?:([A-Z][a-z]{2} \d{1,2})(?:st|nd|rd|th), (\d{4}) )?(\d{1,2}:\d{2} [AP]M)")
        .expect("常量正则，编译得过")
});

/// Claude 会话文件的一行：是不是 CLI 撞上限后写的那条错误消息（`isApiErrorMessage` 的助手消息，
/// `apiErrorStatus` 429 或 `error` 为 `rate_limit`，或者文字是上限的说法）。子代理的不算。
pub(crate) fn claude(v: &Value) -> Option<LimitHit> {
    if str_field(v, "type") != Some("assistant")
        || v.get("isApiErrorMessage").and_then(Value::as_bool) != Some(true)
        || v.get("isSidechain").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let text = message_text(v);
    let status_429 = v.get("apiErrorStatus").and_then(Value::as_i64) == Some(429)
        || str_field(v, "error") == Some("rate_limit");
    if !status_429 && !CLAUDE_LIMIT.is_match(&text) {
        return None;
    }
    let at = str_field(v, "timestamp").and_then(parse_rfc3339);
    Some(LimitHit {
        resume_at: at.and_then(|at| claude_resume_at(&text, at, &local_tz())),
    })
}

fn message_text(v: &Value) -> String {
    match v.pointer("/message/content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|it| str_field(it, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Codex 会话文件的一行：回合结束（`task_complete`，或被打断的 `turn_aborted`）带着上限错误。
/// Codex 不把单独的错误事件写进会话文件，回合结束的 `error.message` 是唯一留下来的记录。
pub(crate) fn codex(v: &Value) -> Option<LimitHit> {
    let p = v.get("payload")?;
    if str_field(v, "type") != Some("event_msg")
        || !matches!(
            str_field(p, "type"),
            Some("task_complete" | "turn_complete" | "turn_aborted")
        )
    {
        return None;
    }
    let text = p.pointer("/error/message").and_then(Value::as_str)?;
    if !CODEX_LIMIT.is_match(text) {
        return None;
    }
    let at = str_field(v, "timestamp").and_then(parse_rfc3339);
    Some(LimitHit {
        resume_at: at.and_then(|at| codex_resume_at(text, at, &local_tz())),
    })
}

/// Claude 上限提示里的重置时间，`now` 是写下提示的时刻，`tz` 是提示没写时区时用的本机时区。
/// 返回重置后 1 分钟。解析不出、或不在 (now, now+8 天] 之内就是 `None`；
/// 提示里的时刻刚过去不到 1 小时（多半是写下提示时已跨过重置点）也是 `None`，免得多等一天或一周。
pub fn claude_resume_at(text: &str, now: SystemTime, tz: &TimeZone) -> Option<SystemTime> {
    let now = Timestamp::try_from(now).ok()?;
    let t = match CLAUDE_EPOCH.captures(text) {
        Some(c) => Timestamp::from_second(c[1].parse().ok()?).ok()?,
        None => claude_resets(text, now, tz)?,
    };
    let ahead = t.as_second() - now.as_second();
    if ahead <= 0 || ahead > MAX_AHEAD {
        return None;
    }
    Some(
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(u64::try_from(t.as_second()).ok()?)
            + RESET_SLACK,
    )
}

fn claude_resets(text: &str, now: Timestamp, local: &TimeZone) -> Option<Timestamp> {
    let c = CLAUDE_RESETS.captures(text)?;
    let tz = match c.get(7) {
        Some(z) => TimeZone::get(z.as_str().trim()).ok()?,
        None => local.clone(),
    };
    let hour: i8 = c[4].parse().ok()?;
    let minute: i8 = c.get(5).map_or(Some(0), |m| m.as_str().parse().ok())?;
    if !(1..=12).contains(&hour) || minute > 59 {
        return None;
    }
    let hour = hour % 12
        + if c[6].eq_ignore_ascii_case("pm") {
            12
        } else {
            0
        };
    let today = now.to_zoned_datetime(tz.clone()).date();
    let at = |d: Date| -> Option<Timestamp> {
        DateTime::new(d.year(), d.month(), d.day(), hour, minute, 0, 0)
            .ok()?
            .to_zoned_datetime(tz.clone())
            .ok()
            .map(|z| z.timestamp())
    };
    // 先按最近的那个算；刚过去不到 1 小时就不算，过得更久就往后推一个周期（一年、一周、一天）。
    let (first, next): (Date, Box<dyn Fn(Date) -> Option<Date>>) = if let Some(mon) = c.get(2) {
        // 「resets Oct 14, 9am」
        let month =
            "janfebmaraprmayjunjulaugsepoctnovdec".find(&mon.as_str().to_ascii_lowercase())? / 3
                + 1;
        let day: i8 = c[3].parse().ok()?;
        let first = Date::new(today.year(), month as i8, day).ok()?;
        (
            first,
            Box::new(|d: Date| Date::new(d.year() + 1, d.month(), d.day()).ok()),
        )
    } else if let Some(wd) = c.get(1) {
        // 「resets Mon 12:00am」
        let want = "sunmontuewedthufrisat".find(&wd.as_str().to_ascii_lowercase())? as i8 / 3;
        let have = today.weekday().to_sunday_zero_offset();
        let first = today
            .checked_add(i64::from((want - have + 7) % 7).days())
            .ok()?;
        (first, Box::new(|d: Date| d.checked_add(7.days()).ok()))
    } else {
        // 「resets 3:45pm」：今天的这个点已过就是明天
        (today, Box::new(|d: Date| d.tomorrow().ok()))
    };
    let t = at(first)?;
    if t > now {
        return Some(t);
    }
    if now.as_second() - t.as_second() <= HOUR {
        return None;
    }
    at(next(first)?)
}

/// Codex 错误文字里的重置时间，`now` 是写下记录的时刻，钟点按本机时区 `tz`（Codex 按它所在机器的时区写）。
/// 只有钟点的算当天，比 `now` 早 1 小时以上（跨了午夜）算第二天；带日期的不在 [now-1 小时, now+8 天] 里就是 `None`。
/// 返回提示的时刻再过 1 分钟。
pub fn codex_resume_at(text: &str, now: SystemTime, tz: &TimeZone) -> Option<SystemTime> {
    let now = Timestamp::try_from(now).ok()?;
    let c = CODEX_RESET.captures(text)?;
    let (hour, minute) = clock_12h(&c[3])?;
    let t = match (c.get(1), c.get(2)) {
        (Some(md), Some(year)) => {
            let (mon, day) = md.as_str().split_once(' ')?;
            let month =
                "janfebmaraprmayjunjulaugsepoctnovdec".find(&mon.to_ascii_lowercase())? / 3 + 1;
            let date =
                Date::new(year.as_str().parse().ok()?, month as i8, day.parse().ok()?).ok()?;
            let t = in_zone(date, hour, minute, tz)?;
            let d = t.as_second() - now.as_second();
            if !(-HOUR..=MAX_AHEAD).contains(&d) {
                return None;
            }
            t
        }
        _ => {
            let today = now.to_zoned_datetime(tz.clone()).date();
            let t = in_zone(today, hour, minute, tz)?;
            if t.as_second() < now.as_second() - HOUR {
                in_zone(today.tomorrow().ok()?, hour, minute, tz)?
            } else {
                t
            }
        }
    };
    Some(
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(u64::try_from(t.as_second()).ok()?)
            + RESET_SLACK,
    )
}

/// 「3:05 PM」→ (15, 5)。
fn clock_12h(s: &str) -> Option<(i8, i8)> {
    let (hm, ampm) = s.split_once(' ')?;
    let (h, m) = hm.split_once(':')?;
    let (h, m): (i8, i8) = (h.parse().ok()?, m.parse().ok()?);
    if !(1..=12).contains(&h) || m > 59 {
        return None;
    }
    Some((
        h % 12
            + if ampm.eq_ignore_ascii_case("pm") {
                12
            } else {
                0
            },
        m,
    ))
}

fn in_zone(d: Date, hour: i8, minute: i8, tz: &TimeZone) -> Option<Timestamp> {
    DateTime::new(d.year(), d.month(), d.day(), hour, minute, 0, 0)
        .ok()?
        .to_zoned_datetime(tz.clone())
        .ok()
        .map(|z| z.timestamp())
}

/// 本机时区（读不到按 UTC）。
pub fn local_tz() -> TimeZone {
    TimeZone::try_system().unwrap_or(TimeZone::UTC)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 东八区（agentq 测试用的 CST）。
    fn cst() -> TimeZone {
        TimeZone::fixed(jiff::tz::offset(8))
    }

    fn at(tz: &TimeZone, y: i16, mo: i8, d: i8, h: i8, mi: i8) -> SystemTime {
        let t = in_zone(Date::new(y, mo, d).unwrap(), h, mi, tz).unwrap();
        SystemTime::UNIX_EPOCH + Duration::from_secs(t.as_second() as u64)
    }

    // 照 agentq internal/backend/claude/claude_test.go TestParseResetTime。
    #[test]
    fn claude_reset_times() {
        let tz = cst();
        let now = at(&tz, 2026, 10, 10, 15, 30); // 周六
        let after = |d: i8, h: i8, m: i8| at(&tz, 2026, 10, d, h, m + 1); // 重置后 1 分钟
        let cases = [
            (
                "You've hit your session limit · resets 3:45pm",
                after(10, 15, 45),
            ),
            ("You've hit your limit · resets 5pm", after(10, 17, 0)),
            // 今天 2 点已过 1 个多小时 → 明天
            ("You've hit your Opus limit · resets 2pm", after(11, 14, 0)),
            (
                "You've hit your weekly limit · resets Mon 12:00am",
                after(12, 0, 0),
            ),
            // 今天就是周六且已过好几个小时 → 下周
            (
                "You've hit your weekly limit · resets Sat 9am",
                after(17, 9, 0),
            ),
            ("5-hour limit reached ∙ resets Oct 12, 9am", after(12, 9, 0)),
            (
                "You've hit your limit · resets 8pm (UTC)",
                at(&tz, 2026, 10, 11, 4, 1),
            ),
            (
                "You've hit your weekly limit · resets Oct 14 at 9pm (Asia/Shanghai)",
                after(14, 21, 0),
            ),
            (
                "Claude AI usage limit reached|1791624600",
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_624_600 + 60),
            ),
        ];
        for (text, want) in cases {
            assert_eq!(claude_resume_at(text, now, &tz), Some(want), "{text}");
        }
        for text in [
            "You've hit your session limit",
            "Claude AI usage limit reached|1760000000", // 已经过去的时间
            "resets 13pm",
            "resets 5pm (Not/AZone)",
            "resets Oct 9, 9am", // 今年的已过 → 明年，超过 8 天不采用
            // 刚过去不到 1 小时：多半已经重置，不推到明天/下周
            "You've hit your session limit · resets 3pm",
            "You've hit your weekly limit · resets Sat 2:45pm",
            "resets Oct 10, 3:00pm",
        ] {
            assert_eq!(claude_resume_at(text, now, &tz), None, "{text}");
        }
    }

    // 照 agentq internal/backend/codex/codex_test.go TestParseResetTimeEdges。
    #[test]
    fn codex_reset_times() {
        let tz = cst();
        let now = at(&tz, 2027, 3, 1, 23, 50);
        assert_eq!(
            codex_resume_at("Try again at 12:10 AM.", now, &tz),
            Some(at(&tz, 2027, 3, 2, 0, 11)),
            "跨午夜算明天"
        );
        assert_eq!(
            codex_resume_at("or try again at 11:30 PM.", now, &tz),
            Some(at(&tz, 2027, 3, 1, 23, 31)),
            "刚过去不到一小时算今天"
        );
        assert_eq!(
            codex_resume_at(
                "You’ve hit your usage limit. Try again at Mar 3rd, 2027 3:05 PM.",
                now,
                &tz
            ),
            Some(at(&tz, 2027, 3, 3, 15, 6))
        );
        for text in [
            "Try again later.",
            "try again at 25:99 PM",
            "Try again at Mar 2nd, 2020 3:05 PM.",
            "Try again at Mar 2nd, 2029 3:05 PM.",
        ] {
            assert_eq!(codex_resume_at(text, now, &tz), None, "{text}");
        }
    }
}
