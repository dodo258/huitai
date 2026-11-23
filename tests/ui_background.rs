//! 列表只看自己的对话、项目默认收起：自动运行的对话默认藏进「另有 N 条后台」，b 切换并记进设置；
//! 项目组默认收起，展开状态交给运行层记进标注；在项目目录里敲 ai 时那个项目自动展开并选中第一条。

mod ui_support;

use std::path::PathBuf;
use std::time::Instant;

use huitai::core::{Config, SessionState, Vendor};
use huitai::ui::app::Effect;
use huitai::ui::rows::{GroupKey, Row};
use huitai::ui::{App, Theme};
use ratatui::crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use ui_support::*;

const SHOP: &str = "/Users/demo/projects/shop";
const BLOG: &str = "/Users/demo/projects/blog";

fn project(p: &str) -> GroupKey {
    GroupKey::Project(PathBuf::from(p))
}

/// 列表里看得到的对话 id（展开着的组里的）。
fn visible_ids(app: &App) -> Vec<String> {
    let l = app.list();
    l.rows
        .iter()
        .filter_map(|r| match r {
            Row::Item { entry, .. } => Some(l.entries[*entry].session.id.to_string()),
            Row::Header(_) => None,
        })
        .collect()
}

fn group<'a>(app: &'a App, key: &GroupKey) -> Option<&'a huitai::ui::rows::Group> {
    app.list().groups.iter().find(|g| g.key == *key)
}

fn selected_id(app: &App) -> Option<String> {
    app.selected_entry().map(|e| e.session.id.to_string())
}

/// 和 demo_app 一样的数据，但从 `launch` 启动、用 `config`、先换上 `expanded`。
fn app_with(launch: &str, config: Config, expanded: &[&str]) -> App {
    let mut app = App::new(PathBuf::from(HOME), PathBuf::from(launch), config);
    app.now = now();
    app.set_expanded_projects(expanded.iter().map(PathBuf::from).collect());
    app.set_entries(Vendor::Claude, claude_entries());
    app.set_entries(Vendor::Codex, codex_entries());
    app
}

fn screen(app: &mut App, w: u16) -> String {
    dump(&render(app, w, 30))
}

fn click(app: &mut App, x: u16, y: u16) -> Vec<Effect> {
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

#[test]
fn automatic_sessions_are_hidden_and_counted_per_group() {
    let mut app = demo_app(Vendor::Claude);
    // 展开全部组，确认自动运行的一条都不在列表里。
    for p in [BLOG, "/Users/demo/projects/api", "/Users/demo/work/api"] {
        app.folds.expanded.insert(PathBuf::from(p));
    }
    app.set_entries(Vendor::Claude, claude_entries());
    let ids = visible_ids(&app);
    assert!(ids.iter().all(|id| !id.starts_with("c-auto")), "{ids:?}");
    assert!(ids.contains(&"c-shop-1".to_owned()));

    let shop = group(&app, &project(SHOP)).unwrap();
    assert_eq!(shop.background.len(), 2);
    assert_eq!(shop.items.len(), 3);
    assert_eq!(group(&app, &project(BLOG)).unwrap().background.len(), 1);
    // 只有自动运行对话的项目整组不显示。
    assert!(group(&app, &project("/Users/demo/projects/ops")).is_none());
    // 编号只给显示出来的项目组，连续。
    let numbers: Vec<u8> = app.list().groups.iter().filter_map(|g| g.number).collect();
    assert_eq!(numbers, [1, 2, 3, 4]);

    let text = screen(&mut app, 120);
    assert!(
        text.lines()
            .any(|l| l.contains("▾ shop") && l.contains("◐ 1")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.contains("blog") && !l.contains("后台")),
        "{text}"
    );
    assert!(!text.contains("每天备份检查") && !text.contains("ops"));
    // 底栏左侧常驻「后台 M 在跑」（c-auto-1 一分钟前还在写）。
    assert!(text.lines().last().unwrap().starts_with(" ◐ 后台 1 在跑"));
    assert_eq!(app.background_running(), 1);
}

#[test]
fn b_toggles_background_and_saves_it_in_config() {
    let mut app = demo_app(Vendor::Claude);
    assert!(!app.config.show_background, "默认不显示");
    let effects = app.key(key(KeyCode::Char('b')));
    match effects.as_slice() {
        [Effect::SaveConfig(cfg)] => assert!(cfg.show_background),
        other => panic!("{other:?}"),
    }
    assert!(visible_ids(&app).contains(&"c-auto-1".to_owned()));
    assert!(
        group(&app, &project("/Users/demo/projects/ops")).is_some(),
        "显示后台对话时只有后台对话的组也出来"
    );
    assert!(group(&app, &project(SHOP)).unwrap().background.is_empty());
    assert_eq!(selected_id(&app).as_deref(), Some("c-shop-1"), "选中不跳");
    let text = screen(&mut app, 120);
    assert!(text.contains("按待办做：结算页骨架屏"), "{text}");
    assert!(!text.contains("另有 2 条后台"));

    // 选中一条后台对话再藏起来：停在它那组的标题上。
    app.key(key(KeyCode::Home));
    select_id(&mut app, "c-auto-1");
    match app.key(key(KeyCode::Char('b'))).as_slice() {
        [Effect::SaveConfig(cfg)] => assert!(!cfg.show_background),
        other => panic!("{other:?}"),
    }
    assert!(selected_id(&app).is_none());
    assert_eq!(app.selected_group().unwrap().key, project(SHOP));
}

#[test]
fn show_background_setting_is_remembered_across_launches() {
    let cfg = Config {
        show_background: true,
        ..Config::default()
    };
    let app = app_with(SHOP, cfg, &[]);
    assert!(visible_ids(&app).contains(&"c-auto-1".to_owned()));
    let json = serde_json::to_string(&app.config).unwrap();
    let back: Config = serde_json::from_str(&json).unwrap();
    assert!(back.show_background);
    // 旧的 config.json 没有这一项：默认不显示。
    let old: Config = serde_json::from_str(r#"{"trash_days": 7}"#).unwrap();
    assert!(!old.show_background);
}

#[test]
fn clicking_background_counter_toggles() {
    let mut app = demo_app(Vendor::Claude);
    render(&mut app, 120, 30);
    let r = app.areas.background;
    assert!(r.width > 0, "有在跑的后台对话时底栏左边有计数");
    let effects = click(&mut app, r.x + 1, r.y);
    assert!(matches!(effects.as_slice(), [Effect::SaveConfig(c)] if c.show_background));

    // 没有在跑的后台对话：不显示，也点不到。
    let mut app = demo_app(Vendor::Codex);
    let text = screen(&mut app, 120);
    assert!(!text.contains("后台"), "{text}");
    assert_eq!(app.areas.background.width, 0);
}

#[test]
fn projects_start_collapsed_except_the_launch_project() {
    let app = app_with(SHOP, Config::default(), &[]);
    let l = app.list();
    for g in &l.groups {
        let collapsed = app.folds.is_collapsed(&g.key);
        match &g.key {
            GroupKey::Project(p) if p == &PathBuf::from(SHOP) => assert!(!collapsed),
            GroupKey::Project(_) | GroupKey::Fold(_) | GroupKey::Stale | GroupKey::Archived => {
                assert!(collapsed, "{:?} 默认收起", g.key)
            }
            GroupKey::Pinned | GroupKey::Interrupted => assert!(!collapsed),
        }
    }
    // 启动目录的项目展开并选中它的第一条。
    assert_eq!(selected_id(&app).as_deref(), Some("c-shop-1"));
    assert_eq!(
        visible_ids(&app),
        ["c-notes-1", "c-shop-1", "c-shop-2", "c-shop-3"]
    );
}

#[test]
fn launch_from_a_subdirectory_or_elsewhere() {
    // 在项目下面的目录（含 agentq 工作树）里敲 ai：也认这个项目。
    let app = app_with(
        "/Users/demo/projects/shop/.claude/worktrees/x/src",
        Config::default(),
        &[],
    );
    assert_eq!(selected_id(&app).as_deref(), Some("c-shop-1"));

    // 从家目录打开：项目都收起，停在第一条（置顶组展开着）。
    let mut app = app_with(HOME, Config::default(), &[]);
    assert_eq!(selected_id(&app).as_deref(), Some("c-notes-1"));
    assert!(app.folds.expanded.is_empty(), "不自动展开任何项目");
    // Codex 里没有置顶、项目都收起：停在第一个组标题上。
    app.key(key(KeyCode::Tab));
    assert!(selected_id(&app).is_none());
    assert_eq!(
        app.selected_group().unwrap().key,
        project("/Users/demo/projects/api")
    );
}

#[test]
fn launch_project_expands_without_being_saved_and_only_once() {
    let mut app = app_with(SHOP, Config::default(), &[]);
    assert!(app.folds.expanded.contains(&PathBuf::from(SHOP)));
    // 收起后列表刷新，不会又被自动展开。
    let effects = app.key(key(KeyCode::Left));
    assert_eq!(
        effects,
        vec![Effect::SetExpanded {
            project: SHOP.into(),
            expanded: false
        }]
    );
    app.set_entries(Vendor::Claude, claude_entries());
    assert!(app.folds.is_collapsed(&project(SHOP)));
}

#[test]
fn expand_and_collapse_emit_effects_and_saved_state_is_restored() {
    let mut app = app_with(HOME, Config::default(), &[]);
    // 置顶组展开，第一个项目组 shop 收起：移到它的标题上。
    app.key(key(KeyCode::Down));
    assert_eq!(app.selected_group().unwrap().key, project(SHOP));
    let rows = app.list().rows.len();

    // → 展开（记进标注），再 → 进到第一条，← 收起（记进标注）并回到标题。
    let open = app.key(key(KeyCode::Right));
    assert_eq!(
        open,
        vec![Effect::SetExpanded {
            project: SHOP.into(),
            expanded: true
        }]
    );
    assert_eq!(app.list().rows.len(), rows + 3);
    assert!(app.key(key(KeyCode::Char('l'))).is_empty());
    assert_eq!(selected_id(&app).as_deref(), Some("c-shop-1"));
    let close = app.key(key(KeyCode::Char('h')));
    assert_eq!(
        close,
        vec![Effect::SetExpanded {
            project: SHOP.into(),
            expanded: false
        }]
    );
    assert_eq!(app.list().rows.len(), rows);
    assert_eq!(app.selected_group().unwrap().key, project(SHOP));
    // 回车在组标题上也是展开/收起。
    assert_eq!(
        app.key(key(KeyCode::Enter)),
        vec![Effect::SetExpanded {
            project: SHOP.into(),
            expanded: true
        }]
    );
    // 展开状态三家共用：Codex 里同一个项目也展开着。
    assert!(!app.folds.is_collapsed(&project(SHOP)));

    // 下次打开：标注里存着的项目展开着，别的收起。
    let app = app_with(HOME, Config::default(), &[BLOG]);
    assert!(!app.folds.is_collapsed(&project(BLOG)));
    assert!(app.folds.is_collapsed(&project(SHOP)));
    let ids = visible_ids(&app);
    assert!(ids.contains(&"c-blog-1".to_owned()) && !ids.contains(&"c-shop-1".to_owned()));

    // 非项目组（归档）展开不记进标注。
    let mut app = app_with(HOME, Config::default(), &[]);
    app.key(key(KeyCode::End));
    assert_eq!(app.selected_group().unwrap().key, GroupKey::Archived);
    assert!(app.key(key(KeyCode::Enter)).is_empty());
    assert!(!app.folds.is_collapsed(&GroupKey::Archived));
}

#[test]
fn number_jump_expands_and_saves() {
    let mut app = app_with(HOME, Config::default(), &[]);
    let effects = app.key(key(KeyCode::Char('2')));
    assert_eq!(
        effects,
        vec![Effect::SetExpanded {
            project: BLOG.into(),
            expanded: true
        }]
    );
    assert_eq!(selected_id(&app).as_deref(), Some("c-blog-1"));
    // 已经展开着再按：只跳过去。
    app.key(key(KeyCode::Home));
    assert!(app.key(key(KeyCode::Char('2'))).is_empty());
    assert_eq!(selected_id(&app).as_deref(), Some("c-blog-1"));
}

#[test]
fn collapsed_header_shows_count_time_and_status() {
    let mut app = demo_app(Vendor::Claude);
    let text = screen(&mut app, 120);
    let line = |needle: &str| {
        text.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("没有 {needle}：\n{text}"))
            .to_owned()
    };
    // 表头写着各列是什么。
    let head = text.lines().nth(1).unwrap();
    for c in ["项目", "状态", "后台", "对话", "最近"] {
        assert!(head.contains(c), "{head}");
    }
    // blog 收起：条数、最近时间；里面有一条等你回复（◆）；后台都没在跑，后台列空着。
    let blog = line("▸ blog");
    assert!(
        blog.contains("◆           2   1 小时前"),
        "后台都没在跑：不占地方\n{blog}"
    );
    // agentq 工作树里有一条在跑：状态列转圈和个数。
    assert!(line("▸ agentq 工作树").contains("◐ 1           1  40 分钟前"));
    // 展开着的 shop 只写条数（第一条就写着时间）。窄屏没有预览区，行尾就是组标题的右边。
    let narrow = screen(&mut app, 70);
    let shop = narrow.lines().find(|l| l.contains("▾ shop")).unwrap();
    assert!(shop.ends_with(" 3"), "{shop}");

    // 状态灯颜色：在跑绿、等你黄。
    let buf = render(&mut app, 120, 30);
    let y = text.lines().position(|l| l.contains("▸ blog")).unwrap() as u16;
    let x = (0..120)
        .find(|&x| buf[(x, y)].symbol() == "◆")
        .expect("blog 标题上有黄色 ◆");
    assert_eq!(
        buf[(x, y)].fg,
        Theme::DARK.state(SessionState::Waiting),
        "等你回复是黄点"
    );
}

/// 每个表头下面都有对应的内容，列靠右对齐：内容的右边和表头字的右边在同一列。
/// 对话的状态灯就在「状态」列里；窄了省掉的列，表头和各行一起省，状态列一直在。
#[test]
fn every_column_header_has_content_under_it() {
    let mut app = demo_app(Vendor::Claude);
    let w = huitai::ui::text::width;
    // 这一行里每个非空白字符的右边落在第几列。
    let ends = |l: &str| -> Vec<usize> {
        l.char_indices()
            .filter(|(_, c)| !c.is_whitespace())
            .map(|(i, c)| w(&l[..i + c.len_utf8()]))
            .collect()
    };
    for width in [120, 70, 44, 36] {
        let text = screen(&mut app, width);
        let lines: Vec<&str> = text.lines().collect();
        let head = lines[1];
        assert!(head.contains("状态"), "状态列一直在（宽 {width}）\n{text}");
        let mut shown = 0;
        for label in ["状态", "后台", "对话", "最近"] {
            let Some(at) = head.find(label) else { continue };
            shown += 1;
            let end = w(&head[..at + label.len()]);
            assert!(
                lines[2..].iter().any(|l| ends(l).contains(&end)),
                "表头「{label}」下面空着（宽 {width}）\n{text}"
            );
        }
        // 对话行的状态灯在状态列里、不在标题前面。
        let row = lines
            .iter()
            // 选中的那条对话（宽屏比组名多缩两格，窄屏不缩）。
            .find(|l| l.starts_with('▎') && l.contains("购物车"))
            .unwrap_or_else(|| panic!("宽 {width}\n{text}"));
        let status_end = w(&head[..head.find("状态").unwrap() + "状态".len()]);
        let light = row.find('◐').unwrap();
        assert_eq!(
            w(&row[..light + '◐'.len_utf8()]),
            status_end,
            "\n{head}\n{row}"
        );
        assert!(!row[..row.find("购物车").unwrap()].contains('◐'), "{row}");
        // 省掉的列，各行也不画：展开的 shop 组标题右边只剩状态列（空着）时，不再写条数。
        if !head.contains("对话") {
            let shop = lines.iter().find(|l| l.contains("▾ shop")).unwrap();
            assert!(!shop.trim_end().ends_with(" 3"), "宽 {width}：{shop}");
        }
        if width == 36 {
            assert!(shown < 4, "窄的时候要省列\n{text}");
        }
    }
}

#[test]
fn background_running_in_a_collapsed_group_is_mentioned() {
    let mut app = demo_app(Vendor::Claude);
    app.key(key(KeyCode::Left)); // 收起 shop
    let text = screen(&mut app, 120);
    let shop = text.lines().find(|l| l.contains("▸ shop")).unwrap();
    // 后台列：转圈加在跑几条，对着表头的「后台」。
    let head = text.lines().nth(1).unwrap();
    // 列靠右对齐：内容的右边和表头字的右边在同一列。
    let end = |l: &str, s: &str| huitai::ui::text::width(&l[..l.rfind(s).unwrap() + s.len()]);
    assert!(shop.contains("◐ 1"), "{shop}");
    assert_eq!(end(shop, "◐ 1"), end(head, "后台"), "\n{head}\n{shop}");
    // 窄的时候换短写法，不截半句。
    let text = screen(&mut app, 50);
    let shop = text.lines().find(|l| l.contains("▸ shop")).unwrap();
    assert!(!shop.contains('…'), "{shop}");
}

#[test]
fn top_bar_labels_quota_and_drops_label_when_narrow() {
    let mut app = demo_app(Vendor::Claude);
    let top = |app: &mut App, w| screen(app, w).lines().next().unwrap().to_owned();
    assert!(top(&mut app, 120).contains("额度  5 小时"));
    // 有某个宽度放得下额度条、放不下「额度」两字（两字最后才省，顺序见 tests/ui_seal.rs）。
    let dropped = (40..120).any(|w| {
        let t = top(&mut app, w);
        t.contains('━') && !t.contains("额度")
    });
    assert!(dropped);
}

#[test]
fn bottom_bar_shows_selected_project_path() {
    let mut app = demo_app(Vendor::Claude);
    let last = |app: &mut App, w| screen(app, w).lines().last().unwrap().to_owned();
    assert!(last(&mut app, 120).ends_with("~/projects/shop"));
    // 选中项目组标题时也显示它的路径。
    app.key(key(KeyCode::Char('2')));
    app.key(key(KeyCode::Up));
    assert!(last(&mut app, 120).ends_with("~/projects/blog"));
    // 放不下截掉中间，最后一段留着。
    let mut app = app_with(
        "/Users/demo/projects/some/very/deep/nested/folder/shop",
        Config::default(),
        &[],
    );
    let mut entries = claude_entries();
    for e in &mut entries {
        if e.session.id.as_str() == "c-shop-1" {
            e.session.cwd = "/Users/demo/projects/some/very/deep/nested/folder/shop".into();
        }
    }
    app.set_entries(Vendor::Claude, entries);
    select_id(&mut app, "c-shop-1");
    let l = last(&mut app, 70);
    assert!(l.contains('…') && l.ends_with("shop"), "{l}");
    // 没有选中项目（置顶组标题）时不显示路径，也不再显示总条数。
    app.key(key(KeyCode::Home));
    let l = last(&mut app, 120);
    assert!(!l.contains("条对话") && !l.contains("~/"), "{l}");
}
