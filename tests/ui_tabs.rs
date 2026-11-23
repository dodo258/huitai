//! 顶栏标签：做成一排按钮（垫底色，看得出能点）；没装的那家（数据目录不在、也没有对话）不显示，Tab 跳过它。

mod ui_support;

use huitai::core::Vendor;
use huitai::ui::{App, Theme};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use ui_support::*;

const W: u16 = 120;
const H: u16 = 30;

fn top(app: &mut App) -> String {
    dump(&render(app, W, H))
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn tab(app: &mut App, shift: bool) {
    let k = if shift {
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)
    } else {
        key(KeyCode::Tab)
    };
    app.key(k);
}

#[test]
fn missing_vendor_is_hidden_and_skipped() {
    let mut app = demo_app(Vendor::Codex);
    app.set_entries(Vendor::Pi, Vec::new());
    app.set_entries(Vendor::Hermes, Vec::new());
    app.set_entries(Vendor::Kimi, Vec::new());
    app.set_entries(Vendor::Gemini, Vec::new());
    app.set_entries(Vendor::OpenCode, Vec::new());
    app.set_entries(Vendor::Antigravity, Vec::new());
    app.hide_missing(|v| {
        !matches!(
            v,
            Vendor::Pi
                | Vendor::Hermes
                | Vendor::Kimi
                | Vendor::Gemini
                | Vendor::OpenCode
                | Vendor::Antigravity
        )
    });
    assert_eq!(app.shown, vec![Vendor::Claude, Vendor::Codex]);
    let line = top(&mut app);
    assert!(!line.contains("π pi"), "{line}");
    assert!(app.areas.tabs.iter().all(|(v, _)| *v != Vendor::Pi));
    tab(&mut app, false);
    assert_eq!(app.vendor, Vendor::Claude, "Codex 往后是 Claude，跳过 pi");
    tab(&mut app, true);
    assert_eq!(app.vendor, Vendor::Codex);
}

#[test]
fn vendor_with_cached_conversations_stays_even_if_dir_is_gone() {
    let mut app = demo_app(Vendor::Claude);
    app.hide_missing(|v| v != Vendor::Pi);
    assert_eq!(app.shown, Vendor::ALL.to_vec(), "pi 还有对话，照样显示");
}

#[test]
fn nothing_installed_shows_all_three() {
    let mut app = demo_app(Vendor::Claude);
    for v in Vendor::ALL {
        app.set_entries(v, Vec::new());
    }
    app.hide_missing(|_| false);
    assert_eq!(app.shown, Vendor::ALL.to_vec());
}

#[test]
fn hidden_current_vendor_moves_to_first_shown() {
    let mut app = demo_app(Vendor::Pi);
    app.set_entries(Vendor::Pi, Vec::new());
    app.hide_missing(|v| v != Vendor::Pi);
    assert_eq!(app.vendor, Vendor::Claude);
}

#[test]
fn tabs_read_as_tabs_without_background_blocks() {
    // 照 gitui / ratatui Tabs：不垫底色；标签之间一道淡竖线；当前这家强调色加粗加下划线，别的家次要色。
    let mut app = demo_app(Vendor::Codex);
    only_first_four(&mut app);
    let buf = render(&mut app, W, H);
    let t = Theme::DARK;
    let line = top(&mut app);
    assert!(line.contains("✻ Claude │ ◎ Codex │ π pi"), "{line}");
    for (v, r) in app.areas.tabs.clone() {
        let c = &buf[(r.x + 2, r.y)];
        assert_eq!(c.bg, t.bar, "{v} 不垫底色");
        if v == Vendor::Codex {
            assert_eq!(c.fg, t.accent(v));
            assert!(c.modifier.contains(ratatui::style::Modifier::UNDERLINED));
        } else {
            assert_eq!(c.fg, t.muted);
            assert!(!c.modifier.contains(ratatui::style::Modifier::UNDERLINED));
        }
    }
}

#[test]
fn very_narrow_terminal_shrinks_other_tabs_to_marks() {
    let mut app = demo_app(Vendor::Codex);
    only_first_four(&mut app);
    // 别的都去掉了还放不下四家全名：别的家只留标记，当前这家写全名，标签还能点。
    let line = dump(&render(&mut app, 40, H))
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    assert!(line.contains("✻ │ ◎ Codex │ π │ ☤"), "{line}");
    let shown: Vec<Vendor> = app.areas.tabs.iter().map(|(v, _)| *v).collect();
    assert_eq!(shown, app.shown);
    // 宽的时候照旧写全名。
    assert!(top(&mut app).contains("✻ Claude │ ◎ Codex │ π pi │ ☤ Hermes"));
}
