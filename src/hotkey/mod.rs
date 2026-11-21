//! 全局热键（Carbon `RegisterEventHotKey`）。对外接口：`register(key, cb)`。只依赖 `core`。
//!
//! 用法（守护进程）：在主线程 `register`，再在主线程调 `run_event_loop()` 阻塞着收按键；
//! Carbon 把热键事件投到主线程的事件队列，别的线程收不到。设置里的写法见 `HotKey::from_str`，
//! 默认 `Config::hotkey` 是 `alt+space`（⌥空格）。

mod carbon;
mod key;

pub use carbon::{Registration, register, run_event_loop};
pub use key::{CMD, CONTROL, HotKey, OPTION, ParseHotKeyError, SHIFT};
