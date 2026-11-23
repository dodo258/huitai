//! 列表：分组和对话行、搜索结果的命中片段、列宽。

use super::preview::light;
use super::*;

pub(super) fn list(buf: &mut Buffer, head: Rect, area: Rect, app: &App, t: &Theme, accent: Color) {
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
        // 各家一起搜的：表头写明，每行标题前标那家的标记（同汇总）。
        let (label, tag) = if s.all {
            ("搜索结果 · 各家", Tag::VendorAndProject)
        } else {
            ("搜索结果", Tag::Project)
        };
        if s.hits.is_empty() {
            list_head(buf, head, label, &Table::new(&[], 0), t);
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
            if !s.all {
                let mut hint = vec![sp("  ", fg(t.faint))];
                hint.extend(key_hints("⌃A 各家一起搜", t, accent));
                row(buf, line(3), hint, vec![]);
            }
            return;
        }
        let cells: Vec<Cells> = s.hits.iter().map(|e| item_cells(app, e, t)).collect();
        let table = Table::new(&cells, usize::from(area.width));
        list_head(buf, head, label, &table, t);
        let mut y = 0;
        for (i, (e, c)) in s.hits.iter().zip(cells).enumerate().skip(s.offset) {
            if y >= area.height {
                break;
            }
            let selected = i == s.selected;
            item(buf, line(y), app, e, tag, selected, &table, c, t, accent);
            if let Some(snip) = app.snippet_line(&e.session.id) {
                y += 1;
                if y < area.height {
                    snippet_row(buf, line(y), app, e, snip, selected, t, accent);
                }
            }
            y += 1;
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
            Row::Item { entry, .. } => item_cells(app, &l.entries[entry], t),
        })
        .collect();
    let table = Table::new(&cells, usize::from(area.width));
    list_head(buf, head, "项目", &table, t);
    let mut y = 0;
    for (i, (r, c)) in l.rows.iter().zip(cells).enumerate().skip(l.offset) {
        if y >= area.height {
            break;
        }
        let selected = i == l.selected;
        match *r {
            Row::Header(g) => {
                // 上面空的那一行就是列表底色。
                if app.gap_before(i) {
                    y += 1;
                    if y >= area.height {
                        break;
                    }
                }
                header(buf, line(y), app, g, selected, &table, c, t, accent);
            }
            Row::Item { group, entry } => {
                let tag = if l.groups[group].mixed() {
                    Tag::Project
                } else {
                    Tag::None
                };
                let e = &l.entries[entry];
                item(buf, line(y), app, e, tag, selected, &table, c, t, accent);
                if let Some(d) = app.doing(e) {
                    y += 1;
                    if y < area.height {
                        activity::doing_row(buf, line(y), app, e, &d, selected, t, accent);
                    }
                }
            }
        }
        y += 1;
    }
    if let Some(h) = l.sticky().filter(|_| area.height >= 2)
        && let Row::Header(g) = l.rows[h]
    {
        let c = group_cells(app, &l.groups[g], t);
        header(buf, line(0), app, g, h == l.selected, &table, c, t, accent);
    }
}

/// 搜索结果标题下那行命中片段，从标题那一列起，命中词和查看视图一样高亮。还没取回来时只画一个暗色 `…` 占位。
#[allow(clippy::too_many_arguments)]
fn snippet_row(
    buf: &mut Buffer,
    area: Rect,
    app: &App,
    e: &Entry,
    snip: Option<&Snippet>,
    selected: bool,
    t: &Theme,
    accent: Color,
) {
    row_bg(buf, area, selected, app.flash(&e.session.id), t, accent);
    // 和动态那一行一样：从标题那一列（第 4 格起；各家一起搜时标题前多了那家的标记，第 6 格起）。
    let indent = if app.search.as_ref().is_some_and(|s| s.all) {
        6
    } else {
        4
    };
    let r = Rect {
        x: area.x + indent,
        width: area.width.saturating_sub(indent + 1),
        ..area
    };
    let room = usize::from(r.width);
    let spans = match snip {
        None => vec![sp("…", fg(t.faint))],
        Some(snip) => {
            let bg = if selected { t.select } else { t.base };
            let hit = Style::default()
                .fg(t.title)
                .bg(mix(bg, accent, 0.35))
                .add_modifier(Modifier::BOLD);
            snippet_spans(snip, room, fg(t.muted), hit)
        }
    };
    Line::from(clip(spans, room)).render_into(buf, r);
}

/// 命中片段排成一行的几段：第一个命中词前面的字最多占 `room` 的三分之一，多了从前面裁掉换成 `…`，
/// 窄终端里也看得到命中词；后面放不下的由 [`clip`] 截掉。
pub(super) fn snippet_spans(
    s: &Snippet,
    room: usize,
    base: Style,
    hit: Style,
) -> Vec<Span<'static>> {
    let first = s.marks.first().map_or(0, |m| m.0);
    let lead_room = room / 3;
    let mut out = Vec::new();
    let mut at = 0;
    if width(&s.text[..first]) > lead_room {
        at = first;
        let mut used = 1;
        for (i, c) in s.text[..first].char_indices().rev() {
            used += width(c.encode_utf8(&mut [0; 4]));
            if used > lead_room {
                break;
            }
            at = i;
        }
        out.push(Span::styled("…", base));
    }
    for &(a, b) in &s.marks {
        if a > at {
            out.push(Span::styled(s.text[at..a].to_owned(), base));
        }
        out.push(Span::styled(s.text[a..b].to_owned(), hit));
        at = b;
    }
    if at < s.text.len() {
        out.push(Span::styled(s.text[at..].to_owned(), base));
    }
    out
}

/// 「⏎ 确定 · 留空恢复原名 · Esc 取消」这类按键说明：按键用强调色加粗（和底栏、帮助一样，看得出能按），
/// 说明用次要色；不以按键开头的一段（「留空恢复原名」）整段用暗色。
pub(super) fn key_hints(text: &str, t: &Theme, accent: Color) -> Vec<Span<'static>> {
    let is_key = |k: &str| {
        matches!(k, "⏎" | "Esc" | "Tab" | "⇧Tab" | "↑↓" | "→" | "!" | "⌃A")
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

/// 列表右边的几列，从左到右：状态（对话的状态灯，有计划时前面写进度；收起的组里在跑、等你、别处在用几条）、
/// 后台（藏起来的后台对话在跑几条）、对话（组里几条）、最近（多久前聊过）。
const COLS: [&str; 4] = ["状态", "后台", "对话", "最近"];
/// 窄的时候按这个顺序省列：先省条数，再省后台，再省最近；状态灯只在这一列，一直在。
const DROP: [usize; 3] = [2, 1, 3];
/// 列和列之间、列和名字之间隔几格。
const GAP: usize = 2;
/// 左边至少留给名字这么多格，不够就省列。
const MIN_NAME: usize = 14;

/// 一行里四列各写什么（没有就是空的）。
pub(super) type Cells = [Vec<Span<'static>>; 4];

/// 列宽：每列取表头字和所有行里最宽的那个，所以上下对得齐；所有行都空着的列不画。
pub(super) struct Table {
    widths: [usize; 4],
    shown: [bool; 4],
}

impl Table {
    pub(super) fn new(rows: &[Cells], row_width: usize) -> Table {
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
pub(super) fn list_head(buf: &mut Buffer, area: Rect, label: &str, table: &Table, t: &Theme) {
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
                format!("{} {running}", SPIN_MARK),
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
            format!("{} ", SPIN_MARK),
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

/// 对话那一行的四列：状态列是状态灯（靠右，上下对成一列），有计划时前面写进度「2/5」；
/// 自动运行的在后台列（在跑转圈，停了空心圈）；最近时间。
pub(super) fn item_cells(app: &App, e: &Entry, t: &Theme) -> Cells {
    let mut status = activity::plan_cell(app, e, t);
    if !status.is_empty() {
        status.push(sp(" ", Style::default()));
    }
    let (glyph, _, color) = light(app, e, t, t.faint);
    status.push(sp(glyph.to_string(), fg(color)));
    let background = match (e.session.automatic, app.background_busy(e)) {
        (true, true) => vec![sp(
            SPIN_MARK.to_string(),
            fg(t.state(SessionState::Running)),
        )],
        (true, false) => vec![sp("○", fg(t.faint))],
        _ => Vec::new(),
    };
    let time = vec![sp(ago(e.session.last_active, app.now), fg(t.faint))];
    [status, background, Vec::new(), time]
}

pub(super) fn select_bg(buf: &mut Buffer, area: Rect, selected: bool, t: &Theme, accent: Color) {
    row_bg(buf, area, selected, None, t, accent);
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
pub(super) fn newest(app: &App, g: &Group) -> Option<SystemTime> {
    let l = app.list();
    g.items
        .iter()
        .map(|&i| l.entries[i].session.last_active)
        .max()
}

/// 预览区里的一句：「另有 N 条后台，其中 M 条在跑」；没有就是空的。
pub(super) fn background_notes(app: &App, g: &Group) -> Option<String> {
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
pub(super) enum Tag {
    None,
    /// 标题后面补项目名（混着几个项目的组、搜索结果）。
    Project,
    /// 标题前面再加那家的标记（汇总里三家混在一起）。
    VendorAndProject,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn item(
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
    // 标题从第 4 格起，和组名、表头「项目」对齐；状态灯在右边状态列。
    let room = table.render(buf, area, cells).saturating_sub(4);
    // 对话标题是列表里最要紧的字：标题色加粗；选中靠底色和左边细条认。
    let title_style = bold(t.title);
    // 混着几个项目的组（置顶、搜索结果……）在标题后面补项目名；标题先让位，项目名尽量留全。
    let project = (tag != Tag::None).then(|| truncate(&app.project_label(&e.session.cwd), 14));
    let project_w = project.as_ref().map_or(0, |p| width(p) + 2);
    // 同一家有几个账号时，标题后面用暗色小字写这条是哪个账号的（只有一个账号不写）。
    let account = app.account_label(&e.session).map(|a| truncate(a, 10));
    let account_w = account.as_ref().map_or(0, |a| width(a) + 2);
    let mark_w = if tag == Tag::VendorAndProject { 2 } else { 0 };
    let title_room = room.saturating_sub(mark_w + project_w + account_w);
    let mut left = Vec::new();
    if tag == Tag::VendorAndProject {
        let v = e.session.vendor;
        left.push(sp(format!("{} ", v.mark()), fg(t.accent(v))));
    }
    left.push(sp(truncate(e.title(), title_room), title_style));
    if let Some(a) = account {
        left.push(sp(format!("  {a}"), fg(t.faint)));
    }
    if let Some(p) = project {
        left.push(sp(format!("  {p}"), fg(t.faint)));
    }
    let r = Rect {
        x: area.x + 4,
        width: (room as u16).min(area.width.saturating_sub(4)),
        ..area
    };
    Line::from(clip(left, room)).render_into(buf, r);
}
