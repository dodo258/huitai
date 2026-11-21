//! 别处在用：不归会话台托管、但会话文件 2 分钟内还在写的对话（桌面端、别的终端、后台任务里跑着）。
//! 状态灯和预览元信息写「别处在用」；回车 / ⌥回车先问一句再开。时钟用假的（`App::now`），文件 mtime 用临时文件。

mod ui_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use huitai::core::{SessionId, SessionState, Vendor};
use huitai::index::{self, Entry};
use huitai::ui::App;
use huitai::ui::app::{Effect, Prompt};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use ui_support::*;

const ID: &str = "c-shop-3";

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("huitai-ui-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn touch(path: &Path, at: SystemTime) {
    fs::write(path, "{}\n").unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at)
        .unwrap();
}

/// Claude 的假列表，`c-shop-3`（没托管）的会话文件换成 `path`、最后时间换成 `last`，并选中它。
fn app_with(path: Option<&Path>, last: SystemTime) -> App {
    let mut app = demo_app(Vendor::Claude);
    let entries: Vec<Entry> = claude_entries()
        .into_iter()
        .map(|mut e| {
            if e.session.id.as_str() == ID {
                if let Some(p) = path {
                    e.session.path = p.to_path_buf();
                }
                e.session.last_active = last;
            }
            e
        })
        .collect();
    app.set_entries(Vendor::Claude, entries);
    select_id(&mut app, ID);
    app
}

fn selected(app: &App) -> Entry {
    app.selected_entry().unwrap().clone()
}

/// 查一遍最近在写的文件，像运行层那样喂给界面。
fn scan_writes(app: &mut App) {
    app.written = index::recent_writes(app.lists.iter().flat_map(|l| &l.entries), now());
}

fn screen(app: &mut App) -> String {
    dump(&render(app, 120, 30))
}

#[test]
fn recent_writes_stats_only_recent_conversations() {
    let dir = temp_dir("recent");
    let written = now() - Duration::from_secs(30);
    let mk = |id: &str, mins: u64, exists: bool| {
        let mut e = selected(&app_with(None, now()));
        e.session.id = SessionId::from(id);
        e.session.path = dir.join(format!("{id}.jsonl"));
        e.session.last_active = now() - Duration::from_secs(mins * 60);
        if exists {
            touch(&e.session.path, written);
        }
        e
    };
    let entries = [
        mk("fresh", 5, true),
        // 最后时间早过 30 分钟：不去 stat 它（文件刚写过也不查）。
        mk("old", 60 * 2, true),
        mk("gone", 1, false),
    ];
    let got = index::recent_writes(&entries, now());
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[&SessionId::from("fresh")], written);
}

#[test]
fn file_written_within_two_minutes_is_elsewhere_with_fake_clock() {
    let dir = temp_dir("mtime");
    let file = dir.join("c-shop-3.jsonl");
    // 最后一条消息 20 分钟前，文件 30 秒前还在写（比如工具调用的输出）。
    touch(&file, now() - Duration::from_secs(30));
    let mut app = app_with(Some(&file), now() - Duration::from_secs(20 * 60));
    let e = selected(&app);
    assert!(
        !app.elsewhere(&e),
        "还没查文件：只看最后时间，20 分钟前算已停"
    );

    scan_writes(&mut app);
    assert!(app.elsewhere(&e));
    let text = screen(&mut app);
    assert!(text.contains("◐ 别处在跑"), "文件还在写：柔绿转圈\n{text}");
    assert!(!text.contains("已停"), "{text}");

    // 假时钟往前走 2 分钟：文件已经 2 分 30 秒没写了，回到已停。
    app.now = now() + Duration::from_secs(120);
    assert!(!app.elsewhere(&e));
    let text = screen(&mut app);
    assert!(text.contains("○ 已停"), "{text}");
    assert!(!text.contains("别处在用"), "{text}");
    app.now = now();

    // 会话台自己托管着、或开着标签页的，不是别处。
    app.states
        .insert(SessionId::from(ID), SessionState::Running);
    assert!(!app.elsewhere(&e));
    app.states.remove(&SessionId::from(ID));
    app.attached.insert(SessionId::from(ID));
    assert!(!app.elsewhere(&e));
    app.attached.remove(&SessionId::from(ID));
    assert!(app.elsewhere(&e));

    // 会话台托管的刚结束：结束前写的是自己写的，不算别处；之后又写了才算。
    app.ended(SessionId::from(ID), now() - Duration::from_secs(10));
    assert!(!app.elsewhere(&e));
    touch(&file, now() - Duration::from_secs(5));
    scan_writes(&mut app);
    assert!(app.elsewhere(&e));
}

#[test]
fn recent_last_message_alone_counts_as_elsewhere() {
    // 文件查不到（Codex 的状态库、还没查过）时，最后一条消息的时间就是文件最晚写的时间的下限。
    let app = app_with(None, now() - Duration::from_secs(60));
    assert!(app.elsewhere(&selected(&app)));
    let app = app_with(None, now() - Duration::from_secs(3 * 60));
    assert!(!app.elsewhere(&selected(&app)));
}

#[test]
fn background_running_count_uses_the_same_judgement() {
    // 后台对话 c-auto-1 一分钟前还在写；改成 10 分钟前但文件刚写过，也还算在跑。
    let dir = temp_dir("auto");
    let file = dir.join("c-auto-1.jsonl");
    touch(&file, now() - Duration::from_secs(20));
    let mut app = demo_app(Vendor::Claude);
    let running = app.background_running();
    let entries: Vec<Entry> = claude_entries()
        .into_iter()
        .map(|mut e| {
            if e.session.id.as_str() == "c-auto-1" {
                e.session.path = file.clone();
                e.session.last_active = now() - Duration::from_secs(10 * 60);
            }
            e
        })
        .collect();
    app.set_entries(Vendor::Claude, entries);
    assert_eq!(app.background_running(), running - 1);
    scan_writes(&mut app);
    assert_eq!(app.background_running(), running);
}

fn alt_enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
}

fn opened(effects: &[Effect]) -> Option<(String, bool)> {
    match effects {
        [Effect::Open { entry, here }] => Some((entry.session.id.to_string(), *here)),
        _ => None,
    }
}

#[test]
fn enter_on_elsewhere_asks_first() {
    let mut app = app_with(None, now() - Duration::from_secs(30));
    assert!(app.key(key(KeyCode::Enter)).is_empty(), "先问，不直接开");
    assert!(matches!(
        app.prompt,
        Some(Prompt::ConfirmOpen { here: false, .. })
    ));
    let text = screen(&mut app);
    assert!(
        text.contains("这条正在别处运行，再开会两边同时写"),
        "{text}"
    );
    assert!(text.contains("⏎ 仍然打开 · Esc 取消"), "{text}");

    let effects = app.key(key(KeyCode::Enter));
    assert_eq!(opened(&effects), Some((ID.to_owned(), false)));
    assert!(app.prompt.is_none());
}

#[test]
fn alt_enter_on_elsewhere_asks_then_opens_here() {
    let mut app = app_with(None, now() - Duration::from_secs(30));
    assert!(app.key(alt_enter()).is_empty());
    assert!(matches!(
        app.prompt,
        Some(Prompt::ConfirmOpen { here: true, .. })
    ));
    let effects = app.key(key(KeyCode::Enter));
    assert_eq!(opened(&effects), Some((ID.to_owned(), true)));
}

#[test]
fn esc_or_other_key_cancels_the_confirm() {
    let mut app = app_with(None, now() - Duration::from_secs(30));
    app.key(key(KeyCode::Enter));
    assert!(app.key(key(KeyCode::Esc)).is_empty());
    assert!(app.prompt.is_none());
    assert!(!screen(&mut app).contains("仍然打开"));

    app.key(key(KeyCode::Enter));
    assert!(app.key(key(KeyCode::Char('j'))).is_empty());
    assert!(app.prompt.is_none());
    assert_eq!(
        app.selected_entry().unwrap().session.id.as_str(),
        ID,
        "取消用的那个键不再当作移动"
    );
}

#[test]
fn stopped_or_hosted_opens_without_asking() {
    let mut app = app_with(None, now() - Duration::from_secs(10 * 60));
    assert_eq!(
        opened(&app.key(key(KeyCode::Enter))),
        Some((ID.to_owned(), false))
    );

    let mut app = app_with(None, now() - Duration::from_secs(30));
    app.states
        .insert(SessionId::from(ID), SessionState::Waiting);
    assert_eq!(opened(&app.key(alt_enter())), Some((ID.to_owned(), true)));
}

#[test]
fn search_hit_on_elsewhere_also_asks() {
    let mut app = app_with(None, now() - Duration::from_secs(30));
    let e = selected(&app);
    app.key(key(KeyCode::Char('/')));
    app.key(key(KeyCode::Char('结')));
    app.set_hits(Vendor::Claude, "结", vec![e]);
    assert!(app.key(key(KeyCode::Enter)).is_empty());
    assert!(matches!(app.prompt, Some(Prompt::ConfirmOpen { .. })));
    assert_eq!(
        opened(&app.key(key(KeyCode::Enter))),
        Some((ID.to_owned(), false))
    );
}

#[test]
fn codex_conversation_hosted_under_temp_id_is_not_elsewhere() {
    // 会话台新建的 Codex 对话托管在临时 id 下，列表里的真实 id 看起来没托管；运行层认出来后不算别处在用。
    let mut app = app_with(None, now() - Duration::from_secs(30));
    let e = selected(&app);
    assert!(app.elsewhere(&e));
    app.aliased.insert(SessionId::from(ID));
    assert!(!app.elsewhere(&e));
    assert!(
        opened(&app.key(key(KeyCode::Enter))).is_some(),
        "直接打开，不问"
    );
}

// ---- 顶栏「别处」 ----

fn top_line(app: &mut App) -> String {
    screen(app).lines().next().unwrap_or_default().to_owned()
}

#[test]
fn top_bar_counts_conversations_open_elsewhere() {
    let mut app = app_with(None, now() - Duration::from_secs(3600));
    assert!(!top_line(&mut app).contains("别处"), "没有别处在用的就不写");
    render(&mut app, 120, 30);
    assert!(app.areas.elsewhere.is_empty());

    app.open_elsewhere.insert(SessionId::from(ID));
    let top = top_line(&mut app);
    assert!(top.contains("●1 别处"), "{top}");
    // 在「在跑 / 等你」后面，各点各的。
    assert!(
        top.find("等你").unwrap() < top.find("别处").unwrap(),
        "{top}"
    );
    render(&mut app, 120, 30);
    assert!(!app.areas.elsewhere.is_empty());
    assert!(app.areas.status.right() < app.areas.elsewhere.x);
}

#[test]
fn elsewhere_chip_spins_while_one_is_working() {
    let dir = temp_dir("chip-busy");
    let path = dir.join("busy.jsonl");
    touch(&path, now() - Duration::from_secs(20));
    let mut app = app_with(Some(&path), now() - Duration::from_secs(20));
    scan_writes(&mut app);
    let top = top_line(&mut app);
    assert!(top.contains("1 别处"), "{top}");
    assert!(
        !top.contains("●1 别处"),
        "正在干活的用转圈，不用实心点：{top}"
    );
}

#[test]
fn clicking_elsewhere_chip_jumps_to_it_across_vendors() {
    let mut app = app_with(None, now() - Duration::from_secs(3600));
    app.open_elsewhere.insert(SessionId::from(ID));
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.vendor, Vendor::Codex);
    render(&mut app, 120, 30);
    let r = app.areas.elsewhere;
    let click = ratatui::crossterm::event::MouseEvent {
        kind: ratatui::crossterm::event::MouseEventKind::Down(
            ratatui::crossterm::event::MouseButton::Left,
        ),
        column: r.x + 1,
        row: r.y,
        modifiers: KeyModifiers::NONE,
    };
    app.mouse(click, std::time::Instant::now());
    assert_eq!(app.vendor, Vendor::Claude);
    assert_eq!(selected(&app).session.id.as_str(), ID);
}

#[test]
fn narrow_top_bar_drops_elsewhere_before_waiting() {
    // 窄到放不下「别处」时先让它走，「等你」那个点（单击跳过去）照旧在。
    for w in 20..=120 {
        let mut plain = app_with(None, now() - Duration::from_secs(3600));
        let before = dump(&render(&mut plain, w, 30));
        let mut both = app_with(None, now() - Duration::from_secs(3600));
        both.open_elsewhere.insert(SessionId::from(ID));
        let after = dump(&render(&mut both, w, 30));
        let top = |s: &str| s.lines().next().unwrap_or_default().to_owned();
        if top(&before).contains('◆') {
            assert!(top(&after).contains('◆'), "宽 {w}：{}", top(&after));
            assert!(!both.areas.status.is_empty(), "宽 {w}：「等你」要点得到");
        }
    }
}

#[test]
fn clicking_the_top_bar_resets_double_click() {
    // 在 Codex 单击一行，再点顶栏「别处」跳到 Claude，马上点同一屏幕行上的那条 Claude 对话：
    // 是单击（选中），不是双击（打开）。
    let mut app = app_with(None, now() - Duration::from_secs(3600));
    app.open_elsewhere.insert(SessionId::from(ID));
    let item_lines = |app: &mut App| -> Vec<u16> {
        render(app, 120, 30);
        let l = app.list();
        let list = app.areas.list;
        (l.offset..l.rows.len())
            .filter(|&i| {
                matches!(l.rows[i], huitai::ui::rows::Row::Item { entry, .. }
                    if !app.elsewhere(&l.entries[entry]))
            })
            .map(|i| list.y + (i - l.offset) as u16)
            .filter(|&y| y < list.bottom())
            .collect()
    };
    let claude = item_lines(&mut app);
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    // Codex 这边的项目组默认收着，展开第一组才有对话行。
    render(&mut app, 120, 30);
    app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    let codex = item_lines(&mut app);
    let row = *codex
        .iter()
        .find(|y| claude.contains(y))
        .expect("两家在同一屏幕行上都有对话");
    let x = app.areas.list.x + 6;
    let at = |x: u16, y: u16| ratatui::crossterm::event::MouseEvent {
        kind: ratatui::crossterm::event::MouseEventKind::Down(
            ratatui::crossterm::event::MouseButton::Left,
        ),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    let t0 = std::time::Instant::now();
    app.mouse(at(x, row), t0);
    let chip = app.areas.elsewhere;
    app.mouse(at(chip.x + 1, chip.y), t0);
    assert_eq!(app.vendor, Vendor::Claude);
    render(&mut app, 120, 30);
    let fx = app.mouse(at(x, row), t0 + Duration::from_millis(100));
    assert!(
        !fx.iter().any(|e| matches!(e, Effect::Open { .. })),
        "被当成双击打开了：{fx:?}"
    );
}

#[test]
fn clicking_anywhere_off_the_list_resets_double_click() {
    // 点列表一行，再点顶栏上不是按钮的地方（标签之间的分隔线），马上点回同一行：是单击，不是双击。
    let mut app = app_with(None, now() - Duration::from_secs(3600));
    render(&mut app, 120, 30);
    let list = app.areas.list;
    let tabs = app.areas.tabs.clone();
    let gap_x = tabs[0].1.right() + 1; // 第一个标签后面的「 │ 」
    let row = (list.y..list.bottom())
        .find(|&y| {
            let l = app.list();
            let i = l.offset + usize::from(y - list.y);
            matches!(l.rows.get(i), Some(huitai::ui::rows::Row::Item { entry, .. })
                if !app.elsewhere(&l.entries[*entry]))
        })
        .expect("有一条普通对话");
    let at = |x: u16, y: u16| ratatui::crossterm::event::MouseEvent {
        kind: ratatui::crossterm::event::MouseEventKind::Down(
            ratatui::crossterm::event::MouseButton::Left,
        ),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    let t0 = std::time::Instant::now();
    app.mouse(at(list.x + 6, row), t0);
    app.mouse(at(gap_x, tabs[0].1.y), t0);
    let fx = app.mouse(at(list.x + 6, row), t0 + Duration::from_millis(100));
    assert!(
        !fx.iter().any(|e| matches!(e, Effect::Open { .. })),
        "被当成双击打开了：{fx:?}"
    );
}
