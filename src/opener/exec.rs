//! 带超时跑一个辅助命令（osascript、kaku cli、ps），取标准输出。

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use std::{io, path::Path};

/// 辅助命令的默认超时。AppleScript 第一次连 Ghostty 会慢一些，给足。
pub const TIMEOUT: Duration = Duration::from_secs(15);

/// 跑 `program args`，超时就杀掉。退出码 0 返回标准输出；否则错误里带上标准错误。
pub fn run(program: &Path, args: &[&str], timeout: Duration) -> io::Result<String> {
    let name = program.display();
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("{name} 启动失败: {e}")))?;
    // 另起线程读输出，免得输出塞满管道时子进程卡住、超时判断失灵。
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
                format!("{name} 超过 {} 秒没结束", timeout.as_secs()),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let out = stdout.join().unwrap_or_default();
    if status.success() {
        return Ok(out);
    }
    let err = stderr.join().unwrap_or_default();
    Err(io::Error::other(format!(
        "{name} 退出码 {status}: {}",
        err.trim()
    )))
}

fn read_in_background<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_string(&mut buf);
        }
        buf
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_stdout() {
        assert_eq!(run(Path::new("echo"), &["hi"], TIMEOUT).unwrap(), "hi\n");
    }

    #[test]
    fn failure_carries_stderr() {
        let err = run(Path::new("sh"), &["-c", "echo boom >&2; exit 3"], TIMEOUT).unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        assert!(run(Path::new("huitai-no-such-program"), &[], TIMEOUT).is_err());
    }

    #[test]
    fn times_out() {
        let err = run(Path::new("sleep"), &["5"], Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }
}
