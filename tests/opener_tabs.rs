//! opener：同一对话已开时聚焦、记录开着的对话、恢复上次所有标签。用假终端，不碰真终端。

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use huitai::core::SessionId;
use huitai::opener::{InlineOpener, Opened, Opener, Outcome, TabHandle, Tabs, attach_command};
use huitai::sources::Command;

/// 假终端：记下开过的命令和聚焦过的标签页；`alive` 是还开着的终端 id。
#[derive(Default)]
struct FakeTerminal {
    opened: RefCell<Vec<Command>>,
    titles: RefCell<Vec<String>>,
    focused: RefCell<Vec<String>>,
    alive: RefCell<HashSet<String>>,
    fail_open: bool,
}

impl FakeTerminal {
    fn close(&self, id: &str) {
        self.alive.borrow_mut().remove(id);
    }
}

impl Opener for FakeTerminal {
    fn open(&self, cmd: &Command, title: &str) -> io::Result<Opened> {
        if self.fail_open {
            return Err(io::Error::other("开不了"));
        }
        self.titles.borrow_mut().push(title.to_owned());
        let mut opened = self.opened.borrow_mut();
        opened.push(cmd.clone());
        let id = format!("term-{}", opened.len());
        self.alive.borrow_mut().insert(id.clone());
        Ok(Opened::Tab(TabHandle::Ghostty { terminal_id: id }))
    }

    fn focus(&self, handle: &TabHandle) -> io::Result<bool> {
        let TabHandle::Ghostty { terminal_id } = handle else {
            return Ok(false);
        };
        if !self.alive.borrow().contains(terminal_id) {
            return Ok(false);
        }
        self.focused.borrow_mut().push(terminal_id.clone());
        Ok(true)
    }
}

const T: &str = "✻ huitai · 修 \"引号\"";

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("huitai-opener-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn cmd(id: &str) -> Command {
    attach_command(
        Path::new("/bin/ai"),
        &SessionId::from(id),
        Path::new("/tmp/proj"),
    )
}

#[test]
fn second_open_of_same_session_focuses_existing_tab() {
    let dir = temp_dir("focus");
    let mut tabs = Tabs::load(&dir.join("tabs.json")).unwrap();
    let term = FakeTerminal::default();
    let s1 = SessionId::from("s1");

    assert_eq!(
        tabs.open_or_focus(&term, &s1, &cmd("s1"), T).unwrap(),
        Outcome::Opened
    );
    assert_eq!(
        tabs.open_or_focus(&term, &s1, &cmd("s1"), T).unwrap(),
        Outcome::Focused
    );
    assert_eq!(term.opened.borrow().len(), 1, "不该开第二个标签页");
    assert_eq!(*term.focused.borrow(), ["term-1"]);

    // 另一条对话照常开新的。
    let s2 = SessionId::from("s2");
    assert_eq!(
        tabs.open_or_focus(&term, &s2, &cmd("s2"), T).unwrap(),
        Outcome::Opened
    );
    assert_eq!(term.opened.borrow().len(), 2);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn closed_tab_is_reopened_and_record_updated() {
    let dir = temp_dir("reopen");
    let path = dir.join("tabs.json");
    let mut tabs = Tabs::load(&path).unwrap();
    let term = FakeTerminal::default();
    let s1 = SessionId::from("s1");
    tabs.open_or_focus(&term, &s1, &cmd("s1"), T).unwrap();
    term.close("term-1");

    assert_eq!(
        tabs.open_or_focus(&term, &s1, &cmd("s1"), T).unwrap(),
        Outcome::Opened
    );
    assert_eq!(
        tabs.records()[&s1].handle,
        TabHandle::Ghostty {
            terminal_id: "term-2".to_owned()
        }
    );
    // 记录落盘，换个进程读回来还是新的那个。
    assert_eq!(
        Tabs::load(&path).unwrap().records()[&s1].handle,
        TabHandle::Ghostty {
            terminal_id: "term-2".to_owned()
        }
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn handle_from_other_terminal_opens_new_tab() {
    let dir = temp_dir("switch");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tabs.json");
    // 上次在 Kaku 里开的，这次在 Ghostty（假终端）里打开。
    fs::write(
        &path,
        r#"{"s1":{"program":"/bin/ai","args":["attach","s1"],"cwd":"/tmp/proj",
            "handle":{"terminal":"kaku","pane_id":3,"tty":"/dev/ttys009","command":"/bin/ai attach s1"}}}"#,
    )
    .unwrap();
    let mut tabs = Tabs::load(&path).unwrap();
    let term = FakeTerminal::default();
    let s1 = SessionId::from("s1");
    assert_eq!(
        tabs.open_or_focus(&term, &s1, &cmd("s1"), T).unwrap(),
        Outcome::Opened
    );
    assert!(matches!(
        tabs.records()[&s1].handle,
        TabHandle::Ghostty { .. }
    ));
    // 旧版记录没有 title 字段也能读；重开后记下新标题。
    assert_eq!(Tabs::load(&path).unwrap().records()[&s1].title, T);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn restore_all_reopens_closed_and_focuses_open_ones() {
    let dir = temp_dir("restore");
    let path = dir.join("tabs.json");
    let term = FakeTerminal::default();
    {
        let mut tabs = Tabs::load(&path).unwrap();
        for id in ["a", "b", "c"] {
            tabs.open_or_focus(&term, &SessionId::from(id), &cmd(id), T)
                .unwrap();
        }
    }
    // 模拟重启：只有 b 的标签页还在。
    term.close("term-1");
    term.close("term-3");

    let mut tabs = Tabs::load(&path).unwrap();
    let results = tabs.restore_all(&term);
    let outcomes: Vec<(String, Outcome)> = results
        .into_iter()
        .map(|(id, r)| (id.to_string(), r.unwrap()))
        .collect();
    assert_eq!(
        outcomes,
        [
            ("a".to_owned(), Outcome::Opened),
            ("b".to_owned(), Outcome::Focused),
            ("c".to_owned(), Outcome::Opened)
        ]
    );
    // 重开时用的是记下的命令、目录和标题。
    assert!(term.titles.borrow().iter().all(|t| t == T));
    let reopened = term.opened.borrow();
    assert_eq!(reopened.len(), 5);
    assert_eq!(reopened[3].args, ["attach", "a"]);
    assert_eq!(reopened[4].cwd, PathBuf::from("/tmp/proj"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn forget_removes_session_from_restore_list() {
    let dir = temp_dir("forget");
    let path = dir.join("tabs.json");
    let term = FakeTerminal::default();
    let mut tabs = Tabs::load(&path).unwrap();
    let s1 = SessionId::from("s1");
    tabs.open_or_focus(&term, &s1, &cmd("s1"), T).unwrap();
    tabs.forget(&s1).unwrap();
    tabs.forget(&SessionId::from("never-opened")).unwrap();
    assert!(Tabs::load(&path).unwrap().records().is_empty());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn failed_open_is_reported_and_not_recorded() {
    let dir = temp_dir("fail");
    let path = dir.join("tabs.json");
    let term = FakeTerminal {
        fail_open: true,
        ..Default::default()
    };
    let mut tabs = Tabs::load(&path).unwrap();
    let err = tabs
        .open_or_focus(&term, &SessionId::from("s1"), &cmd("s1"), T)
        .unwrap_err();
    assert!(err.to_string().contains("开不了"));
    assert!(tabs.records().is_empty());
    assert!(!path.exists());
}

#[test]
fn inline_runs_in_current_window_and_is_not_recorded() {
    let dir = temp_dir("inline");
    fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("ran");
    let run = Command {
        program: "/usr/bin/touch".to_owned(),
        args: vec!["ran".to_owned()],
        cwd: dir.clone(),
    };
    let mut tabs = Tabs::load(&dir.join("tabs.json")).unwrap();
    let s1 = SessionId::from("s1");
    assert_eq!(
        tabs.open_or_focus(&InlineOpener, &s1, &run, T).unwrap(),
        Outcome::Inline
    );
    assert!(marker.exists(), "命令要在项目目录里跑");
    assert!(tabs.records().is_empty());

    let fail = Command {
        program: "/usr/bin/false".to_owned(),
        args: vec![],
        cwd: dir.clone(),
    };
    assert!(tabs.open_or_focus(&InlineOpener, &s1, &fail, T).is_err());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn malformed_record_file_is_an_error_naming_the_path() {
    let dir = temp_dir("malformed");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tabs.json");
    fs::write(&path, "{oops").unwrap();
    let err = Tabs::load(&path).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("tabs.json"));
    fs::remove_dir_all(&dir).unwrap();
}

/// 审计第 10 条：两个会话台各自读了记录、先后开标签页，后存的不能把先存的那条整份盖掉；
/// 写用独占的临时文件，不留下 `tabs.json.tmp*`。
#[test]
fn two_boards_saving_tabs_keep_each_others_records() {
    let dir = temp_dir("two-boards");
    let path = dir.join("tabs.json");
    let term = FakeTerminal::default();
    let mut a = Tabs::load(&path).unwrap();
    let mut b = Tabs::load(&path).unwrap();
    let (s1, s2) = (SessionId::from("s1"), SessionId::from("s2"));
    a.open_or_focus(&term, &s1, &cmd("s1"), T).unwrap();
    b.open_or_focus(&term, &s2, &cmd("s2"), T).unwrap();
    let saved = Tabs::load(&path).unwrap();
    assert!(saved.records().contains_key(&s1) && saved.records().contains_key(&s2));
    // 一边忘掉自己那条，也不动另一边的。
    a.forget(&s1).unwrap();
    let saved = Tabs::load(&path).unwrap();
    assert_eq!(saved.records().keys().collect::<Vec<_>>(), vec![&s2]);
    let leftovers: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    fs::remove_dir_all(&dir).unwrap();
}

/// 几个写入方同时存（模拟两个会话台同时开标签页）：每次都存得下，文件始终读得懂。
#[test]
fn concurrent_saves_never_leave_a_broken_file() {
    let dir = temp_dir("concurrent");
    let path = dir.join("tabs.json");
    let threads: Vec<_> = (0..8)
        .map(|n| {
            let path = path.clone();
            std::thread::spawn(move || {
                let term = FakeTerminal::default();
                for i in 0..20 {
                    let id = format!("t{n}-{i}");
                    let mut tabs = Tabs::load(&path).unwrap();
                    tabs.open_or_focus(&term, &SessionId::from(id.as_str()), &cmd(&id), T)
                        .unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert!(!Tabs::load(&path).unwrap().records().is_empty());
    fs::remove_dir_all(&dir).unwrap();
}
