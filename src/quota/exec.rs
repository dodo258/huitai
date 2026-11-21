//! 带超时跑子进程，取标准输出。

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// 跑 `program args`，可选写入标准输入；超时就杀掉。成功（退出码 0）返回标准输出。
pub fn run(
    program: &str,
    args: &[&str],
    stdin: Option<&str>,
    timeout: Duration,
) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{program} 启动失败: {e}"))?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // 写失败（对方提前退出）时看退出码就知道，这里不单独报。
        let _ = pipe.write_all(input.as_bytes());
    }
    let mut stdout = child.stdout.take().expect("上面设了 Stdio::piped");
    // 另起线程读输出，免得输出塞满管道时子进程卡住、超时判断失灵。
    let reader = thread::spawn(move || {
        let mut buf = String::new();
        stdout.read_to_string(&mut buf).map(|_| buf)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} 超过 {} 秒没结束", timeout.as_secs()));
            }
            Err(e) => return Err(format!("{program} 等待失败: {e}")),
        }
    };
    let out = reader
        .join()
        .map_err(|_| format!("{program} 读输出的线程崩了"))?
        .map_err(|e| format!("{program} 输出读不了: {e}"))?;
    if !status.success() {
        return Err(format!("{program} 退出码 {status}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_stdout_and_feeds_stdin() {
        assert_eq!(
            run("echo", &["hi"], None, Duration::from_secs(5)).unwrap(),
            "hi\n"
        );
        assert_eq!(
            run("cat", &[], Some("abc"), Duration::from_secs(5)).unwrap(),
            "abc"
        );
    }

    #[test]
    fn reports_failure_and_timeout() {
        assert!(run("false", &[], None, Duration::from_secs(5)).is_err());
        assert!(run("huitai-no-such-program", &[], None, Duration::from_secs(5)).is_err());
        let err = run("sleep", &["5"], None, Duration::from_millis(100)).unwrap_err();
        assert!(err.contains("没结束"), "{err}");
    }
}
