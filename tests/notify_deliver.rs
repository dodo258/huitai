//! 通知文字、OSC 序列和投递路线。系统通知用假实现，不弹真实通知。

use std::cell::RefCell;
use std::io::{self, Write};

use huitai::core::SessionId;
use huitai::notify::{Alert, Desktop, Message, OscKind, Tab, deliver, osc_kind, osc_sequence};

#[derive(Default)]
struct FakeDesktop(RefCell<Vec<Message>>);

impl Desktop for FakeDesktop {
    fn show(&self, msg: &Message) -> io::Result<()> {
        self.0.borrow_mut().push(msg.clone());
        Ok(())
    }
}

struct BrokenTab;

impl Write for BrokenTab {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn alert(title: Option<&str>, body: Option<&str>) -> Alert {
    Alert {
        id: SessionId::from("a"),
        title: title.map(str::to_owned),
        body: body.map(str::to_owned),
    }
}

fn msg() -> Message {
    Message {
        title: "✻ huitai · 修登录".to_owned(),
        body: "等你确认".to_owned(),
    }
}

#[test]
fn terminals_map_to_their_notification_sequence() {
    assert_eq!(osc_kind("ghostty"), Some(OscKind::Osc777));
    assert_eq!(osc_kind("WezTerm"), Some(OscKind::Osc777));
    assert_eq!(osc_kind("Kaku"), Some(OscKind::Osc777));
    assert_eq!(osc_kind("iTerm.app"), Some(OscKind::Osc9));
    assert_eq!(osc_kind("Apple_Terminal"), None);
    assert_eq!(osc_kind(""), None);
}

#[test]
fn osc_sequences_are_well_formed() {
    assert_eq!(
        osc_sequence(OscKind::Osc777, &msg()),
        "\x1b]777;notify;✻ huitai · 修登录;等你确认\x07".as_bytes()
    );
    assert_eq!(
        osc_sequence(OscKind::Osc9, &msg()),
        "\x1b]9;✻ huitai · 修登录: 等你确认\x07".as_bytes()
    );
    let semi = Message {
        title: "a;b".to_owned(),
        body: "c;d".to_owned(),
    };
    assert_eq!(
        osc_sequence(OscKind::Osc777, &semi),
        "\x1b]777;notify;a；b;c;d\x07".as_bytes(),
        "标题里的分号不能当成分隔符"
    );
}

#[test]
fn message_uses_cli_text_or_a_default() {
    let label = "◎ app · 重构";
    assert_eq!(
        Message::new(&alert(Some("Codex"), Some("需要批准命令")), label),
        Message {
            title: label.to_owned(),
            body: "需要批准命令".to_owned()
        }
    );
    assert_eq!(
        Message::new(&alert(Some("只给了标题"), Some("  ")), label).body,
        "只给了标题"
    );
    assert_eq!(Message::new(&alert(None, None), label).body, "在等你回复");
}

#[test]
fn message_strips_control_characters_and_truncates() {
    let evil = alert(None, Some("hi\x1b]0;pwned\x07\x1b[2J\nthere\u{9b}"));
    let m = Message::new(&evil, "t\x07itle");
    assert_eq!(m.title, "title");
    assert!(!m.body.chars().any(char::is_control), "{:?}", m.body);
    assert_eq!(m.body, "hi]0;pwned[2J there");
    let long = "长".repeat(500);
    let m = Message::new(&alert(None, Some(&long)), "x");
    assert_eq!(m.body.chars().count(), 201);
    assert!(m.body.ends_with('…'));
    let seq = osc_sequence(OscKind::Osc777, &Message::new(&evil, "x"));
    assert_eq!(
        seq.iter().filter(|&&b| b == 0x1b).count(),
        1,
        "只有开头那个 ESC"
    );
    assert_eq!(
        seq.iter().filter(|&&b| b == 0x07).count(),
        1,
        "只有结尾那个 BEL"
    );
}

#[test]
fn open_ui_tab_gets_the_osc_and_no_system_notification() {
    let desktop = FakeDesktop::default();
    let mut out = Vec::new();
    deliver(
        &msg(),
        Some(Tab {
            out: &mut out,
            kind: OscKind::Osc777,
        }),
        &desktop,
    )
    .unwrap();
    assert_eq!(out, osc_sequence(OscKind::Osc777, &msg()));
    assert!(desktop.0.borrow().is_empty());
}

#[test]
fn without_ui_tab_falls_back_to_system_notification() {
    let desktop = FakeDesktop::default();
    deliver(&msg(), None, &desktop).unwrap();
    assert_eq!(*desktop.0.borrow(), vec![msg()]);
}

#[test]
fn broken_ui_tab_falls_back_to_system_notification() {
    let desktop = FakeDesktop::default();
    let mut tab = BrokenTab;
    deliver(
        &msg(),
        Some(Tab {
            out: &mut tab,
            kind: OscKind::Osc9,
        }),
        &desktop,
    )
    .unwrap();
    assert_eq!(desktop.0.borrow().len(), 1);
}
