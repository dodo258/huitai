//! 开 PTY、在里面用登录 shell 启动 CLI、改窗口大小。

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};

/// 开 PTY 到设好 close-on-exec 之间不能有别的线程 fork，否则主端会漏进那个子进程。
static SPAWN_LOCK: Mutex<()> = Mutex::new(());
/// [`raise_fd_limit`] 调高之前的描述符上限：CLI 照旧用终端给的上限，不跟着守护进程变。
static INHERITED_NOFILE: OnceLock<libc::rlimit> = OnceLock::new();
/// 描述符上限调到这么多（macOS 的 OPEN_MAX；硬上限更低就用硬上限）。
const WANT_NOFILE: libc::rlim_t = 10240;

/// 把本进程的描述符软上限调高到 [`WANT_NOFILE`]（硬上限更低就到硬上限）。之后用 [`spawn`] 启动的 CLI 用回原来的。
/// 调不了就保持原样。
pub fn raise_fd_limit() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: lim 是有效的可写 rlimit。
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return;
    }
    let want = WANT_NOFILE.min(lim.rlim_max);
    if lim.rlim_cur >= want {
        return;
    }
    let raised = libc::rlimit {
        rlim_cur: want,
        rlim_max: lim.rlim_max,
    };
    // SAFETY: raised 是有效的 rlimit，软上限不超过硬上限。
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
        let _ = INHERITED_NOFILE.set(lim);
    }
}

pub struct Pty {
    /// PTY 主端：读 CLI 输出、写输入、改大小。
    pub master: File,
    pub child: Child,
}

/// 在新 PTY 里执行 `$SHELL -l -c <command>`。环境变量整套换成 `env`，工作目录 `cwd`。
pub fn spawn(
    command: &str,
    cwd: &Path,
    env: &[(String, String)],
    cols: u16,
    rows: u16,
) -> io::Result<Pty> {
    let shell = env
        .iter()
        .find(|(k, _)| k == "SHELL")
        .map(|(_, v)| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("/bin/zsh");

    let _guard = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (master, slave) = open(cols, rows)?;
    let mut cmd = Command::new(shell);
    cmd.arg("-l")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    let inherited = INHERITED_NOFILE.get().copied();
    // SAFETY: pre_exec 里只调 setsid、ioctl、setrlimit 这几个直接的系统调用，不分配内存、不拿锁。
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(lim) = inherited {
                // 调不回去也不影响启动：CLI 只是多了些描述符可用。
                libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
            }
            // 新会话的控制终端设成 PTY 从端（这时 0 号描述符已经是从端）。
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("启动 {shell} 失败: {e}")))?;
    Ok(Pty {
        master: File::from(master),
        child,
    })
}

fn open(cols: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (-1, -1);
    let mut ws = winsize(cols, rows);
    // SAFETY: 传入的指针都指向有效的局部变量，termios 传空表示用默认值。
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty 成功时两个描述符都有效且归我们所有。
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    set_cloexec(&master)?;
    set_cloexec(&slave)?;
    Ok((master, slave))
}

fn set_cloexec(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: fd 有效；F_SETFD 只改描述符标志。
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// 改 PTY 大小；内核会给前台进程组发 SIGWINCH。
pub fn resize(master: &File, cols: u16, rows: u16) -> io::Result<()> {
    let ws = winsize(cols, rows);
    // SAFETY: master 是有效的 PTY 主端，ws 是有效的 winsize。
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 读终端（或 PTY）当前大小。
pub fn size_of(fd: &impl AsRawFd) -> io::Result<(u16, u16)> {
    let mut ws = winsize(0, 0);
    // SAFETY: fd 有效，ws 是可写的 winsize。
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((ws.ws_col, ws.ws_row))
}

/// 给 CLI 所在的整个进程组发信号（CLI 是会话首进程，进程组号就是它的 pid）。
pub fn signal_group(pid: u32, sig: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: kill 不涉及内存；进程组可能已经没了，失败无妨。
        unsafe {
            libc::kill(-pid, sig);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn runs_command_in_cwd_with_given_env_and_size() {
        let env = vec![
            ("SHELL".to_owned(), "/bin/sh".to_owned()),
            ("HUITAI_PROBE".to_owned(), "probe-ok".to_owned()),
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("HOME".to_owned(), "/tmp".to_owned()),
        ];
        let mut pty = spawn(
            "echo \"$HUITAI_PROBE\"; pwd; stty size",
            Path::new("/tmp"),
            &env,
            91,
            17,
        )
        .unwrap();
        // macOS 上子进程关从端时要等输出被读走才能退出，所以先读到 EOF/EIO 再 wait。
        let mut out = String::new();
        let mut buf = [0; 4096];
        while let Ok(n) = pty.master.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(pty.child.wait().unwrap().success());
        assert!(out.contains("probe-ok"), "{out}");
        assert!(
            out.contains("/tmp") || out.contains("/private/tmp"),
            "{out}"
        );
        assert!(out.contains("17 91"), "{out}");
    }

    #[test]
    fn resize_is_visible_to_size_query() {
        let env = vec![("SHELL".to_owned(), "/bin/sh".to_owned())];
        let mut pty = spawn("sleep 5", Path::new("/"), &env, 80, 24).unwrap();
        assert_eq!(size_of(&pty.master).unwrap(), (80, 24));
        resize(&pty.master, 120, 40).unwrap();
        assert_eq!(size_of(&pty.master).unwrap(), (120, 40));
        signal_group(pty.child.id(), libc::SIGKILL);
        pty.child.wait().unwrap();
    }
}
