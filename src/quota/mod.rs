//! 多渠道额度读取与降级。对外接口：`quota(vendor) -> QuotaReading`。
//!
//! 每家按 `rules.json` 里的顺序试渠道，前一个取不到或格式不认识就换下一个；
//! 全部失败返回 `QuotaValue::Unavailable`（界面显示「暂时取不到」），不报错。
//!
//! API 余额和订阅额度（DeepSeek、Moonshot、OpenRouter、Kimi Code）走 `balance` 渠道：
//! `balances(accounts) -> BalanceReport`，密钥只读自 pi / Hermes 的配置和环境变量（`keys`）。

mod balance;
mod claude;
mod codex;
mod curl;
mod keys;
mod rules;
mod time;
mod tokens;

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::core::{Account, Config, QuotaReading, QuotaValue, QuotaWindow, Vendor, VendorDirs};
use rules::{Channel, Minutes, Rules, WindowRule};

pub use balance::{
    Balance, BalanceReader, BalanceReport, BalanceRow, BalanceValue, CreditPeriod, HttpFetch,
    Money, Provider, balances, defaults,
};
pub use claude::{UsageFetch, UsageRequest, usage_command};
pub use keys::{Found, FoundKey, KeyOrigin, OriginAccount, Skipped, default_provider, find};

/// 读当前机器上某个账号的额度。
pub fn quota(account: &Account) -> QuotaReading {
    match QuotaReader::system_for(account) {
        Some(reader) => reader.read(account.vendor),
        None => QuotaReading::unavailable(SystemTime::now()),
    }
}

/// 这家有没有额度渠道（没有的顶栏不写额度）。
pub fn supported(vendor: Vendor) -> bool {
    !Rules::builtin().channels(vendor).is_empty()
}

/// 给会话台启动的 Claude 加的 `--settings` 参数值（JSON 字符串）。
/// 注入的状态栏命令把官方给状态栏的数据写进缓存（这个账号自己的那份），再原样调用用户原有的状态栏命令，不覆盖。
/// `cwd` 是 Claude 的项目目录（用来找项目级的原有状态栏设置）。
pub fn claude_settings(account: &Account, cwd: &Path) -> Option<String> {
    QuotaReader::system_for(account).map(|r| r.claude_settings(cwd))
}

/// 这个账号的额度缓存放哪：默认账号是会话台数据目录本身（老用户的缓存不动），别的账号在 `accounts/<短名>`。
pub fn cache_dir(data_dir: &Path, account: &Account) -> PathBuf {
    if account.is_default() {
        data_dir.to_path_buf()
    } else {
        data_dir.join("accounts").join(&account.key)
    }
}

/// 一个渠道试读的结果，给 `ai doctor` 报告每个渠道能不能用。
#[derive(Debug)]
pub struct ChannelReport {
    pub source: &'static str,
    pub result: Result<QuotaReading, String>,
}

/// 读额度需要的环境。`quota()` 用本机环境；测试用 `new` 指定目录和时间。
pub struct QuotaReader {
    home: PathBuf,
    /// 三家的数据目录；规则里 `.claude/…` 这类相对家目录的路径经它换到实际位置。
    dirs: VendorDirs,
    data_dir: PathBuf,
    now: SystemTime,
    utc_offset_secs: i64,
    rules: &'static Rules,
    fetch: Box<UsageFetch>,
}

impl QuotaReader {
    /// 本机环境（各家目录照环境变量），给 `ai doctor` 一次试遍各家渠道；额度缓存用 Claude 主账号那份。
    pub fn system() -> Option<Self> {
        let home = PathBuf::from(env::var_os("HOME").filter(|h| !h.is_empty())?);
        let dirs = VendorDirs::from_env(&home);
        let claude = Account::new(
            Vendor::Claude,
            "",
            dirs.dir(Vendor::Claude).to_path_buf(),
            &home,
        );
        let data_dir = cache_dir(&Config::data_dir()?, &claude);
        Some(
            Self::new(home, data_dir, SystemTime::now(), time::local_utc_offset())
                .with_fetch(Box::new(claude::fetch_usage))
                .with_dirs(dirs),
        )
    }

    /// 本机环境下读这个账号：家目录、这个账号的额度缓存目录（[`cache_dir`]）、当前时间和时区、真实的钥匙串 + 联网。
    /// 这家的数据目录换成账号的目录，别的家照环境变量。
    pub fn system_for(account: &Account) -> Option<Self> {
        let home = PathBuf::from(env::var_os("HOME").filter(|h| !h.is_empty())?);
        let dirs = VendorDirs::from_env(&home).with_dir(account.vendor, account.dir.clone());
        // 每个账号的额度缓存分开放，免得看到另一个账号的额度。缓存写进去时按私有权限建目录。
        let data_dir = cache_dir(&Config::data_dir()?, account);
        Some(
            Self::new(home, data_dir, SystemTime::now(), time::local_utc_offset())
                .with_fetch(Box::new(claude::fetch_usage))
                .with_dirs(dirs),
        )
    }

    /// 指定环境。三家目录取家目录下的默认位置（不看环境变量）；默认不联网（用量接口渠道直接判失败），要联网用 `with_fetch`。
    pub fn new(home: PathBuf, data_dir: PathBuf, now: SystemTime, utc_offset_secs: i64) -> Self {
        Self {
            dirs: VendorDirs::defaults(&home),
            home,
            data_dir,
            now,
            utc_offset_secs,
            rules: Rules::builtin(),
            fetch: Box::new(|_| Err("没有接入用量接口".to_owned())),
        }
    }

    /// 指定三家的数据目录（测试里模拟改过的环境变量）。
    pub fn with_dirs(mut self, dirs: VendorDirs) -> Self {
        self.dirs = dirs;
        self
    }

    /// 替换取用量接口原始响应的方式（测试里换成假数据）。
    pub fn with_fetch(mut self, fetch: Box<UsageFetch>) -> Self {
        self.fetch = fetch;
        self
    }

    /// 按顺序试渠道，返回第一个成功的读数；全失败返回「暂时取不到」。
    pub fn read(&self, vendor: Vendor) -> QuotaReading {
        self.rules
            .channels(vendor)
            .iter()
            .find_map(|ch| self.read_channel(ch).ok())
            .unwrap_or_else(|| QuotaReading::unavailable(self.now))
    }

    /// 每个渠道都试一遍，报告各自结果。
    pub fn probe(&self, vendor: Vendor) -> Vec<ChannelReport> {
        self.rules
            .channels(vendor)
            .iter()
            .map(|ch| ChannelReport {
                source: source_name(ch),
                result: self.read_channel(ch),
            })
            .collect()
    }

    /// 见 [`claude_settings`]。
    pub fn claude_settings(&self, cwd: &Path) -> String {
        claude::settings_arg(self, cwd)
    }

    fn read_channel(&self, ch: &Channel) -> Result<QuotaReading, String> {
        match ch {
            Channel::Statusline { .. } => claude::read_statusline(self, ch),
            Channel::UsageApi { .. } => claude::read_usage_api(self, ch),
            Channel::Rollout { .. } => codex::read_rollout(self, ch),
            Channel::TokensToday { .. } => {
                tokens::tokens_today(self, ch).map(|v| self.reading(ch, v, self.now))
            }
        }
    }

    fn reading(&self, ch: &Channel, value: QuotaValue, at: SystemTime) -> QuotaReading {
        QuotaReading {
            value,
            source: source_name(ch).to_owned(),
            at,
        }
    }
}

/// 渠道在界面上显示的来源名。
fn source_name(ch: &Channel) -> &'static str {
    match ch {
        Channel::Statusline { .. } => "状态栏",
        Channel::UsageApi { .. } => "用量接口",
        Channel::Rollout { .. } => "会话记录",
        Channel::TokensToday { .. } => "本机统计",
    }
}

/// 按规则从一份 JSON 里取额度窗口。已过重置时间的窗口丢掉；一个都取不到就算失败。
fn windows_from(
    doc: &Value,
    rules: &[WindowRule],
    now: SystemTime,
) -> Result<QuotaValue, WindowsError> {
    let now = time::to_secs(now);
    let mut found = 0;
    let mut windows = Vec::new();
    for rule in rules {
        let Some(percent) = doc.pointer(&rule.percent).and_then(Value::as_f64) else {
            continue;
        };
        found += 1;
        let resets_at = doc.pointer(&rule.resets_at).and_then(time::value_secs);
        if resets_at.is_some_and(|t| t <= now) {
            continue;
        }
        let window_minutes = match &rule.minutes {
            Minutes::Fixed(m) => Some(*m),
            Minutes::At(p) => doc
                .pointer(p)
                .and_then(Value::as_u64)
                .and_then(|m| u32::try_from(m).ok()),
        };
        windows.push(QuotaWindow {
            label: window_label(window_minutes),
            used_percent: percent.clamp(0.0, 100.0) as f32,
            window_minutes,
            resets_at: resets_at.map(time::from_secs),
        });
    }
    match (found, windows.is_empty()) {
        (0, _) => Err(WindowsError::Unrecognized),
        (_, true) => Err(WindowsError::AllExpired),
        _ => Ok(QuotaValue::Windows(windows)),
    }
}

#[derive(Debug, PartialEq)]
enum WindowsError {
    /// 一个额度字段都没找到：没带额度，或格式变了。
    Unrecognized,
    /// 找到了，但都已过重置时间。
    AllExpired,
}

impl From<WindowsError> for String {
    fn from(e: WindowsError) -> String {
        match e {
            WindowsError::Unrecognized => "没有认得的额度字段（格式可能变了）",
            WindowsError::AllExpired => "额度窗口都已过重置时间",
        }
        .to_owned()
    }
}

/// 300 → 「5 小时」，10080 → 「7 天」。
fn window_label(minutes: Option<u32>) -> String {
    match minutes {
        Some(m) if m > 0 && m % 1440 == 0 => format!("{} 天", m / 1440),
        Some(m) if m > 0 && m % 60 == 0 => format!("{} 小时", m / 60),
        Some(m) if m > 0 => format!("{m} 分钟"),
        _ => "额度".to_owned(),
    }
}

/// JSON 里这些位置的值都等于给定字符串。
fn matches_all(doc: &Value, matches: &BTreeMap<String, String>) -> bool {
    matches
        .iter()
        .all(|(p, want)| doc.pointer(p).and_then(Value::as_str) == Some(want.as_str()))
}

/// 递归列出目录下的 `*.jsonl` 及修改时间（不跟随符号链接）。目录不存在时报错。
fn jsonl_files(dir: &Path) -> Result<Vec<(PathBuf, SystemTime)>, String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut first = true;
    while let Some(d) = stack.pop() {
        let entries = match fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) if first => return Err(format!("{} 读不了: {e}", d.display())),
            Err(_) => continue,
        };
        first = false;
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file()
                && path.extension().is_some_and(|e| e == "jsonl")
                && let Ok(mtime) = entry.metadata().and_then(|m| m.modified())
            {
                out.push((path, mtime));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(percent: &str, minutes: Minutes) -> WindowRule {
        WindowRule {
            percent: format!("/{percent}/p"),
            resets_at: format!("/{percent}/r"),
            minutes,
        }
    }

    #[test]
    fn windows_skip_missing_and_expired() {
        let now = time::from_secs(1000);
        let rules = [
            rule("a", Minutes::Fixed(300)),
            rule("b", Minutes::At("/b/m".to_owned())),
            rule("c", Minutes::Fixed(60)),
        ];
        let doc = json!({"a": {"p": 12.5, "r": 2000}, "b": {"p": 150, "r": "1970-01-01T00:40:00Z", "m": 10080}});
        let QuotaValue::Windows(w) = windows_from(&doc, &rules, now).unwrap() else {
            panic!()
        };
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].label, "5 小时");
        assert_eq!(w[0].resets_at, Some(time::from_secs(2000)));
        assert_eq!((w[1].label.as_str(), w[1].used_percent), ("7 天", 100.0));

        let expired = json!({"a": {"p": 1, "r": 999}});
        assert_eq!(
            windows_from(&expired, &rules, now),
            Err(WindowsError::AllExpired)
        );
        let renamed = json!({"a": {"percent": 1}});
        assert_eq!(
            windows_from(&renamed, &rules, now),
            Err(WindowsError::Unrecognized)
        );
    }

    #[test]
    fn labels() {
        assert_eq!(window_label(Some(300)), "5 小时");
        assert_eq!(window_label(Some(10080)), "7 天");
        assert_eq!(window_label(Some(45)), "45 分钟");
        assert_eq!(window_label(None), "额度");
    }

    #[test]
    fn missing_dir_is_an_error() {
        assert!(jsonl_files(Path::new("/nonexistent/huitai-quota")).is_err());
    }
}
