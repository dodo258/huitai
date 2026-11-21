//! 热键写法解析。只测解析，不注册真实热键（会抢走用户的按键）。

use huitai::core::Config;
use huitai::hotkey::{CMD, CONTROL, HotKey, OPTION, SHIFT};

fn parse(s: &str) -> Result<HotKey, String> {
    s.parse::<HotKey>().map_err(|e| e.to_string())
}

fn key(key_code: u32, modifiers: u32) -> HotKey {
    HotKey {
        key_code,
        modifiers,
    }
}

#[test]
fn default_config_is_option_space() {
    assert_eq!(parse(&Config::default().hotkey), Ok(key(0x31, OPTION)));
}

#[test]
fn accepts_words_symbols_and_case() {
    assert_eq!(parse("Alt+Space"), Ok(key(0x31, OPTION)));
    assert_eq!(parse("⌥space"), Ok(key(0x31, OPTION)));
    assert_eq!(parse("⌥+空格"), Ok(key(0x31, OPTION)));
    assert_eq!(parse("cmd+shift+k"), Ok(key(0x28, CMD | SHIFT)));
    assert_eq!(parse("⌘⇧K"), Ok(key(0x28, CMD | SHIFT)));
    assert_eq!(parse("ctrl + option + f5"), Ok(key(0x60, CONTROL | OPTION)));
    assert_eq!(parse("control+a"), Ok(key(0x00, CONTROL)));
    assert_eq!(parse("opt+0"), Ok(key(0x1D, OPTION)));
    assert_eq!(parse("command+return"), Ok(key(0x24, CMD)));
    assert_eq!(parse("alt+z"), Ok(key(0x06, OPTION)));
}

#[test]
fn function_keys_need_no_modifier() {
    assert_eq!(parse("F12"), Ok(key(0x6F, 0)));
    assert_eq!(parse("f1"), Ok(key(0x7A, 0)));
}

#[test]
fn rejects_bad_combinations_with_a_reason() {
    for (s, why) in [
        ("space", "修饰键"),
        ("k", "修饰键"),
        ("alt+", "少了按键"),
        ("", "少了按键"),
        ("hyper+k", "不是修饰键"),
        ("alt+alt+k", "重复"),
        ("alt+ü", "认不出"),
        ("alt+f13", "认不出"),
    ] {
        let err = parse(s).expect_err(s);
        assert!(err.contains(why), "{s}: {err}");
    }
}
