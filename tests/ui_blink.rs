//! 「等你」只在有没看过的新结果时闪：答完没看的闪，看过（标签页在前台、回车打开、在会话台里选中看了 2 秒）就停，
//! 之后保持不闪的 ◆，直到下一轮又答完。状态由通知模块推，界面只按状态决定闪不闪、什么时候算看过。

mod ui_support;

use std::time::{Duration, Instant};

use huitai::core::{SessionId, SessionState, Vendor};
use huitai::notify::{Activity, Notifier};
use huitai::ui::app::{BLINK_HALF, SEEN_AFTER};
use huitai::ui::{App, Theme};
use ratatui::crossterm::event::KeyCode;
use ratatui::style::Color;

use ui_support::*;

const W: u16 = 120;
const H: u16 = 30;
const ID: &str = "c-shop-2";

/// 演示数据里只留 `ID` 这一条托管着（别的都已停），由 `n` 推它的状态，像运行层那样同步给界面。
fn one_hosted() -> (App, Notifier, Instant) {
    let mut app = demo_app(Vendor::Claude);
    app.states.clear();
    app.attached.clear();
    let t0 = Instant::now();
    let mut n = Notifier::new();
    feed(&mut app, &mut n, Activity::Started, t0);
    feed(&mut app, &mut n, Activity::Output, t0);
    (app, n, t0)
}

fn feed(app: &mut App, n: &mut Notifier, a: Activity, at: Instant) {
    let id = SessionId::from(ID);
    n.notify(id.clone(), a, at);
    app.set_state(id.clone(), n.state(&id));
}

/// 到了暗半拍还暗不暗（顶栏「◆ 等你」跟着这个闪）。
fn blinks(app: &mut App) -> bool {
    app.tick_blink(BLINK_HALF * 7);
    let dim = app.blink_dim;
    app.tick_blink(BLINK_HALF * 8);
    dim
}

/// 列表里这一条的状态灯颜色（左边列表里标题所在那一行的 ✓ 或 ◆）。
fn light_fg(app: &mut App) -> Color {
    let title = app
        .list()
        .entries
        .iter()
        .find(|e| e.session.id.as_str() == ID)
        .expect("列表里有这条")
        .title()
        .to_owned();
    let buf = render(app, W, H);
    let text = dump(&buf);
    let y = text
        .lines()
        .position(|l| {
            l.chars()
                .take(usize::from(W) * 3 / 5)
                .collect::<String>()
                .contains(&title)
        })
        .expect("列表里有这一行") as u16;
    let x = (0..W * 3 / 5)
        .find(|&x| matches!(buf[(x, y)].symbol(), "✓" | "◆"))
        .expect("这一行有状态灯");
    buf[(x, y)].fg
}

fn top(app: &mut App) -> String {
    dump(&render(app, W, H)).lines().next().unwrap().to_owned()
}

/// 选中 `id`：先往上找，找不到再往下（`select_id` 只往下走）。
fn pick(app: &mut App, id: &str) {
    for _ in 0..100 {
        if app
            .selected_entry()
            .is_some_and(|e| e.session.id.as_str() == id)
        {
            return;
        }
        app.key(key(KeyCode::Up));
    }
    select_id(app, id);
}

/// 选中别的再等多久都不算看过这条。
fn select_other(app: &mut App) {
    pick(app, "c-shop-1");
}

#[test]
fn answered_and_not_seen_blinks() {
    let (mut app, mut n, t0) = one_hosted();
    select_other(&mut app);
    assert!(!blinks(&mut app), "在跑的不闪");
    feed(
        &mut app,
        &mut n,
        Activity::Bell,
        t0 + Duration::from_secs(30),
    );
    assert_eq!(app.state_of(&SessionId::from(ID)), SessionState::Done);
    assert!(top(&mut app).contains("◆1 等你"), "{}", top(&mut app));
    assert!(blinks(&mut app), "答完没看：顶栏闪");
    // 这一行的 ✓ 也跟着闪：暗半拍是暗色，亮半拍是本色。
    app.tick_blink(BLINK_HALF);
    assert_eq!(light_fg(&mut app), Theme::DARK.faint);
    app.tick_blink(BLINK_HALF * 2);
    assert_eq!(light_fg(&mut app), Theme::DARK.state(SessionState::Done));
}

#[test]
fn selecting_it_for_two_seconds_counts_as_seen_and_stops_blinking() {
    let (mut app, mut n, t0) = one_hosted();
    select_other(&mut app);
    feed(
        &mut app,
        &mut n,
        Activity::Bell,
        t0 + Duration::from_secs(30),
    );
    app.tick_blink(Duration::ZERO);
    assert_eq!(app.take_seen(), None, "选中的不是它");

    // 方向键路过不算：选中不到 2 秒就走了。
    pick(&mut app, ID);
    app.tick_blink(Duration::from_secs(10));
    assert_eq!(app.take_seen(), None);
    app.tick_blink(Duration::from_secs(10) + SEEN_AFTER - Duration::from_millis(1));
    assert_eq!(app.take_seen(), None);
    select_other(&mut app);
    app.tick_blink(Duration::from_secs(20));
    assert_eq!(app.take_seen(), None);

    // 回来重新计时，一直选着满 2 秒才算看过，只报一次。
    pick(&mut app, ID);
    app.tick_blink(Duration::from_secs(30));
    assert_eq!(app.take_seen(), None, "刚选中");
    app.tick_blink(Duration::from_secs(30) + SEEN_AFTER);
    assert_eq!(app.take_seen(), Some(SessionId::from(ID)));
    feed(
        &mut app,
        &mut n,
        Activity::Seen,
        t0 + Duration::from_secs(62),
    );
    assert_eq!(app.take_seen(), None, "已经看过了");

    // 看过：顶栏照样计数，◆ 不闪。
    assert_eq!(app.state_of(&SessionId::from(ID)), SessionState::Waiting);
    assert!(top(&mut app).contains("◆1 等你"), "{}", top(&mut app));
    assert!(!blinks(&mut app), "看过的不闪");
    app.tick_blink(BLINK_HALF * 9);
    assert_eq!(
        light_fg(&mut app),
        Theme::DARK.state(SessionState::Waiting),
        "看过的 ◆ 常亮"
    );
}

#[test]
fn opening_it_counts_as_seen_and_the_next_answer_blinks_again() {
    let (mut app, mut n, t0) = one_hosted();
    select_other(&mut app);
    feed(
        &mut app,
        &mut n,
        Activity::Bell,
        t0 + Duration::from_secs(30),
    );
    assert!(blinks(&mut app));

    // 回车打开（后台是收不到焦点事件的旧版也一样：接上就算看过），接上后重画一下也不算新一轮。
    let t1 = t0 + Duration::from_secs(600);
    feed(&mut app, &mut n, Activity::Attached, t1);
    feed(
        &mut app,
        &mut n,
        Activity::Output,
        t1 + Duration::from_millis(100),
    );
    assert!(!blinks(&mut app), "打开看过就不闪");
    feed(
        &mut app,
        &mut n,
        Activity::Detached,
        t1 + Duration::from_secs(5),
    );
    assert!(!blinks(&mut app), "关了标签页也还是看过的");

    // 下一轮：主人回了一句，又答完，没看过，又闪。
    let t2 = t1 + Duration::from_secs(3600);
    feed(&mut app, &mut n, Activity::Output, t2);
    assert!(!blinks(&mut app), "在跑的不闪");
    feed(
        &mut app,
        &mut n,
        Activity::Bell,
        t2 + Duration::from_secs(30),
    );
    assert_eq!(app.state_of(&SessionId::from(ID)), SessionState::Done);
    assert!(blinks(&mut app), "看过后再答完又闪");
}

#[test]
fn answered_while_its_tab_is_in_front_never_blinks() {
    let (mut app, mut n, t0) = one_hosted();
    select_other(&mut app);
    feed(&mut app, &mut n, Activity::Attached, t0);
    feed(&mut app, &mut n, Activity::Focus(true), t0);
    feed(
        &mut app,
        &mut n,
        Activity::Output,
        t0 + Duration::from_secs(5),
    );
    feed(
        &mut app,
        &mut n,
        Activity::Bell,
        t0 + Duration::from_secs(30),
    );
    assert_eq!(app.state_of(&SessionId::from(ID)), SessionState::Waiting);
    assert!(!blinks(&mut app), "前台时答完直接不闪");
    assert!(top(&mut app).contains("◆1 等你"), "{}", top(&mut app));

    // 切走后的下一轮答完：没看过，闪。
    feed(
        &mut app,
        &mut n,
        Activity::Focus(false),
        t0 + Duration::from_secs(40),
    );
    feed(
        &mut app,
        &mut n,
        Activity::Output,
        t0 + Duration::from_secs(50),
    );
    feed(
        &mut app,
        &mut n,
        Activity::Bell,
        t0 + Duration::from_secs(80),
    );
    assert!(blinks(&mut app));
    // 切回来：看过，停。
    feed(
        &mut app,
        &mut n,
        Activity::Focus(true),
        t0 + Duration::from_secs(90),
    );
    assert!(!blinks(&mut app));
}

#[test]
fn blink_setting_off_never_blinks() {
    let (mut app, mut n, t0) = one_hosted();
    select_other(&mut app);
    feed(
        &mut app,
        &mut n,
        Activity::Bell,
        t0 + Duration::from_secs(30),
    );
    app.config.blink_waiting = false;
    assert!(!blinks(&mut app));
}

#[test]
fn selecting_it_while_the_board_is_in_the_background_is_not_seen() {
    let (mut app, mut n, t0) = one_hosted();
    pick(&mut app, ID);
    feed(
        &mut app,
        &mut n,
        Activity::Bell,
        t0 + Duration::from_secs(30),
    );
    // 会话台的标签页在后台：选着多久都不算看过，照样闪。
    app.board_front = false;
    app.tick_blink(Duration::from_secs(1));
    assert_eq!(app.take_seen(), None);
    app.tick_blink(Duration::from_secs(60));
    assert_eq!(app.take_seen(), None);
    assert!(blinks(&mut app));
    // 回到前台后从头算 2 秒。
    app.board_front = true;
    app.tick_blink(Duration::from_secs(100));
    assert_eq!(app.take_seen(), None);
    app.tick_blink(Duration::from_secs(100) + SEEN_AFTER);
    assert_eq!(app.take_seen(), Some(SessionId::from(ID)));
}
