//! 撞用量上限：从会话文件最后一条认出来（`Moment::limit`），解析重置时间。
//! 样例都是手写的（tests/fixtures/limits），照 Claude Code、Codex 实际写的记录格式。

mod sources_common;

use std::fs;
use std::path::PathBuf;

use huitai::core::{Session, SessionId, Vendor};
use huitai::sources::{ClaudeSource, CodexSource, LimitHit, Source, Trash, limit};
use sources_common::{at, fixture, temp_dir};

fn session(vendor: Vendor, path: PathBuf) -> Session {
    Session {
        vendor,
        id: SessionId::from("s1"),
        cwd: PathBuf::from("/Users/demo/projects/a"),
        path,
        title: None,
        git_branch: None,
        last_active: at(1_795_401_000),
        message_count: 1,
        automatic: false,
        account: String::new(),
    }
}

fn claude(name: &str) -> Option<LimitHit> {
    let base = temp_dir(&format!("limit-claude-{name}"));
    let path = base.join("projects/-Users-demo-projects-a/s1.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::copy(fixture(&format!("limits/claude/{name}.jsonl")), &path).unwrap();
    ClaudeSource::new(base.join("projects"), Trash::new(base.join("trash")))
        .load_moment(&session(Vendor::Claude, path))
        .unwrap()
        .limit
}

fn codex(name: &str) -> Option<LimitHit> {
    let base = temp_dir(&format!("limit-codex-{name}"));
    let root = base.join("codex");
    let path = root.join("sessions/2026/11/23/rollout-2026-11-23T10-00-00-s1.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::copy(fixture(&format!("limits/codex/{name}.jsonl")), &path).unwrap();
    CodexSource::new(&root, Trash::new(base.join("trash")))
        .load_moment(&session(Vendor::Codex, path))
        .unwrap()
        .limit
}

fn resume(secs: u64) -> Option<LimitHit> {
    Some(LimitHit {
        resume_at: Some(at(secs)),
    })
}

/// 记录写下的时刻（样例里上限记录都是 2026-11-23T02:30:00Z，session-limit 是 03:10）。
const WRITTEN: u64 = 1_795_401_000;

#[test]
fn claude_limit_messages() {
    // 「resets 1pm (Asia/Shanghai)」：11:10 写下 → 当天 13:00 北京时间（05:00Z）再过 1 分钟；后面的 last-prompt 这类行不算消息。
    assert_eq!(claude("session-limit"), resume(1_795_410_060));
    // 5 小时窗口，新加坡时区。
    assert_eq!(claude("five-hour-limit"), resume(1_795_403_460));
    // 周额度带日期：「resets Nov 25 at 9pm (Asia/Shanghai)」。
    assert_eq!(claude("weekly-limit"), resume(1_795_611_660));
    // 旧版「Claude AI usage limit reached|<unix 秒>」。
    assert_eq!(claude("legacy-epoch"), resume(1_795_928_460));
}

#[test]
fn claude_limit_without_reset_time_is_a_hit_but_not_resumable() {
    assert_eq!(claude("no-reset"), Some(LimitHit { resume_at: None }));
}

#[test]
fn claude_not_a_limit() {
    for name in [
        "limit-then-user",    // 上限之后用户又说了话：最后一条不是它
        "not-limit",          // 别的 API 错误（没登录）
        "sidechain-limit",    // 子代理撞的不算
        "talks-about-limits", // 正文里提到 rate limit，不是错误消息
    ] {
        assert_eq!(claude(name), None, "{name}");
    }
}

#[test]
fn codex_limit_turns() {
    let written = at(WRITTEN);
    let tz = limit::local_tz();
    // 钟点按本机时区写，期望值用同一个时区算；要算得出来。
    let clock = limit::codex_resume_at("try again at 3:05 PM.", written, &tz);
    assert!(clock.is_some());
    assert_eq!(codex("clock"), Some(LimitHit { resume_at: clock }));
    let dated = limit::codex_resume_at("Try again at Nov 25th, 2026 9:00 AM.", written, &tz);
    assert!(dated.is_some_and(|t| t > written));
    assert_eq!(codex("date"), Some(LimitHit { resume_at: dated }));
    assert_eq!(codex("aborted"), Some(LimitHit { resume_at: dated }));
    // 「Try again later.」认得出撞上限，但没有时间。
    assert_eq!(codex("later"), Some(LimitHit { resume_at: None }));
}

#[test]
fn codex_not_a_limit() {
    assert_eq!(codex("limit-then-user"), None, "之后又有用户消息");
    assert_eq!(codex("other-error"), None, "别的错误");
}
