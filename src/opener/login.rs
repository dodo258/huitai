//! 「开机自动打开会话台」：在 `~/Library/LaunchAgents` 放一个登录时跑一次的 plist，
//! 它只负责让首选终端开一个窗口、在里面跑 `ai`。会话台和守护进程仍从终端里拉起，权限归属不变；
//! 不自动发「继续」，由用户在会话台里按 R。
//!
//! 开关状态就是这个文件在不在，不另记设置。只写文件、不调 `launchctl`：下次登录时系统自己加载。

use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use super::Terminal;
use crate::sources::Command;

/// launchd 任务名，也是 plist 文件名。
pub const LABEL: &str = "huitai.board";

/// 登录时 Ghostty 那边跑的脚本：Ghostty 没开着就等它自己开出第一个窗口，直接用那个标签页；
/// 开着就新开一个标签页。和开对话一样只输入命令行再按回车，不用 `command`（会弹执行确认框）。
const GHOSTTY_SCRIPT: &str = r#"on run argv
	set {cmdLine, workDir} to {item 1 of argv, item 2 of argv}
	set wasRunning to application "Ghostty" is running
	tell application "Ghostty"
		activate
		if not wasRunning then
			repeat 50 times
				if (count of windows) > 0 then exit repeat
				delay 0.1
			end repeat
		end if
		set cfg to new surface configuration
		set initial working directory of cfg to workDir
		if (count of windows) is 0 then
			set t to selected tab of (new window with configuration cfg)
		else if wasRunning then
			set t to new tab in front window with configuration cfg
		else
			set t to selected tab of front window
		end if
		set name override of t to "会话台"
		set tm to focused terminal of t
		input text cmdLine to tm
		send key "enter" to tm
	end tell
end run"#;

/// 登录项要跑的程序和参数。`ai` 是会话台的绝对路径，`shell` 是用户的登录 shell，`kaku` 是 Kaku 命令行工具。
/// 认不出能开窗口的终端时返回 `Unsupported`。
pub fn launch_args(
    terminal: &Terminal,
    ai: &Path,
    shell: &str,
    home: &Path,
    kaku: &Path,
) -> io::Result<Vec<String>> {
    let board = Command {
        program: ai.to_string_lossy().into_owned(),
        args: Vec::new(),
        cwd: home.to_path_buf(),
    }
    .shell_line();
    let home = home.to_string_lossy().into_owned();
    match terminal {
        // 开头空格：开了 HIST_IGNORE_SPACE / ignorespace 时不进历史。不用 exec：按 q 退出会话台后留下 shell。
        Terminal::Ghostty => Ok(vec![
            "/usr/bin/osascript".to_owned(),
            "-e".to_owned(),
            GHOSTTY_SCRIPT.to_owned(),
            format!(" {board}"),
            home,
        ]),
        // kaku start 不经 shell：用登录 shell 跑 ai（PATH 等和平时一样），退出会话台后换成交互 shell。
        Terminal::Kaku => {
            let shell_q = Command {
                program: shell.to_owned(),
                args: Vec::new(),
                cwd: PathBuf::new(),
            }
            .shell_line();
            Ok(vec![
                kaku.to_string_lossy().into_owned(),
                "start".to_owned(),
                "--cwd".to_owned(),
                home,
                "--".to_owned(),
                shell.to_owned(),
                "-lc".to_owned(),
                format!("{board}; exec {shell_q} -l"),
            ])
        }
        Terminal::Other(_) => Err(io::Error::new(
            ErrorKind::Unsupported,
            format!("{terminal} 不支持开机自动打开，先在设置里把首选终端选成 Ghostty 或 Kaku"),
        )),
    }
}

/// plist 正文：登录时跑一次 `args`，只在图形登录会话里跑。
pub fn plist(args: &[String]) -> String {
    let mut s = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
        "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<plist version=\"1.0\">\n<dict>\n",
    ));
    s.push_str(&format!(
        "\t<key>Label</key>\n\t<string>{}</string>\n",
        xml_escape(LABEL)
    ));
    s.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for a in args {
        s.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(a)));
    }
    s.push_str("\t</array>\n");
    s.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");
    s.push_str("\t<key>LimitLoadToSessionType</key>\n\t<string>Aqua</string>\n");
    s.push_str("</dict>\n</plist>\n");
    s
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 登录项文件。测试里指到临时目录，不碰真的 `~/Library/LaunchAgents`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginItem {
    path: PathBuf,
}

impl LoginItem {
    /// `dir` 下的 `<LABEL>.plist`。
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join(format!("{LABEL}.plist")),
        }
    }

    /// 真的位置：`~/Library/LaunchAgents`。取不到 HOME 时为 `None`。
    pub fn default_location() -> Option<Self> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
        Some(Self::in_dir(
            &PathBuf::from(home).join("Library/LaunchAgents"),
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_enabled(&self) -> bool {
        self.path.is_file()
    }

    /// 写登录项（已有就覆盖）。先写临时文件再改名，不留半个文件。
    pub fn enable(&self, args: &[String]) -> io::Result<()> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("登录项路径没有上级目录"))?;
        fs::create_dir_all(dir).map_err(|e| with_path(e, dir))?;
        let tmp = self.path.with_extension("plist.tmp");
        let mut f = fs::File::create(&tmp).map_err(|e| with_path(e, &tmp))?;
        f.write_all(plist(args).as_bytes())
            .map_err(|e| with_path(e, &tmp))?;
        f.sync_all().map_err(|e| with_path(e, &tmp))?;
        fs::rename(&tmp, &self.path).map_err(|e| with_path(e, &self.path))
    }

    /// 删掉登录项；本来就没有也算成功。
    pub fn disable(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(with_path(e, &self.path)),
            _ => Ok(()),
        }
    }
}

fn with_path(e: io::Error, path: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}
