//! 中断恢复在会话台里：顶部「被中断」组、R 全部接着做、单条 ⏎ 只打开、x 移出，设置里的开机自动打开开关。

mod ui_support;

use std::collections::HashSet;

use huitai::core::{SessionId, Vendor};
use huitai::ui::App;
use huitai::ui::app::Effect;
use huitai::ui::rows::{GroupKey, Row};
use ratatui::crossterm::event::KeyCode;

use ui_support::*;

fn ids(list: &[&str]) -> HashSet<SessionId> {
    list.iter().map(|s| SessionId::from(*s)).collect()
}

fn press(app: &mut App, c: char) -> Vec<Effect> {
    app.key(key(KeyCode::Char(c)))
}

fn selected_id(app: &App) -> Option<String> {
    app.selected_entry().map(|e| e.session.id.to_string())
}

/// Claude 没有被中断的，Codex、pi 各一条。
fn interrupted_app() -> App {
    let mut app = demo_app(Vendor::Claude);
    app.set_interrupted(ids(&["x-api-2", "p-blog-2"]));
    app
}

#[test]
fn first_report_jumps_to_interrupted_group() {
    let app = interrupted_app();
    // 第一家有被中断对话的是 Codex，停在「被中断」组标题上。
    assert_eq!(app.vendor, Vendor::Codex);
    let l = app.list();
    assert_eq!(l.groups[0].key, GroupKey::Interrupted);
    assert_eq!(l.groups[0].label, "被中断");
    assert_eq!(l.rows[l.selected], Row::Header(0));
    // 被中断的只在这一组里，不在它的项目组里重复出现。
    let item_ids: Vec<&str> = l
        .rows
        .iter()
        .filter_map(|r| match r {
            Row::Item { entry, .. } => Some(l.entries[*entry].session.id.as_str()),
            Row::Header(_) => None,
        })
        .collect();
    assert_eq!(item_ids.iter().filter(|i| **i == "x-api-2").count(), 1);
    assert_eq!(item_ids[0], "x-api-2");
    // Claude 那家没有这一组。
    assert!(
        app.list_of(Vendor::Claude)
            .groups
            .iter()
            .all(|g| g.key != GroupKey::Interrupted)
    );
}

#[test]
fn later_reports_do_not_steal_selection() {
    let mut app = interrupted_app();
    app.key(key(KeyCode::Tab));
    assert_eq!(app.vendor, Vendor::Pi);
    app.set_interrupted(ids(&["p-blog-2"]));
    assert_eq!(app.vendor, Vendor::Pi);
    // 一条都没有了：组消失。
    app.set_interrupted(HashSet::new());
    assert!(
        Vendor::ALL
            .iter()
            .all(|v| app.list_of(*v).groups[0].key != GroupKey::Interrupted)
    );
}

#[test]
fn no_interrupted_conversations_keeps_the_list_as_is() {
    let mut app = demo_app(Vendor::Claude);
    app.set_interrupted(HashSet::new());
    assert_eq!(app.vendor, Vendor::Claude);
    assert_eq!(selected_id(&app).as_deref(), Some("c-shop-1"));
    // 没有被中断的：R 是恢复上次所有标签。
    assert_eq!(press(&mut app, 'R'), vec![Effect::RestoreTabs]);
}

#[test]
fn r_resumes_every_interrupted_conversation_of_all_vendors() {
    let mut app = interrupted_app();
    let effects = press(&mut app, 'R');
    let [Effect::ResumeInterrupted(entries)] = effects.as_slice() else {
        panic!("R 应该接着做被中断的，得到 {effects:?}");
    };
    let mut got: Vec<&str> = entries.iter().map(|e| e.session.id.as_str()).collect();
    got.sort_unstable();
    assert_eq!(got, ["p-blog-2", "x-api-2"]);
}

#[test]
fn enter_on_one_only_opens_it() {
    let mut app = interrupted_app();
    app.key(key(KeyCode::Down));
    assert_eq!(selected_id(&app).as_deref(), Some("x-api-2"));
    let effects = app.key(key(KeyCode::Enter));
    let [Effect::Open { entry, here: false }] = effects.as_slice() else {
        panic!("⏎ 应该只是打开，得到 {effects:?}");
    };
    assert_eq!(entry.session.id.as_str(), "x-api-2");
}

#[test]
fn x_on_interrupted_dismisses_without_asking() {
    let mut app = interrupted_app();
    app.key(key(KeyCode::Down));
    assert_eq!(
        press(&mut app, 'x'),
        vec![Effect::Dismiss(SessionId::from("x-api-2"))]
    );
    assert!(app.prompt.is_none());
}

#[test]
fn settings_toggles_autostart() {
    let mut app = demo_app(Vendor::Claude);
    assert!(!app.autostart, "默认关");
    press(&mut app, ',');
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Down));
    assert_eq!(
        app.key(key(KeyCode::Right)),
        vec![Effect::SetAutostart(true)]
    );
    assert!(app.autostart);
    let text = dump(&render(&mut app, 120, 30));
    assert!(text.contains("开机打开"), "{text}");
    assert_snapshot("settings", &text);
    assert_eq!(
        app.key(key(KeyCode::Left)),
        vec![Effect::SetAutostart(false)]
    );
    assert!(!app.autostart);
}

#[test]
fn interrupted_group_renders_at_top() {
    let mut app = interrupted_app();
    let text = dump(&render(&mut app, 120, 30));
    assert!(text.contains("被中断"));
    assert!(text.contains("R 全部接着做"));
    assert_snapshot("interrupted", &text);
    // 选中被中断的单条：预览里提示 ⏎ 只打开。
    app.key(key(KeyCode::Down));
    load_preview(&mut app);
    let text = dump(&render(&mut app, 120, 30));
    assert!(text.contains("⏎ 只打开"), "{text}");
    assert_snapshot("interrupted-item", &text);
}
