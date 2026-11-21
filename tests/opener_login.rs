//! 「开机自动打开会话台」的登录项：拼装各终端的启动参数、plist 内容、开关只动临时目录里的文件。
//! 不碰真的 ~/Library/LaunchAgents，不调 launchctl。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use huitai::opener::Terminal;
use huitai::opener::login::{self, LABEL, LoginItem};

const AI: &str = "/Users/demo/my tools/ai";
const HOME: &str = "/Users/demo";
const KAKU: &str = "/Applications/Kaku.app/Contents/MacOS/kaku";

fn args(t: &Terminal) -> std::io::Result<Vec<String>> {
    login::launch_args(
        t,
        Path::new(AI),
        "/bin/zsh",
        Path::new(HOME),
        Path::new(KAKU),
    )
}

#[test]
fn ghostty_types_ai_into_a_tab_without_command() {
    let a = args(&Terminal::Ghostty).unwrap();
    assert_eq!(a[0], "/usr/bin/osascript");
    assert_eq!(a[1], "-e");
    let script = &a[2];
    // 和开对话一样：不带 command / initial input（会弹执行确认框），输入后单独按回车。
    assert!(!script.contains("command of"));
    assert!(!script.contains("initial input"));
    assert!(script.contains("input text cmdLine"));
    assert!(script.contains("send key \"enter\""));
    // 路径带空格要加引号；开头空格不进 shell 历史；不 exec，退出会话台后留下 shell。
    assert_eq!(a[3], " '/Users/demo/my tools/ai'");
    assert_eq!(a[4], HOME);
    assert_eq!(a.len(), 5);
}

#[test]
fn kaku_starts_login_shell_running_ai() {
    let a = args(&Terminal::Kaku).unwrap();
    assert_eq!(
        a,
        [
            KAKU,
            "start",
            "--cwd",
            HOME,
            "--",
            "/bin/zsh",
            "-lc",
            "'/Users/demo/my tools/ai'; exec /bin/zsh -l"
        ]
    );
}

#[test]
fn other_terminals_are_refused() {
    let err = args(&Terminal::Other(Some("Apple_Terminal".to_owned()))).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    assert!(err.to_string().contains("Ghostty"));
}

#[test]
fn plist_runs_once_at_login_and_escapes_xml() {
    let p = login::plist(&["/bin/a".to_owned(), "x<y & \"z\"".to_owned()]);
    assert!(p.contains(&format!("<string>{LABEL}</string>")));
    assert!(p.contains("<string>x&lt;y &amp; &quot;z&quot;</string>"));
    assert!(p.contains("<key>RunAtLoad</key>\n\t<true/>"));
    assert!(p.contains("<string>Aqua</string>"));
    // 只在登录时跑一次，进程退出不重启。
    assert!(!p.contains("KeepAlive"));
}

#[test]
fn plist_is_valid_for_plutil() {
    let dir = std::env::temp_dir().join(format!("huitai-plist-{}", std::process::id()));
    let item = LoginItem::in_dir(&dir);
    item.enable(&args(&Terminal::Ghostty).unwrap()).unwrap();
    let out = Command::new("/usr/bin/plutil")
        .arg("-lint")
        .arg(item.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn toggle_only_touches_the_given_dir() {
    let dir = PathBuf::from(format!(
        "{}/huitai-agents-{}",
        std::env::temp_dir().display(),
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    let item = LoginItem::in_dir(&dir);
    assert_eq!(item.path(), dir.join(format!("{LABEL}.plist")));
    // 默认关：文件不存在。
    assert!(!item.is_enabled());
    item.disable().unwrap();
    item.enable(&["/bin/true".to_owned()]).unwrap();
    assert!(item.is_enabled());
    assert!(!dir.join(format!("{LABEL}.plist.tmp")).exists());
    item.enable(&["/bin/echo".to_owned()]).unwrap();
    assert!(
        fs::read_to_string(item.path())
            .unwrap()
            .contains("/bin/echo")
    );
    item.disable().unwrap();
    assert!(!item.is_enabled());
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    fs::remove_dir_all(&dir).unwrap();
}
