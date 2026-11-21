//! 输出扫描：PTY 输出原样放行，只改写 OSC 0/1/2 标题（加前缀），并认出 BEL、OSC 9、OSC 777 通知。
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
    /// DCS/APC/PM/SOS：原样放行到 ST，里面的 BEL 不算响铃。
    StringSeq,
    StringSeqEsc,
}

pub struct Scanner {
    prefix: Vec<u8>,
    state: State,
    held: Vec<u8>,
}

impl Scanner {
    /// `prefix` 加在 CLI 标题前，如 `✻ huitai · `。
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.as_bytes().to_vec(),
            state: State::Ground,
            held: Vec::new(),
        }
    }

    /// 处理一段输出：改写后的字节追加到 `out`，认出的信号追加到 `signals`。
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>, signals: &mut Vec<Signal>) {
        for &b in input {
            self.byte(b, out, signals);
        }
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
                    finish_note(ps, &note, signals);
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
            State::StringSeqEsc => {
                out.push(b);
                match b {
                    b'\\' => State::Ground,
                    ESC => State::StringSeqEsc,
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
                finish_note(ps, &note, signals);
                State::Ground
            }
            CAN | SUB => {
                out.push(b);
                State::Ground
            }
            _ => {
                out.push(b);
                if matches!(ps, Some(9 | 777)) && note.len() < MAX_NOTE {
                    note.push(b);
                }
                State::OscOther { ps, note }
            }
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
        signals.push(Signal::Title(String::from_utf8_lossy(body).into_owned()));
        out.extend_from_slice(&self.held[..body_start]);
        out.extend_from_slice(&self.prefix);
        out.extend_from_slice(body);
        out.extend_from_slice(terminator);
        self.held.clear();
    }
}

fn parse_ps(digits: &[u8]) -> Option<u32> {
    std::str::from_utf8(digits).ok()?.parse().ok()
}

fn finish_note(ps: Option<u32>, note: &[u8], signals: &mut Vec<Signal>) {
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
        _ => {}
    }
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
}
