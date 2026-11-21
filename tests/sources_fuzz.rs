//! 解析器 fuzz：三家适配器喂随机、截断、非 UTF-8、超长行、字段类型不对、深嵌套、半截末行的数据，
//! 不 panic，坏行跳过（好行该列出来的照样列出来）。数据全部由固定种子生成，失败可复现；不读真实会话。

mod fuzz_support;

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use fuzz_support::{MIXED, Rng, bad_line};
use huitai::core::{Session, Vendor};
use huitai::sources::{ClaudeSource, CodexSource, PiSource, Source, Trash};
use serde_json::json;

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("huitai-fuzz-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// 一家的样例：头行（可能没有）、用户行、AI 行、追加在末尾的标题行（可能没有）。
struct Sample {
    header: Option<String>,
    user: fn(&str) -> String,
    assistant: fn(&str) -> String,
}

fn claude_sample() -> Sample {
    Sample {
        header: None,
        user: |t| {
            json!({"type": "user", "message": {"role": "user", "content": t},
                   "timestamp": "2026-11-01T09:00:00.000Z", "cwd": "/Users/demo/projects/shop",
                   "gitBranch": "main", "sessionId": "x"})
            .to_string()
        },
        assistant: |t| {
            json!({"type": "assistant",
                   "message": {"role": "assistant", "content": [{"type": "text", "text": t}]},
                   "timestamp": "2026-11-01T09:00:05.000Z", "cwd": "/Users/demo/projects/shop"})
            .to_string()
        },
    }
}

fn codex_sample(id: &str) -> Sample {
    Sample {
        header: Some(
            json!({"timestamp": "2026-11-02T11:00:00.000Z", "type": "session_meta",
                   "payload": {"id": id, "cwd": "/Users/demo/projects/api", "source": "cli",
                               "git": {"branch": "main"}}})
            .to_string(),
        ),
        user: |t| {
            json!({"timestamp": "2026-11-02T11:00:01.000Z", "type": "event_msg",
                   "payload": {"type": "item_completed",
                               "item": {"type": "UserMessage", "content": [{"type": "text", "text": t}]}}})
            .to_string()
        },
        assistant: |t| {
            json!({"timestamp": "2026-11-02T11:00:02.000Z", "type": "event_msg",
                   "payload": {"type": "agent_message", "message": t}})
            .to_string()
        },
    }
}

fn pi_sample(id: &str) -> Sample {
    Sample {
        header: Some(
            json!({"type": "session", "version": 3, "id": id,
                   "timestamp": "2026-11-03T08:00:00.000Z", "cwd": "/Users/demo/projects/blog"})
            .to_string(),
        ),
        user: |t| {
            json!({"type": "message", "id": "m1", "timestamp": "2026-11-03T08:00:01.000Z",
                   "message": {"role": "user", "content": [{"type": "text", "text": t}],
                               "timestamp": 1_793_779_201_000_i64}})
            .to_string()
        },
        assistant: |t| {
            json!({"type": "message", "id": "m2", "timestamp": "2026-11-03T08:00:02.000Z",
                   "message": {"role": "assistant", "content": [{"type": "text", "text": t}]}})
            .to_string()
        },
    }
}

/// 一个 fuzz 文件：好的骨架（头行、第一句用户话）之后好行坏行混排，可能以半截行结尾。
/// 返回内容、第一句用户话、好的用户行条数。
fn skeleton_file(s: &Sample, rng: &mut Rng) -> (Vec<u8>, String, usize) {
    let mut out = Vec::new();
    let line = |out: &mut Vec<u8>, b: &[u8]| {
        out.extend_from_slice(b);
        out.push(b'\n');
    };
    if let Some(h) = &s.header {
        line(&mut out, h.as_bytes());
    }
    let first = format!("问题 {} 开始", rng.next() % 10_000);
    line(&mut out, (s.user)(&first).as_bytes());
    let mut users = 1;
    for _ in 0..rng.below(30) {
        let text = rng.pick(MIXED).trim().to_owned();
        let text = if text.is_empty() {
            "好".to_owned()
        } else {
            text
        };
        let is_user = rng.chance(50);
        let good = if is_user {
            (s.user)(&text)
        } else {
            (s.assistant)(&text)
        };
        if rng.chance(55) {
            line(&mut out, &bad_line(&good, rng));
        } else {
            users += usize::from(is_user);
            line(&mut out, good.as_bytes());
        }
    }
    // 厂商正写着的半截末行。
    if rng.chance(40) {
        let tail = (s.user)("半截");
        out.extend_from_slice(&tail.as_bytes()[..rng.below(tail.len())]);
    }
    (out, first, users)
}

/// 整个文件都是坏数据。
fn garbage_file(s: &Sample, rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..rng.below(20) {
        let base = if rng.chance(50) {
            (s.user)("x")
        } else {
            s.header.clone().unwrap_or_else(|| (s.assistant)("y"))
        };
        out.extend_from_slice(&bad_line(&base, rng));
        if rng.chance(90) {
            out.push(if rng.chance(80) { b'\n' } else { b'\r' });
        }
    }
    out
}

fn uuid(i: usize) -> String {
    format!("{i:08x}-0000-4000-8000-{i:012x}")
}

/// 每个会话都调一遍预览和精确计数，不能出错或 panic；返回 id → 精确用户发言数。
fn exercise(src: &dyn Source, sessions: &[Session]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for s in sessions {
        for n in [0, 1, 3, 5000] {
            let p = src
                .load_preview(s, n)
                .unwrap_or_else(|e| panic!("{} 预览出错: {e}", s.path.display()));
            assert!(p.len() <= n);
            assert!(p.iter().all(|m| !m.text.is_empty()));
        }
        let n = src
            .count_messages(s)
            .unwrap_or_else(|e| panic!("{} 计数出错: {e}", s.path.display()));
        counts.insert(s.id.as_str().to_owned(), n);
        assert!(
            s.title
                .as_deref()
                .is_none_or(|t| t.chars().count() <= 81 && !t.contains('\n')),
            "标题要折成一行：{:?}",
            s.title
        );
    }
    counts
}

const FILES: usize = 160;

#[test]
fn claude_parser_survives_fuzz() {
    let root = temp_dir("claude").join("projects");
    let dir = root.join("-Users-demo-projects-shop");
    fs::create_dir_all(&dir).unwrap();
    let mut rng = Rng::new(0xC1A0);
    let sample = claude_sample();
    let mut expect = HashMap::new();
    for i in 0..FILES {
        let id = uuid(i);
        let path = dir.join(format!("{id}.jsonl"));
        if i % 4 == 3 {
            fs::write(&path, garbage_file(&sample, &mut rng)).unwrap();
        } else {
            let (bytes, first, users) = skeleton_file(&sample, &mut rng);
            fs::write(&path, bytes).unwrap();
            expect.insert(id, (first, users));
        }
    }
    let src = ClaudeSource::new(&root, Trash::new(root.with_file_name("trash")));
    check(&src, &expect);
}

#[test]
fn codex_parser_survives_fuzz() {
    let base = temp_dir("codex");
    let root = base.join("codex");
    let day = root.join("sessions/2026/11/02");
    fs::create_dir_all(&day).unwrap();
    let mut rng = Rng::new(0xC0DE);
    let mut expect = HashMap::new();
    let mut index = fs::File::create(root.join("session_index.jsonl")).unwrap();
    for i in 0..FILES {
        let id = uuid(i);
        let sample = codex_sample(&id);
        let path = day.join(format!("rollout-2026-11-02T11-00-00-{id}.jsonl"));
        if i % 4 == 3 {
            fs::write(&path, garbage_file(&sample, &mut rng)).unwrap();
            continue;
        }
        let (bytes, first, users) = skeleton_file(&sample, &mut rng);
        fs::write(&path, bytes).unwrap();
        // session_index：坏行和别家 id 的行混在前面，这条的名字在最后（i 是 5 的倍数才有）。
        let other = json!({"id": uuid(i + 10_000), "thread_name": "别的对话"}).to_string();
        for _ in 0..rng.below(3) {
            index.write_all(&bad_line(&other, &mut rng)).unwrap();
            index.write_all(b"\n").unwrap();
        }
        let title = if i % 5 == 0 {
            let name = format!("改过名 {i}");
            writeln!(index, "{}", json!({"id": id, "thread_name": name})).unwrap();
            name
        } else {
            first
        };
        expect.insert(id, (title, users));
    }
    drop(index);
    let src = CodexSource::new(&root, Trash::new(base.join("trash")));
    check(&src, &expect);
}

#[test]
fn pi_parser_survives_fuzz() {
    let base = temp_dir("pi");
    let root = base.join("sessions");
    let dir = root.join("--Users-demo-projects-blog--");
    fs::create_dir_all(&dir).unwrap();
    let mut rng = Rng::new(0x9191);
    let mut expect = HashMap::new();
    for i in 0..FILES {
        let id = uuid(i);
        let sample = pi_sample(&id);
        let path = dir.join(format!("2026-11-03T08-00-00-000Z_{id}.jsonl"));
        if i % 4 == 3 {
            fs::write(&path, garbage_file(&sample, &mut rng)).unwrap();
        } else {
            let (bytes, first, users) = skeleton_file(&sample, &mut rng);
            fs::write(&path, bytes).unwrap();
            expect.insert(id, (first, users));
        }
    }
    let src = PiSource::new(&root, Trash::new(base.join("trash")));
    check(&src, &expect);
}

/// 有好骨架的文件都要列出来：标题是第一句话（或改过的名字），精确计数不少于好的用户行。
fn check(src: &dyn Source, expect: &HashMap<String, (String, usize)>) {
    let scan = src.scan();
    let counts = exercise(src, &scan.sessions);
    let by_id: HashMap<&str, &Session> = scan.sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    for (id, (title, users)) in expect {
        let s = by_id.get(id.as_str()).unwrap_or_else(|| {
            panic!("{} 有好的骨架却没列出来；problems: {:?}", id, scan.problems)
        });
        assert_eq!(s.title.as_deref(), Some(title.as_str()), "{id} 的标题");
        assert!(s.message_count >= 1, "{id} 至少一句用户话");
        assert!(
            counts[id] >= *users,
            "{id} 精确计数 {} 少于好的用户行 {users}",
            counts[id]
        );
    }
    // 同一 id 不重复列出。
    assert_eq!(by_id.len(), scan.sessions.len());
    assert_vendor(src.vendor(), &scan.sessions);
}

fn assert_vendor(v: Vendor, sessions: &[Session]) {
    assert!(sessions.iter().all(|s| s.vendor == v));
}

/// 读到一半的文件：末尾是半截行时，扫描结果和没有这半行时一样；补齐后再扫就多出这一句。
#[test]
fn half_written_last_line_is_ignored_until_complete() {
    let root = temp_dir("half").join("projects");
    let dir = root.join("-Users-demo-projects-shop");
    fs::create_dir_all(&dir).unwrap();
    let s = claude_sample();
    let path = dir.join("half.jsonl");
    let done = format!("{}\n{}\n", (s.user)("第一句"), (s.assistant)("回答"));
    let next = (s.user)("第二句还在写");
    let src = ClaudeSource::new(&root, Trash::new(root.with_file_name("trash")));

    fs::write(&path, &done).unwrap();
    let before = src.scan().sessions;
    for cut in [1, next.len() / 3, next.len() / 2, next.len() - 1] {
        let mut bytes = done.clone().into_bytes();
        bytes.extend_from_slice(&next.as_bytes()[..cut]);
        fs::write(&path, bytes).unwrap();
        let now = src.scan().sessions;
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].title, before[0].title);
        assert_eq!(now[0].message_count, 1, "半截行（{cut} 字节）不能算一句");
        assert_eq!(src.count_messages(&now[0]).unwrap(), 1);
    }
    fs::write(&path, format!("{done}{next}\n")).unwrap();
    let after = src.scan().sessions;
    assert_eq!(after[0].message_count, 2);
}

/// 第一行比扫描读的开头一段（64KB）还长（Codex 的 session_meta 带很长的系统提示）：对话照样列出来。
#[test]
fn codex_long_session_meta_is_still_listed() {
    let base = temp_dir("longmeta");
    let root = base.join("codex");
    let day = root.join("sessions/2026/11/02");
    fs::create_dir_all(&day).unwrap();
    let id = uuid(7);
    let meta = json!({"timestamp": "2026-11-02T11:00:00.000Z", "type": "session_meta",
        "payload": {"id": id, "cwd": "/Users/demo/projects/api", "source": "cli",
                    "base_instructions": {"text": "说明".repeat(60_000)}}});
    let s = codex_sample(&id);
    let mut body = format!("{meta}\n{}\n", (s.user)("长头之后的问题"));
    // 中间再塞满，让文件比头尾两段加起来还大。
    for i in 0..400 {
        body.push_str(&(s.assistant)(&format!("填充回答 {i} {}", "x".repeat(400))));
        body.push('\n');
    }
    body.push_str(&(s.user)("最后一句"));
    body.push('\n');
    fs::write(
        day.join(format!("rollout-2026-11-02T11-00-00-{id}.jsonl")),
        body,
    )
    .unwrap();
    let sessions = CodexSource::new(&root, Trash::new(base.join("trash")))
        .scan()
        .sessions;
    assert_eq!(sessions.len(), 1, "session_meta 超过 64KB 的对话丢了");
    assert_eq!(sessions[0].title.as_deref(), Some("长头之后的问题"));
    assert_eq!(sessions[0].cwd, Path::new("/Users/demo/projects/api"));
    assert_eq!(sessions[0].message_count, 2);
}

/// 最后一行比扫描读的结尾一段还长（大段工具输出）：改过的标题、最后时间仍从结尾读到。
#[test]
fn claude_long_last_line_keeps_tail_info() {
    let root = temp_dir("longtail").join("projects");
    let dir = root.join("-Users-demo-projects-shop");
    fs::create_dir_all(&dir).unwrap();
    let s = claude_sample();
    let mut body = format!("{}\n", (s.user)("开头的问题"));
    for i in 0..300 {
        body.push_str(&(s.assistant)(&format!("中间 {i} {}", "y".repeat(400))));
        body.push('\n');
    }
    body.push_str(&json!({"type": "custom-title", "customTitle": "改过的名字"}).to_string());
    body.push('\n');
    let huge = json!({"type": "assistant", "timestamp": "2026-11-09T10:00:00.000Z",
        "message": {"role": "assistant", "content": [{"type": "text", "text": "z".repeat(300_000)}]}});
    body.push_str(&huge.to_string());
    body.push('\n');
    fs::write(dir.join("longtail.jsonl"), body).unwrap();
    let sessions = ClaudeSource::new(&root, Trash::new(root.with_file_name("trash")))
        .scan()
        .sessions;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].title.as_deref(), Some("改过的名字"));
    // 2026-11-09T10:00:00Z
    let want = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_794_218_400);
    assert_eq!(sessions[0].last_active, want);
}
