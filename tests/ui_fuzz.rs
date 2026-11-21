//! 界面 fuzz：随机按键（含修饰键、中文、emoji、控制字符）、鼠标（含越界坐标）、改窗口大小
//! （1×1、3 列、1 行、超宽超高）、中英文 emoji 混排和带转义序列的标题、坏预览，渲染不 panic、不越界，
//! 屏幕上不出现控制字符（标题里的 ESC 原样输出会被终端当成转义序列执行）。
//! 再测 5000 条对话时按键到重绘的耗时（方案指标 ≤16ms，release 跑）。数据全部手写、固定种子。

mod fuzz_support;
mod ui_support;

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant, SystemTime};

use fuzz_support::{MIXED, Rng};
use huitai::core::{SessionId, SessionState, Vendor};
use huitai::index::Entry;
use huitai::sources::{PreviewMessage, Role};
use huitai::ui::{App, Effect, Preview, Theme, view};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};

use ui_support::*;

/// 各种刁钻的窗口大小。
const SIZES: &[(u16, u16)] = &[
    (1, 1),
    (3, 1),
    (1, 3),
    (3, 10),
    (2, 40),
    (10, 2),
    (20, 5),
    (40, 12),
    (80, 24),
    (120, 40),
    (500, 60),
    (1000, 8),
    (7, 300),
];

const CWDS: &[&str] = &[
    "/Users/demo/projects/shop",
    "/Users/demo/projects/项目 中文",
    "/Users/demo/projects/🚀rocket",
    "/Users/demo",
    "/",
    "",
    "/tmp/scratch",
    "/Users/demo/projects/a/.claude/worktrees/1120-x",
    "/Users/demo/projects/very-long-name-very-long-name-very-long-name-very-long-name-very-long-name",
    "/Users/demo/projects/new\nline",
    "/Users/demo/projects/esc\u{1b}[2Jclear",
];

fn mixed_text(rng: &mut Rng, parts: usize) -> String {
    (0..parts)
        .map(|_| *rng.pick(MIXED))
        .collect::<Vec<_>>()
        .join(" ")
}

fn fuzz_entries(rng: &mut Rng, vendor: Vendor, n: usize) -> Vec<Entry> {
    let mut v: Vec<Entry> = (0..n)
        .map(|i| {
            let parts = 1 + rng.below(4);
            let fake = Fake {
                id: Box::leak(format!("{vendor:?}-{i}").into_boxed_str()),
                cwd: rng.pick(CWDS),
                title: Box::leak(mixed_text(rng, parts).into_boxed_str()),
                mins: rng.below(60 * 24 * 400) as u64,
                messages: rng.below(100_000),
                branch: rng.chance(30).then(|| *rng.pick(MIXED)),
                pinned: rng.chance(5),
                archived: rng.chance(10),
                automatic: rng.chance(30),
            };
            let mut e = entry(vendor, &fake);
            if rng.chance(10) {
                e.note.title = Some(mixed_text(rng, 2));
            }
            if rng.chance(5) {
                e.session.title = None;
            }
            if rng.chance(3) {
                // 时间在「现在」之后（时钟回拨、厂商写错）。
                e.session.last_active = now() + Duration::from_secs(3600);
            }
            e
        })
        .collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.session.last_active));
    v.sort_by_key(|e| !e.note.pinned);
    v
}

fn fuzz_preview(rng: &mut Rng, at: SystemTime) -> Preview {
    if rng.chance(10) {
        return Preview {
            at,
            messages: Err(mixed_text(rng, 3)),
        };
    }
    let messages = (0..rng.below(12))
        .map(|_| PreviewMessage {
            role: if rng.chance(50) {
                Role::User
            } else {
                Role::Assistant
            },
            text: if rng.chance(10) {
                "很长的一段".repeat(500)
            } else {
                (0..1 + rng.below(6))
                    .map(|_| mixed_text(rng, 3))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
            at: rng.chance(80).then_some(at),
        })
        .collect();
    Preview {
        at,
        messages: Ok(messages),
    }
}

const CHARS: &[char] = &[
    'j', 'k', 'h', 'l', 'g', 'G', '/', '?', 'n', 'N', 'r', 'p', 'a', 'd', 'u', 'x', 'c', 'e', 'o',
    'f', 'R', 'T', 's', 'q', '1', '5', '9', '0', '!', ' ', '中', '文', '🚀', '\u{1b}', '\u{7}',
    ',', ',', '?', '\u{0}', '\t', 'é',
];

fn random_key(rng: &mut Rng) -> KeyEvent {
    let code = match rng.below(14) {
        0..=5 => KeyCode::Char(*rng.pick(CHARS)),
        6 => *rng.pick(&[KeyCode::Up, KeyCode::Down, KeyCode::Left, KeyCode::Right]),
        7 => *rng.pick(&[
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
        ]),
        8 => KeyCode::Enter,
        9 => KeyCode::Esc,
        10 => *rng.pick(&[KeyCode::Tab, KeyCode::BackTab]),
        11 => KeyCode::Backspace,
        12 => *rng.pick(&[
            KeyCode::Delete,
            KeyCode::Insert,
            KeyCode::F(1),
            KeyCode::F(12),
        ]),
        _ => KeyCode::Null,
    };
    let mods = *rng.pick(&[
        KeyModifiers::NONE,
        KeyModifiers::NONE,
        KeyModifiers::NONE,
        KeyModifiers::SHIFT,
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::SUPER,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ]);
    let kind = *rng.pick(&[
        KeyEventKind::Press,
        KeyEventKind::Press,
        KeyEventKind::Press,
        KeyEventKind::Repeat,
        KeyEventKind::Release,
    ]);
    KeyEvent {
        code,
        modifiers: mods,
        kind,
        state: KeyEventState::NONE,
    }
}

fn random_mouse(rng: &mut Rng, w: u16, h: u16) -> MouseEvent {
    let kind = *rng.pick(&[
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::Down(MouseButton::Right),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Moved,
        MouseEventKind::ScrollDown,
        MouseEventKind::ScrollUp,
        MouseEventKind::ScrollLeft,
    ]);
    // 坐标偶尔越界（窗口刚缩小、事件还是按旧大小来的）。
    let x = rng.below(w as usize + 3) as u16;
    let y = rng.below(h as usize + 3) as u16;
    let (x, y) = if rng.chance(2) {
        (u16::MAX, u16::MAX)
    } else {
        (x, y)
    };
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

/// 屏幕上每一格都不能有控制字符；格子数等于窗口大小。
fn assert_clean(term: &Terminal<TestBackend>, step: usize, seed: u64) {
    let buf = term.backend().buffer();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let sym = buf[(x, y)].symbol();
            assert!(
                !sym.chars().any(char::is_control),
                "种子 {seed} 第 {step} 步：({x},{y}) 出现控制字符 {sym:?}"
            );
        }
    }
}

/// 运行层对效果的反应里，会改界面数据的那几个照着模拟（搜索结果、git 状态输出）。
fn apply(app: &mut App, effects: Vec<Effect>, rng: &mut Rng) {
    for e in effects {
        match e {
            Effect::Search { vendor, query } => {
                let all = app.list_of(vendor).entries.clone();
                let hits: Vec<Entry> = all.into_iter().filter(|_| rng.chance(20)).collect();
                app.set_hits(vendor, &query, hits);
            }
            Effect::GitStatus(dir) => {
                let out = if rng.chance(70) {
                    Ok(format!(
                        " M {}\n?? {}",
                        mixed_text(rng, 2),
                        "很长".repeat(200)
                    ))
                } else {
                    Err(mixed_text(rng, 2))
                };
                app.git = Some((dir, out));
            }
            _ => {}
        }
    }
}

fn run_fuzz(seed: u64, steps: usize) {
    let mut rng = Rng::new(seed);
    let mut app = demo_app(Vendor::Claude);
    for v in Vendor::ALL {
        let n = *rng.pick(&[0, 1, 3, 40, 300]);
        app.set_entries(v, fuzz_entries(&mut rng, v, n));
    }
    let mut size = *rng.pick(SIZES);
    let mut term = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
    let themes = [Theme::DARK, Theme::LIGHT];
    for step in 0..steps {
        match rng.below(100) {
            0..=59 => {
                let effects = app.key(random_key(&mut rng));
                apply(&mut app, effects, &mut rng);
            }
            60..=79 => {
                let ev = random_mouse(&mut rng, size.0, size.1);
                let effects = app.mouse(ev, Instant::now());
                apply(&mut app, effects, &mut rng);
            }
            80..=87 => {
                size = *rng.pick(SIZES);
                term.backend_mut().resize(size.0, size.1);
            }
            88..=90 => {
                // 后台扫描换上新列表（条目多了、少了、清空）。
                let v = *rng.pick(&Vendor::ALL);
                let n = *rng.pick(&[0, 1, 5, 120]);
                app.set_entries(v, fuzz_entries(&mut rng, v, n));
            }
            91..=92 => {
                let ids: HashSet<SessionId> = app
                    .list()
                    .entries
                    .iter()
                    .filter(|_| rng.chance(10))
                    .map(|e| e.session.id.clone())
                    .collect();
                app.set_interrupted(ids);
            }
            93..=94 => {
                if let Some(e) = app.selected_entry().cloned() {
                    let s = *rng.pick(&[
                        SessionState::Running,
                        SessionState::Waiting,
                        SessionState::Done,
                        SessionState::Stopped,
                    ]);
                    app.states.insert(e.session.id.clone(), s);
                    if rng.chance(50) {
                        app.attached.insert(e.session.id.clone());
                    }
                    if rng.chance(30) {
                        app.live = Some((e.session.id, live_screen()));
                    }
                }
            }
            95..=96 => app.status = Some(mixed_text(&mut rng, 2)),
            _ => app.now = now() + Duration::from_secs(rng.below(1_000_000) as u64),
        }
        if let Some(e) = app.take_preview_request() {
            let p = fuzz_preview(&mut rng, e.session.last_active);
            app.set_preview(e.session.id, p);
        }
        let theme = &themes[step % 2];
        term.draw(|f| view::draw(f, &mut app, theme))
            .unwrap_or_else(|e| panic!("种子 {seed} 第 {step} 步画不出来：{e}"));
        assert_eq!(term.backend().buffer().area.width, size.0);
        assert_clean(&term, step, seed);
        if app.quit {
            app.quit = false;
        }
    }
}

#[test]
fn ui_survives_random_input() {
    for seed in 1..=6 {
        run_fuzz(seed, 1500);
    }
}

#[test]
#[ignore = "长时间 fuzz，手动跑：cargo test --release --test ui_fuzz -- --ignored"]
fn ui_survives_long_fuzz() {
    for seed in 100..140 {
        run_fuzz(seed, 20_000);
    }
}

/// 设置页、帮助页在任何窗口大小、选中任何一项时都画得出来（窗口很矮时设置页选中第 2、3 项曾越界 panic）。
#[test]
fn overlays_fit_any_window_size() {
    for open in [',', '?'] {
        for &(w, h) in SIZES.iter().chain(&[(30, 4), (40, 5), (64, 6), (80, 9)]) {
            let mut app = demo_app(Vendor::Claude);
            app.key(key(KeyCode::Char(open)));
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            for _ in 0..4 {
                term.draw(|f| view::draw(f, &mut app, &Theme::DARK))
                    .unwrap_or_else(|e| panic!("{open} {w}×{h}: {e}"));
                assert_clean(&term, 0, 0);
                if open == ',' {
                    app.key(key(KeyCode::Down));
                }
            }
        }
    }
}

/// 标题、路径、分支、预览里的控制字符和转义序列不会原样到屏幕上。
#[test]
fn escape_sequences_in_data_never_reach_the_screen() {
    let mut rng = Rng::new(42);
    let mut app = demo_app(Vendor::Claude);
    let mut entries = fuzz_entries(&mut rng, Vendor::Claude, 30);
    for e in &mut entries {
        e.session.title = Some("\u{1b}]0;偷改标题\u{7}\u{1b}[2J清屏".to_owned());
        e.session.git_branch = Some("br\u{1b}[31manch".to_owned());
        e.session.cwd = "/Users/demo/projects/esc\u{1b}[2Jdir".into();
        e.note.title = None;
    }
    app.set_entries(Vendor::Claude, entries);
    if let Some(e) = app.take_preview_request() {
        let p = Preview {
            at: e.session.last_active,
            messages: Ok(vec![PreviewMessage {
                role: Role::User,
                text: "前\u{1b}[31m红\u{1b}[0m后\r\n\u{8}退格\u{7}".to_owned(),
                at: None,
            }]),
        };
        app.set_preview(e.session.id, p);
    }
    for (w, h) in [(120, 40), (60, 20), (3, 3)] {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| view::draw(f, &mut app, &Theme::DARK))
            .unwrap();
        assert_clean(&term, 0, 42);
    }
}

/// 5000 条对话（每家都满），中英文 emoji 混排标题，200×50：每次按键到画完一帧（含编码成终端字节，写进 sink）
/// 不超过方案指标 16ms。debug 构建慢十倍左右，只在 release 下按指标判。
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "按 release 测：cargo test --release --test ui_fuzz key_to_redraw -- --nocapture"
)]
fn key_to_redraw_with_5000_sessions() {
    let mut rng = Rng::new(5000);
    let mut app = demo_app(Vendor::Claude);
    for v in Vendor::ALL {
        app.set_entries(v, fuzz_entries(&mut rng, v, 5000));
    }
    let opts = TerminalOptions {
        viewport: Viewport::Fixed(Rect::new(0, 0, 200, 50)),
    };
    let mut term = Terminal::with_options(CrosstermBackend::new(io::sink()), opts).unwrap();
    term.draw(|f| view::draw(f, &mut app, &Theme::DARK))
        .unwrap();
    let mut keys: Vec<KeyCode> = Vec::new();
    keys.extend([KeyCode::Char('j'); 200]);
    keys.extend([KeyCode::PageDown; 30]);
    keys.extend([KeyCode::End, KeyCode::Home]);
    keys.extend([KeyCode::Tab, KeyCode::Tab, KeyCode::Tab]);
    keys.extend([
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Enter,
        KeyCode::Char('h'),
    ]);
    keys.extend([KeyCode::Char('?'), KeyCode::Esc]);
    keys.extend([KeyCode::Char('/'), KeyCode::Char('结'), KeyCode::Char('算')]);
    keys.extend([KeyCode::Backspace, KeyCode::Backspace, KeyCode::Esc]);
    keys.extend([KeyCode::Char('k'); 100]);
    let mut times: Vec<Duration> = keys
        .iter()
        .map(|k| {
            let t = Instant::now();
            let effects = app.key(key(*k));
            apply(&mut app, effects, &mut rng);
            load_preview(&mut app);
            term.draw(|f| view::draw(f, &mut app, &Theme::DARK))
                .unwrap();
            t.elapsed()
        })
        .collect();
    // 后台扫完换上新列表（5000 条重新分组）也算一次重绘。
    let t = Instant::now();
    let entries = app.list().entries.clone();
    app.set_entries(Vendor::Claude, entries);
    term.draw(|f| view::draw(f, &mut app, &Theme::DARK))
        .unwrap();
    let reload = t.elapsed();
    times.sort();
    let pct = |p: usize| times[(times.len() - 1) * p / 100];
    println!(
        "5000 条×3 家，按键→重绘 {} 次：中位 {:?}，p99 {:?}，最慢 {:?}；换上新列表并重绘 {reload:?}",
        times.len(),
        pct(50),
        pct(99),
        times[times.len() - 1]
    );
    assert!(pct(99) <= Duration::from_millis(16), "p99 {:?}", pct(99));
}
