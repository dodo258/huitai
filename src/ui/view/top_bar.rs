//! 顶栏：印章、全局状态、各家标签、额度。

use super::*;
use crate::ui::text::resets_in;

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

/// 额度留哪些：「额度」两字、第二个窗口、来源和更新时间、重置倒计时。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QuotaCut {
    label: bool,
    second: bool,
    source: bool,
    reset: bool,
}

/// 顶栏的写法，从全到简。窄终端依次去掉：额度来源 → 第二个额度窗口 → 重置倒计时 → 状态数字只留点 → 「额度」两字，
/// 再放不下额度让位（后台是旧版、有新版的提示比额度优先），再放不下先去掉「别处」、留着「在跑 / 等你」，最后状态也不显示。
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
    // 「别处」是自己托管的之外多出来的，挤不下先让它走，「等你」那个点（单击跳过去）留到最后。
    out.push((StatusForm::Dots, None, false, false));
    out.push((StatusForm::Hidden, None, false, false));
    out
}

/// 标签几时缩成标记：装了五家以上时，放不下先缩标签（当前这家留全名），再按 `layouts` 依次去掉别的；
/// 少于五家时标签一直写全名，别的都去掉了还放不下才缩。
pub(super) fn with_tab_forms<L: Copy>(layouts: Vec<L>, tabs: usize) -> Vec<(L, bool)> {
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
    let dot = if busy > 0 { SPIN_MARK } else { '●' };
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
        (running, SPIN_MARK, "在跑", t.state(SessionState::Running)),
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

pub(super) fn top_bar(buf: &mut Buffer, area: Rect, app: &mut App, t: &Theme) {
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
