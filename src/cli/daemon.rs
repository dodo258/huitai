//! `ai daemon`：托管层（host）之外，把通知和全局呼出键接上。
//!
//! - 提醒：订阅自己的托管事件，喂一份 `Notifier`（全机只有这一份，会话台只算状态灯），按 `next_due` 定时取；
//!   会话台开着且认 OSC 9/777 就交给它往自己标签页写，否则发系统通知。
//! - 热键：主线程注册 `Config::hotkey` 并跑 Carbon 事件循环（热键事件只投到主线程），socket 接收循环在别的线程。
//!
//! 环境变量（测试和排查用，一律关掉会打扰人的东西）：
//! - `HUITAI_NO_HOTKEY` 非空：不注册全局呼出键。
//! - `HUITAI_NOTIFY_TO=<文件>`：系统通知不弹，改成往这个文件追加一行「标题<TAB>正文」。

use std::fs::OpenOptions;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::Instant;

use crate::core::{Config, SessionId};
use crate::host::{Daemon, DaemonHandle, HostEvent};
use crate::hotkey::{self, HotKey};
use crate::notify::{Desktop, Message, Notifier, SystemNotifier};
use crate::opener;

use super::summon;

pub const NO_HOTKEY: &str = "HUITAI_NO_HOTKEY";
pub const NOTIFY_TO: &str = "HUITAI_NOTIFY_TO";

/// 从环境变量读出的开关。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Switches {
    pub hotkey: bool,
    /// 系统通知改写进这个文件。
    pub notify_to: Option<PathBuf>,
}

impl Switches {
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Self {
        let set = |k| env(k).filter(|v| !v.is_empty());
        Self {
            hotkey: set(NO_HOTKEY).is_none(),
            notify_to: set(NOTIFY_TO).map(PathBuf::from),
        }
    }
}

/// 把系统通知写进文件的假投递。
pub struct FileDesktop(pub PathBuf);

impl Desktop for FileDesktop {
    fn show(&self, msg: &Message) -> io::Result<()> {
        let mut f = OpenOptions::new().create(true).append(true).open(&self.0)?;
        writeln!(f, "{}\t{}", msg.title, msg.body)
    }
}

/// 关掉从父进程继承来的、0/1/2 以外的描述符。守护进程只该有标准输入、输出、错误三个；
/// macOS 上 Rust 建管道和给它标「执行时关闭」是两步，父进程里别的线程刚好在这中间 fork 出我们时，
/// 那根管道的写端会漏进来。守护进程常驻，漏进来的写端永远不关，等这根管道结束的那一方
/// （正在拉起子进程的线程、读 `ps` 输出的线程）就一直卡住。启动第一件事把它们关掉。
fn drop_inherited_fds() {
    let fds: Vec<i32> = match std::fs::read_dir("/dev/fd") {
        Ok(dir) => dir
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect(),
        Err(_) => return,
    };
    // 读目录用的那个描述符在 collect 完已经关了，下面再关一次只会得到 EBADF。
    for fd in fds.into_iter().filter(|&fd| fd > 2) {
        // SAFETY: 进程刚启动，自己还没打开任何要用的描述符，这里关的都是继承来的。
        unsafe {
            libc::close(fd);
        }
    }
}

/// `ai daemon` 的入口：已有守护进程在跑就直接返回。
pub fn run(dir: &Path) -> io::Result<()> {
    drop_inherited_fds();
    let daemon = match Daemon::bind(dir) {
        Ok(d) => d,
        Err(e) if e.kind() == ErrorKind::AddrInUse => return Ok(()),
        Err(e) => return Err(e),
    };
    let switches = Switches::from_env(|k| std::env::var(k).ok());
    let handle = daemon.handle();

    let desktop: Box<dyn Desktop + Send> = match &switches.notify_to {
        Some(path) => {
            eprintln!(
                "huitai daemon: 系统通知改写进 {}（{NOTIFY_TO}）",
                path.display()
            );
            Box::new(FileDesktop(path.clone()))
        }
        None => Box::new(SystemNotifier),
    };
    let events = handle.subscribe();
    {
        let handle = handle.clone();
        thread::spawn(move || alert_loop(&events, &handle, desktop.as_ref()));
    }

    if !switches.hotkey {
        eprintln!("huitai daemon: 全局呼出键已关（{NO_HOTKEY}）");
        return daemon.run();
    }
    let config = load_config(dir);
    let accept = thread::spawn(move || {
        // 主线程卡在 Carbon 事件循环里，接收循环结束只能在这里结束整个进程。
        match daemon.run() {
            Ok(()) => {
                eprintln!("huitai daemon: 托管的对话都结束了，让位给新版");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("huitai daemon: 监听出错，退出: {e}");
                std::process::exit(1);
            }
        }
    });
    match register_hotkey(&config, handle) {
        Ok(_registration) => hotkey::run_event_loop()?,
        Err(e) => eprintln!("huitai daemon: 全局呼出键没注册上: {e}"),
    }
    accept.join().map_err(|_| io::Error::other("接收线程崩了"))
}

fn load_config(dir: &Path) -> Config {
    Config::load(&dir.join("config.json")).unwrap_or_else(|e| {
        eprintln!("huitai daemon: 设置没读到，用默认值: {e}");
        Config::default()
    })
}

/// 注册 `Config::hotkey`：按下时会话台开着就聚焦它，否则用首选终端新开。回调在主线程，活放到别的线程做。
fn register_hotkey(config: &Config, handle: DaemonHandle) -> io::Result<hotkey::Registration> {
    let key: HotKey = config
        .hotkey
        .parse()
        .map_err(|e| io::Error::new(ErrorKind::InvalidInput, e))?;
    let exe = std::env::current_exe()?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "取不到 HOME"))?;
    let preferred = config.preferred_terminal.clone();
    let busy = Arc::new(AtomicBool::new(false));
    let registration = hotkey::register(key, move || {
        // 上一次还没做完（AppleScript 可能要一会儿）就不叠着再做。
        if busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let (handle, exe, home, preferred, busy) = (
            handle.clone(),
            exe.clone(),
            home.clone(),
            preferred.clone(),
            Arc::clone(&busy),
        );
        thread::spawn(move || {
            let result = summon::summon(handle.board(), preferred.as_deref(), &exe, &home);
            match result {
                Ok(done) => eprintln!("huitai daemon: 呼出键：{done}"),
                Err(e) => eprintln!("huitai daemon: 呼出键没做成: {e}"),
            }
            busy.store(false, Ordering::Release);
        });
    })?;
    eprintln!("huitai daemon: 全局呼出键 {} 已注册", config.hotkey);
    Ok(registration)
}

/// 托管事件（含标签页焦点）→ `Notifier`；到期的提醒交给会话台或系统通知。事件流断了（守护进程在退出）就返回。
pub fn alert_loop(events: &Receiver<HostEvent>, handle: &DaemonHandle, desktop: &dyn Desktop) {
    let mut notifier = Notifier::new();
    loop {
        let next = match notifier.next_due() {
            Some(due) => events.recv_timeout(due.saturating_duration_since(Instant::now())),
            None => events.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match next {
            Ok(event) => {
                if let Some((id, activity)) = crate::ui::activity(event) {
                    notifier.notify(id, activity, Instant::now());
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        for alert in notifier.poll(Instant::now()) {
            let msg = Message::new(&alert, &label(handle, &alert.id));
            if handle.alert(&alert.id, &msg.title, &msg.body) {
                continue;
            }
            if let Err(e) = desktop.show(&msg) {
                eprintln!("huitai daemon: 系统通知没发出去: {e}");
            }
        }
    }
}

/// 提醒标题：和标签页标题一样，「厂商标记 项目名 · CLI 设的标题」。
fn label(handle: &DaemonHandle, id: &SessionId) -> String {
    handle.list().into_iter().find(|h| &h.id == id).map_or_else(
        || "会话台".to_owned(),
        |h| opener::tab_title(h.vendor, &h.project, h.title.as_deref()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn switches_default_to_on() {
        let s = Switches::from_env(env(&[]));
        assert_eq!(
            s,
            Switches {
                hotkey: true,
                notify_to: None
            }
        );
    }

    #[test]
    fn switches_turn_things_off() {
        let s = Switches::from_env(env(&[(NO_HOTKEY, "1"), (NOTIFY_TO, "/tmp/n.log")]));
        assert!(!s.hotkey);
        assert_eq!(s.notify_to, Some(PathBuf::from("/tmp/n.log")));
    }

    #[test]
    fn empty_values_count_as_unset() {
        let s = Switches::from_env(env(&[(NO_HOTKEY, ""), (NOTIFY_TO, "")]));
        assert!(s.hotkey);
        assert_eq!(s.notify_to, None);
    }

    #[test]
    fn file_desktop_appends_lines() {
        let path = std::env::temp_dir().join(format!("huitai-fd-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let d = FileDesktop(path.clone());
        for body in ["一", "二"] {
            let msg = Message {
                title: "✻ demo".to_owned(),
                body: body.to_owned(),
            };
            d.show(&msg).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "✻ demo\t一\n✻ demo\t二\n"
        );
        std::fs::remove_file(&path).unwrap();
    }
}
