//! 带超时跑一个短命令（osascript、curl、git、ps、pbcopy……），取标准输出。全项目只有这一份。

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
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
    let program = program.as_ref();
    let name = Path::new(program)
        .file_name()
        .unwrap_or(program)
        .to_string_lossy()
        .into_owned();
    let mut child = Command::new(program)
        .args(args)
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
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{name} 超过 {}没结束", seconds(timeout)),
            ));
        }
        thread::sleep(POLL);
    };
    let out = stdout.join().unwrap_or_default();
    if status.success() {
        return Ok(out);
    }
    let err = stderr.join().unwrap_or_default();
    let code = status
        .code()
        .map_or_else(|| "被信号结束".to_owned(), |c| format!("退出码 {c}"));
    Err(io::Error::other(
        match err.lines().map(str::trim).find(|l| !l.is_empty()) {
            Some(line) => format!("{name} {code}: {line}"),
            None => format!("{name} {code}"),
        },
    ))
}

fn read_in_background<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
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
    fn times_out() {
        let started = Instant::now();
        let err = run("sleep", &["5"], None, Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(err.to_string(), "sleep 超过 0.1 秒没结束");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
