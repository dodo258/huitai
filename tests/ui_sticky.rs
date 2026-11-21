//! 展开的组标题整行垫底色；滚到组中间时组标题钉在列表第一行，看得到、点得到，选中的对话不会被它盖住。

mod ui_support;

use std::path::PathBuf;
use std::time::Instant;

use huitai::core::Vendor;
use huitai::ui::App;
use huitai::ui::rows::{GroupKey, Row};
use huitai::ui::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use ui_support::*;

const W: u16 = 120;
/// 列表区只有 3 行高，shop 展开的 3 条对话加上前面的置顶组放不下，得滚。
const H: u16 = 6;

fn shop_header(app: &App) -> usize {
    let l = app.list();
    let key = GroupKey::Project(PathBuf::from("/Users/demo/projects/shop"));
    l.rows
        .iter()
        .position(|r| matches!(*r, Row::Header(g) if l.groups[g].key == key))
        .expect("演示数据里有 shop")
}

/// 选中 shop 组标题、确保展开，再往下走到组里最后一条对话。
fn scrolled_into_shop() -> App {
    let mut app = demo_app(Vendor::Claude);
    render(&mut app, W, H);
    let h = shop_header(&app);
    while app.list().selected != h {
        let down = app.list().selected < h;
        app.key(key(if down { KeyCode::Down } else { KeyCode::Up }));
    }
    app.key(key(KeyCode::Right));
    let h = shop_header(&app);
    let last = app
        .list()
        .rows
        .iter()
        .rposition(
            |r| matches!(*r, Row::Item { group, .. } if Row::Header(group) == app.list().rows[h]),
        )
        .expect("shop 展开后有对话");
    while app.list().selected < last {
        app.key(key(KeyCode::Down));
        render(&mut app, W, H);
    }
    app
}

fn line_text(buf: &Buffer, y: u16) -> String {
    dump(buf)
        .lines()
        .nth(usize::from(y))
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn group_header_sticks_to_the_top_when_scrolled_inside() {
    let mut app = scrolled_into_shop();
    let buf = render(&mut app, W, H);
    let list = app.areas.list;
    let h = shop_header(&app);
    assert!(app.list().offset > h, "组标题已经滚出去了");
    assert_eq!(app.list().sticky(), Some(h));
    let top = line_text(&buf, list.y);
    assert!(
        top.contains("shop") && top.contains('▾'),
        "第一行是钉住的组标题：{top}"
    );
    // 钉住的那行和展开的组标题一样垫着底色。
    assert_eq!(
        buf[(list.x + 20, list.y)].bg,
        Theme::DARK.band(Theme::DARK.accent(Vendor::Claude))
    );
}

#[test]
fn selected_item_is_never_hidden_under_the_sticky_header() {
    let mut app = scrolled_into_shop();
    // 往上走回组里第一条对话，每一步选中的都在第一行以下。
    let h = shop_header(&app);
    while app.list().selected > h + 1 {
        app.key(key(KeyCode::Up));
        render(&mut app, W, H);
        let l = app.list();
        if l.sticky().is_some() {
            assert!(l.selected > l.offset, "选中的对话被钉住的组标题盖住了");
        }
    }
}

#[test]
fn clicking_the_sticky_header_selects_the_group() {
    let mut app = scrolled_into_shop();
    render(&mut app, W, H);
    let list = app.areas.list;
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: list.x + 10,
        row: list.y,
        modifiers: KeyModifiers::NONE,
    };
    app.mouse(click, Instant::now());
    assert_eq!(app.list().selected, shop_header(&app));
    // 选中组标题后按 ← 收起。
    app.key(key(KeyCode::Left));
    let buf = render(&mut app, W, H);
    assert!(app.list().sticky().is_none());
    assert!(dump(&buf).contains("▸ shop"));
}

#[test]
fn expanded_headers_are_tinted_collapsed_ones_are_not() {
    let mut app = demo_app(Vendor::Claude);
    let buf = render(&mut app, W, 30);
    let list = app.areas.list;
    let l = app.list();
    let mut seen = (false, false);
    for (i, r) in l.rows.iter().enumerate().skip(l.offset) {
        let Row::Header(g) = *r else { continue };
        if i == l.selected || i - l.offset >= usize::from(list.height) {
            continue;
        }
        let y = list.y + (i - l.offset) as u16;
        let bg = buf[(list.x + 20, y)].bg;
        if app.folds.is_collapsed(&l.groups[g].key) {
            assert_eq!(bg, Theme::DARK.base, "收起的组标题不垫底色");
            seen.0 = true;
        } else {
            assert_eq!(
                bg,
                Theme::DARK.band(Theme::DARK.accent(Vendor::Claude)),
                "展开的组标题垫底色"
            );
            seen.1 = true;
        }
    }
    assert!(seen.0 || seen.1, "屏幕上至少有一个没选中的组标题");
}
