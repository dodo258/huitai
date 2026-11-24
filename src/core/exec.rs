//! 带超时跑一个短命令（osascript、curl、git、ps、pbcopy……），取标准输出。全项目只有这一份。

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// 多久看一次子进程结束没有。
const POLL: Duration = Duration::from_millis(10);

/// 跑 `program args`，`stdin` 有值就写进标准输入（写完关掉）；超过 `timeout` 就杀掉。
/// 退出码 0 返回标准输出（不合法的 UTF-8 换成替换字符）；否则错误里带上标准错误的第一行。
/// 输出和输入都在别的线程读写，输出再多也不会塞满管道让子进程卡住、超时判断失灵。
pub fn run<A: AsRef<OsStr>>(
    program: impl AsRef<OsStr>,
    args: &[A],
    stdin: Option<&str>,
    timeout: Duration,
) -> io::Result<String> {
    let name = name_of(program.as_ref());
    let out = spawn(Command::new(program).args(args), stdin, timeout, false)?;
    if out.success() {
        return Ok(out.stdout);
    }
    Err(io::Error::other(
        match out.stderr.lines().map(str::trim).find(|l| !l.is_empty()) {
            Some(line) => format!("{name} {}: {line}", out.code_text()),
            None => format!("{name} {}", out.code_text()),
        },
    ))
}

/// 跑完的子进程：退出码（被信号结束是 `None`）和完整的标准输出、标准错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// 「退出码 3」「被信号结束」。
    pub fn code_text(&self) -> String {
        self.code
            .map_or_else(|| "被信号结束".to_owned(), |c| format!("退出码 {c}"))
    }
}

/// 跑一个准备好的命令（参数、环境、目录由调用方定），不管退出码都交回完整输出；超时杀掉、报 `TimedOut`。
/// 给要把失败原文整段给用户看的地方用（升级各家 CLI）。子进程放进自己的进程组，超时时连它拉起的
/// （`sh -c` 里的 npm、curl）一起结束。命令自己退了、它留在后台的进程还占着输出管道时，读输出也只等到
/// 同一个截止时间：到点交回退出码和已经读到的，那个后台进程不动（可能是装好后起的服务）。
pub fn output(cmd: &mut Command, timeout: Duration) -> io::Result<Output> {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    spawn(cmd, None, timeout, true)
}

fn name_of(program: &OsStr) -> String {
    Path::new(program)
        .file_name()
        .unwrap_or(program)
        .to_string_lossy()
        .into_owned()
}

fn spawn(
    cmd: &mut Command,
    stdin: Option<&str>,
    timeout: Duration,
    group: bool,
) -> io::Result<Output> {
    let name = name_of(cmd.get_program());
    let mut child = cmd
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("{name} 启动失败: {e}")))?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let input = input.to_owned();
        // 写失败（对方提前退出）时看退出码就知道，这里不单独报；写完 pipe 随线程结束关掉，对方读到结尾。
        thread::spawn(move || {
            let _ = pipe.write_all(input.as_bytes());
        });
    }
    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            if group && let Ok(pid) = i32::try_from(child.id()) {
                // SAFETY: 只发信号，不碰内存；进程组号就是子进程号（`process_group(0)`）。
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{name} 超过 {}没结束", seconds(timeout)),
            ));
        }
        thread::sleep(POLL);
    };
    let until = group.then_some(deadline);
    Ok(Output {
        code: status.code(),
        stdout: stdout.finish(until),
        stderr: stderr.finish(until),
    })
}

/// 后台读着的一个管道。
struct Reading {
    /// 读到的；取走后是 `None`，线程接着把管道读空但不再存（不读了对方会写不进去或被 SIGPIPE 结束）。
    buf: Arc<Mutex<Option<Vec<u8>>>>,
    /// 读到结尾时断开。
    done: Receiver<()>,
}

impl Reading {
    /// 等读到结尾（给了 `until` 最多等到那时），交回读到的。
    fn finish(self, until: Option<Instant>) -> String {
        match until {
            Some(at) => {
                let _ = self
                    .done
                    .recv_timeout(at.saturating_duration_since(Instant::now()));
            }
            None => {
                let _ = self.done.recv();
            }
        }
        let bytes = self
            .buf
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

fn read_in_background<R: Read + Send + 'static>(pipe: Option<R>) -> Reading {
    let buf = Arc::new(Mutex::new(Some(Vec::new())));
    let (tx, done) = mpsc::channel();
    let shared = Arc::clone(&buf);
    thread::spawn(move || {
        let _tx = tx;
        let Some(mut pipe) = pipe else { return };
        let mut chunk = [0; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => {
                    let mut buf = shared.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Some(buf) = buf.as_mut() {
                        buf.extend_from_slice(&chunk[..n]);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    });
    Reading { buf, done }
}

/// `15 秒`、`0.1 秒`。
fn seconds(d: Duration) -> String {
    if d.subsec_millis() == 0 {
        format!("{} 秒", d.as_secs())
    } else {
        format!("{:.1} 秒", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(5);
    const NONE: &[&str] = &[];

    #[test]
    fn returns_stdout_and_feeds_stdin() {
        assert_eq!(run("echo", &["hi"], None, T).unwrap(), "hi\n");
        assert_eq!(run("cat", NONE, Some("abc"), T).unwrap(), "abc");
        // 输入比管道缓冲大、对方边读边写：不卡住。
        let big = "x".repeat(1 << 20);
        assert_eq!(run("cat", NONE, Some(&big), T).unwrap().len(), big.len());
    }

    #[test]
    fn invalid_utf8_is_replaced_not_dropped() {
        let out = run("printf", &["a\\377b"], None, T).unwrap();
        assert_eq!(out, "a\u{fffd}b");
    }

    #[test]
    fn failure_carries_first_stderr_line() {
        let err = run(
            "sh",
            &["-c", "echo; echo boom >&2; echo more >&2; exit 3"],
            None,
            T,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "sh 退出码 3: boom");
        assert_eq!(
            run("false", NONE, None, T).unwrap_err().to_string(),
            "false 退出码 1"
        );
        let missing = run("huitai-no-such-program", NONE, None, T).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn output_keeps_everything_and_kills_the_group() {
        let out = output(
            Command::new("sh").args(["-c", "echo a; echo b >&2; echo c >&2; exit 4"]),
            T,
        )
        .unwrap();
        assert_eq!(out.code, Some(4));
        assert_eq!(out.stdout, "a\n");
        assert_eq!(out.stderr, "b\nc\n");
        assert_eq!(out.code_text(), "退出码 4");
        // 超时：sh 拉起的 sleep 也一起结束，不会拿着管道让读线程等它。
        let started = Instant::now();
        let err = output(
            Command::new("sh").args(["-c", "sleep 30 & sleep 30; echo never"]),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn output_returns_on_time_when_a_grandchild_keeps_the_pipes() {
        let started = Instant::now();
        let out = output(
            Command::new("sh").args(["-c", "sleep 30 & echo a; echo b >&2; exit 3"]),
            Duration::from_secs(3),
        )
        .unwrap();
        // 命令退了、sleep 还拿着管道：到点交回退出码和读到的。
        assert_eq!(out.code, Some(3));
        assert_eq!((out.stdout.as_str(), out.stderr.as_str()), ("a\n", "b\n"));
        assert!(started.elapsed() < Duration::from_secs(15));
    }

    #[test]
    fn times_out() {
        let started = Instant::now();
        let err = run("sleep", &["5"], None, Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(err.to_string(), "sleep 超过 0.1 秒没结束");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
