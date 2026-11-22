//! 整段对话的只读滚动视图：在对话上按 `v`（或 `→`，搜索结果里也是 `→`）打开，盖住列表和预览两栏，
//! 顶栏、底栏照旧。工具调用默认折成一行（`t` 展开），从搜索结果打开时高亮搜索词、`n`/`N` 在出现处之间跳。
//! 全文由运行层读好喂进来（[`Reader::take_request`] / [`Reader::set`]），这里只管状态、按键和排版。

use std::time::SystemTime;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::core::Vendor;
use crate::index::Entry;
use crate::sources::{Part, Role, TRANSCRIPT_MAX_BYTES, Transcript};

use super::text::{ago, truncate, width, wrap};
use super::theme::{Theme, mix};
use super::view::{bold, clip, fg, fill, put_lines, row, sp};

/// 正文最宽多少格：再宽一行太长，读着累。
const MAX_TEXT_WIDTH: usize = 110;
/// 展开工具调用时，每次返回最多显示几行。
const RESULT_LINES: usize = 6;
/// 跳到搜索词时，它上面留几行上下文。
const HIT_CONTEXT: usize = 2;

/// 按键的结果：大多是在视图里滚动，少数要 App 去做。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    Close,
    /// 打开这条对话：`here` 为真在当前窗口。
    Open {
        here: bool,
    },
    Quit,
}

pub struct Reader {
    pub entry: Box<Entry>,
    /// 读到的全文，和读的时候对话的最后时间；`None` 是还在读。
    pub data: Option<(SystemTime, Result<Transcript, String>)>,
    /// 已经要过哪个时间的全文（对话又写了新内容、时间变了就再要一次）。
    asked: Option<SystemTime>,
    /// 屏幕第一行是全文的第几行。
    top: usize,
    /// 贴着底：新内容读进来时还停在最后。
    follow: bool,
    /// 工具调用展开着。
    pub tools: bool,
    /// 从搜索结果打开时的搜索词（按空白拆开），高亮、`n`/`N` 跳。
    terms: Vec<String>,
    /// 排好版后跳到第一处搜索词（打开时用一次）。
    jump: bool,
    /// 现在停在第几处搜索词（`n`/`N` 按它走；自己滚动过就清掉，从屏幕位置重新找）。
    hit: Option<usize>,
    /// 重排后要停在第几块上（展开 / 收起工具调用时记下屏幕第一行那一块）。
    anchor: Option<usize>,
    layout: Option<Layout>,
    /// 上次画的时候正文区有几行（翻页用）。
    page: usize,
}

/// 排好的全文（宽度、展开状态、数据、分钟数变了才重排）。
struct Layout {
    key: (usize, bool, SystemTime, u64),
    lines: Vec<Line<'static>>,
    /// 每一行属于第几块（展开 / 收起工具调用后停在同一块上）。
    owner: Vec<usize>,
    /// 有搜索词的行。
    hits: Vec<usize>,
    messages: usize,
    tool_calls: usize,
}

impl Reader {
    /// 打开 `entry` 的全文；`query` 是搜索词（不是从搜索结果打开就是空的）。没有搜索词时从最后看起。
    pub fn new(entry: Entry, query: &str) -> Self {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        Self {
            entry: Box::new(entry),
            data: None,
            asked: None,
            top: 0,
            follow: terms.is_empty(),
            tools: false,
            jump: !terms.is_empty(),
            anchor: None,
            hit: None,
            terms,
            layout: None,
            page: 1,
        }
    }

    /// 对话现在的最后时间是 `at`（运行层从最新列表里查的）：还没读过这个时间的全文就返回要读的对话，一个时间只要一次。
    pub fn take_request(&mut self, at: SystemTime) -> Option<Entry> {
        let fresh = self.data.as_ref().is_some_and(|(t, _)| *t == at);
        if fresh || self.asked == Some(at) {
            return None;
        }
        self.asked = Some(at);
        let mut e = (*self.entry).clone();
        e.session.last_active = at;
        Some(e)
    }

    /// 全文读到了。贴着底的继续贴底；不然停在原来那一行。
    pub fn set(&mut self, at: SystemTime, data: Result<Transcript, String>) {
        self.entry.session.last_active = at;
        self.data = Some((at, data));
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.page.max(2) - 1;
        match key.code {
            KeyCode::Char('c') if ctrl => return Outcome::Quit,
            KeyCode::Char('d') if ctrl => self.scroll(page as isize / 2),
            KeyCode::Char('u') if ctrl => self.scroll(-(page as isize / 2)),
            _ if ctrl => {}
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('q' | 'h' | 'v') => {
                return Outcome::Close;
            }
            KeyCode::Enter => {
                return Outcome::Open {
                    here: key.modifiers.contains(KeyModifiers::ALT),
                };
            }
            KeyCode::Char('w') => return Outcome::Open { here: true },
            KeyCode::Up | KeyCode::Char('k') => self.scroll(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll(1),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll(-(page as isize)),
            KeyCode::PageDown | KeyCode::Char(' ' | 'f') => self.scroll(page as isize),
            KeyCode::Home | KeyCode::Char('g') => {
                self.top = 0;
                self.follow = false;
                self.hit = None;
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.follow = true;
                self.hit = None;
            }
            KeyCode::Char('t') => self.toggle_tools(),
            KeyCode::Char('n') => self.next_hit(true),
            KeyCode::Char('N') => self.next_hit(false),
            _ => {}
        }
        Outcome::Stay
    }

    /// 往下（正数）或往上滚几行。滚到底就贴底。
    pub fn scroll(&mut self, delta: isize) {
        self.hit = None;
        let max = self.max_top();
        let from = if self.follow { max } else { self.top.min(max) };
        self.top = from.saturating_add_signed(delta).min(max);
        self.follow = self.top >= max;
    }

    fn max_top(&self) -> usize {
        self.layout
            .as_ref()
            .map_or(0, |l| l.lines.len().saturating_sub(self.page))
    }

    /// 展开 / 收起工具调用，停在原来屏幕第一行那一块上。
    fn toggle_tools(&mut self) {
        self.tools = !self.tools;
        if self.follow {
            return;
        }
        let anchor = self
            .layout
            .as_ref()
            .and_then(|l| l.owner.get(self.top).copied());
        self.layout = None;
        self.anchor = anchor;
        self.hit = None;
    }

    /// 跳到下一处（`forward`）或上一处搜索词，到头了绕回来。
    fn next_hit(&mut self, forward: bool) {
        let Some(l) = &self.layout else {
            return;
        };
        if l.hits.is_empty() {
            return;
        }
        let n = l.hits.len();
        let i = match self.hit {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            // 自己滚动过：从屏幕第一行（加上下文）往后 / 往前找，找不到绕回。
            None => {
                let at = if self.follow {
                    self.max_top()
                } else {
                    self.top
                } + HIT_CONTEXT;
                if forward {
                    l.hits.iter().position(|&h| h > at).unwrap_or(0)
                } else {
                    l.hits.iter().rposition(|&h| h < at).unwrap_or(n - 1)
                }
            }
        };
        let line = l.hits[i];
        self.show_line(line);
        self.hit = Some(i);
    }

    fn show_line(&mut self, line: usize) {
        self.top = line.saturating_sub(HIT_CONTEXT).min(self.max_top());
        self.follow = self.top >= self.max_top() && self.max_top() > 0;
    }

    /// 屏幕上能看到的是第几行到第几行（从 1 数）、一共几行；还没排版是 `None`。
    pub fn position(&self) -> Option<(usize, usize, usize)> {
        let l = self.layout.as_ref()?;
        let total = l.lines.len();
        let top = if self.follow {
            self.max_top()
        } else {
            self.top.min(self.max_top())
        };
        Some(((top + 1).min(total), (top + self.page).min(total), total))
    }

    /// 画在列表和预览两栏的位置：第一行是标题带，下面是正文。
    pub fn draw(&mut self, buf: &mut Buffer, area: Rect, now: SystemTime, t: &Theme) {
        let accent = t.accent(self.entry.session.vendor);
        let head = Rect { height: 1, ..area };
        let body = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        fill(buf, body, t.preview);
        let pad = |r: Rect| Rect {
            x: r.x + 3,
            width: r.width.saturating_sub(6),
            ..r
        };
        let inner = Rect {
            y: body.y + 1,
            height: body.height.saturating_sub(1),
            ..pad(body)
        };
        let w = usize::from(inner.width).min(MAX_TEXT_WIDTH);
        self.page = usize::from(inner.height).max(1);

        let lines = match &self.data {
            None => Some(vec![Line::from(sp("读取中…", fg(t.faint)))]),
            Some((_, Err(e))) => Some(vec![Line::from(sp(
                truncate(&format!("读不了：{e}"), w.max(1)),
                fg(t.faint),
            ))]),
            Some((_, Ok(tr))) if tr.parts.is_empty() => {
                Some(vec![Line::from(sp("这条对话还没有内容", fg(t.faint)))])
            }
            Some(_) => None,
        };
        if let Some(lines) = lines {
            self.layout = None;
            self.head(buf, head, t);
            put_lines(buf, inner, lines);
            return;
        }
        self.relayout(w, now, t, accent);
        self.head(buf, head, t);
        let Some(l) = &self.layout else {
            return;
        };
        let max = l.lines.len().saturating_sub(self.page);
        self.top = if self.follow { max } else { self.top.min(max) };
        let shown = l.lines[self.top..]
            .iter()
            .take(self.page)
            .cloned()
            .collect();
        put_lines(buf, inner, shown);
    }

    /// 排版（需要时）。打开时有搜索词就跳到第一处；展开 / 收起工具调用后回到原来那一块。
    fn relayout(&mut self, w: usize, now: SystemTime, t: &Theme, accent: Color) {
        let Some((at, Ok(tr))) = &self.data else {
            return;
        };
        let minute = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() / 60);
        let key = (w, self.tools, *at, minute);
        if self.layout.as_ref().is_some_and(|l| l.key == key) {
            return;
        }
        let vendor = self.entry.session.vendor;
        let mut l = layout(tr, vendor, w, self.tools, &self.terms, now, t, accent);
        l.key = key;
        let anchor = self.anchor.take();
        self.layout = Some(l);
        if std::mem::take(&mut self.jump) {
            if let Some(&h) = self.layout.as_ref().and_then(|l| l.hits.first()) {
                self.show_line(h);
                self.hit = Some(0);
            } else {
                self.follow = true;
            }
        } else if let Some(a) = anchor
            && let Some(i) = self
                .layout
                .as_ref()
                .and_then(|l| l.owner.iter().position(|&o| o >= a))
        {
            self.top = i.min(self.max_top());
        }
    }

    fn head(&self, buf: &mut Buffer, area: Rect, t: &Theme) {
        fill(buf, area, t.bar);
        let r = Rect {
            x: area.x + 3,
            width: area.width.saturating_sub(5),
            ..area
        };
        let left = vec![
            sp("对话全文  ", fg(t.muted)),
            sp(self.entry.title().to_owned(), bold(t.title)),
        ];
        let mut right = Vec::new();
        if let Some(l) = &self.layout {
            let mut s = format!("{} 条消息", l.messages);
            if l.tool_calls > 0 {
                s.push_str(&format!(" · 工具调用 {} 次", l.tool_calls));
                if !self.tools {
                    s.push_str("（已折叠）");
                }
            }
            right.push(sp(s, fg(t.faint)));
        }
        row(buf, r, left, right);
    }

    /// 底栏：能按的键（和列表底栏一样的写法），右边写看到第几行。
    pub fn bottom(&self, buf: &mut Buffer, area: Rect, status: Option<&str>, t: &Theme) {
        let accent = t.accent(self.entry.session.vendor);
        let mut keys = vec![("↑↓", "滚动"), ("空格", "翻页"), ("g G", "顶 / 底")];
        if self.layout.as_ref().is_some_and(|l| l.tool_calls > 0) {
            keys.push((
                "t",
                if self.tools {
                    "收起工具"
                } else {
                    "展开工具"
                },
            ));
        }
        if self.layout.as_ref().is_some_and(|l| !l.hits.is_empty()) {
            keys.push(("n N", "下一处 / 上一处"));
        }
        keys.extend([("⏎", "接着聊"), ("Esc", "返回")]);
        let left = keys
            .into_iter()
            .flat_map(|(k, label)| [sp(k, bold(accent)), sp(format!(" {label}   "), fg(t.muted))])
            .collect();
        let right = match (status, self.position()) {
            (Some(s), _) => vec![sp(s.to_owned(), fg(t.text))],
            (None, Some((a, b, n))) => vec![sp(format!("{a}–{b} / {n} 行"), fg(t.faint))],
            (None, None) => Vec::new(),
        };
        row(buf, area, left, right);
    }
}

/// 把全文排成行：消息是「谁 · 多久前」加折好的正文；连着的工具调用和返回合成一组，
/// 收起时一行「▸ 工具 ×N  名字 · 名字」，展开时每次调用一行、返回最多 [`RESULT_LINES`] 行。
#[allow(clippy::too_many_arguments)]
fn layout(
    tr: &Transcript,
    vendor: Vendor,
    w: usize,
    tools: bool,
    terms: &[String],
    now: SystemTime,
    t: &Theme,
    accent: Color,
) -> Layout {
    let w = w.max(8);
    let mut l = Layout {
        key: (0, false, SystemTime::UNIX_EPOCH, 0),
        lines: Vec::new(),
        owner: Vec::new(),
        hits: Vec::new(),
        messages: 0,
        tool_calls: 0,
    };
    let hit = Style::default()
        .fg(t.title)
        .bg(mix(t.preview, accent, 0.35))
        .add_modifier(Modifier::BOLD);
    let push = |l: &mut Layout, owner: usize, line: Line<'static>, has_hit: bool| {
        if has_hit {
            l.hits.push(l.lines.len());
        }
        l.lines.push(line);
        l.owner.push(owner);
    };
    if tr.truncated {
        let mb = TRANSCRIPT_MAX_BYTES / (1024 * 1024);
        for s in wrap(
            &format!("文件太大，只读了最后 {mb} MB；更早的内容按 ⏎ 接着聊后在对话里往上翻"),
            w,
        ) {
            push(&mut l, 0, Line::from(sp(s, fg(t.faint))), false);
        }
    }
    let parts = &tr.parts;
    let mut i = 0;
    while i < parts.len() {
        if !l.lines.is_empty() {
            push(&mut l, i, Line::default(), false);
        }
        if let Part::Message(m) = &parts[i] {
            l.messages += 1;
            let who = match m.role {
                Role::User => sp("你", bold(t.muted)),
                Role::Assistant => sp(vendor.name(), bold(accent)),
            };
            let mut head = vec![who];
            if let Some(at) = m.at {
                head.push(sp(format!("  {}", ago(at, now)), fg(t.faint)));
            }
            push(&mut l, i, Line::from(head), false);
            for s in wrap(&m.text, w) {
                let (spans, found) = highlight(&s, terms, fg(t.text), hit);
                push(&mut l, i, Line::from(spans), found);
            }
            i += 1;
            continue;
        }
        // 连着的一组工具调用和返回。
        let end = parts[i..]
            .iter()
            .position(|p| matches!(p, Part::Message(_)))
            .map_or(parts.len(), |n| i + n);
        let group = &parts[i..end];
        let calls: Vec<(&str, &str)> = group
            .iter()
            .filter_map(|p| match p {
                Part::ToolCall { name, input } => Some((name.as_str(), input.as_str())),
                _ => None,
            })
            .collect();
        l.tool_calls += calls.len();
        let mut names: Vec<&str> = Vec::new();
        for (n, _) in &calls {
            if !names.contains(n) {
                names.push(n);
            }
        }
        let mark = if tools { "▾" } else { "▸" };
        let head = vec![
            sp(
                format!("{mark} 工具 ×{}  ", calls.len().max(1)),
                fg(t.muted),
            ),
            sp(names.join(" · "), fg(t.faint)),
        ];
        push(&mut l, i, Line::from(clip(head, w)), false);
        if tools {
            for (k, p) in group.iter().enumerate() {
                let owner = i + k;
                match p {
                    Part::ToolCall { name, input } => {
                        let room = w.saturating_sub(width(name) + 4);
                        let (mut spans, found) =
                            highlight(&truncate(input, room.max(1)), terms, fg(t.faint), hit);
                        spans.insert(0, sp(format!("  {name}  "), fg(t.muted)));
                        push(&mut l, owner, Line::from(spans), found);
                    }
                    Part::ToolResult { text } => {
                        let wrapped = wrap(text, w.saturating_sub(4));
                        let more = wrapped.len().saturating_sub(RESULT_LINES);
                        for s in wrapped.into_iter().take(RESULT_LINES) {
                            push(
                                &mut l,
                                owner,
                                Line::from(sp(format!("    {s}"), fg(t.faint))),
                                false,
                            );
                        }
                        if more > 0 {
                            push(
                                &mut l,
                                owner,
                                Line::from(sp(format!("    …还有 {more} 行"), fg(t.faint))),
                                false,
                            );
                        }
                    }
                    Part::Message(_) => {}
                }
            }
        }
        i = end;
    }
    l
}

/// 一行字里搜索词（已转成小写英文字母）出现的地方换成 `hit` 样式（不分英文大小写），返回各段和有没有出现。
fn highlight(text: &str, terms: &[String], base: Style, hit: Style) -> (Vec<Span<'static>>, bool) {
    if terms.is_empty() {
        return (vec![Span::styled(text.to_owned(), base)], false);
    }
    // 只换 ASCII 字母的大小写，字节位置不变，下标能直接用在原文上。
    let lower = text.to_ascii_lowercase();
    let mut marks: Vec<(usize, usize)> = Vec::new();
    for term in terms.iter().filter(|t| !t.is_empty()) {
        let mut from = 0;
        while let Some(i) = lower[from..].find(term.as_str()) {
            let s = from + i;
            marks.push((s, s + term.len()));
            from = s + term.len();
        }
    }
    if marks.is_empty() {
        return (vec![Span::styled(text.to_owned(), base)], false);
    }
    marks.sort_unstable();
    let mut spans = Vec::new();
    let mut at = 0;
    for (s, e) in marks {
        let s = s.max(at);
        if s >= e {
            continue;
        }
        if s > at {
            spans.push(Span::styled(text[at..s].to_owned(), base));
        }
        spans.push(Span::styled(text[s..e].to_owned(), hit));
        at = e;
    }
    if at < text.len() {
        spans.push(Span::styled(text[at..].to_owned(), base));
    }
    (spans, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_marks_terms_case_insensitively() {
        let base = Style::default();
        let hit = Style::default().add_modifier(Modifier::BOLD);
        let (spans, found) = highlight("修 Login 页和 login 接口", &["login".into()], base, hit);
        assert!(found);
        let marked: Vec<&str> = spans
            .iter()
            .filter(|s| s.style == hit)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(marked, ["Login", "login"]);
        let all: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(all, "修 Login 页和 login 接口");
        // 重叠的词合在一起，不重复出字。
        let (spans, _) = highlight("结算页", &["结算".into(), "算页".into()], base, hit);
        let all: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(all, "结算页");
        assert!(!highlight("别的", &["结算".into()], base, hit).1);
        assert!(!highlight("别的", &[], base, hit).1);
    }
}
