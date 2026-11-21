//! 会话台遇到不认订阅的旧版守护进程：订阅失败后降级成用 `List` 轮询托管对话。
//!
//! 旧版守护进程是测试里起的假守护进程，说真的旧协议：只认 `Ping`、`List`，别的请求（`Hello`、`Board`、
//! `Events`…）读不懂就直接关连接，和没有握手的旧版 `ai daemon` 一样。另一种假守护进程会握手、报更老的版本，
//! 但同样不认 `Board`，代表以后订阅格式改了的旧版。

mod host_support;

use std::fs;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use host_support::{COLS, ROWS, TestHost, fake_env};
use huitai::core::{SessionId, Vendor};
use huitai::host::client::{BoardItem, board_feed};
use huitai::host::protocol::{self, KIND_REQUEST, KIND_RESPONSE};
use huitai::host::{self, BoardInfo, Build, HelloReply, HostedInfo, Request, Response};

const POLL: Duration = Duration::from_millis(50);

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// 假守护进程。`hello` 为 `None` 时是没有握手的旧版；有值时握手报这个版本。drop 时删 socket、停下。
struct FakeDaemon {
    dir: PathBuf,
    hosted: Arc<Mutex<Vec<HostedInfo>>>,
    stop: Arc<AtomicBool>,
}

impl FakeDaemon {
    fn start(hello: Option<Build>, hosted: Vec<HostedInfo>) -> Self {
        let dir = PathBuf::from(format!(
            "/tmp/htl-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let listener = UnixListener::bind(host::socket_path(&dir)).unwrap();
        let hosted = Arc::new(Mutex::new(hosted));
        let stop = Arc::new(AtomicBool::new(false));
        let (list, quit) = (Arc::clone(&hosted), Arc::clone(&stop));
        thread::spawn(move || {
            for conn in listener.incoming() {
                if quit.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(conn) = conn else { continue };
                let (list, hello) = (Arc::clone(&list), hello.clone());
                thread::spawn(move || serve(conn, hello, &list));
            }
        });
        Self { dir, hosted, stop }
    }

    fn set_hosted(&self, list: Vec<HostedInfo>) {
        *self.hosted.lock().unwrap() = list;
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // 叫醒 accept，让监听线程看到 stop。
        let _ = UnixStream::connect(host::socket_path(&self.dir));
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// 一条连接：只认 `Ping`、`List`（握手版还认 `Hello`），别的就关连接。
fn serve(mut conn: UnixStream, hello: Option<Build>, list: &Mutex<Vec<HostedInfo>>) {
    while let Ok(req) = protocol::read_json::<Request>(&mut conn, KIND_REQUEST) {
        let reply = match req {
            Request::Hello(_) => match &hello {
                Some(build) => Response::Hello(HelloReply {
                    build: build.clone(),
                    pid: std::process::id(),
                    hosted: list.lock().unwrap().len(),
                    refused: None,
                }),
                None => return,
            },
            Request::Ping => Response::Pong {
                pid: std::process::id(),
            },
            Request::List => Response::List(list.lock().unwrap().clone()),
            _ => return,
        };
        if protocol::write_json(&mut conn, KIND_RESPONSE, &reply).is_err() {
            return;
        }
    }
}

fn running(id: &str) -> HostedInfo {
    HostedInfo {
        id: SessionId::from(id),
        vendor: Vendor::Claude,
        project: "demo".to_owned(),
        cwd: PathBuf::from("/tmp"),
        pid: Some(4242),
        attached: false,
        title: None,
    }
}

fn hosted_ids(item: Option<BoardItem>) -> Vec<String> {
    match item {
        Some(BoardItem::Hosted(list)) => list.into_iter().map(|i| i.id.to_string()).collect(),
        other => panic!("应该是托管对话列表：{other:?}"),
    }
}

/// 比当前程序老的已握手版本。
fn older_build() -> Build {
    Build {
        protocol: host::version::current().protocol,
        version: "0.0.1".to_owned(),
        exe: None,
        built: 0,
    }
}

#[test]
fn legacy_daemon_without_subscription_falls_back_to_polling_list() {
    let fake = FakeDaemon::start(None, vec![running("a")]);

    // 旧版不认会话台的订阅。
    assert!(host::board(&fake.dir, BoardInfo::default()).is_err());
    let info = host::probe(&fake.dir).unwrap();
    assert!(info.build.is_none() && info.outdated());
    assert_eq!(info.hosted, 1);

    let mut feed = board_feed(&fake.dir, BoardInfo::default(), POLL).expect("应该降级成轮询");
    assert!(feed.polling());
    assert_eq!(hosted_ids(feed.recv().unwrap()), ["a"]);

    // 旧版上又开了一条：下一轮轮询就看到两条。
    fake.set_hosted(vec![running("a"), running("b")]);
    assert_eq!(hosted_ids(feed.recv().unwrap()), ["a", "b"]);

    // 对话都结束了：轮询停下，调用方去换新版。
    fake.set_hosted(Vec::new());
    assert_eq!(feed.recv().unwrap(), None);
}

#[test]
fn polling_stops_when_legacy_daemon_goes_away() {
    let fake = FakeDaemon::start(None, vec![running("a")]);
    let mut feed = board_feed(&fake.dir, BoardInfo::default(), POLL).unwrap();
    assert!(feed.polling());
    drop(fake);
    assert_eq!(feed.recv().unwrap(), None);
}

#[test]
fn older_handshaking_daemon_that_rejects_board_also_falls_back() {
    let fake = FakeDaemon::start(Some(older_build()), vec![running("a")]);
    let info = host::probe(&fake.dir).unwrap();
    assert!(info.build.is_some() && info.outdated());

    let mut feed = board_feed(&fake.dir, BoardInfo::default(), POLL).expect("应该降级成轮询");
    assert!(feed.polling());
    assert_eq!(hosted_ids(feed.recv().unwrap()), ["a"]);
}

#[test]
fn current_daemon_that_rejects_board_is_an_error_not_polling() {
    // 版本和当前程序一样却不认订阅：不是新旧之分，照常报错（会话台过一会儿再试）。
    let fake = FakeDaemon::start(Some(host::version::current().clone()), vec![running("a")]);
    assert!(board_feed(&fake.dir, BoardInfo::default(), POLL).is_err());
}

#[test]
fn current_daemon_subscribes_to_events() {
    let host = TestHost::start();
    let mut feed = board_feed(&host.dir, BoardInfo::default(), POLL).unwrap();
    assert!(!feed.polling());
    let id = host.spawn("e", "echo hi; sleep 5");
    // 事件流照常：接上就启动，收到的是事件而不是轮询列表。
    let _att = host::attach(&host.dir, &id, fake_env(&host.dir), COLS, ROWS).unwrap();
    assert!(matches!(feed.recv().unwrap(), Some(BoardItem::Event(_))));
}
