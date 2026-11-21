//! 进会话台切英文、离开切回（docs/specs/keys-and-ime.md 第 1 条）。用假输入源，不碰本机真输入法。

use std::cell::RefCell;
use std::io;

use huitai::ime::{InputSource, InputSources, Switcher, pick_ascii};

const PINYIN: &str = "com.apple.inputmethod.SCIM.ITABC";
const ABC: &str = "com.apple.keylayout.ABC";
const US: &str = "com.apple.keylayout.US";
const DVORAK: &str = "com.apple.keylayout.Dvorak";
const KANA: &str = "com.apple.inputmethod.Kotoeri.RomajiTyping.Japanese";

/// 假输入源：记当前是哪个、切过哪些。
struct Fake {
    current: RefCell<String>,
    layouts: Vec<String>,
    selected: RefCell<Vec<String>>,
    fail: bool,
}

fn fake(current: &str, layouts: &[&str]) -> Fake {
    Fake {
        current: RefCell::new(current.to_owned()),
        layouts: layouts.iter().map(|s| (*s).to_owned()).collect(),
        selected: RefCell::new(Vec::new()),
        fail: false,
    }
}

impl InputSources for &Fake {
    fn current(&self) -> Option<InputSource> {
        let id = self.current.borrow().clone();
        let ascii = id.contains("keylayout");
        Some(InputSource { id, ascii })
    }
    fn ascii_layouts(&self) -> Vec<String> {
        self.layouts.clone()
    }
    fn select(&self, id: &str) -> io::Result<()> {
        if self.fail {
            return Err(io::Error::other("切不动"));
        }
        self.selected.borrow_mut().push(id.to_owned());
        *self.current.borrow_mut() = id.to_owned();
        Ok(())
    }
}

fn selected(f: &Fake) -> Vec<String> {
    f.selected.borrow().clone()
}

fn user_switches(f: &Fake, id: &str) {
    *f.current.borrow_mut() = id.to_owned();
}

#[test]
fn focus_in_switches_to_abc_and_focus_out_switches_back() {
    let f = fake(PINYIN, &[US, ABC]);
    let mut s = Switcher::new(&f, true);
    s.focus_gained().unwrap();
    assert_eq!(selected(&f), [ABC], "优先 ABC");
    s.focus_lost().unwrap();
    assert_eq!(selected(&f), [ABC, PINYIN], "切回原来的拼音");
    // 再进再出，照样切。
    s.focus_gained().unwrap();
    s.focus_lost().unwrap();
    assert_eq!(selected(&f), [ABC, PINYIN, ABC, PINYIN]);
}

#[test]
fn already_english_does_nothing() {
    let f = fake(US, &[US, ABC]);
    let mut s = Switcher::new(&f, true);
    s.focus_gained().unwrap();
    s.focus_lost().unwrap();
    assert!(selected(&f).is_empty());
}

#[test]
fn repeated_focus_in_keeps_the_original() {
    // 终端开焦点上报时也报一次「拿到焦点」：已经切过就不再记一遍（不然记下的是 ABC，切不回拼音）。
    let f = fake(PINYIN, &[ABC]);
    let mut s = Switcher::new(&f, true);
    s.focus_gained().unwrap();
    s.focus_gained().unwrap();
    s.focus_lost().unwrap();
    assert_eq!(selected(&f), [ABC, PINYIN]);
}

#[test]
fn leave_and_back_around_tabs_and_handing_over_terminal() {
    let f = fake(PINYIN, &[ABC]);
    let mut s = Switcher::new(&f, true);
    s.focus_gained().unwrap();
    // 打开对话标签页 / 在当前窗口接上对话：切回拼音给对话用。
    s.leave().unwrap();
    assert_eq!(selected(&f), [ABC, PINYIN]);
    // 对话退出回到会话台，还在前台：再切英文。
    s.back().unwrap();
    assert_eq!(selected(&f), [ABC, PINYIN, ABC]);
    // 退出会话台：切回。
    s.leave().unwrap();
    assert_eq!(selected(&f), [ABC, PINYIN, ABC, PINYIN]);
}

#[test]
fn back_after_focus_lost_does_nothing() {
    let f = fake(PINYIN, &[ABC]);
    let mut s = Switcher::new(&f, true);
    s.focus_gained().unwrap();
    s.focus_lost().unwrap();
    s.back().unwrap();
    assert_eq!(selected(&f), [ABC, PINYIN]);
}

#[test]
fn without_focus_report_nothing_happens() {
    // 终端不报焦点：从来收不到 focus_gained，交出终端回来也不切。
    let f = fake(PINYIN, &[ABC]);
    let mut s = Switcher::new(&f, true);
    s.leave().unwrap();
    s.back().unwrap();
    assert!(selected(&f).is_empty());
}

#[test]
fn user_changed_source_inside_is_left_alone() {
    // 在会话台里自己换成了日文：离开时不把它改成拼音。
    let f = fake(PINYIN, &[ABC]);
    let mut s = Switcher::new(&f, true);
    s.focus_gained().unwrap();
    user_switches(&f, KANA);
    s.focus_lost().unwrap();
    assert_eq!(selected(&f), [ABC]);
    assert_eq!(*f.current.borrow(), KANA);
}

#[test]
fn turned_off_never_switches_and_turning_off_restores() {
    let f = fake(PINYIN, &[ABC]);
    let mut s = Switcher::new(&f, false);
    s.focus_gained().unwrap();
    assert!(selected(&f).is_empty());
    // 在前台时打开：马上切英文；关掉：马上切回。
    s.set_enabled(true).unwrap();
    assert_eq!(selected(&f), [ABC]);
    s.set_enabled(false).unwrap();
    assert_eq!(selected(&f), [ABC, PINYIN]);
    assert!(!s.enabled());
}

#[test]
fn no_english_layout_enabled_does_nothing() {
    let f = fake(PINYIN, &[]);
    let mut s = Switcher::new(&f, true);
    s.focus_gained().unwrap();
    s.focus_lost().unwrap();
    assert!(selected(&f).is_empty());
}

#[test]
fn failed_switch_is_reported_and_not_remembered() {
    let mut f = fake(PINYIN, &[ABC]);
    f.fail = true;
    let mut s = Switcher::new(&f, true);
    assert!(s.focus_gained().is_err());
    // 没切成就没东西要切回。
    assert!(s.focus_lost().is_ok());
}

#[test]
fn picks_abc_then_us_then_first_enabled() {
    let l = |ids: &[&str]| ids.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert_eq!(pick_ascii(&l(&[DVORAK, US, ABC])).as_deref(), Some(ABC));
    assert_eq!(pick_ascii(&l(&[DVORAK, US])).as_deref(), Some(US));
    assert_eq!(pick_ascii(&l(&[DVORAK])).as_deref(), Some(DVORAK));
    assert_eq!(pick_ascii(&[]), None);
}
