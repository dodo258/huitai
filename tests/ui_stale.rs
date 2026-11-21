//! 一个月以上没聊的项目收进「很久没聊」分区（默认收起、不编号，回车打开）；底栏「路径：」单击在访达里打开。

mod ui_support;

use std::path::PathBuf;
use std::time::Duration;

use huitai::core::Vendor;
use huitai::ui::App;
use huitai::ui::app::Effect;
use huitai::ui::rows::{GroupKey, Row};
use ratatui::crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use ui_support::*;

const W: u16 = 120;
const H: u16 = 30;
const DAY: u64 = 24 * 60;

/// 演示数据再加两个很久没聊的项目。
fn app_with_stale() -> App {
    let mut app = demo_app(Vendor::Claude);
    let mut entries = claude_entries();
    for (id, cwd, title, days) in [
        (
            "c-resume-1",
            "/Users/demo/projects/resume",
            "简历改成两栏",
            45,
        ),
        (
            "c-dot-1",
            "/Users/demo/projects/dotfiles",
            "zsh 启动太慢",
            90,
        ),
    ] {
        let mut e = entry(
            Vendor::Claude,
            &Fake {
                id,
                cwd,
                title,
                mins: days * DAY,
                messages: 6,
                branch: None,
                pinned: false,
                archived: false,
                automatic: false,
            },
        );
        e.session.last_active = now() - Duration::from_secs(days * DAY * 60);
        entries.push(e);
    }
    app.set_entries(Vendor::Claude, entries);
    app
}

fn project(p: &str) -> GroupKey {
    GroupKey::Project(PathBuf::from(p))
}

fn visible_headers(app: &App) -> Vec<GroupKey> {
    let l = app.list();
    l.rows
        .iter()
        .filter_map(|r| match r {
            Row::Header(g) => Some(l.groups[*g].key.clone()),
            Row::Item { .. } => None,
        })
        .collect()
}

#[test]
fn stale_projects_fold_under_their_own_section() {
    let mut app = app_with_stale();
    let l = app.list();
    let section = l
        .groups
        .iter()
        .position(|g| g.key == GroupKey::Stale)
        .expect("有「很久没聊」分区");
    let stale: Vec<_> = l.groups.iter().filter(|g| g.stale).collect();
    assert_eq!(stale.len(), 2);
    assert!(
        stale.iter().all(|g| g.number.is_none()),
        "分区里的项目不编号"
    );
    assert!(
        l.groups[section + 1..section + 3].iter().all(|g| g.stale),
        "项目紧跟在分区标题后面"
    );
    // 最近的项目照常编号，不受影响。
    assert_eq!(
        l.groups.iter().filter_map(|g| g.number).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );

    // 默认收起：分区标题在，下面的项目不显示。
    let heads = visible_headers(&app);
    assert!(heads.contains(&GroupKey::Stale));
    assert!(!heads.contains(&project("/Users/demo/projects/resume")));
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("▸ 很久没聊  2 个项目 · 一个月以上"), "{text}");

    // 选中分区标题回车：打开，两个项目出现，选中停在分区标题上。
    while app.selected_entry().is_some()
        || app.selected_group().map(|g| &g.key) != Some(&GroupKey::Stale)
    {
        app.key(key(KeyCode::Down));
    }
    app.key(key(KeyCode::Enter));
    let heads = visible_headers(&app);
    assert!(heads.contains(&project("/Users/demo/projects/resume")));
    assert!(heads.contains(&project("/Users/demo/projects/dotfiles")));
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("▾ 很久没聊"), "{text}");
    assert!(text.contains("resume"), "{text}");
}

#[test]
fn launching_inside_a_stale_project_opens_the_section() {
    let mut app = App::new(
        PathBuf::from("/Users/demo"),
        PathBuf::from("/Users/demo/projects/resume"),
        huitai::core::Config::default(),
    );
    app.now = now();
    let fresh = app_with_stale();
    let entries = fresh.list().entries.clone();
    app.set_entries(Vendor::Claude, entries);
    let e = app.selected_entry().expect("选中启动目录那个项目的第一条");
    assert_eq!(e.session.id.as_str(), "c-resume-1");
}

#[test]
fn clicking_the_path_opens_finder() {
    let mut app = demo_app(Vendor::Claude);
    let buf = render(&mut app, W, H);
    let text = dump(&buf);
    let bottom = text.lines().last().unwrap();
    assert!(bottom.contains("路径：~/projects/shop"), "{bottom}");
    let r = app.areas.path;
    assert!(r.width > 0, "记下了路径的位置");
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: r.x + r.width - 2,
        row: r.y,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(
        app.mouse(click, std::time::Instant::now()),
        vec![Effect::Finder(PathBuf::from("/Users/demo/projects/shop"))]
    );
}

#[test]
fn clicking_elsewhere_opens_the_stale_section_to_reach_it() {
    // 很久没聊的项目里有一条在别处开着（进程表里认出来，文件没动）：顶栏「●1 别处」单击要能跳到它，
    // 分区收着也一样（先把分区打开）。
    let mut app = app_with_stale();
    app.open_elsewhere
        .insert(huitai::core::SessionId::from("c-resume-1"));
    let buf = render(&mut app, W, H);
    assert!(dump(&buf).lines().next().unwrap().contains("1 别处"));
    let r = app.areas.elsewhere;
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: r.x + 1,
        row: r.y,
        modifiers: KeyModifiers::NONE,
    };
    app.mouse(click, std::time::Instant::now());
    assert_eq!(
        app.selected_entry()
            .map(|e| e.session.id.as_str().to_owned()),
        Some("c-resume-1".to_owned())
    );
}
