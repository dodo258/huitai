//! 托管层稳定性加测 1：透传字节完全一致（docs/specs/host-stability-tests.md）。
//!
//! 用 proptest 生成随机字节流（被切断的 UTF-8、各种 ESC 序列、OSC/DCS/APC、超长标题、纯随机字节），
//! 任意切块喂给标题改写过滤器（`host::scan::Scanner`）：
//! - 去掉标题前缀后，输出逐字节等于输入；前缀只出现在 `ESC ] <数字> ;` 后面，次数等于认出的标题数；
//! - 同一个流不管怎么切块，输出和认出的信号都一样。

use huitai::host::scan::{Scanner, Signal};
use proptest::prelude::*;

/// 和真实前缀同样形状：厂商标记、项目名、分隔点（多字节 UTF-8）。
const PREFIX: &str = "✻ 会话台 · ";
/// 流末尾补两个 CAN：任何没收尾的序列（暂扣的标题、半个 ESC）都会被放出，过滤器回到初始状态。
const FLUSH: &[u8] = b"\x18\x18";

fn piece() -> impl Strategy<Value = Vec<u8>> {
    let text = prop::collection::vec(any::<char>(), 0..12).prop_map(|cs| {
        let s: String = cs.into_iter().collect();
        s.into_bytes()
    });
    // 多字节字符只取前面几个字节：被切断的 UTF-8。
    let cut_utf8 = (any::<char>(), 0usize..4).prop_map(|(c, keep)| {
        let mut buf = [0u8; 4];
        let bytes = c.encode_utf8(&mut buf).as_bytes();
        bytes[..keep.min(bytes.len())].to_vec()
    });
    let raw = prop::collection::vec(any::<u8>(), 0..24);
    let csi = (prop::collection::vec(0u16..2000, 0..4), b'@'..=b'~').prop_map(|(ps, end)| {
        let ps: Vec<String> = ps.iter().map(u16::to_string).collect();
        let mut s = format!("\x1b[{}", ps.join(";")).into_bytes();
        s.push(end);
        s
    });
    let terminator = prop::sample::select(vec![
        &b"\x07"[..],
        b"\x1b\\",
        b"",
        b"\x18",
        b"\x1b[",
        b"\x1b\x1b",
    ]);
    let ps = prop::sample::select(vec![
        "0",
        "1",
        "2",
        "002",
        "8",
        "9",
        "52",
        "777",
        "",
        "12345678901234567890",
        "x",
    ]);
    let body = prop::collection::vec(any::<u8>(), 0..40);
    let osc = (ps, body, terminator.clone()).prop_map(|(ps, body, end)| {
        let mut s = format!("\x1b]{ps};").into_bytes();
        s.extend(body);
        s.extend(end);
        s
    });
    // 超长标题：超过暂扣上限（4096）的要原样放出。
    let long_title = (0u8..3, 3000usize..6000, terminator.clone()).prop_map(|(ps, n, end)| {
        let mut s = format!("\x1b]{ps};").into_bytes();
        s.extend("标题".repeat(n / 6).into_bytes());
        s.extend(end);
        s
    });
    let string_seq = (
        prop::sample::select(vec![b'P', b'_', b'^', b'X']),
        prop::collection::vec(any::<u8>(), 0..40),
        terminator,
    )
        .prop_map(|(kind, body, end)| {
            let mut s = vec![0x1b, kind];
            s.extend(body);
            s.extend(end);
            s
        });
    prop_oneof![
        4 => text,
        2 => cut_utf8,
        3 => raw,
        2 => csi,
        1 => Just(b"\x1b".to_vec()),
        1 => Just(b"\x07".to_vec()),
        4 => osc,
        1 => long_title,
        2 => string_seq,
    ]
}

fn stream() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(piece(), 0..40).prop_map(|ps| ps.concat())
}

/// 把 `input` 按 `cuts`（任意数，取模落进长度内）切开依次喂，返回输出和信号。
fn scan(input: &[u8], cuts: &[usize]) -> (Vec<u8>, Vec<Signal>) {
    let mut at: Vec<usize> = cuts.iter().map(|c| c % (input.len() + 1)).collect();
    at.push(0);
    at.push(input.len());
    at.sort_unstable();
    let mut s = Scanner::new(PREFIX);
    let (mut out, mut signals) = (Vec::new(), Vec::new());
    for w in at.windows(2) {
        s.feed(&input[w[0]..w[1]], &mut out, &mut signals);
    }
    (out, signals)
}

fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut hits = Vec::new();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            hits.push(i);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    hits
}

/// 前缀前面必须正好是标题序列的开头 `ESC ] <数字> ;`。
fn after_title_header(out: &[u8], at: usize) -> bool {
    let head = &out[..at];
    let Some(rest) = head.strip_suffix(b";") else {
        return false;
    };
    let digits = rest.iter().rev().take_while(|b| b.is_ascii_digit()).count();
    digits > 0 && rest[..rest.len() - digits].ends_with(b"\x1b]")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn output_equals_input_plus_title_prefix(body in stream(), cuts in prop::collection::vec(any::<usize>(), 0..16)) {
        let mut input = body;
        input.extend_from_slice(FLUSH);
        let (out, signals) = scan(&input, &cuts);
        let hits = find_all(&out, PREFIX.as_bytes());
        let titles = signals.iter().filter(|s| matches!(s, Signal::Title(_))).count();
        prop_assert_eq!(hits.len(), titles, "前缀次数和认出的标题数不一样");
        let mut stripped = Vec::with_capacity(out.len());
        let mut from = 0;
        for &h in &hits {
            prop_assert!(after_title_header(&out, h), "前缀出现在标题序列以外：位置 {}", h);
            stripped.extend_from_slice(&out[from..h]);
            from = h + PREFIX.len();
        }
        stripped.extend_from_slice(&out[from..]);
        prop_assert_eq!(stripped, input);
    }

    #[test]
    fn any_chunking_gives_the_same_result(input in stream(), cuts in prop::collection::vec(any::<usize>(), 1..64)) {
        prop_assert_eq!(scan(&input, &cuts), scan(&input, &[]));
    }

    #[test]
    fn byte_by_byte_equals_whole(input in stream()) {
        let every: Vec<usize> = (0..input.len()).collect();
        prop_assert_eq!(scan(&input, &every), scan(&input, &[]));
    }
}
