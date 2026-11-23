//! 会话台界面快照：三家标签、空列表、搜索、帮助页、窄终端。TestBackend 渲染，和 tests/ui_snapshots/ 比对；
//! 颜色另外按格检查（强调色随厂商切换、选中行淡底加细条、底色分区）。

mod ui_support;

use huitai::core::{Config, SessionId, Vendor};
use huitai::index::{Hit, Snippet};
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

/// ⌃A 各家一起搜：Claude、Codex、pi 的结果混在一起，每行标题前标那家的标记，表头、搜索栏写着「各家」。
#[test]
fn search_all_vendors() {
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};
    let mut app = demo_app(Vendor::Claude);
    app.key(key(KeyCode::Char('/')));
    for c in "接口".chars() {
        app.key(key(KeyCode::Char(c)));
    }
    app.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    let pick = |entries: Vec<huitai::index::Entry>, title: &str| {
        entries.into_iter().find(|e| e.title() == title).unwrap()
    };
    let hit = |entry, in_title, in_body| Hit {
        entry,
        in_title,
        in_body,
    };
    app.set_search_hits(
        None,
        "接口",
        vec![
            hit(
                pick(codex_entries(), "给订单列表接口补单元测试"),
                true,
                false,
            ),
            hit(pick(claude_entries(), "把优惠券接口换成新版"), true, false),
            hit(pick(pi_entries(), "RSS 初稿"), false, true),
        ],
    );
    load_preview(&mut app);
    let buf = render(&mut app, W, H);
    let text = dump(&buf);
    assert!(text.contains("搜索结果 · 各家"));
    assert!(text.contains("各家 3 条"));
    assert!(text.contains("⌃A 只搜 Claude"));
    for v in [Vendor::Codex, Vendor::Claude, Vendor::Pi] {
        assert!(
            text.lines().any(|l| l.contains(&format!("{} ", v.mark()))),
            "{} 的结果行前有那家的标记",
            v.name()
        );
    }
    assert_snapshot("search_all", &text);
}

/// 搜「结算」：一条只命中标题、一条标题正文都命中、一条只命中正文（正文很长、命中在中间）。
fn snippet_app() -> App {
    let mut app = demo_app(Vendor::Claude);
    app.key(key(KeyCode::Char('/')));
    for c in "结算".chars() {
        app.key(key(KeyCode::Char(c)));
    }
    let entries = claude_entries();
    let pick = |title: &str| entries.iter().find(|e| e.title() == title).unwrap().clone();
    let hit = |title: &str, in_title, in_body| Hit {
        entry: pick(title),
        in_title,
        in_body,
    };
    app.set_search_hits(
        Some(Vendor::Claude),
        "结算",
        vec![
            hit("结算页首屏加载慢，查一下原因", true, false),
            hit("购物车结算按钮点了没反应", true, true),
            hit("把优惠券接口换成新版", false, true),
        ],
    );
    app
}

fn id_of(app: &App, title: &str) -> SessionId {
    let s = app.search.as_ref().unwrap();
    s.hits
        .iter()
        .find(|e| e.title() == title)
        .unwrap()
        .session
        .id
        .clone()
}

fn snip(text: &str, term: &str) -> Snippet {
    Snippet {
        text: text.to_owned(),
        marks: text
            .match_indices(term)
            .map(|(i, m)| (i, i + m.len()))
            .collect(),
    }
}

#[test]
fn search_snippets() {
    let mut app = snippet_app();
    // 第一帧：正文命中的两条先空着占位（暗色 …），只命中标题的不占行。
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("结算页首屏加载慢"));
    let (_, want) = app
        .take_snippet_request()
        .expect("看得见的正文命中要取片段");
    let want: Vec<&str> = want.iter().map(|e| e.title()).collect();
    assert_eq!(want, ["购物车结算按钮点了没反应", "把优惠券接口换成新版"]);
    assert_eq!(app.take_snippet_request(), None, "要过的不再要");
    assert_eq!(app.line_heights(), [1, 2, 2]);

    let cart = id_of(&app, "购物车结算按钮点了没反应");
    let coupon = id_of(&app, "把优惠券接口换成新版");
    app.set_snippet(
        "结算",
        cart,
        Some(snip(
            "找到了：结算按钮的 onClick 在优惠券还没算完时被禁用",
            "结算",
        )),
    );
    let long = format!(
        "…{}换成新版接口以后，结算页要跟着改参数名；{}…",
        "旧接口的返回字段有几个已经不用了，".repeat(4),
        "顺便补上单元测试，覆盖满减和折扣两种券。".repeat(6)
    );
    app.set_snippet("结算", coupon, Some(snip(&long, "结算")));
    // 查询词已经变了的旧结果丢掉。
    app.set_snippet("别的", id_of(&app, "结算页首屏加载慢，查一下原因"), None);
    load_preview(&mut app);
    let buf = render(&mut app, W, H);
    let text = dump(&buf);
    assert_snapshot("search-snippets", &text);

    // 命中词高亮：第 4 行（第二条结果下面那行）的「结算」加粗、垫强调色。
    let line = row_text(&buf, 4);
    let col = line
        .find("结算")
        .map(|b| huitai::ui::text::width(&line[..b]))
        .unwrap();
    let cell = &buf[(col as u16, 4)];
    assert_eq!(cell.symbol(), "结");
    assert!(cell.modifier.contains(ratatui::style::Modifier::BOLD));
    assert_ne!(cell.bg, buf[((col - 2) as u16, 4)].bg, "命中词垫底色");
}

#[test]
fn search_snippet_not_found_takes_no_line() {
    let mut app = snippet_app();
    render(&mut app, W, H);
    app.take_snippet_request();
    let coupon = id_of(&app, "把优惠券接口换成新版");
    app.set_snippet("结算", coupon, None);
    assert_eq!(app.line_heights(), [1, 2, 1]);
    // 同一个词再搜一遍（比如改了名），取到的留着，不再要。
    let s = app.search.clone().unwrap();
    let hits = s
        .hits
        .into_iter()
        .map(|entry| Hit {
            in_title: true,
            in_body: s.in_body.contains(&entry.session.id),
            entry,
        })
        .collect();
    app.set_search_hits(Some(Vendor::Claude), "结算", hits);
    assert_eq!(app.take_snippet_request(), None);
}

/// 审计 v0.1.4 第 24 条：查询词没变、对话正文更新了（时间、条数跟着变），那条的片段作废重新取；没变的留着。
#[test]
fn search_snippet_is_refetched_when_the_conversation_changes() {
    let mut app = snippet_app();
    render(&mut app, W, H);
    app.take_snippet_request();
    let cart = id_of(&app, "购物车结算按钮点了没反应");
    let coupon = id_of(&app, "把优惠券接口换成新版");
    app.set_snippet("结算", cart.clone(), Some(snip("旧的结算片段", "结算")));
    app.set_snippet("结算", coupon.clone(), Some(snip("结算没变", "结算")));

    let s = app.search.clone().unwrap();
    let hits = s
        .hits
        .into_iter()
        .map(|mut entry| {
            if entry.session.id == cart {
                entry.session.message_count += 1;
                entry.session.last_active += std::time::Duration::from_secs(60);
            }
            Hit {
                in_title: true,
                in_body: s.in_body.contains(&entry.session.id),
                entry,
            }
        })
        .collect();
    app.set_search_hits(Some(Vendor::Claude), "结算", hits);
    let (_, want) = app.take_snippet_request().expect("变了的那条要重新取");
    let want: Vec<&SessionId> = want.iter().map(|e| &e.session.id).collect();
    assert_eq!(want, [&cart]);
    assert_eq!(
        app.snippet_line(&coupon),
        Some(Some(&snip("结算没变", "结算")))
    );
}

#[test]
fn search_snippets_in_narrow_and_tiny_terminals() {
    for (w, h) in [(40, 12), (24, 6), (12, 4), (6, 3)] {
        let mut app = snippet_app();
        render(&mut app, w, h);
        let cart = id_of(&app, "购物车结算按钮点了没反应");
        app.set_snippet(
            "结算",
            cart,
            Some(snip(
                &format!("{}结算按钮{}", "很长的前文".repeat(30), "后".repeat(99)),
                "结算",
            )),
        );
        let text = dump(&render(&mut app, w, h));
        for l in text.lines() {
            assert!(huitai::ui::text::width(l) <= usize::from(w), "{w}×{h}：{l}");
        }
        if w >= 24 {
            assert!(
                text.contains("前文结算按钮"),
                "{w}×{h} 看得到命中词：{text}"
            );
        }
    }
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
    // 五家同屏：当前这家写全名，别的家缩成标记。
    assert!(
        top.contains("会话台") && top.contains("✻ Claude │ ◎ │ π │ ☤ │ K"),
        "{top}"
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

/// 那家 CLI 有新版：顶栏那家标签后紧跟一个暗色「↑」，别的家不标；缩成标记时也跟着；关了检查新版本就不标。
#[test]
fn cli_update_mark_after_the_tab() {
    let mut app = demo_app(Vendor::Claude);
    let top = |app: &mut App, w| row_text(&render(app, w, H), 0);
    assert!(!top(&mut app, W).contains('↑'));

    app.cli_updates.insert(Vendor::Codex, "0.172.0".to_owned());
    let compact = top(&mut app, W);
    assert!(compact.contains("◎↑"), "缩成标记的标签也带着：{compact}");
    assert!(!compact.contains("Claude↑"), "{compact}");
    let buf = render(&mut app, W, H);
    let x = (0..W).find(|&x| buf[(x, 0)].symbol() == "↑").unwrap();
    assert_eq!(buf[(x, 0)].fg, Theme::DARK.faint);
    app.vendor = Vendor::Codex;
    let full = top(&mut app, W);
    assert!(
        full.contains("◎ Codex↑"),
        "当前这家写全名，标记跟在后面：{full}"
    );

    app.config.check_updates = false;
    assert!(!top(&mut app, W).contains('↑'), "关了检查就不标");
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

    // 印章、全局状态和四家标签占了左边，95 列时放得下提示、放不下额度。
    let narrow = top(&mut app, 95);
    assert!(narrow.contains("后台是旧版，2 条"), "{narrow}");
    assert!(!narrow.contains('%'), "放不下时额度让位：{narrow}");

    // 托管的对话都结束了（集合空了）就不提示。
    app.old_host = Some(Default::default());
    assert!(!top(&mut app, W).contains("后台是旧版"));
}

/// 多账号：同一家有两个账号时，每条标题后面用暗色小字写账号名；顶栏额度跟着选中对话的账号换。
#[test]
fn two_accounts_label_rows_and_switch_quota() {
    use huitai::core::{Account, QuotaValue};
    let home = std::path::Path::new(HOME);
    let main = Account::new(Vendor::Claude, "默认", home.join(".claude"), home);
    let work = Account::new(Vendor::Claude, "工作号", home.join(".claude-work"), home);
    let mut entries = claude_entries();
    for e in entries
        .iter_mut()
        .filter(|e| e.session.id.as_str() == "c-shop-2")
    {
        e.session.id = SessionId::in_account(&work.key, "c-shop-2");
        e.session.account = work.key.clone();
    }
    let mut app = demo_app(Vendor::Claude);
    app.set_accounts(vec![main, work.clone()]);
    app.set_entries(Vendor::Claude, entries);
    let mut theirs = quota_for(Vendor::Claude);
    if let QuotaValue::Windows(w) = &mut theirs.value {
        w[0].used_percent = 88.0;
    }
    app.quota.insert((Vendor::Claude, work.key.clone()), theirs);
    select_id(&mut app, "c-shop-1");
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("23%"), "主账号的额度");
    let line = text
        .lines()
        .find(|l| l.contains("把优惠券接口换成新版"))
        .unwrap();
    assert!(line.contains("工作号"), "{line}");
    let line = text
        .lines()
        .find(|l| l.contains("购物车结算按钮点了没反应"))
        .unwrap();
    assert!(line.contains("默认"), "{line}");
    assert_snapshot("accounts", &text);

    // 别处开着它的进程参数里是不带账号前缀的 id，也认得出别处在用。
    app.open_elsewhere.insert(SessionId::from("c-shop-2"));
    let theirs = app
        .list()
        .entries
        .iter()
        .find(|e| e.session.account == work.key)
        .unwrap()
        .clone();
    assert!(app.elsewhere(&theirs));
    app.open_elsewhere.clear();

    select_id(&mut app, &format!("{}:c-shop-2", work.key));
    let text = dump(&render(&mut app, W, H));
    assert!(
        text.contains("88%"),
        "选中工作号的对话，顶栏换成工作号的额度\n{text}"
    );
}

/// 上下文占用：有读数的行标题后写「上下文 N%」，80% 起黄、95% 起红；预览区标题行跟着写。
/// 没读数的、读数之后又在会话台外面聊过（会话文件比读数新 10 分钟以上）的不写。
#[test]
fn context_use_on_rows_and_preview_head() {
    use huitai::core::SessionState;
    use huitai::quota::ContextUse;
    use std::time::Duration;

    let mut app = demo_app(Vendor::Claude);
    let plain = dump(&render(&mut app, W, H));
    assert!(!plain.contains("上下文"), "没读数不写：{plain}");

    let ago = |m: u64| now() - Duration::from_secs(m * 60);
    let put = |app: &mut App, id: &str, percent: f32, at| {
        app.contexts
            .insert(SessionId::from(id), ContextUse { percent, at });
    };
    put(&mut app, "c-shop-1", 72.4, ago(3));
    put(&mut app, "c-shop-2", 85.0, ago(26));
    // c-shop-3 最后在 30 小时前，读数比它早 11 分钟：过时，不写。
    put(&mut app, "c-shop-3", 40.0, ago(60 * 30 + 11));
    let buf = render(&mut app, W, H);
    let text = dump(&buf);
    let line_of = |s: &str| {
        text.lines()
            .position(|l| l.contains(s))
            .unwrap_or_else(|| panic!("没有 {s}：{text}")) as u16
    };
    let row1 = row_text(&buf, line_of("购物车结算按钮点了没反应  "));
    assert!(row1.contains("上下文 72%"), "{row1}");
    assert!(row_text(&buf, line_of("把优惠券接口换成新版")).contains("上下文 85%"));
    assert!(!row_text(&buf, line_of("结算页首屏加载慢")).contains("上下文"));
    // 选中的是在跑的 c-shop-1：预览区标题行「在跑 … · 上下文 72%」。
    // 预览区标题行就在预览区上面一行。
    let head_text = row_text(&buf, app.areas.preview.y - 1);
    assert!(head_text.contains("· 上下文 72%"), "{head_text}");
    assert_snapshot("context", &text);

    // 颜色：没到 80% 是暗色，80% 起黄。
    let color_at = |buf: &ratatui::buffer::Buffer, y: u16, s: &str| {
        let line = row_text(buf, y);
        let col = line[..line.find(s).unwrap()]
            .chars()
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as u16);
        buf[(col.sum(), y)].fg
    };
    assert_eq!(
        color_at(&buf, line_of("购物车结算按钮点了没反应  "), "上下文"),
        Theme::DARK.muted
    );
    assert_eq!(
        color_at(&buf, line_of("把优惠券接口换成新版"), "上下文"),
        Theme::DARK.state(SessionState::Waiting)
    );

    // 选中不在跑的那条：预览标题行是「对话预览 · 上下文 85%」。
    select_id(&mut app, "c-shop-2");
    load_preview(&mut app);
    let buf = render(&mut app, W, H);
    let head_text = row_text(&buf, app.areas.preview.y - 1);
    assert!(head_text.contains("上下文 85%"), "{head_text}");

    // 95% 起红：c-shop-3 换一份跟得上的读数。
    put(&mut app, "c-shop-3", 97.0, ago(60 * 30));
    let buf = render(&mut app, W, H);
    let text = dump(&buf);
    let y = text
        .lines()
        .position(|l| l.contains("结算页首屏加载慢") && l.contains("上下文 97%"))
        .expect("c-shop-3 有读数") as u16;
    assert_eq!(color_at(&buf, y, "上下文"), Theme::DARK.alert());
}
