//! 健壮性测试共用：固定种子的伪随机数（同一种子每次生成一样的数据，失败能复现），
//! 以及把手写的脱敏样例行改坏的几种办法。不读任何真实会话。

#![allow(dead_code)]

use serde_json::{Value, json};

/// xorshift64*：够用的伪随机，不引依赖。
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `[0, n)`；`n == 0` 返回 0。
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }

    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// 中英文、emoji、组合字符、零宽字符、全角、控制字符混排的文字。
pub const MIXED: &[&str] = &[
    "修登录页",
    "fix checkout button",
    "🚀 发布 v2",
    "👨‍👩‍👧‍👦 家庭账本",
    "e\u{301}clair 组合字符",
    "零\u{200b}宽\u{200d}字符",
    "ＦＵＬＬＷＩＤＴＨ 全角",
    "tab\there",
    "换\n行",
    "\u{1b}[31m红色转义\u{1b}[0m",
    "\u{7}响铃\u{8}退格",
    "🇨🇳🇺🇸 国旗",
    "ﷺ 阿拉伯合字",
    "\u{202e}从右往左",
    "",
    " ",
    "a",
    "很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长的标题",
];

/// 换成随机类型的 JSON 值（字段类型不对）。
pub fn wrong_value(rng: &mut Rng) -> Value {
    match rng.below(9) {
        0 => Value::Null,
        1 => json!(rng.chance(50)),
        2 => json!(rng.next() as i64),
        3 => json!(-1),
        4 => json!(1e308),
        5 => json!([1, "x", null, {"type": "text"}]),
        6 => json!({"type": 7, "text": ["x"]}),
        7 => json!(rng.pick(MIXED).to_string()),
        _ => json!("2026-13-45T99:99:99.999+99:99"),
    }
}

/// 把一行 JSON 里随机一个字段（任意深度）换成类型不对的值。
pub fn mutate_value(v: &mut Value, rng: &mut Rng) {
    match v {
        Value::Object(map) if !map.is_empty() => {
            let i = rng.below(map.len());
            let key = map.keys().nth(i).cloned().expect("i < len");
            let child = map.get_mut(&key).expect("刚取到的键");
            if rng.chance(40) && (child.is_object() || child.is_array()) {
                mutate_value(child, rng);
            } else {
                *child = wrong_value(rng);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            let i = rng.below(items.len());
            if rng.chance(50) {
                mutate_value(&mut items[i], rng);
            } else {
                items[i] = wrong_value(rng);
            }
        }
        _ => *v = wrong_value(rng),
    }
}

/// 一行坏数据：截断、乱码、非 UTF-8、超长、类型不对、空白、深嵌套等，从好行 `good` 改出来。
pub fn bad_line(good: &str, rng: &mut Rng) -> Vec<u8> {
    match rng.below(10) {
        // 截断在任意字节（可能截在多字节字符中间）。
        0 => {
            let b = good.as_bytes();
            b[..rng.below(b.len())].to_vec()
        }
        1 => {
            let n = 1 + rng.below(200);
            rng.bytes(n)
        }
        // 非 UTF-8 混进 JSON 字符串。
        2 => {
            let mut b = good.as_bytes().to_vec();
            let at = rng.below(b.len());
            b.splice(at..at, [0xff, 0xfe, 0xc3, 0x28, 0xed, 0xa0, 0x80]);
            b
        }
        // 超长行（单行 70–300KB，比头尾窗口还长）。
        3 => {
            let n = 70_000 + rng.below(230_000);
            let mut v: Value = serde_json::from_str(good).expect("样例行是 JSON");
            v["pad"] = json!("长".repeat(n / 3));
            v.to_string().into_bytes()
        }
        4 | 5 => {
            let mut v: Value = serde_json::from_str(good).expect("样例行是 JSON");
            mutate_value(&mut v, rng);
            v.to_string().into_bytes()
        }
        6 => rng
            .pick(&[
                &b""[..],
                b"   ",
                b"\r",
                b"null",
                b"[]",
                b"123",
                b"\"str\"",
                b"{}",
                b"\xef\xbb\xbf{}",
                b"\0\0\0",
            ])
            .to_vec(),
        // 深嵌套（超过 serde_json 的递归上限）。
        7 => {
            let depth = 200 + rng.below(800);
            let mut s = "[".repeat(depth);
            s.push_str(&"]".repeat(depth));
            s.into_bytes()
        }
        // 两行粘在一起（中间少了换行）。
        8 => format!("{good}{good}").into_bytes(),
        _ => good.replace(':', "：").into_bytes(),
    }
}
