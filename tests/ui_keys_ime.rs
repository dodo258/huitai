//! 单键在中文输入法下也好用（docs/specs/keys-and-ime.md）：全角符号别名、打出中文转搜索、
//! 底栏随选中项换提示、右键菜单、`w` 当前窗口打开、设置里的「自动英文」开关。

mod ui_support;

use std::time::Instant;

use huitai::core::Vendor;
use huitai::ui::App;
use huitai::ui::app::{Effect, Overlay, Prompt};
use huitai::ui::keys::{self, Action, Selected};
use ratatui::crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use ui_support::*;

const W: u16 = 120;
const H: u16 = 30;

fn press(app: &mut App, c: char) -> Vec<Effect> {
    app.key(key(KeyCode::Char(c)))
}

fn bottom(app: &mut App) -> String {
    let buf = render(app, W, H);
    dump(&buf)
        .lines()
        .nth(usize::from(H - 1))
        .unwrap()
        .to_owned()
}

fn line_of(app: &mut App, text: &str) -> u16 {
    let buf = render(app, W, H);
    let screen = dump(&buf);
    screen
        .lines()
        .position(|l| l.contains(text))
        .unwrap_or_else(|| panic!("画面上没有「{text}」：\n{screen}")) as u16
}

fn right_click(app: &mut App, x: u16, y: u16) -> Vec<Effect> {
    app.mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        },
        Instant::now(),
    )
}

fn left_click(app: &mut App, x: u16, y: u16) -> Vec<Effect> {
    app.mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        },
        Instant::now(),
    )
}

fn opened(effects: &[Effect]) -> Option<(String, bool)> {
    match effects {
        [Effect::Open { entry, here }] => Some((entry.session.id.to_string(), *here)),
        _ => None,
    }
}

// ---- 全角符号 ----

#[test]
fn fullwidth_punctuation_works_like_halfwidth() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, '、');
    assert!(app.search.is_some(), "、 当 / 打开搜索");
    app.key(key(KeyCode::Esc));

    press(&mut app, '？');
    assert_eq!(app.overlay, Overlay::Help, "？ 当 ? 打开帮助");
    press(&mut app, 'q');

    press(&mut app, '，');
    assert!(
        matches!(app.overlay, Overlay::Settings { .. }),
        "， 当 , 打开设置"
    );
    app.key(key(KeyCode::Esc));

    // ！ 当 ! 跳到下一条等你回复的：从在跑的 c-shop-1 跳到「刚做完」的 c-shop-2。
    select_id(&mut app, "c-shop-1");
    press(&mut app, '！');
    assert_eq!(
        app.selected_entry().unwrap().session.id.as_str(),
        "c-shop-2"
    );

    assert_eq!(keys::halfwidth('。'), '.');
    assert_eq!(keys::halfwidth('x'), 'x');
}

#[test]
fn fullwidth_aliases_only_in_normal_mode() {
    // 改名输入框里打的全角符号就是字本身。
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, 'e');
    press(&mut app, '，');
    match &app.prompt {
        Some(Prompt::Rename { text, .. }) => assert!(text.ends_with('，')),
        other => panic!("{other:?}"),
    }
}

// ---- 打出中文就当搜索 ----

#[test]
fn chinese_text_opens_search_with_it() {
    let mut app = demo_app(Vendor::Claude);
    let effects = press(&mut app, '结');
    assert_eq!(
        effects,
        vec![Effect::Search {
            vendor: Vendor::Claude,
            query: "结".to_owned()
        }]
    );
    // 输入法一次提交的后面几个字接着进搜索框。
    let effects = press(&mut app, '算');
    assert_eq!(app.search.as_ref().unwrap().query, "结算");
    assert_eq!(
        effects,
        vec![Effect::Search {
            vendor: Vendor::Claude,
            query: "结算".to_owned()
        }]
    );
}

#[test]
fn other_fullwidth_punctuation_does_not_open_search() {
    let mut app = demo_app(Vendor::Claude);
    assert!(press(&mut app, '；').is_empty());
    assert!(app.search.is_none());
    assert_eq!(keys::typed_text(key(KeyCode::Char('中'))), Some('中'));
    assert_eq!(keys::typed_text(key(KeyCode::Char('a'))), None);
}

// ---- 底栏 ----

#[test]
fn bottom_bar_follows_selection() {
    let mut app = demo_app(Vendor::Claude);
    // 在跑的对话。
    select_id(&mut app, "c-shop-1");
    assert_eq!(app.selected_kind(), Selected::Running);
    let bar = bottom(&mut app);
    assert!(bar.contains("⏎ 回到它   x 结束   ? 全部按键"), "{bar}");

    // 停着的对话。
    select_id(&mut app, "c-shop-3");
    assert_eq!(app.selected_kind(), Selected::Stopped);
    let bar = bottom(&mut app);
    assert!(
        bar.contains("⏎ 接着聊   e 改名   d 删除   ? 全部按键"),
        "{bar}"
    );

    // 项目组（收起着）。
    app.key(key(KeyCode::Down));
    assert!(matches!(
        app.selected_kind(),
        Selected::Group {
            expanded: false,
            project: true
        }
    ));
    let bar = bottom(&mut app);
    assert!(
        bar.contains("⏎ 进入   n 新对话   m 新项目   e 改名   ? 全部按键"),
        "{bar}"
    );
    // → 展开后还停在组标题上，写「收起」（⏎ 会直接进到第一条对话）。
    app.key(key(KeyCode::Right));
    let bar = bottom(&mut app);
    assert!(
        bar.contains("⏎ 收起   n 新对话   m 新项目   e 改名   ? 全部按键"),
        "{bar}"
    );
}

#[test]
fn bar_always_ends_with_all_keys() {
    for sel in [
        Selected::Nothing,
        Selected::Running,
        Selected::Stopped,
        Selected::Group {
            expanded: true,
            project: false,
        },
    ] {
        let bar = keys::bar(sel);
        let last = bar.last().unwrap();
        assert_eq!((last.keys, last.label), ("?", "全部按键"), "{sel:?}");
    }
}

// ---- 右键菜单 ----

#[test]
fn right_click_on_conversation_opens_its_menu() {
    let mut app = demo_app(Vendor::Claude);
    let y = line_of(&mut app, "结算页首屏加载慢");
    assert!(right_click(&mut app, 8, y).is_empty());
    assert_eq!(
        app.selected_entry().unwrap().session.id.as_str(),
        "c-shop-3",
        "右键也选中这一行"
    );
    assert!(matches!(
        app.overlay,
        Overlay::Menu {
            sel: Selected::Stopped,
            ..
        }
    ));
    let screen = dump(&render(&mut app, W, H));
    for item in keys::menu(Selected::Stopped) {
        let line = screen
            .lines()
            .find(|l| l.contains(item.label))
            .unwrap_or_else(|| panic!("菜单里没有「{}」：\n{screen}", item.label));
        // 每项右侧标着它的单键。
        let after = &line[line.find(item.label).unwrap() + item.label.len()..];
        assert!(
            after.trim_start().starts_with(item.keys),
            "「{}」右边应是 {}：{line}",
            item.label,
            item.keys
        );
    }
    // 菜单开着时不预览别的、底栏还在。
    assert!(bottom(&mut app).contains("接着聊"));
}

#[test]
fn clicking_a_menu_item_runs_it() {
    let mut app = demo_app(Vendor::Claude);
    let y = line_of(&mut app, "结算页首屏加载慢");
    right_click(&mut app, 8, y);
    render(&mut app, W, H);
    let menu = app.areas.menu;
    let items = keys::menu(Selected::Stopped);
    let here = items
        .iter()
        .position(|i| i.action == Action::OpenHere)
        .unwrap();
    let effects = left_click(&mut app, menu.x + 3, menu.y + here as u16);
    assert_eq!(opened(&effects), Some(("c-shop-3".to_owned(), true)));
    assert_eq!(app.overlay, Overlay::None);

    // 点「改名」进改名输入框。
    right_click(&mut app, 8, y);
    render(&mut app, W, H);
    let rename = items
        .iter()
        .position(|i| i.action == Action::Rename)
        .unwrap();
    let menu = app.areas.menu;
    left_click(&mut app, menu.x + 3, menu.y + rename as u16);
    assert!(matches!(app.prompt, Some(Prompt::Rename { .. })));
}

#[test]
fn menu_keyboard_esc_arrows_and_letters() {
    let mut app = demo_app(Vendor::Claude);
    let y = line_of(&mut app, "结算页首屏加载慢");
    right_click(&mut app, 8, y);
    app.key(key(KeyCode::Esc));
    assert_eq!(app.overlay, Overlay::None, "Esc 关闭");

    // ↓ 到第二项（w 当前窗口打开），⏎ 执行。
    right_click(&mut app, 8, y);
    app.key(key(KeyCode::Down));
    let effects = app.key(key(KeyCode::Enter));
    assert_eq!(opened(&effects), Some(("c-shop-3".to_owned(), true)));

    // 直接按菜单上标的字母：和点那一项一样。
    right_click(&mut app, 8, y);
    let effects = press(&mut app, 'd');
    assert_eq!(app.overlay, Overlay::None);
    assert!(
        matches!(effects.as_slice(), [Effect::Trash(e)] if e.session.id.as_str() == "c-shop-3")
    );
}

#[test]
fn clicking_outside_menu_only_closes_it() {
    let mut app = demo_app(Vendor::Claude);
    let y = line_of(&mut app, "结算页首屏加载慢");
    right_click(&mut app, 8, y);
    render(&mut app, W, H);
    let before = app.selected_entry().unwrap().session.id.clone();
    assert!(left_click(&mut app, 100, 2).is_empty());
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.selected_entry().unwrap().session.id, before);
}

#[test]
fn right_click_on_project_header_lists_project_actions() {
    let mut app = demo_app(Vendor::Claude);
    let y = line_of(&mut app, "▸ blog");
    right_click(&mut app, 8, y);
    let Overlay::Menu { sel, .. } = app.overlay else {
        panic!("{:?}", app.overlay);
    };
    assert_eq!(
        sel,
        Selected::Group {
            expanded: false,
            project: true
        }
    );
    let screen = dump(&render(&mut app, W, H));
    for label in [
        "进入",
        "新对话",
        "改项目显示名",
        "整组归档 / 取消",
        "在 Finder 显示",
    ] {
        assert!(screen.contains(label), "缺「{label}」：\n{screen}");
    }
    // 第一项「展开」：⏎ 执行就展开这组。
    let effects = app.key(key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::SetExpanded {
            project: "/Users/demo/projects/blog".into(),
            expanded: true
        }]
    );
}

#[test]
fn right_click_on_running_conversation_offers_kill() {
    let mut app = demo_app(Vendor::Claude);
    let y = line_of(&mut app, "◐ 购物车结算按钮");
    right_click(&mut app, 8, y);
    assert!(matches!(
        app.overlay,
        Overlay::Menu {
            sel: Selected::Running,
            ..
        }
    ));
    let labels: Vec<_> = keys::menu(Selected::Running)
        .iter()
        .map(|i| (i.keys, i.label))
        .collect();
    assert_eq!(labels[0], ("⏎", "回到它"));
    assert!(labels.contains(&("x", "结束")));
    // 菜单开着时实时预览照常（只有帮助页、设置页挡住预览）。
    assert!(app.live_target().is_some());
}

#[test]
fn right_click_on_empty_space_does_nothing() {
    let mut app = demo_app(Vendor::Claude);
    render(&mut app, W, H);
    assert!(right_click(&mut app, 8, H - 3).is_empty());
    assert_eq!(app.overlay, Overlay::None);
}

#[test]
fn menu_stays_on_screen_near_the_bottom_right() {
    let mut app = demo_app(Vendor::Claude);
    let y = line_of(&mut app, "结算页首屏加载慢");
    app.mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 30,
            row: y,
            modifiers: KeyModifiers::NONE,
        },
        Instant::now(),
    );
    // 窗口很矮很窄也不越界。
    for (w, h) in [(W, H), (40, 12), (20, 6)] {
        render(&mut app, w, h);
        let m = app.areas.menu;
        assert!(m.right() <= w && m.bottom() <= h, "{w}x{h}: {m:?}");
    }
}

// ---- w ----

#[test]
fn w_opens_in_current_window() {
    let mut app = demo_app(Vendor::Claude);
    select_id(&mut app, "c-shop-3");
    assert_eq!(
        opened(&press(&mut app, 'w')),
        Some(("c-shop-3".to_owned(), true))
    );
    let help = {
        press(&mut app, '?');
        dump(&render(&mut app, W, 40))
    };
    assert!(help.contains("⌥⏎ 要终端把 Option 当 Alt"), "{help}");
    assert!(help.contains("不然用 w"));
}

// ---- 设置 ----

#[test]
fn settings_toggle_auto_ascii_input() {
    let mut app = demo_app(Vendor::Claude);
    assert!(app.config.auto_ascii_input, "默认开");
    press(&mut app, ',');
    for _ in 0..5 {
        app.key(key(KeyCode::Down));
    }
    assert!(dump(&render(&mut app, W, H)).contains("自动英文"));
    app.key(key(KeyCode::Right));
    assert!(!app.config.auto_ascii_input);
    match app.key(key(KeyCode::Esc)).as_slice() {
        [Effect::SaveConfig(cfg)] => assert!(!cfg.auto_ascii_input),
        other => panic!("{other:?}"),
    }
}

#[test]
fn menu_opens_below_the_clicked_row_and_keeps_its_left_edge() {
    let mut app = demo_app(Vendor::Claude);
    let y = line_of(&mut app, "结算页首屏加载慢");
    // 第 12 格是「首」的后半格：菜单左边的宽字符要擦掉，不然盖住选中项的细条。
    right_click(&mut app, 12, y);
    let buf = render(&mut app, W, H);
    let m = app.areas.menu;
    assert_eq!(m.y, y + 2, "上面留一行空，菜单框从被点那行的下一行开始");
    assert_eq!(buf[(m.x, m.y)].symbol(), "▎", "选中的第一项左边有细条");
    assert!(
        dump(&buf)
            .lines()
            .nth(usize::from(y))
            .unwrap()
            .contains("结算页首屏加载慢"),
        "被点的那一行不被挡住"
    );
}
