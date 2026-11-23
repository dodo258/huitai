//! 闲置自动收回：哪条托管对话该收回的判断，和结束它的步骤。
//!
//! 收回条件（全满足才收）：没有标签页连着；状态是「等你回复」或「已完成」（最后一次提醒信号之后只有收尾重画，
//! 和状态灯同一个判断；改窗口大小、焦点切换引起的重画也不算开跑）；超过设置的时长没有新输出。
//! 正在跑的（信号之后又有输出，或从没发过信号）守护进程自己绝不收。安静多久都不算停了：pi 隐藏工作指示器、Codex 关了动画时，
//! 干活中也可能几十秒乃至更久没输出（2026-11-23 审计实测），收回会杀掉还在干的活；状态灯的安静超时（`QUIET_AFTER`）只改灯。
//!
//! 答完不发提醒信号的 CLI（pi）：守护进程不读厂商记录，由会话台读（`Source::turn_done`），记录显示这一轮答完、
//! 而且记录也超过时长没变时发 `Request::Reclaim`；守护进程再核对没有标签页连着、超过时长没新输出（[`Activity::quiet`]），
//! 满足就按同样的步骤结束。
//!
//! 结束步骤：按厂商正常退出方式连按两下 Ctrl+C（三家空闲时都是这样退出），等 [`QUIT_GRACE`]；
//! 还在就 SIGTERM 进程组，再等 [`TERM_GRACE`]；还在才 SIGKILL。对话记录在厂商自己的文件里，不受影响。
//!
//! 时间一律从 [`Clock`] 取，测试换成假时钟。

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::core::REDRAW_GRACE;

/// 退出键：Ctrl+C。Claude Code、Codex、pi 空闲时第一下清输入框（或提示「再按一次退出」），
/// 第二下退出；这一招三家都认。出处：Claude Code interactive-mode 文档、Codex TUI #8936「double-press Ctrl+C/Ctrl+D to quit」、
/// pi-coding-agent README 快捷键表（「Ctrl+C twice: Quit」）。
pub const QUIT_KEY: u8 = 0x03;
/// 两下 Ctrl+C 之间隔多久：要在三家「再按一次」的窗口内（Claude 800ms、Codex 约 1 秒）。
pub const QUIT_GAP: Duration = Duration::from_millis(300);
/// 按了退出键之后等它自己退的时间，过了就 SIGTERM。
pub const QUIT_GRACE: Duration = Duration::from_secs(10);
/// SIGTERM 之后再等这么久，还不退就 SIGKILL。
pub const TERM_GRACE: Duration = Duration::from_secs(3);
/// 守护进程每隔这么久查一次有没有该收回的。
pub const SWEEP_EVERY: Duration = Duration::from_secs(30);

/// 取当前时间。默认是系统单调时钟。
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> Instant + Send + Sync>);

impl Clock {
    pub fn new(now: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    pub fn now(&self) -> Instant {
        (self.0)()
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new(Instant::now)
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Clock")
    }
}

/// 一条托管对话最近的动静。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activity {
    /// 最近一次有输出（进程刚起来时算一次）。
    pub last_output: Instant,
    /// 最近一次提醒信号（响铃、桌面通知）。
    last_signal: Option<Instant>,
    /// 最近一次改窗口大小或标签页焦点切换：CLI 跟着重画，不算开跑。
    last_nudge: Option<Instant>,
    /// 等你回复或已完成：发过提醒信号，之后只有收尾重画。
    settled: bool,
}

impl Activity {
    pub fn new(now: Instant) -> Self {
        Self {
            last_output: now,
            last_signal: None,
            last_nudge: None,
            settled: false,
        }
    }

    /// 有输出：提醒信号或窗口变化之后 [`REDRAW_GRACE`] 内的算重画，其余算开跑（和状态灯同一个判断）。
    pub fn output(&mut self, now: Instant) {
        let redraw = [self.last_signal, self.last_nudge]
            .into_iter()
            .flatten()
            .any(|t| now.saturating_duration_since(t) < REDRAW_GRACE);
        if !redraw {
            self.settled = false;
        }
        self.last_output = now;
    }

    pub fn signal(&mut self, now: Instant) {
        self.last_signal = Some(now);
        self.settled = true;
    }

    /// 改了窗口大小、标签页焦点切换（CLI 会重画）。
    pub fn nudge(&mut self, now: Instant) {
        self.last_nudge = Some(now);
    }

    pub fn settled(&self) -> bool {
        self.settled
    }

    /// 该不该收回。`limit` 为 `None` 表示设置里关了。
    pub fn should_reclaim(&self, attached: bool, now: Instant, limit: Option<Duration>) -> bool {
        self.settled && self.quiet(attached, now, limit)
    }

    /// 没有标签页连着、超过 `limit` 没新输出（不管发没发过提醒信号）。会话台凭厂商记录说这一轮答完了、
    /// 请求收回时看这一条。`limit` 为 `None` 表示设置里关了。
    pub fn quiet(&self, attached: bool, now: Instant, limit: Option<Duration>) -> bool {
        let Some(limit) = limit else { return false };
        !attached && now.saturating_duration_since(self.last_output) >= limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);
    const LIMIT: Option<Duration> = Some(Duration::from_secs(30 * 60));

    fn settled_at(t0: Instant) -> Activity {
        let mut a = Activity::new(t0);
        a.output(t0);
        a.signal(t0);
        a
    }

    #[test]
    fn idle_and_detached_is_reclaimed() {
        let t0 = Instant::now();
        let a = settled_at(t0);
        assert!(!a.should_reclaim(false, t0 + 29 * MIN, LIMIT));
        assert!(a.should_reclaim(false, t0 + 30 * MIN, LIMIT));
    }

    #[test]
    fn attached_or_turned_off_is_kept() {
        let t0 = Instant::now();
        let a = settled_at(t0);
        assert!(!a.should_reclaim(true, t0 + 60 * MIN, LIMIT));
        assert!(!a.should_reclaim(false, t0 + 600 * MIN, None));
    }

    #[test]
    fn never_signalled_counts_as_running() {
        let t0 = Instant::now();
        let a = Activity::new(t0);
        assert!(!a.settled());
        assert!(!a.should_reclaim(false, t0 + 60 * MIN, LIMIT));
    }

    #[test]
    fn redraw_after_signal_stays_settled_but_later_output_is_running() {
        let t0 = Instant::now();
        let mut a = settled_at(t0);
        a.output(t0 + REDRAW_GRACE - Duration::from_millis(1));
        assert!(a.settled());
        a.output(t0 + REDRAW_GRACE);
        assert!(!a.settled());
        assert!(!a.should_reclaim(false, t0 + 60 * MIN, LIMIT));
        // 跑完又发了信号：重新算闲置，从最后一次输出起。
        a.output(t0 + 5 * MIN);
        a.signal(t0 + 5 * MIN);
        assert!(!a.should_reclaim(false, t0 + 34 * MIN, LIMIT));
        assert!(a.should_reclaim(false, t0 + 35 * MIN, LIMIT));
    }

    #[test]
    fn quiet_ignores_signals_but_not_output_or_tabs() {
        let t0 = Instant::now();
        let mut a = Activity::new(t0);
        // 从没发过信号（pi）：守护进程自己不收，但够安静了。
        assert!(!a.should_reclaim(false, t0 + 60 * MIN, LIMIT));
        assert!(a.quiet(false, t0 + 30 * MIN, LIMIT));
        assert!(!a.quiet(false, t0 + 29 * MIN, LIMIT));
        assert!(!a.quiet(true, t0 + 60 * MIN, LIMIT), "有标签页连着");
        assert!(!a.quiet(false, t0 + 600 * MIN, None), "设置里关了");
        a.output(t0 + 20 * MIN);
        assert!(
            !a.quiet(false, t0 + 49 * MIN, LIMIT),
            "之后又有输出，从那时重新算"
        );
        assert!(a.quiet(false, t0 + 50 * MIN, LIMIT));
    }

    #[test]
    fn redraw_after_resize_is_not_running() {
        let t0 = Instant::now();
        let mut a = settled_at(t0);
        // 过了一会儿接上标签页、窗口大小不同，CLI 重画。
        a.nudge(t0 + 10 * MIN);
        a.output(t0 + 10 * MIN + Duration::from_millis(100));
        assert!(a.settled());
        assert!(!a.should_reclaim(false, t0 + 39 * MIN, LIMIT));
        assert!(a.should_reclaim(false, t0 + 41 * MIN, LIMIT));
        // 打字回显、开始干活：在跑。
        a.output(t0 + 12 * MIN);
        assert!(!a.settled());
    }
}
