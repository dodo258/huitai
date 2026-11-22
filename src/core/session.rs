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
}

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
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Session>(&json).unwrap(), s);
    }

    #[test]
    fn old_cache_without_automatic_reads_as_own() {
        let json = r#"{"vendor":"claude","id":"s1","cwd":"/p","path":"/p/s1.jsonl","title":null,
            "git_branch":null,"last_active":{"secs_since_epoch":0,"nanos_since_epoch":0},"message_count":1}"#;
        assert!(!serde_json::from_str::<Session>(json).unwrap().automatic);
    }
}
