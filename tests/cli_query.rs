//! `ai list`、`ai search` 端到端：假家目录里手写两条 Claude 对话、复制 pi 的 fixtures，用库建好索引，
//! 再跑真二进制。样例都是手写的。

mod host_support;
mod sources_common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use host_support::{Home, WAIT, contains, fake_env};
use huitai::core::{SessionId, SessionState, Vendor};
use huitai::host::{self, AttachMsg, Attachment, HostedInfo, Request, Response, SpawnSpec};
use huitai::index::{Edit, Index};
use huitai::sources::{self, Trash};
use serde_json::Value;
use sources_common::{copy_tree, fixture, temp_dir};

const LOGIN: &str = "11111111-aaaa-4aaa-8aaa-111111111111";
const DEPLOY: &str = "22222222-bbbb-4bbb-8bbb-222222222222";

fn claude(at: &str, user: &str, reply: &str) -> String {
    [
        format!(
            r#"{{"type":"user","cwd":"/Users/demo/projects/shop","timestamp":"{at}","message":{{"role":"user","content":"{user}"}}}}"#
        ),
        format!(
            r#"{{"type":"assistant","cwd":"/Users/demo/projects/shop","timestamp":"{at}","message":{{"role":"assistant","content":[{{"type":"text","text":"{reply}"}}]}}}}"#
        ),
    ]
    .join("\n")
        + "\n"
}

/// 假家目录：Claude 两条（LOGIN 较新），pi 的 fixtures。还没建索引。
fn home(name: &str) -> PathBuf {
    let home = temp_dir(name);
    fill(&home);
    home
}

fn fill(home: &Path) {
    let dir = home.join(".claude/projects/-Users-demo-projects-shop");
    fs::create_dir_all(&dir).unwrap();
    let sessions = [
        (
            DEPLOY,
            "2026-11-20T02:00:00Z",
            "写个部署脚本",
            "脚本放在 scripts/deploy.sh，跑之前先备份。",
        ),
        (
            LOGIN,
            "2026-11-21T02:00:00Z",
            "修一下登录页的样式",
            "好的，按钮改成蓝色了，输入框加了圆角。",
        ),
    ];
    for (id, at, user, reply) in sessions {
        fs::write(dir.join(format!("{id}.jsonl")), claude(at, user, reply)).unwrap();
    }
    copy_tree(&fixture("pi/sessions"), &home.join(".pi/agent/sessions"));
}

fn data_dir(home: &Path) -> PathBuf {
    home.join("Library/Application Support/huitai")
}

/// 像会话台那样扫一遍、建好全文索引，再把 DEPLOY 改名成只在标题里有的词。
fn build_index(home: &Path) {
    let accounts = sources::accounts(home, home, |_| None, &[]);
    let srcs = sources::sources_for(&accounts, Trash::new(home.join(".Trash")));
    let index = Index::open(&data_dir(home), srcs).unwrap();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    index
        .annotate(
            Vendor::Claude,
            &SessionId::from(DEPLOY),
            Edit::Rename(Some("上线清单".into())),
        )
        .unwrap();
}

fn ai(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ai"))
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .current_dir(home)
        .args(args)
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn json(out: &Output) -> Vec<Value> {
    assert!(out.status.success(), "{}", text(&out.stderr));
    match serde_json::from_slice(&out.stdout).unwrap() {
        Value::Array(a) => a,
        other => panic!("不是数组：{other}"),
    }
}

/// 数据目录里每个文件的内容。SQLite 的 -shm 不算、空的 -wal 不算（只读连接也可能建它们）；
/// 写进库的东西先落在 -wal 里，它有内容就算变了。
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.ends_with("-shm"))
        .map(|n| {
            let bytes = fs::read(dir.join(&n)).unwrap_or_default();
            (n, bytes)
        })
        .filter(|(n, b)| !(n.ends_with("-wal") && b.is_empty()))
        .collect();
    out.sort();
    out
}

#[test]
fn without_an_index_both_commands_say_to_open_ai_first() {
    let home = home("cli-query-no-index");
    for args in [
        &["list"][..],
        &["list", "--json"],
        &["search", "登录"],
        &["search", "登录", "--json"],
    ] {
        let out = ai(&home, args);
        let err = text(&out.stderr);
        assert!(!out.status.success(), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}: {}", text(&out.stdout));
        assert!(
            err.contains("还没有索引") && err.contains("先运行一次 ai"),
            "{args:?}: {err}"
        );
    }
    // 只读：连数据目录都不建。
    assert!(!data_dir(&home).exists());
}

#[test]
fn list_json_has_every_field_newest_first_and_filters_by_vendor() {
    let home = home("cli-query-list-json");
    build_index(&home);
    let all = json(&ai(&home, &["list", "--json"]));
    let claude: Vec<&Value> = all.iter().filter(|r| r["vendor"] == "claude").collect();
    let pi = all.iter().filter(|r| r["vendor"] == "pi").count();
    assert_eq!(claude.len(), 2, "{all:?}");
    assert!(pi > 0, "{all:?}");
    assert_eq!(all.len(), 2 + pi, "只有这两家：{all:?}");

    let login = claude.iter().find(|r| r["id"] == LOGIN).unwrap();
    let keys: Vec<&str> = login
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut want = [
        "id",
        "vendor",
        "account",
        "project",
        "title",
        "state",
        "activity",
        "last_active",
        "archived",
        "automatic",
    ];
    want.sort_unstable();
    let mut got = keys.clone();
    got.sort_unstable();
    assert_eq!(got, want, "list 不带 snippet");
    assert_eq!(login["account"], "默认");
    assert_eq!(login["project"], "/Users/demo/projects/shop");
    assert_eq!(login["title"], "修一下登录页的样式");
    assert_eq!(login["state"], "stopped", "守护进程没在跑");
    assert!(login["activity"].is_null(), "没托管就没有");
    assert_eq!(login["archived"], false);
    assert_eq!(login["automatic"], false);
    let deploy = claude.iter().find(|r| r["id"] == DEPLOY).unwrap();
    assert_eq!(deploy["title"], "上线清单", "会话台里改的名字优先");

    // 最近的在前；时间是带时区的 RFC 3339。
    let times: Vec<&str> = all
        .iter()
        .map(|r| r["last_active"].as_str().unwrap())
        .collect();
    for t in &times {
        assert!(t.len() == 25 && &t[10..11] == "T", "{t}");
    }
    let li = all.iter().position(|r| r["id"] == LOGIN).unwrap();
    let di = all.iter().position(|r| r["id"] == DEPLOY).unwrap();
    assert!(li < di, "LOGIN 比 DEPLOY 新：{times:?}");

    let only = json(&ai(&home, &["list", "--vendor", "claude", "--json"]));
    assert_eq!(only.len(), 2);
    assert!(only.iter().all(|r| r["vendor"] == "claude"));
    let bad = ai(&home, &["list", "--vendor", "nope"]);
    assert!(!bad.status.success());
    assert!(
        text(&bad.stderr).contains("claude"),
        "{}",
        text(&bad.stderr)
    );
}

#[test]
fn list_text_is_one_aligned_line_per_conversation_with_id_last() {
    let home = home("cli-query-list-text");
    build_index(&home);
    let out = ai(&home, &["list", "--vendor", "claude"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let s = text(&out.stdout);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 2, "{s}");
    assert!(
        lines[0].ends_with(LOGIN) && lines[1].ends_with(DEPLOY),
        "{s}"
    );
    for l in &lines {
        assert!(
            l.contains("Claude") && l.contains("默认") && l.contains("/Users/demo/projects/shop"),
            "{l}"
        );
        assert!(!l.contains('\x1b'), "管道里不带颜色：{l:?}");
    }
    // 对齐：id 在每行同一个显示列开始。
    let col = |l: &str, id: &str| unicode_width::UnicodeWidthStr::width(&l[..l.len() - id.len()]);
    assert_eq!(col(lines[0], LOGIN), col(lines[1], DEPLOY), "{s}");
    assert!(lines[0].contains("修一下登录页的样式") && lines[1].contains("上线清单"));
}

#[test]
fn search_returns_title_hits_then_body_hits_with_snippets() {
    let home = home("cli-query-search");
    build_index(&home);
    // 只在正文里的词：带命中片段。
    let hits = json(&ai(&home, &["search", "蓝色", "--json"]));
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["id"], LOGIN);
    let snip = hits[0]["snippet"].as_str().unwrap();
    assert!(snip.contains("蓝色"), "{snip}");
    // 只在改过的名字里：有 snippet 字段，是 null。
    let hits = json(&ai(&home, &["search", "上线清单", "--json"]));
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["id"], DEPLOY);
    assert!(
        hits[0].get("snippet").is_some_and(Value::is_null),
        "{hits:?}"
    );

    // 纯文本：结果一行，片段在下一行缩进四格。
    let out = ai(&home, &["search", "蓝色"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let s = text(&out.stdout);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 2, "{s}");
    assert!(lines[0].ends_with(LOGIN), "{s}");
    assert!(
        lines[1].starts_with("    ") && lines[1].contains("蓝色"),
        "{s}"
    );
    let out = ai(&home, &["search", "上线清单"]);
    assert_eq!(text(&out.stdout).lines().count(), 1, "只命中标题没有片段行");
}

#[test]
fn empty_results_are_empty_array_or_empty_stdout() {
    let home = home("cli-query-empty");
    build_index(&home);
    let out = ai(&home, &["search", "没有这个词zzz", "--json"]);
    assert!(json(&out).is_empty());
    let out = ai(&home, &["search", "没有这个词zzz"]);
    assert!(out.status.success());
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
    assert!(
        text(&out.stderr).contains("没搜到"),
        "{}",
        text(&out.stderr)
    );

    // 建过索引但一条对话都没有。
    let bare = temp_dir("cli-query-empty-bare");
    build_index_without_sessions(&bare);
    assert!(json(&ai(&bare, &["list", "--json"])).is_empty());
    let out = ai(&bare, &["list"]);
    assert!(out.status.success() && out.stdout.is_empty());
    assert!(
        text(&out.stderr).contains("没有对话"),
        "{}",
        text(&out.stderr)
    );
}

fn build_index_without_sessions(home: &Path) {
    let accounts = sources::accounts(home, home, |_| None, &[]);
    let srcs = sources::sources_for(&accounts, Trash::new(home.join(".Trash")));
    Index::open(&data_dir(home), srcs).unwrap().refresh();
}

#[test]
fn queries_change_nothing_on_disk() {
    let home = home("cli-query-readonly");
    build_index(&home);
    let dir = data_dir(&home);
    let before = snapshot(&dir);
    assert!(before.iter().any(|(n, _)| n == "index.sqlite"));
    for args in [
        &["list"][..],
        &["list", "--json"],
        &["search", "蓝色"],
        &["search", "上线清单", "--json"],
    ] {
        assert!(ai(&home, args).status.success(), "{args:?}");
    }
    assert_eq!(snapshot(&dir), before, "数据目录里的文件一个字节都没变");
    // 新加的会话文件不会被这两条命令扫进去（不扫描）。
    let dir = home.join(".claude/projects/-Users-demo-projects-shop");
    fs::write(
        dir.join("33333333-cccc-4ccc-8ccc-333333333333.jsonl"),
        claude("2026-11-22T02:00:00Z", "新的一条", "嗯"),
    )
    .unwrap();
    let all = json(&ai(&home, &["list", "--vendor", "claude", "--json"]));
    assert_eq!(all.len(), 2);
}

/// 旧版 `ai` 建的库（缺后加的列）照样读、不补列；标注文件坏了照样列、不改名留底。
#[test]
fn old_index_and_broken_notes_are_read_not_repaired() {
    let home = home("cli-query-old");
    build_index(&home);
    let dir = data_dir(&home);
    let db = rusqlite::Connection::open(dir.join("index.sqlite")).unwrap();
    db.execute_batch("alter table files drop column session")
        .unwrap();
    drop(db);
    fs::write(dir.join("annotations.json"), "{坏了").unwrap();
    let before = snapshot(&dir);

    let out = ai(&home, &["list", "--vendor", "claude", "--json"]);
    assert_eq!(json(&out).len(), 2);
    assert!(text(&out.stderr).contains("注意"), "{}", text(&out.stderr));
    let hits = json(&ai(&home, &["search", "蓝色", "--json"]));
    assert_eq!(hits.len(), 1);
    assert_eq!(snapshot(&dir), before, "没补列、没把标注文件改名");
}

/// 审计 v0.1.4 复核第 9 条（标注）：0.1.3 之后加的厂商（Antigravity）的改名、归档存在 `annotations-more.json`，
/// `ai list`、`ai search` 也读它。以前只读 `annotations.json`，列出来还是原名、没归档，按新名字搜不到。
#[test]
fn queries_see_notes_of_vendors_added_after_v013() {
    const AGY: &str = "a9e10000-1111-4aaa-8bbb-000000000001";
    let home = home("cli-query-agy-notes");
    let root = home.join(".gemini/antigravity-cli");
    copy_tree(
        &fixture("antigravity/annotations"),
        &root.join("annotations"),
    );
    copy_tree(&fixture("antigravity/brain"), &root.join("brain"));
    let sql = fs::read_to_string(fixture("antigravity/conversation_summaries.sql")).unwrap();
    sources_common::build_db(&root.join("conversation_summaries.db"), &sql);
    build_index(&home);
    let accounts = sources::accounts(&home, &home, |_| None, &[]);
    let srcs = sources::sources_for(&accounts, Trash::new(home.join(".Trash")));
    let index = Index::open(&data_dir(&home), srcs).unwrap();
    let id = SessionId::from(AGY);
    index
        .annotate(
            Vendor::Antigravity,
            &id,
            Edit::Rename(Some("收银台改版".into())),
        )
        .unwrap();
    index
        .annotate(Vendor::Antigravity, &id, Edit::Archive(true))
        .unwrap();
    drop(index);
    assert!(data_dir(&home).join("annotations-more.json").is_file());

    let rows = json(&ai(&home, &["list", "--vendor", "antigravity", "--json"]));
    let row = rows
        .iter()
        .find(|r| r["id"] == AGY)
        .unwrap_or_else(|| panic!("没列出 {AGY}：{rows:?}"));
    assert_eq!(
        (&row["title"], &row["archived"]),
        (&Value::from("收银台改版"), &Value::from(true)),
        "{row}"
    );
    let hits = json(&ai(&home, &["search", "收银台改版", "--json"]));
    assert!(hits.iter().any(|h| h["id"] == AGY), "{hits:?}");
}

/// 假 CLI：读到什么发什么（响铃、普通输出由测试决定）。
const ECHO: &str = "stty raw -echo; printf ready; exec cat";

/// 登记并起一条假 CLI，等它起来。标签页不报焦点（不在前台）。
fn start(home: &Home, vendor: Vendor, id: &str) -> Attachment {
    let id = SessionId::from(id);
    let spec = SpawnSpec {
        id: id.clone(),
        vendor,
        project: "shop".to_owned(),
        cwd: home.home.clone(),
        command: ECHO.to_owned(),
    };
    assert_eq!(
        host::request(&home.data, &Request::Spawn(spec)).unwrap(),
        Response::Ok
    );
    let mut att = host::attach(&home.data, &id, fake_env(&home.home), 60, 12).expect("attach 失败");
    let mut seen = Vec::new();
    while !contains(&seen, b"ready") {
        match att.recv(Some(WAIT)).expect("等不到假 CLI 起来") {
            AttachMsg::Output(bytes) => seen.extend_from_slice(&bytes),
            other => panic!("假 CLI 没起来：{other:?}"),
        }
    }
    att
}

/// 守护进程托管着的那一条。
fn only_hosted(home: &Home) -> HostedInfo {
    match host::request(&home.data, &Request::List).unwrap() {
        Response::List(mut l) if l.len() == 1 => l.remove(0),
        other => panic!("应当只托管一条：{other:?}"),
    }
}

/// `ai list --json` 里这条对话的（state, activity）。
fn lamp(home: &Home, id: &str) -> (Value, Value) {
    let rows = json(&ai(&home.home, &["list", "--json"]));
    let row = rows.iter().find(|r| r["id"] == id).unwrap();
    (row["state"].clone(), row["activity"].clone())
}

/// 等到它是托管中、`activity` 是 `want`。
fn wait_lamp(home: &Home, id: &str, want: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        let got = lamp(home, id);
        if got == (Value::from("hosted"), Value::from(want)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{id} 等不到 {want}，现在是 {got:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// 真起 `ai daemon`：在跑、响铃后等你（没看过、看过都是）、又有输出回到在跑、结束后没有；认领到临时 id 下的记在
/// 真实 id 上；纯文本跟着写。
#[test]
fn activity_follows_the_hosted_conversation() {
    let mut home = Home::new("lamp");
    fill(&home.home);
    build_index(&home.home);
    home.ensure_daemon();

    let login = start(&home, Vendor::Claude, LOGIN);
    wait_lamp(&home, LOGIN, "running");
    assert_eq!(
        lamp(&home, DEPLOY),
        (Value::from("stopped"), Value::Null),
        "没托管的不受影响"
    );
    let line = |home: &Home| {
        let out = ai(&home.home, &["list", "--vendor", "claude"]);
        let s = text(&out.stdout);
        s.lines().find(|l| l.ends_with(LOGIN)).unwrap().to_owned()
    };
    assert!(line(&home).contains("托管中·在跑"), "{}", line(&home));

    // 没看过的（刚做完）也是等你。
    login.writer().input(b"\x07").unwrap();
    wait_lamp(&home, LOGIN, "waiting");
    assert!(line(&home).contains("托管中·等你"), "{}", line(&home));
    let hits = json(&ai(&home.home, &["search", "蓝色", "--json"]));
    assert_eq!(hits[0]["activity"], "waiting", "搜索结果也带");

    // 看过了还在等你。
    let id = SessionId::from(LOGIN);
    let seen = Request::Seen { id: id.clone() };
    assert_eq!(host::request(&home.data, &seen).unwrap(), Response::Ok);
    let deadline = Instant::now() + WAIT;
    while only_hosted(&home).status != Some(SessionState::Waiting) {
        assert!(Instant::now() < deadline, "等不到看过");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        lamp(&home, LOGIN),
        (Value::from("hosted"), Value::from("waiting"))
    );

    // 收尾重画的 2 秒过了再有输出：新一轮。
    thread::sleep(Duration::from_millis(2200));
    login.writer().input(b"working").unwrap();
    wait_lamp(&home, LOGIN, "running");

    assert_eq!(
        host::request(&home.data, &Request::Kill { id }).unwrap(),
        Response::Ok
    );
    let deadline = Instant::now() + WAIT;
    while lamp(&home, LOGIN) != (Value::from("stopped"), Value::Null) {
        assert!(
            Instant::now() < deadline,
            "结束后还有：{:?}",
            lamp(&home, LOGIN)
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// 会话台新建、托管在临时 id 下的：认领了才知道是哪条，状态记在真实 id 上。
#[test]
fn activity_of_a_claimed_temporary_id_goes_to_the_real_conversation() {
    let mut home = Home::new("lampt");
    fill(&home.home);
    build_index(&home.home);
    home.ensure_daemon();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let temp = format!("new-codex-{}", now.as_millis());
    let att = start(&home, Vendor::Codex, &temp);
    let pid = only_hosted(&home).pid.unwrap();
    assert_eq!(lamp(&home, DEPLOY).0, "stopped", "还没认领");
    let claims = serde_json::json!({ &temp: { "real": DEPLOY, "pid": pid } });
    fs::write(home.data.join("claims.json"), claims.to_string()).unwrap();
    wait_lamp(&home, DEPLOY, "running");
    att.writer().input(b"\x07").unwrap();
    wait_lamp(&home, DEPLOY, "waiting");
}

#[test]
fn help_lists_both_commands() {
    let out = ai(Path::new("/"), &["--help"]);
    let s = text(&out.stdout);
    assert!(s.contains("list") && s.contains("search"), "{s}");
    let out = ai(Path::new("/"), &["list", "--help"]);
    let s = text(&out.stdout);
    assert!(s.contains("--vendor") && s.contains("--json"), "{s}");
}
