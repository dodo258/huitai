//! 精确计数接着上次数：文件只是变长时只读新增部分，变短、开头或接缝处被改写时整份重数；
//! 不管怎么接着数，结果都和整份重数一致，交出的新增消息拼起来就是整个文件的消息。

mod sources_common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use huitai::core::Session;
use huitai::sources::{
    ClaudeSource, CodexSource, Counted, PiSource, Progress, Role, Source, Trash,
};
use serde_json::json;
use sources_common::{copy_tree, fixture, temp_dir};

fn user(text: &str) -> String {
    json!({"type": "user", "message": {"role": "user", "content": text},
           "timestamp": "2026-11-01T09:00:00.000Z", "cwd": "/Users/demo/projects/shop"})
    .to_string()
}

fn assistant(text: &str) -> String {
    json!({"type": "assistant",
           "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
           "timestamp": "2026-11-01T09:00:05.000Z", "cwd": "/Users/demo/projects/shop"})
    .to_string()
}

/// 一个 Claude 会话文件：`rounds` 轮一问一答，每行带一段填充，文件比摘要读的两段（各 4KB）长得多。
fn claude(name: &str, rounds: usize) -> (ClaudeSource, Session, PathBuf) {
    let base = temp_dir(&format!("since-{name}"));
    let dir = base.join("projects/-Users-demo-projects-shop");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s1.jsonl");
    let mut text = String::new();
    for i in 0..rounds {
        text += &format!(
            "{}\n{}\n",
            user(&format!("问题 {i}")),
            assistant(&"回答".repeat(40))
        );
    }
    fs::write(&path, text).unwrap();
    let src = ClaudeSource::new(base.join("projects"), Trash::new(base.join("trash")));
    let session = src.scan().sessions.remove(0);
    (src, session, path)
}

fn append(path: &Path, text: &str) {
    let mut f = OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

/// 把文件里第一处 `from` 原地换成等长的 `to`，文件长度不变。
fn patch(path: &Path, at_least: usize, from: &str, to: &str) {
    assert_eq!(from.len(), to.len());
    let text = fs::read_to_string(path).unwrap();
    let i = at_least + text[at_least..].find(from).unwrap();
    let mut bytes = text.into_bytes();
    bytes[i..i + to.len()].copy_from_slice(to.as_bytes());
    fs::write(path, bytes).unwrap();
}

fn texts(c: &Counted) -> Vec<(Role, String)> {
    c.appended
        .as_ref()
        .unwrap()
        .iter()
        .map(|m| (m.role, m.text.clone()))
        .collect()
}

#[test]
fn appended_part_is_the_only_part_read() {
    let (src, s, path) = claude("append", 100);
    let first = src.count_since(&s, None).unwrap();
    assert_eq!(first.users, 100);
    assert_eq!(first.appended, None, "从头数不交消息");
    let prev = first.progress.unwrap();
    assert_eq!(prev.pos, fs::metadata(&path).unwrap().len());

    // 中间一条用户发言改成别的类型（等长、不在开头和接缝那两段里）：只读新增部分就看不到这处改动。
    let len = prev.pos as usize;
    patch(&path, len / 2, r#""type":"user""#, r#""type":"note""#);
    append(
        &path,
        &format!("{}\n{}\n", user("新问题"), assistant("新回答")),
    );

    let next = src.count_since(&s, Some(prev)).unwrap();
    assert_eq!(next.users, 101, "接着数：100 + 新增 1");
    assert_eq!(
        texts(&next),
        [
            (Role::User, "新问题".to_owned()),
            (Role::Assistant, "新回答".to_owned())
        ]
    );
    assert_eq!(
        src.count_messages(&s).unwrap(),
        100,
        "整份重数能看到中间那处改动"
    );
    assert_eq!(
        next.progress.unwrap().pos,
        fs::metadata(&path).unwrap().len()
    );

    // 什么都没追加：不读新内容，计数不变。
    let same = src.count_since(&s, next.progress).unwrap();
    assert_eq!((same.users, texts(&same)), (101, vec![]));
}

#[test]
fn shrunk_or_rewritten_file_is_counted_whole() {
    type Change = fn(&Path, &Progress);
    let cases: [(&str, Change); 4] = [
        ("截短", |p, _| {
            let f = OpenOptions::new().write(true).open(p).unwrap();
            f.set_len(f.metadata().unwrap().len() / 3).unwrap();
        }),
        ("开头改写、长度不变再追加", |p, _| {
            patch(p, 0, "问题 0", "问题 X");
            append(p, &format!("{}\n", user("追加")));
        }),
        (
            "接缝处（上次数到的最后一行）改写",
            |p, prev| {
                patch(
                    p,
                    prev.pos as usize - 500,
                    r#""type":"user""#,
                    r#""type":"note""#,
                );
                append(p, &format!("{}\n", user("追加")));
            },
        ),
        ("换成另一份更长的文件", |p, _| {
            let mut text = String::new();
            for i in 0..300 {
                text += &format!("{}\n", user(&format!("另一份 {i}")));
            }
            fs::write(p, text).unwrap();
        }),
    ];
    for (i, (what, change)) in cases.iter().enumerate() {
        let (src, s, path) = claude(&format!("rewrite-{i}"), 100);
        let prev = src.count_since(&s, None).unwrap().progress.unwrap();
        change(&path, &prev);
        let got = src.count_since(&s, Some(prev)).unwrap();
        assert_eq!(got.appended, None, "{what}：要整份重数");
        assert_eq!(got.users, src.count_messages(&s).unwrap(), "{what}");
        assert_eq!(
            got.progress.unwrap(),
            src.count_since(&s, None).unwrap().progress.unwrap(),
            "{what}：进度和从头数的一样"
        );
    }
}

/// 一行一行地追加，有时只写半行（还没写换行），每一步都和整份重数一致；
/// 交出的新增消息拼起来，就是整个文件的全部消息。
#[test]
fn resumed_counts_match_whole_counts_step_by_step() {
    let (src, s, path) = claude("steps", 30);
    let first = src.count_since(&s, None).unwrap();
    let mut prev = first.progress;
    let mut seen: Vec<(Role, String)> = src
        .load_preview(&s, usize::MAX)
        .unwrap()
        .into_iter()
        .map(|m| (m.role, m.text))
        .collect();
    let mut seed = 7u32;
    for step in 0..60 {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let line = match seed % 4 {
            0 | 1 => user(&format!("第 {step} 步的问题")),
            2 => assistant(&format!("第 {step} 步的回答")),
            _ => r#"{"type":"progress","data":{}}"#.to_owned(),
        };
        if seed.is_multiple_of(5) {
            // 半行先写进去，下一步再补完。
            let (a, b) = line.split_at(line.len() / 2);
            append(&path, a);
            let mid = src.count_since(&s, prev).unwrap();
            assert_eq!(
                mid.users,
                src.count_messages(&s).unwrap(),
                "第 {step} 步半行"
            );
            assert_eq!(mid.progress, prev, "半行不算进进度");
            append(&path, &format!("{b}\n"));
        } else {
            append(&path, &format!("{line}\n"));
        }
        let c = src.count_since(&s, prev).unwrap();
        assert_eq!(c.users, src.count_messages(&s).unwrap(), "第 {step} 步");
        seen.extend(texts(&c));
        prev = c.progress;
    }
    let all: Vec<(Role, String)> = src
        .load_preview(&s, usize::MAX)
        .unwrap()
        .into_iter()
        .map(|m| (m.role, m.text))
        .collect();
    assert_eq!(seen, all);
}

/// 三家有会话文件的对话都能接着数：把文件内容整份再追加一遍，接着数等于整份重数。
#[test]
fn every_vendor_resumes_on_append() {
    let base = temp_dir("since-vendors");
    let trash = || Trash::new(base.join("trash"));
    copy_tree(&fixture("claude/projects"), &base.join("claude"));
    copy_tree(&fixture("codex"), &base.join("codex"));
    copy_tree(&fixture("pi/sessions"), &base.join("pi"));
    let sources: Vec<Box<dyn Source>> = vec![
        Box::new(ClaudeSource::new(base.join("claude"), trash())),
        Box::new(CodexSource::new(base.join("codex"), trash())),
        Box::new(PiSource::new(base.join("pi"), trash())),
    ];
    for src in &sources {
        let files: Vec<Session> = src
            .scan()
            .sessions
            .into_iter()
            .filter(|s| s.path.is_file())
            .collect();
        assert!(!files.is_empty(), "{}", src.vendor());
        for s in files {
            let first = src.count_since(&s, None).unwrap();
            assert_eq!(first.users, src.count_messages(&s).unwrap());
            let mut text = fs::read_to_string(&s.path).unwrap();
            if !text.ends_with('\n') {
                text.insert(0, '\n');
            }
            append(&s.path, &text);
            let next = src.count_since(&s, first.progress).unwrap();
            assert!(next.appended.is_some(), "{}：要接着数", s.path.display());
            assert_eq!(
                next.users,
                src.count_messages(&s).unwrap(),
                "{}",
                s.path.display()
            );
        }
    }
    fs::remove_dir_all(base).unwrap();
}
