//! ratatui 界面、键鼠、主题。只调用下层模块（index、quota、opener、host 客户端、sources）的接口。
//!
//! - [`app`]：界面状态机，键鼠 → 状态和 [`Effect`]，不做 IO，测试直接喂数据。
//!   类型和喂数据在 `app.rs`，按功能分的 `impl App` 在 `app/`：search、board（汇总）、nav（选中）、input（键盘）、menu、settings、mouse。
//! - [`view`]：把 [`App`] 画成一帧（顶栏、列表、预览、底栏、帮助页、设置页）。
//!   公用的画法在 `view.rs`，按区域分在 `view/`：top_bar、list、board、preview、bottom_bar、overlays。
//! - [`theme`]：全部颜色，跟随终端深浅色。
//! - [`keys`]：快捷键表，帮助页和底栏都从这里取。
//! - [`balance`]：`$` 额度面板，顶栏 pi / Hermes 的余额。
//! - [`reader`]：整段对话的只读滚动视图（状态、按键、排版）。
//! - `claims`：会话台新建、托管在临时 id 下的对话认回列表里的哪条（证据、按时间配对、落盘）。
//! - `runtime`：真终端里的事件循环和后台线程，把 Effect 落到下层模块。
//!   后台线程在 `runtime/workers.rs`，动作线程（`Job`、`Worker`）在 `runtime/jobs.rs`。

mod activity;
pub mod app;
pub mod balance;
mod claims;
pub mod keys;
pub mod newdir;
pub mod reader;
pub mod rows;
mod runtime;
pub mod text;
pub mod theme;
pub mod view;

pub use app::{App, Effect, Preview};
pub use runtime::{UpdateCheck, activity, hosted_now, run};
pub use theme::Theme;
