//! socket 协议：帧格式和请求/应答/事件类型。
//!
//! 每帧 = 1 字节类型 + 4 字节大端长度 + 负载。一条连接先握手：客户端发 `Hello`（自己的协议版本和程序版本），
//! 守护进程回 `Hello`（它的版本、进程号、托管着几条对话，不接待时附上原因）；之后客户端发一个请求帧（`Q`），
//! 守护进程回一个应答帧（`R`）。没有握手的旧客户端第一帧就是请求，新守护进程回 `Error`（[`OLD_CLIENT`]）。
//! 握手早于第一个公开版（v0.1.0），发布出去的守护进程都会握手。
//!
//! 兼容规则：请求、应答、事件、记录的字段只加不改，新加的字段带 `#[serde(default)]`；读到不认识的字段忽略，
//! 读到不认识的请求回 `Error`，不认识的事件跳过。协议有不兼容的改动才把 [`PROTOCOL`] 加一。
//!
//!
//! `Attach` 应答成功后这条连接变成转发通道：守护进程发 `O`（输出字节）和最后一个 `X`（结束原因），
//! 客户端发 `I`（输入字节）、`W`（窗口大小）和 `F`（标签页焦点：1 字节，1 在前台、0 切走，
//! 来自真终端的焦点上报 `ESC [ I` / `ESC [ O`）；`Events`、`Board` 应答成功后守护进程持续发 `E`。
//! `Board` 是会话台的订阅：除了状态事件，还会收到要它代为弹出的提醒（`HostEvent::Alert`）；
//! 会话台关掉（连接断开）守护进程就知道它不在了。

use std::io::{self, ErrorKind, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};

use crate::core::{SessionId, SessionState, Vendor};

/// 协议版本。
pub const PROTOCOL: u32 = 1;
/// 守护进程还接待的最老客户端协议。
pub const MIN_CLIENT_PROTOCOL: u32 = 1;
/// 守护进程回给太老的客户端的话：旧版 `ai` 把它原样显示出来。
pub const OLD_CLIENT: &str = "会话台升级了，后台已经换成新版：请重新打开 ai";

/// 单帧负载上限，挡住坏数据把内存撑爆。
const MAX_FRAME: usize = 16 << 20;

pub const KIND_REQUEST: u8 = b'Q';
pub const KIND_RESPONSE: u8 = b'R';
pub const KIND_EVENT: u8 = b'E';
pub const KIND_INPUT: u8 = b'I';
pub const KIND_OUTPUT: u8 = b'O';
pub const KIND_RESIZE: u8 = b'W';
pub const KIND_FOCUS: u8 = b'F';
pub const KIND_END: u8 = b'X';

/// 登记一条要托管的对话。进程在第一次 attach 时才启动，环境取自那次 attach 的终端。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub id: SessionId,
    pub vendor: Vendor,
    /// 项目名，用在标签页标题前缀。
    pub project: String,
    /// 项目目录，CLI 的工作目录。
    pub cwd: PathBuf,
    /// 交给登录 shell 执行的命令，如 `claude --resume <id>`。
    pub command: String,
}

/// 一个程序是哪一版（握手时双方互报）。新旧怎么比见 [`super::version`]。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub protocol: u32,
    /// 程序版本号（`Cargo.toml` 的 version）。
    pub version: String,
    /// 程序文件（规范化的绝对路径）。
    #[serde(default)]
    pub exe: Option<PathBuf>,
    /// 程序文件的修改时间（Unix 秒）；守护进程报的是它启动时的。
    #[serde(default)]
    pub built: u64,
}

impl Build {
    /// 这一版的客户端认不认识 `vendor`。`host` 是守护进程自己：和它同版或更新的客户端什么都认识
    /// （开发时版本号还没改，新加的厂商也照发）；比它旧的只认自己那一版已经有的厂商。
    /// 守护进程据此只把客户端认识的厂商发给它：旧版解码到不认识的厂商会让整份应答失败。
    pub fn knows(&self, vendor: Vendor, host: &Build) -> bool {
        let at = |version: &str| Build {
            version: version.to_owned(),
            ..Build::default()
        };
        let mine = at(&self.version);
        if !at(&host.version).newer_than(&mine) {
            return true;
        }
        vendor_since(vendor).is_none_or(|since| !at(since).newer_than(&mine))
    }
}

/// 这家从哪一版起有；`None` 是一开始就有的。新加一家时写它将随之发布的版本号。
fn vendor_since(vendor: Vendor) -> Option<&'static str> {
    match vendor {
        Vendor::Claude | Vendor::Codex | Vendor::Pi => None,
        Vendor::Hermes | Vendor::Kimi | Vendor::Gemini | Vendor::OpenCode | Vendor::Qwen => {
            Some("0.1.3")
        }
        Vendor::Antigravity => Some("0.1.4"),
    }
}

/// 守护进程对 `Hello` 的回答。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloReply {
    pub build: Build,
    pub pid: u32,
    /// 登记着的托管对话条数（含还没启动的）。
    pub hosted: usize,
    /// 不接待这个客户端（协议太老）时的原因；有它客户端就不发别的请求。
    #[serde(default)]
    pub refused: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    /// 握手，每条连接的第一帧。
    Hello(Build),
    /// 没有托管对话就退出（新版客户端换掉空闲的旧版守护进程）；还有就回 `Error`，等最后一条结束再退出。
    Retire,
    Ping,
    Spawn(SpawnSpec),
    Attach {
        id: SessionId,
        /// 发起 attach 的终端的全部环境变量；第一次 attach 时用来启动 CLI。
        env: Vec<(String, String)>,
        cols: u16,
        rows: u16,
    },
    /// 让正接着这条对话的标签页退出（对话不停）。
    Detach {
        id: SessionId,
    },
    /// 结束这条对话的进程。
    Kill {
        id: SessionId,
    },
    /// 闲置收回这条对话：会话台读厂商记录认定这一轮答完了（不发提醒信号的 CLI 用）。守护进程再看
    /// 没有标签页连着、超过设置的闲置时长没新输出，满足才按闲置收回的步骤结束，否则回 `Error`。
    Reclaim {
        id: SessionId,
    },
    List,
    Events,
    /// 只读取这条对话此刻的屏幕（不影响正接着它的标签页）。`since` 是上次拿到的画面版本，
    /// 画面没变就只回 `Unchanged`。
    Snapshot {
        id: SessionId,
        since: Option<u64>,
    },
    /// 上一个守护进程没正常收尾（断电、重启、崩溃）时还在跑的对话。
    Interrupted,
    /// 把这些对话从「被中断」里拿掉（已重开，或用户不想再接着做）。
    Dismiss {
        ids: Vec<SessionId>,
    },
    /// 会话台打开时订阅：同 `Events`，并登记自己在哪个标签页、能不能代弹提醒。
    Board(BoardInfo),
    /// 用户在会话台里选中/预览了这条对话（看过了）。守护进程转成 `HostEvent::Seen` 广播。
    Seen {
        id: SessionId,
    },
}

/// 会话台登记的自己：在哪个终端的哪个标签页（全局呼出键用来聚焦它），以及能不能弹提醒。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardInfo {
    /// 会话台所在终端的 `TERM_PROGRAM`。
    pub term_program: Option<String>,
    /// 会话台所在标签页的 tty（如 `/dev/ttys012`）。
    pub tty: Option<String>,
    /// Kaku/WezTerm 的窗格 id（`WEZTERM_PANE`）。
    pub pane: Option<String>,
    /// 会话台进程的命令行（`ps` 显示的样子），用来核对窗格里跑的还是它。
    pub command: String,
    /// 会话台所在终端认 OSC 9/777，能往自己标签页写提醒。
    pub alerts: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Hello(HelloReply),
    Ok,
    Pong {
        pid: u32,
    },
    List(Vec<HostedInfo>),
    Snapshot(ScreenSnapshot),
    /// 画面和请求里的版本一样。
    Unchanged,
    Interrupted(Vec<RunRecord>),
    Error(String),
}

/// 一条对话此刻的可见屏幕：每行是若干段同样式的文字，行尾空白已去掉。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    /// 画面版本：有输出或改大小就变，用来判断要不要重画。
    pub version: u64,
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Vec<Run>>,
}

/// 一段样式相同的文字。宽字符在 `text` 里只出现一次，占两列。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub text: String,
    #[serde(default, skip_serializing_if = "CellColor::is_default")]
    pub fg: CellColor,
    #[serde(default, skip_serializing_if = "CellColor::is_default")]
    pub bg: CellColor,
    /// [`attr`] 里各位的组合。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attrs: u8,
}

fn is_zero(n: &u8) -> bool {
    *n == 0
}

/// 文字样式位。
pub mod attr {
    pub const BOLD: u8 = 1;
    pub const DIM: u8 = 1 << 1;
    pub const ITALIC: u8 = 1 << 2;
    pub const UNDERLINE: u8 = 1 << 3;
    pub const INVERSE: u8 = 1 << 4;
    pub const HIDDEN: u8 = 1 << 5;
    pub const STRIKEOUT: u8 = 1 << 6;
}

/// 格子颜色：终端默认色、256 色索引（0–15 是基本 16 色）或真彩色。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl CellColor {
    pub fn is_default(&self) -> bool {
        *self == CellColor::Default
    }
}

/// 一条托管对话的现状。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedInfo {
    pub id: SessionId,
    pub vendor: Vendor,
    pub project: String,
    pub cwd: PathBuf,
    /// CLI 进程号；还没 attach 过、没启动时为 `None`。
    pub pid: Option<u32>,
    pub attached: bool,
    /// CLI 最后设的标题（未加前缀）。
    pub title: Option<String>,
    /// 此刻的状态灯（守护进程的 Notifier 算的，和标签页标题前面的是同一份）。刚启动还没算出来、
    /// 旧版守护进程（没有这个字段）、比自己新的守护进程报了不认识的状态，都是 `None`。
    #[serde(default, deserialize_with = "known_state")]
    pub status: Option<SessionState>,
}

/// 不认识的状态当不知道：只为一个状态灯，不让整份 `List` 解码失败（以后加状态时旧客户端照常用）。
fn known_state<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SessionState>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
}

/// 一条有进程在跑的对话，记在 `running.json`；守护进程没收尾就没了时原样转成「被中断」。
/// 新旧版本的守护进程会读对方写的文件：字段只加不改，新字段带 `#[serde(default)]`，不认识的字段忽略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: SessionId,
    pub vendor: Vendor,
    pub project: String,
    pub cwd: PathBuf,
    /// 进程启动时间（Unix 秒）。
    pub started_at: u64,
}

/// 守护进程广播的状态事件，给状态灯和通知用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostEvent {
    Started {
        id: SessionId,
        pid: u32,
    },
    /// 有输出（同一条对话最多每秒一次）。
    Output {
        id: SessionId,
    },
    /// CLI 停下来要你回来：响铃（BEL），或 OSC 7501 报 blocked/done/error、Gemini 的「✋ Action Required」标题。
    /// 程序状态不另加事件，旧版会话台也认得。
    Bell {
        id: SessionId,
    },
    /// CLI 发的桌面通知（OSC 9 / OSC 777）。
    Notify {
        id: SessionId,
        title: Option<String>,
        body: String,
    },
    Title {
        id: SessionId,
        title: String,
    },
    Attached {
        id: SessionId,
    },
    Detached {
        id: SessionId,
    },
    /// CLI 退出，PTY 已回收。`code` 为退出码，被信号结束时为 `None`。
    Exited {
        id: SessionId,
        code: Option<i32>,
    },
    /// 用户在会话台里看过这条对话（会话台发 `Request::Seen` 引起）。
    Seen {
        id: SessionId,
    },
    /// 接着这条对话的标签页到了前台（`true`）或被切走（`false`）：attach 客户端接上时发一次 `true`，
    /// 之后按终端的焦点上报发；标签页断开时守护进程自己记一次 `false`。守护进程和会话台都靠它判断
    /// 「标签页在前台」，状态灯和提醒吃同样的事件。
    Focus {
        id: SessionId,
        front: bool,
    },
    /// 请会话台往自己的标签页写一条桌面通知（只发给一个能弹提醒的会话台，不广播）。
    /// `title`、`body` 是守护进程排好的显示文字。
    Alert {
        id: SessionId,
        title: String,
        body: String,
    },
}

/// 转发通道为什么结束。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// 同一条对话在别的标签页打开了。
    Elsewhere,
    /// 会话台要求这个标签页退出。
    Detached,
    /// CLI 退出了。
    Exited { code: Option<i32> },
}

/// 读到的一帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub payload: Vec<u8>,
}

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|&n| n as usize <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "帧太大"))?;
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(kind);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)
}

pub fn write_json<T: Serialize>(w: &mut impl Write, kind: u8, value: &T) -> io::Result<()> {
    let json = serde_json::to_vec(value).map_err(io::Error::other)?;
    write_frame(w, kind, &json)
}

/// 读一帧；对端正常关闭（帧边界上 EOF）时返回 `None`。
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head[..1]) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    r.read_exact(&mut head[1..])?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(ErrorKind::InvalidData, "帧长度超限"));
    }
    let mut payload = vec![0; len];
    r.read_exact(&mut payload)?;
    Ok(Some(Frame {
        kind: head[0],
        payload,
    }))
}

impl Frame {
    pub fn json<T: for<'de> Deserialize<'de>>(&self) -> io::Result<T> {
        serde_json::from_slice(&self.payload)
            .map_err(|e| io::Error::new(ErrorKind::InvalidData, format!("协议数据不对: {e}")))
    }
}

/// 读一帧并要求它是 `kind` 类型的 JSON。
pub fn read_json<T: for<'de> Deserialize<'de>>(r: &mut impl Read, kind: u8) -> io::Result<T> {
    let frame =
        read_frame(r)?.ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "连接断了"))?;
    if frame.kind != kind {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("期望帧 {}，收到 {}", kind as char, frame.kind as char),
        ));
    }
    frame.json()
}

pub fn resize_payload(cols: u16, rows: u16) -> [u8; 4] {
    let [c0, c1] = cols.to_be_bytes();
    let [r0, r1] = rows.to_be_bytes();
    [c0, c1, r0, r1]
}

pub fn parse_resize(payload: &[u8]) -> Option<(u16, u16)> {
    let p: [u8; 4] = payload.try_into().ok()?;
    Some((
        u16::from_be_bytes([p[0], p[1]]),
        u16::from_be_bytes([p[2], p[3]]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, KIND_INPUT, b"\x1b[13;2u").unwrap();
        write_json(&mut buf, KIND_REQUEST, &Request::List).unwrap();
        let mut r = buf.as_slice();
        let f = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(
            (f.kind, f.payload.as_slice()),
            (KIND_INPUT, &b"\x1b[13;2u"[..])
        );
        assert_eq!(
            read_json::<Request>(&mut r, KIND_REQUEST).unwrap(),
            Request::List
        );
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn truncated_frame_is_an_error() {
        let mut buf = Vec::new();
        write_frame(&mut buf, KIND_OUTPUT, b"abcdef").unwrap();
        buf.truncate(8);
        assert!(read_frame(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn oversized_length_is_rejected() {
        let buf = [KIND_OUTPUT, 0xff, 0xff, 0xff, 0xff];
        let err = read_frame(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn wrong_kind_is_rejected() {
        let mut buf = Vec::new();
        write_json(&mut buf, KIND_EVENT, &Response::Ok).unwrap();
        assert!(read_json::<Response>(&mut buf.as_slice(), KIND_RESPONSE).is_err());
    }

    #[test]
    fn snapshot_round_trips_and_skips_default_style() {
        let snap = ScreenSnapshot {
            version: 7,
            cols: 10,
            rows: 2,
            lines: vec![
                vec![
                    Run {
                        text: "plain ".to_owned(),
                        ..Run::default()
                    },
                    Run {
                        text: "红".to_owned(),
                        fg: CellColor::Indexed(1),
                        bg: CellColor::Rgb(1, 2, 3),
                        attrs: attr::BOLD | attr::INVERSE,
                    },
                ],
                Vec::new(),
            ],
        };
        let json = serde_json::to_string(&Response::Snapshot(snap.clone())).unwrap();
        assert!(json.contains(r#"{"text":"plain "}"#), "{json}");
        assert_eq!(
            serde_json::from_str::<Response>(&json).unwrap(),
            Response::Snapshot(snap)
        );
    }

    #[test]
    fn hosted_status_is_optional_and_unknown_values_are_ignored() {
        let decode = |status: &str| {
            let json = format!(
                r#"{{"list":[{{"id":"a","vendor":"claude","project":"p","cwd":"/p","pid":1,"attached":false,"title":null{status}}}]}}"#
            );
            match serde_json::from_str::<Response>(&json).unwrap() {
                Response::List(l) => l[0].status,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(decode(""), None, "旧版守护进程没有这个字段");
        assert_eq!(decode(r#","status":null"#), None);
        assert_eq!(
            decode(r#","status":"waiting""#),
            Some(SessionState::Waiting)
        );
        assert_eq!(decode(r#","status":"以后才有的""#), None, "整份照样解得开");
    }

    #[test]
    fn older_clients_know_only_their_vendors() {
        let at = |version: &str| Build {
            protocol: PROTOCOL,
            version: version.to_owned(),
            ..Build::default()
        };
        let host = at("0.1.4");
        assert!(at("0.1.2").knows(Vendor::Pi, &host));
        assert!(!at("0.1.2").knows(Vendor::Kimi, &host));
        assert!(at("0.1.3").knows(Vendor::Kimi, &host));
        // 同版（开发时版本号还没改）和更新的客户端什么都认识。
        assert!(at("0.1.2").knows(Vendor::Qwen, &at("0.1.2")));
        assert!(at("0.2.0").knows(Vendor::Qwen, &host));
    }

    #[test]
    fn resize_round_trips() {
        assert_eq!(parse_resize(&resize_payload(300, 80)), Some((300, 80)));
        assert_eq!(parse_resize(b"abc"), None);
    }
}
