//! 预览：标题行、最近几条消息、托管对话的屏幕、git 状态。

use super::list::{background_notes, key_hints, newest};
use super::*;

/// 计划块下面至少给画面或消息留几行；留不够就把计划收成一行。
const PLAN_ROOM: usize = 4;

/// 预览区的标题行：这一栏此刻在显示什么。
/// 选中的是托管着或别处在跑的对话时，标题跟着状态走：「在跑 · 4 分 12 秒」「做完 · 用时 6 分钟」
/// 「等你回复 · 3 分钟」「失败」，用状态的颜色。选中的对话报过上下文占用时后面接「 · 上下文 72%」。
pub(super) fn preview_head(buf: &mut Buffer, area: Rect, app: &App, t: &Theme) {
    fill(buf, area, t.bar);
    let r = Rect {
        x: area.x + 3,
        width: area.width.saturating_sub(5),
        ..area
    };
    if let Some(e) = app.selected_entry()
        && !matches!(&app.git, Some((dir, _)) if *dir == e.session.cwd)
        && let Some((status, color)) = activity::head_status(app, e, t)
    {
        let mut spans = vec![sp(status, fg(color))];
        if app.live_screen().is_some() {
            spans.push(sp(" · 此刻的画面", fg(t.muted)));
        }
        spans.extend(context_note(app, e, t));
        spans.extend(deletion_note(app, e, t));
        Line::from(clip(spans, usize::from(r.width))).render_into(buf, r);
        return;
    }
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
    let mut spans = vec![sp(label, fg(t.muted))];
    if let Some(e) = app.selected_entry() {
        spans.extend(context_note(app, e, t));
        spans.extend(deletion_note(app, e, t));
    }
    Line::from(clip(spans, usize::from(r.width))).render_into(buf, r);
}

/// 预览区标题行后面的「 · 约 2 天后被 Claude 删除」：离 Claude 删它不到 3 天的才有，用提醒的红。
fn deletion_note(app: &App, e: &Entry, t: &Theme) -> Vec<Span<'static>> {
    app.deletion_note(e)
        .map(|n| vec![sp(" · ", fg(t.faint)), sp(n, fg(t.alert()))])
        .unwrap_or_default()
}

/// 预览区标题行后面的「 · 上下文 72%」；没有读数就没有。
fn context_note(app: &App, e: &Entry, t: &Theme) -> Vec<Span<'static>> {
    app.context_of(e)
        .map(|c| vec![sp(" · ", fg(t.faint)), context_span(c, t, t.muted)])
        .unwrap_or_default()
}

pub(super) fn preview(buf: &mut Buffer, area: Rect, app: &App, t: &Theme, accent: Color) {
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
    lines.push(if e.session.resumable() {
        Line::from(sp(
            truncate(&tilde(&e.session.cwd, &app.home), w),
            fg(t.muted),
        ))
    } else {
        Line::from(sp(truncate(VIEW_ONLY, w), fg(t.text)))
    });
    let git = matches!(&app.git, Some((dir, _)) if *dir == e.session.cwd);
    // 标题行已经写了状态（在跑 / 等你回复 / 做完 / 失败）就不再写状态灯。
    let show_light = git || activity::head_status(app, e, t).is_none();
    lines.push(meta_line(app, e, show_light, w, t));
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
    if let Some(m) = app.moments.get(&e.session.id).filter(|_| !git) {
        let mut plan = activity::plan_lines(m, w, t, accent);
        // 展开的计划会让下面的画面或消息不足 PLAN_ROOM 行（计划前后各空一行）就收成一行。
        let left = usize::from(inner.height).saturating_sub(lines.len() + plan.len() + 2);
        if plan.len() > 1 && left < PLAN_ROOM {
            plan = activity::plan_line(m, w, t, accent).into_iter().collect();
        }
        if !plan.is_empty() {
            lines.push(Line::default());
            lines.extend(plan);
        }
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

/// 预览元信息行：「分支 · 你发了 N 条 · 最近活跃 4 分钟前 · ○ 已停」。各家的计数都是用户发言条数。
/// 放不下先省计数、再省分支，最近活跃和状态灯留着。
fn meta_line(app: &App, e: &Entry, show_light: bool, w: usize, t: &Theme) -> Line<'static> {
    let branch = e.session.git_branch.as_deref();
    let count = format!("你发了 {} 条", e.session.message_count);
    let active = format!("最近活跃 {}", ago(e.session.last_active, app.now));
    let lamp = show_light.then(|| light(app, e, t, t.muted));
    let light_text = lamp.map(|(glyph, label, _)| format!("{glyph} {label}"));
    let fits = |with_branch: bool, with_count: bool| {
        let parts = [
            branch.filter(|_| with_branch),
            with_count.then_some(count.as_str()),
            Some(active.as_str()),
            light_text.as_deref(),
        ];
        let parts: Vec<&str> = parts.into_iter().flatten().collect();
        width(&parts.join(" · ")) <= w
    };
    let with_count = fits(true, true);
    let mut text: Vec<String> = Vec::new();
    if fits(true, with_count) {
        text.extend(branch.map(str::to_owned));
    }
    if with_count {
        text.push(count);
    }
    text.push(active);
    let mut spans = vec![sp(text.join(" · "), fg(t.muted))];
    if let (Some((_, _, color)), Some(l)) = (lamp, light_text) {
        spans.push(sp(" · ", fg(t.muted)));
        spans.push(sp(l, fg(color)));
    }
    Line::from(clip(spans, w))
}

/// 状态灯：字符、说法、颜色；「已停」用 `stopped` 色（列表里比预览再暗一档）。
/// 在跑的转圈。没托管但别的进程开着的是「别处在用」：柔绿实心点；文件还在写的是「别处在跑」：柔绿转圈。
pub(super) fn light(
    app: &App,
    e: &Entry,
    t: &Theme,
    stopped: Color,
) -> (char, &'static str, Color) {
    if app.elsewhere_busy(e) {
        return (SPIN_MARK, "别处在跑", t.elsewhere());
    }
    if app.elsewhere(e) {
        return ('●', "别处在用", t.elsewhere());
    }
    if app.failed(e) {
        return ('✗', "失败", t.alert());
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
        // 没看过的「刚做完」和顶栏「◆ 待处理」一起慢闪；看过的「等你回复」不闪。
        SessionState::Done if app.blink_dim => t.faint,
        s => t.state(s),
    };
    let glyph = match s {
        SessionState::Running => SPIN_MARK,
        s => s.glyph(),
    };
    (glyph, label, color)
}

fn group_summary(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    g: &crate::ui::rows::Group,
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
    g: &crate::ui::rows::Group,
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
