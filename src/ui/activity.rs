//! 看得见的动态（方案 v2「5. 看得见的动态」）：对话行下面那一行压暗的「此刻」、行尾的计划进度、
//! 预览标题随状态（在跑 · 4 分 12 秒 / 做完 · 用时 6 分钟 / 等你回复 · 3 分钟 / 失败）、预览里的计划块。
//! 数据是 sources 读来的 [`Moment`]（只读会话文件已有的，不调模型）；没有数据就不画。

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};

use crate::core::SessionState;
use crate::index::Entry;
use crate::sources::{Moment, StepStatus};

use super::app::{App, Doing};
use super::text::{clock, span, truncate};
use super::theme::Theme;
use super::view::{bold, clip, fg, row_bg, sp};

/// 计划块最多列几步（照 Codex 的计划样式）。
const PLAN_STEPS: usize = 5;

/// 对话行下面那一行：和标题对齐，整行压暗；在跑的工具名稍亮一档，失败用红色。
#[allow(clippy::too_many_arguments)]
pub(super) fn doing_row(
    buf: &mut Buffer,
    area: Rect,
    app: &App,
    e: &Entry,
    doing: &Doing,
    selected: bool,
    t: &Theme,
    accent: Color,
) {
    row_bg(buf, area, selected, app.flash(&e.session.id), t, accent);
    // 动态从标题那一列（第 4 格）起。
    let r = Rect {
        x: area.x + 4,
        width: area.width.saturating_sub(5),
        ..area
    };
    let spans = match doing {
        Doing::Tool { name, input } => vec![
            sp(name.clone(), fg(t.muted)),
            sp(" · ", fg(t.faint)),
            sp(input.clone(), fg(t.faint)),
        ],
        Doing::Said(s) => vec![sp(s.clone(), fg(t.faint))],
        Doing::Failed(reason) => {
            let mut v = vec![sp("失败", fg(t.alert()))];
            if let Some(r) = reason_text(reason) {
                v.push(sp(format!(" · {r}"), fg(t.alert())));
            }
            v
        }
    };
    Line::from(clip(spans, usize::from(r.width))).render(buf, r);
}

/// 列表行尾的计划进度「2/5」（只给有动态的对话：停了很久的旧计划不算数）。
pub(super) fn plan_cell(app: &App, e: &Entry, t: &Theme) -> Vec<Span<'static>> {
    let Some((done, total)) = app
        .active(e)
        .then(|| app.moments.get(&e.session.id))
        .flatten()
        .and_then(Moment::plan_progress)
    else {
        return Vec::new();
    };
    vec![sp(format!("{done}/{total}"), fg(t.muted))]
}

/// 预览标题行里这条对话此刻的状态和颜色；没托管、也不在别处跑的为 `None`（照旧写「对话预览」）。
pub(super) fn head_status(app: &App, e: &Entry, t: &Theme) -> Option<(String, Color)> {
    let m = app.moments.get(&e.session.id);
    let since = |from| app.now.duration_since(from).unwrap_or(Duration::ZERO);
    if app.failed(e) {
        let reason = m.and_then(|m| m.failed.as_deref()).and_then(reason_text);
        let text = reason.map_or_else(|| "失败".to_owned(), |r| format!("失败 · {r}"));
        return Some((text, t.alert()));
    }
    if app.busy(e) {
        let label = if app.elsewhere_busy(e) {
            "别处在跑"
        } else {
            "在跑"
        };
        let color = if app.elsewhere_busy(e) {
            t.elsewhere()
        } else {
            t.state(SessionState::Running)
        };
        let text = match m.and_then(|m| m.turn_started) {
            Some(at) => format!("{label} · {}", clock(since(at))),
            None => label.to_owned(),
        };
        return Some((text, color));
    }
    match app.state_of(&e.session.id) {
        SessionState::Waiting => Some((
            format!("等你回复 · {}", span(since(e.session.last_active))),
            t.state(SessionState::Waiting),
        )),
        SessionState::Done => {
            let text = match m.and_then(|m| m.duration) {
                Some(d) => format!("做完 · 用时 {}", span(d)),
                None => "做完".to_owned(),
            };
            Some((text, t.state(SessionState::Done)))
        }
        _ => None,
    }
}

/// 预览里消息上面的计划块：标题「计划 2/5」，最多 [`PLAN_STEPS`] 步，从正在做的前一步起。
/// 做完的 `✔` 压暗加删除线，正在做的 `□` 强调色加粗，没做的 `□` 压暗。没有计划返回空。
pub(super) fn plan_lines(m: &Moment, w: usize, t: &Theme, accent: Color) -> Vec<Line<'static>> {
    let Some((done, total)) = m.plan_progress() else {
        return Vec::new();
    };
    let current = m
        .plan
        .iter()
        .position(|s| s.status != StepStatus::Done)
        .unwrap_or(total);
    let start = current
        .saturating_sub(1)
        .min(total.saturating_sub(PLAN_STEPS));
    let mut out = vec![Line::from(vec![
        sp("计划 ", bold(t.muted)),
        sp(format!("{done}/{total}"), bold(t.text)),
    ])];
    for s in m.plan.iter().skip(start).take(PLAN_STEPS) {
        let (mark, style) = match s.status {
            StepStatus::Done => ("✔", fg(t.faint).add_modifier(Modifier::CROSSED_OUT)),
            StepStatus::Doing => ("□", bold(accent)),
            StepStatus::Todo => ("□", fg(t.faint)),
        };
        let mark_style = if s.status == StepStatus::Done {
            fg(t.faint)
        } else {
            style
        };
        out.push(Line::from(vec![
            sp(format!("{mark} "), mark_style),
            sp(truncate(&s.step, w.saturating_sub(2)), style),
        ]));
    }
    out
}

/// 失败原因的说法：Codex 用户按 Esc 打断是 `interrupted`；空串不写。
fn reason_text(reason: &str) -> Option<String> {
    match reason.trim() {
        "" => None,
        "interrupted" => Some("被打断".to_owned()),
        r => Some(truncate(r, 40)),
    }
}

trait Render {
    fn render(self, buf: &mut Buffer, area: Rect);
}

impl Render for Line<'_> {
    fn render(self, buf: &mut Buffer, area: Rect) {
        ratatui::widgets::Widget::render(self, area, buf);
    }
}
