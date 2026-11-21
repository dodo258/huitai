//! 故障注入（只在 debug 构建里有，测 panic 隔离用；release 里是空函数）。
//!
//! 环境变量 [`PANIC_ON_OUTPUT`]`=<标记>`：守护进程处理 PTY 输出时，某一段里出现这个标记就在持着
//! 这条对话的锁时 panic，模拟状态机遇到没见过的输入崩了。

/// 设了它的守护进程在输出里看到它的值就 panic。
pub const PANIC_ON_OUTPUT: &str = "HUITAI_FAILPOINT_PANIC";

#[cfg(debug_assertions)]
pub fn output(chunk: &[u8]) {
    use std::sync::OnceLock;
    static MARK: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    let mark = MARK.get_or_init(|| {
        std::env::var(PANIC_ON_OUTPUT)
            .ok()
            .filter(|m| !m.is_empty())
            .map(String::into_bytes)
    });
    if let Some(m) = mark
        && chunk.windows(m.len()).any(|w| w == m.as_slice())
    {
        panic!("故障注入：输出里出现了 {PANIC_ON_OUTPUT} 的标记");
    }
}

#[cfg(not(debug_assertions))]
#[inline]
pub fn output(_chunk: &[u8]) {}
