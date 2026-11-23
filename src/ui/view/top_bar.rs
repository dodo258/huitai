//! 顶栏：印章、全局状态、各家标签、额度。

use super::*;
use crate::ui::text::resets_in;

/// 顶栏印章后的全局状态写成什么样（窄了一步步缩）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusForm {
    /// `◐3 在跑  ◆1 待处理  ●2 别处`
    Full,
    /// `◐3 ◆1 ●2`
    Counts,
    /// `◐ ◆ ●`
    Dots,
    Hidden,
}

/// 额度留哪些：「额度」两字、第二个窗口、来源和更新时间、重置倒计时。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QuotaCut {
    label: bool,
    second: bool,
    source: bool,
    reset: bool,
}

/// 顶栏的写法，从全到简。窄终端依次去掉：额度来源 → 第二个额度窗口 → 重置倒计时 → 状态数字只留点 → 「额度」两字，
/// 再放不下额度让位（后台是旧版、有新版的提示比额度优先），再放不下先去掉「别处」、留着「在跑 / 待处理」，最后状态也不显示。
/// 每项是（状态写法，额度留哪些，带不带提示，带不带「别处」）。
fn top_layouts(hint: bool) -> Vec<(StatusForm, Option<QuotaCut>, bool, bool)> {
    let cut = |label, second, source, reset| {
        Some(QuotaCut {
            label,
            second,
            source,
            reset,
        })
    };
    let mut out: Vec<_> = [
        (StatusForm::Full, cut(true, true, true, true)),
        (StatusForm::Full, cut(true, true, false, true)),
        (StatusForm::Full, cut(true, false, false, true)),
        (StatusForm::Full, cut(true, false, false, false)),
        (StatusForm::Counts, cut(true, false, false, false)),
        (StatusForm::Dots, cut(true, false, false, false)),
        (StatusForm::Dots, cut(false, false, false, false)),
    ]
    .into_iter()
    .map(|(s, q)| (s, q, hint, true))
    .collect();
    if hint {
        out.push((StatusForm::Dots, None, true, true));
    }
    out.push((StatusForm::Dots, None, false, true));
    // 「别处」是自己托管的之外多出来的，挤不下先让它走，「待处理」那个点（单击跳过去）留到最后。
    out.push((StatusForm::Dots, None, false, false));
    out.push((StatusForm::Hidden, None, false, false));
    out
}

/// 各家标签写成什么样（窄了一步步缩）；当前这家一直写「标记 全名」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TabForm {
    /// 每家「标记 全名」：`✻ Claude │ ◎ Codex │ K Kimi Code`
    Full,
    /// 别家去掉标记、只写短名：`✻ Claude │ Codex │ Kimi`
    Short,
    /// 别家只剩标记：`✻ Claude │ ◎ │ K`
    Marks,
}

/// 标签和右边一起怎么缩。装了五家以上时，放不下先把别家写成短名，再按 `layouts` 依次去掉额度细节；
/// 到 `plain`（额度细节都去掉了、状态还写全的那一种）还放不下，别家才缩成标记，再从最全的写法试起
/// （名字让出来的地方还给额度细节，和原来一样）。少于五家时标签一直写全名，别的都去掉了还放不下才写短名、再缩成标记。
pub(super) fn with_tab_forms<L: Copy>(
    layouts: Vec<L>,
    tabs: usize,
    plain: usize,
) -> Vec<(L, TabForm)> {
    const MANY_TABS: usize = 5;
    let Some(&last) = layouts.last() else {
        return Vec::new();
    };
    if tabs < MANY_TABS {
        let mut out: Vec<_> = layouts.into_iter().map(|l| (l, TabForm::Full)).collect();
        out.extend([(last, TabForm::Short), (last, TabForm::Marks)]);
        return out;
    }
    let plain = plain.min(layouts.len() - 1);
    let mut out = vec![(layouts[0], TabForm::Full)];
    out.extend(layouts[..=plain].iter().map(|&l| (l, TabForm::Short)));
    out.extend(layouts.iter().map(|&l| (l, TabForm::Marks)));
    out
}

/// 别家标签的短名：全名的第一个词（「Kimi Code」→「Kimi」），本来就短的照写。
fn short_name(v: Vendor) -> &'static str {
    v.name().split(' ').next().unwrap_or(v.name())
}

/// 全局状态，分两段（各自单击做的事不同）：会话台自己托管的在跑（绿）、待处理（等回复、待读、失败，黄），
/// 和别处在用（柔绿；有正在干活的就转圈）。都没有就是空的。符号和计数用状态色，后面的字用次要色；
/// 待处理慢闪时只有 ◆ 在暗半拍变暗，数字和字一直稳着（照 gitui 加载时只动一个字符）。
fn status_spans(
    app: &App,
    form: StatusForm,
    t: &Theme,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let (open, busy) = app.elsewhere_counts();
    let dot = if busy > 0 { SPIN_MARK } else { '●' };
    let c = t.elsewhere();
    let elsewhere = chips(&[(open, dot, "别处", c, c)], form, t);
    (own_status(app, form, t), elsewhere)
}

/// 自己托管的：在跑、待处理（和列表状态列补短词的三种、`s` 汇总数的是同一批）。
fn own_status(app: &App, form: StatusForm, t: &Theme) -> Vec<Span<'static>> {
    let (running, waiting) = app.global_counts();
    let run = t.state(SessionState::Running);
    let wait = t.state(SessionState::Waiting);
    let wait_mark = if app.blink_dim {
        t.blink_dim(wait)
    } else {
        wait
    };
    let parts = [
        (running, SPIN_MARK, "在跑", run, run),
        (waiting, '◆', "待处理", wait, wait_mark),
    ];
    chips(&parts, form, t)
}

/// 「◐3 在跑」这样一个个排开，按 `form` 缩写；数是 0 的不写。每项是（数，符号，字，计数的颜色，符号的颜色）。
fn chips(
    parts: &[(usize, char, &str, Color, Color)],
    form: StatusForm,
    t: &Theme,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for &(n, dot, word, color, mark) in parts.iter().filter(|p| p.0 > 0) {
        let gap = match form {
            StatusForm::Full => "  ",
            StatusForm::Counts | StatusForm::Dots => " ",
            StatusForm::Hidden => return Vec::new(),
        };
        if !out.is_empty() {
            out.push(sp(gap, Style::default()));
        }
        out.push(sp(dot.to_string(), fg(mark)));
        if form != StatusForm::Dots {
            out.push(sp(n.to_string(), fg(color)));
        }
        if form == StatusForm::Full {
            out.push(sp(format!(" {word}"), fg(t.muted)));
        }
    }
    out
}

pub(super) fn top_bar(buf: &mut Buffer, area: Rect, app: &mut App, t: &Theme) {
    fill(buf, area, t.bar);
    app.areas.tabs.clear();
    app.areas.status = Rect::default();
    app.areas.elsewhere = Rect::default();
    let accent = t.accent(app.vendor);
    let seal = " 会话台 ";
    // 当前这家一直写「标记 全名」，别家按 `form` 缩。
    let tabs_of = |form: TabForm| -> Vec<(Vendor, String)> {
        app.shown
            .iter()
            .map(|&v| {
                let label = match form {
                    _ if v == app.vendor => format!("{} {}", v.mark(), v.name()),
                    TabForm::Full => format!("{} {}", v.mark(), v.name()),
                    TabForm::Short => short_name(v).to_string(),
                    TabForm::Marks => v.mark().to_string(),
                };
                (v, label)
            })
            .collect()
    };
    // 那家 CLI 有新版：标签后紧跟一个暗色「↑」（设置里关了检查新版本就不标）。
    let cli_new = |v: Vendor| app.config.check_updates && app.cli_updates.contains_key(&v);
    let mark_w = |v: Vendor| usize::from(cli_new(v)) * width(CLI_NEW_MARK);
    // 标签之间隔一道淡竖线（两边各一格空）。
    const TAB_DIVIDER: &str = " │ ";
    let tabs_width = |tabs: &[(Vendor, String)]| {
        tabs.iter()
            .map(|(v, l)| width(l) + mark_w(*v))
            .sum::<usize>()
            + width(TAB_DIVIDER) * tabs.len().saturating_sub(1)
    };
    let form_w = [TabForm::Full, TabForm::Short, TabForm::Marks].map(|f| tabs_width(&tabs_of(f)));
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
    // 「更新」按钮（有能升级的各家 CLI、升级中、有没升上的才有）：放在右边最前，比提示和额度都优先。
    let button = app.update_button().map(|label| {
        let kind = match app.cli_upgrade {
            CliUpgrade::Running { .. } => UpdateButton::Running,
            CliUpgrade::Failed(_) => UpdateButton::Failed,
            CliUpgrade::Idle => UpdateButton::Ready,
        };
        sp(
            format!(" {label} "),
            Style::default()
                .bg(t.update_button(kind))
                .fg(t.title)
                .add_modifier(Modifier::BOLD),
        )
    });
    let total = usize::from(area.width);
    // 两段状态之间隔两格，和段里各项之间一样。
    let status_w = |(own, other): &(Vec<Span>, Vec<Span>)| {
        let (a, b) = (line_width(own), line_width(other));
        a + b + if a > 0 && b > 0 { 2 } else { 0 }
    };
    let left_w = |status: &(Vec<Span>, Vec<Span>), form: TabForm| {
        let sw = status_w(status);
        let tabs_w = form_w[form as usize];
        1 + width(seal) + if sw > 0 { 1 + sw } else { 0 } + gap.len() + tabs_w
    };
    // 从全到简挑第一种放得下的；连最简的都放不下（很窄）就只有印章和标签。
    let layouts = top_layouts(hint.is_some());
    // 别家的名字比额度细节（来源、第二个窗口、重置倒计时）要紧，比状态的字和额度百分比次要。
    let plain = layouts
        .iter()
        .position(|&(form, cut, ..)| {
            form == StatusForm::Full && matches!(cut, Some(c) if !c.source && !c.second && !c.reset)
        })
        .unwrap_or(0);
    let (status, right, tab_form) = with_tab_forms(layouts, app.shown.len(), plain)
        .into_iter()
        .map(|((form, cut, with_hint, with_elsewhere), tab_form)| {
            let mut status = status_spans(app, form, t);
            if !with_elsewhere {
                status.1.clear();
            }
            // pi / Hermes 默认 provider 有余额或订阅额度就显示它，没有再照旧（本机统计）。
            let mut right = match (cut, app.balance_shown()) {
                (Some(c), Some((row, b))) => {
                    let mut s = crate::ui::balance::top_spans(
                        row, b, app.now, app.vendor, t, c.source, c.reset,
                    );
                    if c.label {
                        s.insert(0, sp("额度  ", fg(t.muted)));
                    }
                    s
                }
                (Some(c), None) if quota::supported(app.vendor) => {
                    quota_spans(app.quota_shown(), app.now, app.vendor, t, c)
                }
                _ => Vec::new(),
            };
            if let Some(h) = hint.as_ref().filter(|_| with_hint) {
                let lead = if right.is_empty() { "" } else { gap };
                right.splice(
                    0..0,
                    [sp(h.clone(), fg(t.faint)), sp(lead, Style::default())],
                );
            }
            if let Some(b) = &button {
                let lead = if right.is_empty() { "" } else { "  " };
                right.splice(0..0, [b.clone(), sp(lead, Style::default())]);
            }
            (status, right, tab_form)
        })
        .find(|(status, right, tab_form)| {
            let rw = line_width(right);
            left_w(status, *tab_form) + if rw > 0 { 2 + rw } else { 0 } < total
        })
        .unwrap_or_else(|| (Default::default(), Vec::new(), TabForm::Marks));
    let tabs = tabs_of(tab_form);

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
        let mark = mark_w(v) as u16;
        if x + lead + w + mark > area.right() {
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
        if mark > 0 {
            buf.set_string(x, area.y, CLI_NEW_MARK, fg(t.faint));
            x += mark;
        }
    }
    let w = line_width(&right);
    app.areas.update = Rect::default();
    if w > 0 {
        let r = Rect {
            x: area.right() - 1 - w as u16,
            width: w as u16,
            ..area
        };
        if let Some(b) = right.first().filter(|_| button.is_some()) {
            app.areas.update = Rect {
                width: width(&b.content) as u16,
                ..r
            };
        }
        Line::from(right).render_into(buf, r);
    }
}

/// 那家 CLI 有新版时标签后的标记。
pub const CLI_NEW_MARK: &str = "↑";

/// 额度用到这么多（百分比）变黄。
pub const QUOTA_WARN: f32 = 80.0;
/// 用到这么多变红。
pub const QUOTA_ALERT: f32 = 95.0;

/// 额度窗口离重置还有多久（「1 小时 20 分后重置」）；没写重置时间或已经过了（读数过期）就没有。
pub fn reset_note(w: &crate::core::QuotaWindow, now: SystemTime) -> Option<String> {
    let left = w.resets_at?.duration_since(now).ok()?;
    (!left.is_zero()).then(|| resets_in(left))
}

/// 额度：「额度」两字，每个窗口一条细进度条加百分比和重置倒计时，后面是来源和更新时间；`cut` 定留哪些。
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
                let mut out = vec![
                    sp(format!("{} ", w.label), fg(t.muted)),
                    sp("━".repeat(filled), fg(bar_color)),
                    sp("━".repeat(8 - filled), fg(t.select)),
                    sp(format!(" {used:.0}%"), fg(pct_color)),
                ];
                if let Some(note) = reset_note(w, now).filter(|_| cut.reset) {
                    out.push(sp(format!(" · {note}"), fg(t.faint)));
                }
                out
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
