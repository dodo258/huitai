use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// 一次额度读数：数值、来自哪个渠道、什么时候读的。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaReading {
    pub value: QuotaValue,
    /// 渠道名，例如「状态栏」「用量接口」「本机统计」。
    pub source: String,
    pub at: SystemTime,
}

impl QuotaReading {
    /// 所有渠道都取不到时的读数，界面显示「暂时取不到」。
    pub fn unavailable(at: SystemTime) -> Self {
        Self {
            value: QuotaValue::Unavailable,
            source: String::new(),
            at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaValue {
    /// 官方限额窗口（Claude 五小时/七天，Codex primary 等）。
    Windows(Vec<QuotaWindow>),
    /// 只能统计本机当天用掉的 token。
    TokensToday(u64),
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// 窗口名，例如「5 小时」「7 天」。
    pub label: String,
    /// 已用百分比，0–100。
    pub used_percent: f32,
    pub window_minutes: Option<u32>,
    pub resets_at: Option<SystemTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_has_no_source() {
        let r = QuotaReading::unavailable(SystemTime::UNIX_EPOCH);
        assert_eq!(r.value, QuotaValue::Unavailable);
        assert!(r.source.is_empty());
    }

    #[test]
    fn reading_round_trips_through_json() {
        let r = QuotaReading {
            value: QuotaValue::Windows(vec![QuotaWindow {
                label: "5 小时".to_owned(),
                used_percent: 42.5,
                window_minutes: Some(300),
                resets_at: None,
            }]),
            source: "状态栏".to_owned(),
            at: SystemTime::UNIX_EPOCH,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<QuotaReading>(&json).unwrap(), r);
    }
}
