//! 状态推导和提醒去重。时间全由测试给定，不弹真实通知。

use std::time::{Duration, Instant};

use huitai::core::{SessionId, SessionState};
use huitai::notify::{Activity, Alert, COALESCE, Notifier, REDRAW_GRACE};

fn id(s: &str) -> SessionId {
    SessionId::from(s)
}

fn notify(body: &str) -> Activity {
    Activity::Notify {
        title: Some("Claude Code".to_owned()),
        body: body.to_owned(),
    }
}

/// 一条托管中、没有标签页接着的对话。
fn started(n: &mut Notifier, s: &str, t0: Instant) {
    n.notify(id(s), Activity::Started, t0);
    n.notify(id(s), Activity::Output, t0);
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn unknown_session_is_stopped_and_started_is_running() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    assert_eq!(n.state(&id("a")), SessionState::Stopped);
    started(&mut n, "a", t0);
    assert_eq!(n.state(&id("a")), SessionState::Running);
    assert!(n.poll(t0 + Duration::from_secs(10)).is_empty());
}

#[test]
fn bell_in_background_is_done_and_alerts_once_after_coalescing() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Bell, t0 + ms(100));
    assert_eq!(n.state(&id("a")), SessionState::Done);
    assert_eq!(n.next_due(), Some(t0 + ms(100) + COALESCE));
    assert!(n.poll(t0 + ms(150)).is_empty(), "还在合并窗口里");
    let alerts = n.poll(t0 + ms(100) + COALESCE);
    assert_eq!(
        alerts,
        vec![Alert {
            id: id("a"),
            title: None,
            body: None
        }]
    );
    assert!(
        n.poll(t0 + Duration::from_secs(5)).is_empty(),
        "同一事件不重复"
    );
    assert_eq!(n.next_due(), None);
}

#[test]
fn bell_and_notification_together_become_one_alert_with_text() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Bell, t0);
    n.notify(id("a"), notify("Claude 在等你确认"), t0 + ms(50));
    n.notify(id("a"), Activity::Bell, t0 + ms(80));
    let alerts = n.poll(t0 + Duration::from_secs(1));
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].body.as_deref(), Some("Claude 在等你确认"));
    assert_eq!(alerts[0].title.as_deref(), Some("Claude Code"));
}

#[test]
fn repeated_reminders_in_the_same_round_do_not_alert_again() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    n.notify(id("a"), notify("等你输入"), t0);
    assert_eq!(n.poll(t0 + Duration::from_secs(1)).len(), 1);
    // CLI 一分钟后又提醒一次，中间没开跑。
    let later = t0 + Duration::from_secs(60);
    n.notify(id("a"), notify("等你输入"), later);
    n.notify(id("a"), Activity::Bell, later);
    assert!(n.poll(later + Duration::from_secs(1)).is_empty());
    assert_eq!(n.state(&id("a")), SessionState::Done);
}

#[test]
fn redraw_right_after_the_signal_is_not_running_again() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Bell, t0);
    n.notify(id("a"), Activity::Output, t0 + REDRAW_GRACE - ms(1));
    assert_eq!(n.state(&id("a")), SessionState::Done);
    assert_eq!(n.poll(t0 + REDRAW_GRACE).len(), 1);
}

#[test]
fn output_after_the_grace_starts_a_new_round_that_can_alert_again() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Bell, t0);
    assert_eq!(n.poll(t0 + Duration::from_secs(1)).len(), 1);
    let t1 = t0 + Duration::from_secs(30);
    n.notify(id("a"), Activity::Output, t1);
    assert_eq!(n.state(&id("a")), SessionState::Running);
    n.notify(id("a"), Activity::Bell, t1 + Duration::from_secs(5));
    assert_eq!(n.state(&id("a")), SessionState::Done);
    assert_eq!(n.poll(t1 + Duration::from_secs(6)).len(), 1);
}

#[test]
fn output_cancels_a_pending_alert() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    // 合并窗口比收尾宽限短，这里用宽限后的输出验证「开跑了就不再提醒」。
    n.notify(id("a"), Activity::Bell, t0);
    n.notify(id("a"), Activity::Output, t0 + REDRAW_GRACE);
    assert_eq!(n.state(&id("a")), SessionState::Running);
    assert!(n.poll(t0 + REDRAW_GRACE + COALESCE).is_empty());
}

#[test]
fn foreground_tab_is_waiting_without_alert() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Attached, t0);
    n.notify(id("a"), Activity::Focus(true), t0);
    n.notify(id("a"), Activity::Bell, t0 + ms(10));
    n.notify(id("a"), notify("等你"), t0 + ms(20));
    assert_eq!(n.state(&id("a")), SessionState::Waiting);
    assert_eq!(n.next_due(), None);
    assert!(n.poll(t0 + Duration::from_secs(1)).is_empty());
}

#[test]
fn background_tab_alerts_on_bell() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Attached, t0);
    n.notify(id("a"), Activity::Focus(false), t0);
    n.notify(id("a"), Activity::Bell, t0);
    assert_eq!(n.state(&id("a")), SessionState::Done);
    assert_eq!(n.poll(t0 + COALESCE).len(), 1);
}

#[test]
fn open_tab_already_got_the_cli_notification_so_no_duplicate() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Attached, t0);
    n.notify(id("a"), Activity::Bell, t0);
    n.notify(id("a"), notify("做完了"), t0 + ms(10));
    assert!(n.poll(t0 + Duration::from_secs(1)).is_empty());
    assert_eq!(n.state(&id("a")), SessionState::Done, "没看过仍是刚做完");
}

#[test]
fn tab_coming_to_front_marks_it_seen_and_cancels_the_alert() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Attached, t0);
    n.notify(id("a"), Activity::Focus(false), t0);
    n.notify(id("a"), Activity::Bell, t0);
    n.notify(id("a"), Activity::Focus(true), t0 + ms(100));
    assert_eq!(n.state(&id("a")), SessionState::Waiting);
    assert!(n.poll(t0 + Duration::from_secs(1)).is_empty());
}

#[test]
fn attaching_or_seeing_turns_done_into_waiting() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    for s in ["a", "b"] {
        started(&mut n, s, t0);
        n.notify(id(s), Activity::Bell, t0);
    }
    assert_eq!(n.poll(t0 + COALESCE).len(), 2);
    n.notify(id("a"), Activity::Attached, t0 + Duration::from_secs(3));
    n.notify(id("b"), Activity::Seen, t0 + Duration::from_secs(3));
    assert_eq!(n.state(&id("a")), SessionState::Waiting);
    assert_eq!(n.state(&id("b")), SessionState::Waiting);
    // 看过之后 CLI 再提醒，同一轮里不再弹。
    n.notify(id("b"), Activity::Bell, t0 + Duration::from_secs(10));
    assert_eq!(n.state(&id("b")), SessionState::Waiting);
    assert!(n.poll(t0 + Duration::from_secs(11)).is_empty());
}

#[test]
fn seen_before_due_cancels_the_alert() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Bell, t0);
    n.notify(id("a"), Activity::Seen, t0 + ms(10));
    assert!(n.poll(t0 + Duration::from_secs(1)).is_empty());
    assert_eq!(n.state(&id("a")), SessionState::Waiting);
}

#[test]
fn detached_tab_alerts_again_like_an_unopened_one() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Attached, t0);
    n.notify(id("a"), Activity::Focus(true), t0);
    n.notify(id("a"), Activity::Detached, t0 + ms(1));
    n.notify(id("a"), notify("做完了"), t0 + ms(2));
    assert_eq!(n.state(&id("a")), SessionState::Done);
    let alerts = n.poll(t0 + Duration::from_secs(1));
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].body.as_deref(), Some("做完了"));
}

#[test]
fn exit_stops_and_drops_the_pending_alert() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Bell, t0);
    n.notify(id("a"), Activity::Exited, t0 + ms(10));
    assert_eq!(n.state(&id("a")), SessionState::Stopped);
    assert_eq!(n.next_due(), None);
    assert!(n.poll(t0 + Duration::from_secs(1)).is_empty());
}

#[test]
fn sessions_are_independent_and_alerts_come_out_in_id_order() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    for s in ["c", "a", "b"] {
        started(&mut n, s, t0);
    }
    n.notify(id("c"), Activity::Bell, t0);
    n.notify(id("a"), Activity::Bell, t0 + ms(100));
    assert_eq!(n.next_due(), Some(t0 + COALESCE));
    let ids: Vec<_> = n
        .poll(t0 + ms(100) + COALESCE)
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert_eq!(ids, vec![id("a"), id("c")]);
    assert_eq!(n.state(&id("b")), SessionState::Running);
}

#[test]
fn attaching_alone_does_not_mean_in_front() {
    // 审查的复现：只有接上、没有焦点事件时不算在前台，响铃后是「刚做完」（未读），并且提醒。
    let t0 = Instant::now();
    let mut n = Notifier::new();
    n.notify(id("a"), Activity::Started, t0);
    n.notify(id("a"), Activity::Attached, t0);
    n.notify(id("a"), Activity::Bell, t0 + ms(10));
    assert_eq!(n.state(&id("a")), SessionState::Done);
    assert_eq!(n.poll(t0 + Duration::from_secs(1)).len(), 1);
}

#[test]
fn focus_decides_whether_the_open_tab_is_in_front() {
    let t0 = Instant::now();
    let mut n = Notifier::new();
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Attached, t0);
    n.notify(id("a"), Activity::Focus(true), t0);
    n.notify(id("a"), Activity::Bell, t0 + ms(10));
    assert_eq!(
        n.state(&id("a")),
        SessionState::Waiting,
        "在前台响铃：等你回复"
    );
    assert!(
        n.poll(t0 + Duration::from_secs(1)).is_empty(),
        "在前台不提醒"
    );

    // 下一轮：切走后响铃，刚做完并提醒。
    let t1 = t0 + Duration::from_secs(5);
    n.notify(id("a"), Activity::Output, t1);
    n.notify(id("a"), Activity::Focus(false), t1);
    n.notify(id("a"), Activity::Bell, t1 + ms(10));
    assert_eq!(n.state(&id("a")), SessionState::Done);
    assert_eq!(n.poll(t1 + Duration::from_secs(1)).len(), 1);
}

// —— 安静超时：答完不发提醒信号的 CLI（pi）——

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[test]
fn quiet_running_session_becomes_done_without_alert() {
    let t0 = Instant::now();
    let mut n = Notifier::with_quiet(secs(20));
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Output, t0 + secs(5));
    assert!(n.settle(t0 + secs(24)).is_empty());
    assert_eq!(n.state(&id("a")), SessionState::Running);
    assert_eq!(n.settle(t0 + secs(25)), vec![id("a")]);
    assert_eq!(n.state(&id("a")), SessionState::Done);
    // 只改状态灯：不产生提醒，也不重复报变化。
    assert!(n.poll(t0 + secs(60)).is_empty());
    assert!(n.settle(t0 + secs(60)).is_empty());
}

#[test]
fn quiet_in_front_is_waiting() {
    let t0 = Instant::now();
    let mut n = Notifier::with_quiet(secs(20));
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Attached, t0);
    n.notify(id("a"), Activity::Focus(true), t0);
    n.settle(t0 + secs(20));
    assert_eq!(n.state(&id("a")), SessionState::Waiting);
}

#[test]
fn output_after_quiet_runs_again_and_redraw_rules_still_apply() {
    let t0 = Instant::now();
    let mut n = Notifier::with_quiet(secs(20));
    started(&mut n, "a", t0);
    n.settle(t0 + secs(20));
    assert_eq!(n.state(&id("a")), SessionState::Done);
    n.notify(id("a"), Activity::Output, t0 + secs(30));
    assert_eq!(n.state(&id("a")), SessionState::Running);
    // 又安静了 20 秒：再停。
    assert!(n.settle(t0 + secs(49)).is_empty());
    assert_eq!(n.settle(t0 + secs(50)), vec![id("a")]);
    // 响铃之后 2 秒内的收尾重画不算开跑（原有规则不变）。
    n.notify(id("a"), Activity::Output, t0 + secs(60));
    n.notify(id("a"), Activity::Bell, t0 + secs(61));
    n.notify(
        id("a"),
        Activity::Output,
        t0 + secs(61) + REDRAW_GRACE - ms(1),
    );
    assert_eq!(n.state(&id("a")), SessionState::Done);
}

#[test]
fn steady_output_never_settles() {
    let t0 = Instant::now();
    let mut n = Notifier::with_quiet(secs(20));
    started(&mut n, "a", t0);
    for s in 1..=300 {
        n.notify(id("a"), Activity::Output, t0 + secs(s));
        assert!(n.settle(t0 + secs(s)).is_empty());
    }
    assert_eq!(n.state(&id("a")), SessionState::Running);
}

#[test]
fn real_bell_after_quiet_still_alerts() {
    let t0 = Instant::now();
    let mut n = Notifier::with_quiet(secs(20));
    started(&mut n, "a", t0);
    n.settle(t0 + secs(20));
    n.notify(id("a"), Activity::Bell, t0 + secs(30));
    let alerts = n.poll(t0 + secs(30) + COALESCE);
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].id, id("a"));
}

// —— 看过之后的重画不算新一轮（闪烁只表示没看过的新结果）——

#[test]
fn redraw_right_after_opening_keeps_it_seen() {
    // 回车打开一条答完的对话：接上后 CLI 按新窗口大小重画，不算开跑；安静多久都还是看过的「等你回复」。
    let t0 = Instant::now();
    let mut n = Notifier::with_quiet(secs(20));
    started(&mut n, "a", t0);
    n.settle(t0 + secs(20));
    assert_eq!(n.state(&id("a")), SessionState::Done, "答完没看");
    let t1 = t0 + secs(600);
    n.notify(id("a"), Activity::Attached, t1);
    assert_eq!(n.state(&id("a")), SessionState::Waiting, "打开就算看过");
    n.notify(id("a"), Activity::Output, t1 + ms(50));
    n.notify(id("a"), Activity::Output, t1 + REDRAW_GRACE - ms(1));
    assert!(n.settle(t1 + secs(60)).is_empty());
    assert_eq!(n.state(&id("a")), SessionState::Waiting);
    // 真有新一轮（重画窗口之后的输出）：答完又是没看过。
    n.notify(id("a"), Activity::Output, t1 + secs(70));
    assert_eq!(n.state(&id("a")), SessionState::Running);
    n.notify(id("a"), Activity::Bell, t1 + secs(80));
    assert_eq!(n.state(&id("a")), SessionState::Done);
}

#[test]
fn seen_in_the_board_then_a_new_round_is_unseen_again() {
    let t0 = Instant::now();
    let mut n = Notifier::with_quiet(secs(20));
    started(&mut n, "a", t0);
    n.notify(id("a"), Activity::Bell, t0 + secs(1));
    n.notify(id("a"), Activity::Seen, t0 + secs(5));
    assert_eq!(n.state(&id("a")), SessionState::Waiting);
    // 看过之后很久不回：一直是看过的，安静超时也不改。
    assert!(n.settle(t0 + secs(3600)).is_empty());
    assert_eq!(n.state(&id("a")), SessionState::Waiting);
    // 回了一句、又答完：没看过。
    n.notify(id("a"), Activity::Output, t0 + secs(3700));
    n.settle(t0 + secs(3720));
    assert_eq!(n.state(&id("a")), SessionState::Done);
}
