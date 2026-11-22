//! ratatui 界面、键鼠、主题。只调用下层模块（index、quota、opener、host 客户端、sources）的接口。
//!
//! - [`app`]：界面状态机，键鼠 → 状态和 [`Effect`]，不做 IO，测试直接喂数据。
//! - [`view`]：把 [`App`] 画成一帧（顶栏、列表、预览、底栏、帮助页、设置页）。
//! - [`theme`]：全部颜色，跟随终端深浅色。
//! - [`keys`]：快捷键表，帮助页和底栏都从这里取。
//! - [`reader`]：整段对话的只读滚动视图（状态、按键、排版）。
//! - `runtime`：真终端里的事件循环和后台线程，把 Effect 落到下层模块。

pub mod app;
pub mod keys;
pub mod newdir;
pub mod reader;
pub mod rows;
mod runtime;
pub mod text;
pub mod theme;
pub mod view;

pub use app::{App, Effect, Preview};
pub use runtime::{UpdateCheck, activity, run};
pub use theme::Theme;
