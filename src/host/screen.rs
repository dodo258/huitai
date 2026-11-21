//! 内存终端状态机：守护进程把 PTY 输出喂给它，重连时用它生成「补发画面」的字节序列，
//! 会话台实时预览时用它导出此刻的可见屏幕（[`Screen::snapshot`]）。
//!
//! 状态机用 `alacritty_terminal`（选型见 DECISIONS.md）。补发顺序：清屏 →（备用屏）→ 历史和当前画面 →
//! 已开启的终端模式（键盘协议、括号粘贴、鼠标、焦点等）→ 光标位置和当前画笔。
//!
//! 历史分两段存：最近的不到 [`LIVE_HISTORY`] 行留在状态机里，更早的压成补发用的字节序列放进 `archive`。
//! alacritty 每格 24 字节、每行按全宽分配，1 万行 × 120 列要 29 MB；压成序列后一行通常一两百字节。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color, CursorShape, CursorStyle, NamedColor, Processor, Timeout,
};
use alacritty_terminal::vte::{self, Params, Perform};

use super::protocol::{CellColor, Run, ScreenSnapshot, attr};

/// 画面版本号在整个进程里递增，同一条对话退出后重开（新状态机）也不会和旧版本撞号。
static NEXT_VERSION: AtomicU64 = AtomicU64::new(1);

fn next_version() -> u64 {
    NEXT_VERSION.fetch_add(1, Ordering::Relaxed)
}

/// 保留的历史行数。
pub const HISTORY_LINES: usize = 10_000;
/// 状态机里的历史攒到这么多行，就整段压进 `archive`。
const LIVE_HISTORY: usize = 256;

/// 收集状态机替终端作答的字节（DA、DSR、键盘协议查询等），没人接着时由守护进程代答。
#[derive(Clone, Default)]
struct Replies(Arc<Mutex<Vec<u8>>>);

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(text.as_bytes());
        }
    }
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

fn size(cols: u16, rows: u16) -> Size {
    Size {
        cols: usize::from(cols.max(2)),
        rows: usize::from(rows.max(1)),
    }
}

/// 不做同步更新（`CSI ? 2026 h`）缓冲：那是给屏幕渲染防闪烁用的，守护进程的状态机要字节到了就生效，
/// 不然 CLI 开了同步更新没关（比如中途崩了）画面会一直停在旧的，也没法按字节位置判断清空历史什么时候生效。
#[derive(Default)]
struct NoSync;

impl Timeout for NoSync {
    fn set_timeout(&mut self, _: std::time::Duration) {}
    fn clear_timeout(&mut self) {}
    fn pending_timeout(&self) -> bool {
        false
    }
}

/// 在字节喂给状态机之前先过一遍，找出会清空滚动历史的序列：`CSI 3 J`（清空历史）和 `ESC c`（终端重置）。
/// 找到就停在它后面，让 [`Screen::feed`] 在这里切开，把压缩出去的历史也一起清掉。认法和 alacritty 一致。
#[derive(Default)]
struct ClearScan {
    hit: Option<Clear>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Clear {
    /// `CSI 3 J`：清当前这块屏的历史（备用屏时不动主屏历史）。
    History,
    /// `ESC c`：两块屏连同历史全部重置。
    Reset,
}

impl Perform for ClearScan {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        let first = params.iter().next().and_then(|p| p.first().copied());
        if action == 'J' && intermediates.is_empty() && !ignore && first == Some(3) {
            self.hit = Some(Clear::History);
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if byte == b'c' && intermediates.is_empty() {
            self.hit = Some(Clear::Reset);
        }
    }

    fn terminated(&self) -> bool {
        self.hit.is_some()
    }
}

pub struct Screen {
    term: Term<Replies>,
    parser: Processor<NoSync>,
    /// 和 `parser` 读同样的字节，只为认出清空历史的序列。
    clear_parser: vte::Parser,
    clears: ClearScan,
    replies: Replies,
    /// 画面版本：每次喂字节或改大小换一个新号。
    version: u64,
    /// 比状态机里的历史更早的行，从最早的开始，一次压缩一块。
    archive: VecDeque<Block>,
    /// `archive` 里一共多少行（含 `archive_skip`）。
    archive_rows: usize,
    /// 第一块开头已经超出历史上限、不再算数的行数。
    archive_skip: usize,
}

/// 一次压缩挪进来的一批历史行。整批的字节序列连在一起存，免得每行一次分配把内存弄得很碎。
struct Block {
    /// 每行是补发用的字节序列：从默认画笔开始画，结尾回到默认画笔。
    seq: String,
    /// 每行在 `seq` 里的结束位置，和这一行是否自动折到下一行。
    rows: Vec<(usize, bool)>,
}

impl Block {
    fn rows(&self) -> impl Iterator<Item = (&str, bool)> {
        let mut start = 0;
        self.rows.iter().map(move |&(end, wrapped)| {
            let row = &self.seq[start..end];
            start = end;
            (row, wrapped)
        })
    }
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Self {
        let replies = Replies::default();
        let config = Config {
            scrolling_history: HISTORY_LINES,
            kitty_keyboard: true,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size(cols, rows), replies.clone()),
            parser: Processor::new(),
            clear_parser: vte::Parser::new(),
            clears: ClearScan::default(),
            replies,
            version: next_version(),
            archive: VecDeque::new(),
            archive_rows: 0,
            archive_skip: 0,
        }
    }

    pub fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            // 喂到清空历史的序列（含）为止：这时状态机已经清了自己那段，压缩出去的那段跟着清。
            let n = self
                .clear_parser
                .advance_until_terminated(&mut self.clears, bytes);
            self.parser.advance(&mut self.term, &bytes[..n]);
            match self.clears.hit.take() {
                Some(Clear::History) if !self.mode().contains(TermMode::ALT_SCREEN) => {
                    self.clear_archive()
                }
                Some(Clear::Reset) => self.clear_archive(),
                _ => {}
            }
            bytes = &bytes[n..];
        }
        self.compact();
        self.version = next_version();
    }

    fn clear_archive(&mut self) {
        self.archive.clear();
        self.archive_rows = 0;
        self.archive_skip = 0;
    }

    /// 状态机里的历史攒够了就压进 `archive` 并清掉（备用屏时历史在另一块屏上，不动）。
    fn compact(&mut self) {
        let lines = self.term.history_size();
        if lines < LIVE_HISTORY || self.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        let mut prev_wrapped = self
            .archive
            .back()
            .and_then(|b| b.rows.last())
            .is_some_and(|&(_, w)| w);
        let mut block = Block {
            seq: String::new(),
            rows: Vec::with_capacity(lines),
        };
        for l in -(lines as i32)..0 {
            let mut pen = Pen::default();
            let wrapped = self.draw_row(&mut block.seq, &mut pen, Line(l), prev_wrapped);
            if pen != Pen::default() {
                block.seq.push_str("\x1b[0m");
            }
            block.rows.push((block.seq.len(), wrapped));
            prev_wrapped = wrapped;
        }
        block.seq.shrink_to_fit();
        self.term.grid_mut().clear_history();
        self.archive.push_back(block);
        self.archive_rows += lines;
        // 超出上限的行先记着跳过，整块都超出了再扔掉。
        self.archive_skip += (self.archive_rows - self.archive_skip).saturating_sub(HISTORY_LINES);
        while let Some(front) = self.archive.front()
            && front.rows.len() <= self.archive_skip
        {
            self.archive_skip -= front.rows.len();
            self.archive_rows -= front.rows.len();
            self.archive.pop_front();
        }
    }

    /// 补发和历史文字里用到的压缩行（连同状态机里的历史不超过 [`HISTORY_LINES`]）。
    fn archived(&self) -> impl Iterator<Item = (&str, bool)> {
        let kept = self.archive_rows - self.archive_skip;
        let extra = (kept + self.term.history_size()).saturating_sub(HISTORY_LINES);
        self.archive
            .iter()
            .flat_map(Block::rows)
            .skip(self.archive_skip + extra)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(size(cols, rows));
        self.version = next_version();
    }

    /// 画面版本，有输出或改大小就变。
    pub fn version(&self) -> u64 {
        self.version
    }

    /// 此刻的可见屏幕（不含历史），每格带颜色和样式，行尾空白去掉。只读，不动状态机。
    pub fn snapshot(&self) -> ScreenSnapshot {
        let (cols, rows) = self.size();
        let lines = (0..self.term.screen_lines() as i32)
            .map(|l| self.row_runs(Line(l)))
            .collect();
        ScreenSnapshot {
            version: self.version,
            cols,
            rows,
            lines,
        }
    }

    fn row_runs(&self, line: Line) -> Vec<Run> {
        let row = &self.term.grid()[line];
        let end = (0..self.term.columns())
            .rev()
            .find(|&c| !is_blank(&row[Column(c)]))
            .map_or(0, |c| c + 1);
        let mut runs: Vec<Run> = Vec::new();
        for col in 0..end {
            let cell = &row[Column(col)];
            if cell.flags.intersects(SPACERS) {
                continue;
            }
            let (fg, bg, attrs) = (cell_color(cell.fg), cell_color(cell.bg), cell_attrs(cell));
            match runs.last_mut() {
                Some(r) if (r.fg, r.bg, r.attrs) == (fg, bg, attrs) => {
                    push_cell_chars(&mut r.text, cell)
                }
                _ => {
                    let mut text = String::new();
                    push_cell_chars(&mut text, cell);
                    runs.push(Run {
                        text,
                        fg,
                        bg,
                        attrs,
                    });
                }
            }
        }
        runs
    }

    /// (列, 行)。
    pub fn size(&self) -> (u16, u16) {
        let clamp = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
        (clamp(self.term.columns()), clamp(self.term.screen_lines()))
    }

    /// 取走状态机替终端作答的字节。
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(
            &mut *self
                .replies
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// 重连时补发给新终端的完整字节序列。
    pub fn restore_sequence(&self) -> Vec<u8> {
        let mut out = String::from("\x1b[0m");
        let mut pen = Pen::default();
        if self.mode().contains(TermMode::ALT_SCREEN) {
            out.push_str("\x1b[?1049h\x1b[H\x1b[J");
            self.draw_fixed(&mut out, &mut pen);
        } else {
            out.push_str("\x1b[H\x1b[J");
            self.draw_flowing(&mut out, &mut pen);
        }
        out.push_str("\x1b[0m");
        self.write_modes(&mut out);
        self.write_cursor(&mut out);
        out.into_bytes()
    }

    /// 当前画面各行文字（去掉行尾空白）。
    pub fn visible_text(&self) -> Vec<String> {
        (0..self.term.screen_lines() as i32)
            .map(|l| self.row_text(Line(l)))
            .collect()
    }

    /// 历史区各行文字（不含当前画面），从最早的一行开始。
    pub fn history_text(&self) -> Vec<String> {
        let archived = self.archived().map(|(seq, _)| {
            let mut text = strip_escapes(seq);
            text.truncate(text.trim_end().len());
            text
        });
        archived
            .chain((-(self.term.history_size() as i32)..0).map(|l| self.row_text(Line(l))))
            .collect()
    }

    /// 画面的完整指纹（各格字符和颜色属性、模式、光标），用来比较两次画面是否一致。
    pub fn signature(&self) -> String {
        let mut out = String::new();
        let mut pen = Pen::default();
        self.draw_fixed(&mut out, &mut pen);
        out.push_str("\x1b[0m");
        self.write_modes(&mut out);
        self.write_cursor(&mut out);
        out
    }

    /// 当前 kitty 键盘协议标志（0 表示没开）。
    pub fn kitty_keyboard_flags(&self) -> u8 {
        let m = self.mode();
        [
            (TermMode::DISAMBIGUATE_ESC_CODES, 1),
            (TermMode::REPORT_EVENT_TYPES, 2),
            (TermMode::REPORT_ALTERNATE_KEYS, 4),
            (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
            (TermMode::REPORT_ASSOCIATED_TEXT, 16),
        ]
        .into_iter()
        .filter(|(flag, _)| m.contains(*flag))
        .map(|(_, bit)| bit)
        .sum()
    }

    pub fn bracketed_paste(&self) -> bool {
        self.mode().contains(TermMode::BRACKETED_PASTE)
    }

    fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    fn row_text(&self, line: Line) -> String {
        let row = &self.term.grid()[line];
        let mut s = String::new();
        for col in 0..self.term.columns() {
            let cell = &row[Column(col)];
            if !cell.flags.intersects(SPACERS) {
                push_cell_chars(&mut s, cell);
            }
        }
        s.truncate(s.trim_end().len());
        s
    }

    /// 每行用光标定位画（备用屏、指纹用），不滚动。
    fn draw_fixed(&self, out: &mut String, pen: &mut Pen) {
        for l in 0..self.term.screen_lines() as i32 {
            let _ = write!(out, "\x1b[{};1H", l + 1);
            self.draw_row(out, pen, Line(l), false);
        }
    }

    /// 从最早的历史行顺序往下写：多出来的行自然滚进新终端自己的回滚区，最后一屏正好是当前画面。
    /// 自动换行的行不写换行，让新终端按自己的宽度接着折。
    fn draw_flowing(&self, out: &mut String, pen: &mut Pen) {
        let mut first = true;
        let mut prev_wrapped = false;
        // 换行前回到默认画笔，免得新滚出来的行被当前背景色填满。
        let mut next_row = |out: &mut String, pen: &mut Pen, prev_wrapped: bool| {
            if !first && !prev_wrapped {
                if *pen != Pen::default() {
                    out.push_str("\x1b[0m");
                    *pen = Pen::default();
                }
                out.push_str("\r\n");
            }
            first = false;
        };
        // 压缩行从默认画笔开始画、结尾回到默认画笔。
        for (seq, wrapped) in self.archived() {
            next_row(out, pen, prev_wrapped);
            out.push_str(seq);
            prev_wrapped = wrapped;
        }
        let top = -(self.term.history_size() as i32);
        for l in top..self.term.screen_lines() as i32 {
            next_row(out, pen, prev_wrapped);
            prev_wrapped = self.draw_row(out, pen, Line(l), prev_wrapped);
        }
    }

    /// 画一行，返回这一行是否自动折到下一行。`force` 时至少写一个字符，把上一行留下的待折行落实。
    fn draw_row(&self, out: &mut String, pen: &mut Pen, line: Line, force: bool) -> bool {
        let row = &self.term.grid()[line];
        let cols = self.term.columns();
        let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        let end = if wrapped {
            cols
        } else {
            (0..cols)
                .rev()
                .find(|&c| !is_blank(&row[Column(c)]))
                .map_or(0, |c| c + 1)
        };
        let mut wrote = false;
        for col in 0..end {
            let cell = &row[Column(col)];
            if cell.flags.intersects(SPACERS) {
                continue;
            }
            pen.apply(cell, out);
            push_cell_chars(out, cell);
            wrote = true;
        }
        if force && !wrote {
            out.push(' ');
        }
        wrapped
    }

    fn write_modes(&self, out: &mut String) {
        let m = self.mode();
        let private = [
            (TermMode::APP_CURSOR, 1),
            (TermMode::ORIGIN, 6),
            (TermMode::MOUSE_REPORT_CLICK, 1000),
            (TermMode::MOUSE_DRAG, 1002),
            (TermMode::MOUSE_MOTION, 1003),
            (TermMode::FOCUS_IN_OUT, 1004),
            (TermMode::UTF8_MOUSE, 1005),
            (TermMode::SGR_MOUSE, 1006),
            (TermMode::BRACKETED_PASTE, 2004),
        ];
        for (flag, n) in private {
            if m.contains(flag) {
                let _ = write!(out, "\x1b[?{n}h");
            }
        }
        if !m.contains(TermMode::LINE_WRAP) {
            out.push_str("\x1b[?7l");
        }
        if !m.contains(TermMode::SHOW_CURSOR) {
            out.push_str("\x1b[?25l");
        }
        if m.contains(TermMode::APP_KEYPAD) {
            out.push_str("\x1b=");
        }
        if m.contains(TermMode::INSERT) {
            out.push_str("\x1b[4h");
        }
        if m.contains(TermMode::LINE_FEED_NEW_LINE) {
            out.push_str("\x1b[20h");
        }
        let kitty = self.kitty_keyboard_flags();
        if kitty != 0 {
            let _ = write!(out, "\x1b[>{kitty}u");
        }
        let style = self.term.cursor_style();
        if style != CursorStyle::default() {
            let n = match (style.shape, style.blinking) {
                (CursorShape::Underline, true) => 3,
                (CursorShape::Underline, false) => 4,
                (CursorShape::Beam, true) => 5,
                (CursorShape::Beam, false) => 6,
                (_, true) => 1,
                (_, false) => 2,
            };
            let _ = write!(out, "\x1b[{n} q");
        }
    }

    fn write_cursor(&self, out: &mut String) {
        let cursor = &self.term.grid().cursor;
        let _ = write!(
            out,
            "\x1b[{};{}H",
            cursor.point.line.0 + 1,
            cursor.point.column.0 + 1
        );
        Pen::default().apply(&cursor.template, out);
    }
}

/// 去掉压缩行里的 CSI 序列（只有 SGR），剩下文字。
fn strip_escapes(seq: &str) -> String {
    let mut out = String::with_capacity(seq.len());
    let mut chars = seq.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // ESC [ 参数 … 结束字节（0x40–0x7e）。
        chars.next();
        for c in chars.by_ref() {
            if ('\x40'..='\x7e').contains(&c) {
                break;
            }
        }
    }
    out
}

const SPACERS: Flags = Flags::WIDE_CHAR_SPACER.union(Flags::LEADING_WIDE_CHAR_SPACER);
const STYLE_FLAGS: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

fn is_blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell
            .flags
            .intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
        && cell.zerowidth().is_none()
}

/// 状态机的颜色换成快照颜色：前景/背景/光标这类「默认色」统一成 `Default`，暗色换成对应的基本色（暗由样式位表示）。
fn cell_color(c: Color) -> CellColor {
    match c {
        Color::Spec(rgb) => CellColor::Rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(i) => CellColor::Indexed(i),
        Color::Named(named) => match named as u16 {
            n @ 0..=15 => CellColor::Indexed(n as u8),
            n if (NamedColor::DimBlack as u16..=NamedColor::DimWhite as u16).contains(&n) => {
                CellColor::Indexed((n - NamedColor::DimBlack as u16) as u8)
            }
            _ => CellColor::Default,
        },
    }
}

fn cell_attrs(cell: &Cell) -> u8 {
    [
        (Flags::BOLD, attr::BOLD),
        (Flags::DIM, attr::DIM),
        (Flags::ITALIC, attr::ITALIC),
        (Flags::ALL_UNDERLINES, attr::UNDERLINE),
        (Flags::INVERSE, attr::INVERSE),
        (Flags::HIDDEN, attr::HIDDEN),
        (Flags::STRIKEOUT, attr::STRIKEOUT),
    ]
    .into_iter()
    .filter(|(flag, _)| cell.flags.intersects(*flag))
    .fold(0, |acc, (_, bit)| acc | bit)
}

fn push_cell_chars(out: &mut String, cell: &Cell) {
    out.push(cell.c);
    if let Some(extra) = cell.zerowidth() {
        out.extend(extra);
    }
}

/// 当前画笔（SGR 状态），只在变化时写新的 SGR。
#[derive(Clone, Copy, PartialEq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
            underline: None,
        }
    }
}

impl Pen {
    fn apply(&mut self, cell: &Cell, out: &mut String) {
        let next = Pen {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & STYLE_FLAGS,
            underline: cell.underline_color(),
        };
        if next == *self {
            return;
        }
        *self = next;
        out.push_str("\x1b[0");
        let f = next.flags;
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::UNDERLINE, "4"),
            (Flags::DOUBLE_UNDERLINE, "4:2"),
            (Flags::UNDERCURL, "4:3"),
            (Flags::DOTTED_UNDERLINE, "4:4"),
            (Flags::DASHED_UNDERLINE, "4:5"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
        ] {
            if f.contains(flag) {
                out.push(';');
                out.push_str(code);
            }
        }
        color_params(out, next.fg, 30, 90, 38);
        color_params(out, next.bg, 40, 100, 48);
        if let Some(c) = next.underline {
            color_params(out, c, 58, 58, 58);
        }
        out.push('m');
    }
}

/// 写一个颜色的 SGR 参数。`base`/`bright` 是 8 色和亮 8 色的起点，`ext` 是 256 色/真彩色的引导码。
fn color_params(out: &mut String, color: Color, base: u16, bright: u16, ext: u16) {
    let _ = match color {
        Color::Spec(rgb) => write!(out, ";{ext};2;{};{};{}", rgb.r, rgb.g, rgb.b),
        Color::Indexed(i) => write!(out, ";{ext};5;{i}"),
        Color::Named(named) => match named as u16 {
            // 下划线颜色没有 8 色写法，一律用 256 色索引。
            n @ 0..=15 if ext == 58 => write!(out, ";58;5;{n}"),
            n @ 0..=7 => write!(out, ";{}", base + n),
            n @ 8..=15 => write!(out, ";{}", bright + n - 8),
            _ => Ok(()),
        },
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_with(cols: u16, rows: u16, bytes: &[u8]) -> Screen {
        let mut s = Screen::new(cols, rows);
        s.feed(bytes);
        s
    }

    /// 补发序列喂给一块同尺寸的新屏，画面指纹必须一致。
    fn assert_restores(src: &Screen) -> Screen {
        let (cols, rows) = src.size();
        let copy = screen_with(cols, rows, &src.restore_sequence());
        assert_eq!(copy.signature(), src.signature());
        assert_eq!(copy.visible_text(), src.visible_text());
        copy
    }

    #[test]
    fn restores_colored_text_and_cursor() {
        let src = screen_with(
            20,
            5,
            b"\x1b[1;31mred\x1b[0m plain \x1b[38;2;1;2;3;48;5;200mrgb\x1b[0m\r\n\x1b[4:3;58;5;9mcurl\x1b[0m\r\n\xe4\xb8\xad\xe6\x96\x87 wide\x1b[2;3H\x1b[7m",
        );
        assert_restores(&src);
    }

    #[test]
    fn restores_history_into_scrollback() {
        let mut bytes = Vec::new();
        for i in 0..30 {
            bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        let src = screen_with(20, 5, &bytes);
        let copy = assert_restores(&src);
        assert_eq!(copy.history_text(), src.history_text());
        assert_eq!(src.history_text()[0], "line 0");
    }

    #[test]
    fn wrapped_lines_stay_wrapped() {
        let src = screen_with(10, 4, b"0123456789abcdefghij\r\nnext");
        let copy = assert_restores(&src);
        assert_eq!(
            copy.visible_text()[..3],
            ["0123456789", "abcdefghij", "next"]
        );
    }

    #[test]
    fn wide_char_at_line_end_wraps_the_same() {
        let src = screen_with(5, 4, "abcd中文x".as_bytes());
        assert_restores(&src);
    }

    #[test]
    fn restores_alt_screen() {
        let src = screen_with(20, 5, b"main\x1b[?1049h\x1b[2;2Halt \x1b[32mgreen");
        let copy = assert_restores(&src);
        assert!(copy.mode().contains(TermMode::ALT_SCREEN));
    }

    #[test]
    fn replays_modes_including_kitty_keyboard() {
        let src = screen_with(
            20,
            5,
            b"\x1b[>1u\x1b[?2004h\x1b[?1004h\x1b[?1000h\x1b[?1006h\x1b[?25l\x1b[?1h\x1b[5 q",
        );
        assert_eq!(src.kitty_keyboard_flags(), 1);
        let seq = String::from_utf8(src.restore_sequence()).unwrap();
        for want in [
            "\x1b[>1u",
            "\x1b[?2004h",
            "\x1b[?1004h",
            "\x1b[?1000h",
            "\x1b[?1006h",
            "\x1b[?25l",
            "\x1b[?1h",
            "\x1b[5 q",
        ] {
            assert!(seq.contains(want), "缺 {want:?}");
        }
        let copy = assert_restores(&src);
        assert_eq!(copy.kitty_keyboard_flags(), 1);
        assert!(copy.bracketed_paste());
    }

    #[test]
    fn fresh_screen_replays_no_modes() {
        let seq = String::from_utf8(Screen::new(10, 3).restore_sequence()).unwrap();
        assert!(!seq.contains("\x1b[>"));
        assert!(!seq.contains("2004"));
    }

    #[test]
    fn history_is_capped() {
        let mut bytes = Vec::new();
        for i in 0..(HISTORY_LINES + 50) {
            bytes.extend_from_slice(format!("{i}\r\n").as_bytes());
        }
        let src = screen_with(10, 3, &bytes);
        assert_eq!(src.history_text().len(), HISTORY_LINES);
    }

    #[test]
    fn long_history_is_compacted_and_restores_the_same() {
        // 彩色、宽字符、折行（含折行跨过压缩边界）的长历史。
        let mut bytes = Vec::new();
        for i in 0..(LIVE_HISTORY * 5 + 7) {
            let tail = "w".repeat(i % 25);
            bytes.extend_from_slice(
                format!(
                    "\x1b[3{}m{i}\x1b[0m 中文 \x1b[48;5;{}m{tail}\x1b[0m\r\n",
                    i % 7,
                    i % 256
                )
                .as_bytes(),
            );
        }
        bytes.extend_from_slice("\x1b[1;35mtail 末行\x1b[0m".as_bytes());
        let src = screen_with(16, 6, &bytes);
        assert!(src.term.history_size() < LIVE_HISTORY, "旧历史应已压缩");
        assert!(!src.archive.is_empty());
        let history = src.history_text();
        assert!(history.len() > LIVE_HISTORY * 5);
        assert_eq!(history[0], "0 中文");
        let copy = assert_restores(&src);
        assert_eq!(copy.history_text(), history);
    }

    #[test]
    fn compacted_history_keeps_the_cap() {
        let mut bytes = Vec::new();
        for i in 0..(HISTORY_LINES + 1000) {
            bytes.extend_from_slice(format!("\x1b[32m{i}\x1b[0m\r\n").as_bytes());
        }
        let src = screen_with(10, 3, &bytes);
        let history = src.history_text();
        assert_eq!(history.len(), HISTORY_LINES);
        assert_eq!(history[0], "998");
        let copy = assert_restores(&src);
        assert_eq!(copy.history_text(), history);
    }

    fn numbered(from: usize, to: usize) -> Vec<u8> {
        (from..to)
            .flat_map(|i| format!("line{i}\r\n").into_bytes())
            .collect()
    }

    fn assert_no_history(s: &Screen) {
        assert!(s.history_text().is_empty(), "{:?}", &s.history_text()[..3]);
        let seq = String::from_utf8_lossy(&s.restore_sequence()).into_owned();
        assert!(!seq.contains("line1\r"), "补发画面里还有清掉的历史");
    }

    #[test]
    fn clear_history_also_drops_compacted_rows() {
        // Claude Code 的 /clear、全量重绘都会发这一串。
        let mut s = screen_with(40, 5, &numbered(0, LIVE_HISTORY * 2 + 88));
        assert!(!s.archive.is_empty());
        s.feed(b"\x1b[2J\x1b[3J\x1b[H");
        assert_no_history(&s);
        assert_restores(&s);
    }

    #[test]
    fn clear_history_split_across_feeds() {
        let mut s = screen_with(40, 5, &numbered(0, LIVE_HISTORY * 2));
        s.feed(b"\x1b[3");
        s.feed(b"J");
        assert_no_history(&s);
    }

    #[test]
    fn rows_after_clear_in_the_same_chunk_are_kept() {
        // 3J 只清滚动历史，可见画面上的行留着；CLI 先 2J 清屏再 3J。
        let mut bytes = numbered(0, LIVE_HISTORY * 2);
        bytes.extend_from_slice(b"\x1b[2J\x1b[3J\x1b[H");
        bytes.extend(numbered(5000, 5000 + LIVE_HISTORY + 20));
        let s = screen_with(40, 5, &bytes);
        let history = s.history_text();
        assert_eq!(history[0], "line5000");
        assert_eq!(history.len(), LIVE_HISTORY + 20 - 4);
        assert_restores(&s);
    }

    #[test]
    fn clear_history_on_alt_screen_keeps_main_history() {
        let mut s = screen_with(40, 5, &numbered(0, LIVE_HISTORY * 2));
        let before = s.history_text();
        s.feed(b"\x1b[?1049h\x1b[3J\x1b[?1049l");
        assert_eq!(s.history_text(), before);
    }

    #[test]
    fn reset_drops_compacted_rows() {
        let mut s = screen_with(40, 5, &numbered(0, LIVE_HISTORY * 2));
        s.feed(b"\x1b[?1049h\x1bc");
        assert_no_history(&s);
    }

    #[test]
    fn bytes_apply_immediately_even_inside_synchronized_update() {
        // CLI 开了同步更新没关（比如中途崩了）：状态机照样跟上，清空历史也立即生效。
        let mut s = screen_with(40, 5, &numbered(0, LIVE_HISTORY * 2));
        s.feed(b"\x1b[?2026h\x1b[2J\x1b[3J\x1b[Hnow");
        assert_no_history(&s);
        assert_eq!(s.visible_text()[0], "now");
    }

    #[test]
    fn answers_status_queries() {
        let mut s = screen_with(10, 3, b"ab\x1b[6n");
        assert_eq!(s.take_replies(), b"\x1b[1;3R");
        assert!(s.take_replies().is_empty());
    }

    fn run(text: &str, fg: CellColor, bg: CellColor, attrs: u8) -> Run {
        Run {
            text: text.to_owned(),
            fg,
            bg,
            attrs,
        }
    }

    #[test]
    fn snapshot_keeps_colors_and_styles_per_run() {
        let s = screen_with(
            20,
            3,
            "\x1b[1;31mred\x1b[0m plain \x1b[38;2;1;2;3;48;5;200mrgb\x1b[0m\r\n\x1b[2;4;7m中x\x1b[0m   \r\n\x1b[44m  \x1b[0m".as_bytes(),
        );
        let snap = s.snapshot();
        assert_eq!((snap.cols, snap.rows), (20, 3));
        let d = CellColor::Default;
        assert_eq!(
            snap.lines,
            vec![
                vec![
                    run("red", CellColor::Indexed(1), d, attr::BOLD),
                    run(" plain ", d, d, 0),
                    run("rgb", CellColor::Rgb(1, 2, 3), CellColor::Indexed(200), 0),
                ],
                // 宽字符只出现一次；行尾的空格去掉。
                vec![run(
                    "中x",
                    d,
                    d,
                    attr::DIM | attr::UNDERLINE | attr::INVERSE
                )],
                // 有底色的空格不算空白。
                vec![run("  ", d, CellColor::Indexed(4), 0)],
            ]
        );
    }

    #[test]
    fn snapshot_shows_only_the_visible_screen() {
        let mut bytes = Vec::new();
        for i in 0..10 {
            bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        bytes.extend_from_slice(b"\x1b[?1049h\x1b[2;1Halt");
        let s = screen_with(10, 3, &bytes);
        let text: Vec<String> = s
            .snapshot()
            .lines
            .iter()
            .map(|l| l.iter().map(|r| r.text.as_str()).collect())
            .collect();
        assert_eq!(text, ["", "alt", ""]);
    }

    #[test]
    fn version_changes_on_output_and_resize_only() {
        let mut s = Screen::new(10, 3);
        let v0 = s.version();
        let _ = s.snapshot();
        assert_eq!(s.version(), v0, "取快照不改版本");
        s.feed(b"a");
        let v1 = s.version();
        assert_ne!(v1, v0);
        s.resize(12, 3);
        assert_ne!(s.version(), v1);
        assert_eq!(s.snapshot().version, s.version());
        // 新开的状态机（同一条对话重开）不会和旧的撞号。
        assert_ne!(Screen::new(10, 3).version(), s.version());
    }

    #[test]
    fn resize_keeps_content() {
        let mut s = screen_with(20, 5, b"hello");
        s.resize(30, 8);
        assert_eq!(s.size(), (30, 8));
        assert_eq!(s.visible_text()[0], "hello");
    }
}
