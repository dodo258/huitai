//! `$` 额度面板：按账号一组，每组先写这个账号的额度（Claude、Codex、pi），再一行一个 provider 写余额或订阅额度
//! （额度窗口带重置倒计时）；
//! 环境变量里的密钥单独一组。比例用 LineGauge 画（80% 黄、95% 红），没有公开文档的接口在行尾标出来。
//! 另有顶栏用的 [`top_spans`]：pi / Hermes 显示它默认 provider 的读数。

use std::time::SystemTime;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{LineGauge, Widget};

use super::app::App;
use super::text::{ago, truncate, width};
use super::theme::Theme;
use super::view::{QUOTA_ALERT, QUOTA_WARN, bold, centered, fg, fill, reset_note, row, sp, tokens};
use crate::core::{Account, QuotaValue, QuotaWindow, SessionState, Vendor};
use crate::quota::{self, Balance, BalanceRow, BalanceValue, Money, OriginAccount};

/// 一行里的一段：文字，重置倒计时（放不下时最先让出来），或一条 LineGauge（前面带窗口名）。
enum Cell {
    Text(Span<'static>),
    Reset(Span<'static>),
    Gauge {
        label: String,
        ratio: f64,
        color: Color,
    },
}

/// LineGauge 的进度条画多宽（不含前面的窗口名）。
const BAR: u16 = 10;

impl Cell {
    fn width(&self) -> u16 {
        match self {
            Cell::Text(s) | Cell::Reset(s) => width(&s.content) as u16,
            Cell::Gauge { label, .. } => width(label) as u16 + 1 + BAR,
        }
    }
}

/// 面板的一行：左边几段，右边一段暗字（来源、非公开接口提示）。
struct PanelLine {
    cells: Vec<Cell>,
    right: Option<Span<'static>>,
}

fn text(s: Span<'static>) -> PanelLine {
    PanelLine {
        cells: vec![Cell::Text(s)],
        right: None,
    }
}

/// 用了多少（0–100）对应的颜色：80% 起黄、95% 起红，否则 `normal`。
fn level(t: &Theme, used: f64, normal: Color) -> Color {
    match used as f32 {
        u if u >= QUOTA_ALERT => t.alert(),
        u if u >= QUOTA_WARN => t.state(SessionState::Waiting),
        _ => normal,
    }
}

/// 每个窗口：进度条、百分比；`now` 给了就在后面写重置倒计时（[`reset_note`]）。
fn window_cells(
    ws: &[QuotaWindow],
    t: &Theme,
    accent: Color,
    now: Option<SystemTime>,
) -> Vec<Cell> {
    let mut out = Vec::new();
    for (i, w) in ws.iter().enumerate() {
        if i > 0 {
            out.push(Cell::Text(sp("   ", Style::default())));
        }
        let used = f64::from(w.used_percent.clamp(0.0, 100.0));
        out.push(Cell::Gauge {
            label: w.label.clone(),
            ratio: used / 100.0,
            color: level(t, used, accent),
        });
        out.push(Cell::Text(sp(
            format!(" {used:.0}%"),
            fg(level(t, used, t.text)),
        )));
        if let Some(note) = now.and_then(|now| reset_note(w, now)) {
            out.push(Cell::Reset(sp(format!(" · {note}"), fg(t.faint))));
        }
    }
    out
}

fn currency(c: &str) -> &str {
    match c {
        "CNY" => "¥",
        "USD" => "$",
        other => other,
    }
}

fn money(m: &Money) -> String {
    let sign = if m.amount < 0.0 { "-" } else { "" };
    format!("{sign}{}{:.2}", currency(&m.currency), m.amount.abs())
}

/// 余额、额度的文字（顶栏和面板共用）；欠费、余额不够调用的标红。`now` 见 [`window_cells`]。
fn value_cells(v: &BalanceValue, t: &Theme, accent: Color, now: Option<SystemTime>) -> Vec<Cell> {
    match v {
        BalanceValue::Money { amounts, available } => {
            let color = if *available { t.title } else { t.alert() };
            let mut text = amounts.iter().map(money).collect::<Vec<_>>().join(" · ");
            if !available {
                let owed = amounts.iter().any(|m| m.amount < 0.0);
                text.push_str(if owed {
                    "  欠费"
                } else {
                    "  余额不够调用"
                });
            }
            vec![Cell::Text(sp(text, bold(color)))]
        }
        BalanceValue::Credit {
            used, currency: c, ..
        } => {
            let c = currency(c);
            match v.credit_period() {
                Some(p) => {
                    let pct = p.ratio() * 100.0;
                    let (left, l) = (p.left, p.limit);
                    vec![
                        Cell::Gauge {
                            label: "限额".to_owned(),
                            ratio: p.ratio(),
                            color: level(t, pct, accent),
                        },
                        Cell::Text(sp(format!(" 剩 {c}{left:.2}"), fg(level(t, pct, t.text)))),
                        Cell::Text(sp(format!(" / {c}{l:.2}"), fg(t.muted))),
                    ]
                }
                None => vec![
                    Cell::Text(sp("已用 ", fg(t.muted))),
                    Cell::Text(sp(format!("{c}{used:.2}"), fg(t.text))),
                    Cell::Text(sp("  不限额", fg(t.faint))),
                ],
            }
        }
        BalanceValue::Windows(ws) => window_cells(ws, t, accent, now),
    }
}

/// 一个 provider 一行：名字、读数（或取不到的原因）、右边标非公开接口或密钥在哪。
fn provider_line(r: &BalanceRow, now: SystemTime, t: &Theme, accent: Color) -> PanelLine {
    let mut cells = vec![Cell::Text(sp(name_col(r.provider.name()), fg(t.muted)))];
    match &r.result {
        Ok(b) => cells.extend(value_cells(&b.value, t, accent, Some(now))),
        Err(e) => cells.push(Cell::Text(sp(format!("取不到：{e}"), fg(t.faint)))),
    }
    let right = if r.provider.unofficial() {
        "非公开接口，可能失效".to_owned()
    } else {
        r.origins[0].place.clone()
    };
    PanelLine {
        cells,
        right: Some(sp(right, fg(t.faint))),
    }
}

fn from_account(r: &BalanceRow, a: &Account) -> bool {
    matches!(&r.origins[0].account, Some(OriginAccount { vendor, key, .. }) if *vendor == a.vendor && *key == a.key)
}

/// 面板全部内容（不含标题和底部提示）。
fn panel_lines(app: &App, t: &Theme) -> Vec<PanelLine> {
    let mut out: Vec<PanelLine> = Vec::new();
    let report = app.balance.as_ref();
    let rows: Vec<&BalanceRow> = report.map(|r| r.rows.iter().collect()).unwrap_or_default();
    for a in &app.accounts {
        let mine: Vec<&&BalanceRow> = rows.iter().filter(|r| from_account(r, a)).collect();
        let has_quota = quota::supported(a.vendor);
        if mine.is_empty() && !(has_quota && app.shown.contains(&a.vendor)) {
            continue;
        }
        let accent = t.accent(a.vendor);
        if !out.is_empty() {
            out.push(text(sp("", Style::default())));
        }
        out.push(PanelLine {
            cells: vec![
                Cell::Text(sp(
                    format!("{} {}", a.vendor.mark(), a.vendor.name()),
                    bold(accent),
                )),
                Cell::Text(sp(format!(" · {}", a.name), fg(t.muted))),
            ],
            right: None,
        });
        if has_quota {
            let mut cells = vec![Cell::Text(sp(name_col("额度"), fg(t.muted)))];
            let reading = app.quota.get(&(a.vendor, a.key.clone()));
            let right = reading
                .filter(|r| r.value != QuotaValue::Unavailable)
                .map(|r| {
                    sp(
                        format!("{} · {}", r.source, ago(r.at, app.now)),
                        fg(t.faint),
                    )
                });
            cells.extend(match reading.map(|r| &r.value) {
                None => vec![Cell::Text(sp("读取中…", fg(t.faint)))],
                Some(QuotaValue::Unavailable) => vec![Cell::Text(sp("暂时取不到", fg(t.faint)))],
                Some(QuotaValue::TokensToday(n)) => vec![
                    Cell::Text(sp("今天 ", fg(t.muted))),
                    Cell::Text(sp(tokens(*n), fg(t.text))),
                    Cell::Text(sp(" token", fg(t.muted))),
                ],
                Some(QuotaValue::Windows(ws)) => window_cells(ws, t, accent, Some(app.now)),
            });
            out.push(PanelLine { cells, right });
        }
        out.extend(
            mine.into_iter()
                .map(|r| provider_line(r, app.now, t, accent)),
        );
    }
    let env: Vec<&&BalanceRow> = rows
        .iter()
        .filter(|r| r.origins[0].account.is_none())
        .collect();
    if !env.is_empty() {
        if !out.is_empty() {
            out.push(text(sp("", Style::default())));
        }
        out.push(text(sp("环境变量", bold(t.muted))));
        out.extend(
            env.into_iter()
                .map(|r| provider_line(r, app.now, t, t.title)),
        );
    }
    match report {
        None => {
            out.push(text(sp("", Style::default())));
            out.push(text(sp("余额读取中…", fg(t.faint))));
        }
        Some(r) => {
            if r.rows.is_empty() {
                out.push(text(sp("", Style::default())));
                out.push(text(sp(
                    "没找到 DeepSeek、Moonshot、OpenRouter、Kimi Code 的密钥",
                    fg(t.faint),
                )));
                out.push(text(sp(
                    "只读 pi 的 auth.json、models.json，Hermes 的 .env 和环境变量",
                    fg(t.faint),
                )));
            }
            if !r.skipped.is_empty() {
                out.push(text(sp("", Style::default())));
            }
            for s in &r.skipped {
                let who = s.origin.account.as_ref().map_or_else(String::new, |a| {
                    format!("{} {} · ", a.vendor.name(), a.name)
                });
                out.push(text(sp(
                    format!("跳过 {who}{}：{}", s.origin.place, s.reason),
                    fg(t.faint),
                )));
            }
        }
    }
    out
}

/// 名字那一列：缩进两格，按显示宽度补齐到 [`NAME_W`]。
fn name_col(name: &str) -> String {
    format!("  {name}{}", " ".repeat(NAME_W.saturating_sub(width(name))))
}

/// 名字列宽：最长的「Moonshot 国际」13 格，再空两格。
const NAME_W: usize = 15;

fn draw_line(buf: &mut Buffer, area: Rect, mut line: PanelLine, t: &Theme) {
    let cells_w = |cells: &[Cell]| cells.iter().map(Cell::width).sum::<u16>();
    // 放不下时先从后往前去掉重置倒计时，保住右边的暗字（「非公开接口，可能失效」、来源）；
    // 倒计时都去掉了还放不下，右边的暗字才不放，先保住读数。
    if let Some(right_w) = line.right.as_ref().map(|s| width(&s.content) as u16) {
        while cells_w(&line.cells) + 2 + right_w > area.width {
            let Some(i) = line.cells.iter().rposition(|c| matches!(c, Cell::Reset(_))) else {
                break;
            };
            line.cells.remove(i);
        }
    }
    let left_w = cells_w(&line.cells);
    let right = line
        .right
        .filter(|s| left_w + 2 + width(&s.content) as u16 <= area.width);
    let right_w = right.as_ref().map_or(0, |s| width(&s.content) as u16);
    let mut x = area.x;
    let limit = area
        .right()
        .saturating_sub(if right_w > 0 { right_w + 2 } else { 0 });
    for cell in line.cells {
        let w = cell.width();
        if x >= limit {
            break;
        }
        let room = limit - x;
        match cell {
            Cell::Text(s) | Cell::Reset(s) => {
                let clipped = truncate(&s.content, usize::from(room));
                buf.set_string(x, area.y, &clipped, s.style);
            }
            Cell::Gauge {
                label,
                ratio,
                color,
            } => {
                let r = Rect {
                    x,
                    width: w.min(room),
                    ..area
                };
                LineGauge::default()
                    .ratio(ratio.clamp(0.0, 1.0))
                    .label(Line::from(sp(label, fg(t.muted))))
                    .filled_symbol("━")
                    .unfilled_symbol("━")
                    .filled_style(fg(color))
                    .unfilled_style(fg(t.select))
                    .render(r, buf);
            }
        }
        x += w;
    }
    if let Some(s) = right {
        row(buf, area, vec![], vec![s]);
    }
}

/// 面板。内容比框高时 ↑↓ 翻，`scroll` 超出范围按最后一屏算。
pub fn draw(buf: &mut Buffer, area: Rect, app: &App, scroll: usize, t: &Theme) {
    let lines = panel_lines(app, t);
    let want = lines.len() as u16 + 4;
    // 最宽 124：两个额度窗口各带重置倒计时（最长「6 天 23 小时后重置」），右边的「非公开接口，可能失效」
    // 也放得下（要 117 列）；终端窄了跟着收，放不下时倒计时先让（见 draw_line）。
    let boxr = centered(area, 124, want + 2);
    fill(buf, boxr, t.bar);
    let inner = Rect {
        x: boxr.x + 3,
        y: boxr.y + 1,
        width: boxr.width.saturating_sub(6),
        height: boxr.height.saturating_sub(2),
    };
    if inner.height < 3 {
        return;
    }
    let line = |i: u16| Rect {
        y: inner.y + i,
        height: 1,
        ..inner
    };
    row(
        buf,
        line(0),
        vec![sp("额度和余额", bold(t.title))],
        vec![sp("每 5 分钟更新", fg(t.faint))],
    );
    let body = inner.height - 3;
    let max_scroll = lines.len().saturating_sub(usize::from(body));
    let scroll = scroll.min(max_scroll);
    for (i, l) in lines
        .into_iter()
        .skip(scroll)
        .take(usize::from(body))
        .enumerate()
    {
        draw_line(buf, line(2 + i as u16), l, t);
    }
    let hint = if max_scroll > 0 {
        "↑↓ 翻 · $ 或 Esc 关闭"
    } else {
        "$ 或 Esc 关闭"
    };
    row(
        buf,
        line(inner.height - 1),
        vec![sp(hint, fg(t.faint))],
        vec![],
    );
}

/// 纯文字写法（`ai doctor` 用）：`¥36.42`、`已用 $7.50 / $20.00`、`5 小时 已用 83% · 周 已用 31%`。
pub fn plain(v: &BalanceValue) -> String {
    match v {
        BalanceValue::Money { amounts, available } => {
            let mut s = amounts.iter().map(money).collect::<Vec<_>>().join(" · ");
            if !available {
                let owed = amounts.iter().any(|m| m.amount < 0.0);
                s.push_str(if owed {
                    " 欠费"
                } else {
                    " 余额不够调用"
                });
            }
            s
        }
        BalanceValue::Credit {
            used, currency: c, ..
        } => {
            let c = currency(c);
            match v.credit_period() {
                Some(p) if p.left <= 0.0 => {
                    format!("已用 {c}{:.2} / {c}{:.2}（用完了）", p.used, p.limit)
                }
                Some(p) => format!("已用 {c}{:.2} / {c}{:.2}", p.used, p.limit),
                None => format!("已用 {c}{used:.2}（不限额）"),
            }
        }
        BalanceValue::Windows(ws) => ws
            .iter()
            .map(|w| format!("{} 已用 {:.0}%", w.label, w.used_percent))
            .collect::<Vec<_>>()
            .join(" · "),
    }
}

/// 顶栏（pi / Hermes）：默认 provider 的读数。`with_source` 时后面跟「provider · 几分钟前」，
/// `with_reset` 时额度窗口后面写重置倒计时。
pub fn top_spans(
    row: &BalanceRow,
    b: &Balance,
    now: SystemTime,
    vendor: Vendor,
    t: &Theme,
    with_source: bool,
    with_reset: bool,
) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let reset_now = with_reset.then_some(now);
    for c in value_cells(&b.value, t, t.accent(vendor), reset_now) {
        match c {
            Cell::Text(s) | Cell::Reset(s) => out.push(s),
            // 顶栏只有一行窄地方，进度条用字符画（和额度一样 8 格）。
            Cell::Gauge {
                label,
                ratio,
                color,
            } => {
                let filled = (ratio * 8.0).round() as usize;
                out.push(sp(format!("{label} "), fg(t.muted)));
                out.push(sp("━".repeat(filled), fg(color)));
                out.push(sp("━".repeat(8 - filled.min(8)), fg(t.select)));
            }
        }
    }
    if with_source {
        out.push(sp(
            format!("  {} · {}", row.provider.name(), ago(b.at, now)),
            fg(t.faint),
        ));
    }
    out
}
