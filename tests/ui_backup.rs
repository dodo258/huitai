//! Claude 对话被删的提醒和只剩备份的对话在界面上：离删除不到 3 天的预览区标题行标「约 X 天后被 Claude 删除」，
//! 原文件没了的行尾标「只剩备份」，回车提示接不上，菜单和 `P` 放回 Claude 目录。

mod ui_support;

use std::path::PathBuf;
use std::time::Duration;

use huitai::core::Vendor;
use huitai::ui::App;
use huitai::ui::app::{BACKUP_ONLY_OPEN, Effect};
use huitai::ui::keys::Action;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use ui_support::*;

/// 演示数据里「结算页首屏加载慢」只剩备份；Claude 留对话 4 天，
/// 「把优惠券接口换成新版」挪到 84 小时前，还剩 12 小时。
fn app() -> App {
    let mut app = demo_app(Vendor::Claude);
    let mut entries = claude_entries();
    for e in &mut entries {
        match e.session.id.as_str() {
            "c-shop-3" => {
                e.session.path = PathBuf::from(
                    "/Users/demo/Library/Application Support/huitai/backups/claude/-Users-demo-projects-shop/c-shop-3.jsonl.zst",
                );
            }
            "c-shop-2" => e.session.last_active = now() - Duration::from_secs(84 * 3600),
            _ => {}
        }
    }
    app.set_entries(Vendor::Claude, entries);
    app.retention.insert(String::new(), 4);
    app
}

fn screen(app: &mut App) -> String {
    load_preview(app);
    dump(&render(app, 120, 30))
}

#[test]
fn near_deletion_is_flagged_in_the_preview_head() {
    let mut app = app();
    // 刚动过的不提醒，过了保留期的说下次开 Claude 时删。
    let shop1 = claude_entries()
        .into_iter()
        .find(|e| e.session.id.as_str() == "c-shop-1")
        .unwrap();
    assert_eq!(app.deletion_note(&shop1), None);
    let mut old = shop1.clone();
    old.session.last_active = now() - Duration::from_secs(5 * 86_400);
    assert_eq!(
        app.deletion_note(&old).as_deref(),
        Some("已过 Claude 的保留期，下次开 Claude 时会被删")
    );
    // 别的家不提醒。
    old.session.vendor = Vendor::Codex;
    assert_eq!(app.deletion_note(&old), None);
    select_id(&mut app, "c-shop-2");
    let s = screen(&mut app);
    assert!(s.contains("约 1 天后被 Claude 删除"), "{s}");
    // 只剩备份的不提醒（已经删了）。
    select_id(&mut app, "c-shop-3");
    assert!(!screen(&mut app).contains("被 Claude 删除"));
    // 默认 30 天时 30 小时前动过的不提醒。
    let mut app = demo_app(Vendor::Claude);
    select_id(&mut app, "c-shop-3");
    assert!(!screen(&mut app).contains("被 Claude 删除"));
}

#[test]
fn backup_only_row_is_marked_and_cannot_be_opened_directly() {
    let mut app = app();
    let s = screen(&mut app);
    let row = s
        .lines()
        .find(|l| l.contains("结算页首屏加载慢"))
        .expect("列表里照常有这条");
    assert!(row.contains("只剩备份"), "{row}");
    assert_eq!(s.matches("只剩备份").count(), 1, "{s}");

    select_id(&mut app, "c-shop-3");
    assert!(app.key(key(KeyCode::Enter)).is_empty());
    assert_eq!(app.status.as_deref(), Some(BACKUP_ONLY_OPEN));

    // 菜单最前面是「放回 Claude 目录再接着聊」，按 P 和点它一样。
    let sel = app.selected_kind();
    let first = app.menu_items(sel)[0];
    assert_eq!(
        (first.action, first.keys, first.label),
        (Action::PutBack, "P", "放回 Claude 目录再接着聊")
    );
    match app
        .key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT))
        .as_slice()
    {
        [Effect::PutBack(e)] => assert_eq!(e.session.id.as_str(), "c-shop-3"),
        other => panic!("{other:?}"),
    }

    // 原文件还在的对话菜单里没有这一项，按 P 只提示。
    let mut app = demo_app(Vendor::Claude);
    select_id(&mut app, "c-shop-3");
    let sel = app.selected_kind();
    assert!(
        !app.menu_items(sel)
            .iter()
            .any(|i| i.action == Action::PutBack)
    );
    assert!(
        app.key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT))
            .is_empty()
    );
}
