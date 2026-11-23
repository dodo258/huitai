//! 状态推导与去重：把托管层的状态事件折算成状态灯，并决定哪一次该提醒。纯逻辑，时间由调用方传入。

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub use crate::core::REDRAW_GRACE;
use crate::core::{QUIET_AFTER, SessionId, SessionState};

/// 一轮提醒先攒这么久再发：CLI 常把响铃和带文字的通知前后脚一起发，合成一条。
pub const COALESCE: Duration = Duration::from_millis(300);

/// 一条对话发生的事。由托管层的状态事件转来（notify 只依赖 core，不直接认 host 的类型），
/// 另加两种只有界面或终端知道的：标签页前后台、用户在会话台里看过。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    /// CLI 进程启动。
    Started,
    /// CLI 有输出。
    Output,
    /// CLI 停下来要你回来：响铃（BEL）或托管层认出的程序状态（OSC 7501、Gemini 等待标题）。
    Bell,
    /// CLI 发的桌面通知（OSC 9 / OSC 777）。
    Notify { title: Option<String>, body: String },
    /// 有标签页接上了这条对话。
    Attached,
    /// 标签页断开（对话照跑）。
    Detached,
    /// 接着这条对话的标签页到了前台（`true`）或退到后台（`false`）。
    Focus(bool),
    /// 用户在会话台里看过这条对话。
    Seen,
    /// CLI 退出，PTY 已回收。
    Exited,
}

/// 该发出去的一条提醒。标题和正文是 CLI 自己给的（只响铃时都没有）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub id: SessionId,
    pub title: Option<String>,
    pub body: Option<String>,
}

/// 所有托管对话的状态灯和提醒去重。
#[derive(Debug)]
pub struct Notifier {
    sessions: HashMap<SessionId, Track>,
    /// 在跑的对话这么久没输出就当这一轮停了（[`QUIET_AFTER`]；测试里调短）。
    quiet: Duration,
}

impl Default for Notifier {
    fn default() -> Self {
        Self::with_quiet(QUIET_AFTER)
    }
}

#[derive(Debug)]
struct Track {
    state: SessionState,
    attached: bool,
    /// 标签页是否在前台；没接着或不知道时为 `None`。
    foreground: Option<bool>,
    /// 最近一次提醒信号或看过（接上、到前台、在会话台看过）的时间：之后 [`REDRAW_GRACE`] 内的输出是收尾重画
    /// 或接上后按新窗口大小的重画，不算新一轮（否则回车打开一条答完的对话，重画一下 20 秒后又成了没看过）。
    last_signal: Option<Instant>,
    /// 在跑以来最近一次有输出（开跑时算一次），用来判断安静超时。
    last_output: Instant,
    /// 这一轮（从开跑到下一次开跑）已经处理过提醒，不再提醒。
    handled: bool,
    pending: Option<Pending>,
}

#[derive(Debug)]
struct Pending {
    due: Instant,
    title: Option<String>,
    body: Option<String>,
    /// 这一轮里 CLI 发过自己的桌面通知：标签页开着时它已经原样到了那个终端，终端会自己弹。
    cli_notified: bool,
}

impl Track {
    fn new(now: Instant) -> Self {
        Self {
            state: SessionState::Running,
            attached: false,
            foreground: None,
            last_signal: None,
            last_output: now,
            handled: false,
            pending: None,
        }
    }

    fn in_front(&self) -> bool {
        self.attached && self.foreground == Some(true)
    }

    /// 用户看到了：未读的「刚做完」变成「等你回复」，这一轮不再提醒。
    fn seen(&mut self, now: Instant) {
        // 在跑时没有待发提醒（开跑会清掉），什么也不用做。
        if self.state == SessionState::Running {
            return;
        }
        self.state = SessionState::Waiting;
        self.last_signal = Some(now);
        self.pending = None;
        self.handled = true;
    }

    fn running(&mut self, now: Instant) {
        self.state = SessionState::Running;
        self.last_output = now;
        self.last_signal = None;
        self.handled = false;
        self.pending = None;
    }

    fn signal(&mut self, title: Option<String>, body: Option<String>, now: Instant) {
        let cli_notified = body.is_some();
        self.last_signal = Some(now);
        if self.in_front() {
            self.state = SessionState::Waiting;
            self.handled = true;
            return;
        }
        if self.state != SessionState::Waiting {
            self.state = SessionState::Done;
        }
        if self.handled {
            return;
        }
        match &mut self.pending {
            Some(p) => {
                // 先到的是响铃、后到的带文字：用带文字的那条。
                if p.body.is_none() && body.is_some() {
                    p.title = title;
                    p.body = body;
                }
                p.cli_notified |= cli_notified;
            }
            None => {
                self.pending = Some(Pending {
                    due: now + COALESCE,
                    title,
                    body,
                    cli_notified,
                });
            }
        }
    }
}

impl Notifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// 安静时长换成 `quiet`（测试用）。
    pub fn with_quiet(quiet: Duration) -> Self {
        Self {
            sessions: HashMap::new(),
            quiet,
        }
    }

    /// 喂一件事。
    pub fn notify(&mut self, id: SessionId, activity: Activity, now: Instant) {
        if activity == Activity::Exited {
            self.sessions.remove(&id);
            return;
        }
        let track = self.sessions.entry(id).or_insert_with(|| Track::new(now));
        match activity {
            Activity::Started => track.running(now),
            Activity::Output => {
                let redraw = track.last_signal.is_some_and(|t| now - t < REDRAW_GRACE);
                if track.state == SessionState::Running {
                    track.last_output = now;
                } else if !redraw {
                    track.running(now);
                }
            }
            Activity::Bell => track.signal(None, None, now),
            Activity::Notify { title, body } => track.signal(title, Some(body), now),
            Activity::Attached => {
                track.attached = true;
                track.foreground = None;
                track.seen(now);
            }
            Activity::Detached => {
                track.attached = false;
                track.foreground = None;
            }
            Activity::Focus(front) => {
                track.foreground = Some(front);
                if track.in_front() {
                    track.seen(now);
                }
            }
            Activity::Seen => track.seen(now),
            // 上面已移除这条对话。
            Activity::Exited => {}
        }
    }

    /// 取出到期的提醒。每轮最多一条；标签页开着且 CLI 自己发过桌面通知的，终端已经弹过，不再重复。
    pub fn poll(&mut self, now: Instant) -> Vec<Alert> {
        let mut alerts = Vec::new();
        for (id, track) in &mut self.sessions {
            let Some(p) = track.pending.take_if(|p| p.due <= now) else {
                continue;
            };
            track.handled = true;
            if track.in_front() || (track.attached && p.cli_notified) {
                continue;
            }
            alerts.push(Alert {
                id: id.clone(),
                title: p.title,
                body: p.body,
            });
        }
        alerts.sort_by(|a, b| a.id.cmp(&b.id));
        alerts
    }

    /// 在跑却已经安静了 `quiet` 的对话：当这一轮停了（标签页在前台算等你回复，否则算刚做完）。
    /// 只改状态灯，不产生提醒（安静可能是误判）；之后真来了响铃或通知照常提醒。返回状态变了的对话。
    pub fn settle(&mut self, now: Instant) -> Vec<SessionId> {
        let mut changed = Vec::new();
        for (id, track) in &mut self.sessions {
            if track.state != SessionState::Running
                || now.saturating_duration_since(track.last_output) < self.quiet
            {
                continue;
            }
            track.state = if track.in_front() {
                SessionState::Waiting
            } else {
                SessionState::Done
            };
            changed.push(id.clone());
        }
        changed.sort();
        changed
    }

    /// 最早一条待发提醒的到期时间，事件循环据此决定等多久再 `poll`。
    pub fn next_due(&self) -> Option<Instant> {
        self.sessions
            .values()
            .filter_map(|t| t.pending.as_ref().map(|p| p.due))
            .min()
    }

    /// 状态灯；没有托管的对话是「已停」。
    pub fn state(&self, id: &SessionId) -> SessionState {
        self.sessions
            .get(id)
            .map_or(SessionState::Stopped, |t| t.state)
    }
}
