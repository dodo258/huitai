//! 正在写的对话：建全文索引时会话文件只是变长，就只读新增部分，新增的消息追加进正文、计数接着数；
//! 文件被截短或改写就整份重建（旧正文不再命中）；对话没了，追加的正文一起删掉；
//! 追加的段太多时整份重建一次。结果都和整份重读一致。

mod index_common;

use std::fs;

use huitai::core::Vendor;
use huitai::index::{BODY_MAX_PARTS, Index};
use index_common::{Env, assistant_line, user_line};

const AT: &str = "2026-11-01T09:10:00.000Z";

fn hits(index: &Index, q: &str) -> Vec<String> {
    index
        .search(Vendor::Claude, q, 50)
        .unwrap()
        .iter()
        .map(|h| h.entry.session.id.as_str().to_owned())
        .collect()
}

fn count(index: &Index, id: &str) -> usize {
    index
        .list(Vendor::Claude)
        .unwrap()
        .into_iter()
        .find(|e| e.session.id.as_str() == id)
        .unwrap()
        .session
        .message_count
}

/// 刷新列表再建正文（后台线程每轮做的事）。
fn update(env: &Env, index: &Index, id: &str) {
    index.refresh_paths(Vendor::Claude, &[env.session_path(id)]);
    index.index_bodies(&mut || true).unwrap();
}

fn ready(name: &str) -> (Env, Index) {
    let env = Env::new(name);
    env.write_session("s1", "最初的问题 alpha", "最初的回答");
    let index = env.open();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    assert_eq!(env.calls.take_counts(), [(None, false)], "第一次从头数");
    assert_eq!(env.calls.take_previews(), 1);
    (env, index)
}

#[test]
fn append_reads_only_the_new_part() {
    let (env, index) = ready("append");
    env.append("s1", &user_line("s1", "后来追加的 bravo", AT));
    env.append("s1", &assistant_line("回答 charlie", AT));
    update(&env, &index, "s1");

    let counts = env.calls.take_counts();
    assert_eq!(counts.len(), 1);
    assert!(
        counts[0].0.is_some() && counts[0].1,
        "接着上次数：{counts:?}"
    );
    assert_eq!(env.calls.take_previews(), 0, "正文只追加新的，不整份重取");
    assert_eq!(count(&index, "s1"), 2);
    for q in ["alpha", "bravo", "charlie", "追加"] {
        assert_eq!(hits(&index, q), ["s1"], "搜 {q}");
    }

    // 再追加一次：接着上一次的位置。
    env.append("s1", &user_line("s1", "第三个问题 delta", AT));
    update(&env, &index, "s1");
    let counts = env.calls.take_counts();
    assert!(counts[0].1);
    assert_eq!(count(&index, "s1"), 3);
    assert_eq!(hits(&index, "delta"), ["s1"]);
    assert_eq!(hits(&index, "bravo"), ["s1"], "之前追加的还在");
}

#[test]
fn rewritten_or_truncated_file_is_reindexed_whole() {
    let (env, index) = ready("rewrite");
    env.append("s1", &user_line("s1", "追加的 bravo", AT));
    update(&env, &index, "s1");
    assert!(env.calls.take_counts()[0].1);
    assert_eq!(hits(&index, "bravo"), ["s1"]);

    // 截短成只剩第一行：整份重读，追加过的那段不再命中。
    let path = env.session_path("s1");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{}\n", text.lines().next().unwrap())).unwrap();
    update(&env, &index, "s1");
    let counts = env.calls.take_counts();
    assert!(
        counts[0].0.is_some() && !counts[0].1,
        "要整份重数：{counts:?}"
    );
    assert_eq!(env.calls.take_previews(), 1);
    assert_eq!(count(&index, "s1"), 1);
    assert!(hits(&index, "bravo").is_empty());
    assert!(hits(&index, "最初的回答").is_empty());
    assert_eq!(hits(&index, "alpha"), ["s1"]);

    // 整个改写成更长的另一份：同样整份重读。
    env.write_session("s1", "换了 echo", "回答 foxtrot");
    env.append("s1", &user_line("s1", "再问 golf", AT));
    update(&env, &index, "s1");
    assert!(!env.calls.take_counts()[0].1);
    assert_eq!(count(&index, "s1"), 2);
    assert!(hits(&index, "alpha").is_empty());
    for q in ["echo", "foxtrot", "golf"] {
        assert_eq!(hits(&index, q), ["s1"], "搜 {q}");
    }
}

#[test]
fn removed_conversation_takes_appended_parts_along() {
    let (env, index) = ready("removed");
    env.append("s1", &user_line("s1", "追加的 hotel", AT));
    update(&env, &index, "s1");
    assert_eq!(hits(&index, "hotel"), ["s1"]);

    fs::remove_file(env.session_path("s1")).unwrap();
    index.refresh();
    assert!(hits(&index, "hotel").is_empty());
    assert!(hits(&index, "alpha").is_empty());

    // 新对话拿到同一个行号也搜不到旧对话追加的段。
    env.write_session("s2", "新的对话", "india");
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    assert!(hits(&index, "hotel").is_empty());
    assert_eq!(hits(&index, "india"), ["s2"]);
}

/// 一条对话正文分了好几段、每段都命中，搜索结果里也只出现一次，不挤掉别的对话。
#[test]
fn one_hit_per_conversation_across_parts() {
    let (env, index) = ready("parts");
    for i in 0..3 {
        env.append("s1", &user_line("s1", &format!("deploy 第 {i} 次"), AT));
        update(&env, &index, "s1");
    }
    // 另一条正文很长，相关度排在那几段后面：按行取前两名会是 s1 的两段。
    // 标题里没有这个词，只靠正文命中。
    let long = format!("also deploy {}", "filler words here ".repeat(300));
    env.write_session("s2", "另一个问题", &long);
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    let got = index.search(Vendor::Claude, "deploy", 2).unwrap();
    let mut ids: Vec<&str> = got.iter().map(|h| h.entry.session.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["s1", "s2"]);
}

/// 追加满 [`BODY_MAX_PARTS`] 段后整份重建一次，之后接着追加；计数一直对。
#[test]
fn too_many_parts_rebuild_once() {
    let (env, index) = ready("cap");
    for i in 0..BODY_MAX_PARTS {
        env.append("s1", &user_line("s1", &format!("问题 {i}"), AT));
        update(&env, &index, "s1");
    }
    assert_eq!(env.calls.take_previews(), 0);
    assert_eq!(count(&index, "s1"), 1 + BODY_MAX_PARTS as usize);

    env.append("s1", &user_line("s1", "满了之后 juliet", AT));
    update(&env, &index, "s1");
    assert_eq!(env.calls.take_previews(), 1, "段数满了整份重建");
    env.append("s1", &user_line("s1", "重建之后 kilo", AT));
    update(&env, &index, "s1");
    assert_eq!(env.calls.take_previews(), 0, "重建后又能追加");
    assert_eq!(count(&index, "s1"), 3 + BODY_MAX_PARTS as usize);
    for q in ["alpha", "juliet", "kilo"] {
        assert_eq!(hits(&index, q), ["s1"], "搜 {q}");
    }
}
