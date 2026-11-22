//! 顶栏印章和全局状态：印章颜色随标签、三家合计的「●N 在跑 ◆N 等你」（只算自己的对话）、慢闪、
//! `!` / 单击跳到下一条等你回复的对话（跨家换标签）、窄终端的省略顺序，以及 `c` 复制的恢复命令。

mod ui_support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use huitai::core::{SessionId, SessionState, Vendor};
use huitai::sources::{ClaudeSource, CodexSource, Command, PiSource, Source, Trash};
use huitai::ui::app::{BLINK_HALF, Effect, Overlay, SPIN_STEP, SPINNER};
use huitai::ui::text::resume_line;
use huitai::ui::{App, Theme};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use ui_support::*;

const W: u16 = 120;
const H: u16 = 30;

fn top(app: &mut App, w: u16) -> String {
    dump(&render(app, w, H)).lines().next().unwrap().to_owned()
}

fn bang(app: &mut App) -> Vec<Effect> {
    app.key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT))
}

fn selected(app: &App) -> (Vendor, String) {
    let id = app
        .selected_entry()
        .expect("选中一条对话")
        .session
        .id
        .to_string();
    (app.vendor, id)
}

fn set_state(app: &mut App, id: &str, s: SessionState) {
    app.states.insert(SessionId::from(id), s);
}

/// 顶栏第一行里这个字符所在的格。
fn cell_of(buf: &Buffer, sym: &str) -> (u16, u16) {
    let x = (0..buf.area.width)
        .find(|&x| buf[(x, 0)].symbol() == sym)
        .unwrap_or_else(|| panic!("顶栏没有 {sym}"));
    (x, 0)
}

#[test]
fn seal_takes_accent_of_current_tab() {
    for theme in [Theme::DARK, Theme::LIGHT] {
        let mut app = demo_app(Vendor::Claude);
        for v in [Vendor::Claude, Vendor::Codex, Vendor::Pi, Vendor::Claude] {
            while app.vendor != v {
                app.key(key(KeyCode::Tab));
            }
            let buf = render_with(&mut app, W, H, &theme);
            let seal = cell_of(&buf, "会");
            assert_eq!(buf[seal].bg, theme.accent(v), "{v} 印章底色");
            assert_eq!(buf[seal].fg, theme.bar, "{v} 印章字是顶栏底色");
            // 印章是反白的一块：前后各留一格同色。
            assert_eq!(buf[(seal.0 - 1, 0)].bg, theme.accent(v));
            assert_eq!(buf[(seal.0 + 6, 0)].bg, theme.accent(v));
            assert_eq!(buf[(seal.0 + 7, 0)].bg, theme.bar, "印章后面回到顶栏底色");
        }
    }
}

#[test]
fn status_counts_own_conversations_across_vendors() {
    let mut app = demo_app(Vendor::Codex);
    // 在跑：c-shop-1、c-wt-1、x-api-1；等你：c-shop-2（刚做完）、c-blog-1、p-blog-1（刚做完）。
    assert_eq!(app.global_counts(), (3, 3));
    // 自动运行的对话不算，哪家的都不算。
    set_state(&mut app, "c-auto-1", SessionState::Running);
    set_state(&mut app, "c-auto-2", SessionState::Waiting);
    set_state(&mut app, "c-auto-4", SessionState::Done);
    assert_eq!(app.global_counts(), (3, 3));
    let t = top(&mut app, W);
    assert!(t.contains("◐3 在跑") && t.contains("◆3 等你"), "{t}");
    // 停在哪家都显示三家合计。
    app.key(key(KeyCode::Tab));
    assert!(top(&mut app, W).contains("◐3 在跑  ◆3 等你"));

    // 状态颜色：在跑绿、等你黄。
    let buf = render(&mut app, W, H);
    assert_eq!(
        buf[cell_of(&buf, "◐")].fg,
        Theme::DARK.state(SessionState::Running)
    );
    assert_eq!(
        buf[cell_of(&buf, "◆")].fg,
        Theme::DARK.state(SessionState::Waiting)
    );

    // 只剩在跑的：只显示在跑；都没有就都不显示。
    for id in ["c-shop-2", "c-blog-1", "p-blog-1"] {
        app.states.remove(&SessionId::from(id));
    }
    let t = top(&mut app, W);
    assert!(t.contains("◐3 在跑") && !t.contains('◆'), "{t}");
    app.states.clear();
    let t = top(&mut app, W);
    assert!(!t.contains('◐') && !t.contains('◆'), "{t}");
    assert!(app.areas.status.is_empty(), "没有状态就没有可点的地方");
}

#[test]
fn waiting_blinks_slowly_unless_turned_off() {
    let mut app = demo_app(Vendor::Claude);
    let waiting_fg = |app: &mut App| {
        let buf = render(app, W, H);
        buf[cell_of(&buf, "◆")].fg
    };
    let bright = Theme::DARK.state(SessionState::Waiting);
    // 亮 0.8 秒、暗 0.8 秒。
    assert!(!app.tick_blink(Duration::ZERO));
    assert_eq!(waiting_fg(&mut app), bright);
    assert!(app.tick_blink(BLINK_HALF));
    assert_eq!(waiting_fg(&mut app), Theme::DARK.faint);
    // 半拍内一直暗着（有在跑的对话时转圈照样换帧，所以这里不看返回值）。
    app.tick_blink(BLINK_HALF + Duration::from_millis(500));
    assert!(app.blink_dim, "半拍内一直暗着");
    assert!(app.tick_blink(BLINK_HALF * 2));
    assert_eq!(waiting_fg(&mut app), bright);
    // 在跑的不闪。
    app.tick_blink(BLINK_HALF);
    let buf = render(&mut app, W, H);
    assert_eq!(
        buf[cell_of(&buf, &app.spinner().to_string())].fg,
        Theme::DARK.state(SessionState::Running)
    );

    // 设置里关掉：常亮。
    app.key(key(KeyCode::Char(',')));
    for _ in 0..4 {
        app.key(key(KeyCode::Down));
    }
    assert_eq!(app.overlay, Overlay::Settings { cursor: 4 });
    let text = dump(&render(&mut app, W, H));
    assert!(
        text.contains("等你闪烁") && text.contains("‹ 开 ›"),
        "{text}"
    );
    app.key(key(KeyCode::Right));
    assert!(!app.config.blink_waiting);
    let saved = app.key(key(KeyCode::Esc));
    assert!(
        matches!(&saved[..], [Effect::SaveConfig(c)] if !c.blink_waiting),
        "{saved:?}"
    );
    assert!(app.tick_blink(BLINK_HALF), "关掉时马上回到亮的");
    app.tick_blink(BLINK_HALF * 3);
    assert!(!app.blink_dim, "关掉后一直亮着");
    assert_eq!(waiting_fg(&mut app), bright);

    // 没有等你回复的就不闪。
    app.config.blink_waiting = true;
    for id in ["c-shop-2", "c-blog-1", "p-blog-1"] {
        app.states.remove(&SessionId::from(id));
    }
    app.tick_blink(BLINK_HALF);
    assert!(!app.blink_dim, "没有等你回复的不闪");
}

#[test]
fn bang_jumps_to_next_waiting_across_vendors() {
    let mut app = demo_app(Vendor::Claude);
    // 自动运行的在等你也不跳过去（只算自己的）。
    set_state(&mut app, "c-auto-1", SessionState::Waiting);
    assert_eq!(selected(&app), (Vendor::Claude, "c-shop-1".to_owned()));

    // 同一组里往下。
    assert!(bang(&mut app).is_empty());
    assert_eq!(selected(&app), (Vendor::Claude, "c-shop-2".to_owned()));
    // 下一组（收起着就展开，展开记进标注）。
    let fx = bang(&mut app);
    assert_eq!(selected(&app), (Vendor::Claude, "c-blog-1".to_owned()));
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::SetExpanded { project, expanded: true } if project.as_path() == std::path::Path::new("/Users/demo/projects/blog")
    )));
    // 这家没有了：Codex 也没有，换到 pi。
    bang(&mut app);
    assert_eq!(selected(&app), (Vendor::Pi, "p-blog-1".to_owned()));
    // 绕一圈回到 Claude 第一条。
    bang(&mut app);
    assert_eq!(selected(&app), (Vendor::Claude, "c-shop-2".to_owned()));

    // 从组标题上找，先找这组里的。
    app.key(key(KeyCode::Home));
    assert!(app.selected_entry().is_none());
    bang(&mut app);
    assert_eq!(selected(&app), (Vendor::Claude, "c-shop-2".to_owned()));

    // 别家（列表空的位置）开始也一样按 Tab 顺序找。
    let mut app = demo_app(Vendor::Codex);
    bang(&mut app);
    assert_eq!(selected(&app), (Vendor::Pi, "p-blog-1".to_owned()));
    bang(&mut app);
    assert_eq!(selected(&app), (Vendor::Claude, "c-shop-2".to_owned()));
}

#[test]
fn bang_without_waiting_says_so() {
    let mut app = demo_app(Vendor::Claude);
    app.states.retain(|_, s| *s == SessionState::Running);
    let before = selected(&app);
    assert!(bang(&mut app).is_empty());
    assert_eq!(selected(&app), before);
    assert_eq!(app.status.as_deref(), Some("没有等你回复的对话"));
}

#[test]
fn clicking_status_jumps_like_bang() {
    let mut app = demo_app(Vendor::Codex);
    render(&mut app, W, H);
    let r = app.areas.status;
    assert!(!r.is_empty());
    let fx = app.mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: r.x + 1,
            row: r.y,
            modifiers: KeyModifiers::NONE,
        },
        Instant::now(),
    );
    assert!(fx.iter().all(|e| matches!(e, Effect::SetExpanded { .. })));
    assert_eq!(selected(&app), (Vendor::Pi, "p-blog-1".to_owned()));
}

/// 窄终端依次去掉：额度来源 → 第二个额度窗口 → 状态数字只留点 → 「额度」两字。
#[test]
fn narrow_top_bar_drops_in_order() {
    let mut app = demo_app(Vendor::Claude);
    // 不到五家时的去掉顺序（五家以上先把别的家缩成标记，见 ui_tabs）。
    only_first_four(&mut app);
    // 每一样东西在多宽时最后一次出现（从宽到窄扫，去掉了就不再回来）。
    let marks = [
        ("来源", "状态栏"),
        ("第二个窗口", "7 天"),
        ("状态的字", "在跑"),
        ("状态的数", "◐3"),
        ("「额度」两字", "额度"),
        ("额度条", "━"),
    ];
    let mut gone = [None; 6];
    for w in (40..=160u16).rev() {
        let t = top(&mut app, w);
        assert!(t.contains("会话台"), "印章一直在：{w} {t}");
        for (i, (name, needle)) in marks.iter().enumerate() {
            let shown = t.contains(needle);
            match gone[i] {
                None if !shown => gone[i] = Some(w),
                Some(at) => assert!(!shown, "{name} 在 {at} 列去掉后到 {w} 列又出现：{t}"),
                None => {}
            }
        }
        // 只要有额度，状态的点就还在。
        if t.contains('━') {
            assert!(t.contains('◐') && t.contains('◆'), "{w}: {t}");
        }
    }
    let gone: Vec<u16> = gone
        .iter()
        .zip(marks)
        .map(|(g, (name, _))| g.unwrap_or_else(|| panic!("{name} 一直没去掉")))
        .collect();
    assert!(
        gone.windows(2).all(|p| p[0] > p[1]),
        "去掉的顺序不对（各自去掉时的宽度）：{gone:?}"
    );
    // 120 列放得下两个窗口和完整状态，只省了来源。
    assert!(gone[0] >= 120 && gone[1] < 120, "{gone:?}");
}

#[test]
fn c_copies_resume_command_of_selected() {
    let mut app = demo_app(Vendor::Claude);
    let fx = app.key(key(KeyCode::Char('c')));
    let [Effect::CopyResume(e)] = &fx[..] else {
        panic!("{fx:?}");
    };
    assert_eq!(e.session.id.as_str(), "c-shop-1");
    let trash = || Trash::new("/tmp/huitai-test-trash-unused");
    let claude = ClaudeSource::new("/unused", trash());
    assert_eq!(
        resume_line(&claude.resume_cmd(&e.session), ""),
        "cd '/Users/demo/projects/shop' && claude --resume c-shop-1"
    );
    let codex = CodexSource::new("/unused", trash());
    let x = entry(
        Vendor::Codex,
        &codex_like("x-api-1", "/Users/demo/projects/api"),
    );
    assert_eq!(
        resume_line(&codex.resume_cmd(&x.session), ""),
        "cd '/Users/demo/projects/api' && codex resume x-api-1"
    );
    let pi = PiSource::new("/unused", trash());
    let p = entry(
        Vendor::Pi,
        &codex_like("p-blog-1", "/Users/demo/projects/blog"),
    );
    assert_eq!(
        resume_line(&pi.resume_cmd(&p.session), ""),
        "cd '/Users/demo/projects/blog' && pi --session p-blog-1"
    );

    // 目录里有空格、中文、单引号也能原样粘进 shell。
    let odd = Command {
        program: "claude".to_owned(),
        args: vec!["--resume".to_owned(), "abc".to_owned()],
        cwd: PathBuf::from("/Users/demo/我的 项目/it's"),
    };
    assert_eq!(
        resume_line(&odd, ""),
        r"cd '/Users/demo/我的 项目/it'\''s' && claude --resume abc"
    );

    // 选中组标题时没有命令可复制。
    app.key(key(KeyCode::Home));
    assert!(app.key(key(KeyCode::Char('c'))).is_empty());
    assert_eq!(app.status.as_deref(), Some("先选一条对话"));
}

fn codex_like(id: &'static str, cwd: &'static str) -> Fake {
    Fake {
        id,
        cwd,
        title: "t",
        mins: 1,
        messages: 1,
        branch: None,
        pinned: false,
        archived: false,
        automatic: false,
    }
}

#[test]
fn help_lists_new_keys() {
    let mut app = demo_app(Vendor::Claude);
    app.key(key(KeyCode::Char('?')));
    let text = dump(&render(&mut app, W, 40));
    for k in ["下一条等你回复的", "复制恢复命令", "顶栏 ◆"] {
        assert!(text.contains(k), "帮助页缺「{k}」：{text}");
    }
}

#[test]
fn spinner_turns_only_while_something_runs() {
    let mut app = demo_app(Vendor::Claude);
    // 有自己在跑的：每 120 毫秒换一帧，顶栏和列表都是这一帧。
    assert!(app.tick_blink(SPIN_STEP));
    assert_eq!(app.spinner(), SPINNER[1]);
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains(&format!("{}3 在跑", SPINNER[1])), "{text}");
    // 什么都没在跑（自己的都停了，后台和别处的文件也早不写了）：停在第一帧，不再要求重画。
    app.states.clear();
    app.now += Duration::from_secs(24 * 3600);
    assert!(app.tick_blink(SPIN_STEP * 3), "停下这一下要重画");
    assert_eq!(app.spin, 0);
    assert!(!app.tick_blink(SPIN_STEP * 5), "停着不重画");
}
