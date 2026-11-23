//! 对话刚停下来那一刻整行亮一下再淡掉（收起的组由组标题替它亮），以及额度条 80% 变黄、95% 变红。

mod ui_support;

use std::time::Duration;

use huitai::core::{QuotaValue, QuotaWindow, SessionId, SessionState, Vendor};
use huitai::ui::app::FLASH;
use huitai::ui::{App, Theme};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::Color;

use ui_support::*;

const W: u16 = 120;
const H: u16 = 30;
const X: u16 = 6;

/// 这条对话所在的屏幕行。
fn row_of(app: &mut App, id: &str) -> u16 {
    let title = app
        .list()
        .entries
        .iter()
        .find(|e| e.session.id.as_str() == id)
        .expect("列表里有这条")
        .title()
        .to_owned();
    // 只看左边列表那 3/5（右边预览的标题也是这几个字）。
    let text = dump(&render(app, W, H));
    text.lines()
        .position(|l| {
            l.chars()
                .take(usize::from(W) * 3 / 5)
                .collect::<String>()
                .contains(&title)
        })
        .unwrap_or_else(|| panic!("列表里没有「{title}」\n{text}")) as u16
}

fn bg_at(app: &mut App, y: u16) -> Color {
    render(app, W, H)[(X, y)].bg
}

#[test]
fn row_lights_up_when_running_stops_then_fades() {
    let t = Theme::DARK;
    let mut app = demo_app(Vendor::Claude);
    select_id(&mut app, "c-shop-2");
    app.tick_blink(Duration::ZERO);
    let y = row_of(&mut app, "c-shop-1");
    assert_eq!(bg_at(&mut app, y), t.base, "没停之前是普通底色");

    app.set_state(SessionId::from("c-shop-1"), SessionState::Done);
    let (left, s) = app
        .flash(&SessionId::from("c-shop-1"))
        .expect("刚停下来在亮");
    assert_eq!(s, SessionState::Done);
    assert!(left > 0.99, "{left}");
    let peak = bg_at(&mut app, y);
    assert_ne!(peak, t.base, "刚做完整行染色");

    // 淡到一半：还亮，但比刚停时淡；这一拍要重画。
    assert!(app.tick_blink(FLASH / 2), "淡的过程中每一拍都重画");
    let half = bg_at(&mut app, y);
    assert!(
        distance(half, t.base) < distance(peak, t.base) && half != t.base,
        "peak {peak:?} half {half:?}"
    );

    // 淡完：回到普通底色，最后这一拍也重画一次。
    assert!(app.tick_blink(FLASH + Duration::from_millis(1)));
    assert!(app.flash(&SessionId::from("c-shop-1")).is_none());
    assert_eq!(bg_at(&mut app, y), t.base);
}

#[test]
fn waiting_lights_yellow_and_only_after_running() {
    let t = Theme::DARK;
    let mut app = demo_app(Vendor::Claude);
    app.tick_blink(Duration::ZERO);
    // 在跑停成等你回复：亮黄色那一档。
    app.set_state(SessionId::from("c-shop-1"), SessionState::Waiting);
    assert_eq!(
        app.flash(&SessionId::from("c-shop-1")).map(|f| f.1),
        Some(SessionState::Waiting)
    );
    // 不是从在跑停下来的不亮：等你回复变刚做完、已停变刚做完、开跑。
    for (id, s) in [
        ("c-blog-1", SessionState::Done),
        ("c-new", SessionState::Done),
        ("c-shop-2", SessionState::Running),
    ] {
        app.set_state(SessionId::from(id), s);
        assert!(app.flash(&SessionId::from(id)).is_none(), "{id}");
    }
    // 选中的行也亮：底色从选中色往状态色走，左边那道强调色还在。
    select_id(&mut app, "c-shop-1");
    let y = row_of(&mut app, "c-shop-1");
    let buf = render(&mut app, W, H);
    assert_ne!(buf[(X, y)].bg, t.select);
    assert_eq!(buf[(0, y)].symbol(), "▎");
    assert_eq!(buf[(0, y)].bg, buf[(X, y)].bg);
}

#[test]
fn collapsed_group_header_lights_for_its_conversation() {
    let t = Theme::DARK;
    let mut app = demo_app(Vendor::Claude);
    app.tick_blink(Duration::ZERO);
    // 选中 c-shop-1，← 收起它所在的组，选中落到组标题上。
    app.key(key(KeyCode::Left));
    let y = row_of_header(&mut app);
    assert_eq!(bg_at(&mut app, y), t.select);
    app.set_state(SessionId::from("c-shop-1"), SessionState::Done);
    assert_ne!(bg_at(&mut app, y), t.select, "收起的组替里面刚做完的那条亮");
}

/// 选中行（组标题）的屏幕行。
fn row_of_header(app: &mut App) -> u16 {
    let buf = render(app, W, H);
    (2..H)
        .find(|&y| buf[(X, y)].bg == Theme::DARK.select)
        .expect("有选中行")
}

#[test]
fn quota_bar_turns_yellow_then_red() {
    let t = Theme::DARK;
    let mut app = demo_app(Vendor::Codex);
    let bar = |app: &mut App, used: f32| -> (Color, Color) {
        let mut r = quota_for(Vendor::Codex);
        r.value = QuotaValue::Windows(vec![QuotaWindow {
            label: "7 天".to_owned(),
            used_percent: used,
            window_minutes: Some(10080),
            resets_at: None,
        }]);
        app.quota.insert((Vendor::Codex, String::new()), r);
        let buf = render(app, W, H);
        let x = (0..W)
            .find(|&x| buf[(x, 0)].symbol() == "━")
            .expect("有额度条");
        (buf[(x, 0)].fg, pct_fg(&buf))
    };
    assert_eq!(bar(&mut app, 50.0), (t.accent(Vendor::Codex), t.text));
    let yellow = t.state(SessionState::Waiting);
    assert_eq!(bar(&mut app, 80.0), (yellow, yellow));
    assert_eq!(bar(&mut app, 94.0), (yellow, yellow));
    assert_eq!(bar(&mut app, 95.0), (t.alert(), t.alert()));
}

/// 顶栏百分号的前景色（这一家只有一个额度窗口，顶栏只有一个 %）。
fn pct_fg(buf: &Buffer) -> Color {
    let x = (0..W)
        .find(|&x| buf[(x, 0)].symbol() == "%")
        .expect("顶栏有百分比");
    buf[(x, 0)].fg
}

fn distance(a: Color, b: Color) -> i32 {
    let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (a, b) else {
        panic!("主题色都是 RGB");
    };
    [(ar, br), (ag, bg), (ab, bb)]
        .iter()
        .map(|&(x, y)| (i32::from(x) - i32::from(y)).abs())
        .sum()
}
