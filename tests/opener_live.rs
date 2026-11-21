//! 本机实测：在当前终端（Ghostty 或 Kaku）里开两个新标签页，再打开第一条，确认是聚焦而不是开第三个；
//! 关掉它再打开，确认会重开。会在真终端里开标签页，结束时关掉自己开的。
//! 必须在 Ghostty / Kaku 的标签页里、终端窗口在前台时手动跑：
//! `cargo test --test opener_live -- --ignored --nocapture`
//!
//! `ai attach` 还没接上托管层，标签页里跑一个替身：打开终端的焦点上报（`ESC[?1004h`），
//! 把收到的字节记进文件。终端切到这个标签页时会发 `ESC[I`、切走时发 `ESC[O`，以此判断谁在前台。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Proc;
use std::thread;
use std::time::{Duration, Instant};

use huitai::core::SessionId;
use huitai::opener::{Opener, Outcome, TabHandle, Tabs, Terminal, opener_for};
use huitai::sources::Command;

const FOCUS_IN: &[u8] = b"\x1b[I";
const FOCUS_OUT: &[u8] = b"\x1b[O";

fn stand_in(dir: &Path, id: &str) -> Command {
    let log = dir.join(format!("focus-{id}.log"));
    Command {
        program: "/bin/sh".to_owned(),
        args: vec![
            "-c".to_owned(),
            format!(
                "printf '\\033[?1004h'; stty raw -echo; cat > '{}'",
                log.display()
            ),
        ],
        cwd: dir.to_path_buf(),
    }
}

/// 这个替身最近一次收到的焦点事件：`Some(true)` 获得焦点，`Some(false)` 失去焦点，`None` 还没收到过。
/// 新开的标签页在替身打开焦点上报之前就已经在前台，收不到 `ESC[I`，所以「刚开的在前台」看别的标签页收到 `ESC[O`。
fn focus(dir: &Path, id: &str) -> Option<bool> {
    let bytes = fs::read(dir.join(format!("focus-{id}.log"))).unwrap_or_default();
    let last = |pat: &[u8]| bytes.windows(pat.len()).rposition(|w| w == pat);
    match (last(FOCUS_IN), last(FOCUS_OUT)) {
        (Some(i), Some(o)) => Some(i > o),
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
        (None, None) => None,
    }
}

fn raw(dir: &Path, id: &str) -> String {
    String::from_utf8_lossy(&fs::read(dir.join(format!("focus-{id}.log"))).unwrap_or_default())
        .escape_debug()
        .to_string()
}

fn kaku_cli() -> String {
    std::env::var("WEZTERM_EXECUTABLE")
        .unwrap_or_else(|_| "/Applications/Kaku.app/Contents/MacOS/kaku".to_owned())
}

fn tab_count(terminal: &Terminal) -> usize {
    match terminal {
        Terminal::Ghostty => {
            let out = Proc::new("/usr/bin/osascript")
                .args(["-e", r#"tell application "Ghostty" to count terminals"#])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
        }
        _ => {
            let out = Proc::new(kaku_cli())
                .args(["cli", "--no-auto-start", "list", "--format", "json"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout)
                .matches("\"pane_id\"")
                .count()
        }
    }
}

fn close(handle: &TabHandle) {
    match handle {
        TabHandle::Ghostty { terminal_id } => {
            let script =
                format!(r#"tell application "Ghostty" to close terminal id "{terminal_id}""#);
            let _ = Proc::new("/usr/bin/osascript")
                .args(["-e", &script])
                .output();
        }
        TabHandle::Kaku { pane_id, .. } => {
            let _ = Proc::new(kaku_cli())
                .args([
                    "cli",
                    "--no-auto-start",
                    "kill-pane",
                    "--pane-id",
                    &pane_id.to_string(),
                ])
                .output();
        }
    }
}

/// 测试结束（包括断言失败）时关掉开过的标签页、删临时目录。
struct Cleanup {
    dir: PathBuf,
    handles: Vec<TabHandle>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for h in &self.handles {
            close(h);
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn settle() {
    thread::sleep(Duration::from_millis(800));
}

#[test]
#[ignore = "在真终端里开标签页，手动在 Ghostty / Kaku 里跑"]
fn open_then_focus_existing_tab() {
    let terminal = Terminal::current();
    assert!(
        terminal.has_tabs(),
        "要在 Ghostty 或 Kaku 里跑，当前是 {terminal}"
    );
    println!("终端：{terminal}");
    let opener: Box<dyn Opener> = opener_for(&terminal, |k| std::env::var(k).ok());
    let dir = std::env::temp_dir().join(format!("huitai-opener-live-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let mut cleanup = Cleanup {
        dir: dir.clone(),
        handles: Vec::new(),
    };
    let mut tabs = Tabs::load(&dir.join("tabs.json")).unwrap();
    let (a, b) = (SessionId::from("a1"), SessionId::from("b2"));
    let before = tab_count(&terminal);

    let t = Instant::now();
    let first = tabs
        .open_or_focus(
            opener.as_ref(),
            &a,
            &stand_in(&dir, "a1"),
            "✻ 会话台 实测 · a1",
        )
        .unwrap();
    println!("开新标签页 a1：{first:?}，{:?}", t.elapsed());
    cleanup.handles.push(tabs.records()[&a].handle.clone());
    settle();
    let second = tabs
        .open_or_focus(
            opener.as_ref(),
            &b,
            &stand_in(&dir, "b2"),
            "◎ 会话台 实测 · b2",
        )
        .unwrap();
    cleanup.handles.push(tabs.records()[&b].handle.clone());
    settle();
    println!(
        "a1 → {:?}\nb2 → {:?}",
        tabs.records()[&a].handle,
        tabs.records()[&b].handle
    );
    assert_eq!((first, second), (Outcome::Opened, Outcome::Opened));
    assert_eq!(tab_count(&terminal), before + 2, "应多出两个标签页");
    assert!(
        focus(&dir, "a1") == Some(false),
        "开 b2 时 a1 应失去焦点；焦点记录 a1={:?} b2={:?}",
        raw(&dir, "a1"),
        raw(&dir, "b2")
    );

    let t = Instant::now();
    let again = tabs
        .open_or_focus(
            opener.as_ref(),
            &a,
            &stand_in(&dir, "a1"),
            "✻ 会话台 实测 · a1",
        )
        .unwrap();
    println!("再次打开 a1：{again:?}，{:?}", t.elapsed());
    settle();
    assert_eq!(again, Outcome::Focused);
    assert_eq!(tab_count(&terminal), before + 2, "聚焦不应多开标签页");
    assert!(
        focus(&dir, "a1") == Some(true) && focus(&dir, "b2") == Some(false),
        "a1 应被切到前台、b2 退到后面"
    );

    // 关掉 a1 的标签页后再打开：应开新的。
    close(&tabs.records()[&a].handle);
    settle();
    let reopen = tabs
        .open_or_focus(
            opener.as_ref(),
            &a,
            &stand_in(&dir, "a1"),
            "✻ 会话台 实测 · a1",
        )
        .unwrap();
    cleanup.handles.push(tabs.records()[&a].handle.clone());
    println!("关掉 a1 后再打开：{reopen:?}");
    settle();
    assert_eq!(reopen, Outcome::Opened);
    assert_eq!(tab_count(&terminal), before + 2);

    // 记录指向的东西已经不是这条对话（Ghostty 终端没了；Kaku 重启后同号窗格跑的是别的程序）：不能聚焦。
    let stale = match &terminal {
        Terminal::Ghostty => TabHandle::Ghostty {
            terminal_id: "00000000-0000-0000-0000-000000000000".to_owned(),
        },
        _ => {
            let TabHandle::Kaku { pane_id, tty, .. } = tabs.records()[&b].handle.clone() else {
                unreachable!()
            };
            TabHandle::Kaku {
                pane_id,
                tty,
                command: "/usr/local/bin/ai attach someone-else".to_owned(),
            }
        }
    };
    assert!(!opener.focus(&stale).unwrap(), "不该聚焦到别人的标签页");
    println!("通过：开新标签页、聚焦已开标签页、关掉后重开、不认错标签页");
}
