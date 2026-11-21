//! 热键写法解析：`alt+space`、`cmd+shift+k`、`⌃⌥F5` 这类 → Carbon 虚拟键码和修饰键掩码。

use std::fmt;
use std::str::FromStr;

/// Carbon 修饰键掩码（Events.h：cmdKey、shiftKey、optionKey、controlKey）。
pub const CMD: u32 = 1 << 8;
pub const SHIFT: u32 = 1 << 9;
pub const OPTION: u32 = 1 << 11;
pub const CONTROL: u32 = 1 << 12;

/// 一个全局热键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HotKey {
    /// Carbon 虚拟键码（`kVK_*`，按键的物理位置，和输入法无关）。
    pub key_code: u32,
    /// 修饰键掩码，见本文件的 `CMD`/`SHIFT`/`OPTION`/`CONTROL`。
    pub modifiers: u32,
}

/// 写法不对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseHotKeyError(String);

impl fmt::Display for ParseHotKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseHotKeyError {}

impl FromStr for HotKey {
    type Err = ParseHotKeyError;

    /// 用 `+` 连接，大小写不限；修饰键可写 `cmd/command/⌘`、`shift/⇧`、`alt/opt/option/⌥`、`ctrl/control/⌃`，
    /// 符号写法也可以不带 `+`（`⌥space`）。必须带修饰键（F1–F12 除外），免得抢走普通按键。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = |why: &str| ParseHotKeyError(format!("热键「{s}」{why}"));
        let mut modifiers = 0;
        let mut rest = s.trim();
        // 先剥掉开头的符号修饰键（⌘⇧⌥⌃）。
        while let Some(c) = rest.chars().next() {
            let Some(m) = symbol_modifier(c) else { break };
            modifiers |= m;
            rest = rest[c.len_utf8()..].trim_start_matches('+');
        }
        let mut parts: Vec<&str> = rest.split('+').map(str::trim).collect();
        let key = parts
            .pop()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| err("少了按键"))?;
        for part in parts {
            let m = word_modifier(&part.to_ascii_lowercase())
                .ok_or_else(|| err(&format!("里的「{part}」不是修饰键")))?;
            if modifiers & m != 0 {
                return Err(err(&format!("里的「{part}」重复了")));
            }
            modifiers |= m;
        }
        let key_lower = key.to_ascii_lowercase();
        let key_code = key_code(&key_lower).ok_or_else(|| err(&format!("里的「{key}」认不出")))?;
        if modifiers == 0 && !is_function_key(&key_lower) {
            return Err(err("至少要带一个修饰键（⌘⇧⌥⌃）"));
        }
        Ok(Self {
            key_code,
            modifiers,
        })
    }
}

fn symbol_modifier(c: char) -> Option<u32> {
    match c {
        '⌘' => Some(CMD),
        '⇧' => Some(SHIFT),
        '⌥' => Some(OPTION),
        '⌃' => Some(CONTROL),
        _ => None,
    }
}

fn word_modifier(word: &str) -> Option<u32> {
    match word {
        "cmd" | "command" | "⌘" => Some(CMD),
        "shift" | "⇧" => Some(SHIFT),
        "alt" | "opt" | "option" | "⌥" => Some(OPTION),
        "ctrl" | "control" | "⌃" => Some(CONTROL),
        _ => None,
    }
}

fn is_function_key(key: &str) -> bool {
    key.strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=12).contains(&n))
}

/// 美式键盘布局的虚拟键码（HIToolbox Events.h 的 `kVK_*`）。
fn key_code(key: &str) -> Option<u32> {
    const LETTERS: [u32; 26] = [
        0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, 0x2D, 0x1F,
        0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06,
    ];
    const DIGITS: [u32; 10] = [0x1D, 0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19];
    const FUNCTION: [u32; 12] = [
        0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F,
    ];
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_lowercase() {
            return Some(LETTERS[(c as u8 - b'a') as usize]);
        }
        if c.is_ascii_digit() {
            return Some(DIGITS[(c as u8 - b'0') as usize]);
        }
    }
    if is_function_key(key) {
        let n: usize = key[1..].parse().ok()?;
        return Some(FUNCTION[n - 1]);
    }
    Some(match key {
        "space" | "空格" => 0x31,
        "return" | "enter" => 0x24,
        "tab" => 0x30,
        "escape" | "esc" => 0x35,
        "delete" | "backspace" => 0x33,
        "-" | "minus" => 0x1B,
        "=" | "equal" => 0x18,
        "[" => 0x21,
        "]" => 0x1E,
        ";" => 0x29,
        "'" => 0x27,
        "," | "comma" => 0x2B,
        "." | "period" => 0x2F,
        "/" | "slash" => 0x2C,
        "\\" | "backslash" => 0x2A,
        "`" | "grave" => 0x32,
        _ => return None,
    })
}
