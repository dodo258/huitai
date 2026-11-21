//! 会话台（huitai）。模块划分与依赖方向见 AGENTS.md「模块」：
//! `cli → ui → (index, quota, opener, host 客户端) → sources → core`；
//! `host`、`notify`、`hotkey`、`ime` 只依赖 `core`。

pub mod cli;
pub mod core;
pub mod host;
pub mod hotkey;
pub mod ime;
pub mod index;
pub mod notify;
pub mod opener;
pub mod quota;
pub mod sources;
pub mod ui;
