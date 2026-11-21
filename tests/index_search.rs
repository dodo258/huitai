//! 全文搜索：标题（含会话台里改的名字）和正文都能搜到；中文按子串、英文按词前缀；
//! 内容变了重建、对话没了不再命中；只在当前那一家里搜。

mod index_common;

use std::fs;

use huitai::core::{SessionId, Vendor};
use huitai::index::{Edit, Hit, Index};
use huitai::sources::{ClaudeSource, PiSource, Trash};
use index_common::{Env, assistant_line, title_line};

fn hit_ids(hits: &[Hit]) -> Vec<(String, bool)> {
    hits.iter()
        .map(|h| (h.entry.session.id.as_str().to_owned(), h.in_title))
        .collect()
}

fn ready(env: &Env) -> Index {
    let index = env.open();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    index
}

fn setup(name: &str) -> Env {
    let env = Env::new(name);
    env.write_session("login", "首页点了没反应", "我修复了登录页面的跳转问题");
    env.write_session(
        "tree",
        "Please tidy the repo",
        "Done: removed 3 stale worktrees.",
    );
    env.write_session("cart", "看一下", "好的");
    env.append("cart", &title_line("购物车结算"));
    env
}

#[test]
fn chinese_substring_and_english_prefix_hit_body() {
    let env = setup("body");
    let index = ready(&env);
    let s = |q: &str| hit_ids(&index.search(Vendor::Claude, q, 50).unwrap());

    // 「登录」夹在一串汉字中间。
    assert_eq!(s("登录"), [("login".to_owned(), false)]);
    assert_eq!(s("跳转 登录"), [("login".to_owned(), false)]);
    assert_eq!(s("worktree"), [("tree".to_owned(), false)]);
    assert_eq!(s("WORK"), [("tree".to_owned(), false)], "前缀、不分大小写");
    assert!(s("登录 worktree").is_empty(), "每个词都要命中");
    assert!(s("没出现过的词").is_empty());
}

#[test]
fn title_hits_come_first_and_include_renames() {
    let env = setup("title");
    env.append(
        "login",
        &assistant_line("顺便把购物车结算也看了", "2026-11-01T09:01:00.000Z"),
    );
    let index = ready(&env);

    let hits = index.search(Vendor::Claude, "结算", 50).unwrap();
    assert_eq!(
        hit_ids(&hits),
        [("cart".to_owned(), true), ("login".to_owned(), false)]
    );

    index
        .annotate(
            Vendor::Claude,
            &SessionId::from("tree"),
            Edit::Rename(Some("清理工作树".into())),
        )
        .unwrap();
    let hits = index.search(Vendor::Claude, "工作树", 50).unwrap();
    assert_eq!(hit_ids(&hits), [("tree".to_owned(), true)]);
    assert_eq!(hits[0].entry.title(), "清理工作树");
}

#[test]
fn odd_queries_do_not_error() {
    let env = setup("odd");
    let index = ready(&env);
    for q in [
        "",
        "   ",
        "\"",
        "*",
        "-",
        "AND",
        "a\"b",
        "(登录",
        "NEAR(x y)",
        "col:登录",
    ] {
        index
            .search(Vendor::Claude, q, 50)
            .unwrap_or_else(|e| panic!("搜 {q:?} 报错: {e}"));
    }
    assert_eq!(
        hit_ids(&index.search(Vendor::Claude, "(登录", 50).unwrap()),
        [("login".to_owned(), false)]
    );
}

#[test]
fn changed_content_is_reindexed_and_removed_sessions_stop_hitting() {
    let env = setup("update");
    let index = ready(&env);
    assert!(index.search(Vendor::Claude, "支付", 50).unwrap().is_empty());

    env.append(
        "cart",
        &assistant_line("新增了支付功能", "2026-11-01T10:00:00.000Z"),
    );
    fs::remove_file(env.session_path("login")).unwrap();
    index.refresh();
    assert_eq!(
        index.index_bodies(&mut || true).unwrap(),
        1,
        "只重建变了的那条"
    );

    assert_eq!(
        hit_ids(&index.search(Vendor::Claude, "支付", 50).unwrap()),
        [("cart".to_owned(), false)]
    );
    assert!(index.search(Vendor::Claude, "登录", 50).unwrap().is_empty());
    // 没变的不重建。
    assert_eq!(index.index_bodies(&mut || true).unwrap(), 0);
}

#[test]
fn search_stays_inside_one_vendor_and_limit_applies() {
    let env = setup("vendor");
    let pi_root = env.root.join("pi/sessions/--Users-demo-projects-blog--");
    fs::create_dir_all(&pi_root).unwrap();
    fs::write(
        pi_root.join("2026-11-20T10-00-00-000Z_p1.jsonl"),
        [
            r#"{"type":"session","version":3,"id":"p1","timestamp":"2026-11-20T10:00:00.000Z","cwd":"/Users/demo/projects/blog"}"#,
            r#"{"type":"message","id":"m1","timestamp":"2026-11-20T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"博客登录坏了"}],"timestamp":1763632801000}}"#,
        ]
        .join("\n"),
    )
    .unwrap();
    let trash = Trash::new(env.root.join("trash"));
    let index = Index::open(
        &env.data(),
        vec![
            Box::new(ClaudeSource::new(env.projects(), trash.clone())),
            Box::new(PiSource::new(env.root.join("pi/sessions"), trash)),
        ],
    )
    .unwrap();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();

    assert_eq!(
        hit_ids(&index.search(Vendor::Claude, "登录", 50).unwrap()),
        [("login".to_owned(), false)]
    );
    assert_eq!(
        hit_ids(&index.search(Vendor::Pi, "登录", 50).unwrap()),
        [("p1".to_owned(), true)],
        "pi 没有标题时用第一句话"
    );
    assert_eq!(
        index.search(Vendor::Claude, "worktree 3", 1).unwrap().len(),
        1
    );
    assert_eq!(index.search(Vendor::Claude, "好的", 1).unwrap().len(), 1);
}
