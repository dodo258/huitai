//! 取数规则的类型。规则本身在同目录的 `rules.json`，编译进二进制；厂商改格式时只改那个文件。

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::core::Vendor;

/// 每家各自的有序渠道列表。`rules.json` 里按厂商的存储名（[`Vendor::key`]）分开写，没写的那家没有额度渠道。
#[derive(Debug)]
pub struct Rules {
    by_vendor: [Vec<Channel>; Vendor::SLOTS],
}

impl Rules {
    /// 编译进二进制的默认规则。
    pub fn builtin() -> &'static Rules {
        static RULES: OnceLock<Rules> = OnceLock::new();
        RULES.get_or_init(|| {
            Rules::parse(include_str!("rules.json")).expect("quota/rules.json 格式不对")
        })
    }

    fn parse(json: &str) -> serde_json::Result<Rules> {
        let mut doc: serde_json::Map<String, serde_json::Value> = serde_json::from_str(json)?;
        let mut by_vendor: [Vec<Channel>; Vendor::SLOTS] = Default::default();
        for v in Vendor::ALL {
            if let Some(list) = doc.remove(v.key()) {
                by_vendor[v.index()] = serde_json::from_value(list)?;
            }
        }
        Ok(Rules { by_vendor })
    }

    pub fn channels(&self, vendor: Vendor) -> &[Channel] {
        &self.by_vendor[vendor.index()]
    }
}

/// 一个渠道。路径都是 JSON Pointer。
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Channel {
    /// 会话台注入的 Claude 状态栏命令写下的原始 JSON。
    Statusline {
        /// 数据目录下的缓存文件名。
        cache_file: String,
        /// 缓存比这更旧就换下一个渠道。
        max_age_minutes: u64,
        /// 每条对话一份的状态栏数据放在数据目录下的这个子目录，文件名 `<session_id>.json`（上下文占用用）。
        context_dir: String,
        /// 上下文已用百分比（0–100）在状态栏数据里的位置。
        context_percent: String,
        windows: Vec<WindowRule>,
    },
    /// Claude 官方用量接口（未公开文档，CLI 内部在用）。
    UsageApi {
        url: String,
        headers: Vec<String>,
        /// 钥匙串里 Claude 登录凭证的服务名。
        keychain_service: String,
        /// 凭证 JSON 里 access token 的位置。
        token: String,
        /// 凭证 JSON 里过期时间（毫秒）的位置。
        expires_at_ms: String,
        timeout_secs: u64,
        /// 读数缓存文件名，避免频繁联网。
        cache_file: String,
        cache_minutes: u64,
        windows: Vec<WindowRule>,
    },
    /// Codex 最新会话记录里带额度的事件。
    Rollout {
        /// 相对家目录的默认位置（`.codex/…`），改过数据目录的按 `VendorDirs` 换。
        dir: String,
        line_contains: String,
        matches: BTreeMap<String, String>,
        timestamp: String,
        /// 按修改时间倒序最多看几个文件。
        max_files: usize,
        /// 每个文件只读末尾这么多字节。
        tail_bytes: u64,
        windows: Vec<WindowRule>,
    },
    /// 统计本机会话记录里今天用掉的 token。
    TokensToday {
        /// 相对家目录的默认位置，改过数据目录的按 `VendorDirs` 换；递归找 `*.jsonl`。
        dir: String,
        /// 先按子串粗筛，命中再解析。
        line_contains: String,
        matches: BTreeMap<String, String>,
        timestamp: String,
        /// 这些字段拼成的键重复出现时只算一次（同一条消息被写多行）。
        #[serde(default)]
        dedupe: Vec<String>,
        /// 相加得到一行的 token 数。
        count: Vec<String>,
        /// `count` 是每个文件内的累计值：今天的用量 = 今天最后一个值 − 今天之前最后一个值。
        #[serde(default)]
        cumulative: bool,
    },
}

/// 一个额度窗口在 JSON 里的位置。
#[derive(Debug, Deserialize)]
pub struct WindowRule {
    /// 已用百分比（0–100）。
    pub percent: String,
    /// 重置时间：Unix 秒或 RFC 3339 字符串。
    pub resets_at: String,
    pub minutes: Minutes,
}

/// 窗口长度：固定值，或从 JSON 里读。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Minutes {
    Fixed(u32),
    At(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_rules_parse_and_every_vendor_has_a_channel() {
        let rules = Rules::builtin();
        for v in [Vendor::Claude, Vendor::Codex, Vendor::Pi] {
            assert!(!rules.channels(v).is_empty(), "{v} 没有渠道");
        }
        // Hermes 的额度（各 provider 的余额）还没接，没有渠道。
        assert!(rules.channels(Vendor::Hermes).is_empty());
        let claude = rules.channels(Vendor::Claude);
        assert!(matches!(claude[0], Channel::Statusline { .. }));
        assert!(matches!(claude[1], Channel::UsageApi { .. }));
        assert!(matches!(
            rules.channels(Vendor::Codex)[0],
            Channel::Rollout { .. }
        ));
    }
}
