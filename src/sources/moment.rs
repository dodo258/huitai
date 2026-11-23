//! 「此刻」：一条对话现在在干嘛、上一轮停在哪（列表的动态行、预览标题和计划块用）。
//!
//! 只读会话文件尾部（[`MOMENT_TAIL_BYTES`]），按时间先后把每行拆成 [`Event`] 交给 [`Folder`] 推一遍：
//! 用户发一句话开一轮新的；工具调用没等到返回的就是「在跑的工具」；停下后用厂商自己写的总结
//! （Claude `away_summary`），没有就用最后一条回复的第一句。不调模型生成任何东西。

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::jsonl::{self, str_field};
use super::{Part, Role, window};

/// 只读会话文件最后这么多字节：在跑的工具、最后一句、计划都在最近几轮里。
pub const MOMENT_TAIL_BYTES: u64 = 512 * 1024;
/// 动态行一句话最多保留的字符数。
const LINE_CHARS: usize = 120;
/// 计划每一步最多保留的字符数。
const STEP_CHARS: usize = 80;

/// 一条对话的「此刻」。没有的项留空；整个是空的就不画。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Moment {
    /// 最后一个还没有返回的工具调用（在跑时显示）。
    pub tool: Option<ToolCall>,
    /// 停下时的一句话：厂商写的总结，没有就是最后一条回复的第一句。
    pub summary: Option<String>,
    /// 这一轮从什么时候开始（最后一条用户消息，或厂商记的开始时间）。
    pub turn_started: Option<SystemTime>,
    /// 上一轮用了多久（厂商记的；这一轮还没完时为 `None`）。
    pub duration: Option<Duration>,
    /// 最新的计划（Codex `update_plan`、Claude `TodoWrite`），没有就是空的。
    pub plan: Vec<PlanStep>,
    /// 这一轮失败或被打断了：原因（厂商写的，可能是空串）。
    pub failed: Option<String>,
}

impl Moment {
    /// 计划做完几步、一共几步；没有计划为 `None`。
    pub fn plan_progress(&self) -> Option<(usize, usize)> {
        if self.plan.is_empty() {
            return None;
        }
        let done = self
            .plan
            .iter()
            .filter(|s| s.status == StepStatus::Done)
            .count();
        Some((done, self.plan.len()))
    }
}

/// 在跑的工具：工具名、参数摘要（一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    pub input: String,
}

/// 计划里的一步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub step: String,
    pub status: StepStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Done,
    Doing,
    Todo,
}

impl StepStatus {
    /// Codex `update_plan` 和 Claude `TodoWrite` 都用 `completed` / `in_progress` / `pending`。
    fn parse(s: &str) -> Self {
        match s {
            "completed" | "done" => StepStatus::Done,
            "in_progress" | "active" => StepStatus::Doing,
            _ => StepStatus::Todo,
        }
    }
}

/// 会话文件里一件和「此刻」有关的事，按时间先后。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Event {
    /// 消息、工具调用、工具返回（和整段对话同一种拆法）。没有关联 id 的返回收最早的调用。
    Part(Part),
    /// 带关联 id 的工具调用（Claude `tool_use.id`）。
    ToolCall {
        id: String,
        name: String,
        input: String,
    },
    /// 带关联 id 的工具返回（Claude `tool_result.tool_use_id`）：收掉 id 对上的那个调用，并行工具先返回的也对得上。
    ToolResult {
        id: String,
    },
    /// 厂商写的这一轮的总结（Claude `system/away_summary`）。
    Summary(String),
    /// 新一轮开始了（Codex `task_started`）。
    TurnStarted(Option<SystemTime>),
    /// 这一轮结束了：用时，失败时带原因。
    TurnEnded {
        duration: Option<Duration>,
        failed: Option<String>,
    },
    Plan(Vec<PlanStep>),
}

/// 把事件按先后推一遍，得出「此刻」。
#[derive(Debug, Default)]
pub(crate) struct Folder {
    m: Moment,
    /// 还没等到返回的工具调用和它的关联 id（没有的是 `None`），先调的在前。
    calls: VecDeque<(Option<String>, ToolCall)>,
    reply: Option<String>,
    summary: Option<String>,
    /// 上一件事是厂商记的「这一轮开始」：紧跟着的用户消息是这一轮的开头，不另开一轮。
    started: bool,
}

impl Folder {
    pub(crate) fn push(&mut self, e: Event) {
        let started = std::mem::take(&mut self.started);
        match e {
            // 厂商记了开始时间的，用户消息只是这一轮的头一句（它自己可能没带时间）。
            Event::Part(Part::Message(m)) if m.role == Role::User => {
                let at = if started {
                    self.m.turn_started.or(m.at)
                } else {
                    m.at
                };
                self.new_turn(at);
            }
            Event::Part(Part::Message(m)) => {
                if let Some(s) = first_sentence(&m.text) {
                    self.reply = Some(s);
                }
            }
            Event::Part(Part::ToolCall { name, input }) => self.call(None, name, &input),
            Event::Part(Part::ToolResult { .. }) => _ = self.calls.pop_front(),
            Event::ToolCall { id, name, input } => self.call(Some(id), name, &input),
            // 对不上的（调用在读的尾部之前）不动别的调用。
            Event::ToolResult { id } => self.calls.retain(|(c, _)| c.as_deref() != Some(&id)),
            Event::Summary(s) => {
                let s = jsonl::one_line(&s, LINE_CHARS);
                if !s.is_empty() {
                    self.summary = Some(s);
                }
            }
            Event::TurnStarted(at) => {
                self.new_turn(at);
                self.started = true;
            }
            Event::TurnEnded { duration, failed } => {
                self.calls.clear();
                self.m.duration = duration;
                self.m.failed = failed;
            }
            Event::Plan(steps) => self.m.plan = steps,
        }
    }

    fn call(&mut self, id: Option<String>, name: String, input: &str) {
        let input = jsonl::one_line(input, LINE_CHARS);
        self.calls.push_back((id, ToolCall { name, input }));
    }

    fn new_turn(&mut self, at: Option<SystemTime>) {
        self.calls.clear();
        self.reply = None;
        self.summary = None;
        self.m.turn_started = at;
        self.m.duration = None;
        self.m.failed = None;
    }

    pub(crate) fn finish(mut self) -> Moment {
        self.m.tool = self.calls.pop_back().map(|(_, c)| c);
        self.m.summary = self.summary.or(self.reply);
        self.m
    }
}

/// 整段对话（或它的最后一段）里的块推出来的「此刻」：只有消息和工具，没有厂商的轮次事件。
pub(crate) fn from_parts(parts: impl IntoIterator<Item = Part>) -> Moment {
    let mut f = Folder::default();
    for p in parts {
        f.push(Event::Part(p));
    }
    f.finish()
}

/// jsonl 会话文件最后 [`MOMENT_TAIL_BYTES`] 里的「此刻」：每行交给 `events` 拆成事件。
pub(crate) fn read_jsonl(path: &Path, events: impl Fn(&Value) -> Vec<Event>) -> io::Result<Moment> {
    let (buf, _) = window::read_tail_lines(path, MOMENT_TAIL_BYTES)?;
    let mut f = Folder::default();
    for v in window::lines(&buf).filter_map(jsonl::parse) {
        for e in events(&v) {
            f.push(e);
        }
    }
    Ok(f.finish())
}

/// 把整段对话的块包成事件（只有消息和工具的那几家用）。
pub(crate) fn parts(parts: Vec<Part>) -> Vec<Event> {
    parts.into_iter().map(Event::Part).collect()
}

/// `[{step|content, status}]` 形式的计划（Codex `update_plan.plan`、Claude `TodoWrite.todos`）。
/// 一步文字的键按 `keys` 的顺序找。
pub(crate) fn plan_steps(items: &Value, keys: &[&str]) -> Option<Vec<PlanStep>> {
    let items = items.as_array()?;
    Some(
        items
            .iter()
            .filter_map(|it| {
                let step = keys.iter().find_map(|k| str_field(it, k))?;
                let step = jsonl::one_line(step, STEP_CHARS);
                (!step.is_empty()).then(|| PlanStep {
                    step,
                    status: StepStatus::parse(str_field(it, "status").unwrap_or("")),
                })
            })
            .collect(),
    )
}

/// 一段回复的第一句（折成一行、截短）：到第一个句号、问号、叹号或换行为止。
fn first_sentence(text: &str) -> Option<String> {
    let text = text.trim();
    let end = text
        .char_indices()
        .find_map(|(i, c)| match c {
            '。' | '！' | '？' | '\n' => Some(i + c.len_utf8()),
            '.' | '!' | '?' if text[i + 1..].starts_with(char::is_whitespace) => Some(i + 1),
            _ => None,
        })
        .unwrap_or(text.len());
    let s = jsonl::one_line(text[..end].trim(), LINE_CHARS);
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::PreviewMessage;

    fn msg(role: Role, text: &str, at: u64) -> Event {
        Event::Part(Part::Message(PreviewMessage {
            role,
            text: text.to_owned(),
            at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(at)),
        }))
    }

    fn call(name: &str, input: &str) -> Event {
        Event::Part(Part::ToolCall {
            name: name.to_owned(),
            input: input.to_owned(),
        })
    }

    fn result() -> Event {
        Event::Part(Part::ToolResult {
            text: String::new(),
        })
    }

    fn fold(events: Vec<Event>) -> Moment {
        let mut f = Folder::default();
        for e in events {
            f.push(e);
        }
        f.finish()
    }

    #[test]
    fn pending_tool_is_the_last_call_without_a_result() {
        let m = fold(vec![
            msg(Role::User, "跑测试", 10),
            call("Read", "src/a.rs"),
            result(),
            call("Bash", "cargo test"),
        ]);
        let tool = m.tool.unwrap();
        assert_eq!(
            (tool.name.as_str(), tool.input.as_str()),
            ("Bash", "cargo test")
        );
        assert_eq!(
            m.turn_started,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(10))
        );
    }

    #[test]
    fn results_with_ids_close_their_own_call() {
        let call = |id: &str, name: &str| Event::ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            input: String::new(),
        };
        let done = |id: &str| Event::ToolResult { id: id.to_owned() };
        // 先起慢的、再起快的，快的先返回：在跑的是慢的。
        let m = fold(vec![call("a", "慢"), call("b", "快"), done("b")]);
        assert_eq!(m.tool.map(|t| t.name).as_deref(), Some("慢"));
        // 对不上的返回不收别的调用。
        let m = fold(vec![call("a", "慢"), done("x")]);
        assert_eq!(m.tool.map(|t| t.name).as_deref(), Some("慢"));
        let m = fold(vec![call("a", "慢"), call("b", "快"), done("b"), done("a")]);
        assert_eq!(m.tool, None);
    }

    #[test]
    fn answered_calls_leave_no_tool_and_reply_gives_summary() {
        let m = fold(vec![
            msg(Role::User, "看看", 1),
            call("Bash", "ls"),
            result(),
            msg(Role::Assistant, "改好了。另外还有一处要看", 2),
        ]);
        assert_eq!(m.tool, None);
        assert_eq!(m.summary.as_deref(), Some("改好了。"));
    }

    #[test]
    fn vendor_summary_wins_and_new_turn_resets() {
        let m = fold(vec![
            msg(Role::User, "a", 1),
            msg(Role::Assistant, "好的", 2),
            Event::Summary("我在修登录".to_owned()),
            Event::TurnEnded {
                duration: Some(Duration::from_secs(60)),
                failed: None,
            },
        ]);
        assert_eq!(m.summary.as_deref(), Some("我在修登录"));
        assert_eq!(m.duration, Some(Duration::from_secs(60)));
        let m = fold(vec![
            Event::Summary("旧的".to_owned()),
            Event::TurnEnded {
                duration: Some(Duration::from_secs(5)),
                failed: Some("x".to_owned()),
            },
            msg(Role::User, "再来", 9),
        ]);
        assert_eq!((m.summary, m.duration, m.failed), (None, None, None));
    }

    #[test]
    fn turn_end_clears_unanswered_calls() {
        let m = fold(vec![
            call("Bash", "sleep 100"),
            Event::TurnEnded {
                duration: None,
                failed: Some("interrupted".to_owned()),
            },
        ]);
        assert_eq!(m.tool, None);
        assert_eq!(m.failed.as_deref(), Some("interrupted"));
    }

    #[test]
    fn plan_survives_new_turns_and_counts_done() {
        let steps = plan_steps(
            &serde_json::json!([
                {"step": "读代码", "status": "completed"},
                {"step": "改", "status": "in_progress"},
                {"step": "测", "status": "pending"},
                {"status": "pending"}
            ]),
            &["step"],
        )
        .unwrap();
        let m = fold(vec![Event::Plan(steps), msg(Role::User, "继续", 3)]);
        assert_eq!(m.plan.len(), 3);
        assert_eq!(m.plan[1].status, StepStatus::Doing);
        assert_eq!(m.plan_progress(), Some((1, 3)));
        assert_eq!(Moment::default().plan_progress(), None);
    }

    #[test]
    fn first_sentence_cuts_at_sentence_end() {
        assert_eq!(first_sentence("Done. Next we").as_deref(), Some("Done."));
        assert_eq!(
            first_sentence("版本 0.1.3 已发").as_deref(),
            Some("版本 0.1.3 已发")
        );
        assert_eq!(first_sentence("第一行\n第二行").as_deref(), Some("第一行"));
        assert_eq!(first_sentence("   "), None);
    }
}
