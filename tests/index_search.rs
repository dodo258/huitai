//! 全文搜索：标题（含会话台里改的名字）和正文都能搜到；中文按子串、英文按词前缀；
//! 内容变了重建、对话没了不再命中；只在当前那一家里搜，或各家一起搜（`search_all`）。

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

/// Claude 三条（见 [`setup`]）加 pi 一条「博客登录坏了」，正文都建好索引。
fn claude_and_pi(name: &str) -> (Env, Index) {
    let env = setup(name);
    let pi_root = env.root.join("pi/sessions/--Users-demo-projects-blog--");
    fs::create_dir_all(&pi_root).unwrap();
    fs::write(
        pi_root.join("2026-11-20T10-00-00-000Z_p1.jsonl"),
        [
            r#"{"type":"session","version":3,"id":"p1","timestamp":"2026-11-20T10:00:00.000Z","cwd":"/Users/demo/projects/blog"}"#,
            r#"{"type":"message","id":"m1","timestamp":"2026-11-20T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"博客登录坏了"}],"timestamp":1763632801000}}"#,
            r#"{"type":"message","id":"m2","timestamp":"2026-11-20T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"改好了跳转"}],"timestamp":1763632802000}}"#,
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
    (env, index)
}

#[test]
fn search_stays_inside_one_vendor_and_limit_applies() {
    let (_env, index) = claude_and_pi("vendor");

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

#[test]
fn search_all_merges_vendors_titles_first_then_body_by_relevance() {
    let (_env, index) = claude_and_pi("all");
    let vendors =
        |hits: &[Hit]| -> Vec<Vendor> { hits.iter().map(|h| h.entry.session.vendor).collect() };

    // pi 那条标题命中排前，Claude 那条只在正文里命中排后。
    let hits = index.search_all("登录", 50).unwrap();
    assert_eq!(
        hit_ids(&hits),
        [("p1".to_owned(), true), ("login".to_owned(), false)]
    );
    assert_eq!(vendors(&hits), [Vendor::Pi, Vendor::Claude]);

    // 两家都只在正文里命中：都在，各自标着正文命中。
    let hits = index.search_all("跳转", 50).unwrap();
    let mut ids: Vec<_> = hits.iter().map(|h| h.entry.session.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["login", "p1"]);
    assert!(hits.iter().all(|h| h.in_body && !h.in_title));

    // 条数上限对几家合起来算；一家里也搜得到的词各家一起搜不丢。
    assert_eq!(index.search_all("登录", 1).unwrap().len(), 1);
    assert_eq!(
        hit_ids(&index.search_all("worktree", 50).unwrap()),
        hit_ids(&index.search(Vendor::Claude, "worktree", 50).unwrap())
    );
    assert!(index.search_all("没出现过的词", 50).unwrap().is_empty());
}

#[test]
fn body_hits_carry_a_snippet_and_title_only_hits_do_not() {
    let env = setup("snippet");
    let index = ready(&env);

    // 「跳转」只在正文里：结果标着正文命中，读回的片段里有它，高亮落在它上面。
    let hits = index.search(Vendor::Claude, "跳转", 50).unwrap();
    assert_eq!(hit_ids(&hits), [("login".to_owned(), false)]);
    assert!(hits[0].in_body);
    let snip = index
        .snippet(&hits[0].entry.session, "跳转")
        .unwrap()
        .expect("正文里有这个词");
    assert_eq!(snip.text, "我修复了登录页面的跳转问题");
    let marked: Vec<&str> = snip.marks.iter().map(|&(a, b)| &snip.text[a..b]).collect();
    assert_eq!(marked, ["跳转"]);

    // 英文前缀、不分大小写，和查看视图的高亮一样按子串标。
    let hits = index.search(Vendor::Claude, "WORKTREE", 50).unwrap();
    let snip = index
        .snippet(&hits[0].entry.session, "WORKTREE")
        .unwrap()
        .unwrap();
    assert_eq!(&snip.text[snip.marks[0].0..snip.marks[0].1], "worktree");

    // 「结算」只在标题里：不标正文命中，界面不给它加那一行。
    let hits = index.search(Vendor::Claude, "结算", 50).unwrap();
    assert_eq!(hit_ids(&hits), [("cart".to_owned(), true)]);
    assert!(!hits[0].in_body);
}

/// 审计 v0.1.4 第 23 条：停用账号留在缓存里的对话不占条数上限，挤不掉当前账号的正文结果。
#[test]
fn stale_account_rows_do_not_eat_the_limit() {
    use huitai::core::Account;
    use huitai::sources::AccountSource;

    let env = Env::new("search-stale-account");
    env.write_session(
        "keep",
        "看一下",
        "这次只清了过期的缓存，别的都留着没动，顺手把日志也按天切开了。",
    );
    let other = Env::new("search-stale-account-b");
    for i in 0..5 {
        other.write_session(&format!("stale{i}"), "看一下", "缓存 缓存 缓存");
    }
    let trash = Trash::new(env.root.join("trash"));
    let account = Account::new(Vendor::Claude, "乙", other.root.join("claude"), &env.root);
    assert!(!account.key.is_empty(), "不在默认位置，是另一个账号");
    let both = Index::open(
        &env.data(),
        vec![
            Box::new(ClaudeSource::new(env.projects(), trash.clone())),
            Box::new(AccountSource::new(
                Box::new(ClaudeSource::new(other.projects(), trash.clone())),
                &account,
            )),
        ],
    )
    .unwrap();
    both.refresh();
    both.index_bodies(&mut || true).unwrap();
    assert_eq!(both.search(Vendor::Claude, "缓存", 50).unwrap().len(), 6);
    drop(both);

    // 设置里删了「乙」：它的对话留在缓存里，但不列、不搜，也不占 limit。
    let index = Index::open(
        &env.data(),
        vec![Box::new(ClaudeSource::new(env.projects(), trash))],
    )
    .unwrap();
    assert_eq!(
        hit_ids(&index.search(Vendor::Claude, "缓存", 1).unwrap()),
        [("keep".to_owned(), false)]
    );
}

/// 审计 v0.1.4 第 24 条：全文索引去变音、不分大小写（`unicode61 remove_diacritics 2`），搜 `cafe` 命中只含
/// `Café` 的正文；片段按同样的规则找，不能是空的。
#[test]
fn snippet_folds_diacritics_like_the_index() {
    let env = Env::new("search-fold");
    env.write_session("cafe", "看一下", "下午去 Café Lumière 坐坐");
    let index = ready(&env);
    let hits = index.search(Vendor::Claude, "cafe lumiere", 50).unwrap();
    assert_eq!(hit_ids(&hits), [("cafe".to_owned(), false)]);
    let snip = index
        .snippet(&hits[0].entry.session, "cafe lumiere")
        .unwrap()
        .expect("索引命中了，片段也得找得到");
    let marked: Vec<&str> = snip.marks.iter().map(|&(a, b)| &snip.text[a..b]).collect();
    assert_eq!(marked, ["Café", "Lumière"]);
}
