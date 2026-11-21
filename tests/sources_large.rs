//! 超大会话文件：扫描只读头尾，内存有上限，与文件多大无关；精确计数逐行读，超长行不进内存。
//! 内存用进程峰值（ru_maxrss）前后对比，所以这个文件只放这几个测试，别的测试不要加进来。
//! 实测耗时：`cargo test --release --test sources_large -- --ignored --nocapture --test-threads=1`。

use std::fs::{self, File};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Instant;

use huitai::sources::{ClaudeSource, Source, Trash};
use serde_json::json;

const MB: u64 = 1024 * 1024;

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("huitai-large-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// 进程到目前为止的内存峰值（字节；macOS 的 ru_maxrss 单位是字节）。
fn peak_rss() -> u64 {
    // SAFETY: getrusage 只往传进去的结构体里写，结构体全零是合法初值。
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: 同上，指针指向栈上有效的 rusage。
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    assert_eq!(rc, 0);
    u.ru_maxrss as u64
}

fn user(text: &str, i: usize) -> String {
    json!({"type": "user", "message": {"role": "user", "content": text},
           "timestamp": format!("2026-11-01T09:{:02}:{:02}.000Z", (i / 60) % 60, i % 60),
           "cwd": "/Users/demo/projects/big", "gitBranch": "main"})
    .to_string()
}

fn assistant(text: &str) -> String {
    json!({"type": "assistant",
           "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
           "timestamp": "2026-11-01T10:00:00.000Z", "cwd": "/Users/demo/projects/big"})
    .to_string()
}

fn setup(name: &str) -> (PathBuf, PathBuf, ClaudeSource) {
    let base = temp_dir(name);
    let root = base.join("projects");
    let dir = root.join("-Users-demo-projects-big");
    fs::create_dir_all(&dir).unwrap();
    let src = ClaudeSource::new(&root, Trash::new(base.join("trash")));
    (base, dir.join("big.jsonl"), src)
}

/// 200MB 的文件，中间是一整段不换行的空洞（稀疏文件，不占磁盘）：
/// 扫描只读头尾，精确计数时这一「行」超长被跳过，内存都不随文件变大。
#[test]
fn huge_sparse_file_scans_in_bounded_memory() {
    let (base, path, src) = setup("sparse");
    {
        let mut f = File::create(&path).unwrap();
        writeln!(f, "{}", user("开头的问题", 0)).unwrap();
        writeln!(f, "{}", assistant("开头的回答")).unwrap();
        f.seek(SeekFrom::Start(200 * MB)).unwrap();
        writeln!(f).unwrap();
        writeln!(f, "{}", user("最后的问题", 1)).unwrap();
        writeln!(f, "{}", assistant("最后的回答")).unwrap();
    }
    let before = peak_rss();
    let t = Instant::now();
    let scan = src.scan();
    let scan_ms = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(scan.sessions.len(), 1, "{:?}", scan.problems);
    let s = &scan.sessions[0];
    assert_eq!(s.title.as_deref(), Some("开头的问题"));
    assert_eq!(s.message_count, 2);

    let preview = src.load_preview(s, 5000).unwrap();
    assert_eq!(preview.last().map(|m| m.text.as_str()), Some("最后的回答"));
    let t = Instant::now();
    assert_eq!(src.count_messages(s).unwrap(), 2);
    let count_ms = t.elapsed().as_secs_f64() * 1000.0;

    let grew = peak_rss().saturating_sub(before);
    eprintln!(
        "稀疏 200MB：扫描 {scan_ms:.1}ms，精确计数 {count_ms:.0}ms，内存峰值增长 {:.1}MB",
        grew as f64 / MB as f64
    );
    // 预览最多往回读 8MB、计数单行上限 16MB；200MB 整读进来会远超这个数。
    assert!(grew < 48 * MB, "内存峰值涨了 {}MB", grew / MB);
    fs::remove_dir_all(base).unwrap();
}

/// 实打实写满 200MB 的普通对话（约 10 万行），量扫描、预览、精确计数的耗时和内存。耗时长，手动跑。
#[test]
#[ignore = "写 200MB 文件，手动跑：cargo test --release --test sources_large -- --ignored --nocapture"]
fn real_200mb_file_timing() {
    let (base, path, src) = setup("real");
    let filler = "这是一段比较长的回答，模拟真实对话里的代码说明和改动总结。".repeat(30);
    let mut users = 0;
    {
        let mut w = BufWriter::new(File::create(&path).unwrap());
        let mut i = 0;
        while w.get_ref().metadata().unwrap().len() < 200 * MB {
            for _ in 0..1000 {
                writeln!(w, "{}", user(&format!("第 {i} 个问题"), i)).unwrap();
                writeln!(w, "{}", assistant(&filler)).unwrap();
                users += 1;
                i += 1;
            }
            w.flush().unwrap();
        }
    }
    let len = fs::metadata(&path).unwrap().len();
    let before = peak_rss();

    let t = Instant::now();
    let scan = src.scan();
    let scan_ms = t.elapsed().as_secs_f64() * 1000.0;
    let s = &scan.sessions[0];
    assert_eq!(s.title.as_deref(), Some("第 0 个问题"));

    let t = Instant::now();
    let preview = src.load_preview(s, 5000).unwrap();
    let preview_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let n = src.count_messages(s).unwrap();
    let count_ms = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(n, users);

    let grew = peak_rss().saturating_sub(before);
    eprintln!(
        "实写 {:.0}MB（{users} 轮）：扫描 {scan_ms:.1}ms，预览最后 5000 条（读 8MB 内，取到 {} 条）{preview_ms:.0}ms，\
         精确计数 {count_ms:.0}ms，内存峰值增长 {:.1}MB",
        len as f64 / MB as f64,
        preview.len(),
        grew as f64 / MB as f64
    );
    assert!(grew < 64 * MB, "内存峰值涨了 {}MB", grew / MB);
    fs::remove_dir_all(base).unwrap();
}
