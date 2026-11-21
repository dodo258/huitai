//! 提示栏、搜索栏里的按键说明：按键用这家的强调色加粗，和底栏、帮助一致；不以按键开头的说明用暗色。

mod ui_support;

use huitai::core::Vendor;
use huitai::ui::Theme;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::Modifier;

use ui_support::*;

const W: u16 = 120;
const H: u16 = 30;

/// 最后一行里第一次出现 `s` 的那一格。
fn cell_of<'a>(buf: &'a Buffer, s: &str) -> &'a ratatui::buffer::Cell {
    let y = H - 1;
    let x = (0..W)
        .find(|&x| buf[(x, y)].symbol() == s)
        .unwrap_or_else(|| panic!("底栏没有 {s}"));
    &buf[(x, y)]
}

#[test]
fn rename_prompt_keys_use_accent() {
    let mut app = demo_app(Vendor::Claude);
    select_id(&mut app, "c-shop-1");
    app.key(key(KeyCode::Char('e')));
    let buf = render(&mut app, W, H);
    let accent = Theme::DARK.accent(Vendor::Claude);
    let enter = cell_of(&buf, "⏎");
    assert_eq!(enter.fg, accent);
    assert!(enter.modifier.contains(Modifier::BOLD));
    assert_eq!(cell_of(&buf, "E").fg, accent, "Esc 也是按键");
    assert_eq!(
        cell_of(&buf, "留").fg,
        Theme::DARK.faint,
        "「留空恢复原名」不是按键"
    );
}

#[test]
fn search_bar_keys_use_accent() {
    let mut app = demo_app(Vendor::Codex);
    app.key(key(KeyCode::Char('/')));
    let buf = render(&mut app, W, H);
    assert_eq!(cell_of(&buf, "⏎").fg, Theme::DARK.accent(Vendor::Codex));
}

#[test]
fn new_project_key_is_offered_on_project_groups() {
    // 项目上：底栏、预览的按键列表、右键菜单都有 `N 新项目`，菜单点它和按 N 一样弹出选目录的输入框。
    let mut app = demo_app(Vendor::Claude);
    let shop = app
        .list()
        .rows
        .iter()
        .position(|r| {
            matches!(*r, huitai::ui::rows::Row::Header(g)
                if matches!(app.list().groups[g].key, huitai::ui::rows::GroupKey::Project(_)))
        })
        .expect("有项目组");
    while app.list().selected != shop {
        app.key(key(KeyCode::Up));
    }
    let text = dump(&render(&mut app, W, H));
    let bar = text.lines().last().unwrap_or_default();
    assert!(bar.contains("N 新项目"), "{bar}");
    assert!(
        text.contains("N  新建项目，或在别的目录开对话"),
        "预览里要有 N"
    );
    let item = huitai::ui::keys::menu(app.selected_kind())
        .into_iter()
        .position(|m| m.keys == "N")
        .expect("右键菜单里要有 N");
    assert_eq!(
        huitai::ui::keys::menu(app.selected_kind())[item].action,
        huitai::ui::keys::Action::NewElsewhere
    );
    app.key(key(KeyCode::Char('N')));
    assert!(
        matches!(app.prompt, Some(huitai::ui::app::Prompt::NewDir { .. })),
        "按 N 弹出选目录"
    );
}
