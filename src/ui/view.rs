//! 渲染：顶栏一行、左列表、右预览、底栏一行，帮助页和设置页浮在上面。
//! 不画面板边框，靠底色分区；选中行是淡底色加左侧强调色细条。

use std::collections::HashSet;
use std::time::SystemTime;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::core::{QuotaReading, QuotaValue, SessionState, Vendor};
use crate::host::{CellColor, ScreenSnapshot, attr};
use crate::index::Entry;
use crate::quota;
use crate::sources::Role;

use super::app::{App, Overlay, Prompt, TERMINAL_CHOICES};
use super::keys::{self, HELP, Selected};
use super::newdir::{self, DirPicker};
use super::rows::{Group, GroupKey, Row};
use super::text::{ago, tilde, truncate, truncate_middle, width, wrap};
use super::theme::{Theme, mix};

/// 窄于这个宽度只显示列表，不显示预览。
const MIN_SPLIT_WIDTH: u16 = 80;
/// 一条预览消息最多显示几行。
const MESSAGE_LINES: usize = 8;

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
        Overlay::Menu { sel, x, y, cursor } => {
            app.areas.menu = menu(buf, area, sel, (x, y), cursor, theme, accent);
        }
        Overlay::None => {}
    }
}

fn keep_visible(app: &mut App, height: u16) {
    let h = usize::from(height.max(1));
    let fix = |sel: usize, off: &mut usize| {
        if sel < *off {
            *off = sel;
        } else if sel >= *off + h {
            *off = sel + 1 - h;
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

pub(super) fn fg(c: Color) -> Style {
    Style::default().fg(c)
}

pub(super) fn bold(c: Color) -> Style {
    fg(c).add_modifier(Modifier::BOLD)
}

pub(super) fn sp(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

// ---- 顶栏 ----

/// 顶栏印章后的全局状态写成什么样（窄了一步步缩）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusForm {
    /// `◐3 在跑  ◆1 等你  ●2 别处`
    Full,
    /// `◐3 ◆1 ●2`
    Counts,
    /// `◐ ◆ ●`
    Dots,
    Hidden,
}

/// 额度留哪些：「额度」两字、第二个窗口、来源和更新时间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QuotaCut {
    label: bool,
    second: bool,
    source: bool,
}

/// 顶栏的写法，从全到简。窄终端依次去掉：额度来源 → 第二个额度窗口 → 状态数字只留点 → 「额度」两字，
/// 再放不下额度让位（后台是旧版、有新版的提示比额度优先），再放不下先去掉「别处」、留着「在跑 / 等你」，最后状态也不显示。
/// 每项是（状态写法，额度留哪些，带不带提示，带不带「别处」）。
fn top_layouts(hint: bool) -> Vec<(StatusForm, Option<QuotaCut>, bool, bool)> {
    let cut = |label, second, source| {
        Some(QuotaCut {
            label,
            second,
            source,
        })
    };
    let mut out: Vec<_> = [
        (StatusForm::Full, cut(true, true, true)),
        (StatusForm::Full, cut(true, true, false)),
        (StatusForm::Full, cut(true, false, false)),
        (StatusForm::Counts, cut(true, false, false)),
        (StatusForm::Dots, cut(true, false, false)),
        (StatusForm::Dots, cut(false, false, false)),
    ]
    .into_iter()
    .map(|(s, q)| (s, q, hint, true))
    .collect();
    if hint {
        out.push((StatusForm::Dots, None, true, true));
    }
    out.push((StatusForm::Dots, None, false, true));
    // 「别处」是自己托管的之外多出来的，挤不下先让它走，「等你」那个点（单击跳过去）留到最后。
    out.push((StatusForm::Dots, None, false, false));
    out.push((StatusForm::Hidden, None, false, false));
    out
}

/// 标签几时缩成标记：装了五家以上时，放不下先缩标签（当前这家留全名），再按 `layouts` 依次去掉别的；
/// 少于五家时标签一直写全名，别的都去掉了还放不下才缩。
fn with_tab_forms<L: Copy>(layouts: Vec<L>, tabs: usize) -> Vec<(L, bool)> {
    const MANY_TABS: usize = 5;
    let Some(&last) = layouts.last() else {
        return Vec::new();
    };
    let mut out: Vec<(L, bool)> = Vec::new();
    for (i, l) in layouts.into_iter().enumerate() {
        if tabs >= MANY_TABS {
            if i == 0 {
                out.push((l, false));
            }
            out.push((l, true));
        } else {
            out.push((l, false));
        }
    }
    if tabs < MANY_TABS {
        out.push((last, true));
    }
    out
}

/// 全局状态，分两段（各自单击做的事不同）：会话台自己托管的在跑（绿）、等你回复（黄，慢闪的暗半拍换暗色），
/// 和别处在用（柔绿；有正在干活的就转圈）。都没有就是空的。
fn status_spans(
    app: &App,
    form: StatusForm,
    t: &Theme,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let (open, busy) = app.elsewhere_counts();
    let dot = if busy > 0 { app.spinner() } else { '●' };
    let elsewhere = chips(&[(open, dot, "别处", t.elsewhere())], form);
    (own_status(app, form, t), elsewhere)
}

/// 自己托管的：在跑、等你回复。
fn own_status(app: &App, form: StatusForm, t: &Theme) -> Vec<Span<'static>> {
    let (running, waiting) = app.global_counts();
    let wait_color = if app.blink_dim {
        t.faint
    } else {
        t.state(SessionState::Waiting)
    };
    let parts = [
        (
            running,
            app.spinner(),
            "在跑",
            t.state(SessionState::Running),
        ),
        (waiting, '◆', "等你", wait_color),
    ];
    chips(&parts, form)
}

/// 「◐3 在跑」这样一个个排开，按 `form` 缩写；数是 0 的不写。
fn chips(parts: &[(usize, char, &str, Color)], form: StatusForm) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for &(n, dot, word, color) in parts.iter().filter(|p| p.0 > 0) {
        let (text, gap) = match form {
            StatusForm::Full => (format!("{dot}{n} {word}"), "  "),
            StatusForm::Counts => (format!("{dot}{n}"), " "),
            StatusForm::Dots => (dot.to_string(), " "),
            StatusForm::Hidden => return Vec::new(),
        };
        if !out.is_empty() {
            out.push(sp(gap, Style::default()));
        }
        out.push(sp(text, fg(color)));
    }
    out
}

fn top_bar(buf: &mut Buffer, area: Rect, app: &mut App, t: &Theme) {
    fill(buf, area, t.bar);
    app.areas.tabs.clear();
    app.areas.status = Rect::default();
    app.areas.elsewhere = Rect::default();
    let accent = t.accent(app.vendor);
    let seal = " 会话台 ";
    // 缩写的标签：别的家只写标记字符，当前这家写全名。
    let tabs_of = |compact: bool| -> Vec<(Vendor, String)> {
        app.shown
            .iter()
            .map(|&v| {
                let label = if compact && v != app.vendor {
                    v.mark().to_string()
                } else {
                    format!("{} {}", v.mark(), v.name())
                };
                (v, label)
            })
            .collect()
    };
    // 标签之间隔一道淡竖线（两边各一格空）。
    const TAB_DIVIDER: &str = " │ ";
    let tabs_width = |tabs: &[(Vendor, String)]| {
        tabs.iter().map(|(_, l)| width(l)).sum::<usize>()
            + width(TAB_DIVIDER) * tabs.len().saturating_sub(1)
    };
    let full_w = tabs_width(&tabs_of(false));
    let compact_w = tabs_width(&tabs_of(true));
    // 后台是旧版的提示比新版提醒要紧，两条只放一条。
    let hint = app
        .old_host
        .as_ref()
        .map(HashSet::len)
        .filter(|&n| n > 0)
        .map(|n| format!("后台是旧版，{n} 条对话结束后自动换新"))
        .or_else(|| {
            app.new_version
                .as_ref()
                .filter(|_| app.config.check_updates)
                .map(|v| format!("有新版 {v} · ai update"))
        });
    let gap = "   ";
    let total = usize::from(area.width);
    // 两段状态之间隔两格，和段里各项之间一样。
    let status_w = |(own, other): &(Vec<Span>, Vec<Span>)| {
        let (a, b) = (line_width(own), line_width(other));
        a + b + if a > 0 && b > 0 { 2 } else { 0 }
    };
    let left_w = |status: &(Vec<Span>, Vec<Span>), compact: bool| {
        let sw = status_w(status);
        let tabs_w = if compact { compact_w } else { full_w };
        1 + width(seal) + if sw > 0 { 1 + sw } else { 0 } + gap.len() + tabs_w
    };
    // 从全到简挑第一种放得下的；连最简的都放不下（很窄）就只有印章和标签。
    let (status, right, compact) = with_tab_forms(top_layouts(hint.is_some()), app.shown.len())
        .into_iter()
        .map(|((form, cut, with_hint, with_elsewhere), compact)| {
            let mut status = status_spans(app, form, t);
            if !with_elsewhere {
                status.1.clear();
            }
            let mut right = cut
                .filter(|_| quota::supported(app.vendor))
                .map_or_else(Vec::new, |c| {
                    quota_spans(app.quota.get(&app.vendor), app.now, app.vendor, t, c)
                });
            if let Some(h) = hint.as_ref().filter(|_| with_hint) {
                let lead = if right.is_empty() { "" } else { gap };
                right.splice(
                    0..0,
                    [sp(h.clone(), fg(t.faint)), sp(lead, Style::default())],
                );
            }
            (status, right, compact)
        })
        .find(|(status, right, compact)| {
            let rw = line_width(right);
            left_w(status, *compact) + if rw > 0 { 2 + rw } else { 0 } < total
        })
        .unwrap_or_else(|| (Default::default(), Vec::new(), true));
    let tabs = tabs_of(compact);

    // 印章：反白，底色是当前这家的强调色，字是顶栏底色。整个界面唯一的大胆元素。
    let mut x = area.x + 1;
    let seal_style = Style::default()
        .bg(accent)
        .fg(t.bar)
        .add_modifier(Modifier::BOLD);
    buf.set_string(x, area.y, seal, seal_style);
    x += width(seal) as u16;
    let (own, other) = status;
    let mut lead = 1;
    for (spans, own_part) in [(own, true), (other, false)] {
        let w = line_width(&spans) as u16;
        if w == 0 {
            continue;
        }
        let r = Rect {
            x: x + lead,
            width: w,
            ..area
        };
        Line::from(spans).render_into(buf, r);
        if own_part {
            app.areas.status = r;
        } else {
            app.areas.elsewhere = r;
        }
        x = r.right();
        lead = 2;
    }
    x += gap.len() as u16;
    // 标签照 gitui、ratatui Tabs 的做法：不垫底色，标签之间一道淡竖线，看得出是一组能切换的标签；
    // 当前这家强调色加粗加下划线（和底栏能点的路径一样用下划线），别的家次要色。底色只给印章。
    for (i, (v, label)) in tabs.into_iter().enumerate() {
        let w = width(&label) as u16;
        let lead = if i == 0 { 0 } else { width(TAB_DIVIDER) as u16 };
        if x + lead + w > area.right() {
            break;
        }
        if lead > 0 {
            buf.set_string(x, area.y, TAB_DIVIDER, fg(t.faint));
            x += lead;
        }
        let style = if v == app.vendor {
            bold(accent).add_modifier(Modifier::UNDERLINED)
        } else {
            fg(t.muted)
        };
        buf.set_string(x, area.y, &label, style);
        app.areas.tabs.push((
            v,
            Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            },
        ));
        x += w;
    }
    let w = line_width(&right);
    if w > 0 {
        let r = Rect {
            x: area.right() - 1 - w as u16,
            width: w as u16,
            ..area
        };
        Line::from(right).render_into(buf, r);
    }
}

/// 额度用到这么多（百分比）变黄。
pub const QUOTA_WARN: f32 = 80.0;
/// 用到这么多变红。
pub const QUOTA_ALERT: f32 = 95.0;

/// 额度：「额度」两字，每个窗口一条细进度条加百分比，后面是来源和更新时间；`cut` 定留哪些。
fn quota_spans(
    reading: Option<&QuotaReading>,
    now: SystemTime,
    vendor: Vendor,
    t: &Theme,
    cut: QuotaCut,
) -> Vec<Span<'static>> {
    let Some(r) = reading else {
        return vec![sp("额度读取中…", fg(t.faint))];
    };
    let mut out = match &r.value {
        QuotaValue::Unavailable => return vec![sp("额度暂时取不到", fg(t.faint))],
        QuotaValue::TokensToday(n) => vec![
            sp("今天 ", fg(t.muted)),
            sp(tokens(*n), fg(t.text)),
            sp(" token", fg(t.muted)),
        ],
        QuotaValue::Windows(ws) => {
            let one = |w: &crate::core::QuotaWindow| {
                let used = w.used_percent.clamp(0.0, 100.0);
                let filled = (used / 100.0 * 8.0).round() as usize;
                // 80% 起变黄、95% 起变红，百分比跟着变色；没到就是这家的强调色。
                let (bar_color, pct_color) = match used {
                    u if u >= QUOTA_ALERT => (t.alert(), t.alert()),
                    u if u >= QUOTA_WARN => {
                        let c = t.state(SessionState::Waiting);
                        (c, c)
                    }
                    _ => (t.accent(vendor), t.text),
                };
                vec![
                    sp(format!("{} ", w.label), fg(t.muted)),
                    sp("━".repeat(filled), fg(bar_color)),
                    sp("━".repeat(8 - filled), fg(t.select)),
                    sp(format!(" {used:.0}%"), fg(pct_color)),
                ]
            };
            let n = if cut.second { 2 } else { 1 };
            let mut out = Vec::new();
            for (i, w) in ws.iter().take(n).enumerate() {
                if i > 0 {
                    out.push(sp("   ", Style::default()));
                }
                out.extend(one(w));
            }
            out
        }
    };
    if cut.source {
        out.push(sp(
            format!("  {} · {}", r.source, ago(r.at, now)),
            fg(t.faint),
        ));
    }
    if cut.label {
        out.insert(0, sp("额度  ", fg(t.muted)));
    }
    out
}

/// token 数：万、亿。
pub fn tokens(n: u64) -> String {
    match n {
        0..10_000 => n.to_string(),
        10_000..100_000_000 => format!("{:.1} 万", n as f64 / 1e4),
        _ => format!("{:.2} 亿", n as f64 / 1e8),
    }
}

// ---- 列表 ----

fn list(buf: &mut Buffer, head: Rect, area: Rect, app: &App, t: &Theme, accent: Color) {
    fill(buf, area, t.base);
    fill(buf, head, t.bar);
    if area.height == 0 {
        return;
    }
    let line = |i: u16| Rect {
        y: area.y + i,
        height: 1,
        ..area
    };
    if let Some(b) = &app.board {
        board(buf, head, area, app, b.offset, t, accent);
        return;
    }
    if let Some(s) = app.search.as_ref().filter(|s| !s.query.is_empty()) {
        if s.hits.is_empty() {
            list_head(buf, head, "搜索结果", &Table::new(&[], 0), t);
            row(
                buf,
                line(1),
                vec![sp(format!("  没找到「{}」", s.query), fg(t.muted))],
                vec![],
            );
            row(
                buf,
                line(2),
                vec![sp(
                    "  标题和正文都搜过了，正文索引刚启动时还在后台补",
                    fg(t.faint),
                )],
                vec![],
            );
            return;
        }
        let cells: Vec<Cells> = s
            .hits
            .iter()
            .map(|e| item_cells(app, e, t, accent))
            .collect();
        let table = Table::new(&cells, usize::from(area.width));
        list_head(buf, head, "搜索结果", &table, t);
        for (i, (e, c)) in s
            .hits
            .iter()
            .zip(cells)
            .enumerate()
            .skip(s.offset)
            .take(usize::from(area.height))
        {
            let r = line((i - s.offset) as u16);
            item(
                buf,
                r,
                app,
                e,
                Tag::Project,
                i == s.selected,
                &table,
                c,
                t,
                accent,
            );
        }
        return;
    }
    let l = app.list();
    if l.rows.is_empty() {
        list_head(buf, head, "项目", &Table::new(&[], 0), t);
        let name = app.vendor.name();
        row(
            buf,
            line(1),
            vec![sp(format!("  还没有 {name} 对话"), fg(t.muted))],
            vec![],
        );
        row(
            buf,
            line(2),
            vec![
                sp("  按 ", fg(t.faint)),
                sp("n", bold(t.title)),
                sp(
                    format!(" 在 {} 新建一条", tilde(&app.launch_dir, &app.home)),
                    fg(t.faint),
                ),
            ],
            vec![],
        );
        return;
    }
    let cells: Vec<Cells> = l
        .rows
        .iter()
        .map(|r| match *r {
            Row::Header(g) => group_cells(app, &l.groups[g], t),
            Row::Item { entry, .. } => item_cells(app, &l.entries[entry], t, accent),
        })
        .collect();
    let table = Table::new(&cells, usize::from(area.width));
    list_head(buf, head, "项目", &table, t);
    for (i, (r, c)) in l
        .rows
        .iter()
        .zip(cells)
        .enumerate()
        .skip(l.offset)
        .take(usize::from(area.height))
    {
        let rect = line((i - l.offset) as u16);
        let selected = i == l.selected;
        match *r {
            Row::Header(g) => header(buf, rect, app, g, selected, &table, c, t, accent),
            Row::Item { group, entry } => {
                let tag = if l.groups[group].mixed() {
                    Tag::Project
                } else {
                    Tag::None
                };
                item(
                    buf,
                    rect,
                    app,
                    &l.entries[entry],
                    tag,
                    selected,
                    &table,
                    c,
                    t,
                    accent,
                );
            }
        }
    }
    if let Some(h) = l.sticky().filter(|_| area.height >= 2)
        && let Row::Header(g) = l.rows[h]
    {
        let c = group_cells(app, &l.groups[g], t);
        header(buf, line(0), app, g, h == l.selected, &table, c, t, accent);
    }
}

/// 「等你回复」汇总（`s`）：三家的对话混在一行行里，每行标题前是那家的标记，后面是项目名。
fn board(
    buf: &mut Buffer,
    head: Rect,
    area: Rect,
    app: &App,
    offset: usize,
    t: &Theme,
    accent: Color,
) {
    const LABEL: &str = "等你回复和在跑的 · 各家";
    let line = |i: u16| Rect {
        y: area.y + i,
        height: 1,
        ..area
    };
    let entries = app.board_entries();
    if entries.is_empty() {
        list_head(buf, head, LABEL, &Table::new(&[], 0), t);
        row(
            buf,
            line(1),
            vec![sp("  没有等你回复或在跑的对话", fg(t.muted))],
            vec![],
        );
        row(
            buf,
            line(2),
            vec![sp(
                "  会话台托管的对话停下来要你回复时，会列在这里",
                fg(t.faint),
            )],
            vec![],
        );
        return;
    }
    let selected = app.board_index(&entries).unwrap_or(0);
    let cells: Vec<Cells> = entries
        .iter()
        .map(|e| item_cells(app, e, t, accent))
        .collect();
    let table = Table::new(&cells, usize::from(area.width));
    list_head(buf, head, LABEL, &table, t);
    for (i, (e, c)) in entries
        .iter()
        .zip(cells)
        .enumerate()
        .skip(offset)
        .take(usize::from(area.height))
    {
        let r = line((i - offset) as u16);
        item(
            buf,
            r,
            app,
            e,
            Tag::VendorAndProject,
            i == selected,
            &table,
            c,
            t,
            accent,
        );
    }
}

/// 「⏎ 确定 · 留空恢复原名 · Esc 取消」这类按键说明：按键用强调色加粗（和底栏、帮助一样，看得出能按），
/// 说明用次要色；不以按键开头的一段（「留空恢复原名」）整段用暗色。
fn key_hints(text: &str, t: &Theme, accent: Color) -> Vec<Span<'static>> {
    let is_key = |k: &str| {
        matches!(k, "⏎" | "Esc" | "Tab" | "⇧Tab" | "↑↓" | "→" | "!")
            || (k.len() == 1 && k.bytes().all(|b| b.is_ascii_alphabetic()))
    };
    let mut out = Vec::new();
    for (i, seg) in text.split(" · ").enumerate() {
        if i > 0 {
            out.push(sp(" · ", fg(t.faint)));
        }
        match seg.split_once(' ') {
            Some((k, rest)) if is_key(k) => {
                out.push(sp(k.to_owned(), bold(accent)));
                out.push(sp(format!(" {rest}"), fg(t.muted)));
            }
            _ => out.push(sp(seg.to_owned(), fg(t.faint))),
        }
    }
    out
}

// ---- 列表的列 ----

/// 列表右边的几列，从左到右：状态（收起的组里在跑、等你、别处在用；对话没看过的小点）、
/// 后台（藏起来的后台对话在跑几条）、对话（组里几条）、最近（多久前聊过）。
const COLS: [&str; 4] = ["状态", "后台", "对话", "最近"];
/// 窄的时候按这个顺序省列：先省条数，再省状态，再省后台；「最近」一直在。
const DROP: [usize; 3] = [2, 0, 1];
/// 列和列之间、列和名字之间隔几格。
const GAP: usize = 2;
/// 左边至少留给名字这么多格，不够就省列。
const MIN_NAME: usize = 14;

/// 一行里四列各写什么（没有就是空的）。
type Cells = [Vec<Span<'static>>; 4];

/// 列宽：每列取表头字和所有行里最宽的那个，所以上下对得齐；所有行都空着的列不画。
struct Table {
    widths: [usize; 4],
    shown: [bool; 4],
}

impl Table {
    fn new(rows: &[Cells], row_width: usize) -> Table {
        let mut widths = COLS.map(width);
        let mut shown = [false; 4];
        for r in rows {
            for (i, c) in r.iter().enumerate() {
                let w = line_width(c);
                if w > 0 {
                    shown[i] = true;
                    widths[i] = widths[i].max(w);
                }
            }
        }
        let mut table = Table { widths, shown };
        for i in DROP {
            if row_width.saturating_sub(table.used() + 1) >= MIN_NAME {
                break;
            }
            table.shown[i] = false;
        }
        table
    }

    /// 各列连同前面的间隔一共占几格。
    fn used(&self) -> usize {
        (0..4)
            .filter(|&i| self.shown[i])
            .map(|i| self.widths[i] + GAP)
            .sum()
    }

    /// 把各列靠右对齐画在这一行右边（最右留一格），返回左边留给名字的宽度（从行首算）。
    fn render(&self, buf: &mut Buffer, area: Rect, cells: Cells) -> usize {
        let mut end = usize::from(area.width).saturating_sub(1);
        for (i, c) in cells.into_iter().enumerate().rev() {
            if !self.shown[i] {
                continue;
            }
            let w = self.widths[i];
            let cw = line_width(&c).min(w).min(end);
            if cw > 0 {
                let r = Rect {
                    x: area.x + (end - cw) as u16,
                    width: cw as u16,
                    ..area
                };
                Line::from(clip(c, cw)).render_into(buf, r);
            }
            end = end.saturating_sub(w + GAP);
        }
        end
    }
}

/// 表头带：左边写这一栏是什么，右边各列的名字对着下面的列。
fn list_head(buf: &mut Buffer, area: Rect, label: &str, table: &Table, t: &Theme) {
    if area.height == 0 {
        return;
    }
    let labels: Cells = COLS.map(|c| vec![sp(c, fg(t.muted))]);
    let room = table.render(buf, area, labels);
    // 和组名对齐（组名前面是序号和箭头，共 4 格）。
    let r = Rect {
        x: area.x + 4,
        width: (room.saturating_sub(4) as u16).min(area.width.saturating_sub(4)),
        ..area
    };
    Line::from(sp(label.to_owned(), fg(t.muted))).render_into(buf, r);
}

/// 组标题那一行的四列。收起时看得出组里的状态：自己的对话在跑几条（转圈）、有没有等你回复的（◆）、
/// 别处在用几条（柔绿点）。藏起来的后台对话有在跑的写在「后台」列：转圈加条数。
/// 条数总写；最近时间只在收起时写（展开着第一条对话就写着时间）。
fn group_cells(app: &App, g: &Group, t: &Theme) -> Cells {
    let l = app.list();
    let collapsed = app.folds.is_collapsed(&g.key);
    let mut status = Vec::new();
    let mut add = |text: String, color: Color| {
        if !status.is_empty() {
            status.push(sp(" ", Style::default()));
        }
        status.push(sp(text, fg(color)));
    };
    if collapsed {
        let count = |s: SessionState| {
            g.items
                .iter()
                .filter(|&&i| app.state_of(&l.entries[i].session.id) == s)
                .count()
        };
        let (running, waiting) = (count(SessionState::Running), count(SessionState::Waiting));
        if running > 0 {
            add(
                format!("{} {running}", app.spinner()),
                t.state(SessionState::Running),
            );
        }
        if waiting > 0 {
            add("◆".to_owned(), t.state(SessionState::Waiting));
        }
        let elsewhere = g
            .items
            .iter()
            .filter(|&&i| app.elsewhere(&l.entries[i]))
            .count();
        if elsewhere > 0 {
            add(format!("● {elsewhere}"), t.elsewhere());
        }
    }
    let mut background = Vec::new();
    let (_, bg_running) = background_counts(app, g);
    if bg_running > 0 {
        background.push(sp(
            format!("{} ", app.spinner()),
            fg(t.state(SessionState::Running)),
        ));
        background.push(sp(bg_running.to_string(), fg(t.text)));
    }
    // 只有后台对话的组（agentq 工作树等）不写「0」。
    let count = if g.items.is_empty() {
        Vec::new()
    } else {
        vec![sp(g.items.len().to_string(), fg(t.muted))]
    };
    let time = newest(app, g)
        .filter(|_| collapsed)
        .map(|n| vec![sp(ago(n, app.now), fg(t.faint))])
        .unwrap_or_default();
    [status, background, count, time]
}

/// 对话那一行的四列：没看过的「刚做完」在状态列一个强调色小点；自动运行的在后台列（在跑转圈，停了空心圈）；
/// 最近时间。
fn item_cells(app: &App, e: &Entry, t: &Theme, accent: Color) -> Cells {
    let status = if app.state_of(&e.session.id) == SessionState::Done {
        vec![sp("•", fg(accent))]
    } else {
        Vec::new()
    };
    let background = match (e.session.automatic, app.background_busy(e)) {
        (true, true) => vec![sp(
            app.spinner().to_string(),
            fg(t.state(SessionState::Running)),
        )],
        (true, false) => vec![sp("○", fg(t.faint))],
        _ => Vec::new(),
    };
    let time = vec![sp(ago(e.session.last_active, app.now), fg(t.faint))];
    [status, background, Vec::new(), time]
}

fn select_bg(buf: &mut Buffer, area: Rect, selected: bool, t: &Theme, accent: Color) {
    row_bg(buf, area, selected, None, t, accent);
}

/// 行底色：选中是淡底加左边一道强调色；刚停下来的（`flash`）整行染上那个状态的颜色，随时间淡回去。
fn row_bg(
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

#[allow(clippy::too_many_arguments)]
fn header(
    buf: &mut Buffer,
    area: Rect,
    app: &App,
    gi: usize,
    selected: bool,
    table: &Table,
    cells: Cells,
    t: &Theme,
    accent: Color,
) {
    let l = app.list();
    let g = &l.groups[gi];
    let collapsed = app.folds.is_collapsed(&g.key);
    // 收起的组里有对话刚停下来：组标题替它亮（取最新的那条）。
    let flash = collapsed
        .then(|| {
            g.items
                .iter()
                .filter_map(|&i| app.flash(&l.entries[i].session.id))
                .max_by(|a, b| a.0.total_cmp(&b.0))
        })
        .flatten();
    // 展开着的组标题整行垫一层底色，一眼分得出哪些组开着；滚到组中间时它钉在第一行，看得到也点得到。
    if !collapsed {
        fill(buf, area.intersection(buf.area), t.band(accent));
    }
    row_bg(buf, area, selected, flash, t, accent);
    let room = table.render(buf, area, cells);
    // 序号是按键（1–9 跳到这一组），用强调色；选中时加粗顶替左边那道细条，看得出点开的是第几个。
    if let Some(n) = g.number
        && area.width > 0
    {
        let style = if selected {
            bold(accent)
        } else {
            fg(mix(accent, t.base, 0.3))
        };
        buf.set_string(area.x, area.y, n.to_string(), style);
    }
    let arrow = if collapsed { "▸" } else { "▾" };
    // 项目名带一点这家的强调色，和下面的对话标题分得开；置顶、折叠组这些不是项目，用次要色。
    let label_style = match g.key {
        GroupKey::Interrupted => bold(t.state(SessionState::Waiting)),
        GroupKey::Project(_) => bold(mix(t.title, accent, 0.3)),
        _ if selected => bold(t.title),
        _ => bold(t.muted),
    };
    let mut left = vec![
        sp(format!(" {arrow} "), fg(t.faint)),
        sp(g.label.clone(), label_style),
    ];
    if let Some(h) = &g.hint {
        left.push(sp(format!("  {h}"), fg(t.faint)));
    }
    let room = room.saturating_sub(1);
    let r = Rect {
        x: area.x + 1,
        width: (room as u16).min(area.width.saturating_sub(1)),
        ..area
    };
    Line::from(clip(left, room)).render_into(buf, r);
}

/// 组里显示出来的对话最近一条的时间。
fn newest(app: &App, g: &Group) -> Option<SystemTime> {
    let l = app.list();
    g.items
        .iter()
        .map(|&i| l.entries[i].session.last_active)
        .max()
}

/// 预览区里的一句：「另有 N 条后台，其中 M 条在跑」；没有就是空的。
fn background_notes(app: &App, g: &Group) -> Option<String> {
    match background_counts(app, g) {
        (0, _) => None,
        (n, 0) => Some(format!("另有 {n} 条后台")),
        (n, running) => Some(format!("另有 {n} 条后台，其中 {running} 条在跑")),
    }
}

/// 组里藏起来的自动运行对话：一共几条、其中几条在跑。
fn background_counts(app: &App, g: &Group) -> (usize, usize) {
    let l = app.list();
    let running = g
        .background
        .iter()
        .filter(|&&i| app.background_busy(&l.entries[i]))
        .count();
    (g.background.len(), running)
}

/// 对话那一行除了标题还标什么。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    None,
    /// 标题后面补项目名（混着几个项目的组、搜索结果）。
    Project,
    /// 标题前面再加那家的标记（汇总里三家混在一起）。
    VendorAndProject,
}

#[allow(clippy::too_many_arguments)]
fn item(
    buf: &mut Buffer,
    area: Rect,
    app: &App,
    e: &Entry,
    tag: Tag,
    selected: bool,
    table: &Table,
    cells: Cells,
    t: &Theme,
    accent: Color,
) {
    row_bg(buf, area, selected, app.flash(&e.session.id), t, accent);
    let room = table.render(buf, area, cells).saturating_sub(2);
    let (glyph, _, glyph_color) = light(app, e, t, t.faint);
    let title_style = if selected { bold(t.title) } else { fg(t.text) };
    // 混着几个项目的组（置顶、搜索结果……）在标题后面补项目名；标题先让位，项目名尽量留全。
    let project = (tag != Tag::None).then(|| truncate(&app.project_label(&e.session.cwd), 14));
    let project_w = project.as_ref().map_or(0, |p| width(p) + 2);
    let mark_w = if tag == Tag::VendorAndProject { 2 } else { 0 };
    let title_room = room.saturating_sub(2 + mark_w + project_w);
    let mut left = vec![sp(format!("{glyph} "), fg(glyph_color))];
    if tag == Tag::VendorAndProject {
        let v = e.session.vendor;
        left.push(sp(format!("{} ", v.mark()), fg(t.accent(v))));
    }
    left.push(sp(truncate(e.title(), title_room), title_style));
    if let Some(p) = project {
        left.push(sp(format!("  {p}"), fg(t.faint)));
    }
    let r = Rect {
        x: area.x + 2,
        width: (room as u16).min(area.width.saturating_sub(2)),
        ..area
    };
    Line::from(clip(left, room)).render_into(buf, r);
}

// ---- 预览 ----

/// 预览区的标题行：这一栏此刻在显示什么。
fn preview_head(buf: &mut Buffer, area: Rect, app: &App, t: &Theme) {
    fill(buf, area, t.bar);
    let label = match app.selected_entry() {
        Some(e) => match (&app.git, app.live_screen()) {
            (Some((dir, _)), _) if *dir == e.session.cwd => "改了哪些文件",
            (_, Some(_)) => "对话 · 此刻的画面",
            _ => "对话预览",
        },
        None => match app.selected_group().map(|g| &g.key) {
            Some(GroupKey::Project(_)) => "项目概览",
            Some(GroupKey::Interrupted) => "被中断的对话",
            Some(_) => "分组概览",
            None => return,
        },
    };
    let r = Rect {
        x: area.x + 3,
        width: area.width.saturating_sub(5),
        ..area
    };
    Line::from(sp(label, fg(t.muted))).render_into(buf, r);
}

fn preview(buf: &mut Buffer, area: Rect, app: &App, t: &Theme, accent: Color) {
    fill(buf, area, t.preview);
    let inner = Rect {
        x: area.x + 3,
        y: area.y + 1,
        width: area.width.saturating_sub(5),
        height: area.height.saturating_sub(1),
    };
    if inner.width < 4 || inner.height == 0 {
        return;
    }
    let w = usize::from(inner.width);
    let mut lines: Vec<Line<'static>> = Vec::new();

    let Some(e) = app.selected_entry() else {
        let mut actions = Vec::new();
        if let Some(g) = app.selected_group() {
            group_summary(&mut lines, app, g, w, t);
            actions = group_actions(app, g, w, t, accent);
        }
        // 能按的键一行一个，贴在预览区底部；放不下就紧跟在说明后面。
        let h = usize::from(inner.height);
        let at = h.saturating_sub(actions.len() + 1).max(lines.len() + 1);
        lines.resize(at.min(h), Line::default());
        lines.extend(actions);
        put_lines(buf, inner, lines);
        return;
    };
    for l in wrap(e.title(), w).into_iter().take(2) {
        lines.push(Line::from(sp(l, bold(t.title))));
    }
    lines.push(Line::from(sp(
        truncate(&tilde(&e.session.cwd, &app.home), w),
        fg(t.muted),
    )));
    let mut meta = Vec::new();
    if let Some(b) = &e.session.git_branch {
        meta.push(sp(format!("{b} · "), fg(t.muted)));
    }
    meta.push(sp(
        format!(
            "{} 条消息 · {} · ",
            e.session.message_count,
            ago(e.session.last_active, app.now)
        ),
        fg(t.muted),
    ));
    let (glyph, label, color) = light(app, e, t, t.muted);
    meta.push(sp(format!("{glyph} {label}"), fg(color)));
    lines.push(Line::from(clip(meta, w)));
    if app.attached.contains(&e.session.id) {
        lines.push(Line::from(
            vec![
                sp(
                    format!("{} 已在标签页打开", e.session.vendor.mark()),
                    fg(accent),
                ),
                sp(" · ", fg(t.faint)),
            ]
            .into_iter()
            .chain(key_hints("⏎ 切过去", t, accent))
            .collect::<Vec<_>>(),
        ));
    }
    if app.interrupted.contains(&e.session.id) {
        lines.push(Line::from(clip(
            [
                sp("被中断", fg(t.state(SessionState::Waiting))),
                sp(" · ", fg(t.faint)),
            ]
            .into_iter()
            .chain(key_hints("⏎ 只打开 · R 全部接着做 · x 移出", t, accent))
            .collect(),
            w,
        )));
    }
    lines.push(Line::default());

    let body = match (&app.git, app.live_screen()) {
        (Some((dir, out)), _) if *dir == e.session.cwd => git_lines(out, w, t),
        // 「此刻的画面」写在预览区标题行，这里不再重复。
        (_, Some(snap)) => screen_lines(snap, w, t),
        _ => message_lines(app, e, w, t, accent),
    };
    let room = usize::from(inner.height).saturating_sub(lines.len());
    // 预览从最后一条往上显示，滚轮往上翻。
    let max_scroll = body.len().saturating_sub(room);
    let scroll = app.preview_scroll.min(max_scroll);
    let start = body.len().saturating_sub(room + scroll);
    lines.extend(body.into_iter().skip(start).take(room));
    put_lines(buf, inner, lines);
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

/// 状态灯：字符、说法、颜色；「已停」用 `stopped` 色（列表里比预览再暗一档）。
/// 在跑的转圈。没托管但别的进程开着的是「别处在用」：柔绿实心点；文件还在写的是「别处在跑」：柔绿转圈。
fn light(app: &App, e: &Entry, t: &Theme, stopped: Color) -> (char, &'static str, Color) {
    if app.elsewhere_busy(e) {
        return (app.spinner(), "别处在跑", t.elsewhere());
    }
    if app.elsewhere(e) {
        return ('●', "别处在用", t.elsewhere());
    }
    let s = app.state_of(&e.session.id);
    let label = match s {
        SessionState::Running => "在跑",
        SessionState::Waiting => "等你回复",
        SessionState::Done => "刚做完",
        SessionState::Stopped => "已停",
    };
    let color = match s {
        SessionState::Stopped => stopped,
        // 等你回复和顶栏「◆ 等你」一起慢闪。
        SessionState::Waiting if app.blink_dim => t.faint,
        s => t.state(s),
    };
    let glyph = match s {
        SessionState::Running => app.spinner(),
        s => s.glyph(),
    };
    (glyph, label, color)
}

fn group_summary(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    g: &super::rows::Group,
    w: usize,
    t: &Theme,
) {
    lines.push(Line::from(sp(truncate(&g.label, w), bold(t.title))));
    if let GroupKey::Project(p) = &g.key {
        lines.push(Line::from(sp(
            truncate(&tilde(p, &app.home), w),
            fg(t.muted),
        )));
    }
    let mut meta = format!("{} 条对话", g.items.len());
    if let Some(n) = newest(app, g) {
        meta.push_str(&format!(" · 最近 {}", ago(n, app.now)));
    }
    lines.push(Line::from(sp(meta, fg(t.muted))));
    if let Some(bg) = background_notes(app, g) {
        lines.push(Line::from(clip(
            vec![
                sp(bg, fg(t.muted)),
                sp(" · 脚本和后台任务起的 · b 显示", fg(t.faint)),
            ],
            w,
        )));
    }
    lines.push(Line::default());
    if g.key == GroupKey::Interrupted {
        for text in [
            "上次关机、重启或崩溃时，这些对话还在跑。",
            "R 全部在新标签页接着做，先发一句「继续上次没做完的事……」",
            "选中单条按 ⏎ 只打开、不发话；x 移出这一组",
        ] {
            lines.push(Line::from(sp(truncate(text, w), fg(t.text))));
        }
        lines.push(Line::default());
    }
}

/// 组标题上能按的键，一行一个：键用标题色，说明用次要色（和底栏一样）。
fn group_actions(
    app: &App,
    g: &super::rows::Group,
    w: usize,
    t: &Theme,
    accent: Color,
) -> Vec<Line<'static>> {
    let enter = if app.folds.is_collapsed(&g.key) {
        "进入"
    } else {
        "收起"
    };
    let mut keys = vec![("⏎", enter)];
    if let GroupKey::Project(p) = &g.key {
        let archive = if app.projects.get(p).is_some_and(|n| n.archived) {
            "移出归档"
        } else {
            "整组归档"
        };
        keys.extend([
            ("n", "在这个项目新对话"),
            ("m", "新建项目，或在别的目录开对话"),
            ("g", "看改了哪些文件"),
            ("e", "改项目显示名"),
            ("a", archive),
        ]);
    }
    keys.into_iter()
        .map(|(k, label)| {
            Line::from(clip(
                vec![sp(format!("{k}  "), bold(accent)), sp(label, fg(t.muted))],
                w,
            ))
        })
        .collect()
}

fn message_lines(app: &App, e: &Entry, w: usize, t: &Theme, accent: Color) -> Vec<Line<'static>> {
    let Some(p) = app.previews.get(&e.session.id) else {
        return vec![Line::from(sp("读取中…", fg(t.faint)))];
    };
    let messages = match &p.messages {
        Ok(m) if m.is_empty() => {
            return vec![Line::from(sp("这条对话还没有文字消息", fg(t.faint)))];
        }
        Ok(m) => m,
        Err(err) => {
            return vec![Line::from(sp(
                truncate(&format!("读不了：{err}"), w),
                fg(t.faint),
            ))];
        }
    };
    let mut out = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if i > 0 {
            out.push(Line::default());
        }
        let who = match m.role {
            Role::User => sp("你", bold(t.muted)),
            Role::Assistant => sp(e.session.vendor.name(), bold(accent)),
        };
        let mut head = vec![who];
        if let Some(at) = m.at {
            head.push(sp(format!("  {}", ago(at, app.now)), fg(t.faint)));
        }
        out.push(Line::from(head));
        let wrapped = wrap(&m.text, w);
        let cut = wrapped.len() > MESSAGE_LINES;
        for (j, l) in wrapped.into_iter().take(MESSAGE_LINES).enumerate() {
            let l = if cut && j == MESSAGE_LINES - 1 {
                truncate(&format!("{l}…"), w)
            } else {
                l
            };
            out.push(Line::from(sp(l, fg(t.text))));
        }
    }
    out
}

/// 实时预览：对话此刻的屏幕，按原样的颜色和样式画，宽度超出的截掉；去掉底部空行，好让最下面的内容贴底显示。
fn screen_lines(snap: &ScreenSnapshot, w: usize, t: &Theme) -> Vec<Line<'static>> {
    let used = snap
        .lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(0, |i| i + 1);
    snap.lines[..used]
        .iter()
        .map(|runs| {
            let spans = runs
                .iter()
                .map(|r| {
                    let text = if r.attrs & attr::HIDDEN != 0 {
                        " ".repeat(width(&r.text))
                    } else {
                        r.text.clone()
                    };
                    sp(text, cell_style(r.fg, r.bg, r.attrs, t))
                })
                .collect();
            Line::from(clip(spans, w))
        })
        .collect()
}

/// 屏幕格子的样式。默认前景用正文色、默认背景留预览区底色；反显自己换前后景，免得落到终端默认色上。
fn cell_style(fg: CellColor, bg: CellColor, attrs: u8, t: &Theme) -> Style {
    let color = |c: CellColor| match c {
        CellColor::Default => None,
        CellColor::Indexed(i) => Some(Color::Indexed(i)),
        CellColor::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    };
    let (mut f, mut b) = (color(fg), color(bg));
    if attrs & attr::INVERSE != 0 {
        (f, b) = (Some(b.unwrap_or(t.preview)), Some(f.unwrap_or(t.text)));
    }
    let mut style = Style::default().fg(f.unwrap_or(t.text));
    if let Some(b) = b {
        style = style.bg(b);
    }
    for (bit, m) in [
        (attr::BOLD, Modifier::BOLD),
        (attr::DIM, Modifier::DIM),
        (attr::ITALIC, Modifier::ITALIC),
        (attr::UNDERLINE, Modifier::UNDERLINED),
        (attr::STRIKEOUT, Modifier::CROSSED_OUT),
    ] {
        if attrs & bit != 0 {
            style = style.add_modifier(m);
        }
    }
    style
}

fn git_lines(out: &Result<String, String>, w: usize, t: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        sp("改了哪些文件", bold(t.muted)),
        sp("  git status --short", fg(t.faint)),
    ])];
    match out {
        Ok(s) if s.trim().is_empty() => lines.push(Line::from(sp("没有改动", fg(t.faint)))),
        Ok(s) => lines.extend(
            s.lines()
                .map(|l| Line::from(sp(truncate(l, w), fg(t.text)))),
        ),
        Err(e) => lines.push(Line::from(sp(truncate(e, w), fg(t.faint)))),
    }
    lines
}

// ---- 底栏 ----

fn bottom_bar(buf: &mut Buffer, area: Rect, app: &mut App, t: &Theme, accent: Color) {
    fill(buf, area, t.bar);
    app.areas.background = Rect::default();
    app.areas.path = Rect::default();
    let inner = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    };
    let cursor = sp("▏", fg(accent));
    if let Some(p) = &app.prompt {
        let (left, right) = match p {
            Prompt::Rename { text, .. } => (
                vec![
                    sp("改名  ", fg(t.muted)),
                    sp(text.clone(), fg(t.title)),
                    cursor,
                ],
                "⏎ 确定 · 留空恢复原名 · Esc 取消",
            ),
            Prompt::NewDir { vendor, picker } => {
                let mut left = vec![
                    sp(
                        format!("在哪个目录新建 {} 对话  ", vendor.name()),
                        fg(t.muted),
                    ),
                    sp(picker.text.clone(), fg(t.title)),
                    cursor,
                ];
                // 没选候选时写出 ⏎ 会去哪（只输名字是 projects_root 下面）。
                if picker.picked.is_none()
                    && let Some(dir) = picker.target(&app.projects_root(), &app.launch_dir)
                {
                    left.push(sp(format!("  → {}", tilde(&dir, &app.home)), fg(t.faint)));
                }
                (left, "Tab 补全 · ↑↓ 选 · ⏎ 确定 · Esc 取消")
            }
            Prompt::ConfirmCreate { dir, .. } => (
                vec![sp(
                    format!("新建目录 {}？", tilde(dir, &app.home)),
                    fg(t.state(SessionState::Waiting)),
                )],
                "⏎ 确认 · Esc 取消",
            ),
            Prompt::RenameProject { text, .. } => (
                vec![
                    sp("项目显示名  ", fg(t.muted)),
                    sp(text.clone(), fg(t.title)),
                    cursor,
                ],
                "⏎ 确定 · 留空恢复目录名 · 不动文件夹 · Esc 取消",
            ),
            Prompt::ConfirmKill { title, .. } => (
                vec![sp(
                    format!("结束「{}」的后台进程？", truncate(title, 30)),
                    fg(t.state(SessionState::Waiting)),
                )],
                "y 确定 · 其他键取消",
            ),
            Prompt::ConfirmOpen { .. } => (
                vec![sp(
                    "这条正在别处运行，再开会两边同时写",
                    fg(t.state(SessionState::Waiting)),
                )],
                "⏎ 仍然打开 · Esc 取消",
            ),
        };
        row(buf, inner, left, key_hints(right, t, accent));
        return;
    }
    if let Some(r) = &app.reader {
        r.bottom(buf, inner, app.status.as_deref(), t);
        return;
    }
    if app.board.is_some() {
        let entries = app.board_entries();
        let waiting = entries
            .iter()
            .filter(|e| app.state_of(&e.session.id) != SessionState::Running)
            .count();
        let running = entries.len() - waiting;
        let left = vec![
            sp("s ", bold(accent)),
            sp(format!("等你 {waiting} · 在跑 {running}"), fg(t.title)),
        ];
        let right = match &app.status {
            Some(s) => vec![sp(s.clone(), fg(t.text))],
            None => key_hints("⏎ 打开 · ! 下一条等你 · Esc 返回", t, accent),
        };
        row(buf, inner, left, right);
        return;
    }
    if let Some(s) = &app.search {
        let left = vec![
            sp("/ ", bold(accent)),
            sp(s.query.clone(), fg(t.title)),
            cursor,
        ];
        let count = if s.query.is_empty() {
            "搜标题和正文".to_owned()
        } else {
            format!("{} 条", s.hits.len())
        };
        let mut right = vec![sp(format!("{count} · "), fg(t.faint))];
        right.extend(key_hints("⏎ 打开 · → 看全文 · Esc 返回", t, accent));
        row(buf, inner, left, right);
        return;
    }
    let mut left = Vec::new();
    // 常驻的「后台 M 在跑」，单击等同 b。
    let running = app.background_running();
    if running > 0 {
        // 只给圆点上状态色，字用次要色：后台对话不是要你处理的事，不和自己的「在跑」抢眼。
        let label = format!("后台 {running} 在跑");
        app.areas.background = Rect {
            width: (width(&label) as u16 + 2).min(inner.width),
            ..inner
        };
        left.push(sp(
            format!("{} ", app.spinner()),
            fg(t.state(SessionState::Running)),
        ));
        left.push(sp(label, fg(t.muted)));
        left.push(sp("   ", fg(t.faint)));
    }
    // 按键用这家的强调色：能按的东西一个颜色。
    for k in keys::bar(app.selected_kind()) {
        left.push(sp(k.keys, bold(accent)));
        left.push(sp(format!(" {}   ", k.label), fg(t.muted)));
    }
    let right = match &app.status {
        Some(s) => vec![sp(s.clone(), fg(t.text))],
        // 选中的对话或项目组所在的目录，和左边隔两格；放不下截掉中间，最少留 16 格（左边的按键让位）。
        // 单击在访达里打开。
        None => app.selected_dir().map_or_else(Vec::new, |dir| {
            const LABEL: &str = "路径：";
            let room = usize::from(inner.width)
                .saturating_sub(line_width(&left) + 2 + width(LABEL))
                .max(16);
            let path = truncate_middle(&tilde(&dir, &app.home), room);
            vec![
                sp(format!("  {LABEL}"), fg(t.faint)),
                sp(path, fg(t.muted).add_modifier(Modifier::UNDERLINED)),
            ]
        }),
    };
    if app.status.is_none() && !right.is_empty() {
        let w = line_width(&right).min(usize::from(inner.width)) as u16;
        app.areas.path = Rect {
            x: inner.right() - w,
            width: w,
            ..inner
        };
    }
    row(buf, inner, left, right);
}

// ---- 浮层 ----

/// N 的目录候选：贴在底栏上方、和列表一样宽，最多 [`newdir::SHOWN`] 行，↑↓ 选中的那行高亮。
fn dir_candidates(buf: &mut Buffer, list: Rect, picker: &DirPicker, t: &Theme, accent: Color) {
    let matches = picker.matches();
    let rows = matches.len().clamp(1, newdir::SHOWN) as u16;
    let h = (rows + 1).min(list.height);
    if h == 0 {
        return;
    }
    let area = Rect {
        y: list.bottom() - h,
        height: h,
        ..list
    };
    fill(buf, area, t.bar);
    let line = |i: u16| Rect {
        x: area.x + 1,
        y: area.y + i,
        width: area.width.saturating_sub(2),
        height: 1,
    };
    let count = match matches.len() {
        0 => {
            let mut v = vec![sp("没有对得上的 · ", fg(t.faint))];
            v.extend(key_hints("⏎ 用输入的路径", t, accent));
            v
        }
        n => vec![sp(format!("{n} 个"), fg(t.faint))],
    };
    row(buf, line(0), vec![sp(" 目录", bold(t.muted))], count);
    // 选中的超出可见行时往下滚。
    let offset = picker
        .picked
        .map_or(0, |p| (p + 1).saturating_sub(newdir::SHOWN));
    let room = usize::from(h - 1);
    for (k, &ci) in matches.iter().enumerate().skip(offset).take(room) {
        let r = line(1 + (k - offset) as u16);
        let selected = picker.picked == Some(k);
        select_bg(
            buf,
            Rect {
                x: area.x,
                width: area.width,
                ..r
            },
            selected,
            t,
            accent,
        );
        let style = if selected { bold(t.title) } else { fg(t.text) };
        let shown = truncate_middle(
            &picker.candidates[ci].shown,
            usize::from(r.width.saturating_sub(2)),
        );
        row(buf, r, vec![sp(format!(" {shown}"), style)], vec![]);
    }
}

/// 右键菜单：左上角贴着点的位置，放不下就往左、往上挪。返回各项所在的区域（一项一行），给鼠标定位用。
fn menu(
    buf: &mut Buffer,
    area: Rect,
    sel: Selected,
    (x, y): (u16, u16),
    cursor: usize,
    t: &Theme,
    accent: Color,
) -> Rect {
    let items = keys::menu(sel);
    let label_w = items.iter().map(|i| width(i.label)).max().unwrap_or(0);
    let keys_w = items.iter().map(|i| width(i.keys)).max().unwrap_or(0);
    // 左右各留两格，标签和按键之间至少四格；上下各留一行。
    let w = ((label_w + keys_w + 8) as u16).min(area.width);
    let h = (items.len() as u16 + 2).min(area.height);
    // 从点的那一行下面弹出，不挡住被点的这一行。
    let boxr = Rect {
        x: x.min(area.right().saturating_sub(w)),
        y: y.saturating_add(1).min(area.bottom().saturating_sub(h)),
        width: w,
        height: h,
    };
    // 左边紧挨着的是宽字符（中文）时它会盖住菜单第一格，先擦掉。
    if boxr.x > area.x {
        for yy in boxr.top()..boxr.bottom() {
            if let Some(c) = buf.cell_mut((boxr.x - 1, yy))
                && width(c.symbol()) > 1
            {
                c.set_symbol(" ");
            }
        }
    }
    fill(buf, boxr, t.bar);
    let rows = Rect {
        y: boxr.y + 1,
        height: h.saturating_sub(2),
        ..boxr
    };
    for (i, item) in items.iter().enumerate().take(usize::from(rows.height)) {
        let r = Rect {
            y: rows.y + i as u16,
            height: 1,
            ..rows
        };
        let selected = i == cursor;
        select_bg(buf, r, selected, t, accent);
        let style = if selected { bold(t.title) } else { fg(t.text) };
        row(
            buf,
            Rect {
                x: r.x + 2,
                width: r.width.saturating_sub(4),
                ..r
            },
            vec![sp(item.label, style)],
            vec![sp(item.keys, bold(t.muted))],
        );
    }
    rows
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn help(buf: &mut Buffer, area: Rect, t: &Theme, accent: Color) {
    let (left_secs, right_secs) = HELP.split_at(2);
    let col_h = |secs: &[(&str, &[super::keys::KeyHelp])]| {
        secs.iter().map(|(_, ks)| ks.len() + 2).sum::<usize>()
    };
    let h = col_h(left_secs).max(col_h(right_secs)) as u16 + 3;
    let boxr = centered(area, 78, h + 2);
    fill(buf, boxr, t.bar);
    let inner = Rect {
        x: boxr.x + 3,
        y: boxr.y + 1,
        width: boxr.width.saturating_sub(6),
        height: boxr.height.saturating_sub(1),
    };
    row(
        buf,
        Rect { height: 1, ..inner },
        vec![sp("全部按键", bold(t.title))],
        vec![sp("任意键关闭", fg(t.faint))],
    );
    let col_w = inner.width / 2;
    for (ci, secs) in [left_secs, right_secs].into_iter().enumerate() {
        let col = Rect {
            x: inner.x + col_w * ci as u16,
            y: inner.y + 2,
            width: col_w,
            height: inner.height.saturating_sub(3),
        };
        let mut lines = Vec::new();
        for (name, keys) in secs {
            lines.push(Line::from(sp(*name, bold(t.muted))));
            for k in *keys {
                let pad = 8usize.saturating_sub(width(k.keys));
                lines.push(Line::from(vec![
                    sp(format!("{}{}", k.keys, " ".repeat(pad)), bold(accent)),
                    sp(k.label, fg(t.text)),
                ]));
            }
            lines.push(Line::default());
        }
        put_lines(buf, col, lines);
    }
    let foot = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    put_lines(
        buf,
        foot,
        vec![Line::from(sp("更多技巧见 README", fg(t.faint)))],
    );
}

/// 闲置收回的时长：没有标签页连着、等你回复、这么久没输出就结束进程（对话记录保留）。
fn idle_label(minutes: u32) -> String {
    match minutes {
        0 => "不收回".to_owned(),
        m if m % 60 == 0 => format!("{} 小时", m / 60),
        m => format!("{m} 分钟"),
    }
}

fn settings(buf: &mut Buffer, area: Rect, app: &App, cursor: usize, t: &Theme, accent: Color) {
    let cfg = &app.config;
    let boxr = centered(area, 64, 15);
    fill(buf, boxr, t.bar);
    let inner = Rect {
        x: boxr.x + 3,
        y: boxr.y + 1,
        width: boxr.width.saturating_sub(6),
        height: boxr.height.saturating_sub(2),
    };
    // 框装不下的行高度为 0，不画（窗口很矮时）。
    let line = |i: u16| Rect {
        y: inner.y + i,
        height: u16::from(i < inner.height),
        ..inner
    };
    row(buf, line(0), vec![sp("设置", bold(t.title))], vec![]);
    let terminal = match TERMINAL_CHOICES
        .iter()
        .find(|c| **c == cfg.preferred_terminal.as_deref())
    {
        Some(None) | None => "跟随当前终端".to_owned(),
        Some(Some("ghostty")) => "Ghostty".to_owned(),
        Some(Some(other)) => {
            let mut s = other.to_string();
            if let Some(f) = s.get_mut(0..1) {
                f.make_ascii_uppercase();
            }
            s
        }
    };
    let items = [
        ("首选终端", terminal),
        ("回收站保留", format!("{} 天", cfg.trash_days)),
        (
            "开机打开",
            if app.autostart { "开" } else { "关" }.to_owned(),
        ),
        ("闲置收回", idle_label(cfg.idle_reclaim_minutes)),
        (
            "等你闪烁",
            if cfg.blink_waiting { "开" } else { "关" }.to_owned(),
        ),
        (
            "自动英文",
            if cfg.auto_ascii_input {
                "开 · 进会话台切英文，离开切回"
            } else {
                "关"
            }
            .to_owned(),
        ),
        (
            "检查新版本",
            if cfg.check_updates {
                "开 · 每天最多查一次 GitHub"
            } else {
                "关"
            }
            .to_owned(),
        ),
    ];
    for (i, (name, value)) in items.iter().enumerate() {
        let r = line(2 + i as u16);
        let selected = i == cursor;
        let full = Rect {
            x: boxr.x + 1,
            width: boxr.width.saturating_sub(2),
            ..r
        };
        select_bg(buf, full, selected, t, accent);
        let vstyle = if selected { bold(t.title) } else { fg(t.text) };
        row(
            buf,
            r,
            vec![
                sp(format!("{name:<8}"), fg(t.muted)),
                sp("‹ ", fg(t.faint)),
                sp(value.clone(), vstyle),
                sp(" ›", fg(t.faint)),
            ],
            vec![],
        );
    }
    row(
        buf,
        line(9),
        vec![
            sp(format!("{:<8}", "呼出键"), fg(t.muted)),
            sp(cfg.hotkey.clone(), fg(t.text)),
            sp("  在 config.json 里改", fg(t.faint)),
        ],
        vec![],
    );
    row(
        buf,
        line(11),
        vec![sp("↑↓ 选择 · ←→ 修改 · Esc 保存并关闭", fg(t.faint))],
        vec![],
    );
    row(
        buf,
        line(12),
        vec![sp(
            "其余在 ~/Library/Application Support/huitai/config.json",
            fg(t.faint),
        )],
        vec![],
    );
}

#[cfg(test)]
mod tests {
    use super::with_tab_forms;

    #[test]
    fn tabs_shrink_first_only_with_five_or_more() {
        // 少于五家：每种写法都用全名，最后再加一种缩写的。
        assert_eq!(
            with_tab_forms(vec![1, 2, 3], 4),
            [(1, false), (2, false), (3, false), (3, true)]
        );
        // 五家以上：最全的写法先试全名，放不下就缩标签，之后都是缩写的。
        assert_eq!(
            with_tab_forms(vec![1, 2, 3], 5),
            [(1, false), (1, true), (2, true), (3, true)]
        );
        assert!(with_tab_forms(Vec::<u8>::new(), 5).is_empty());
    }
}
