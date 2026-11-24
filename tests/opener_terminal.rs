//! opener：终端识别与各终端的命令拼装。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command as Proc;

use huitai::core::{SessionId, Vendor};
use huitai::opener::{
    CmuxOpener, GhosttyOpener, Iterm2Opener, KakuOpener, Terminal, attach_command, ps_command_line,
    tab_title,
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
fn detects_cmux_even_though_it_says_ghostty() {
    // 2026-11-22 本机实测 cmux 0.65.2：TERM_PROGRAM=ghostty，另设 CMUX_SOCKET_PATH、CMUX_SURFACE_ID 等。
    let cmux = env(&[
        ("TERM_PROGRAM", "ghostty"),
        ("CMUX_SOCKET_PATH", "/Users/me/.local/state/cmux/cmux.sock"),
        ("CMUX_SURFACE_ID", "ABC"),
    ]);
    assert_eq!(Terminal::detect(cmux), Terminal::Cmux);
    let surface_only = env(&[("TERM_PROGRAM", "ghostty"), ("CMUX_SURFACE_ID", "ABC")]);
    assert_eq!(Terminal::detect(surface_only), Terminal::Cmux);
    // 只有 socket 路径（别的终端里为用 cmux 命令行设的）不算在 cmux 里：认终端自己（审计第 23 条）。
    for (p, want) in [("Kaku", Terminal::Kaku), ("ghostty", Terminal::Ghostty)] {
        let pairs = [("TERM_PROGRAM", p), ("CMUX_SOCKET_PATH", "/tmp/cmux.sock")];
        assert_eq!(Terminal::detect(env(&pairs)), want);
    }
    // 空值不算；cmux 里再开 tmux，新标签页会落到 tmux 外面，仍按其他终端处理。
    let empty = env(&[("TERM_PROGRAM", "ghostty"), ("CMUX_SURFACE_ID", "")]);
    assert_eq!(Terminal::detect(empty), Terminal::Ghostty);
    let tmux = env(&[("TERM_PROGRAM", "tmux"), ("CMUX_SURFACE_ID", "/x")]);
    assert_eq!(
        Terminal::detect(tmux),
        Terminal::Other(Some("tmux".to_owned()))
    );

    assert_eq!(Terminal::from_name("cmux"), Terminal::Cmux);
    assert_eq!(Terminal::Cmux.name().as_deref(), Some("cmux"));
    assert_eq!(
        Terminal::from_name(&Terminal::Ghostty.name().unwrap()),
        Terminal::Ghostty
    );
    assert!(Terminal::Cmux.has_tabs());
    // 登录项、呼出键从 cmux 外面驱动，cmux 不接受。
    assert!(!Terminal::Cmux.drivable_from_outside());
    assert!(Terminal::Ghostty.drivable_from_outside() && Terminal::Kaku.drivable_from_outside());
}

#[test]
fn cmux_opens_surface_and_types_exec_line() {
    let cmd = attach_command(
        Path::new("/Users/me/My Tools/ai"),
        &SessionId::from("s3"),
        Path::new("/Users/me/proj x"),
    );
    assert_eq!(
        CmuxOpener::open_args(&cmd),
        [
            "--json",
            "--id-format",
            "uuids",
            "new-surface",
            "--type",
            "terminal",
            "--working-directory",
            "/Users/me/proj x",
            "--command",
            " exec '/Users/me/My Tools/ai' attach s3",
            "--focus",
            "true"
        ]
    );
    // 聚焦只给 UUID：cmux 自己找它在哪个工作区。
    assert_eq!(CmuxOpener::focus_args("S"), ["focus-panel", "--panel", "S"]);
}

#[test]
fn cmux_cli_comes_from_bundled_path() {
    let inside = env(&[(
        "CMUX_BUNDLED_CLI_PATH",
        "/Opt/cmux.app/Contents/Resources/bin/cmux",
    )]);
    assert_eq!(
        CmuxOpener::new(inside).cli(),
        Path::new("/Opt/cmux.app/Contents/Resources/bin/cmux")
    );
    assert_eq!(
        CmuxOpener::new(env(&[])).cli(),
        Path::new("/Applications/cmux.app/Contents/Resources/bin/cmux")
    );
}

#[test]
fn parses_cmux_new_surface_json() {
    // 字段按 cmux v0.65.2 源码（surface.create 的返回，uuids 模式只留 *_id 和 type）。
    let out = r#"{"pane_id":"P","surface_id":"S-1","type":"terminal","window_id":"W","workspace_id":"WS"}
"#;
    assert_eq!(
        CmuxOpener::parse_created(out).unwrap(),
        ("S-1".to_owned(), "WS".to_owned())
    );
    assert_eq!(
        CmuxOpener::parse_created(r#"{"surface_id":"S"}"#).unwrap(),
        ("S".to_owned(), String::new())
    );
    // remote tmux 工作区之类不带 surface 的回答、文本输出都算失败，不瞎记。
    assert!(CmuxOpener::parse_created(r#"{"surface_id":""}"#).is_err());
    assert!(CmuxOpener::parse_created("OK surface:5 pane:2 workspace:1").is_err());
    assert!(CmuxOpener::parse_created("").is_err());
}

#[test]
fn detects_iterm2() {
    // iTerm2 3.7.9 源码 PTYSession.m：TERM_PROGRAM=iTerm.app，另设 ITERM_SESSION_ID。
    let iterm = env(&[
        ("TERM_PROGRAM", "iTerm.app"),
        ("ITERM_SESSION_ID", "w0t0p0:U"),
    ]);
    assert_eq!(Terminal::detect(iterm), Terminal::Iterm2);
    assert_eq!(Terminal::Iterm2.to_string(), "iTerm2");
    // 登记给守护进程的名字和旧版一样，认得回来。
    assert_eq!(Terminal::Iterm2.name().as_deref(), Some("iTerm.app"));
    assert_eq!(Terminal::from_name("iTerm.app"), Terminal::Iterm2);
    assert!(Terminal::Iterm2.has_tabs());
    // 开机打开、呼出键还没做。
    assert!(!Terminal::Iterm2.drivable_from_outside());
    // iTerm2 里再开 tmux：新标签页会落到 tmux 外面，仍在当前窗口打开。
    let tmux = env(&[
        ("TERM_PROGRAM", "tmux"),
        ("ITERM_SESSION_ID", "w0t0p0:U"),
        ("LC_TERMINAL", "iTerm2"),
    ]);
    assert!(!Terminal::detect(tmux).has_tabs());
}

#[test]
fn iterm2_types_cd_and_exec_into_a_new_tab() {
    let cmd = attach_command(
        Path::new("/Users/me/My Tools/ai"),
        &SessionId::from("s4"),
        Path::new("/Users/me/项目 \"x\"/it's here"),
    );
    let args = Iterm2Opener::open_args(&cmd);
    assert_eq!(args.len(), 3);
    assert_eq!(args[0], "-e");
    let script = &args[1];
    // 值只作为 argv 传；命令是打进去的，不用 command 参数（不经 shell、会被 iTerm2 当插值字符串求值）。
    for v in ["My Tools", "项目", "it's", "s4"] {
        assert!(!script.contains(v), "{v} 不该出现在脚本里");
    }
    assert!(!script.contains(" command "));
    // 打字之前先拿到 id、切到前面：之后没有会失败的步骤，出错就是没打进去。
    let id = script.find("set sid to id of s").unwrap();
    let write = script.find("tell s to write text cmdLine").unwrap();
    assert!(id < write);
    assert!(script[write..].contains("return sid"));
    assert_eq!(
        args[2],
        r#" cd '/Users/me/项目 "x"/it'\''s here'; exec '/Users/me/My Tools/ai' attach s4"#
    );

    for args in [
        Iterm2Opener::find_args("U-1"),
        Iterm2Opener::focus_args("U-1"),
    ] {
        assert_eq!(args[0], "-e");
        assert!(args[1].contains("id of s is (item 1 of argv)"));
        assert_eq!(args[2], "U-1");
    }
}

/// 打进 shell 的那一行：进到工作目录、参数原样到达；目录没了也照样接上。
#[test]
fn iterm2_typed_line_round_trips_through_shells() {
    let dir = std::env::temp_dir().join(format!(
        "huitai-iterm2-{} it's \"项目\"",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let real = std::fs::canonicalize(&dir).unwrap();
    let cmd = |cwd: &Path| huitai::sources::Command {
        program: "/bin/sh".to_owned(),
        args: [
            "-c",
            "pwd -P; printf '[%s]' \"$@\"",
            "sh",
            "a b",
            "it's $HOME",
        ]
        .map(str::to_owned)
        .to_vec(),
        cwd: cwd.to_path_buf(),
    };
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
        let run = |cwd: &Path| {
            let line = Iterm2Opener::typed_line(&cmd(cwd));
            assert!(line.starts_with(' '), "{line}");
            let out = Proc::new(shell)
                .args(["-c", &line])
                .current_dir("/")
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(
            run(&real),
            format!("{}\n[a b][it's $HOME]", real.display()),
            "{shell}"
        );
        assert_eq!(run(&dir.join("没了")), "/\n[a b][it's $HOME]", "{shell}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn iterm2_only_switches_to_a_session_still_running_our_attach() {
    let cmd = "/bin/ai attach s4";
    let ours = |tty: &str, opened_at: u64, now: u64, running: &str| {
        Iterm2Opener::still_ours(tty, cmd, opened_at, now, |t, c| {
            assert_eq!((t, c), (tty, cmd));
            Ok(c == running)
        })
        .unwrap()
    };
    assert!(ours("/dev/ttys003", 100, 9_000, cmd));
    // 会话关了。
    assert!(!ours("", 100, 101, cmd));
    // 退出 iTerm2 再恢复窗口：id 沿用，里面是新 shell。
    assert!(!ours("/dev/ttys003", 100, 9_000, "-zsh"));
    // 刚开、shell 还没换成 attach：算我们的，连按两次回车不开第二个。
    assert!(ours("/dev/ttys003", 100, 105, "-zsh"));
    assert!(!ours("/dev/ttys003", 100, 110, "-zsh"));
    // 时钟往回拨过：不当作刚开。
    assert!(!ours("/dev/ttys003", 100, 50, "-zsh"));
    // 查不了就报错，不瞎切。
    assert!(
        Iterm2Opener::still_ours("/dev/ttys003", cmd, 0, 9_000, |_, _| {
            Err(std::io::Error::other("ps 超时"))
        })
        .is_err()
    );
}

#[test]
fn other_terminals_attach_in_current_window() {
    for program in ["Apple_Terminal", "WezTerm", "tmux"] {
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
