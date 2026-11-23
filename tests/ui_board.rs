//! 「等你回复」汇总（`s`）：三家自己的对话按 要输入 > 失败 > 做完待看 > 在跑 排，选中、打开、`!`、换家、搜索、
//! 状态变化时选中跟着走，以及渲染出来的样子。

mod ui_support;

use std::time::{Duration, Instant};

use huitai::core::{SessionId, SessionState, Vendor};
use huitai::ui::App;
use huitai::ui::app::Effect;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use ui_support::*;

const W: u16 = 120;
const H: u16 = 30;

fn press(app: &mut App, c: char) -> Vec<Effect> {
    app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

fn board_ids(app: &App) -> Vec<String> {
    app.board_entries()
        .iter()
        .map(|e| e.session.id.to_string())
        .collect()
}

fn selected(app: &App) -> Option<String> {
    app.selected_entry().map(|e| e.session.id.to_string())
}

fn set_state(app: &mut App, id: &str, s: SessionState) {
    app.states.insert(SessionId::from(id), s);
}

#[test]
fn lists_waiting_first_then_running_across_vendors() {
    let mut app = demo_app(Vendor::Codex);
    press(&mut app, 's');
    assert!(app.board.is_some());
    // 等你回复在前，其次刚做完（还没看），在跑在后，各自最近聊过的在前；三家混在一起。
    assert_eq!(
        board_ids(&app),
        [
            "c-blog-1", "p-blog-1", "c-shop-2", "c-shop-1", "x-api-1", "c-wt-1"
        ]
    );
    // 和顶栏「◐3 在跑 ◆3 等你」数的是同一批。
    assert_eq!(app.global_counts(), (3, 3));
    // 打开时停在第一条上，标签不换。
    assert_eq!(selected(&app).as_deref(), Some("c-blog-1"));
    assert_eq!(app.vendor, Vendor::Codex);
}

#[test]
fn leaves_out_background_and_stopped() {
    let mut app = demo_app(Vendor::Claude);
    set_state(&mut app, "c-auto-1", SessionState::Running);
    set_state(&mut app, "c-blog-1", SessionState::Stopped);
    press(&mut app, 's');
    let ids = board_ids(&app);
    assert!(
        !ids.contains(&"c-auto-1".to_owned()),
        "后台对话不算：{ids:?}"
    );
    assert!(!ids.contains(&"c-blog-1".to_owned()), "停了的不算：{ids:?}");
    assert_eq!(ids.len(), 5);
}

#[test]
fn opens_the_selected_conversation_of_another_vendor() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, 's');
    // 第二条是 pi 的，会话台还停在 Claude 标签上。
    app.key(key(KeyCode::Down));
    let effects = app.key(key(KeyCode::Enter));
    let [Effect::Open { entry, here: false }] = effects.as_slice() else {
        panic!("回车打开选中那条：{effects:?}");
    };
    assert_eq!(entry.session.id.as_str(), "p-blog-1");
    assert_eq!(entry.session.vendor, Vendor::Pi);
    assert!(app.board.is_some(), "打开后汇总还开着，回来接着看下一条");
    // ↓ 到第三条，c 复制的是它的恢复命令。
    app.key(key(KeyCode::Down));
    let effects = press(&mut app, 'c');
    let [Effect::CopyResume(e)] = effects.as_slice() else {
        panic!("{effects:?}");
    };
    assert_eq!(e.session.id.as_str(), "c-shop-2");
    // n 在选中那条的项目里用它那家新开。
    app.key(key(KeyCode::Up));
    let effects = press(&mut app, 'n');
    assert_eq!(
        effects,
        vec![Effect::New {
            vendor: Vendor::Pi,
            cwd: "/Users/demo/projects/blog".into(),
        }]
    );
}

#[test]
fn bang_cycles_waiting_ones_inside_the_board() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, 's');
    let mut seen = Vec::new();
    for _ in 0..4 {
        app.key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT));
        seen.push(selected(&app).unwrap());
    }
    assert_eq!(seen, ["p-blog-1", "c-shop-2", "c-blog-1", "p-blog-1"]);
    assert!(app.board.is_some(), "! 不离开汇总");
    // 没有等你回复的：底栏说一句，选中不动。
    for id in ["p-blog-1", "c-shop-2", "c-blog-1"] {
        set_state(&mut app, id, SessionState::Running);
    }
    app.key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT));
    assert_eq!(app.status.as_deref(), Some("没有等你回复的对话"));
}

#[test]
fn selection_follows_the_conversation_when_its_state_changes() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, 's');
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Down));
    assert_eq!(selected(&app).as_deref(), Some("c-shop-2"));
    // 回了它一句，它变成在跑，挪到后面那段：选中跟着它。
    app.set_state(SessionId::from("c-shop-2"), SessionState::Running);
    assert_eq!(selected(&app).as_deref(), Some("c-shop-2"));
    assert_eq!(board_ids(&app)[4], "c-shop-2");
    render(&mut app, W, H);
    // 它结束了（不在汇总里）：停在最后看到它的位置上（第五条，现在是 c-wt-1）。
    app.set_state(SessionId::from("c-shop-2"), SessionState::Stopped);
    assert_eq!(selected(&app).as_deref(), Some("c-wt-1"));
}

#[test]
fn esc_s_tab_and_search_leave_the_board() {
    let mut app = demo_app(Vendor::Claude);
    let before = selected(&app);
    press(&mut app, 's');
    app.key(key(KeyCode::Esc));
    assert!(app.board.is_none());
    assert_eq!(selected(&app), before, "回到列表原来选中的那条");

    press(&mut app, 's');
    press(&mut app, 's');
    assert!(app.board.is_none(), "再按 s 关上");

    press(&mut app, 's');
    app.key(key(KeyCode::Tab));
    assert!(app.board.is_none());
    assert_eq!(app.vendor, Vendor::Codex, "Tab 关上汇总并换家");

    press(&mut app, 's');
    press(&mut app, '/');
    assert!(app.board.is_none() && app.search.is_some(), "/ 换成搜索");
    app.key(key(KeyCode::Esc));

    // 搜索框里打 s 是输入的字，不开汇总。
    press(&mut app, '/');
    press(&mut app, 's');
    assert!(app.board.is_none());
    assert_eq!(app.search.as_ref().unwrap().query, "s");
}

#[test]
fn click_selects_and_double_click_opens() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, 's');
    render(&mut app, W, H);
    let list = app.areas.list;
    let click = |row: u16| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: list.x + 5,
        row: list.y + row,
        modifiers: KeyModifiers::NONE,
    };
    let t0 = Instant::now();
    // 第一行是第一条（汇总没有钉住的组标题）。
    assert!(app.mouse(click(0), t0).is_empty());
    assert_eq!(selected(&app).as_deref(), Some("c-blog-1"));
    assert!(app.mouse(click(3), t0).is_empty());
    assert_eq!(selected(&app).as_deref(), Some("c-shop-1"));
    let effects = app.mouse(click(3), t0 + Duration::from_millis(100));
    assert!(
        matches!(effects.as_slice(), [Effect::Open { entry, .. }] if entry.session.id.as_str() == "c-shop-1"),
        "{effects:?}"
    );
    // 点标签：关上汇总、换到那家。
    let (_, tab) = app.areas.tabs[2];
    app.mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: tab.x,
            row: tab.y,
            modifiers: KeyModifiers::NONE,
        },
        t0 + Duration::from_secs(2),
    );
    assert!(app.board.is_none());
    assert_eq!(app.vendor, Vendor::Pi);
}

#[test]
fn renders_vendor_marks_projects_and_counts() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, 's');
    app.key(key(KeyCode::Down));
    load_preview(&mut app);
    let text = dump(&render(&mut app, W, H));
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[1].contains("等你回复和在跑的 · 各家"), "{}", lines[1]);
    // 每行标题前是那家的标记，后面是项目名；状态灯在右边状态列。
    let (title, status) = lines[2]
        .split_once("✻ 给博客加 RSS 订阅  blog")
        .unwrap_or_else(|| panic!("{}", lines[2]));
    assert!(!title.contains('◆'), "{}", lines[2]);
    assert!(status.contains('◆'), "{}", lines[2]);
    assert!(lines[3].contains("π RSS 初稿  blog"), "{}", lines[3]);
    assert!(
        lines[4].contains("✻ 把优惠券接口换成新版  shop"),
        "{}",
        lines[4]
    );
    assert!(
        lines[6].contains("◎ 给订单列表接口补单元测试  api"),
        "{}",
        lines[6]
    );
    let bottom = lines[H as usize - 1];
    assert!(bottom.contains("s 等你 3 · 在跑 3"), "{bottom}");
    assert!(bottom.contains("Esc 返回"), "{bottom}");
    // 预览显示选中那条（pi 的）的消息。
    assert!(text.contains("RSS 初稿"), "{text}");
    assert_snapshot("board", &text);
}

#[test]
fn empty_board_says_so() {
    let mut app = demo_app(Vendor::Claude);
    app.states.clear();
    press(&mut app, 's');
    assert!(app.selected_entry().is_none());
    assert!(app.key(key(KeyCode::Enter)).is_empty());
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("没有等你回复或在跑的对话"), "{text}");
    assert!(text.contains("s 等你 0 · 在跑 0"), "{text}");
}

#[test]
fn small_terminal_scrolls_to_keep_selection_visible() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, 's');
    app.key(key(KeyCode::End));
    // 列表区只有 3 行（顶栏、表头、底栏各占一行）。
    let text = dump(&render(&mut app, W, 6));
    assert_eq!(selected(&app).as_deref(), Some("c-wt-1"));
    assert!(text.contains("后台任务：购物车重构"), "{text}");
    assert!(!text.contains("RSS 初稿"), "{text}");
}
