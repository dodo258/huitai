//! 「此刻」（`Source::load_moment`）：Claude、Codex、pi 的在跑工具、停下的一句话、用时、计划、失败。
//! 样例都是手写的；另外五家在各自的 sources_<家>.rs 里用它们的样例测。

mod sources_common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use huitai::core::{Session, SessionId, Vendor};
use huitai::sources::{
    AntigravitySource, ClaudeSource, CodexSource, HermesSource, Moment, OpenCodeSource, PiSource,
    Source, StepStatus, Trash, moment,
};
use sources_common::{at, temp_dir};

fn session(vendor: Vendor, id: &str, path: PathBuf) -> Session {
    Session {
        vendor,
        id: SessionId::from(id),
        cwd: PathBuf::from("/Users/demo/projects/a"),
        path,
        title: None,
        git_branch: None,
        last_active: at(1_795_000_000),
        message_count: 1,
        automatic: false,
        account: String::new(),
    }
}

fn write(path: &Path, lines: &[&str]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn claude(name: &str, lines: &[&str]) -> Moment {
    let base = temp_dir(name);
    let path = base.join("projects/-Users-demo-projects-a/s1.jsonl");
    write(&path, lines);
    ClaudeSource::new(base.join("projects"), Trash::new(base.join("trash")))
        .load_moment(&session(Vendor::Claude, "s1", path))
        .unwrap()
}

fn codex(name: &str, lines: &[&str]) -> Moment {
    let base = temp_dir(name);
    let root = base.join("codex");
    let path = root.join("sessions/2026/11/20/rollout-2026-11-20T10-00-00-c1.jsonl");
    write(&path, lines);
    CodexSource::new(&root, Trash::new(base.join("trash")))
        .load_moment(&session(Vendor::Codex, "c1", path))
        .unwrap()
}

fn tool(m: &Moment) -> Option<(&str, &str)> {
    m.tool.as_ref().map(|t| (t.name.as_str(), t.input.as_str()))
}

#[test]
fn claude_running_tool_and_plan() {
    let m = claude(
        "mo-claude-run",
        &[
            r#"{"type":"user","timestamp":"2026-11-20T10:00:00Z","message":{"role":"user","content":"修登录页"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t0","name":"TodoWrite","input":{"todos":[{"content":"读代码","status":"completed","activeForm":"在读代码"},{"content":"改登录","status":"in_progress","activeForm":"在改登录"},{"content":"跑测试","status":"pending","activeForm":"在跑测试"}]}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t0","content":"ok"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"我先看文件。"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"src/ui/list.rs","old_string":"a","new_string":"b"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"done"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"cargo test","description":"跑测试"}}]}}"#,
            "坏行",
            r#"{"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"tool_use","id":"t9","name":"Grep","input":{"pattern":"子代理"}}]}}"#,
        ],
    );
    assert_eq!(tool(&m), Some(("Bash", "cargo test")), "子代理的调用不算");
    assert_eq!(m.turn_started, Some(at(1_795_168_800)));
    assert_eq!(m.duration, None, "这一轮还没完");
    assert_eq!(m.plan_progress(), Some((1, 3)));
    assert_eq!(m.plan[1].step, "改登录");
    assert_eq!(m.plan[1].status, StepStatus::Doing);
    assert_eq!(m.failed, None);
}

/// 一条回复里并行起两个工具，快的先返回：结果按 `tool_use_id` 收掉快的，在跑的是慢的。
#[test]
fn claude_parallel_tools_close_by_tool_use_id() {
    let m = claude(
        "mo-claude-parallel",
        &[
            r#"{"type":"user","timestamp":"2026-11-20T10:00:00Z","message":{"role":"user","content":"跑测试顺便看下日志"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_slow","name":"Bash","input":{"command":"cargo test"}},{"type":"tool_use","id":"toolu_fast","name":"Read","input":{"file_path":"app.log"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_fast","content":"log"}]}}"#,
        ],
    );
    assert_eq!(tool(&m), Some(("Bash", "cargo test")));
    // 没有 id 的旧格式照旧按先后对上。
    let m = claude(
        "mo-claude-noid",
        &[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}},{"type":"tool_use","name":"Read","input":{"file_path":"a"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#,
        ],
    );
    assert_eq!(tool(&m), Some(("Read", "a")));
}

#[test]
fn claude_stopped_uses_away_summary_and_turn_duration() {
    let lines = [
        r#"{"type":"user","timestamp":"2026-11-20T10:00:00Z","message":{"role":"user","content":"修登录页"}}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"改好了，测试都过了。另外顺手删了一个没用的函数"}]}}"#,
        r#"{"type":"system","subtype":"turn_duration","durationMs":372000,"timestamp":"2026-11-20T10:06:12Z"}"#,
    ];
    let m = claude("mo-claude-stop", &lines);
    assert_eq!(tool(&m), None);
    assert_eq!(
        m.summary.as_deref(),
        Some("改好了，测试都过了。"),
        "没有总结用最后一句"
    );
    assert_eq!(m.duration, Some(Duration::from_secs(372)));

    let mut with_summary = lines.to_vec();
    with_summary.push(
        r#"{"type":"system","subtype":"away_summary","content":"登录页按钮修好了，等你确认要不要发版。"}"#,
    );
    let m = claude("mo-claude-away", &with_summary);
    assert_eq!(
        m.summary.as_deref(),
        Some("登录页按钮修好了，等你确认要不要发版。")
    );
    assert_eq!(m.duration, Some(Duration::from_secs(372)));
}

#[test]
fn codex_running_tool_turn_start_and_plan() {
    let m = codex(
        "mo-codex-run",
        &[
            r#"{"type":"session_meta","payload":{"id":"c1","cwd":"/Users/demo"}}"#,
            r#"{"timestamp":"2026-11-20T10:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"x","started_at":1795168800}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"加个导出"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"update_plan","arguments":"{\"plan\":[{\"step\":\"读接口\",\"status\":\"completed\"},{\"step\":\"写导出\",\"status\":\"in_progress\"},{\"step\":\"补测试\",\"status\":\"pending\"},{\"step\":\"更新文档\",\"status\":\"pending\"},{\"step\":\"发版\",\"status\":\"pending\"}]}","call_id":"p"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"p","output":"Plan updated"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"cargo\",\"build\"]}","call_id":"a"}}"#,
        ],
    );
    assert_eq!(tool(&m), Some(("shell", "cargo build")));
    assert_eq!(m.turn_started, Some(at(1_795_168_800)));
    assert_eq!(m.plan_progress(), Some((1, 5)));
    assert_eq!(m.plan[0].status, StepStatus::Done);
    assert_eq!(m.plan[4].status, StepStatus::Todo);
}

#[test]
fn codex_task_complete_and_turn_aborted() {
    let done = codex(
        "mo-codex-done",
        &[
            r#"{"type":"event_msg","payload":{"type":"task_started","started_at":1795168800}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"加个导出"}}"#,
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"导出做好了，在设置页最下面。"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","duration_ms":360000,"last_agent_message":"导出做好了"}}"#,
        ],
    );
    assert_eq!(
        done.summary.as_deref(),
        Some("导出做好了，在设置页最下面。")
    );
    assert_eq!(done.duration, Some(Duration::from_secs(360)));
    assert_eq!(done.failed, None);

    let aborted = codex(
        "mo-codex-abort",
        &[
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"跑全量测试"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"cargo\",\"test\"]}","call_id":"a"}}"#,
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted","duration_ms":300276}}"#,
        ],
    );
    assert_eq!(aborted.failed.as_deref(), Some("interrupted"));
    assert_eq!(tool(&aborted), None, "打断后不再算在跑那个工具");
    assert_eq!(aborted.duration, Some(Duration::from_millis(300_276)));
}

#[test]
fn codex_without_rollout_file_reads_history_db_like_transcript() {
    // 文件不在、也没有正文库：空的，不报错。
    let base = temp_dir("mo-codex-gone");
    let root = base.join("codex");
    fs::create_dir_all(&root).unwrap();
    let m = CodexSource::new(&root, Trash::new(base.join("trash")))
        .load_moment(&session(Vendor::Codex, "c1", root.join("gone.jsonl")))
        .unwrap();
    assert_eq!(m, Moment::default());
}

#[test]
fn pi_pending_tool_call_then_reply() {
    let base = temp_dir("mo-pi");
    let path = base.join("sessions/--Users-demo--/2026-11-20T10-00-00-000Z_p1.jsonl");
    let mut lines = vec![
        r#"{"type":"session","id":"p1","cwd":"/Users/demo"}"#,
        r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"列一下目录"}],"timestamp":1795168800000}}"#,
        r#"{"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls -la"}}]}}"#,
    ];
    write(&path, &lines);
    let src = PiSource::new(base.join("sessions"), Trash::new(base.join("trash")));
    let s = session(Vendor::Pi, "p1", path.clone());
    let m = src.load_moment(&s).unwrap();
    assert_eq!(tool(&m), Some(("bash", "ls -la")));
    assert_eq!(m.turn_started, Some(at(1_795_168_800)));

    lines.extend([
        r#"{"type":"message","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"a.txt"}]}}"#,
        r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"只有 a.txt\n别的都没有"}]}}"#,
    ]);
    write(&path, &lines);
    let m = src.load_moment(&s).unwrap();
    assert_eq!(tool(&m), None);
    assert_eq!(m.summary.as_deref(), Some("只有 a.txt"));
}

#[test]
fn only_the_tail_is_read() {
    // 很早以前的工具调用在尾部窗口之外：读不到也不算在跑。
    let base = temp_dir("mo-tail");
    let path = base.join("projects/-Users-demo-projects-a/s1.jsonl");
    let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"很早的命令"}}]}}"#;
    let filler = format!(r#"{{"type":"progress","data":"{}"}}"#, "x".repeat(1024));
    let mut lines = vec![call.to_owned()];
    let n = (moment::MOMENT_TAIL_BYTES / 1024 + 8) as usize;
    lines.extend(std::iter::repeat_n(filler, n));
    lines.push(
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"最后一句"}]}}"#
            .to_owned(),
    );
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    write(&path, &refs);
    let m = ClaudeSource::new(base.join("projects"), Trash::new(base.join("trash")))
        .load_moment(&session(Vendor::Claude, "s1", path))
        .unwrap();
    assert_eq!(tool(&m), None);
    assert_eq!(m.summary.as_deref(), Some("最后一句"));
}

#[test]
fn missing_file_is_an_error_not_a_panic() {
    let base = temp_dir("mo-missing");
    let src = ClaudeSource::new(base.join("projects"), Trash::new(base.join("trash")));
    assert!(
        src.load_moment(&session(Vendor::Claude, "x", base.join("none.jsonl")))
            .is_err()
    );
}

#[test]
fn non_default_account_keeps_vendor_events() {
    use huitai::core::Account;
    use huitai::sources::AccountSource;
    let base = temp_dir("mo-account");
    let dir = base.join(".claude-work");
    let path = dir.join("projects/-Users-demo-projects-a/s1.jsonl");
    write(
        &path,
        &[
            r#"{"type":"user","timestamp":"2026-11-20T10:00:00Z","message":{"role":"user","content":"修登录页"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"改好了。"}]}}"#,
            r#"{"type":"system","subtype":"turn_duration","durationMs":60000}"#,
            r#"{"type":"system","subtype":"away_summary","content":"登录页修好了。"}"#,
        ],
    );
    let work = Account::new(Vendor::Claude, "工作号", dir.clone(), &base);
    assert!(!work.key.is_empty());
    let inner = ClaudeSource::new(dir.join("projects"), Trash::new(base.join("trash")));
    let src = AccountSource::new(Box::new(inner), &work);
    let mut s = session(Vendor::Claude, "s1", path);
    s.id = SessionId::in_account(&work.key, "s1");
    let m = src.load_moment(&s).unwrap();
    // 包装层要把「此刻」交给 Claude 自己的读法：总结和用时是 Claude 的轮次事件，从整段对话推不出来。
    assert_eq!(m.summary.as_deref(), Some("登录页修好了。"));
    assert_eq!(m.duration, Some(Duration::from_secs(60)));
}

/// 对话都在一个库里的几家，「此刻」重读看库和它的 WAL：Hermes、OpenCode 的会话路径是空的，
/// Antigravity 的是产物目录，只看它们，库里来了新回复也不会重读。
#[test]
fn database_vendors_refresh_by_their_database() {
    let base = PathBuf::from("/h");
    let trash = || Trash::new(base.join("trash"));
    let wal = |db: &str| [base.join(db), base.join(format!("{db}-wal"))];
    let cases: [(Box<dyn Source>, PathBuf, [PathBuf; 2]); 3] = [
        (
            Box::new(HermesSource::new(base.join(".hermes"), trash())),
            PathBuf::new(),
            wal(".hermes/state.db"),
        ),
        (
            Box::new(OpenCodeSource::new(base.join("oc/opencode.db"), trash())),
            PathBuf::new(),
            wal("oc/opencode.db"),
        ),
        (
            Box::new(AntigravitySource::new(base.join("agy"), trash())),
            base.join("agy/brain/c1"),
            wal("agy/conversation_summaries.db"),
        ),
    ];
    for (src, path, want) in cases {
        let s = session(src.vendor(), "c1", path);
        assert_eq!(src.moment_files(&s), want, "{}", src.vendor());
    }
    // jsonl 几家看会话文件本身（连同旁边的 -wal，没有就是没有）。
    let claude = ClaudeSource::new(base.join("projects"), trash());
    let file = base.join("projects/-a/s1.jsonl");
    assert_eq!(
        claude.moment_files(&session(Vendor::Claude, "s1", file.clone()))[0],
        file
    );
}
