//! 状态变化 → 通知。对外接口：`Notifier::notify(id, activity, now)` 喂事件、`poll` 取该发的提醒、
//! `state` 取状态灯；`deliver` 把提醒送到会话台标签页（OSC 9/777）或系统通知。只依赖 `core`。
//!
//! 状态：● 在跑（有输出）；收到响铃/通知且之后没有新输出时，用户没看过是 ✓ 刚做完，看过是 ◆ 等你回复；
//! ○ 已停（没有 PTY）。只在那条对话的标签页不在前台时提醒，从开跑到下一次开跑之间最多提醒一次。
//! 不读也不改 Codex 的 `notify` 配置：提醒全靠托管层看到的 PTY 输出。
//!
//! 通知判断只该有一份（放在守护进程里），否则会话台和守护进程各算一遍会重复弹。

mod deliver;
mod tracker;

pub use deliver::{
    Desktop, Message, OscKind, SystemNotifier, Tab, deliver, osc_kind, osc_sequence,
};
pub use tracker::{Activity, Alert, COALESCE, Notifier, REDRAW_GRACE};
