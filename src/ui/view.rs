//! 渲染：顶栏一行、左列表、右预览、底栏一行，帮助页和设置页浮在上面。
//! 不画面板边框，靠底色分区；选中行是淡底色加左侧强调色细条。

use std::collections::HashSet;
use std::time::SystemTime;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::core::{QuotaReading, QuotaValue, SessionState, Vendor};
use crate::host::{CellColor, ScreenSnapshot, attr};
use crate::index::{Entry, Snippet};
use crate::quota;
use crate::sources::Role;

use super::activity;
use super::app::{App, CliUpgrade, Overlay, Prompt, TERMINAL_CHOICES, VIEW_ONLY};
use super::keys::{self, HELP, MenuItem};
use super::newdir::{self, DirPicker};
use super::rows::{Group, GroupKey, Row};
use super::text::{ago, tilde, truncate, truncate_middle, width, wrap};
use super::theme::{Theme, UpdateButton, mix};

mod board;
mod bottom_bar;
mod list;
mod overlays;
mod preview;
mod top_bar;

use board::board;
use bottom_bar::bottom_bar;
use list::list;
use overlays::{cli_failures, dir_candidates, help, menu, settings};
use preview::{preview, preview_head};
use top_bar::top_bar;
pub use top_bar::{QUOTA_ALERT, QUOTA_WARN, reset_note, tokens};

/// 窄于这个宽度只显示列表，不显示预览。
const MIN_SPLIT_WIDTH: u16 = 80;
/// 一条预览消息最多显示几行。
const MESSAGE_LINES: usize = 8;
/// 转圈先画成这个占位字符（私用区，宽一格，不会出现在正文里），一帧画完再换成此刻那一帧、记下位置：
/// 只有转圈换帧时运行层只改这几格，不整帧重画（见 [`App::areas`] 的 `spin`）。
pub const SPIN_MARK: char = '\u{E000}';

struct Areas {
    top: Rect,
    /// 列表的表头（项目 / 状态 / 后台 / 对话 / 最近）。
    list_head: Rect,
    list: Rect,
    /// 预览区的标题行（这块在显示什么）。
    preview_head: Option<Rect>,
    preview: Option<Rect>,
    bottom: Rect,
}

fn split(area: Rect) -> Areas {
    let top = Rect { height: 1, ..area };
    let bottom = Rect {
        y: area.bottom().saturating_sub(1),
        height: 1,
        ..area
    };
    // 顶栏下面一行是表头带，再下面是列表和预览。
    let head = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(2).min(1),
        ..area
    };
    let body = Rect {
        y: area.y + 2,
        height: area.height.saturating_sub(3),
        ..area
    };
    if area.width < MIN_SPLIT_WIDTH {
        return Areas {
            top,
            list_head: head,
            list: body,
            preview_head: None,
            preview: None,
            bottom,
        };
    }
    // 列表占八分之五（项目看得全），预览八分之三：再窄实时画面里 CLI 的输出就截得看不清了；
    // 要看全就回车进那条对话。特别宽的终端列表最多 120 列，多的给预览。
    let list_w = (area.width * 5 / 8).clamp(36, 120);
    let left = |r: Rect| Rect { width: list_w, ..r };
    let right = |r: Rect| Rect {
        x: r.x + list_w,
        width: r.width - list_w,
        ..r
    };
    Areas {
        top,
        list_head: left(head),
        list: left(body),
        preview_head: Some(right(head)),
        preview: Some(right(body)),
        bottom,
    }
}

/// 画一帧。顺带记下各区域（鼠标定位用）并把列表滚到选中行可见。
pub fn draw(frame: &mut Frame, app: &mut App, theme: &Theme) {
    let area = frame.area();
    let a = split(area);
    app.areas.list = a.list;
    app.areas.preview = a.preview.unwrap_or_default();
    keep_visible(app, a.list.height);

    let buf = frame.buffer_mut();
    let accent = theme.accent(app.vendor);
    top_bar(buf, a.top, app, theme);
    let now = app.now;
    if let Some(r) = &mut app.reader {
        // 整段对话盖住列表和预览两栏（表头带当它的标题带）；列表点不到。
        let body = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(2),
            ..area
        };
        r.draw(buf, body, now, theme);
        app.areas.list = Rect::default();
        app.areas.preview = Rect::default();
    } else {
        list(buf, a.list_head, a.list, app, theme, accent);
        if let (Some(h), Some(p)) = (a.preview_head, a.preview) {
            preview_head(buf, h, app, theme);
            preview(buf, p, app, theme, accent);
        }
    }
    bottom_bar(buf, a.bottom, app, theme, accent);
    if let Some(Prompt::NewDir { picker, .. }) = &app.prompt {
        dir_candidates(buf, a.list, picker, theme, accent);
    }
    app.areas.menu = Rect::default();
    match app.overlay {
        Overlay::Help => help(buf, area, theme, accent),
        Overlay::Settings { cursor } => settings(buf, area, app, cursor, theme, accent),
        Overlay::Balance { scroll } => super::balance::draw(buf, area, app, scroll, theme),
        Overlay::CliFailures { scroll } => cli_failures(buf, area, app, scroll, theme),
        Overlay::Menu { sel, x, y, cursor } => {
            let items = app.menu_items(sel);
            app.areas.menu = menu(buf, area, &items, (x, y), cursor, theme, accent);
        }
        Overlay::None => {}
    }
    app.areas.spin = place_spinner(buf, app.spinner());
}

/// 把占位的 [`SPIN_MARK`] 换成转圈此刻那一帧，返回它们的位置。
fn place_spinner(buf: &mut Buffer, frame: char) -> Vec<Position> {
    let mut mark = [0u8; 4];
    let mark: &str = SPIN_MARK.encode_utf8(&mut mark);
    let area = buf.area;
    let mut out = Vec::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(c) = buf.cell_mut((x, y))
                && c.symbol() == mark
            {
                c.set_char(frame);
                out.push(Position { x, y });
            }
        }
    }
    out
}

/// 只有转圈换了一帧：把上一帧的那几格换成 `frame`（其余不动）。
pub fn respin(buf: &mut Buffer, cells: &[Position], frame: char) {
    for &p in cells {
        if let Some(c) = buf.cell_mut(p) {
            c.set_char(frame);
        }
    }
}

fn keep_visible(app: &mut App, height: u16) {
    let h = usize::from(height.max(1));
    // 有动态的对话占两行：选中的那条整条放得下为止往下挪。
    let heights = app.line_heights();
    let fix = |sel: usize, off: &mut usize| {
        if sel < *off {
            *off = sel;
            return;
        }
        let lines = |off: usize| {
            heights
                .get(off..=sel)
                .map_or(0, |r| r.iter().sum::<usize>())
        };
        while *off < sel && lines(*off) > h {
            *off += 1;
        }
    };
    if app.board.is_some() {
        let entries = app.board_entries();
        let (len, sel) = (entries.len(), app.board_index(&entries).unwrap_or(0));
        if let Some(b) = &mut app.board {
            // 记下这一帧选中的位置：选中的那条下一刻不在了，就停在用户最后看到它的地方。
            b.selected = sel;
            b.offset = b.offset.min(len.saturating_sub(h));
            fix(sel, &mut b.offset);
        }
        return;
    }
    match &mut app.search {
        Some(s) if !s.query.is_empty() => fix(s.selected, &mut s.offset),
        _ => {
            let v = app.vendor;
            let l = &mut app.lists[v.index()];
            let len = l.rows.len();
            l.offset = l.offset.min(len.saturating_sub(h));
            fix(l.selected, &mut l.offset);
            // 第一行会被钉住的组标题盖住：选中的正好在那一行就往上让一行。
            if h >= 2 && l.selected == l.offset && l.sticky().is_some() {
                l.offset -= 1;
            }
        }
    }
}

// ---- 基础 ----

pub(super) fn fill(buf: &mut Buffer, area: Rect, bg: Color) {
    buf.set_style(area, Style::default().bg(bg));
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.set_symbol(" ");
            }
        }
    }
}

fn line_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| width(&s.content)).sum()
}

/// 在一行里左边放 `left`、右边贴边放 `right`；放不下时先截左边。
pub(super) fn row(
    buf: &mut Buffer,
    area: Rect,
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
) {
    let w = usize::from(area.width);
    let rw = line_width(&right).min(w);
    let left = clip(left, w.saturating_sub(rw));
    Line::from(left).render_into(buf, area);
    if rw > 0 {
        let x = area.x + (w - rw) as u16;
        let r = Rect {
            x,
            width: rw as u16,
            ..area
        };
        Line::from(right).render_into(buf, r);
    }
}

/// 截掉超出 `max` 格的部分（最后一段带省略号）。
pub(super) fn clip(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for s in spans {
        let w = width(&s.content);
        if used + w <= max {
            used += w;
            out.push(s);
            continue;
        }
        let room = max - used;
        if room > 0 {
            out.push(Span::styled(truncate(&s.content, room), s.style));
        }
        break;
    }
    out
}

trait RenderInto {
    fn render_into(self, buf: &mut Buffer, area: Rect);
}

impl RenderInto for Line<'_> {
    fn render_into(self, buf: &mut Buffer, area: Rect) {
        ratatui::widgets::Widget::render(self, area, buf);
    }
}

/// 「上下文 72%」：用到 80% 起是提醒的黄、95% 起红（和额度一个门槛），没到用 `normal`。
pub(super) fn context_span(c: quota::ContextUse, t: &Theme, normal: Color) -> Span<'static> {
    let color = match c.percent {
        u if u >= QUOTA_ALERT => t.alert(),
        u if u >= QUOTA_WARN => t.state(SessionState::Waiting),
        _ => normal,
    };
    sp(format!("上下文 {:.0}%", c.percent), fg(color))
}

pub(super) fn fg(c: Color) -> Style {
    Style::default().fg(c)
}

pub(super) fn bold(c: Color) -> Style {
    fg(c).add_modifier(Modifier::BOLD)
}

pub(super) fn sp(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

/// 行底色：选中是淡底加左边一道强调色；刚停下来的（`flash`）整行染上那个状态的颜色，随时间淡回去。
pub(super) fn row_bg(
    buf: &mut Buffer,
    area: Rect,
    selected: bool,
    flash: Option<(f32, SessionState)>,
    t: &Theme,
    accent: Color,
) {
    // 窗口很小时这一行可能整个在屏幕外，`set_string` 越界会 panic。
    let area = area.intersection(buf.area);
    if area.is_empty() || !(selected || flash.is_some()) {
        return;
    }
    let base = if selected { t.select } else { t.base };
    // 先快后慢地淡：刚停那一下最亮，尾巴拖长一点。
    let bg = flash.map_or(base, |(left, s)| mix(base, t.state(s), 0.35 * left * left));
    fill(buf, area, bg);
    if selected {
        buf.set_string(area.x, area.y, "▎", fg(accent).bg(bg));
    }
}
pub(super) fn put_lines(buf: &mut Buffer, area: Rect, lines: Vec<Line<'static>>) {
    for (i, l) in lines.into_iter().take(usize::from(area.height)).enumerate() {
        let r = Rect {
            y: area.y + i as u16,
            height: 1,
            ..area
        };
        l.render_into(buf, r);
    }
}
pub(super) fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}
#[cfg(test)]
mod tests {
    use super::list::snippet_spans;
    use super::top_bar::{TabForm, with_tab_forms};
    use super::{Snippet, Style, clip, width};
    use ratatui::style::Modifier;

    fn hit() -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    /// 排成一行、截到 `room` 后的字和高亮的那几段。
    fn lay(s: &Snippet, room: usize) -> (String, Vec<String>) {
        let spans = clip(snippet_spans(s, room, Style::default(), hit()), room);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        let hits = spans
            .iter()
            .filter(|s| s.style == hit())
            .map(|s| s.content.to_string())
            .collect();
        (text, hits)
    }

    fn snip(text: &str, term: &str) -> Snippet {
        let marks = text
            .match_indices(term)
            .map(|(i, m)| (i, i + m.len()))
            .collect();
        Snippet {
            text: text.to_owned(),
            marks,
        }
    }

    #[test]
    fn snippet_short_lead_kept_whole() {
        let s = snip("顺便把购物车结算也看了", "结算");
        let (text, hits) = lay(&s, 60);
        assert_eq!(text, "顺便把购物车结算也看了");
        assert_eq!(hits, ["结算"]);
    }

    #[test]
    fn snippet_long_lead_is_cut_so_the_hit_shows_in_narrow_rows() {
        let s = snip(
            &format!("…{}结算按钮{}", "前面的字".repeat(10), "后面".repeat(50)),
            "结算",
        );
        for room in [1, 2, 3, 6, 10, 15, 24, 40, 80] {
            let (text, hits) = lay(&s, room);
            assert!(width(&text) <= room, "{room} 格：{text}");
            if room >= 6 {
                assert!(text.starts_with('…'), "{room} 格：{text}");
                assert_eq!(hits, ["结算"], "{room} 格要看得到命中词：{text}");
                let lead = text.find("结算").unwrap();
                assert!(width(&text[..lead]) <= room / 3, "{room} 格：{text}");
            }
        }
    }

    #[test]
    fn snippet_without_marks_still_lays_out() {
        let s = Snippet {
            text: "x".repeat(300),
            marks: Vec::new(),
        };
        let (text, hits) = lay(&s, 20);
        assert_eq!(width(&text), 20);
        assert!(hits.is_empty());
    }

    #[test]
    fn tabs_keep_short_names_until_quota_details_are_gone() {
        use TabForm::{Full, Marks, Short};
        // 少于五家：每种写法都用全名，最后再写短名、缩成标记。
        assert_eq!(
            with_tab_forms(vec![1, 2, 3], 4, 1),
            [(1, Full), (2, Full), (3, Full), (3, Short), (3, Marks)]
        );
        // 五家以上：最全的写法先试全名，放不下别家写短名，一直缩到 plain 那种（这里是第 2 种），
        // 还放不下才缩成标记，再从最全的写法试起。
        assert_eq!(
            with_tab_forms(vec![1, 2, 3], 5, 1),
            [
                (1, Full),
                (1, Short),
                (2, Short),
                (1, Marks),
                (2, Marks),
                (3, Marks)
            ]
        );
        assert!(with_tab_forms(Vec::<u8>::new(), 5, 0).is_empty());
    }
}
