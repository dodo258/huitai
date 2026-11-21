//! 解析三家会话文件里的 ISO 8601 / RFC 3339 时间戳（如 `2026-11-20T12:34:56.789Z`）。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 解析 `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)`；格式不对返回 `None`。
pub(crate) fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year = num(&b[0..4])?;
    let month = num(&b[5..7])?;
    let day = num(&b[8..10])?;
    let (hour, min, sec) = (num(&b[11..13])?, num(&b[14..16])?, num(&b[17..19])?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }

    let mut i = 19;
    let mut nanos: u64 = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            if i - start < 9 {
                nanos = nanos * 10 + u64::from(b[i] - b'0');
            }
            i += 1;
        }
        let digits = (i - start).min(9);
        if digits == 0 {
            return None;
        }
        nanos *= 10u64.pow(9 - digits as u32);
    }

    let offset_secs: i64 = match b.get(i..)? {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let h = num(&[*h1, *h2])?;
            let m = num(&[*m1, *m2])?;
            let v = (h * 3600 + m * 60) as i64;
            if *sign == b'+' { v } else { -v }
        }
        _ => return None,
    };

    let days = days_from_civil(year as i64, month, day);
    let secs = days * 86_400 + (hour * 3600 + min * 60 + sec) as i64 - offset_secs;
    let secs = u64::try_from(secs).ok()?;
    Some(UNIX_EPOCH + Duration::new(secs, nanos as u32))
}

/// 毫秒时间戳 → `SystemTime`；负数返回 `None`。
pub(crate) fn from_millis(ms: i64) -> Option<SystemTime> {
    u64::try_from(ms)
        .ok()
        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms))
}

fn num(b: &[u8]) -> Option<u32> {
    b.iter().try_fold(0u32, |acc, &c| {
        c.is_ascii_digit().then(|| acc * 10 + u32::from(c - b'0'))
    })
}

/// 公历日期 → 1970-01-01 起的天数（Howard Hinnant 的 days_from_civil）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(t: SystemTime) -> f64 {
        t.duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
    }

    #[test]
    fn parses_utc_with_millis() {
        let t = parse_rfc3339("2026-11-20T12:34:56.789Z").unwrap();
        assert_eq!(secs(t), 1_795_178_096.789);
    }

    #[test]
    fn parses_epoch_and_leap_day() {
        assert_eq!(secs(parse_rfc3339("1970-01-01T00:00:00Z").unwrap()), 0.0);
        assert_eq!(
            secs(parse_rfc3339("2024-02-29T00:00:00Z").unwrap()),
            1_709_164_800.0
        );
    }

    #[test]
    fn applies_offset() {
        let a = parse_rfc3339("2026-11-20T20:00:00+08:00").unwrap();
        let b = parse_rfc3339("2026-11-20T12:00:00Z").unwrap();
        assert_eq!(a, b);
        let c = parse_rfc3339("2026-11-20T07:00:00-05:00").unwrap();
        assert_eq!(c, b);
    }

    #[test]
    fn long_fraction_is_truncated_to_nanos() {
        let t = parse_rfc3339("2026-11-20T12:00:00.1234567891Z").unwrap();
        assert_eq!(
            t.duration_since(UNIX_EPOCH).unwrap().subsec_nanos(),
            123_456_789
        );
    }

    #[test]
    fn rejects_garbage() {
        for s in [
            "",
            "yesterday",
            "2026-13-01T00:00:00Z",
            "2026-11-20T12:00:00",
            "2026-11-20T12:00:00.Z",
            "2026-11-20T25:00:00Z",
            "2026-11-20T12:00:00+0800",
        ] {
            assert!(parse_rfc3339(s).is_none(), "{s}");
        }
    }

    #[test]
    fn millis() {
        assert_eq!(secs(from_millis(1500).unwrap()), 1.5);
        assert!(from_millis(-1).is_none());
    }
}
