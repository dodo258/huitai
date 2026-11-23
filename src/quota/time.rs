//! 时间小工具：RFC 3339 解析、Unix 秒与 SystemTime 互转、本地今天零点。

use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::core::exec;

pub fn to_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

pub fn from_secs(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

/// JSON 里的时间：数字当 Unix 秒，字符串当 RFC 3339。
pub fn value_secs(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => parse_rfc3339(s),
        _ => None,
    }
}

/// 解析 `2026-11-20T14:29:53.617Z`、`2026-11-20T19:19:59.905540+00:00` 这类时间，返回 Unix 秒（舍去小数）。
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 && matches!(&rest[..1], "+" | "-") && &rest[3..4] == ":" => {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            sign * (rest[1..3].parse::<i64>().ok()? * 3600 + rest[4..6].parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86400 + hour * 3600 + min * 60 + sec - offset)
}

/// 公历日期到 1970-01-01 起的天数（Howard Hinnant 的算法）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `now` 所在本地日的零点（Unix 秒）。`utc_offset_secs` 是本地时区相对 UTC 的偏移。
pub fn local_midnight(now: SystemTime, utc_offset_secs: i64) -> i64 {
    let local = to_secs(now) + utc_offset_secs;
    local - local.rem_euclid(86400) - utc_offset_secs
}

/// 本机时区偏移（秒）。标准库取不到时区，用 `date +%z` 读一次；读不到按 UTC。
pub fn local_utc_offset() -> i64 {
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        exec::run("date", &["+%z"], None, Duration::from_secs(2))
            .ok()
            .and_then(|out| parse_offset(out.trim()))
            .unwrap_or(0)
    })
}

/// `+0800` → 28800。
fn parse_offset(s: &str) -> Option<i64> {
    if s.len() != 5 {
        return None;
    }
    let sign = match &s[..1] {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    Some(sign * (s[1..3].parse::<i64>().ok()? * 3600 + s[3..5].parse::<i64>().ok()? * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_rfc3339_forms() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2026-11-20T14:29:53.617Z"),
            Some(1_795_184_993)
        );
        assert_eq!(
            parse_rfc3339("2026-11-20T19:19:59.905540+00:00"),
            Some(1_795_202_399)
        );
        assert_eq!(
            parse_rfc3339("2026-11-21T03:19:59+08:00"),
            Some(1_795_202_399)
        );
    }

    #[test]
    fn rejects_garbage() {
        for s in [
            "",
            "yesterday",
            "2026-11-20",
            "2026-13-01T00:00:00Z",
            "2026-11-20T10:00:00",
        ] {
            assert_eq!(parse_rfc3339(s), None, "{s}");
        }
    }

    #[test]
    fn value_secs_accepts_numbers_and_strings() {
        assert_eq!(value_secs(&serde_json::json!(1738425600)), Some(1738425600));
        assert_eq!(value_secs(&serde_json::json!(1.5)), Some(1));
        assert_eq!(
            value_secs(&serde_json::json!("1970-01-01T00:01:00Z")),
            Some(60)
        );
        assert_eq!(value_secs(&serde_json::json!(null)), None);
    }

    #[test]
    fn midnight_respects_offset() {
        // 2026-11-20T14:29:53Z = 北京时间 22:29:53，本地零点 = 2026-11-19T16:00:00Z。
        let now = from_secs(1_795_184_993);
        assert_eq!(local_midnight(now, 8 * 3600), 1_795_104_000);
        assert_eq!(local_midnight(now, 0), 1_795_104_000 + 8 * 3600);
        assert_eq!(parse_offset("+0800"), Some(28800));
        assert_eq!(parse_offset("-0530"), Some(-19800));
        assert_eq!(parse_offset("CST"), None);
    }
}
