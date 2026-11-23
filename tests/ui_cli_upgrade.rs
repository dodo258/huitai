//! 顶栏「更新」按钮：有能一键升的各家 CLI 才出现、带底色；按 U 或点一下开始升，升级中显示进度，
//! 全成功后自己消失；有没升上的变成「N 家没升上」，点开看原文，U 再试。自动升级的结果只进状态栏。

mod ui_support;

use std::time::Instant;

use huitai::core::Vendor;
use huitai::ui::app::{CliReport, CliResult, CliUpgrade, Overlay};
use huitai::ui::theme::UpdateButton;
use huitai::ui::{App, Effect, Theme};
use ratatui::crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use ui_support::*;

const W: u16 = 120;
const H: u16 = 30;

fn top(app: &mut App) -> String {
    dump(&render(app, W, H)).lines().next().unwrap().to_owned()
}

fn report(newer: &[(Vendor, &str)], ready: &[Vendor]) -> CliReport {
    CliReport {
        newer: newer.iter().map(|(v, s)| (*v, (*s).to_owned())).collect(),
        ready: ready.to_vec(),
    }
}

fn result(vendor: Vendor, ok: bool, text: &str) -> CliResult {
    CliResult {
        vendor,
        ok,
        text: text.to_owned(),
    }
}

/// 有能一键升的才有按钮；只有新版标记（比如自带更新的还没满 24 小时、要手动跑的）不出按钮。
#[test]
fn button_only_when_something_can_be_upgraded() {
    let mut app = demo_app(Vendor::Claude);
    assert!(!top(&mut app).contains("更新"));
    app.set_cli_report(report(&[(Vendor::Claude, "2.1.340")], &[]));
    let t = top(&mut app);
    assert!(!t.contains(" 更新 "), "只标 ↑、不出按钮：{t}");
    assert_eq!(app.areas.update.width, 0);

    app.set_cli_report(report(
        &[(Vendor::Claude, "2.1.340"), (Vendor::Codex, "0.172.0")],
        &[Vendor::Codex],
    ));
    let buf = render(&mut app, W, H);
    let t = dump(&buf).lines().next().unwrap().to_owned();
    assert!(t.contains(" 更新 "), "{t}");
    // 按钮带底色（走 theme），在额度前面。
    let r = app.areas.update;
    assert_eq!(r.width, 6);
    assert_eq!(buf[(r.x + 1, 0)].symbol(), "更");
    assert_eq!(
        buf[(r.x + 1, 0)].bg,
        Theme::DARK.update_button(UpdateButton::Ready)
    );
    assert!(t.find("更新").unwrap() < t.find('%').unwrap(), "{t}");

    // 关了检查新版本：不显示。
    app.config.check_updates = false;
    assert!(!top(&mut app).contains(" 更新 "));
}

/// U：开始升级，按钮变成进度；全都成了按钮消失、标记去掉、状态栏说一句。
#[test]
fn press_u_upgrades_and_button_goes_away() {
    let mut app = demo_app(Vendor::Claude);
    app.set_cli_report(report(
        &[(Vendor::Codex, "0.172.0"), (Vendor::Pi, "1.6.1")],
        &[Vendor::Codex, Vendor::Pi],
    ));
    assert_eq!(app.key(key(KeyCode::Char('U'))), vec![Effect::UpgradeClis]);
    assert!(top(&mut app).contains(" 更新中 0/2 "));
    assert_eq!(
        render(&mut app, W, H)[(app.areas.update.x + 1, 0)].bg,
        Theme::DARK.update_button(UpdateButton::Running)
    );
    // 升级中再按：不重复开始。
    assert!(app.key(key(KeyCode::Char('U'))).is_empty());
    assert_eq!(app.status.as_deref(), Some("正在升级，等它跑完"));
    app.cli_progress(1, 2);
    assert!(top(&mut app).contains(" 更新中 1/2 "));

    app.cli_upgrade_done(vec![
        result(Vendor::Codex, true, "已升到 0.172.0"),
        result(Vendor::Pi, true, "已升到 1.6.1"),
    ]);
    assert_eq!(app.cli_upgrade, CliUpgrade::Idle);
    let t = top(&mut app);
    assert!(!t.contains("更新"), "全成功按钮消失：{t}");
    assert!(!t.contains('↑'), "新版标记也去掉：{t}");
    assert_eq!(
        app.status.as_deref(),
        Some("已升级：Codex 0.172.0、pi 1.6.1")
    );
    // 没有能升的时候按 U 只说一句。
    assert!(app.key(key(KeyCode::Char('U'))).is_empty());
}

/// 点按钮等同 U。
#[test]
fn click_the_button() {
    let mut app = demo_app(Vendor::Claude);
    app.set_cli_report(report(&[(Vendor::Codex, "0.172.0")], &[Vendor::Codex]));
    render(&mut app, W, H);
    let r = app.areas.update;
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: r.x + 2,
        row: 0,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(app.mouse(click, Instant::now()), vec![Effect::UpgradeClis]);
    assert!(matches!(
        app.cli_upgrade,
        CliUpgrade::Running { done: 0, total: 1 }
    ));
}

/// 有没升上的：按钮变成「1 家没升上」（红底），升上的那家标记去掉；点开看原文（快照），U 再试，Esc 关掉按钮还在。
#[test]
fn failures_stay_and_open_the_raw_output() {
    let mut app = demo_app(Vendor::Claude);
    app.set_cli_report(report(
        &[(Vendor::Codex, "0.172.0"), (Vendor::Kimi, "2.7.2")],
        &[Vendor::Codex, Vendor::Kimi],
    ));
    app.key(key(KeyCode::Char('U')));
    app.cli_upgrade_done(vec![
        result(Vendor::Codex, true, "已升到 0.172.0"),
        result(
            Vendor::Kimi,
            false,
            "没升上：npm install -g @moonshot-ai/kimi-code@latest 退出码 243：\n\
             npm error code EACCES\n\
             npm error syscall rename\n\
             npm error path /usr/local/lib/node_modules/@moonshot-ai/kimi-code",
        ),
    ]);
    let buf = render(&mut app, W, H);
    let t = dump(&buf).lines().next().unwrap().to_owned();
    assert!(t.contains(" 1 家没升上 "), "{t}");
    assert_eq!(
        buf[(app.areas.update.x + 1, 0)].bg,
        Theme::DARK.update_button(UpdateButton::Failed)
    );
    assert_eq!(
        app.status.as_deref(),
        Some("1 家没升上，按 U 或点顶栏看原文")
    );
    assert_eq!(app.cli_ready, vec![Vendor::Kimi]);

    // 点开看原文。
    assert!(app.key(key(KeyCode::Char('U'))).is_empty());
    assert_eq!(app.overlay, Overlay::CliFailures { scroll: 0 });
    let text = dump(&render(&mut app, W, H));
    assert!(text.contains("npm error code EACCES"), "{text}");
    assert!(text.contains("npm install -g @moonshot-ai/kimi-code@latest 退出码 243"));
    assert_snapshot("cli-failures", &text);

    // Esc 关掉，按钮还在；再点开按 U 再试一次。
    assert!(app.key(key(KeyCode::Esc)).is_empty());
    assert_eq!(app.overlay, Overlay::None);
    assert!(top(&mut app).contains(" 1 家没升上 "));
    app.key(key(KeyCode::Char('U')));
    assert_eq!(app.key(key(KeyCode::Char('U'))), vec![Effect::UpgradeClis]);
    assert!(matches!(
        app.cli_upgrade,
        CliUpgrade::Running { done: 0, total: 1 }
    ));
}

/// 没升上的那家主人自己升好了（下一轮查到已不是旧版）：从没升上的里去掉，都没了按钮就消失。
#[test]
fn failure_clears_when_fixed_elsewhere() {
    let mut app = demo_app(Vendor::Claude);
    app.set_cli_report(report(&[(Vendor::Kimi, "2.7.2")], &[Vendor::Kimi]));
    app.key(key(KeyCode::Char('U')));
    app.cli_upgrade_done(vec![result(Vendor::Kimi, false, "没升上：x")]);
    assert!(matches!(app.cli_upgrade, CliUpgrade::Failed(_)));
    app.set_cli_report(report(&[], &[]));
    assert_eq!(app.cli_upgrade, CliUpgrade::Idle);
    assert!(!top(&mut app).contains("没升上"));
}

/// 自动升级的结果只在状态栏说一句，不动按钮；成了的标记去掉。
#[test]
fn auto_results_go_to_the_status_line() {
    let mut app = demo_app(Vendor::Claude);
    app.set_cli_report(report(
        &[(Vendor::Codex, "0.172.0"), (Vendor::Pi, "1.6.1")],
        &[Vendor::Codex, Vendor::Pi],
    ));
    app.cli_auto_done(vec![
        result(Vendor::Codex, true, "已升到 0.172.0"),
        result(Vendor::Pi, false, "没升上：x"),
    ]);
    assert_eq!(
        app.status.as_deref(),
        Some("已自动升级：Codex 0.172.0；pi 自动升级没成（ai doctor 看原文，按 U 再试）")
    );
    assert_eq!(app.cli_upgrade, CliUpgrade::Idle);
    assert_eq!(app.cli_ready, vec![Vendor::Pi]);
    assert!(top(&mut app).contains(" 更新 "));
}

/// 窄终端：按钮比额度优先留着。
#[test]
fn narrow_terminal_keeps_the_button() {
    let mut app = demo_app(Vendor::Claude);
    app.set_cli_report(report(&[(Vendor::Codex, "0.172.0")], &[Vendor::Codex]));
    let t = dump(&render(&mut app, 80, H))
        .lines()
        .next()
        .unwrap()
        .to_owned();
    assert!(t.ends_with(" 更新"), "{t}");
    assert_eq!(app.areas.update.width, 6);
}
