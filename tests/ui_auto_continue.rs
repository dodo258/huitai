//! 撞额度后到重置时间自动继续：对话菜单里的开关，和假时间下什么时候发、什么时候不发。

mod ui_support;

use std::time::{Duration, SystemTime};

use huitai::core::{SessionId, SessionState, Vendor};
use huitai::index::Edit;
use huitai::sources::{LimitHit, Moment};
use huitai::ui::keys::Action;
use huitai::ui::{App, Effect};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ui_support::{claude_entries, demo_app, key, now, render};

/// 「给博客加 RSS 订阅」这条：演示数据里是等你回复。
const ID: &str = "c-blog-1";

/// 撞了上限、重置后 1 分钟是 `now() + 2 小时`。
fn resume_at() -> SystemTime {
    now() + Duration::from_secs(2 * 3600)
}

fn unix(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// 演示数据里 `c-blog-1` 开了自动继续（`continued` 是已经为哪次上限继续过），最后一条是撞上限。
fn armed(continued: Option<i64>) -> App {
    let mut app = demo_app(Vendor::Claude);
    let entries = claude_entries()
        .into_iter()
        .map(|mut e| {
            if e.session.id.as_str() == ID {
                e.note.auto_continue = true;
                e.note.continued = continued;
            }
            e
        })
        .collect();
    app.set_entries(Vendor::Claude, entries);
    app.set_moment(SessionId::from(ID), limited(Some(resume_at())));
    app
}

fn limited(resume_at: Option<SystemTime>) -> Moment {
    Moment {
        limit: Some(LimitHit { resume_at }),
        ..Moment::default()
    }
}

fn after() -> SystemTime {
    resume_at() + Duration::from_secs(1)
}

fn sent(effects: &[Effect]) -> Vec<(&str, &str, i64)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::AutoContinue {
                entry,
                hosted,
                resume_at,
            } => Some((entry.session.id.as_str(), hosted.as_str(), *resume_at)),
            _ => None,
        })
        .collect()
}

#[test]
fn fires_once_after_the_reset_time() {
    let mut app = armed(None);
    assert_eq!(app.state_of(&SessionId::from(ID)), SessionState::Waiting);
    // 没到点。
    assert!(
        app.auto_continue_due(resume_at() - Duration::from_secs(1))
            .is_empty()
    );
    // 到点了：发一次。
    let effects = app.auto_continue_due(after());
    assert_eq!(sent(&effects), vec![(ID, ID, unix(resume_at()))]);
    // 标注还没写回来之前，再到 tick 也不重复发。
    assert!(
        app.auto_continue_due(after() + Duration::from_secs(60))
            .is_empty()
    );
}

#[test]
fn done_and_reclaimed_count_as_stopped_for_input() {
    // 刚做完（没看过）也是停着等输入。
    let mut app = armed(None);
    app.states.insert(SessionId::from(ID), SessionState::Done);
    assert_eq!(sent(&app.auto_continue_due(after())).len(), 1);
    // 闲置收回了（不托管了）、也没在别处开着：照样接着做。
    let mut app = armed(None);
    app.states.remove(&SessionId::from(ID));
    assert_eq!(sent(&app.auto_continue_due(after())).len(), 1);
}

#[test]
fn does_not_fire() {
    // 已经为这次上限送过（标注里记着）。
    let mut app = armed(Some(unix(resume_at())));
    assert!(app.auto_continue_due(after()).is_empty(), "已送过");
    // 关着。
    let mut app = demo_app(Vendor::Claude);
    app.set_moment(SessionId::from(ID), limited(Some(resume_at())));
    assert!(app.auto_continue_due(after()).is_empty(), "关着");
    // 在跑（不是等你）。
    let mut app = armed(None);
    app.states
        .insert(SessionId::from(ID), SessionState::Running);
    assert!(app.auto_continue_due(after()).is_empty(), "在跑");
    // 不托管、但别处的 CLI 开着它：不能再起一个。
    let mut app = armed(None);
    app.states.remove(&SessionId::from(ID));
    app.open_elsewhere.insert(SessionId::from(ID));
    assert!(app.auto_continue_due(after()).is_empty(), "别处开着");
    // 最后一条撞了上限但解析不出重置时间。
    let mut app = armed(None);
    app.set_moment(SessionId::from(ID), limited(None));
    assert!(app.auto_continue_due(after()).is_empty(), "没有重置时间");
    // 最后一条不是撞上限。
    let mut app = armed(None);
    app.set_moment(SessionId::from(ID), Moment::default());
    assert!(app.auto_continue_due(after()).is_empty(), "没撞上限");
}

#[test]
fn a_new_limit_after_continuing_fires_again() {
    // 上次为早一点的那次上限继续过，现在是新的一次。
    let mut app = armed(Some(unix(now())));
    assert_eq!(sent(&app.auto_continue_due(after())).len(), 1);
}

/// 跳到 blog 项目（第 2 个），停在 `c-blog-1` 上。
fn to_blog(app: &mut App) {
    let _ = app.key(key(KeyCode::Char('2')));
    assert_eq!(
        app.selected_entry().map(|e| e.session.id.as_str()),
        Some(ID)
    );
}

fn shift_a() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)
}

#[test]
fn menu_toggle_only_where_limits_are_readable() {
    let mut app = demo_app(Vendor::Claude);
    to_blog(&mut app);
    let sel = app.selected_kind();
    let item = app
        .menu_items(sel)
        .into_iter()
        .find(|i| i.action == Action::AutoContinue)
        .expect("Claude 的对话有这个开关");
    assert_eq!((item.keys, item.label), ("A", "撞额度后到点自动继续"));
    // 按 A 和点那一项一样：打开开关。
    let effects = app.key(shift_a());
    assert_eq!(
        effects,
        vec![Effect::Annotate {
            vendor: Vendor::Claude,
            id: SessionId::from(ID),
            edit: Edit::AutoContinue(true),
        }]
    );

    // 开着时菜单写「开」，再按就关。
    let mut app = armed(None);
    to_blog(&mut app);
    let sel = app.selected_kind();
    let labels: Vec<_> = app.menu_items(sel).iter().map(|i| i.label).collect();
    assert!(
        labels.contains(&"撞额度后自动继续：开（再按关）"),
        "{labels:?}"
    );
    assert!(matches!(
        app.key(shift_a()).as_slice(),
        [Effect::Annotate {
            edit: Edit::AutoContinue(false),
            ..
        }]
    ));

    // pi 认不出撞额度：菜单里没有，按 A 只提示。
    let mut app = demo_app(Vendor::Pi);
    let _ = app.key(key(KeyCode::Char('1')));
    assert_eq!(
        app.selected_entry().map(|e| e.session.vendor),
        Some(Vendor::Pi)
    );
    let sel = app.selected_kind();
    assert!(
        !app.menu_items(sel)
            .iter()
            .any(|i| i.action == Action::AutoContinue)
    );
    assert!(app.key(shift_a()).is_empty());
    assert!(app.status.as_deref().is_some_and(|s| s.contains("认不出")));
}

#[test]
fn continued_rows_say_so() {
    let mut app = armed(Some(unix(resume_at())));
    to_blog(&mut app);
    let text = ui_support::dump(&render(&mut app, 132, 34));
    // 只有继续过的那一行标，写在它的标题后面。
    let rows: Vec<_> = text.lines().filter(|l| l.contains("已自动继续")).collect();
    assert_eq!(rows.len(), 1, "{text}");
    assert!(rows[0].contains("给博客加 RSS 订阅  已自动继续"), "{text}");
}
