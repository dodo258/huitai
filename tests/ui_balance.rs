//! `$` 额度面板和顶栏余额：按账号分组列额度、各 provider 余额，欠费标红，
//! 非公开接口标出来，写成命令的密钥列为跳过；pi 顶栏显示默认 provider 的余额。

mod ui_support;

use std::path::Path;
use std::time::Duration;

use huitai::core::{Account, QuotaValue, QuotaWindow, Vendor};
use huitai::quota::{
    Balance, BalanceReport, BalanceRow, BalanceValue, KeyOrigin, Money, OriginAccount, Provider,
    Skipped,
};
use huitai::ui::app::{Effect, Overlay};
use huitai::ui::{App, Theme};
use ratatui::crossterm::event::KeyCode;

use ui_support::*;

const W: u16 = 120;
const H: u16 = 34;

fn accounts() -> Vec<Account> {
    let home = Path::new(HOME);
    vec![
        Account::new(Vendor::Claude, "默认", home.join(".claude"), home),
        Account::new(Vendor::Codex, "默认", home.join(".codex"), home),
        Account::new(Vendor::Pi, "默认", home.join(".pi/agent"), home),
        Account::new(Vendor::Hermes, "默认", home.join(".hermes"), home),
    ]
}

fn from(vendor: Vendor, place: &str) -> KeyOrigin {
    KeyOrigin {
        account: Some(OriginAccount {
            vendor,
            key: String::new(),
            name: "默认".to_owned(),
        }),
        place: place.to_owned(),
    }
}

fn ok(provider: Provider, origin: KeyOrigin, value: BalanceValue) -> BalanceRow {
    BalanceRow {
        provider,
        origins: vec![origin],
        result: Ok(Balance {
            value,
            at: now() - Duration::from_secs(180),
        }),
    }
}

fn report() -> BalanceReport {
    let window = |label: &str, used, minutes| QuotaWindow {
        label: label.to_owned(),
        used_percent: used,
        window_minutes: Some(minutes),
        resets_at: None,
    };
    BalanceReport {
        rows: vec![
            ok(
                Provider::DeepSeek,
                from(Vendor::Pi, "auth.json · deepseek"),
                BalanceValue::Money {
                    amounts: vec![Money {
                        currency: "CNY".to_owned(),
                        amount: 36.42,
                    }],
                    available: true,
                },
            ),
            ok(
                Provider::KimiCode,
                from(Vendor::Pi, "models.json · kimi-coding"),
                BalanceValue::Windows(vec![window("5 小时", 83.0, 300), window("周", 31.0, 10080)]),
            ),
            ok(
                Provider::OpenRouter,
                from(Vendor::Hermes, ".env · OPENROUTER_API_KEY"),
                BalanceValue::Credit {
                    used: 7.5,
                    limit: Some(20.0),
                    remaining: Some(12.5),
                    currency: "USD".to_owned(),
                },
            ),
            BalanceRow {
                provider: Provider::MoonshotIntl,
                origins: vec![from(Vendor::Hermes, ".env · KIMI_API_KEY")],
                result: Err("HTTP 503".to_owned()),
            },
            ok(
                Provider::MoonshotCn,
                KeyOrigin {
                    account: None,
                    place: "环境变量 MOONSHOT_API_KEY".to_owned(),
                },
                BalanceValue::Money {
                    amounts: vec![Money {
                        currency: "CNY".to_owned(),
                        amount: -3.2,
                    }],
                    available: false,
                },
            ),
        ],
        skipped: vec![Skipped {
            origin: from(Vendor::Pi, "models.json · kimi-coding"),
            reason: "密钥写成了命令，会话台不执行".to_owned(),
        }],
        defaults: vec![
            ((Vendor::Pi, String::new()), Provider::DeepSeek),
            ((Vendor::Hermes, String::new()), Provider::MoonshotIntl),
        ],
    }
}

fn app_with_balance(vendor: Vendor) -> App {
    let mut app = demo_app(vendor);
    app.set_accounts(accounts());
    app.hide_missing(|v| {
        matches!(
            v,
            Vendor::Claude | Vendor::Codex | Vendor::Pi | Vendor::Hermes
        )
    });
    app.balance = Some(report());
    app
}

#[test]
fn dollar_opens_panel_and_asks_for_fresh_readings() {
    let mut app = app_with_balance(Vendor::Claude);
    assert_eq!(
        app.key(key(KeyCode::Char('$'))),
        vec![Effect::RefreshBalances]
    );
    assert_eq!(app.overlay, Overlay::Balance { scroll: 0 });
    app.key(key(KeyCode::Esc));
    assert_eq!(app.overlay, Overlay::None);
    // 中文输入法下 ⇧4 打出 ￥，一样打开、一样关。
    assert_eq!(
        app.key(key(KeyCode::Char('￥'))),
        vec![Effect::RefreshBalances]
    );
    app.key(key(KeyCode::Char('￥')));
    assert_eq!(app.overlay, Overlay::None);
}

#[test]
fn panel_lists_accounts_and_providers() {
    let mut app = app_with_balance(Vendor::Claude);
    app.key(key(KeyCode::Char('$')));
    let buf = render(&mut app, W, H);
    let text = dump(&buf);
    for want in [
        "额度和余额",
        "✻ Claude · 默认",
        "π pi · 默认",
        "今天 123.5 万 token",
        "¥36.42",
        "auth.json · deepseek",
        "非公开接口，可能失效",
        "剩 $12.50 / $20.00",
        "取不到：HTTP 503",
        "环境变量",
        "-¥3.20  欠费",
        "跳过 pi 默认 · models.json · kimi-coding：密钥写成了命令，会话台不执行",
    ] {
        assert!(text.contains(want), "缺「{want}」\n{text}");
    }
    assert_snapshot("balance", &text);

    // 欠费红色；Kimi 5 小时用了 83% 黄色进度条。
    let t = Theme::DARK;
    let (x, y) = find(&buf, "欠费");
    assert_eq!(buf[(x, y)].fg, t.alert());
    let (x, y) = find(&buf, "83%");
    assert_eq!(
        buf[(x + 1, y)].fg,
        t.state(huitai::core::SessionState::Waiting)
    );
}

/// 额度窗口有重置时间时面板写倒计时（账号额度和订阅额度都写），过了的不写；没有重置时间的见上面 `balance` 快照。
#[test]
fn panel_shows_reset_countdown() {
    let mut app = app_with_balance(Vendor::Claude);
    let at = |secs: i64| {
        Some(if secs >= 0 {
            now() + Duration::from_secs(secs as u64)
        } else {
            now() - Duration::from_secs(secs.unsigned_abs())
        })
    };
    let mut claude = quota_for(Vendor::Claude);
    if let QuotaValue::Windows(ws) = &mut claude.value {
        ws[0].resets_at = at(80 * 60);
        ws[1].resets_at = at(-60);
    }
    app.quota.insert((Vendor::Claude, String::new()), claude);
    let mut report = report();
    if let Ok(Balance {
        value: BalanceValue::Windows(ws),
        ..
    }) = &mut report.rows[1].result
    {
        ws[0].resets_at = at(45 * 60);
        ws[1].resets_at = at(2 * 86_400 + 3 * 3600);
    }
    app.balance = Some(report);
    app.key(key(KeyCode::Char('$')));
    // 132 列（常见终端宽）：倒计时、右边的来源和「非公开接口，可能失效」都放得下。
    let text = dump(&render(&mut app, 132, H));
    for want in [
        "23% · 1 小时 20 分后重置",
        "83% · 45 分钟后重置",
        "31% · 2 天 3 小时后重置",
        "非公开接口，可能失效",
        "状态栏 · 2 分钟前",
    ] {
        assert!(text.contains(want), "缺「{want}」\n{text}");
    }
    // Claude 7 天窗口的重置时间已过：读数过期，不写倒计时。
    assert!(!text.contains("41% ·"), "{text}");
    assert_snapshot("balance-reset", &text);

    // 窄一点放不下：Kimi Code 那行先让出最后一个倒计时，「非公开接口，可能失效」留着。
    let text = dump(&render(&mut app, W, H));
    let kimi = text.lines().find(|l| l.contains("Kimi Code")).unwrap();
    assert!(kimi.contains("非公开接口，可能失效"), "{kimi}");
    assert!(kimi.contains("83% · 45 分钟后重置"), "{kimi}");
    assert!(!kimi.contains("31% ·"), "{kimi}");
}

/// 面板打开时，顶栏和底栏之间只有面板：背后列表的行号、箭头和预览的字不从旁边、上下漏出来；关掉就回来。
#[test]
fn panel_hides_list_and_preview_behind_it() {
    for (theme, w, h) in [
        (Theme::DARK, W, H),
        (Theme::DARK, 132, H),
        (Theme::LIGHT, 132, H),
        (Theme::DARK, 45, 24),
        (Theme::DARK, 220, 60),
    ] {
        let mut app = app_with_balance(Vendor::Claude);
        let before = dump(&render_with(&mut app, w, h, &theme));
        app.key(key(KeyCode::Char('$')));
        let buf = render_with(&mut app, w, h, &theme);
        let (_, title_y) = find(&buf, "额度和余额");
        let (_, hint_y) = find(&buf, "Esc 关闭");
        let (left, rows) = ((w - 124.min(w - 2)) / 2, title_y - 1..hint_y + 2);
        let cols = left..w - left;
        for y in 1..h - 1 {
            for x in 0..w {
                if rows.contains(&y) && cols.contains(&x) {
                    continue;
                }
                let c = &buf[(x, y)];
                assert_eq!(
                    (c.symbol(), c.bg),
                    (" ", theme.base),
                    "面板外 ({x},{y})，{w}×{h}\n{}",
                    dump(&buf)
                );
            }
        }
        app.key(key(KeyCode::Esc));
        assert_eq!(dump(&render_with(&mut app, w, h, &theme)), before);
    }
}

#[test]
fn panel_without_keys_says_where_it_looked() {
    let mut app = demo_app(Vendor::Claude);
    app.set_accounts(accounts());
    app.balance = Some(BalanceReport::default());
    app.key(key(KeyCode::Char('$')));
    let text = dump(&render(&mut app, W, H));
    assert!(
        text.contains("没找到 DeepSeek、Moonshot、OpenRouter、Kimi Code 的密钥"),
        "{text}"
    );
    assert!(text.contains("只读 pi 的 auth.json、models.json，Hermes 的 .env 和环境变量"));
}

#[test]
fn panel_scrolls_when_short() {
    let mut app = app_with_balance(Vendor::Claude);
    app.key(key(KeyCode::Char('$')));
    let text = dump(&render(&mut app, W, 16));
    assert!(text.contains("↑↓ 翻"), "{text}");
    assert!(text.contains("✻ Claude · 默认"));
    for _ in 0..40 {
        app.key(key(KeyCode::Down));
    }
    let text = dump(&render(&mut app, W, 16));
    assert!(text.contains("跳过 pi"), "翻到底：\n{text}");
    assert!(!text.contains("✻ Claude · 默认"));
}

#[test]
fn pi_top_bar_shows_default_provider_balance() {
    let mut app = app_with_balance(Vendor::Pi);
    // 宽一点：八家同屏时来源让给别家短名，120 列只写得下余额。
    let top = dump(&render(&mut app, 160, H))
        .lines()
        .next()
        .unwrap()
        .to_owned();
    assert!(top.contains("¥36.42"), "{top}");
    assert!(top.contains("DeepSeek · 3 分钟前"), "{top}");
    assert!(!top.contains("token"), "{top}");
    // 默认 provider 取不到（Hermes 默认 Moonshot 国际，HTTP 503）：顶栏不写额度。
    let mut app = app_with_balance(Vendor::Hermes);
    let top = dump(&render(&mut app, W, H))
        .lines()
        .next()
        .unwrap()
        .to_owned();
    assert!(!top.contains("额度"), "{top}");
    // 还没读到余额：pi 照旧显示本机统计。
    let mut app = app_with_balance(Vendor::Pi);
    app.balance = None;
    let top = dump(&render(&mut app, W, H))
        .lines()
        .next()
        .unwrap()
        .to_owned();
    assert!(top.contains("token"), "{top}");
}

/// `needle` 第一次出现的格子位置（按显示宽度算列）。
fn find(buf: &ratatui::buffer::Buffer, needle: &str) -> (u16, u16) {
    for (y, line) in dump(buf).lines().enumerate() {
        if let Some(i) = line.find(needle) {
            return (
                unicode_width::UnicodeWidthStr::width(&line[..i]) as u16,
                y as u16,
            );
        }
    }
    panic!("没找到 {needle}");
}

fn credit(used: f64, limit: Option<f64>, remaining: Option<f64>) -> BalanceValue {
    BalanceValue::Credit {
        used,
        limit,
        remaining,
        currency: "USD".to_owned(),
    }
}

/// 顶栏的字（拼起来）和进度条实心格数。
fn top_of(v: BalanceValue) -> (String, usize) {
    let row = ok(
        Provider::OpenRouter,
        from(Vendor::Pi, "auth.json · openrouter"),
        v,
    );
    let b = row.result.clone().unwrap();
    let spans =
        huitai::ui::balance::top_spans(&row, &b, now(), Vendor::Pi, &Theme::DARK, false, false);
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    let filled = spans
        .iter()
        .skip_while(|s| !s.content.contains("限额"))
        .nth(1)
        .map_or(0, |s| s.content.chars().count());
    (text, filled)
}

/// OpenRouter 的 `usage` 是累计用量：累计 $50、日限额 $10、本期还剩 $9 时这一期只用了 $1，不是满格；
/// 面板、顶栏和 `ai doctor` 算的是同一个数。
#[test]
fn openrouter_period_used_is_limit_minus_remaining() {
    let v = credit(50.0, Some(10.0), Some(9.0));
    assert_eq!(huitai::ui::balance::plain(&v), "已用 $1.00 / $10.00");
    let (text, filled) = top_of(v);
    assert!(text.contains("剩 $9.00 / $10.00"), "{text}");
    assert_eq!(filled, 1, "1/10 只占 1 格：{text}");
}

/// 限额 0 是用完了，不是「不限额」；没设限额（空）才是不限额。
#[test]
fn openrouter_zero_limit_is_used_up_not_unlimited() {
    let v = credit(3.0, Some(0.0), Some(0.0));
    let plain = huitai::ui::balance::plain(&v);
    assert!(
        plain.contains("用完了") && !plain.contains("不限额"),
        "{plain}"
    );
    let (text, filled) = top_of(v);
    assert!(!text.contains("不限额"), "{text}");
    assert_eq!(filled, 8, "满格：{text}");

    let unlimited = credit(3.0, None, None);
    assert!(huitai::ui::balance::plain(&unlimited).contains("不限额"));
}
