//! 按键到重绘耗时：2000 条对话、150 个项目，200×50 终端，每次按键后画一帧并编码成真实的终端字节
//! （CrosstermBackend 写进内存）。方案指标 ≤16ms。
//! 默认跑一遍只确认中位数达标；实测数字用 release 跑：
//! `cargo test --release --test ui_perf -- --nocapture`

mod ui_support;

use std::time::{Duration, Instant};

use huitai::core::Vendor;
use huitai::ui::{App, Theme, view};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};

use ui_support::*;

fn big_app() -> App {
    let fakes: Vec<Fake> = (0..2000u64)
        .map(|i| Fake {
            id: Box::leak(format!("perf-{i}").into_boxed_str()),
            cwd: Box::leak(format!("/Users/demo/projects/p{}", i % 150).into_boxed_str()),
            title: Box::leak(
                format!("第 {i} 条：把结算页的接口换成新版，顺便补测试").into_boxed_str(),
            ),
            mins: i * 7,
            messages: (i % 90) as usize,
            branch: Some("main"),
            pinned: i % 400 == 0,
            archived: false,
            automatic: i % 3 == 0,
        })
        .collect();
    let mut entries: Vec<_> = fakes.iter().map(|f| entry(Vendor::Claude, f)).collect();
    entries.sort_by_key(|e| !e.note.pinned);
    let mut app = demo_app(Vendor::Claude);
    app.set_entries(Vendor::Claude, entries);
    app
}

/// 吞掉终端字节（编码照做，只是不显示）。
struct Sink;

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn frame_times(app: &mut App, keys: &[KeyCode]) -> Vec<Duration> {
    let opts = TerminalOptions {
        viewport: Viewport::Fixed(Rect::new(0, 0, 200, 50)),
    };
    let mut term = Terminal::with_options(CrosstermBackend::new(Sink), opts).unwrap();
    term.draw(|f| view::draw(f, app, &Theme::DARK)).unwrap();
    keys.iter()
        .map(|k| {
            let t = Instant::now();
            app.key(key(*k));
            load_preview(app);
            term.draw(|f| view::draw(f, app, &Theme::DARK)).unwrap();
            t.elapsed()
        })
        .collect()
}

#[test]
fn key_to_redraw_within_16ms() {
    let mut app = big_app();
    let mut keys = vec![KeyCode::Char('j'); 300];
    keys.extend([KeyCode::PageDown; 20]);
    keys.extend([KeyCode::Tab, KeyCode::Tab, KeyCode::Tab]);
    keys.extend([
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Char('?'),
        KeyCode::Esc,
    ]);
    keys.extend([KeyCode::Char('k'); 100]);
    let mut t = frame_times(&mut app, &keys);
    t.sort();
    let pct = |p: usize| t[(t.len() - 1) * p / 100];
    println!(
        "按键→重绘（{} 次，200×50，2000 条）：中位 {:?}，p99 {:?}，最慢 {:?}",
        t.len(),
        pct(50),
        pct(99),
        t[t.len() - 1]
    );
    assert!(pct(50) <= Duration::from_millis(16), "中位数 {:?}", pct(50));
}
