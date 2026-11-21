//! 会话台界面快照：三家标签、空列表、搜索、帮助页、窄终端。TestBackend 渲染，和 tests/ui_snapshots/ 比对；
//! 颜色另外按格检查（强调色随厂商切换、选中行淡底加细条、底色分区）。

mod ui_support;

use huitai::core::{Config, SessionId, Vendor};
use huitai::ui::rows::Row;
use huitai::ui::{App, Theme};
use ratatui::crossterm::event::KeyCode;
use ratatui::style::Color;
use std::path::PathBuf;

use ui_support::*;

const W: u16 = 120;
const H: u16 = 30;

fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
    dump(buf)
        .lines()
        .nth(usize::from(y))
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn claude_tab() {
    let mut app = demo_app(Vendor::Claude);
    let buf = render(&mut app, W, H);
    assert_snapshot("claude", &dump(&buf));
}

#[test]
fn codex_tab() {
    let mut app = demo_app(Vendor::Codex);
    let buf = render(&mut app, W, H);
    assert_snapshot("codex", &dump(&buf));
}

#[test]
fn pi_tab() {
    let mut app = demo_app(Vendor::Pi);
    let buf = render(&mut app, W, H);
    assert_snapshot("pi", &dump(&buf));
}

#[test]
fn empty_list() {
    let mut app = App::new(
        PathBuf::from(HOME),
        PathBuf::from("/Users/demo/projects/shop"),
        Config::default(),
    );
    app.now = now();
    let buf = render(&mut app, W, H);
    let text = dump(&buf);
    assert!(text.contains("还没有 Claude 对话"));
    assert!(text.contains("额度读取中"));
    assert_snapshot("empty", &text);
}

#[test]
fn search() {
    let mut app = demo_app(Vendor::Claude);
    app.key(key(KeyCode::Char('/')));
    for c in "结算".chars() {
        app.key(key(KeyCode::Char(c)));
    }
    let hits: Vec<_> = claude_entries()
        .into_iter()
        .filter(|e| e.title().contains("结算"))
        .collect();
    app.set_hits(Vendor::Claude, "结算", hits);
    load_preview(&mut app);
    let buf = render(&mut app, W, H);
    assert_snapshot("search", &dump(&buf));
}

#[test]
fn search_without_hits() {
    let mut app = demo_app(Vendor::Claude);
    app.key(key(KeyCode::Char('/')));
    app.key(key(KeyCode::Char('z')));
    app.set_hits(Vendor::Claude, "z", Vec::new());
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("没找到「z」"));
}

#[test]
fn help_page() {
    let mut app = demo_app(Vendor::Claude);
    app.key(key(KeyCode::Char('?')));
    let buf = render(&mut app, W, H);
    let text = dump(&buf);
    for k in [
        "⌥⏎",
        "恢复上次所有标签",
        "进回收站",
        "看改了哪些文件",
        "双击",
    ] {
        assert!(text.contains(k), "帮助页缺「{k}」");
    }
    assert_snapshot("help", &text);
}

#[test]
fn narrow_terminal_hides_preview() {
    let mut app = demo_app(Vendor::Claude);
    let buf = render(&mut app, 70, 20);
    assert_snapshot("narrow", &dump(&buf));
}

#[test]
fn accent_follows_vendor() {
    for v in Vendor::ALL {
        let mut app = demo_app(v);
        let buf = render(&mut app, W, H);
        let accent = Theme::DARK.accent(v);
        // 印章「会话台」用当前这家的强调色做底、顶栏底色做字；当前标签只用强调色写字，不铺底。
        assert_eq!(buf[(1, 0)].bg, accent, "{v} 印章底色");
        assert_eq!(buf[(2, 0)].symbol(), "会");
        assert_eq!(buf[(2, 0)].fg, Theme::DARK.bar, "{v} 印章字色");
        let (_, tab) = *app.areas.tabs.iter().find(|(tv, _)| *tv == v).unwrap();
        assert_eq!(buf[(tab.x + 1, 0)].fg, accent, "{v} 当前标签字色");
        assert_eq!(
            buf[(tab.x + 1, 0)].bg,
            Theme::DARK.bar,
            "{v} 当前标签不铺底"
        );
        // 选中行：整行淡底，左边是强调色——对话是一道细条，项目组是加粗的序号。
        let y = (2..H - 1)
            .find(|y| buf[(1, *y)].bg == Theme::DARK.select)
            .expect("有选中行");
        let edge = buf[(0, y)].symbol();
        assert!(edge == "▎" || edge.parse::<u8>().is_ok(), "{v} {edge}");
        assert_eq!(buf[(0, y)].fg, accent);
        // 顶栏下面一行是表头带，和顶栏同色。
        assert_eq!(buf[(1, 1)].bg, Theme::DARK.bar);
        assert_eq!(buf[(W - 2, 1)].bg, Theme::DARK.bar);
        // 不选中的对话行、收起的组标题是列表底色（展开的组标题垫一层带强调色的底），预览区更深，顶栏底栏同色。
        let l = app.list();
        let list_y = app.areas.list.y;
        let other = (l.offset..l.rows.len())
            .find(|&i| {
                i != l.selected
                    && match l.rows[i] {
                        Row::Item { .. } => true,
                        Row::Header(g) => app.folds.is_collapsed(&l.groups[g].key),
                    }
            })
            .map(|i| list_y + (i - l.offset) as u16)
            // 只有一行（选中的）时，比列表下面的空行。
            .unwrap_or(list_y + (l.rows.len() - l.offset) as u16);
        assert_eq!(buf[(1, other)].bg, Theme::DARK.base);
        assert_eq!(buf[(W - 2, 5)].bg, Theme::DARK.preview);
        assert_eq!(buf[(W - 1, 0)].bg, Theme::DARK.bar);
        assert_eq!(buf[(W - 1, H - 1)].bg, Theme::DARK.bar);
    }
}

#[test]
fn light_theme_uses_light_palette() {
    let mut app = demo_app(Vendor::Codex);
    let buf = render_with(&mut app, W, H, &Theme::LIGHT);
    assert_eq!(buf[(W - 2, 5)].bg, Theme::LIGHT.preview);
    let (_, tab) = *app
        .areas
        .tabs
        .iter()
        .find(|(v, _)| *v == Vendor::Codex)
        .unwrap();
    assert_eq!(buf[(1, 0)].bg, Theme::LIGHT.accent(Vendor::Codex));
    assert_eq!(buf[(tab.x + 1, 0)].fg, Theme::LIGHT.accent(Vendor::Codex));
    assert_ne!(Theme::LIGHT.base, Theme::DARK.base);
    assert!(matches!(Theme::LIGHT.base, Color::Rgb(r, _, _) if r > 200));
}

#[test]
fn same_name_projects_show_path_and_preview_shows_full_path() {
    let mut app = demo_app(Vendor::Claude);
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("api  ~/projects/api"), "{text}");
    assert!(text.contains("api  ~/work/api"));
    // 预览元信息：项目完整路径（~ 缩写）和「已在标签页打开」。
    assert!(text.contains("~/projects/shop"));
    assert!(text.contains("已在标签页打开"));
    let top = row_text(&render(&mut app, W, H), 0);
    assert!(
        top.contains("会话台")
            && top.contains("✻ Claude")
            && top.contains("◎ Codex")
            && top.contains("π pi")
    );
    assert!(top.contains("5 小时") && top.contains("23%"));
}

#[test]
fn running_preview_shows_live_screen() {
    let mut app = demo_app(Vendor::Claude);
    assert_eq!(app.live_target(), Some(SessionId::from("c-shop-1")));
    app.live = Some((SessionId::from("c-shop-1"), live_screen()));
    let buf = render(&mut app, W, H);
    let text = dump(&buf);
    assert!(text.contains("此刻的画面"), "{text}");
    assert!(text.contains("✓ checkout.spec.ts (12 tests) 4.2s"));
    // 比预览区宽的行截断，消息预览不再显示。
    assert!(text.contains("正在跑 e2e 测试 (npm run test:e2e"));
    assert!(!text.contains("--workers=4"));
    assert!(!text.contains("移动端走的是同一个组件"));
    // 颜色和样式照原样：绿色对勾、反显的 RUN、真彩色的厂商标记。
    let find = |sym: &str| {
        let area = app.areas.preview;
        (area.top()..area.bottom())
            .flat_map(|y| (area.left()..area.right()).map(move |x| (x, y)))
            .find(|&p| buf[p].symbol() == sym)
            .unwrap_or_else(|| panic!("预览里没有 {sym}"))
    };
    assert_eq!(buf[find("✓")].fg, Color::Indexed(2));
    assert_eq!(buf[find("✻")].fg, Color::Rgb(217, 119, 87));
    let run = find("R");
    assert_eq!(buf[run].bg, Theme::DARK.text, "反显：背景换成正文色");
    assert_eq!(buf[run].fg, Theme::DARK.preview);
    assert_snapshot("live", &text);
}

#[test]
fn stopped_preview_shows_messages() {
    let mut app = demo_app(Vendor::Claude);
    select_id(&mut app, "c-shop-3");
    load_preview(&mut app);
    assert_eq!(app.live_target(), None, "已停的不取画面");
    // 即使还留着旧画面，已停的对话也只显示最后几条消息。
    app.live = Some((SessionId::from("c-shop-3"), live_screen()));
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("○ 已停"), "{text}");
    assert!(!text.contains("此刻的画面"));
    assert!(!text.contains("checkout.spec.ts"));
    assert!(text.contains("移动端走的是同一个组件"));
    assert_snapshot("stopped", &text);
}

#[test]
fn live_screen_of_another_session_is_not_shown() {
    let mut app = demo_app(Vendor::Claude);
    app.live = Some((SessionId::from("c-wt-1"), live_screen()));
    let text = dump(&render(&mut app, W, H));
    assert!(!text.contains("此刻的画面"));
    assert!(text.contains("移动端走的是同一个组件"));
}

/// 有新版：顶栏右侧额度前一句暗色「有新版 x.y.z · ai update」；设置里关了检查就不显示；后台是旧版的提示优先。
#[test]
fn new_version_hint_in_top_bar() {
    let mut app = demo_app(Vendor::Claude);
    let top = |app: &mut App, w| row_text(&render(app, w, H), 0);
    assert!(!top(&mut app, W).contains("有新版"));

    app.new_version = Some("0.2.0".to_owned());
    let wide = top(&mut app, W);
    assert!(wide.contains("有新版 0.2.0 · ai update"), "{wide}");
    let (hint, quota) = (wide.find("有新版").unwrap(), wide.find('%').unwrap());
    assert!(hint < quota, "提示在额度前面：{wide}");
    let buf = render(&mut app, W, H);
    let x = (0..W).find(|&x| buf[(x, 0)].symbol() == "有").unwrap();
    assert_eq!(buf[(x, 0)].fg, Theme::DARK.faint);

    app.config.check_updates = false;
    assert!(!top(&mut app, W).contains("有新版"), "关了检查就不提示");

    app.config.check_updates = true;
    app.old_host = Some(["a"].into_iter().map(SessionId::from).collect());
    let both = top(&mut app, W);
    assert!(
        both.contains("后台是旧版") && !both.contains("有新版"),
        "{both}"
    );
}

/// 后台是旧版：顶栏右侧额度前一句暗色提示，条数跟着托管的对话走；放不下额度时额度让位。
#[test]
fn old_daemon_hint_in_top_bar() {
    let mut app = demo_app(Vendor::Claude);
    let top = |app: &mut App, w| row_text(&render(app, w, H), 0);
    assert!(!top(&mut app, W).contains("后台是旧版"));

    app.old_host = Some(["a", "b"].into_iter().map(SessionId::from).collect());
    let wide = top(&mut app, W);
    assert!(
        wide.contains("后台是旧版，2 条对话结束后自动换新"),
        "{wide}"
    );
    let (hint, quota) = (wide.find("后台是旧版").unwrap(), wide.find('%').unwrap());
    assert!(hint < quota, "提示在额度前面：{wide}");
    let buf = render(&mut app, W, H);
    let x = (0..W).find(|&x| buf[(x, 0)].symbol() == "后").unwrap();
    assert_eq!(buf[(x, 0)].fg, Theme::DARK.faint);

    // 印章和全局状态占了左边，84 列时放得下提示、放不下额度。
    let narrow = top(&mut app, 84);
    assert!(narrow.contains("后台是旧版，2 条"), "{narrow}");
    assert!(!narrow.contains('%'), "放不下时额度让位：{narrow}");

    // 托管的对话都结束了（集合空了）就不提示。
    app.old_host = Some(Default::default());
    assert!(!top(&mut app, W).contains("后台是旧版"));
}
