//! 升级过渡期：新旧两版同时开着时，旧版（v0.1.2）会话台读得懂新版给它的东西。
//! - 连新版守护进程：托管列表和「被中断」里只有它认识的厂商，用 v0.1.2 的类型解码不失败；
//!   当前版本的客户端照样拿到全部。
//! - 新版在 cmux 里开过标签页之后，v0.1.2 照样读得了 `tabs.json`。

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use huitai::core::{SessionId, Vendor};
use huitai::host::protocol::{self, KIND_REQUEST, KIND_RESPONSE};
use huitai::host::{self, Build, Daemon, Request, Response, RunRecord, SpawnSpec, interrupt};
use huitai::opener::{Opened, Opener, Outcome, TabHandle, Tabs};
use huitai::sources::Command;

/// v0.1.2 原样的类型（`git show 42a4c75:src/core/vendor.rs`、`src/host/protocol.rs`），
/// 只留这里用到的应答；解码规则和旧版一致：枚举遇到不认识的名字整份失败。
#[allow(dead_code)]
mod v012 {
    use std::path::PathBuf;

    use serde::Deserialize;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum Vendor {
        Claude,
        Codex,
        Pi,
    }

    #[derive(Debug, Deserialize)]
    pub struct HostedInfo {
        pub id: String,
        pub vendor: Vendor,
        pub project: String,
        pub cwd: PathBuf,
        pub pid: Option<u32>,
        pub attached: bool,
        pub title: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub struct RunRecord {
        pub id: String,
        pub vendor: Vendor,
        pub project: String,
        pub cwd: PathBuf,
        pub started_at: u64,
    }

    #[derive(Debug, Deserialize)]
    pub struct Build {
        pub protocol: u32,
        pub version: String,
    }

    #[derive(Debug, Deserialize)]
    pub struct HelloReply {
        pub build: Build,
        pub pid: u32,
        pub hosted: usize,
        #[serde(default)]
        pub refused: Option<String>,
    }

    /// `src/opener/tabs.rs` 的记录：会话台把整个 `tabs.json` 一次解码。
    #[derive(Debug, Deserialize)]
    pub struct TabRecord {
        pub program: String,
        pub args: Vec<String>,
        pub cwd: PathBuf,
        #[serde(default)]
        pub title: String,
        pub handle: TabHandle,
    }

    /// `src/opener/mod.rs`：只有 Ghostty、Kaku。
    #[derive(Debug, Deserialize)]
    #[serde(tag = "terminal", rename_all = "lowercase")]
    pub enum TabHandle {
        Ghostty {
            terminal_id: String,
        },
        Kaku {
            pane_id: u64,
            tty: String,
            command: String,
        },
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Response {
        Hello(HelloReply),
        List(Vec<HostedInfo>),
        Interrupted(Vec<RunRecord>),
        Error(String),
    }
}

/// 以 v0.1.2 的身份握手后发一个请求，用 v0.1.2 的类型解码应答。
fn ask_as_v012(dir: &std::path::Path, req: &Request) -> v012::Response {
    let mut stream = UnixStream::connect(host::socket_path(dir)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let hello = Build {
        protocol: 1,
        version: "0.1.2".to_owned(),
        exe: None,
        built: 0,
    };
    protocol::write_json(&mut stream, KIND_REQUEST, &Request::Hello(hello)).unwrap();
    match protocol::read_json::<v012::Response>(&mut stream, KIND_RESPONSE).unwrap() {
        v012::Response::Hello(h) => assert_eq!(h.refused, None),
        other => panic!("握手应答不对：{other:?}"),
    }
    protocol::write_json(&mut stream, KIND_REQUEST, req).unwrap();
    protocol::read_json(&mut stream, KIND_RESPONSE).expect("v0.1.2 解码失败")
}

fn spec(dir: &std::path::Path, id: &str, vendor: Vendor) -> SpawnSpec {
    SpawnSpec {
        id: SessionId::from(id),
        vendor,
        project: "proj".to_owned(),
        cwd: dir.to_path_buf(),
        command: "sleep 30".to_owned(),
    }
}

fn record(id: &str, vendor: Vendor) -> RunRecord {
    RunRecord {
        id: SessionId::from(id),
        vendor,
        project: "proj".to_owned(),
        cwd: PathBuf::from("/tmp"),
        started_at: 1,
    }
}

#[test]
fn old_client_gets_only_vendors_it_knows() {
    let dir = PathBuf::from(format!("/tmp/hta-old-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    // 上一个守护进程没收尾就没了，留下两家在跑的对话：起来后它们进「被中断」。
    let running: BTreeMap<SessionId, RunRecord> = [
        record("cut-claude", Vendor::Claude),
        record("cut-kimi", Vendor::Kimi),
    ]
    .into_iter()
    .map(|r| (r.id.clone(), r))
    .collect();
    interrupt::save(&interrupt::running_path(&dir), &running).unwrap();
    let daemon = Daemon::bind(&dir).expect("守护进程监听失败");
    thread::spawn(move || daemon.run());

    // 只登记不 attach：进程不启动，列表里照样有。
    for (id, vendor) in [
        ("c1", Vendor::Claude),
        ("k1", Vendor::Kimi),
        ("o1", Vendor::OpenCode),
    ] {
        let r = host::request(&dir, &Request::Spawn(spec(&dir, id, vendor))).unwrap();
        assert_eq!(r, Response::Ok);
    }

    match ask_as_v012(&dir, &Request::List) {
        v012::Response::List(list) => {
            let got: Vec<_> = list.iter().map(|h| (h.id.as_str(), h.vendor)).collect();
            assert_eq!(got, [("c1", v012::Vendor::Claude)]);
        }
        other => panic!("List 应答不对：{other:?}"),
    }
    match ask_as_v012(&dir, &Request::Interrupted) {
        v012::Response::Interrupted(list) => {
            let got: Vec<_> = list.iter().map(|r| r.id.as_str()).collect();
            assert_eq!(got, ["cut-claude"]);
        }
        other => panic!("Interrupted 应答不对：{other:?}"),
    }

    // 当前版本的客户端照样拿到全部。
    match host::request(&dir, &Request::List).unwrap() {
        Response::List(list) => assert_eq!(list.len(), 3, "{list:?}"),
        other => panic!("List 应答不对：{other:?}"),
    }
    match host::request(&dir, &Request::Interrupted).unwrap() {
        Response::Interrupted(list) => assert_eq!(list.len(), 2, "{list:?}"),
        other => panic!("Interrupted 应答不对：{other:?}"),
    }
    let _ = fs::remove_dir_all(&dir);
}

/// 依次交出这些标签页的假终端。
struct Handing(RefCell<Vec<TabHandle>>);

impl Opener for Handing {
    fn open(&self, _: &Command, _: &str) -> io::Result<Opened> {
        Ok(Opened::Tab(self.0.borrow_mut().remove(0)))
    }
    fn focus(&self, _: &TabHandle) -> io::Result<bool> {
        Ok(false)
    }
}

/// 审计 v0.1.3 第 06 条：v0.1.3 把 cmux 记录写进 `tabs.json`，v0.1.2 整份读不了。
#[test]
fn old_client_reads_tabs_after_cmux_tab_opened() {
    let dir = std::env::temp_dir().join(format!("huitai-old-tabs-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let path = dir.join("tabs.json");
    let term = Handing(RefCell::new(vec![
        TabHandle::Cmux {
            surface_id: "s-1".to_owned(),
            workspace_id: "w-1".to_owned(),
        },
        TabHandle::Ghostty {
            terminal_id: "g-1".to_owned(),
        },
    ]));
    let cmd = Command {
        program: "ai".to_owned(),
        args: vec!["attach".to_owned()],
        cwd: dir.clone(),
    };
    let mut tabs = Tabs::load(&path).unwrap();
    for id in ["c", "g"] {
        let out = tabs.open_or_focus(&term, &SessionId::from(id), &cmd, "");
        assert_eq!(out.unwrap(), Outcome::Opened);
    }

    let text = fs::read_to_string(&path).unwrap();
    let old: BTreeMap<String, v012::TabRecord> =
        serde_json::from_str(&text).expect("v0.1.2 读不了 tabs.json");
    assert_eq!(old.keys().collect::<Vec<_>>(), ["g"]);

    // 新版重读两条都在；cmux 那条忘掉后，旁边那份删掉。
    let ids = |t: &Tabs| -> Vec<String> { t.records().keys().map(|k| k.to_string()).collect() };
    assert_eq!(ids(&Tabs::load(&path).unwrap()), ["c", "g"]);
    tabs.forget(&SessionId::from("c")).unwrap();
    assert!(!dir.join("tabs-more.json").exists());
    assert_eq!(ids(&Tabs::load(&path).unwrap()), ["g"]);
    fs::remove_dir_all(&dir).unwrap();
}
