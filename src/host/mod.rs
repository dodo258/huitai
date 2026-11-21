//! 守护进程：PTY 托管、原样转发、终端状态机重连恢复、标题改写、通知序列识别、防休眠、闲置收回、socket 协议。
//! 对外接口：`Hello/Retire/Spawn/Attach/Detach/Kill/List/Events/Board/Seen/Snapshot/Interrupted/Dismiss`（见 [`protocol`]），客户端函数在 [`client`]。只依赖 `core`。

pub mod client;
pub mod daemon;
pub mod failpoint;
pub mod idle;
pub mod interrupt;
mod outbox;
pub mod power;
pub mod protocol;
pub mod pty;
pub mod scan;
pub mod screen;
pub mod version;

pub use client::{
    AttachMsg, AttachWriter, Attachment, DaemonInfo, EventStream, Snapshot, attach, board,
    ensure_daemon, probe, request, snapshot, subscribe,
};
pub use daemon::{Daemon, DaemonHandle, socket_path};
pub use protocol::{
    BoardInfo, Build, CellColor, EndReason, HelloReply, HostEvent, HostedInfo, Request, Response,
    Run, RunRecord, ScreenSnapshot, SpawnSpec, attr,
};
pub use screen::Screen;
