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

fn work_file(home: &Path) -> PathBuf {
    home.join(".claude-work/projects/-Users-demo-shop")
        .join(format!("{ID}.jsonl"))
}

#[test]
fn bare_id_rescans_before_trusting_a_single_cached_hit() {
    let (home, _) = home("cli-export-stale", "个人的对话", None);
    // 先导一次：缓存里只有默认账号那条。
    let out = export(&home, &[ID]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    // 工作号里后来有了同一个 id：缓存里还是一条，也要报歧义，不能导出默认账号那份。
    let f = work_file(&home);
    fs::create_dir_all(f.parent().unwrap()).unwrap();
    fs::write(&f, session("公司的对话")).unwrap();
    let out = export(&home, &[ID]);
    let err = text(&out.stderr);
    assert!(!out.status.success(), "{}", text(&out.stdout));
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
    assert!(err.contains("不止一个账号"), "{err}");
}

#[test]
fn ambiguity_message_drops_control_chars_from_titles() {
    let (home, _) = home(
        "cli-export-title",
        r"\u001b]0;被改了\u0007个人\u009b2J",
        Some("公司的对话"),
    );
    let out = export(&home, &[ID]);
    let err = text(&out.stderr);
    assert!(err.contains("不止一个账号"), "{err}");
    assert!(err.contains("]0;被改了个人2J"), "{err:?}");
    assert!(!err.chars().any(|c| c.is_control() && c != '\n'), "{err:?}");
}

#[test]
fn output_inside_a_data_dir_is_refused() {
    let (home, _) = home("cli-export-datadir", "个人的对话", None);
    // Hermes 的库（`Session.path` 是空的，同文件检查管不到）、工作号的配置、会话目录里还没有的文件。
    let db = home.join(".hermes/state.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    fs::write(&db, "库").unwrap();
    let settings = home.join(".claude-work/settings.json");
    fs::write(&settings, "{}").unwrap();
    let fresh = home.join(".claude/projects/new.md");
    let link = home.join("db-link.md");
    std::os::unix::fs::symlink(&db, &link).unwrap();
    // 悬空的软链指进会话目录：不能跟过去建文件。
    let dangling = home.join("dangling.md");
    std::os::unix::fs::symlink(&fresh, &dangling).unwrap();
    for target in [&db, &settings, &fresh, &link, &dangling] {
        let out = export(&home, &[ID, "-o", target.to_str().unwrap()]);
        let err = text(&out.stderr);
        assert!(!out.status.success(), "{}", target.display());
        if target != &dangling {
            assert!(err.contains("在会话数据目录"), "{err}");
        }
    }
    // 只写文件名、当前目录在会话目录里。
    let out = ai(&home)
        .current_dir(fresh.parent().unwrap())
        .args(["export", ID, "-o", "new.md"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{}", text(&out.stderr));
    assert_eq!(fs::read_to_string(&db).unwrap(), "库");
    assert_eq!(fs::read_to_string(&settings).unwrap(), "{}");
    assert!(!fresh.exists());
}

#[test]
fn read_only_export_is_not_replaced() {
    use std::os::unix::fs::PermissionsExt;
    let (home, _) = home("cli-export-readonly", "个人的对话", None);
    let old = home.join("old.md");
    fs::write(&old, "旧导出").unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o444)).unwrap();
    let out = export(&home, &[ID, "-o", old.to_str().unwrap()]);
    assert!(!out.status.success(), "只读文件不能因为目录可写就被换掉");
    assert_eq!(fs::read_to_string(&old).unwrap(), "旧导出");
    // 还没有的文件直接新建。
    let fresh = home.join("fresh.md");
    let out = export(&home, &[ID, "-o", fresh.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        fs::read_to_string(&fresh)
            .unwrap()
            .starts_with("# 个人的对话")
    );
}

#[test]
fn named_pipe_is_written_in_place() {
    use std::os::unix::fs::FileTypeExt;
    let (home, _) = home("cli-export-fifo", "个人的对话", None);
    let fifo = home.join("pipe");
    let ok = Command::new("/usr/bin/mkfifo").arg(&fifo).status().unwrap();
    assert!(ok.success());
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = fifo.clone();
    // 换成普通文件的话没人往管道里写，这个线程一直等着：不 join，只按超时收。
    std::thread::spawn(move || {
        let _ = tx.send(fs::read_to_string(&reader));
    });
    let out = export(&home, &[ID, "-o", fifo.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo(),
        "命名管道不能被换成普通文件"
    );
    let got = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("管道那头没收到")
        .unwrap();
    assert!(got.starts_with("# 个人的对话"), "{got}");
}
