//! 会话台的键鼠行为：方案「界面」节的单键操作和鼠标（单击选中、双击打开、滚轮）。

mod ui_support;

use std::time::{Duration, Instant};

use huitai::core::{BackKey, SessionId, SessionState, Vendor};
use huitai::index::Edit;
use huitai::ui::app::{Effect, Overlay, Prompt};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use ui_support::*;

fn selected_id(app: &huitai::ui::App) -> Option<String> {
    app.selected_entry().map(|e| e.session.id.to_string())
}

fn press(app: &mut huitai::ui::App, c: char) -> Vec<Effect> {
    app.key(key(KeyCode::Char(c)))
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn starts_on_first_conversation_and_moves() {
    let mut app = demo_app(Vendor::Codex);
    // Codex 里没有启动目录（shop）的项目：项目组都收起，停在第一个组标题上；→ 展开，再 → 进到第一条。
    assert!(selected_id(&app).is_none());
    assert_eq!(app.selected_group().unwrap().label, "api");
    app.key(key(KeyCode::Right));
    assert!(selected_id(&app).is_none(), "展开后还停在组标题上");
    app.key(key(KeyCode::Right));
    assert_eq!(selected_id(&app).as_deref(), Some("x-api-1"));
    press(&mut app, 'j');
    assert_eq!(selected_id(&app).as_deref(), Some("x-api-2"));
    press(&mut app, 'k');
    assert_eq!(selected_id(&app).as_deref(), Some("x-api-1"));
}

#[test]
fn enter_on_collapsed_group_goes_into_first_conversation() {
    let mut app = demo_app(Vendor::Codex);
    assert_eq!(app.selected_group().unwrap().label, "api");
    app.key(key(KeyCode::Enter));
    assert_eq!(selected_id(&app).as_deref(), Some("x-api-1"), "进到第一条");
    // 回到组标题再按回车是收起。
    press(&mut app, 'k');
    assert!(selected_id(&app).is_none());
    app.key(key(KeyCode::Enter));
    assert!(app.folds.is_collapsed(&app.selected_group().unwrap().key));
}

#[test]
fn tab_cycles_vendors_both_ways() {
    let mut app = demo_app(Vendor::Claude);
    app.key(key(KeyCode::Tab));
    assert_eq!(app.vendor, Vendor::Codex);
    app.key(key(KeyCode::BackTab));
    app.key(key(KeyCode::BackTab));
    // 从第一家往回绕到最后一家。
    assert_eq!(app.vendor, *Vendor::ALL.last().unwrap());
}

#[test]
fn left_collapses_group_and_right_expands() {
    let mut app = demo_app(Vendor::Claude);
    let rows = app.list().rows.len();
    app.key(key(KeyCode::Left));
    assert!(app.selected_entry().is_none(), "收起后停在组标题上");
    assert_eq!(app.list().rows.len(), rows - 3);
    app.key(key(KeyCode::Right));
    assert_eq!(app.list().rows.len(), rows);
    app.key(key(KeyCode::Right));
    assert_eq!(selected_id(&app).as_deref(), Some("c-shop-1"));
}

#[test]
fn enter_on_folded_group_expands_it() {
    let mut app = demo_app(Vendor::Claude);
    app.key(key(KeyCode::End));
    assert!(app.selected_entry().is_none());
    let before = app.list().rows.len();
    assert!(app.key(key(KeyCode::Enter)).is_empty());
    assert_eq!(app.list().rows.len(), before + 1, "归档组展开");
}

#[test]
fn number_jumps_to_project() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, '2');
    assert_eq!(selected_id(&app).as_deref(), Some("c-blog-1"));
    press(&mut app, '4');
    assert_eq!(selected_id(&app).as_deref(), Some("c-api-2"));
}

#[test]
fn open_keys_emit_open_effects() {
    let mut app = demo_app(Vendor::Claude);
    match app.key(key(KeyCode::Enter)).as_slice() {
        [Effect::Open { entry, here: false }] => assert_eq!(entry.session.id.as_str(), "c-shop-1"),
        other => panic!("{other:?}"),
    }
    let alt = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
    assert!(matches!(
        app.key(alt).as_slice(),
        [Effect::Open { here: true, .. }]
    ));
    match press(&mut app, 'n').as_slice() {
        [
            Effect::New {
                vendor: Vendor::Claude,
                cwd,
            },
        ] => {
            assert_eq!(cwd.to_str(), Some("/Users/demo/projects/shop"))
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(press(&mut app, 'R'), vec![Effect::RestoreTabs]);
}

#[test]
fn new_elsewhere_asks_for_a_directory() {
    let mut app = demo_app(Vendor::Pi);
    let effects = press(&mut app, 'm');
    assert_eq!(
        effects,
        vec![Effect::ScanProjectsRoot("/Users/demo/projects".into())]
    );
    assert!(matches!(&app.prompt, Some(Prompt::NewDir { picker, .. }) if picker.text.is_empty()));
    let tmp = std::env::temp_dir();
    for c in tmp.to_string_lossy().chars() {
        press(&mut app, c);
    }
    match app.key(key(KeyCode::Enter)).as_slice() {
        [
            Effect::New {
                vendor: Vendor::Pi,
                cwd,
            },
        ] => assert_eq!(cwd, &tmp),
        other => panic!("{other:?}"),
    }
    // 不存在的目录先问要不要新建。
    press(&mut app, 'm');
    press(&mut app, 'x');
    assert!(app.key(key(KeyCode::Enter)).is_empty());
    assert!(
        matches!(&app.prompt, Some(Prompt::ConfirmCreate { dir, .. }) if dir.to_str() == Some("/Users/demo/projects/x"))
    );
}

#[test]
fn rename_pin_archive_annotate() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, 'e');
    assert!(
        matches!(&app.prompt, Some(Prompt::Rename { text, .. }) if text == "购物车结算按钮点了没反应")
    );
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    for c in "修结算".chars() {
        press(&mut app, c);
    }
    let id = SessionId::from("c-shop-1");
    assert_eq!(
        app.key(key(KeyCode::Enter)),
        vec![Effect::Annotate {
            vendor: Vendor::Claude,
            id: id.clone(),
            edit: Edit::Rename(Some("修结算".into()))
        }]
    );
    // Esc 取消不改。
    press(&mut app, 'e');
    assert!(app.key(key(KeyCode::Esc)).is_empty());
    assert!(app.prompt.is_none());
    assert_eq!(
        press(&mut app, 'p'),
        vec![Effect::Annotate {
            vendor: Vendor::Claude,
            id: id.clone(),
            edit: Edit::Pin(true)
        }]
    );
    assert_eq!(
        press(&mut app, 'a'),
        vec![Effect::Annotate {
            vendor: Vendor::Claude,
            id,
            edit: Edit::Archive(true)
        }]
    );
}

#[test]
fn trash_untrash_and_project_keys() {
    let mut app = demo_app(Vendor::Claude);
    assert!(
        matches!(press(&mut app, 'd').as_slice(), [Effect::Trash(e)] if e.session.id.as_str() == "c-shop-1")
    );
    assert_eq!(press(&mut app, 'u'), vec![Effect::Untrash(Vendor::Claude)]);
    let shop = std::path::PathBuf::from("/Users/demo/projects/shop");
    assert_eq!(press(&mut app, 'o'), vec![Effect::Editor(shop.clone())]);
    assert_eq!(press(&mut app, 'f'), vec![Effect::Finder(shop.clone())]);
    assert_eq!(press(&mut app, 'g'), vec![Effect::GitStatus(shop)]);
}

#[test]
fn kill_asks_first_and_only_for_hosted() {
    let mut app = demo_app(Vendor::Claude);
    assert!(press(&mut app, 'x').is_empty());
    assert!(matches!(app.prompt, Some(Prompt::ConfirmKill { .. })));
    assert!(press(&mut app, 'n').is_empty(), "其他键取消");
    assert!(app.prompt.is_none());
    press(&mut app, 'x');
    assert_eq!(
        press(&mut app, 'y'),
        vec![Effect::Kill(SessionId::from("c-shop-1"))]
    );
    // 已停的不问，直接说明。
    press(&mut app, 'j');
    press(&mut app, 'j');
    assert_eq!(selected_id(&app).as_deref(), Some("c-shop-3"));
    assert!(press(&mut app, 'x').is_empty());
    assert!(app.prompt.is_none());
    assert_eq!(app.status.as_deref(), Some("这条对话没有在后台跑"));
}

/// 搜索里 ⌃A 在「只搜当前这家」和「各家一起搜」之间切；各家的结果回车打开的是那条自己的家，
/// Tab 换家回到只搜那家。
#[test]
fn ctrl_a_searches_every_vendor_and_back() {
    let ctrl_a = || KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, '/');
    // 还没输词：只切范围，不搜。
    assert!(app.key(ctrl_a()).is_empty());
    assert!(app.search.as_ref().unwrap().all);
    assert!(app.key(ctrl_a()).is_empty());
    assert!(
        !app.search.as_ref().unwrap().all,
        "默认只搜当前这家，再按切回"
    );
    press(&mut app, 'q');
    assert_eq!(
        app.key(ctrl_a()),
        vec![Effect::Search {
            vendor: None,
            query: "q".into()
        }],
        "⌃A 是切换，不是往搜索框里打 a"
    );
    assert_eq!(app.search.as_ref().unwrap().query, "q");
    assert_eq!(app.search_scope(), None);
    // 只搜一家的旧结果丢掉；各家一起的收下。
    app.set_hits(Vendor::Claude, "q", claude_entries());
    assert!(app.search.as_ref().unwrap().hits.is_empty());
    let mixed: Vec<_> = codex_entries()
        .into_iter()
        .take(1)
        .chain(claude_entries().into_iter().take(1))
        .map(|entry| huitai::index::Hit {
            entry,
            in_title: true,
            in_body: false,
        })
        .collect();
    app.set_search_hits(None, "q", mixed);
    assert_eq!(app.search.as_ref().unwrap().hits.len(), 2);
    // 第一条是 Codex 的：当前标签还是 Claude，回车打开的照样是那条 Codex 对话。
    assert_eq!(app.vendor, Vendor::Claude);
    match app.key(key(KeyCode::Enter)).as_slice() {
        [Effect::Open { entry, .. }] => assert_eq!(entry.session.vendor, Vendor::Codex),
        other => panic!("回车应打开选中的那条：{other:?}"),
    }
    // 打字照样各家一起搜。
    assert_eq!(
        press(&mut app, 'x'),
        vec![Effect::Search {
            vendor: None,
            query: "qx".into()
        }]
    );
    // Tab 换家：回到只搜换到的那家。
    assert_eq!(
        app.key(key(KeyCode::Tab)),
        vec![Effect::Search {
            vendor: Some(Vendor::Codex),
            query: "qx".into()
        }]
    );
    assert!(!app.search.as_ref().unwrap().all);
    // 再切到各家，点当前这家的标签也回到只搜这家。
    app.key(ctrl_a());
    render(&mut app, 120, 30);
    let (_, r) = *app
        .areas
        .tabs
        .iter()
        .find(|(v, _)| *v == Vendor::Codex)
        .unwrap();
    assert_eq!(
        app.mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), r.x, 0),
            Instant::now(),
        ),
        vec![Effect::Search {
            vendor: Some(Vendor::Codex),
            query: "qx".into()
        }]
    );
    assert_eq!(app.search_scope(), Some(Vendor::Codex));
}

#[test]
fn search_types_query_and_esc_returns() {
    let mut app = demo_app(Vendor::Claude);
    assert!(press(&mut app, '/').is_empty());
    assert_eq!(
        press(&mut app, 'q'),
        vec![Effect::Search {
            vendor: Some(Vendor::Claude),
            query: "q".into()
        }],
        "搜索框里 q 是字，不是退出"
    );
    assert!(!app.quit);
    // 换家时带着查询词重搜。
    assert_eq!(
        app.key(key(KeyCode::Tab)),
        vec![Effect::Search {
            vendor: Some(Vendor::Codex),
            query: "q".into()
        }]
    );
    // 旧结果（别家、旧词）丢掉。
    app.set_hits(Vendor::Claude, "q", claude_entries());
    assert!(app.search.as_ref().unwrap().hits.is_empty());
    app.set_hits(Vendor::Codex, "q", codex_entries());
    assert_eq!(app.search.as_ref().unwrap().hits.len(), 3);
    app.key(key(KeyCode::Down));
    assert_eq!(selected_id(&app).as_deref(), Some("x-api-2"));
    assert!(matches!(
        app.key(key(KeyCode::Enter)).as_slice(),
        [Effect::Open { .. }]
    ));
    app.key(key(KeyCode::Esc));
    assert!(app.search.is_none());
    // 回到 Codex 列表原来的选中（项目组都收起，停在第一个组标题上）。
    assert!(selected_id(&app).is_none());
    assert_eq!(app.selected_group().unwrap().label, "api");
}

#[test]
fn help_and_settings_overlays() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, '?');
    assert_eq!(app.overlay, Overlay::Help);
    assert!(press(&mut app, 'j').is_empty(), "任意键关闭帮助，不移动");
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(selected_id(&app).as_deref(), Some("c-shop-1"));

    press(&mut app, ',');
    app.key(key(KeyCode::Right));
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Left));
    match app.key(key(KeyCode::Esc)).as_slice() {
        [Effect::SaveConfig(cfg)] => {
            assert_eq!(cfg.preferred_terminal.as_deref(), Some("ghostty"));
            assert_eq!(cfg.trash_days, 29);
        }
        other => panic!("{other:?}"),
    }
    press(&mut app, 'q');
    assert!(app.quit);
}

#[test]
fn settings_change_idle_reclaim_minutes() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, ',');
    for _ in 0..3 {
        app.key(key(KeyCode::Down));
    }
    app.key(key(KeyCode::Right));
    assert_eq!(app.config.idle_reclaim_minutes, 60);
    for _ in 0..4 {
        app.key(key(KeyCode::Left));
    }
    assert_eq!(app.config.idle_reclaim_minutes, 0, "最左一档是不收回");
    assert!(dump(&render(&mut app, 120, 30)).contains("不收回"));
    app.key(key(KeyCode::Left));
    assert_eq!(app.config.idle_reclaim_minutes, 240, "转一圈");
    match app.key(key(KeyCode::Esc)).as_slice() {
        [Effect::SaveConfig(cfg)] => assert_eq!(cfg.idle_reclaim_minutes, 240),
        other => panic!("{other:?}"),
    }
}

/// 设置「检查新版本」（倒数第二项）：←→ 切换，Esc 存进配置。
#[test]
fn settings_toggle_update_check() {
    let mut app = demo_app(Vendor::Claude);
    assert!(app.config.check_updates, "默认开");
    press(&mut app, ',');
    for _ in 0..7 {
        app.key(key(KeyCode::Down));
    }
    assert!(dump(&render(&mut app, 120, 30)).contains("会话台和各家 CLI，每天最多查一次"));
    app.key(key(KeyCode::Right));
    assert!(!app.config.check_updates);
    match app.key(key(KeyCode::Esc)).as_slice() {
        [Effect::SaveConfig(cfg)] => assert!(!cfg.check_updates),
        other => panic!("{other:?}"),
    }
}

/// 设置「自动升级」（各家 CLI，倒数第二项）：默认开，←→ 切换，Esc 存进配置；关了检查新版本时写明要先开它。
#[test]
fn settings_toggle_auto_upgrade() {
    let mut app = demo_app(Vendor::Claude);
    assert!(app.config.auto_upgrade_clis, "默认开");
    press(&mut app, ',');
    for _ in 0..8 {
        app.key(key(KeyCode::Down));
    }
    assert!(
        dump(&render(&mut app, 120, 30))
            .contains("自动升级    ‹ 开 · 各家 CLI 没有对话在跑时升，不跨大版本 ›")
    );
    app.key(key(KeyCode::Left));
    assert!(!app.config.auto_upgrade_clis);
    assert!(dump(&render(&mut app, 120, 30)).contains("关 · 顶栏「更新」手动升"));
    app.key(key(KeyCode::Right));
    app.config.check_updates = false;
    assert!(dump(&render(&mut app, 120, 30)).contains("开 · 要先开「检查新版本」"));
    match app.key(key(KeyCode::Esc)).as_slice() {
        [Effect::SaveConfig(cfg)] => assert!(cfg.auto_upgrade_clis),
        other => panic!("{other:?}"),
    }
}

/// 设置最后一项「回会话台」：默认 ⌃\，→ 依次换成 ⌃Q、⌃]、关，再转回 ⌃\；Esc 存进配置。
/// 当前窗口接上前底栏的提示跟着换，关了就没有提示。
#[test]
fn settings_switch_back_key() {
    let mut app = demo_app(Vendor::Claude);
    assert_eq!(app.config.back_key, BackKey::CtrlBackslash, "默认 ⌃\\");
    assert_eq!(app.back_key_hint().as_deref(), Some("⌃\\ 回到会话台"));
    press(&mut app, ',');
    for _ in 0..20 {
        app.key(key(KeyCode::Down));
    }
    assert!(
        dump(&render(&mut app, 120, 30))
            .contains("回会话台    ‹ ⌃\\ · 对话里按它回来，对话在后台继续 ›")
    );
    for (want, hint) in [
        (BackKey::CtrlQ, Some("⌃Q 回到会话台")),
        (BackKey::CtrlBracket, Some("⌃] 回到会话台")),
        (BackKey::Off, None),
        (BackKey::CtrlBackslash, Some("⌃\\ 回到会话台")),
    ] {
        app.key(key(KeyCode::Right));
        assert_eq!(app.config.back_key, want);
        assert_eq!(app.back_key_hint().as_deref(), hint);
    }
    app.key(key(KeyCode::Left));
    assert_eq!(app.config.back_key, BackKey::Off, "← 反着转");
    assert!(dump(&render(&mut app, 120, 30)).contains("关 · 对话结束才回到会话台"));
    match app.key(key(KeyCode::Esc)).as_slice() {
        [Effect::SaveConfig(cfg)] => assert_eq!(cfg.back_key, BackKey::Off),
        other => panic!("{other:?}"),
    }
}

/// 当前窗口接上对话前，底栏写「⌃\\ 回到会话台」。
#[test]
fn bottom_bar_shows_back_key_hint() {
    let mut app = demo_app(Vendor::Claude);
    app.status = app.back_key_hint();
    let text = dump(&render(&mut app, 120, 30));
    assert!(
        text.lines().last().unwrap().contains("⌃\\ 回到会话台"),
        "{text}"
    );
}

#[test]
fn mouse_click_selects_double_click_opens_wheel_moves() {
    let mut app = demo_app(Vendor::Claude);
    let line_of = |app: &mut huitai::ui::App, text: &str| {
        let buf = render(app, 120, 30);
        (1..29).find(|y| dump(&buf).lines().nth(*y as usize).unwrap().contains(text))
    };
    // blog 组默认收起：单击组标题展开它。
    assert_eq!(line_of(&mut app, "给博客加 RSS"), None);
    let header = line_of(&mut app, "▸ blog").unwrap();
    assert_eq!(
        app.mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 10, header),
            Instant::now() - Duration::from_secs(10)
        ),
        vec![Effect::SetExpanded {
            project: "/Users/demo/projects/blog".into(),
            expanded: true
        }]
    );
    let buf = render(&mut app, 120, 30);
    let y = (1..29)
        .find(|y| {
            dump(&buf)
                .lines()
                .nth(*y as usize)
                .unwrap()
                .contains("给博客加 RSS")
        })
        .unwrap();
    let t0 = Instant::now();
    assert!(
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 10, y), t0)
            .is_empty()
    );
    assert_eq!(selected_id(&app).as_deref(), Some("c-blog-1"));
    let open = app.mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), 12, y),
        t0 + Duration::from_millis(200),
    );
    assert!(
        matches!(open.as_slice(), [Effect::Open { entry, here: false }] if entry.session.id.as_str() == "c-blog-1")
    );
    // 隔太久的第二下只算单击。
    let later = t0 + Duration::from_secs(2);
    app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 10, y), later);
    assert!(
        app.mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 10, y),
            later + Duration::from_secs(1)
        )
        .is_empty()
    );

    // 滚轮一格 3 行：c-blog-2、api 组标题、另一个 api 组标题（都收起着）。
    app.mouse(mouse(MouseEventKind::ScrollDown, 10, 5), later);
    let work_api = huitai::ui::rows::GroupKey::Project("/Users/demo/work/api".into());
    assert_eq!(app.selected_group().unwrap().key, work_api);
    // 预览区上滚轮翻预览，不动选中。
    app.mouse(mouse(MouseEventKind::ScrollUp, 100, 10), later);
    assert_eq!(app.preview_scroll, 3);
    assert_eq!(app.selected_group().unwrap().key, work_api);
}

#[test]
fn clicking_a_tab_switches_vendor() {
    let mut app = demo_app(Vendor::Claude);
    render(&mut app, 120, 30);
    let (_, r) = *app
        .areas
        .tabs
        .iter()
        .find(|(v, _)| *v == Vendor::Pi)
        .unwrap();
    // 五家以上放不下时别的家缩成一格标记，点它那一格。
    app.mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), r.x, 0),
        Instant::now(),
    );
    assert_eq!(app.vendor, Vendor::Pi);
}

#[test]
fn selection_survives_list_reload() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, 'j');
    assert_eq!(selected_id(&app).as_deref(), Some("c-shop-2"));
    // 新对话出现在最前面，选中的那条不跳。
    let mut entries = claude_entries();
    entries.insert(
        1,
        entry(
            Vendor::Claude,
            &Fake {
                id: "c-new",
                cwd: "/Users/demo/projects/blog",
                title: "刚开的对话",
                mins: 0,
                messages: 1,
                branch: None,
                pinned: false,
                archived: false,
                automatic: false,
            },
        ),
    );
    app.set_entries(Vendor::Claude, entries);
    assert_eq!(selected_id(&app).as_deref(), Some("c-shop-2"));
}

#[test]
fn preview_is_requested_once_per_selection() {
    let mut app = demo_app(Vendor::Claude);
    assert!(app.take_preview_request().is_none(), "已经读好");
    press(&mut app, 'j');
    let e = app.take_preview_request().expect("新选中的要读");
    assert_eq!(e.session.id.as_str(), "c-shop-2");
    assert!(app.take_preview_request().is_none(), "读着呢，不重复要");
}

#[test]
fn live_preview_follows_selected_running_session_and_stops_under_overlays() {
    let mut app = demo_app(huitai::core::Vendor::Claude);
    // 在跑的：取画面。
    assert_eq!(app.live_target(), Some(SessionId::from("c-shop-1")));
    // 换到「刚做完」（进程还在）的：换目标。
    select_id(&mut app, "c-shop-2");
    assert_eq!(app.live_target(), Some(SessionId::from("c-shop-2")));
    // 已停的：不取。
    select_id(&mut app, "c-shop-3");
    assert_eq!(app.live_target(), None);
    // 帮助页挡住预览时停，关掉恢复。
    app.key(key(KeyCode::Up));
    assert_eq!(app.live_target(), Some(SessionId::from("c-shop-2")));
    press(&mut app, '?');
    assert_eq!(app.live_target(), None);
    press(&mut app, 'x');
    assert_eq!(app.live_target(), Some(SessionId::from("c-shop-2")));
    // 选中组标题：没有对话可预览。
    app.key(key(KeyCode::Up));
    app.key(key(KeyCode::Up));
    assert!(app.selected_entry().is_none());
    assert_eq!(app.live_target(), None);
}

/// 审计 v0.1.4 第 21 条：Antigravity 工作区为空或不是本机路径的对话没有项目目录，接不上：预览标成只能查看，
/// 回车、w 都只给提示、不去开标签页；v 照样能看整段。
#[test]
fn conversation_without_a_local_workdir_is_view_only() {
    let mut app = demo_app(Vendor::Antigravity);
    let mut entries = antigravity_entries();
    let mut remote = entries[0].clone();
    remote.session.id = SessionId::from("a9e10000-3333-4aaa-8bbb-000000000003");
    remote.session.cwd = std::path::PathBuf::new();
    remote.session.title = Some("远程工作区里的对话".to_owned());
    entries.push(remote);
    app.set_entries(Vendor::Antigravity, entries);
    // 没有目录的收在「零散」组里：碰到组标题就展开。
    for _ in 0..20 {
        if selected_id(&app).as_deref() == Some("a9e10000-3333-4aaa-8bbb-000000000003") {
            break;
        }
        if app.selected_entry().is_none() {
            app.key(key(KeyCode::Right));
        }
        app.key(key(KeyCode::Down));
    }
    select_id(&mut app, "a9e10000-3333-4aaa-8bbb-000000000003");
    load_preview(&mut app);

    assert!(app.key(key(KeyCode::Enter)).is_empty(), "回车不打开");
    assert_eq!(app.status.as_deref(), Some(huitai::ui::app::VIEW_ONLY));
    app.status = None;
    assert!(press(&mut app, 'w').is_empty(), "w 也不打开");
    assert_eq!(app.status.as_deref(), Some(huitai::ui::app::VIEW_ONLY));
    assert!(
        dump(&render(&mut app, 120, 30)).contains("只能查看"),
        "预览标成只能查看"
    );
    press(&mut app, 'v');
    assert!(app.reader.is_some(), "整段照样能看");

    // 有目录的照常打开。
    app.reader = None;
    for _ in 0..20 {
        app.key(key(KeyCode::Up));
    }
    select_id(&mut app, "a9e10000-1111-4aaa-8bbb-000000000001");
    assert!(matches!(
        app.key(key(KeyCode::Enter)).as_slice(),
        [Effect::Open { .. }]
    ));
}

/// 答完不发提醒的 CLI 闲置收回：交给收回线程看记录的是托管着、没有标签页连着的，给守护进程里的 id；
/// 设置里关了就一条也没有。
#[test]
fn reclaim_targets_are_hosted_and_detached() {
    let mut app = demo_app(Vendor::Pi);
    let ids = |app: &huitai::ui::App| {
        let mut v: Vec<(String, String)> = app
            .reclaim_targets()
            .into_iter()
            .map(|(h, e)| (h.as_str().to_owned(), e.session.id.as_str().to_owned()))
            .collect();
        v.sort();
        v
    };
    let all = ids(&app);
    assert!(all.iter().any(|(_, e)| e == "p-blog-1"), "{all:?}");
    assert!(all.iter().any(|(_, e)| e == "c-shop-2"));
    assert!(
        !all.iter().any(|(_, e)| e == "c-shop-1"),
        "演示数据里它开着标签页"
    );
    assert!(!all.iter().any(|(_, e)| e == "c-shop-3"), "已停的不看");

    app.attached.insert(SessionId::from("p-blog-1"));
    assert!(
        !ids(&app).iter().any(|(_, e)| e == "p-blog-1"),
        "有标签页连着不看"
    );

    // 认领到临时 id 下的：给守护进程的是临时 id。
    let (real, temp) = (
        SessionId::from("x-api-2"),
        SessionId::from("new-codex-1000000000"),
    );
    app.states.insert(temp.clone(), SessionState::Waiting);
    app.set_aliases(std::collections::HashMap::from([(real, temp)]));
    assert!(
        ids(&app).contains(&("new-codex-1000000000".to_owned(), "x-api-2".to_owned())),
        "{:?}",
        ids(&app)
    );

    app.config.idle_reclaim_minutes = 0;
    assert!(app.reclaim_targets().is_empty());
}
