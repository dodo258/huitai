//! 900MB 级的会话文件追加一行后，建全文索引（含精确计数）要多久：接着上次数只读新增部分，应在毫秒级。
//! 对照整份重数的耗时。写 900MB 文件，手动跑：
//! `cargo test --release --test index_append_perf -- --ignored --nocapture`

mod index_common;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::Instant;

use huitai::core::Vendor;
use huitai::sources::{ClaudeSource, Source, Trash};
use index_common::{Env, assistant_line, user_line};

const MB: u64 = 1024 * 1024;
const AT: &str = "2026-11-01T09:00:00.000Z";

#[test]
#[ignore = "写 900MB 文件，手动跑：cargo test --release --test index_append_perf -- --ignored --nocapture"]
fn append_one_line_to_900mb_session() {
    let env = Env::new("perf-900");
    let path = env.session_path("big");
    let filler = "这是一段比较长的回答，模拟真实对话里的代码说明和改动总结。".repeat(30);
    let mut users = 0;
    {
        let mut w = BufWriter::new(File::create(&path).unwrap());
        while w.get_ref().metadata().unwrap().len() < 900 * MB {
            for _ in 0..1000 {
                writeln!(w, "{}", user_line("big", &format!("第 {users} 个问题"), AT)).unwrap();
                writeln!(w, "{}", assistant_line(&filler, AT)).unwrap();
                users += 1;
            }
            w.flush().unwrap();
        }
    }
    let len = std::fs::metadata(&path).unwrap().len();
    let index = env.open();
    let count =
        |index: &huitai::index::Index| index.list(Vendor::Claude).unwrap()[0].session.message_count;

    let t = Instant::now();
    index.refresh();
    index.index_bodies(&mut || true).unwrap();
    let first_ms = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(count(&index), users);

    let mut appended_ms = Vec::new();
    for i in 0..5 {
        env.append("big", &user_line("big", &format!("追加的问题 zulu{i}"), AT));
        users += 1;
        let t = Instant::now();
        index.refresh_paths(Vendor::Claude, std::slice::from_ref(&path));
        index.index_bodies(&mut || true).unwrap();
        appended_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(count(&index), users);
        let hit = index
            .search(Vendor::Claude, &format!("zulu{i}"), 5)
            .unwrap();
        assert_eq!(hit.len(), 1);
    }

    // 对照：整份重数一遍。
    let src = ClaudeSource::new(env.projects(), Trash::new(env.root.join("trash")));
    let s = src.scan().sessions.remove(0);
    let t = Instant::now();
    assert_eq!(src.count_messages(&s).unwrap(), users);
    let whole_ms = t.elapsed().as_secs_f64() * 1000.0;

    appended_ms.sort_by(f64::total_cmp);
    eprintln!(
        "{:.0}MB（{users} 轮）：第一次建索引（整份数+取正文）{first_ms:.0}ms；追加一行后更新（刷新+建正文）\
         5 次中位 {:.2}ms、最慢 {:.2}ms；对照整份重数 {whole_ms:.0}ms",
        len as f64 / MB as f64,
        appended_ms[2],
        appended_ms[4],
    );
    assert!(appended_ms[2] < 50.0, "追加后更新太慢：{appended_ms:?}");
    std::fs::remove_dir_all(&env.root).unwrap();
}
