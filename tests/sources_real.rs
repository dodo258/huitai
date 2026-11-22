//! 本机实测：扫描真实的三家目录，只打印条数、耗时、折叠组和问题数，不打印任何对话内容。
//! 默认不跑：`cargo test --release --test sources_real -- --ignored --nocapture`。

use std::path::PathBuf;
use std::time::Instant;

use huitai::sources::{FoldGroup, Trash, default_sources, fold_group};

#[test]
#[ignore = "读本机真实会话目录，手动跑"]
fn scan_real_dirs() {
    let home = PathBuf::from(std::env::var_os("HOME").expect("需要 HOME"));
    // 回收站放临时目录：实测不碰真实回收站。
    let trash = Trash::new(std::env::temp_dir().join("huitai-real-scan-trash"));
    let all = Instant::now();
    for src in default_sources(&home, trash) {
        let t = Instant::now();
        let scan = src.scan();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let agentq = scan
            .sessions
            .iter()
            .filter(|s| fold_group(&s.cwd, &home) == Some(FoldGroup::Agentq))
            .count();
        let loose = scan
            .sessions
            .iter()
            .filter(|s| fold_group(&s.cwd, &home) == Some(FoldGroup::Loose))
            .count();
        let titled = scan.sessions.iter().filter(|s| s.title.is_some()).count();
        let preview_t = Instant::now();
        let previewed = scan
            .sessions
            .iter()
            .take(20)
            .filter(|s| src.load_preview(s, 5).is_ok_and(|p| !p.is_empty()))
            .count();
        let preview_ms =
            preview_t.elapsed().as_secs_f64() * 1000.0 / scan.sessions.len().clamp(1, 20) as f64;
        println!(
            "{}: {} 条, 扫描 {ms:.0} ms, agentq 组 {agentq}, 零散组 {loose}, 有标题 {titled}, 问题 {}, 预览 {previewed}/{} 条有内容（平均 {preview_ms:.1} ms/条）",
            src.vendor(),
            scan.sessions.len(),
            scan.problems.len(),
            scan.sessions.len().min(20),
        );
    }
    println!("三家合计 {:.0} ms", all.elapsed().as_secs_f64() * 1000.0);
}

/// 整段对话：每家挑最大的那条读全文，打印文件大小、块数、耗时（不打印内容）。
#[test]
#[ignore = "读本机真实会话目录，手动跑"]
fn transcript_of_largest_real_session() {
    let home = PathBuf::from(std::env::var_os("HOME").expect("需要 HOME"));
    let trash = Trash::new(std::env::temp_dir().join("huitai-real-transcript-trash"));
    for src in default_sources(&home, trash) {
        let scan = src.scan();
        let size = |s: &huitai::core::Session| std::fs::metadata(&s.path).map_or(0, |m| m.len());
        // 最大的文件、最近一条、最近一条没有会话文件的（Codex 只在正文库里的）。
        let largest = scan.sessions.iter().max_by_key(|s| size(s));
        let latest = scan.sessions.iter().max_by_key(|s| s.last_active);
        let db_only = scan
            .sessions
            .iter()
            .filter(|s| !s.path.is_file())
            .max_by_key(|s| s.last_active);
        for s in largest.into_iter().chain(latest).chain(db_only) {
            let t = Instant::now();
            let r = src.load_transcript(s);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            match r {
                Ok(tr) => println!(
                    "{:?} {:.1}MB：{} 块，只读了尾部 {}，{ms:.0}ms",
                    src.vendor(),
                    size(s) as f64 / 1048576.0,
                    tr.parts.len(),
                    tr.truncated
                ),
                Err(e) => println!("{:?} 读不了：{e}", src.vendor()),
            }
        }
    }
}
