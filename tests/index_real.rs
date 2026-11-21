//! 本机实测：用真实的三家目录建索引，只打印条数、耗时、索引大小，不打印任何对话内容。
//! 缓存和标注放临时目录，不碰真实数据目录。
//! 默认不跑：`cargo test --release --test index_real -- --ignored --nocapture`。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use huitai::core::Vendor;
use huitai::index::{Index, Update};
use huitai::sources::{Trash, default_sources};

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

#[test]
#[ignore = "读本机真实会话目录，手动跑"]
fn measure_real_dirs() {
    let home = PathBuf::from(std::env::var_os("HOME").expect("需要 HOME"));
    let data = std::env::temp_dir().join(format!("huitai-index-real-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let trash = || Trash::new(data.join("trash"));
    let open = || Index::open(&data, default_sources(&home, trash())).unwrap();

    // 1. 第一次：没有缓存，后台首扫。start 立刻返回；等三家的 Listed 都到。
    let t = Instant::now();
    let index = open();
    let (bg, rx) = index.start();
    println!("首次 open + start 返回: {}", ms(t.elapsed()));
    let mut listed = Vec::new();
    while listed.len() < 3 {
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Update::Listed(v)) if !listed.contains(&v) => {
                listed.push(v);
                println!("  {v} 列表就绪: {}", ms(t.elapsed()));
            }
            Ok(Update::Problem(_)) | Ok(_) => {}
            Err(_) => break,
        }
    }
    let first_scan = t.elapsed();
    println!("首次全量扫描（三家列表都出来）: {}", ms(first_scan));
    for v in Vendor::ALL {
        println!("  {v}: {} 条", index.list(v).unwrap().len());
    }

    // 2. 全文索引在后台继续建，等它建完。
    let t2 = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(Update::Searchable) => break,
            Ok(_) => {}
            Err(_) => {
                println!("全文索引 120 秒内没建完");
                break;
            }
        }
    }
    println!("全文索引建完（首扫之后）: {}", ms(t2.elapsed()));
    drop(bg);
    drop(index);
    let size = |name: &str| std::fs::metadata(data.join(name)).map_or(0, |m| m.len());
    println!(
        "索引库大小: {:.1} MB（含 wal {:.1} MB）",
        (size("index.sqlite") + size("index.sqlite-wal")) as f64 / 1e6,
        size("index.sqlite-wal") as f64 / 1e6
    );

    // 3. 冷启动（有缓存）：open + 三家 list。
    let mut best = Duration::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        let index = open();
        let n: usize = Vendor::ALL
            .iter()
            .map(|v| index.list(*v).unwrap().len())
            .sum();
        best = best.min(t.elapsed());
        assert!(n > 0);
    }
    println!("有缓存冷启动 open + 三家 list（5 次最快）: {}", ms(best));
    let t = Instant::now();
    let index = open();
    let n = index.list(Vendor::Claude).unwrap().len();
    println!(
        "有缓存冷启动 open + Claude list（{n} 条）: {}",
        ms(t.elapsed())
    );

    // 4. 没有变化时的增量扫描（每家分开计时；FSEvents 触发时只扫变了的那一家）。
    for v in Vendor::ALL {
        let t = Instant::now();
        let r = index.refresh_vendor(v);
        println!(
            "增量扫描 {v}（无变化）: {}，重读 {} 未变 {} 去掉 {} 问题 {}",
            ms(t.elapsed()),
            r.read,
            r.unchanged,
            r.removed,
            r.problems.len()
        );
    }

    // FSEvents 报来一个会话文件时（Claude 正在写）只看这一个。
    let newest = index.list(Vendor::Claude).unwrap()[0].session.path.clone();
    let t = Instant::now();
    let r = index.refresh_paths(Vendor::Claude, &[newest]);
    println!(
        "增量扫描 Claude 单个文件事件: {}，重读 {}",
        ms(t.elapsed()),
        r.read
    );

    // 5. 搜索耗时（只打印命中条数）。
    for q in ["登录", "额度", "worktree", "test", "数据库 迁移"] {
        let t = Instant::now();
        let hits = index.search(Vendor::Claude, q, 200).unwrap();
        println!(
            "  搜索 {} 个字的词: {} 条, {}",
            q.chars().count(),
            hits.len(),
            ms(t.elapsed())
        );
    }

    let _ = std::fs::remove_dir_all(&data);
}
