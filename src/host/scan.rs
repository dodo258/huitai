//! 输出扫描：PTY 输出原样放行，只改写 OSC 0/1/2 标题（加前缀），并认出 BEL、OSC 9、OSC 777 通知、
//! OSC 7501 程序状态（Hermes、OpenCode v2 发）和 Gemini CLI 的「✋ Action Required」等待标题。
//! OSC 7501 只看根记录（不带 `id` 的报告）：带 `id` 的是 CLI 里某个子任务（OpenCode 2.x 每个标签页一条），
//! 不代表整个进程，见 [`Scanner::cli_blocked`]。
//!
//! 流式处理：序列被拆在两次读里也能认。只有标题序列在结束前会被暂扣（上限 [`MAX_HELD`] 字节，
//! 超了就原样放出、不再改写），其余字节一律立即原样输出。

/// 标题序列最多暂扣这么多字节。
const MAX_HELD: usize = 4096;
/// 通知正文最多记这么多字节（序列本身照样完整放行）。
const MAX_NOTE: usize = 4096;

const BEL: u8 = 0x07;
const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// 扫描时认出的信号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    Bell,
    Notify {
        title: Option<String>,
        body: String,
    },
    /// CLI 设的标题（未加前缀）。
    Title(String),
    /// CLI 报告的程序状态：OSC 7501 的 `state=`，或 Gemini 的等待标题（算 [`Status::Blocked`]）。
    Status(Status),
}

/// OSC 7501 根记录里会话台关心的状态（`idle`、`clear` 和不认识的不报）。
/// 格式：`ESC ] 7501 ; key=value:key=value… ST`，`state` 必有，例 `state=blocked:kind=permission:app=hermes`。
/// 规范：<https://www.superlogical.com/rex/docs/build/program-status>（没有 `id` 的报告写根记录，键重复取最后一个）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 在跑。
    Working,
    /// 要你回复（权限、提问、登录）。
    Blocked,
    /// 做完了，等你看结果。
    Done,
    /// 出错停下了。
    Error,
}

impl Status {
    /// 停下来要你回来看（提醒、状态灯走和响铃一样的路）。
    pub fn needs_you(self) -> bool {
        self != Status::Working
    }
}

#[derive(Debug)]
enum State {
    Ground,
    Esc,
    /// `ESC ]` 之后在读参数数字；`held` 是还没放出的原始字节。
    OscParam,
    /// 标题正文，整段暂扣待改写。`ps` 是 0/1/2。
    TitleBody {
        ps: u8,
        body_start: usize,
    },
    TitleBodyEsc {
        body_start: usize,
    },
    /// 其它 OSC：原样放行，9/777 记下正文。
    OscOther {
        ps: Option<u32>,
        note: Vec<u8>,
    },
    OscOtherEsc {
        ps: Option<u32>,
        note: Vec<u8>,
    },
    /// DCS/APC/PM/SOS：原样放行到 ST，里面的 BEL 不算响铃、`ESC ESC` 不算 ESC（tmux 透传）。
    StringSeq,
    StringSeqEsc,
}

pub struct Scanner {
    prefix: Vec<u8>,
    state: State,
    held: Vec<u8>,
    /// 上一次 CLI 设的标题是 Gemini 的等待标题：同一个标题重发不再报。
    title_waiting: bool,
    /// OSC 7501 根记录现在的状态；`idle`、`clear` 或还没报过是 `None`。
    program: Option<Status>,
    /// 已经放出去的字节停在哪儿，决定能不能在这里插一条标题序列。
    tail: Tail,
}

impl Scanner {
    /// `prefix` 加在 CLI 标题前，如 `✻ huitai · `。
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.as_bytes().to_vec(),
            state: State::Ground,
            held: Vec::new(),
            title_waiting: false,
            program: None,
            tail: Tail::default(),
        }
    }

    /// 换前缀（标签页标题的状态变了）。之后 CLI 设的标题用新前缀；暂扣着、还没结束的标题也用新的。
    pub fn set_prefix(&mut self, prefix: &str) {
        self.prefix = prefix.as_bytes().to_vec();
    }

    /// 处理一段输出：改写后的字节追加到 `out`，认出的信号追加到 `signals`。
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>, signals: &mut Vec<Signal>) {
        let start = out.len();
        for &b in input {
            self.byte(b, out, signals);
        }
        for &b in &out[start..] {
            self.tail.push(b);
        }
    }

    /// 已经放出去的字节停在一个完整的地方（不在转义序列、字符串序列或一个 UTF-8 字符的中间）：
    /// 这时在输出里插一条完整的标题序列，不会打断 CLI 的任何字节。暂扣着的字节还没放出去，不影响。
    pub fn at_boundary(&self) -> bool {
        self.tail.clean()
    }

    /// CLI 用 OSC 7501 根记录说它在等你（blocked），还没改口（working/done/error/idle/clear）。
    /// 这期间的输出不算在跑：OpenCode 2.x 一个标签页等确认时，别的标签页开工会一直重画（标签页上的扫光
    /// 每秒 20 帧），根记录照样是 blocked。
    pub fn cli_blocked(&self) -> bool {
        self.program == Some(Status::Blocked)
    }

    fn byte(&mut self, b: u8, out: &mut Vec<u8>, signals: &mut Vec<Signal>) {
        let state = std::mem::replace(&mut self.state, State::Ground);
        self.state = match state {
            State::Ground => self.ground(b, out, signals),
            State::Esc => match b {
                b']' => {
                    self.held.extend_from_slice(&[ESC, b']']);
                    State::OscParam
                }
                b'P' | b'_' | b'^' | b'X' => {
                    out.extend_from_slice(&[ESC, b]);
                    State::StringSeq
                }
                ESC => {
                    out.push(ESC);
                    State::Esc
                }
                _ => {
                    out.extend_from_slice(&[ESC, b]);
                    State::Ground
                }
            },
            State::OscParam => match b {
                b'0'..=b'9' if self.held.len() < 16 => {
                    self.held.push(b);
                    State::OscParam
                }
                b';' => {
                    let ps = parse_ps(&self.held[2..]);
                    self.held.push(b);
                    match ps {
                        Some(ps @ 0..=2) => State::TitleBody {
                            ps: ps as u8,
                            body_start: self.held.len(),
                        },
                        _ => {
                            out.append(&mut self.held);
                            State::OscOther {
                                ps,
                                note: Vec::new(),
                            }
                        }
                    }
                }
                _ => {
                    let ps = parse_ps(&self.held[2..]);
                    out.append(&mut self.held);
                    self.osc_other(ps, Vec::new(), b, out, signals)
                }
            },
            State::TitleBody { ps, body_start } => match b {
                BEL => {
                    self.emit_title(body_start, &[BEL], out, signals);
                    State::Ground
                }
                ESC => State::TitleBodyEsc { body_start },
                CAN | SUB => {
                    out.append(&mut self.held);
                    out.push(b);
                    State::Ground
                }
                _ if self.held.len() >= MAX_HELD => {
                    out.append(&mut self.held);
                    out.push(b);
                    State::OscOther {
                        ps: Some(u32::from(ps)),
                        note: Vec::new(),
                    }
                }
                _ => {
                    self.held.push(b);
                    State::TitleBody { ps, body_start }
                }
            },
            State::TitleBodyEsc { body_start } => {
                if b == b'\\' {
                    self.emit_title(body_start, &[ESC, b'\\'], out, signals);
                    State::Ground
                } else {
                    // ESC 打断了标题序列：原样放出，按新序列的开头重新处理。
                    out.append(&mut self.held);
                    self.state = State::Esc;
                    self.byte(b, out, signals);
                    return;
                }
            }
            State::OscOther { ps, note } => self.osc_other(ps, note, b, out, signals),
            State::OscOtherEsc { ps, note } => match b {
                b'\\' => {
                    out.push(b);
                    self.finish_note(ps, &note, signals);
                    State::Ground
                }
                // 又一个 ESC：先不放，交给 `Esc` 状态和后面的字节一起放（它会补上这个 ESC）。
                ESC => State::Esc,
                _ => {
                    out.push(b);
                    State::Ground
                }
            },
            State::StringSeq => {
                out.push(b);
                if b == ESC {
                    State::StringSeqEsc
                } else if b == CAN || b == SUB {
                    State::Ground
                } else {
                    State::StringSeq
                }
            }
            // `ESC ESC` 是透传里转义过的一个 ESC（tmux 的规则），后面的 `\` 不算 ST，里面的标题也不改写。
            State::StringSeqEsc => {
                out.push(b);
                match b {
                    b'\\' => State::Ground,
                    _ => State::StringSeq,
                }
            }
        };
    }

    fn ground(&mut self, b: u8, out: &mut Vec<u8>, signals: &mut Vec<Signal>) -> State {
        match b {
            ESC => State::Esc,
            BEL => {
                out.push(b);
                signals.push(Signal::Bell);
                State::Ground
            }
            _ => {
                out.push(b);
                State::Ground
            }
        }
    }

    fn osc_other(
        &mut self,
        ps: Option<u32>,
        mut note: Vec<u8>,
        b: u8,
        out: &mut Vec<u8>,
        signals: &mut Vec<Signal>,
    ) -> State {
        match b {
            ESC => {
                out.push(b);
                State::OscOtherEsc { ps, note }
            }
            BEL => {
                out.push(b);
                self.finish_note(ps, &note, signals);
                State::Ground
            }
            CAN | SUB => {
                out.push(b);
                State::Ground
            }
            _ => {
                out.push(b);
                if matches!(ps, Some(9 | 777 | 7501)) && note.len() < MAX_NOTE {
                    note.push(b);
                }
                State::OscOther { ps, note }
            }
        }
    }

    fn finish_note(&mut self, ps: Option<u32>, note: &[u8], signals: &mut Vec<Signal>) {
        let text = String::from_utf8_lossy(note);
        match ps {
            // OSC 9;<数字>;… 是 ConEmu 的进度条等子命令，不是通知。
            Some(9) if !is_conemu_subcommand(&text) => signals.push(Signal::Notify {
                title: None,
                body: text.into_owned(),
            }),
            Some(777) => {
                let mut parts = text.splitn(3, ';');
                if parts.next() == Some("notify") {
                    let title = parts.next().map(str::to_owned).filter(|t| !t.is_empty());
                    let body = parts.next().unwrap_or_default().to_owned();
                    signals.push(Signal::Notify { title, body });
                }
            }
            Some(7501) => {
                let (Some(state), true) = parse_report(&text) else {
                    return;
                };
                match (state, status_of(state)) {
                    (_, Some(status)) => {
                        self.program = Some(status);
                        signals.push(Signal::Status(status));
                    }
                    ("idle" | "clear", None) => self.program = None,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn emit_title(
        &mut self,
        body_start: usize,
        terminator: &[u8],
        out: &mut Vec<u8>,
        signals: &mut Vec<Signal>,
    ) {
        let body = &self.held[body_start..];
        let title = String::from_utf8_lossy(body).into_owned();
        let waiting = is_waiting_title(&title);
        signals.push(Signal::Title(title));
        if waiting && !self.title_waiting {
            signals.push(Signal::Status(Status::Blocked));
        }
        self.title_waiting = waiting;
        out.extend_from_slice(&self.held[..body_start]);
        out.extend_from_slice(&self.prefix);
        out.extend_from_slice(body);
        out.extend_from_slice(terminator);
        self.held.clear();
    }
}

/// 跟着放出去的字节走的极简终端解析：只为知道停在哪儿，不认具体序列。
#[derive(Debug, Default)]
struct Tail {
    state: TailState,
    /// 当前 UTF-8 字符还差几个续字节。
    utf8_left: u8,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum TailState {
    #[default]
    Ground,
    Esc,
    /// `ESC` 加中间字节（如 `ESC ( B`），等结束字节。
    EscInter,
    /// `ESC [` 之后，等结束字节（0x40–0x7E）。
    Csi,
    /// OSC 的正文，到 BEL 或 ST。
    Osc,
    OscEsc,
    /// DCS/APC/PM/SOS 的正文，只到 ST：里面的 BEL 是数据（tmux 透传 `ESC P tmux; ESC ESC ] … BEL ESC \`
    /// 里内层 OSC 的 BEL 之后外层还没完）。
    Dcs,
    DcsEsc,
}

impl Tail {
    fn clean(&self) -> bool {
        self.state == TailState::Ground && self.utf8_left == 0
    }

    fn push(&mut self, b: u8) {
        use TailState::*;
        if matches!(b, CAN | SUB) {
            self.state = Ground;
            self.utf8_left = 0;
            return;
        }
        self.state = match self.state {
            Ground => {
                if b == ESC {
                    self.utf8_left = 0;
                    Esc
                } else {
                    self.utf8_left = match b {
                        0x80..=0xbf => self.utf8_left.saturating_sub(1),
                        0xc0..=0xdf => 1,
                        0xe0..=0xef => 2,
                        0xf0..=0xf7 => 3,
                        _ => 0,
                    };
                    Ground
                }
            }
            Esc => match b {
                b'[' => Csi,
                b']' => Osc,
                b'P' | b'_' | b'^' | b'X' => Dcs,
                ESC => Esc,
                0x20..=0x2f => EscInter,
                _ => Ground,
            },
            EscInter => match b {
                0x20..=0x2f => EscInter,
                ESC => Esc,
                _ => Ground,
            },
            Csi => match b {
                0x40..=0x7e => Ground,
                ESC => Esc,
                _ => Csi,
            },
            Osc => match b {
                BEL => Ground,
                ESC => OscEsc,
                _ => Osc,
            },
            OscEsc => match b {
                b'\\' => Ground,
                ESC => OscEsc,
                _ => Osc,
            },
            Dcs => match b {
                ESC => DcsEsc,
                _ => Dcs,
            },
            // `ESC ESC` 是透传里转义过的一个 ESC（tmux 的规则），后面的 `\` 不算 ST。
            DcsEsc => match b {
                b'\\' => Ground,
                _ => Dcs,
            },
        };
    }
}

fn parse_ps(digits: &[u8]) -> Option<u32> {
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// OSC 7501 一条报告：`state`（没有是 `None`）和是不是写根记录（没有 `id` 键）。查询（`?`）没有 `state`。
/// 键值两边的空白去掉，没有 `=` 或键为空的跳过，键重复取最后一个（规范「Syntax」节）。
fn parse_report(text: &str) -> (Option<&str>, bool) {
    let mut state = None;
    let mut root = true;
    for pair in text.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key.trim() {
            "state" => state = Some(value.trim()),
            "id" => root = false,
            _ => {}
        }
    }
    (state, root)
}

fn status_of(state: &str) -> Option<Status> {
    match state {
        "working" => Some(Status::Working),
        "blocked" => Some(Status::Blocked),
        "done" => Some(Status::Done),
        "error" => Some(Status::Error),
        _ => None,
    }
}

/// Gemini CLI 等你确认时把窗口标题设成 `✋  Action Required (目录)`（`ui.dynamicWindowTitle`，默认开）。
fn is_waiting_title(title: &str) -> bool {
    title
        .trim_start()
        .strip_prefix('✋')
        .is_some_and(|rest| rest.trim_start().starts_with("Action Required"))
}

fn is_conemu_subcommand(text: &str) -> bool {
    let head = text.split(';').next().unwrap_or_default();
    !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "✻ huitai · ";

    fn scan_chunks(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Signal>) {
        let mut s = Scanner::new(PREFIX);
        let (mut out, mut sig) = (Vec::new(), Vec::new());
        for c in chunks {
            s.feed(c, &mut out, &mut sig);
        }
        (out, sig)
    }

    fn scan(input: &[u8]) -> (Vec<u8>, Vec<Signal>) {
        scan_chunks(&[input])
    }

    /// 每个字节单独喂一次，结果必须和整段喂一样。
    fn scan_bytewise(input: &[u8]) -> (Vec<u8>, Vec<Signal>) {
        let chunks: Vec<&[u8]> = input.chunks(1).collect();
        scan_chunks(&chunks)
    }

    #[test]
    fn plain_bytes_pass_through_unchanged() {
        let input = b"hello \x1b[31mred\x1b[0m\r\n\xe4\xb8\xad\x1b[?2004h\x1b[>1u";
        assert_eq!(scan(input), (input.to_vec(), vec![]));
    }

    #[test]
    fn title_osc_gets_prefix_for_0_1_2_and_both_terminators() {
        for ps in ["0", "1", "2"] {
            let input = format!("a\x1b]{ps};修登录\x07b");
            let want = format!("a\x1b]{ps};{PREFIX}修登录\x07b");
            let (out, sig) = scan(input.as_bytes());
            assert_eq!(String::from_utf8(out).unwrap(), want);
            assert_eq!(sig, vec![Signal::Title("修登录".into())]);
        }
        let (out, _) = scan(b"\x1b]2;t\x1b\\");
        assert_eq!(out, format!("\x1b]2;{PREFIX}t\x1b\\").into_bytes());
    }

    #[test]
    fn split_title_is_rewritten_the_same() {
        let input = "x\x1b]0;claude 在干活\x07y\x1b]2;two\x1b\\z".as_bytes();
        assert_eq!(scan_bytewise(input), scan(input));
        assert_eq!(scan(input).1.len(), 2);
    }

    #[test]
    fn other_osc_passes_through_untouched() {
        let input = b"\x1b]8;;https://a.b\x07link\x1b]8;;\x07\x1b]52;c;aGk=\x07\x1b]10;?\x1b\\";
        assert_eq!(scan(input), (input.to_vec(), vec![]));
        assert_eq!(scan_bytewise(input), (input.to_vec(), vec![]));
    }

    #[test]
    fn bell_is_a_signal_but_osc_terminator_is_not() {
        let (out, sig) = scan(b"done\x07 \x1b]8;;u\x07");
        assert_eq!(out, b"done\x07 \x1b]8;;u\x07");
        assert_eq!(sig, vec![Signal::Bell]);
    }

    #[test]
    fn bel_inside_apc_or_dcs_is_not_a_bell() {
        let input = b"\x1b_Gf=100;\x07abc\x1b\\\x1bPq\x07\x1b\\";
        assert_eq!(scan(input), (input.to_vec(), vec![]));
    }

    #[test]
    fn osc9_and_osc777_are_notifications() {
        let input = b"\x1b]9;Claude \xe5\xae\x8c\xe6\x88\x90\x07\x1b]777;notify;Codex;\xe7\xad\x89\xe4\xbd\xa0\x1b\\";
        let want = vec![
            Signal::Notify {
                title: None,
                body: "Claude 完成".into(),
            },
            Signal::Notify {
                title: Some("Codex".into()),
                body: "等你".into(),
            },
        ];
        assert_eq!(scan(input), (input.to_vec(), want.clone()));
        assert_eq!(scan_bytewise(input), (input.to_vec(), want));
    }

    #[test]
    fn osc9_progress_subcommand_is_not_a_notification() {
        let input = b"\x1b]9;4;1;50\x07\x1b]777;preexec\x07";
        assert_eq!(scan(input), (input.to_vec(), vec![]));
    }

    #[test]
    fn escape_interrupting_a_title_flushes_it_unchanged() {
        let input = b"\x1b]2;half\x1b[31mred";
        assert_eq!(scan(input), (input.to_vec(), vec![]));
    }

    #[test]
    fn overlong_title_is_released_unchanged() {
        let mut input = b"\x1b]2;".to_vec();
        input.extend(std::iter::repeat_n(b'a', MAX_HELD + 10));
        input.push(BEL);
        let (out, sig) = scan(&input);
        assert_eq!(out, input);
        assert!(sig.is_empty());
    }

    #[test]
    fn double_escape_inside_other_osc_is_not_duplicated() {
        // OSC 正文里 ESC 后面又跟 ESC：两个 ESC 原样各出一次（之前第二个 ESC 会多出一个）。
        let input = b"\x1b]8;;u\x1b\x1b[31mred\x1b]52;c;\x1b\x1b\\";
        assert_eq!(scan(input), (input.to_vec(), vec![]));
        assert_eq!(scan_bytewise(input), (input.to_vec(), vec![]));
    }

    #[test]
    fn osc_without_params_passes_through() {
        let input = b"\x1b]\x07\x1b]abc\x07";
        assert_eq!(scan(input), (input.to_vec(), vec![]));
    }

    #[test]
    fn osc7501_states_are_signals_with_both_terminators() {
        let cases = [
            ("working", Some(Status::Working)),
            ("blocked", Some(Status::Blocked)),
            ("done", Some(Status::Done)),
            ("error", Some(Status::Error)),
            ("idle", None),
            ("clear", None),
            ("bogus", None),
        ];
        for (state, want) in cases {
            for end in ["\x07", "\x1b\\"] {
                let input = format!("a\x1b]7501;state={state}:app=hermes{end}b");
                let (out, sig) = scan(input.as_bytes());
                assert_eq!(out, input.as_bytes(), "{state} 字节要原样");
                let want: Vec<_> = want.into_iter().map(Signal::Status).collect();
                assert_eq!(sig, want, "{state} {end:?}");
                assert_eq!(scan_bytewise(input.as_bytes()), (input.into_bytes(), want));
            }
        }
    }

    #[test]
    fn osc7501_state_can_come_after_other_keys() {
        let input = b"\x1b]7501;app=opencode:kind=permission:state=blocked:msg=QXBwbHk/\x1b\\";
        assert_eq!(
            scan(input),
            (input.to_vec(), vec![Signal::Status(Status::Blocked)])
        );
    }

    #[test]
    fn osc7501_split_across_reads_is_recognised() {
        let input = b"x\x1b]7501;state=done:app=hermes\x1b\\y\x1b]7501;state=error\x07z";
        let want = vec![Signal::Status(Status::Done), Signal::Status(Status::Error)];
        // 每个位置切一刀都一样。
        for cut in 0..input.len() {
            let (a, b) = input.split_at(cut);
            assert_eq!(
                scan_chunks(&[a, b]),
                (input.to_vec(), want.clone()),
                "切在 {cut}"
            );
        }
    }

    #[test]
    fn osc7501_query_and_missing_state_are_ignored() {
        let input = b"\x1b]7501;?\x07\x1b]7501;app=hermes\x07\x1b]7501;xstate=done\x07\x1b]75010;state=done\x07";
        assert_eq!(scan(input), (input.to_vec(), vec![]));
    }

    /// OpenCode 2.x（program-status.ts）：A 等确认时根记录是 blocked（只有它一个时就是 A 的那条，不带 id）；
    /// 三秒后 B 开工，根记录没变不重发，只发一条带 B 的 id 的 working。子记录不能盖掉根记录。
    #[test]
    fn osc7501_child_records_do_not_override_the_root() {
        let mut s = Scanner::new(PREFIX);
        let (mut out, mut sig) = (Vec::new(), Vec::new());
        s.feed(
            b"\x1b]7501;state=blocked:kind=permission:app=opencode:title=QQ\x1b\\\x1b]7501;state=blocked:kind=permission:id=ses_a:app=opencode\x1b\\",
            &mut out,
            &mut sig,
        );
        assert_eq!(sig, vec![Signal::Status(Status::Blocked)], "只报根记录");
        assert!(s.cli_blocked());
        let child = b"\x1b]7501;state=working:id=ses_b:app=opencode:title=Qg\x1b\\";
        let (mut out, mut sig) = (Vec::new(), Vec::new());
        s.feed(child, &mut out, &mut sig);
        assert_eq!(out, child.to_vec(), "原样放行");
        assert_eq!(sig, vec![], "子记录的 working 不报");
        assert!(s.cli_blocked(), "根记录还是 blocked");
        // A 答完：根记录改成 working（B 还在跑，working 只剩一条时也不带 id）。
        s.feed(
            b"\x1b]7501;state=working:app=opencode\x07",
            &mut out,
            &mut sig,
        );
        assert_eq!(sig, vec![Signal::Status(Status::Working)]);
        assert!(!s.cli_blocked());
    }

    #[test]
    fn osc7501_idle_and_clear_end_blocked_and_last_key_wins() {
        let mut s = Scanner::new(PREFIX);
        let (mut out, mut sig) = (Vec::new(), Vec::new());
        for (input, blocked) in [
            (&b"\x1b]7501;state=blocked\x07"[..], true),
            (b"\x1b]7501;state=idle:app=hermes\x07", false),
            (b"\x1b]7501; state = blocked \x07", true),
            (b"\x1b]7501;state=clear\x07", false),
            (b"\x1b]7501;state=blocked:state=done\x07", false),
            (b"\x1b]7501;state=blocked\x07", true),
            // 不认识的状态、`id=` 为空（不合语法，规范说整条忽略）都不改根记录。
            (b"\x1b]7501;state=paused\x07", true),
            (b"\x1b]7501;state=idle:id=\x07", true),
        ] {
            s.feed(input, &mut out, &mut sig);
            assert_eq!(
                s.cli_blocked(),
                blocked,
                "{:?}",
                String::from_utf8_lossy(input)
            );
        }
        assert_eq!(
            sig,
            vec![
                Signal::Status(Status::Blocked),
                Signal::Status(Status::Blocked),
                Signal::Status(Status::Done),
                Signal::Status(Status::Blocked),
            ]
        );
    }

    #[test]
    fn gemini_action_required_title_means_waiting_once() {
        let input = "\x1b]0;✦  Working… (demo)\x07\x1b]0;✋  Action Required (demo)\x07\x1b]0;✋  Action Required (demo)\x07\x1b]0;◇  Ready (demo)\x07\x1b]2;✋ Action Required\x1b\\";
        let (out, sig) = scan(input.as_bytes());
        let want_out = input
            .replace("\x1b]0;", &format!("\x1b]0;{PREFIX}"))
            .replace("\x1b]2;", &format!("\x1b]2;{PREFIX}"));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            want_out,
            "只加前缀，其余原样"
        );
        let title = |t: &str| Signal::Title(t.to_owned());
        assert_eq!(
            sig,
            vec![
                title("✦  Working… (demo)"),
                title("✋  Action Required (demo)"),
                Signal::Status(Status::Blocked),
                title("✋  Action Required (demo)"),
                title("◇  Ready (demo)"),
                title("✋ Action Required"),
                Signal::Status(Status::Blocked),
            ]
        );
        assert_eq!(scan_bytewise(input.as_bytes()).1, sig);
    }

    #[test]
    fn titles_merely_mentioning_action_required_are_not_waiting() {
        let (_, sig) = scan("\x1b]2;fix Action Required banner\x07".as_bytes());
        assert_eq!(
            sig,
            vec![Signal::Title("fix Action Required banner".into())]
        );
    }

    #[test]
    fn boundary_is_only_where_the_emitted_bytes_are_whole() {
        // 每一段喂完后，已放出的字节是不是停在完整的地方。
        let steps: [(&[u8], bool); 12] = [
            (b"plain", true),
            (b"\x1b[3", false),
            (b"1m", true),
            (b"\xe4\xb8", false),
            (b"\xad", true),
            (b"\x1b(", false),
            (b"B", true),
            (b"\x1b]8;;http://x", false),
            (b"\x07", true),
            (b"\x1bPq#0", false),
            (b"\x1b\\", true),
            // 标题序列整段暂扣、还没放出去：放出去的停在它前面，是完整的。
            (b"\x1b]2;half", true),
        ];
        let mut s = Scanner::new("P ");
        for (input, whole) in steps {
            let (mut out, mut sig) = (Vec::new(), Vec::new());
            s.feed(input, &mut out, &mut sig);
            assert_eq!(s.at_boundary(), whole, "喂完 {input:?}");
        }
        s.set_prefix("Q ");
        let (mut out, mut sig) = (Vec::new(), Vec::new());
        s.feed(b"\x07", &mut out, &mut sig);
        assert_eq!(out, b"\x1b]2;Q half\x07", "暂扣着的标题用新前缀");
        assert!(s.at_boundary());
    }
}
