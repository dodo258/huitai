//! 有对话在跑时持有防休眠断言，全部停下就释放。单独一个测试进程，按本进程 pid 查 `pmset`。

mod host_support;

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use host_support::*;
use huitai::host::power::ASSERTION_NAME;
use huitai::host::{HostEvent, Request, Response};

fn held_by_me() -> bool {
    let out = Command::new("pmset")
        .args(["-g", "assertions"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let me = format!("pid {}(", std::process::id());
    text.lines()
        .any(|l| l.contains(&me) && l.contains(ASSERTION_NAME))
}

fn wait_for(want: bool) {
    let deadline = Instant::now() + WAIT;
    while held_by_me() != want {
        assert!(Instant::now() < deadline, "防休眠断言应为 {want}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn sleep_assertion_follows_running_sessions() {
    let host = TestHost::start();
    assert!(!held_by_me());
    let id = host.spawn("p", "sleep 60");
    // 只登记、没启动时不持有。
    assert!(!held_by_me());
    let mut events = host.subscribe();
    let mut att = host.attach(&id, COLS, ROWS);
    let mut seen = Seen::new(COLS, ROWS);
    read_snapshot(&mut att, &mut seen);
    wait_for(true);
    drop(att);
    // 关掉标签页对话照跑，断言还在。
    host.wait_info(&id, "断开", |i| i.is_some_and(|i| !i.attached));
    assert!(held_by_me());
    assert_eq!(
        host.request(&Request::Kill { id: id.clone() }),
        Response::Ok
    );
    wait_event(&mut events, "退出", |e| {
        matches!(e, HostEvent::Exited { .. })
    });
    wait_for(false);
}
