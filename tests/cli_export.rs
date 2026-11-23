//! `ai export` 端到端：假家目录里两个 Claude 账号（默认 `~/.claude` 和 `CLAUDE_CONFIG_DIR` 指的工作号）
//! 放同一个会话 id、不同正文，跑真二进制。样例都是手写的。

mod sources_common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sources_common::temp_dir;

const ID: &str = "77777777-7777-4777-8777-777777777777";

fn session(text: &str) -> String {
    [
        format!(
            r#"{{"type":"user","cwd":"/Users/demo/shop","timestamp":"2026-11-20T02:00:00Z","message":{{"role":"user","content":"{text}"}}}}"#
        ),
        r#"{"type":"assistant","cwd":"/Users/demo/shop","timestamp":"2026-11-20T02:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"收到。"}]}}"#.to_owned(),
    ]
    .join("\n")
        + "\n"
}

/// 假家目录；返回（家目录，默认账号的会话文件）。`work` 是 `None` 时工作号里没有这个 id。
fn home(name: &str, personal: &str, work: Option<&str>) -> (PathBuf, PathBuf) {
    let home = temp_dir(name);
    fs::create_dir_all(home.join(".claude-work/projects")).unwrap();
    let mut files = Vec::new();
    let both = [(".claude", Some(personal)), (".claude-work", work)];
    for (dir, text) in both.into_iter().filter_map(|(d, t)| Some((d, t?))) {
        let f = home
            .join(dir)
            .join("projects/-Users-demo-shop")
            .join(format!("{ID}.jsonl"));
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(&f, session(text)).unwrap();
        files.push(f);
    }
    (home, files.swap_remove(0))
}

fn ai(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ai"));
    c.env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUDE_CONFIG_DIR", home.join(".claude-work"))
        .current_dir(home);
    c
}

fn export(home: &Path, args: &[&str]) -> Output {
    ai(home).arg("export").args(args).output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn native_id_in_two_accounts_lists_prefixed_ids() {
    let (home, _) = home("cli-export-ambiguous", "个人的对话", Some("公司的对话"));
    let out = export(&home, &[ID]);
    let err = text(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(
        out.stdout.is_empty(),
        "歧义时不导出任何一份：{}",
        text(&out.stdout)
    );
    assert!(err.contains("不止一个账号"), "{err}");
    // 列出的写法各自导出对的那份。
    let listed: Vec<&str> = err
        .lines()
        .filter_map(|l| l.trim().split_once('（').map(|(id, _)| id))
        .filter(|id| id.ends_with(ID))
        .collect();
    assert_eq!(listed.len(), 2, "{err}");
    let personal = format!("claude:{ID}");
    assert!(listed.contains(&personal.as_str()), "{err}");
    let work = listed.iter().find(|id| **id != personal).unwrap();
    let md = text(&export(&home, &[&personal]).stdout);
    assert!(
        md.contains("个人的对话") && !md.contains("公司的对话"),
        "{md}"
    );
    let md = text(&export(&home, &[work]).stdout);
    assert!(
        md.contains("公司的对话") && !md.contains("个人的对话"),
        "{md}"
    );
}

#[test]
fn output_that_is_the_session_file_is_refused() {
    let (home, personal) = home("cli-export-same", "个人的对话", None);
    let before = fs::read(&personal).unwrap();
    let link = home.join("link.md");
    std::os::unix::fs::symlink(&personal, &link).unwrap();
    let id = ID;
    for target in [&personal, &link] {
        let out = export(&home, &[id, "-o", target.to_str().unwrap()]);
        let err = text(&out.stderr);
        assert!(!out.status.success(), "{err}");
        assert!(err.contains("就是这条对话的会话文件"), "{err}");
        assert_eq!(fs::read(&personal).unwrap(), before, "会话文件不能被改");
    }
    // 普通的旧导出整份换掉，目录里不留临时文件。
    let old = home.join("old.md");
    fs::write(&old, "旧导出".repeat(1000)).unwrap();
    let out = export(&home, &[id, "-o", old.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let md = fs::read_to_string(&old).unwrap();
    assert!(
        md.starts_with("# 个人的对话") && !md.contains("旧导出"),
        "{md}"
    );
    let names: Vec<String> = fs::read_dir(&home)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(names.is_empty(), "{names:?}");
}

#[test]
fn terminal_output_escapes_controls_pipe_output_is_raw() {
    let (home, _) = home("cli-export-tty", r"\u001b]0;被改了\u0007正文\t缩进", None);
    let id = ID;
    let piped = text(&export(&home, &[id]).stdout);
    assert!(piped.contains("\x1b]0;被改了\x07正文\t缩进"), "{piped:?}");
    // script 给命令一个伪终端：标准输出是终端。
    let out = Command::new("/usr/bin/script")
        .args(["-q", "/dev/null", env!("CARGO_BIN_EXE_ai"), "export", id])
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUDE_CONFIG_DIR", home.join(".claude-work"))
        .current_dir(&home)
        .output()
        .unwrap();
    let tty = text(&out.stdout);
    assert!(out.status.success(), "{tty:?}");
    assert!(tty.contains("^[]0;被改了^G正文\t缩进"), "{tty:?}");
    assert!(!tty.contains('\x1b') && !tty.contains('\x07'), "{tty:?}");
}
