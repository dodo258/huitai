//! 顶栏额度的重置倒计时：有重置时间就在百分比后写「1 小时 20 分后重置」，
//! 没有或已经过了（读数过期）就不写；窄终端先去掉来源、第二个窗口，再去掉倒计时。

mod ui_support;

use std::time::{Duration, SystemTime};

use huitai::core::{QuotaReading, QuotaValue, QuotaWindow, Vendor};
use huitai::ui::App;

use ui_support::*;

const H: u16 = 24;

fn window(label: &str, used: f32, mins: u32, resets_at: Option<SystemTime>) -> QuotaWindow {
    QuotaWindow {
        label: label.to_owned(),
        used_percent: used,
        window_minutes: Some(mins),
        resets_at,
    }
}

/// Claude 顶栏：5 小时窗口 `five` 后重置、7 天窗口 `seven` 后重置（`None` 是没写重置时间）。
fn claude_with(five: Option<SystemTime>, seven: Option<SystemTime>) -> App {
    let mut app = demo_app(Vendor::Claude);
    app.quota.insert(
        (Vendor::Claude, String::new()),
        QuotaReading {
            value: QuotaValue::Windows(vec![
                window("5 小时", 23.0, 300, five),
                window("7 天", 41.0, 10080, seven),
            ]),
            source: "状态栏".to_owned(),
            at: now() - Duration::from_secs(120),
        },
    );
    app
}

fn top(app: &mut App, w: u16) -> String {
    dump(&render(app, w, H)).lines().next().unwrap().to_owned()
}

fn later(secs: u64) -> Option<SystemTime> {
    Some(now() + Duration::from_secs(secs))
}

#[test]
fn top_bar_shows_countdown_next_to_each_window() {
    let mut app = claude_with(later(80 * 60), later(3 * 86_400 + 5 * 3600));
    // 八家同屏时别家短名比来源、第二个窗口要紧：宽到 210 列才全写得下。
    let line = top(&mut app, 210);
    assert!(line.contains("23% · 1 小时 20 分后重置"), "{line}");
    assert!(line.contains("41% · 3 天 5 小时后重置"), "{line}");
    assert!(line.contains("状态栏 · 2 分钟前"), "{line}");
    assert_snapshot("quota-reset-top", &line);
}

#[test]
fn no_reset_time_means_no_countdown() {
    let mut app = claude_with(None, None);
    let line = top(&mut app, 180);
    assert!(line.contains("5 小时 ") && line.contains("23%"), "{line}");
    assert!(!line.contains("重置"), "{line}");
    assert_snapshot("quota-reset-none", &line);
}

#[test]
fn expired_reading_shows_no_countdown() {
    // 5 小时窗口的重置时间已经过了：读数过期，不写倒计时；7 天那个照写。
    let past = Some(now() - Duration::from_secs(60));
    let mut app = claude_with(past, later(2 * 3600));
    let line = top(&mut app, 180);
    assert!(line.contains("23%"), "{line}");
    assert!(!line.contains("23% ·"), "{line}");
    assert!(line.contains("41% · 2 小时后重置"), "{line}");
    // 刚好到点也算过了。
    let mut app = claude_with(Some(now()), None);
    assert!(!top(&mut app, 180).contains("重置"));
}

#[test]
fn narrow_top_bar_drops_countdown_after_second_window() {
    let mut app = claude_with(later(80 * 60), later(3 * 86_400));
    // 逐列收窄，记下每种写法第一次出现的宽度：来源先走，再是第二个窗口，再是倒计时，最后「额度」两字。
    let mut seen = Vec::new();
    for w in (40..=260).rev() {
        let line = top(&mut app, w);
        let form = (
            line.contains("分钟前"),
            line.contains("7 天"),
            line.contains("重置"),
            line.contains("额度"),
        );
        if seen.last() != Some(&form) {
            seen.push(form);
        }
    }
    // （来源, 第二个窗口, 倒计时, 「额度」）按收窄顺序出现。
    let at = |f| {
        seen.iter()
            .position(|&s| s == f)
            .unwrap_or_else(|| panic!("缺 {f:?}：{seen:?}"))
    };
    let steps = [
        at((true, true, true, true)),
        at((false, true, true, true)),
        at((false, false, true, true)),
        at((false, false, false, true)),
    ];
    assert!(steps.is_sorted(), "{seen:?}");
}
