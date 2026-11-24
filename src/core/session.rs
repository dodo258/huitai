use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::Vendor;

/// 厂商自己的对话 id（Claude/pi 的会话文件名、Codex 的 rollout id）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 账号 `account`（[`crate::core::Account::key`]）里厂商自己的 id `native`：默认账号就是 `native` 本身
    /// （老用户的 id 不变），别的账号前面加 `<账号>:`，几个账号里同名的 id（Codex 临时 id）不会撞。
    pub fn in_account(account: &str, native: &str) -> Self {
        if account.is_empty() {
            Self(native.to_owned())
        } else {
            Self(format!("{account}:{native}"))
        }
    }

    /// 拆成（账号，厂商自己的 id），见 [`SessionId::in_account`]。没有账号前缀的账号是空串。
    pub fn split_account(&self) -> (&str, &str) {
        let s = self.0.as_str();
        match s.split_at_checked(ACCOUNT_KEY_LEN) {
            Some((key, rest))
                if rest.starts_with(':')
                    && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) =>
            {
                (key, &rest[1..])
            }
            _ => ("", s),
        }
    }

    /// 厂商自己的 id（去掉账号前缀）。
    pub fn native(&self) -> &str {
        self.split_account().1
    }
}

/// 账号短名的长度（8 位十六进制）。
const ACCOUNT_KEY_LEN: usize = 8;

impl From<String> for SessionId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for SessionId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 提醒信号（响铃、桌面通知）之后这么久内的输出算 CLI 收尾重画（清转圈、刷状态栏），不算重新开跑。
/// 状态灯（notify）和闲置收回（host）用同一个判断。
pub const REDRAW_GRACE: Duration = Duration::from_secs(2);

/// 在跑的对话这么久没有任何输出，状态灯就当这一轮停了。只改灯：闲置收回不认它（见 `host::idle`）。
/// 给答完不发提醒信号的 CLI 用（pi 只发 OSC 133 区段，不响铃不发通知）；Claude、Codex 干活时转圈一直有输出，
/// 守护进程每秒最多报一次有输出，20 秒没报就是真没动静。取舍见 DECISIONS.md「安静超时」。
pub const QUIET_AFTER: Duration = Duration::from_secs(20);

/// 对话在会话台里的状态（状态灯）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// 托管中且有输出。
    Running,
    /// 收到 CLI 的通知/响铃，之后没有输出。
    Waiting,
    /// 同上但用户还没看过。
    Done,
    /// 没有托管的 PTY。
    #[default]
    Stopped,
}

impl SessionState {
    /// 状态灯字符。
    pub fn glyph(self) -> char {
        match self {
            SessionState::Running => '●',
            SessionState::Waiting => '◆',
            SessionState::Done => '✓',
            SessionState::Stopped => '○',
        }
    }

    /// 等你回复（「刚做完」是还没看过的等你回复）。
    pub fn waits(self) -> bool {
        matches!(self, SessionState::Waiting | SessionState::Done)
    }
}

/// 一条对话的元数据（由 sources 从厂商文件读出，只读）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub vendor: Vendor,
    pub id: SessionId,
    /// 对话所在项目目录。
    pub cwd: PathBuf,
    /// 厂商会话文件路径。
    pub path: PathBuf,
    /// 厂商给的标题（自定义标题优先，其次自动标题）。
    pub title: Option<String>,
    pub git_branch: Option<String>,
    pub last_active: SystemTime,
    pub message_count: usize,
    /// 自动运行（无头）的对话：Claude `claude -p`、Codex `codex exec` 这类脚本和后台任务起的。
    /// 列表默认把它们收进「另有 N 条后台」。旧缓存里没有这一项时当作不是。
    #[serde(default)]
    pub automatic: bool,
    /// 哪个账号的（[`crate::core::Account::key`]）：默认位置的账号是空串，旧缓存里没有这一项也是。
    /// 非默认账号的 [`Session::id`] 带着同样的前缀。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account: String,
}

impl Session {
    /// 有本机项目目录、能接着聊。没有的（Antigravity 的工作区为空或不是本机路径）只能查看：CLI 要在项目目录里启动。
    pub fn resumable(&self) -> bool {
        !self.cwd.as_os_str().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_round_trips_as_plain_string() {
        let id = SessionId::from("abc-123");
        assert_eq!(id.as_str(), "abc-123");
        assert_eq!(id.to_string(), "abc-123");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"abc-123\"");
    }

    #[test]
    fn account_prefix_round_trips() {
        let plain = SessionId::in_account("", "abc-123");
        assert_eq!(plain.as_str(), "abc-123");
        assert_eq!(plain.split_account(), ("", "abc-123"));
        let other = SessionId::in_account("0123abcd", "new-codex-17");
        assert_eq!(other.as_str(), "0123abcd:new-codex-17");
        assert_eq!(other.split_account(), ("0123abcd", "new-codex-17"));
        assert_eq!(other.native(), "new-codex-17");
        // 厂商 id 里本来就有冒号、或前面不是 8 位十六进制的，不当账号前缀。
        assert_eq!(SessionId::from("Z123abcd:x").native(), "Z123abcd:x");
        assert_eq!(SessionId::from("ses:x").native(), "ses:x");
        assert_eq!(SessionId::from("中文标题").native(), "中文标题");
    }

    #[test]
    fn state_defaults_to_stopped_and_has_glyphs() {
        assert_eq!(SessionState::default(), SessionState::Stopped);
        assert_eq!(SessionState::Waiting.glyph(), '◆');
        assert_eq!(
            serde_json::to_string(&SessionState::Running).unwrap(),
            "\"running\""
        );
    }

    #[test]
    fn session_round_trips_through_json() {
        let s = Session {
            vendor: Vendor::Claude,
            id: SessionId::from("s1"),
            cwd: PathBuf::from("/tmp/proj"),
            path: PathBuf::from("/tmp/proj/s1.jsonl"),
            title: Some("修登录".to_owned()),
            git_branch: None,
            last_active: SystemTime::UNIX_EPOCH,
            message_count: 3,
            automatic: true,
            account: "0123abcd".to_owned(),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Session>(&json).unwrap(), s);
    }

    #[test]
    fn old_cache_without_automatic_reads_as_own() {
        let json = r#"{"vendor":"claude","id":"s1","cwd":"/p","path":"/p/s1.jsonl","title":null,
            "git_branch":null,"last_active":{"secs_since_epoch":0,"nanos_since_epoch":0},"message_count":1}"#;
        let s = serde_json::from_str::<Session>(json).unwrap();
        assert!(!s.automatic);
        assert_eq!(s.account, "", "旧缓存里的都是默认账号的");
    }
}
