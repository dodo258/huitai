//! 整段对话的只读滚动视图：v / → 打开，读全文的请求只发一次，工具调用默认折叠、t 展开，
//! 从最后看起、g 回顶，从搜索结果打开时高亮搜索词并跳到第一处、n 跳下一处，对话还在写时跟着更新，
//! ⏎ 接着聊，滚轮翻页，点击不穿透到列表，开着时不取实时画面。

mod ui_support;

use std::time::{Duration, Instant};

use huitai::core::{SessionId, SessionState, Vendor};
use huitai::sources::{Part, PreviewMessage, Role, Transcript};
use huitai::ui::{App, Effect};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use ui_support::*;

const W: u16 = 120;
const H: u16 = 30;

fn msg(role: Role, text: &str) -> Part {
    Part::Message(PreviewMessage {
        role,
        text: text.to_owned(),
        at: Some(now() - Duration::from_secs(600)),
    })
}

fn call(name: &str, input: &str) -> Part {
    Part::ToolCall {
        name: name.to_owned(),
        input: input.to_owned(),
    }
}

fn result(text: &str) -> Part {
    Part::ToolResult {
        text: text.to_owned(),
    }
}

/// 一段像样的对话：问题、两次工具调用、回答，再来一轮。
fn short_transcript() -> Transcript {
    Transcript {
        parts: vec![
            msg(Role::User, "结算按钮点了没反应，帮我查查"),
            msg(Role::Assistant, "我先看结算页的点击处理。"),
            call("Read", "src/pages/checkout.tsx"),
            result("export function Checkout() {\n  const onPay = () => {}\n}"),
            call("Bash", "npm test -- checkout"),
            result("1 failed: onPay 没有绑定到按钮"),
            msg(
                Role::Assistant,
                "找到了：onPay 没绑到按钮上。已经改好，测试通过。",
            ),
            msg(Role::User, "好，顺便看看优惠券那块"),
        ],
        truncated: false,
    }
}

/// 很长的对话：第 0..n 轮，每轮一问一答。
fn long_transcript(n: usize) -> Transcript {
    let mut parts = Vec::new();
    for i in 0..n {
        parts.push(msg(Role::User, &format!("第 {i} 个问题")));
        parts.push(msg(Role::Assistant, &format!("第 {i} 个回答")));
    }
    Transcript {
        parts,
        truncated: false,
    }
}

/// 停在一条停着的对话上，按 v 打开，把全文「读好」。
fn open_with(app: &mut App, t: Transcript) {
    app.key(key(KeyCode::Char('v')));
    let e = app.take_transcript_request().expect("打开后要读全文");
    app.set_transcript(&e.session.id, e.session.last_active, Ok(t));
}

fn stopped_app() -> App {
    let mut app = demo_app(Vendor::Claude);
    select_id(&mut app, "c-shop-3");
    app
}

#[test]
fn v_opens_reader_and_asks_for_the_transcript_once() {
    let mut app = stopped_app();
    app.key(key(KeyCode::Char('v')));
    assert!(app.reader.is_some());
    let e = app.take_transcript_request().expect("要读全文");
    assert_eq!(e.session.id.as_str(), "c-shop-3");
    assert!(
        app.take_transcript_request().is_none(),
        "同一个时间只要一次"
    );
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("对话全文"));
    assert!(text.contains("结算页首屏加载慢"), "标题带写着是哪条");
    assert!(text.contains("读取中…"));

    app.set_transcript(&e.session.id, e.session.last_active, Ok(short_transcript()));
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("结算按钮点了没反应，帮我查查"));
    assert!(text.contains("找到了：onPay 没绑到按钮上"));
    // 工具调用折成一行，返回内容不显示。
    assert!(text.contains("▸ 工具 ×2  Read · Bash"));
    assert!(!text.contains("npm test -- checkout"));
    assert!(text.contains("4 条消息 · 工具调用 2 次（已折叠）"));
    assert!(text.contains("t 展开工具"));
    assert!(text.contains("⏎ 接着聊   Esc 返回"));
    // 列表和预览被盖住了。
    assert!(!text.contains("周报模板和每周复盘"));

    app.key(key(KeyCode::Esc));
    assert!(app.reader.is_none());
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("周报模板和每周复盘"), "Esc 回到列表");
}

#[test]
fn right_arrow_on_a_conversation_opens_reader_but_expands_on_a_group() {
    let mut app = stopped_app();
    app.key(key(KeyCode::Right));
    assert!(app.reader.is_some(), "对话上 → 看全文");
    app.key(key(KeyCode::Left));
    assert!(app.reader.is_none(), "← 返回");
    // 组标题上 → 还是展开 / 进入，不开全文。
    app.key(key(KeyCode::Home));
    assert!(app.selected_entry().is_none());
    app.key(key(KeyCode::Right));
    assert!(app.reader.is_none());
}

#[test]
fn t_expands_tool_calls_and_results() {
    let mut app = stopped_app();
    open_with(&mut app, short_transcript());
    app.key(key(KeyCode::Char('t')));
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("▾ 工具 ×2"));
    assert!(text.contains("Read  src/pages/checkout.tsx"));
    assert!(text.contains("Bash  npm test -- checkout"));
    assert!(text.contains("1 failed: onPay 没有绑定到按钮"));
    assert!(text.contains("t 收起工具"));
    app.key(key(KeyCode::Char('t')));
    let text = dump(&render(&mut app, W, H));
    assert!(!text.contains("npm test -- checkout"));
}

#[test]
fn long_transcript_starts_at_the_end_and_scrolls() {
    let mut app = stopped_app();
    open_with(&mut app, long_transcript(40));
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("第 39 个回答"), "从最后看起");
    assert!(!text.contains("第 0 个问题"));
    app.key(key(KeyCode::Char('g')));
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("第 0 个问题"), "g 回到顶上");
    assert!(!text.contains("第 39 个回答"));
    assert!(text.contains("1–"), "底栏写着看到第几行");
    // 往下一行、翻一页、G 到底。
    app.key(key(KeyCode::Down));
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("2–"));
    app.key(key(KeyCode::Char(' ')));
    let text = dump(&render(&mut app, W, H));
    assert!(!text.contains("第 0 个问题"), "空格翻页");
    app.key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("第 39 个回答"));
    // 滚轮往上翻。
    let wheel = |kind| MouseEvent {
        kind,
        column: 40,
        row: 10,
        modifiers: KeyModifiers::NONE,
    };
    for _ in 0..3 {
        app.mouse(wheel(MouseEventKind::ScrollUp), Instant::now());
    }
    let text = dump(&render(&mut app, W, H));
    assert!(!text.contains("第 39 个回答"), "滚轮往上翻了");
}

#[test]
fn clicks_do_not_reach_the_list_underneath() {
    let mut app = stopped_app();
    open_with(&mut app, short_transcript());
    render(&mut app, W, H);
    let before = app.selected_entry().map(|e| e.session.id.clone());
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 10,
        row: 4,
        modifiers: KeyModifiers::NONE,
    };
    let t0 = Instant::now();
    assert!(app.mouse(click, t0).is_empty());
    assert!(
        app.mouse(click, t0 + Duration::from_millis(100)).is_empty(),
        "双击也不打开"
    );
    assert_eq!(app.selected_entry().map(|e| e.session.id.clone()), before);
    assert!(app.reader.is_some());
}

#[test]
fn opened_from_search_highlights_and_jumps_between_hits() {
    let mut app = stopped_app();
    app.key(key(KeyCode::Char('/')));
    for c in "onpay".chars() {
        app.key(key(KeyCode::Char(c)));
    }
    let hit = app.list().entries[0].clone();
    let id = hit.session.id.clone();
    app.set_hits(Vendor::Claude, "onpay", vec![hit]);
    app.key(key(KeyCode::Right));
    assert!(app.reader.is_some(), "搜索结果里 → 看全文");
    let e = app.take_transcript_request().unwrap();
    assert_eq!(e.session.id, id);
    // 前面垫很多轮，搜索词出现在中间和最后。
    let mut t = long_transcript(30);
    t.parts
        .insert(20, msg(Role::Assistant, "第一处：onPay 没绑定"));
    t.parts.push(msg(Role::Assistant, "第二处：OnPay 改好了"));
    app.set_transcript(&e.session.id, e.session.last_active, Ok(t));
    let buf = render(&mut app, W, H);
    let text = dump(&buf);
    assert!(text.contains("第一处：onPay 没绑定"), "跳到第一处");
    assert!(!text.contains("第 0 个问题"));
    assert!(text.contains("n N 下一处 / 上一处"));
    // 搜索词那几格换了底色。
    let (x, y) = find(&text, "onPay").expect("屏幕上有搜索词");
    let bg = buf[(x, y)].bg;
    let plain = find(&text, "第一处").map(|(x, y)| buf[(x, y)].bg).unwrap();
    assert_ne!(bg, plain, "搜索词高亮");

    app.key(key(KeyCode::Char('n')));
    let text = dump(&render(&mut app, W, H));
    assert!(
        text.contains("第二处：OnPay 改好了"),
        "n 跳到下一处（不分大小写）"
    );
    app.key(key(KeyCode::Char('n')));
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("第一处：onPay 没绑定"), "到头了绕回第一处");
    // Esc 回到搜索结果，搜索还在。
    app.key(key(KeyCode::Esc));
    assert!(app.reader.is_none());
    assert!(app.search.is_some());
}

/// 第 `y` 行里 `needle` 在哪一格（按显示宽度算）。
fn find(text: &str, needle: &str) -> Option<(u16, u16)> {
    text.lines().enumerate().find_map(|(y, l)| {
        let i = l.find(needle)?;
        let x = huitai::ui::text::width(&l[..i]);
        Some((x as u16, y as u16))
    })
}

#[test]
fn still_writing_conversation_reloads_and_stays_at_the_end() {
    let mut app = stopped_app();
    open_with(&mut app, long_transcript(20));
    assert!(app.take_transcript_request().is_none());
    // 对话写了新内容：列表里的最后时间变了，再读一次，贴着底的还在底。
    let mut entries = claude_entries();
    let e = entries
        .iter_mut()
        .find(|e| e.session.id.as_str() == "c-shop-3")
        .unwrap();
    e.session.last_active += Duration::from_secs(5);
    let at = e.session.last_active;
    app.set_entries(Vendor::Claude, entries);
    let req = app.take_transcript_request().expect("时间变了重读");
    assert_eq!(req.session.last_active, at);
    let mut t = long_transcript(20);
    t.parts.push(msg(Role::Assistant, "刚写进来的一句"));
    app.set_transcript(&req.session.id, at, Ok(t));
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("刚写进来的一句"));
    // 别的对话的旧结果丢掉。
    app.set_transcript(&SessionId::from("c-shop-2"), at, Ok(long_transcript(1)));
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("刚写进来的一句"));
}

#[test]
fn errors_and_empty_transcripts_say_so() {
    let mut app = stopped_app();
    app.key(key(KeyCode::Char('v')));
    let e = app.take_transcript_request().unwrap();
    app.set_transcript(&e.session.id, e.session.last_active, Err("没有权限".into()));
    assert!(dump(&render(&mut app, W, H)).contains("读不了：没有权限"));
    app.key(key(KeyCode::Esc));
    app.key(key(KeyCode::Char('v')));
    let e = app.take_transcript_request().unwrap();
    app.set_transcript(
        &e.session.id,
        e.session.last_active,
        Ok(Transcript::default()),
    );
    assert!(dump(&render(&mut app, W, H)).contains("这条对话还没有内容"));
}

#[test]
fn truncated_transcript_says_only_the_tail_was_read() {
    let mut app = stopped_app();
    let mut t = short_transcript();
    t.truncated = true;
    open_with(&mut app, t);
    app.key(key(KeyCode::Char('g')));
    assert!(dump(&render(&mut app, W, H)).contains("文件太大，只读了最后 32 MB"));
}

#[test]
fn enter_continues_the_conversation() {
    let mut app = stopped_app();
    open_with(&mut app, short_transcript());
    let fx = app.key(key(KeyCode::Enter));
    match fx.as_slice() {
        [Effect::Open { entry, here: false }] => assert_eq!(entry.session.id.as_str(), "c-shop-3"),
        other => panic!("应该打开这条：{other:?}"),
    }
    let fx = app.key(key(KeyCode::Char('w')));
    assert!(matches!(fx.as_slice(), [Effect::Open { here: true, .. }]));
}

#[test]
fn s_exports_the_open_conversation_and_shows_where_it_went() {
    let mut app = stopped_app();
    open_with(&mut app, short_transcript());
    let fx = app.key(key(KeyCode::Char('s')));
    match fx.as_slice() {
        [Effect::Export(entry)] => assert_eq!(entry.session.id.as_str(), "c-shop-3"),
        other => panic!("应该导出这条：{other:?}"),
    }
    assert!(app.reader.is_some(), "导出后还在看全文");
    assert!(dump(&render(&mut app, W, H)).contains("s 存成 Markdown"));
    // 运行层存完把路径写进状态栏。
    app.status = Some("已存到 ~/Downloads/修结算-2026-11-20.md".to_owned());
    assert!(dump(&render(&mut app, W, H)).contains("已存到 ~/Downloads/修结算-2026-11-20.md"));
}

#[test]
fn no_live_screen_while_reading_a_running_conversation() {
    let mut app = demo_app(Vendor::Claude);
    // c-shop-1 在跑：选中它时要实时画面，开着全文时不要。
    assert_eq!(
        app.live_target().map(|i| i.to_string()).as_deref(),
        Some("c-shop-1")
    );
    assert_eq!(
        app.state_of(&SessionId::from("c-shop-1")),
        SessionState::Running
    );
    app.key(key(KeyCode::Char('v')));
    assert!(app.live_target().is_none());
}

#[test]
fn v_works_from_the_waiting_board_too() {
    let mut app = demo_app(Vendor::Claude);
    app.key(key(KeyCode::Char('s')));
    assert!(app.board.is_some());
    let want = app.selected_entry().map(|e| e.session.id.clone()).unwrap();
    app.key(key(KeyCode::Char('v')));
    let e = app.take_transcript_request().expect("汇总里选中的那条");
    assert_eq!(e.session.id, want);
    app.key(key(KeyCode::Esc));
    assert!(app.reader.is_none());
    assert!(app.board.is_some(), "Esc 回到汇总");
    app.key(key(KeyCode::Right));
    assert!(app.reader.is_some(), "汇总里 → 也看全文");
}

#[test]
fn reader_snapshot() {
    let mut app = stopped_app();
    open_with(&mut app, short_transcript());
    assert_snapshot("reader", &dump(&render(&mut app, W, H)));
    app.key(key(KeyCode::Char('t')));
    assert_snapshot("reader-tools", &dump(&render(&mut app, W, H)));
}
