//! ui 测试共用：手写的假对话（不读真实数据）、渲染到 TestBackend、快照比对。

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use huitai::core::{
    Config, QuotaReading, QuotaValue, QuotaWindow, Session, SessionId, SessionState, Vendor,
};
use huitai::index::{Entry, Note};
use huitai::sources::{PreviewMessage, Role};
use huitai::ui::{App, Preview, Theme, view};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthStr;

pub const HOME: &str = "/Users/demo";

/// 固定的「现在」：2026-11-20 12:00 UTC。
pub fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_795_176_000)
}

fn mins_ago(m: u64) -> SystemTime {
    now() - Duration::from_secs(m * 60)
}

pub struct Fake {
    pub id: &'static str,
    pub cwd: &'static str,
    pub title: &'static str,
    pub mins: u64,
    pub messages: usize,
    pub branch: Option<&'static str>,
    pub pinned: bool,
    pub archived: bool,
    /// 自动运行（claude -p 之类）起的。
    pub automatic: bool,
}

const fn f(
    id: &'static str,
    cwd: &'static str,
    title: &'static str,
    mins: u64,
    messages: usize,
) -> Fake {
    Fake {
        id,
        cwd,
        title,
        mins,
        messages,
        branch: None,
        pinned: false,
        archived: false,
        automatic: false,
    }
}

pub fn entry(vendor: Vendor, x: &Fake) -> Entry {
    Entry {
        session: Session {
            vendor,
            id: SessionId::from(x.id),
            cwd: PathBuf::from(x.cwd),
            path: PathBuf::from(format!("{}/{}.jsonl", x.cwd, x.id)),
            title: Some(x.title.to_owned()),
            git_branch: x.branch.map(str::to_owned),
            last_active: mins_ago(x.mins),
            message_count: x.messages,
            automatic: x.automatic,
        },
        note: Note {
            title: None,
            pinned: x.pinned,
            archived: x.archived,
        },
    }
}

/// 按 index 的顺序排：置顶在前，其余新的在前。
fn sorted(vendor: Vendor, fakes: &[Fake]) -> Vec<Entry> {
    let mut v: Vec<Entry> = fakes.iter().map(|x| entry(vendor, x)).collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.session.last_active));
    v.sort_by_key(|e| !e.note.pinned);
    v
}

pub fn claude_entries() -> Vec<Entry> {
    let mut list = vec![
        Fake {
            branch: Some("fix/checkout"),
            ..f(
                "c-shop-1",
                "/Users/demo/projects/shop",
                "购物车结算按钮点了没反应",
                3,
                42,
            )
        },
        f(
            "c-shop-2",
            "/Users/demo/projects/shop",
            "把优惠券接口换成新版",
            26,
            18,
        ),
        f(
            "c-shop-3",
            "/Users/demo/projects/shop",
            "结算页首屏加载慢，查一下原因",
            60 * 30,
            57,
        ),
        Fake {
            pinned: true,
            ..f(
                "c-notes-1",
                "/Users/demo/notes",
                "周报模板和每周复盘",
                60 * 50,
                9,
            )
        },
        f(
            "c-blog-1",
            "/Users/demo/projects/blog",
            "给博客加 RSS 订阅",
            95,
            23,
        ),
        f(
            "c-blog-2",
            "/Users/demo/projects/blog",
            "文章页代码块不换行",
            60 * 24 * 4,
            11,
        ),
        f(
            "c-api-1",
            "/Users/demo/projects/api",
            "接口加分页和游标",
            60 * 7,
            31,
        ),
        f(
            "c-api-2",
            "/Users/demo/work/api",
            "订单服务超时排查",
            60 * 24 * 2,
            64,
        ),
        f(
            "c-wt-1",
            "/Users/demo/projects/shop/.claude/worktrees/1120-cart",
            "后台任务：购物车重构",
            40,
            120,
        ),
        f(
            "c-home-1",
            "/Users/demo",
            "mac 上 brew 装的 python 找不到",
            60 * 24 * 9,
            6,
        ),
        Fake {
            archived: true,
            ..f(
                "c-old-1",
                "/Users/demo/projects/shop",
                "旧版支付接入",
                60 * 24 * 60,
                88,
            )
        },
    ];
    list.truncate(11);
    // 后台任务（claude -p）起的：shop 里一条正在跑、一条早跑完了，blog 一条；ops 只有后台对话，整组不显示。
    let auto = |x: Fake| Fake {
        automatic: true,
        ..x
    };
    list.extend([
        auto(f(
            "c-auto-1",
            "/Users/demo/projects/shop",
            "按待办做：结算页骨架屏",
            1,
            3,
        )),
        auto(f(
            "c-auto-2",
            "/Users/demo/projects/shop",
            "门检查：跑测试和 clippy",
            60 * 6,
            2,
        )),
        auto(f(
            "c-auto-3",
            "/Users/demo/projects/blog",
            "夜班复盘",
            60 * 20,
            4,
        )),
        auto(f(
            "c-auto-4",
            "/Users/demo/projects/ops",
            "每天备份检查",
            60 * 3,
            1,
        )),
    ]);
    sorted(Vendor::Claude, &list)
}

pub fn codex_entries() -> Vec<Entry> {
    sorted(
        Vendor::Codex,
        &[
            Fake {
                branch: Some("main"),
                ..f(
                    "x-api-1",
                    "/Users/demo/projects/api",
                    "给订单列表接口补单元测试",
                    8,
                    27,
                )
            },
            f(
                "x-api-2",
                "/Users/demo/projects/api",
                "把 SQL 查询改成参数化",
                60 * 5,
                15,
            ),
            f(
                "x-cli-1",
                "/Users/demo/projects/ledger-cli",
                "导出 CSV 时金额精度丢了",
                60 * 26,
                33,
            ),
        ],
    )
}

pub fn pi_entries() -> Vec<Entry> {
    sorted(
        Vendor::Pi,
        &[
            f("p-blog-1", "/Users/demo/projects/blog", "RSS 初稿", 12, 8),
            f(
                "p-blog-2",
                "/Users/demo/projects/blog",
                "首页加阅读时长",
                60 * 3,
                5,
            ),
        ],
    )
}

pub fn hermes_entries() -> Vec<Entry> {
    sorted(
        Vendor::Hermes,
        &[
            Fake {
                branch: Some("main"),
                ..f(
                    "20261120_101500_a1b2c3",
                    "/Users/demo/projects/api",
                    "整理接口文档的目录结构",
                    60 * 30,
                    6,
                )
            },
            f(
                "20261118_090000_d4e5f6",
                "/Users/demo/projects/blog",
                "把草稿里的图片压一遍",
                60 * 24 * 3,
                3,
            ),
        ],
    )
}

pub fn kimi_entries() -> Vec<Entry> {
    sorted(
        Vendor::Kimi,
        &[
            f(
                "session_6b1f0c2e-4d3a-4b5c-9e8f-7a6b5c4d3e2f",
                "/Users/demo/projects/blog",
                "首页图片改成懒加载",
                20,
                4,
            ),
            f(
                "session_0a1b2c3d-1111-4222-8333-944455566677",
                "/Users/demo/projects/shop",
                "商品详情页加骨架屏",
                60 * 8,
                7,
            ),
        ],
    )
}

pub fn gemini_entries() -> Vec<Entry> {
    sorted(
        Vendor::Gemini,
        &[
            f(
                "1a2b3c4d-1111-4aaa-8bbb-000000000001",
                "/Users/demo/projects/shop",
                "修结算按钮",
                45,
                4,
            ),
            f(
                "2b3c4d5e-2222-4aaa-8bbb-000000000002",
                "/Users/demo/projects/blog",
                "看 README 的部署步骤对不对",
                60 * 26,
                2,
            ),
        ],
    )
}

pub fn opencode_entries() -> Vec<Entry> {
    sorted(
        Vendor::OpenCode,
        &[
            f(
                "ses_e113ed23fffeV2onlyAbCdEfGh",
                "/Users/demo/projects/shop",
                "给登录页加验证码",
                60 * 2,
                4,
            ),
            f(
                "ses_e11a99e17ffeV1fallbackAbCd",
                "/Users/demo/projects/blog",
                "评论换成自建的",
                60 * 24 * 2,
                2,
            ),
        ],
    )
}

pub fn qwen_entries() -> Vec<Entry> {
    sorted(
        Vendor::Qwen,
        &[
            f(
                "5e1c0a2b-1111-4aaa-8bbb-000000000001",
                "/Users/demo/projects/shop",
                "购物车数量改成可以直接输入",
                60 * 5,
                3,
            ),
            f(
                "5e1c0a2b-7777-4aaa-8bbb-000000000007",
                "/Users/demo/projects/blog",
                "文章列表分页",
                60 * 30,
                2,
            ),
        ],
    )
}

pub fn preview_for(vendor: Vendor) -> Preview {
    let m = |role, text: &str, mins| PreviewMessage {
        role,
        text: text.to_owned(),
        at: Some(mins_ago(mins)),
    };
    let messages = match vendor {
        Vendor::Claude => vec![
            m(Role::User, "购物车结算按钮点了没反应，帮我查一下", 9),
            m(
                Role::Assistant,
                "找到了：结算按钮的 onClick 在优惠券还没算完时被禁用，算完后没有重新启用。\n我改了 useCheckout 里的状态更新，并补了一个测试。",
                7,
            ),
            m(Role::User, "好，顺便看看移动端是不是一样", 4),
            m(
                Role::Assistant,
                "移动端走的是同一个组件，已经一起修好。正在跑 e2e 测试……",
                3,
            ),
        ],
        Vendor::Codex => vec![
            m(
                Role::User,
                "给订单列表接口补单元测试，覆盖空列表和分页边界",
                10,
            ),
            m(
                Role::Assistant,
                "已加 6 个测试，全部通过。分页最后一页少一条的问题顺手修了。",
                8,
            ),
        ],
        Vendor::Pi => vec![
            m(Role::User, "给博客加一个 RSS", 14),
            m(
                Role::Assistant,
                "RSS 已生成在 /feed.xml，包含最近 20 篇文章。",
                12,
            ),
        ],
        Vendor::Hermes => vec![
            m(Role::User, "接口文档按模块分目录，旧链接要能跳过去", 32),
            m(
                Role::Assistant,
                "已按订单、用户、支付三块分好目录，旧路径都加了重定向。",
                30,
            ),
        ],
        Vendor::Kimi => vec![
            m(Role::User, "首页图片改成懒加载", 20),
            m(Role::Assistant, "首屏以下的图片都加了 loading=lazy。", 18),
        ],
        Vendor::Gemini => vec![
            m(Role::User, "结算按钮点了没反应", 45),
            m(
                Role::Assistant,
                "优惠券算完后没有重新启用按钮，已修好。",
                43,
            ),
        ],
        Vendor::Qwen => vec![
            m(Role::User, "购物车数量改成可以直接输入", 310),
            m(
                Role::Assistant,
                "数量框改成输入框了，输入 0 会删掉这件商品。",
                300,
            ),
        ],
        Vendor::OpenCode => vec![
            m(Role::User, "给登录页加验证码", 125),
            m(
                Role::Assistant,
                "验证码加好了，用的滑块；改了 src/login.tsx。",
                120,
            ),
        ],
    };
    Preview {
        at: SystemTime::UNIX_EPOCH,
        messages: Ok(messages),
    }
}

pub fn quota_for(vendor: Vendor) -> QuotaReading {
    let w = |label: &str, used, mins| QuotaWindow {
        label: label.to_owned(),
        used_percent: used,
        window_minutes: Some(mins),
        resets_at: None,
    };
    let (value, source) = match vendor {
        Vendor::Claude => (
            QuotaValue::Windows(vec![w("5 小时", 23.0, 300), w("7 天", 41.0, 10080)]),
            "状态栏",
        ),
        Vendor::Codex => (QuotaValue::Windows(vec![w("7 天", 9.0, 10080)]), "rollout"),
        Vendor::Pi => (QuotaValue::TokensToday(1_234_567), "本机统计"),
        // Hermes、Kimi Code、Gemini、OpenCode、Qwen Code 还没有额度渠道，顶栏不写额度。
        Vendor::Hermes | Vendor::Kimi | Vendor::Gemini | Vendor::OpenCode | Vendor::Qwen => {
            (QuotaValue::Unavailable, "")
        }
    };
    QuotaReading {
        value,
        source: source.to_owned(),
        at: now() - Duration::from_secs(120),
    }
}

/// 各家都有数据的会话台，停在 `vendor`。
pub fn demo_app(vendor: Vendor) -> App {
    let mut app = App::new(
        PathBuf::from(HOME),
        PathBuf::from("/Users/demo/projects/shop"),
        Config::default(),
    );
    app.now = now();
    app.set_entries(Vendor::Claude, claude_entries());
    app.set_entries(Vendor::Codex, codex_entries());
    app.set_entries(Vendor::Pi, pi_entries());
    app.set_entries(Vendor::Hermes, hermes_entries());
    app.set_entries(Vendor::Kimi, kimi_entries());
    app.set_entries(Vendor::Gemini, gemini_entries());
    app.set_entries(Vendor::OpenCode, opencode_entries());
    app.set_entries(Vendor::Qwen, qwen_entries());
    for v in Vendor::ALL {
        app.quota.insert(v, quota_for(v));
    }
    let states = [
        ("c-shop-1", SessionState::Running),
        ("c-shop-2", SessionState::Done),
        ("c-blog-1", SessionState::Waiting),
        ("c-wt-1", SessionState::Running),
        ("x-api-1", SessionState::Running),
        ("p-blog-1", SessionState::Done),
    ];
    for (id, s) in states {
        app.states.insert(SessionId::from(id), s);
    }
    app.attached.insert(SessionId::from("c-shop-1"));
    while app.vendor != vendor {
        app.key(key(KeyCode::Tab));
    }
    // Claude 停在一条在跑、已开着标签页的对话上，预览内容最全。
    if vendor == Vendor::Claude {
        select_id(&mut app, "c-shop-1");
    }
    load_preview(&mut app);
    app
}

/// 往下按到选中这条为止。
/// 当 Kimi Code、Gemini、OpenCode、Qwen Code 都没装（没有对话、数据目录不在），顶栏回到四家：测「不到五家时」的顶栏行为用。
pub fn only_first_four(app: &mut App) {
    app.set_entries(Vendor::Kimi, Vec::new());
    app.set_entries(Vendor::Gemini, Vec::new());
    app.set_entries(Vendor::OpenCode, Vec::new());
    app.set_entries(Vendor::Qwen, Vec::new());
    app.hide_missing(|v| {
        !matches!(
            v,
            Vendor::Kimi | Vendor::Gemini | Vendor::OpenCode | Vendor::Qwen
        )
    });
}

pub fn select_id(app: &mut App, id: &str) {
    for _ in 0..100 {
        if app
            .selected_entry()
            .is_some_and(|e| e.session.id.as_str() == id)
        {
            return;
        }
        app.key(key(KeyCode::Down));
    }
    panic!("列表里没有 {id}");
}

/// 把选中那条的预览「读好」。
pub fn load_preview(app: &mut App) {
    if let Some(e) = app.take_preview_request() {
        let mut p = preview_for(e.session.vendor);
        p.at = e.session.last_active;
        app.set_preview(e.session.id.clone(), p);
    }
}

pub fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub fn render(app: &mut App, w: u16, h: u16) -> Buffer {
    render_with(app, w, h, &Theme::DARK)
}

pub fn render_with(app: &mut App, w: u16, h: u16, theme: &Theme) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| view::draw(f, app, theme)).unwrap();
    term.backend().buffer().clone()
}

/// 缓冲区转成文字，一行一行；宽字符后面的占位格跳过。
pub fn dump(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in 0..area.height {
        let mut line = String::new();
        let mut x = 0;
        while x < area.width {
            let s = buf[(x, y)].symbol();
            line.push_str(s);
            x += UnicodeWidthStr::width(s).max(1) as u16;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// 和 tests/ui_snapshots/<name>.txt 比；`UPDATE_SNAPSHOTS=1` 时重写。
pub fn assert_snapshot(name: &str, text: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/ui_snapshots")
        .join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        return;
    }
    let want = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("没有快照 {}，用 UPDATE_SNAPSHOTS=1 生成", path.display()));
    assert!(
        want == text,
        "快照 {name} 不一致\n---- 期望 ----\n{want}\n---- 实际 ----\n{text}"
    );
}

/// 假 CLI 此刻的屏幕（手写的字节喂给真的终端状态机），给实时预览用。比预览区宽，测截断。
pub fn live_screen() -> huitai::host::ScreenSnapshot {
    let mut s = huitai::host::Screen::new(90, 14);
    s.feed(
        "\x1b[38;2;217;119;87m✻\x1b[0m \x1b[1mClaude Code\x1b[0m  ~/projects/shop\r\n\
         \r\n\
         \x1b[2m>\x1b[0m 购物车结算按钮点了没反应，帮我查一下\r\n\
         \r\n\
         ⏺ 正在跑 e2e 测试 (npm run test:e2e -- --project=chromium --reporter=line --workers=4)\r\n\
         \x20 ⎿  \x1b[32m✓\x1b[0m checkout.spec.ts (12 tests) 4.2s\r\n\
         \x20    \x1b[32m✓\x1b[0m coupon.spec.ts (8 tests) 2.9s\r\n\
         \x20    \x1b[7m RUN \x1b[0m mobile.spec.ts\r\n\
         \r\n\
         \x1b[38;5;208m✶ Running…\x1b[0m \x1b[2m(esc to interrupt)\x1b[0m\r\n"
            .as_bytes(),
    );
    s.snapshot()
}
