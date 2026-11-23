//! 整段对话（`Source::load_transcript`）：三家的消息、工具调用、工具返回按时间先后，坏行跳过，
//! Codex 没有 rollout 文件时从正文库读。样例都是手写的。

mod sources_common;

use std::fs;
use std::path::{Path, PathBuf};

use huitai::core::{Session, SessionId, Vendor};
use huitai::sources::{ClaudeSource, CodexSource, Part, PiSource, Role, Source, Transcript, Trash};
use sources_common::{at, build_db, temp_dir};

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

/// 每块写成一行好比较：`你: …`、`AI: …`、`⚙ 名字 参数`、`↳ 返回`。
fn outline(t: &Transcript) -> Vec<String> {
    t.parts
        .iter()
        .map(|p| match p {
            Part::Message(m) if m.role == Role::User => format!("你: {}", m.text),
            Part::Message(m) => format!("AI: {}", m.text),
            Part::ToolCall { name, input } => format!("⚙ {name} {input}"),
            Part::ToolResult { text } => format!("↳ {text}"),
        })
        .collect()
}

fn write(path: &Path, lines: &[&str]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lines.join("\n") + "\n").unwrap();
}

#[test]
fn claude_messages_tools_and_results_in_order() {
    let base = temp_dir("tr-claude");
    let path = base.join("projects/-Users-demo-projects-a/s1.jsonl");
    write(
        &path,
        &[
            r#"{"type":"user","timestamp":"2026-11-20T10:00:00Z","message":{"role":"user","content":"看看登录页"}}"#,
            r#"{"type":"assistant","timestamp":"2026-11-20T10:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"我先看文件"},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/x/login.rs"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"fn login() {}"}]}}"#,
            "坏行",
            r#"{"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"子代理的话"}]}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"元信息"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"cargo test","description":"跑测试"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":[{"type":"text","text":"ok"}]}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"改好了"}]}}"#,
        ],
    );
    let src = ClaudeSource::new(base.join("projects"), Trash::new(base.join("trash")));
    let t = src
        .load_transcript(&session(Vendor::Claude, "s1", path))
        .unwrap();
    assert!(!t.truncated);
    assert_eq!(
        outline(&t),
        [
            "你: 看看登录页",
            "AI: 我先看文件",
            "⚙ Read /x/login.rs",
            "↳ fn login() {}",
            "⚙ Bash cargo test",
            "↳ ok",
            "AI: 改好了",
        ]
    );
    let Part::Message(first) = &t.parts[0] else {
        unreachable!()
    };
    assert!(first.at.is_some(), "带时间");
}

#[test]
fn pi_tool_calls_and_tool_results() {
    let base = temp_dir("tr-pi");
    let path = base.join("sessions/--Users-demo--/2026-11-20T10-00-00-000Z_p1.jsonl");
    write(
        &path,
        &[
            r#"{"type":"session","id":"p1","cwd":"/Users/demo"}"#,
            r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"列一下目录"}],"timestamp":1795000000000}}"#,
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"thinking","thinking":"想想"},{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls","timeout":10}}]}}"#,
            r#"{"type":"message","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"a.txt"}]}}"#,
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"只有 a.txt"}]}}"#,
        ],
    );
    let src = PiSource::new(base.join("sessions"), Trash::new(base.join("trash")));
    let t = src
        .load_transcript(&session(Vendor::Pi, "p1", path))
        .unwrap();
    assert_eq!(
        outline(&t),
        ["你: 列一下目录", "⚙ bash ls", "↳ a.txt", "AI: 只有 a.txt"]
    );
}

#[test]
fn codex_rollout_tools_without_duplicates() {
    let base = temp_dir("tr-codex-file");
    let root = base.join("codex");
    let path = root.join("sessions/2026/11/20/rollout-2026-11-20T10-00-00-c1.jsonl");
    write(
        &path,
        &[
            r#"{"type":"session_meta","payload":{"id":"c1","cwd":"/Users/demo"}}"#,
            r#"{"timestamp":"2026-11-20T10:00:00Z","type":"event_msg","payload":{"type":"user_message","message":"查一下状态"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"查一下状态"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"git\",\"status\"]}","call_id":"a"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"a","output":"clean"}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"run()","call_id":"b"}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","command":["zsh","-lc","pwd"]}}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"b","output":[{"type":"input_text","text":"done"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"没有改动"}}"#,
        ],
    );
    let src = CodexSource::new(&root, Trash::new(base.join("trash")));
    let t = src
        .load_transcript(&session(Vendor::Codex, "c1", path))
        .unwrap();
    assert_eq!(
        outline(&t),
        [
            "你: 查一下状态",
            "⚙ shell git status",
            "↳ clean",
            "⚙ exec run()",
            "↳ done",
            "AI: 没有改动",
        ]
    );
}

#[test]
fn codex_history_db_when_rollout_is_gone() {
    let base = temp_dir("tr-codex-db");
    let root = base.join("codex");
    fs::create_dir_all(&root).unwrap();
    let id = "ffffffff-0000-4000-8000-000000000006";
    let sql = format!(
        "CREATE TABLE thread_items (thread_id TEXT, turn_id TEXT, item_id TEXT, rollout_ordinal INTEGER, \
         created_at_ms INTEGER, item_json TEXT, item_type TEXT);
         INSERT INTO thread_items VALUES
         ('{id}','t','1',1,1795172000000,'{{\"type\":\"userMessage\",\"content\":[{{\"type\":\"text\",\"text\":\"跑一下\"}}]}}','userMessage'),
         ('{id}','t','2',2,1795172001000,'{{\"type\":\"reasoning\"}}','reasoning'),
         ('{id}','t','3',3,1795172002000,'{{\"type\":\"commandExecution\",\"command\":\"/bin/zsh -lc pwd\",\"aggregatedOutput\":\"/tmp\"}}','commandExecution'),
         ('{id}','t','4',4,1795172003000,'{{\"type\":\"fileChange\",\"changes\":[{{\"path\":\"/x/a.rs\"}},{{\"path\":\"/x/b.rs\"}}]}}','fileChange'),
         ('{id}','t','5',5,1795172004000,'{{\"type\":\"agentMessage\",\"text\":\"好了\"}}','agentMessage'),
         ('other','t','1',1,1795172000000,'{{\"type\":\"agentMessage\",\"text\":\"别的对话\"}}','agentMessage');"
    );
    build_db(&root.join("thread_history_1.sqlite"), &sql);
    let src = CodexSource::new(&root, Trash::new(base.join("trash")));
    let t = src
        .load_transcript(&session(Vendor::Codex, id, root.join("gone.jsonl")))
        .unwrap();
    assert!(!t.truncated);
    assert_eq!(
        outline(&t),
        [
            "你: 跑一下",
            "⚙ shell /bin/zsh -lc pwd",
            "↳ /tmp",
            "⚙ 改文件 /x/a.rs /x/b.rs",
            "AI: 好了",
        ]
    );
    // 没有库也没有文件：空的，不报错。
    let none = CodexSource::new(base.join("nothing"), Trash::new(base.join("trash")));
    let t = none
        .load_transcript(&session(Vendor::Codex, id, root.join("gone.jsonl")))
        .unwrap();
    assert!(t.parts.is_empty());
}

#[test]
fn missing_file_is_an_error_for_jsonl_sources() {
    let base = temp_dir("tr-missing");
    let src = ClaudeSource::new(base.join("projects"), Trash::new(base.join("trash")));
    assert!(
        src.load_transcript(&session(Vendor::Claude, "x", base.join("none.jsonl")))
            .is_err()
    );
}
