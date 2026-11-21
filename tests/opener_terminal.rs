//! opener：终端识别与各终端的命令拼装。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command as Proc;

use huitai::core::{SessionId, Vendor};
use huitai::opener::{
    GhosttyOpener, KakuOpener, Terminal, attach_command, ps_command_line, tab_title,
};

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |k| map.get(k).cloned()
}

#[test]
fn detects_ghostty_and_kaku_by_term_program() {
    // 2026-11-20 本机实测：Ghostty 1.4.0 设 ghostty，Kaku 0.30.0 设 Kaku。
    let ghostty = env(&[("TERM_PROGRAM", "ghostty"), ("TERM", "xterm-ghostty")]);
    assert_eq!(Terminal::detect(ghostty), Terminal::Ghostty);
    let kaku = env(&[("TERM_PROGRAM", "Kaku"), ("WEZTERM_PANE", "1")]);
    assert_eq!(Terminal::detect(kaku), Terminal::Kaku);
    assert_eq!(Terminal::from_name("GHOSTTY"), Terminal::Ghostty);
    assert_eq!(Terminal::from_name("kaku"), Terminal::Kaku);
}

#[test]
fn other_terminals_attach_in_current_window() {
    for program in ["Apple_Terminal", "iTerm.app", "WezTerm", "tmux"] {
        let t = Terminal::detect(env(&[("TERM_PROGRAM", program)]));
        assert_eq!(t, Terminal::Other(Some(program.to_owned())));
        assert!(!t.has_tabs());
        assert_eq!(t.to_string(), program);
    }
    assert_eq!(Terminal::detect(env(&[])), Terminal::Other(None));
    assert_eq!(
        Terminal::detect(env(&[("TERM_PROGRAM", "")])),
        Terminal::Other(None)
    );
    assert!(Terminal::Ghostty.has_tabs() && Terminal::Kaku.has_tabs());
}

#[test]
fn attach_command_runs_ai_attach_in_project_dir() {
    let cmd = attach_command(
        Path::new("/usr/local/bin/ai"),
        &SessionId::from("0b1c-22"),
        Path::new("/Users/me/proj"),
    );
    assert_eq!(cmd.program, "/usr/local/bin/ai");
    assert_eq!(cmd.args, ["attach", "0b1c-22"]);
    assert_eq!(cmd.cwd, PathBuf::from("/Users/me/proj"));
    assert_eq!(ps_command_line(&cmd), "/usr/local/bin/ai attach 0b1c-22");
}

#[test]
fn ghostty_opens_plain_shell_then_types_command() {
    let cmd = attach_command(
        Path::new("/Users/me/My Tools/ai"),
        &SessionId::from("s1"),
        Path::new("/Users/me/项目 \"x\"/it's here"),
    );
    let title = tab_title(Vendor::Claude, "it's \"会话台\"", Some("修 $HOME 的 bug"));
    let args = GhosttyOpener::open_args(&cmd, &title);
    assert_eq!(args.len(), 5);
    assert_eq!(args[0], "-e");
    let script = &args[1];
    // 不设 command / initial input：这两项会让 Ghostty 弹「Allow Ghostty to execute …」。
    assert!(!script.contains("command of cfg") && !script.contains("initial input"));
    assert!(script.contains("set initial working directory of cfg to workDir"));
    assert!(script.contains("new tab in front window with configuration cfg"));
    assert!(script.contains("set name override of t to tabTitle"));
    // 先输入不带换行的命令，再单独按回车；顺序不能反。
    let input = script.find("input text cmdLine to tm").unwrap();
    let enter = script.find(r#"send key "enter" to tm"#).unwrap();
    assert!(input < enter);
    assert!(script.contains("return id of tm"));
    // 值不进脚本文本，只作为 argv 传。
    for v in ["My Tools", "项目", "it's", "会话台", "$HOME"] {
        assert!(!script.contains(v), "{v} 不该出现在脚本里");
    }
    assert_eq!(args[2], r" exec '/Users/me/My Tools/ai' attach s1");
    assert!(!args[2].contains('\n') && !args[2].contains('\r'));
    assert_eq!(args[3], "/Users/me/项目 \"x\"/it's here");
    assert_eq!(args[4], "✻ it's \"会话台\" · 修 $HOME 的 bug");

    // 没标题时传空串，脚本里跳过 name override。
    assert_eq!(GhosttyOpener::open_args(&cmd, "")[4], "");
    assert!(script.contains(r#"if tabTitle is not "" then"#));

    let focus = GhosttyOpener::focus_args("ABCD-1234");
    assert!(focus[1].contains("focus terminal id (item 1 of argv)"));
    assert_eq!(focus[2], "ABCD-1234");
}

#[test]
fn tab_title_has_vendor_mark_project_and_title() {
    assert_eq!(
        tab_title(Vendor::Codex, "huitai", Some("修 opener")),
        "◎ huitai · 修 opener"
    );
    assert_eq!(tab_title(Vendor::Pi, "记账本", None), "π 记账本");
    assert_eq!(tab_title(Vendor::Claude, "x", Some("  ")), "✻ x");
}

/// 输进 shell 的那一行，在 sh、zsh、bash、fish（装了的话）里都要让参数原样到达。用 printf 代替 ai。
#[test]
fn ghostty_typed_line_round_trips_through_shells() {
    let cmd = huitai::sources::Command {
        program: "printf".to_owned(),
        args: vec![
            "[%s]".to_owned(),
            "a b".to_owned(),
            "it's".to_owned(),
            "$HOME".to_owned(),
            "中文 路径/\"双引号\"".to_owned(),
            "`x`;&|".to_owned(),
        ],
        cwd: PathBuf::from("/"),
    };
    let line = GhosttyOpener::typed_line(&cmd);
    assert!(line.starts_with(" exec "), "{line}");
    for shell in [
        "/bin/sh",
        "/bin/zsh",
        "/bin/bash",
        "/opt/homebrew/bin/fish",
        "/usr/local/bin/fish",
    ] {
        if !Path::new(shell).exists() {
            continue;
        }
        let out = Proc::new(shell).args(["-c", &line]).output().unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "[a b][it's][$HOME][中文 路径/\"双引号\"][`x`;&|]",
            "{shell}"
        );
    }
}

#[test]
fn kaku_spawn_passes_program_and_args_directly() {
    let cmd = attach_command(
        Path::new("/Users/me/My Tools/ai"),
        &SessionId::from("s2"),
        Path::new("/Users/me/proj x"),
    );
    assert_eq!(
        KakuOpener::spawn_args(&cmd),
        [
            "cli",
            "--no-auto-start",
            "spawn",
            "--cwd",
            "/Users/me/proj x",
            "--",
            "/Users/me/My Tools/ai",
            "attach",
            "s2"
        ]
    );
    assert_eq!(
        KakuOpener::list_args(),
        ["cli", "--no-auto-start", "list", "--format", "json"]
    );
    assert_eq!(
        KakuOpener::activate_args(7),
        ["cli", "--no-auto-start", "activate-pane", "--pane-id", "7"]
    );
}

#[test]
fn kaku_cli_comes_from_wezterm_executable() {
    let inside = env(&[("WEZTERM_EXECUTABLE", "/Opt/Kaku.app/Contents/MacOS/kaku")]);
    assert_eq!(
        KakuOpener::new(inside).cli(),
        Path::new("/Opt/Kaku.app/Contents/MacOS/kaku")
    );
    // 真 WezTerm 的 WEZTERM_EXECUTABLE 不是 kaku，不能拿来用。
    let wezterm = env(&[(
        "WEZTERM_EXECUTABLE",
        "/Applications/WezTerm.app/Contents/MacOS/wezterm-gui",
    )]);
    assert_eq!(
        KakuOpener::new(wezterm).cli(),
        Path::new("/Applications/Kaku.app/Contents/MacOS/kaku")
    );
    assert_eq!(
        KakuOpener::new(env(&[])).cli(),
        Path::new("/Applications/Kaku.app/Contents/MacOS/kaku")
    );
}

#[test]
fn parses_kaku_list_output() {
    // 字段取自 2026-11-20 本机 kaku cli list --format json 的输出（只保留结构）。
    let json = r#"[
      {"window_id":0,"tab_id":0,"pane_id":0,"workspace":"default",
       "size":{"rows":22,"cols":110,"pixel_width":1870,"pixel_height":1056,"dpi":144},
       "title":"kaku","cwd":"file:///tmp/","cursor_x":2,"cursor_y":0,
       "is_active":true,"is_zoomed":false,"tty_name":"/dev/ttys005","program_status":null},
      {"window_id":0,"tab_id":1,"pane_id":3,"title":"x"}
    ]"#;
    let panes = KakuOpener::parse_list(json).unwrap();
    assert_eq!(panes.len(), 2);
    assert_eq!(panes[0].pane_id, 0);
    assert_eq!(panes[0].tty_name.as_deref(), Some("/dev/ttys005"));
    assert_eq!(panes[1].tty_name, None);
    assert!(KakuOpener::parse_list("not json").is_err());
    assert!(KakuOpener::parse_list("[]").unwrap().is_empty());
}
