//! `ai list`、`ai search` 端到端：假家目录里手写两条 Claude 对话、复制 pi 的 fixtures，用库建好索引，
//! 再跑真二进制。样例都是手写的。

mod sources_common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use huitai::core::{SessionId, Vendor};
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
    home
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

#[test]
fn help_lists_both_commands() {
    let out = ai(Path::new("/"), &["--help"]);
    let s = text(&out.stdout);
    assert!(s.contains("list") && s.contains("search"), "{s}");
    let out = ai(Path::new("/"), &["list", "--help"]);
    let s = text(&out.stdout);
    assert!(s.contains("--vendor") && s.contains("--json"), "{s}");
}
