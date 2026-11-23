//! 看得见的动态：在跑和刚停的对话行下多一行压暗的「此刻」、行尾计划进度、预览标题随状态、计划块、
//! 失败单独成状态（红色、汇总里排在等你回复后面）、两行的行点哪一行都选中它、转圈只改那几格。

mod ui_support;

use std::time::Duration;

use huitai::core::{SessionId, SessionState, Vendor};
use huitai::sources::Moment;
use huitai::ui::app::Doing;
use huitai::ui::{App, Theme, view};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use ui_support::*;

const W: u16 = 120;
const H: u16 = 30;

fn app_with_moments(vendor: Vendor) -> App {
    let mut app = demo_app(vendor);
    with_moments(&mut app);
    app
}

/// 列表里（左边那栏）写着 `needle` 的那一行。
fn line_of(text: &str, needle: &str) -> usize {
    text.lines()
        .position(|l| l.chars().take(70).collect::<String>().contains(needle))
        .unwrap_or_else(|| panic!("没有「{needle}」：\n{text}"))
}

fn entry<'a>(app: &'a App, id: &str) -> &'a huitai::index::Entry {
    app.lists
        .iter()
        .flat_map(|l| &l.entries)
        .find(|e| e.session.id.as_str() == id)
        .unwrap()
}

#[test]
fn running_and_just_stopped_rows_get_a_dim_second_line() {
    let mut app = app_with_moments(Vendor::Claude);
    let buf = render(&mut app, W, H);
    let text = dump(&buf);
    // 在跑：在跑的工具；刚做完：总结。行尾计划进度。
    let run = line_of(&text, "购物车结算按钮点了没反应");
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[run + 1].contains("Bash · npm run e2e -- checkout"),
        "{}",
        lines[run + 1]
    );
    assert!(lines[run].contains("2/5"), "{}", lines[run]);
    let done = line_of(&text, "把优惠券接口换成新版");
    assert!(
        lines[done + 1].contains("优惠券接口换成 v2 了，旧接口的调用都删了。"),
        "{}",
        lines[done + 1]
    );
    // 停了的那条还是一行；展开的组后面空一行再接下一组。
    let stopped = line_of(&text, "结算页首屏加载慢");
    let left = |l: &str| l.chars().take(30).collect::<String>();
    assert_eq!(
        left(lines[stopped + 1]).trim(),
        "",
        "{}",
        lines[stopped + 1]
    );
    assert!(
        lines[stopped + 2].contains("blog"),
        "{}",
        lines[stopped + 2]
    );
    // 动态行压暗：用最暗的那档。
    let y = (run + 1) as u16;
    let cell = (4..W / 2)
        .map(|x| &buf[(x, y)])
        .find(|c| c.symbol() == "n")
        .unwrap();
    assert_eq!(cell.fg, Theme::DARK.faint);
    assert_snapshot("activity", &text);
}

#[test]
fn preview_head_and_plan_block_follow_the_state() {
    let mut app = app_with_moments(Vendor::Claude);
    let text = dump(&render(&mut app, W, H));
    // 选中在跑的 shop 那条：标题行写跑了多久，消息上面是计划块（Codex 样式）。
    assert!(
        text.lines().nth(1).unwrap().contains("在跑 · 4 分 12 秒"),
        "{text}"
    );
    // 计划块在右边预览栏，整行找（`line_of` 只看左边那栏）。
    let lines: Vec<&str> = text.lines().collect();
    let plan = lines
        .iter()
        .position(|l| l.contains("计划 2/5"))
        .unwrap_or_else(|| panic!("没有计划块：\n{text}"));
    assert!(lines[plan + 1].contains("✔ 找到结算按钮被禁用的原因"));
    assert!(lines[plan + 3].contains("□ 跑 e2e 测试"));
    assert!(lines[plan + 5].contains("□ 补一条回归测试"));

    select_id(&mut app, "c-shop-2");
    load_preview(&mut app);
    let text = dump(&render(&mut app, W, H));
    assert!(
        text.lines().nth(1).unwrap().contains("做完 · 用时 6 分钟"),
        "{text}"
    );
    assert!(!text.contains("计划 "), "没有计划就不画计划块");

    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Enter)); // 展开 blog
    select_id(&mut app, "c-blog-1");
    let text = dump(&render(&mut app, W, H));
    assert!(
        text.lines()
            .nth(1)
            .unwrap()
            .contains("等你回复 · 1 小时 35 分"),
        "{text}"
    );
}

#[test]
fn failed_is_its_own_red_state() {
    let mut app = app_with_moments(Vendor::Pi);
    app.key(key(KeyCode::Enter)); // 展开 blog
    select_id(&mut app, "p-blog-1");
    load_preview(&mut app);
    let e = entry(&app, "p-blog-1").clone();
    assert!(app.failed(&e));
    assert_eq!(app.doing(&e), Some(Doing::Failed("interrupted".to_owned())));
    let buf = render(&mut app, W, H);
    let text = dump(&buf);
    let row = line_of(&text, "RSS 初稿");
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[row].contains("✗"), "状态灯换成失败：{}", lines[row]);
    assert!(
        lines[row + 1].contains("失败 · 被打断"),
        "{}",
        lines[row + 1]
    );
    assert!(!lines[row].contains('•'), "失败的不再标没看过的小点");
    let red = |y: usize, s: &str| {
        (0..W)
            .map(|x| &buf[(x, y as u16)])
            .find(|c| c.symbol() == s)
            .map(|c| c.fg)
    };
    assert_eq!(red(row, "✗"), Some(Theme::DARK.alert()));
    assert_eq!(red(row + 1, "失"), Some(Theme::DARK.alert()));
    assert!(
        text.lines().nth(1).unwrap().contains("失败 · 被打断"),
        "{text}"
    );
    assert_snapshot("activity-failed", &text);

    // 再发一句话（开始在跑）就不算失败了。
    app.set_state(SessionId::from("p-blog-1"), SessionState::Running);
    assert!(!app.failed(&e));
}

#[test]
fn board_orders_input_then_failed_then_done_then_running() {
    let mut app = app_with_moments(Vendor::Claude);
    app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    let ids: Vec<&str> = app
        .board_entries()
        .iter()
        .map(|e| e.session.id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "c-blog-1", "p-blog-1", "c-shop-2", "c-shop-1", "x-api-1", "c-wt-1"
        ]
    );
    let text = dump(&render(&mut app, W, H));
    let lines: Vec<&str> = text.lines().collect();
    // 汇总里同样两行一条。
    assert!(
        lines[3].contains("RSS 加好了，要不要把订阅链接放到页脚？"),
        "{}",
        lines[3]
    );
    assert!(lines[5].contains("失败 · 被打断"), "{}", lines[5]);
    assert_snapshot("activity-board", &text);
}

#[test]
fn clicking_either_line_of_a_two_line_row_selects_it() {
    let mut app = app_with_moments(Vendor::Claude);
    let text = dump(&render(&mut app, W, H));
    let list = app.areas.list;
    let click = |y: usize| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: list.x + 6,
        row: y as u16,
        modifiers: KeyModifiers::NONE,
    };
    let t0 = std::time::Instant::now();
    let done = line_of(&text, "把优惠券接口换成新版");
    app.mouse(click(done + 1), t0);
    assert_eq!(
        app.selected_entry().unwrap().session.id.as_str(),
        "c-shop-2"
    );
    // 两行的那条下面一条也点得准。
    let stopped = line_of(&text, "结算页首屏加载慢");
    app.mouse(click(stopped), t0 + Duration::from_secs(1));
    assert_eq!(
        app.selected_entry().unwrap().session.id.as_str(),
        "c-shop-3"
    );
}

#[test]
fn small_list_scrolls_two_line_rows_fully_into_view() {
    let mut app = app_with_moments(Vendor::Claude);
    select_id(&mut app, "c-shop-2");
    // 列表区 4 行：选中的两行都要看得见。
    let text = dump(&render(&mut app, W, 7));
    assert!(text.contains("把优惠券接口换成新版"), "{text}");
    assert!(text.contains("优惠券接口换成 v2 了"), "{text}");
}

#[test]
fn no_moment_no_second_line() {
    let mut app = demo_app(Vendor::Claude);
    let e = entry(&app, "c-shop-1").clone();
    assert_eq!(app.doing(&e), None);
    assert_eq!(app.entry_lines(&e), 1);
    // 在跑但还没有在跑的工具、也没说过话：没有数据就不画。
    app.set_moment(e.session.id.clone(), Moment::default());
    assert_eq!(app.entry_lines(&e), 1);
    // 停了的对话有「此刻」也不画（只给在跑和刚停的）。
    let stopped = entry(&app, "c-shop-3").clone();
    app.set_moment(
        stopped.session.id.clone(),
        Moment {
            summary: Some("旧的一句".to_owned()),
            ..Moment::default()
        },
    );
    assert_eq!(app.doing(&stopped), None);
    assert!(
        app.moment_targets()
            .iter()
            .all(|t| t.session.id != stopped.session.id),
        "停了的不读，除非选中"
    );
}

#[test]
fn spinner_frame_only_changes_spinner_cells() {
    let mut app = app_with_moments(Vendor::Claude);
    let first = render(&mut app, W, H);
    assert!(!app.areas.spin.is_empty(), "有在跑的就有转圈");
    for p in &app.areas.spin {
        assert_eq!(first[*p].symbol(), app.spinner().to_string());
    }
    // 换一帧：只改记下的那几格，结果和整帧重画一样。
    let mut patched = first.clone();
    app.spin = (app.spin + 1) % huitai::ui::app::SPINNER.len();
    view::respin(&mut patched, &app.areas.spin.clone(), app.spinner());
    let full = render(&mut app, W, H);
    assert_eq!(patched, full);
    assert_ne!(patched, first);
    assert!(
        !dump(&full).contains(view::SPIN_MARK),
        "占位字符不会留在画面上"
    );
}
