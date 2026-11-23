//! 答完不发提醒信号的 CLI（pi）的闲置收回：会话台读厂商记录（[`Source::turn_done`]），这一轮答完了、
//! 记录也超过闲置时长没变，就请守护进程收回（`Request::Reclaim`）；没有标签页连着、PTY 也超过时长没输出
//! 由守护进程核对，满足才按闲置收回的步骤结束（两下 Ctrl+C → SIGTERM → SIGKILL）。会话台开着时才收。

use super::*;

/// 多久看一遍（和守护进程查闲置一样）。
const CHECK_EVERY: Duration = crate::host::idle::SWEEP_EVERY;

/// 交给收回线程的：设置里的闲置时长（`None` 是关了）和要看的对话（守护进程里的 id，列表里的那条）。
pub(super) type Targets = (Option<Duration>, Vec<(SessionId, Entry)>);

/// 收回线程：每 [`CHECK_EVERY`] 看一遍，目标变了马上看一遍。请求失败（守护进程说还不能收、旧版不认）不管，下一轮再看。
pub(super) fn reclaim_worker(ctx: Ctx) -> Sender<Targets> {
    let (req, rx) = mpsc::channel::<Targets>();
    thread::spawn(move || {
        let Ok(sources) = ctx.sources() else {
            return;
        };
        let mut targets: Targets = (None, Vec::new());
        loop {
            match rx.recv_timeout(CHECK_EVERY) {
                Ok(t) => targets = rx.try_iter().last().unwrap_or(t),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let Some(limit) = targets.0 else { continue };
            let now = SystemTime::now();
            for (hosted, e) in &targets.1 {
                let Ok(src) = source_of(&sources, &e.session) else {
                    continue;
                };
                if record_idle(src, &e.session, now, limit) {
                    let _ = host::client::request(
                        &ctx.data_dir,
                        &Request::Reclaim { id: hosted.clone() },
                    );
                }
            }
        }
    });
    req
}

/// 记录说这一轮答完了，而且记录（[`Source::moment_files`] 里最新的一个）到 `now` 已有 `limit` 没变。
/// 先看修改时间，够久了才读记录；读不了、看不出来都算不能收。
pub(super) fn record_idle(src: &dyn Source, s: &Session, now: SystemTime, limit: Duration) -> bool {
    let changed = src
        .moment_files(s)
        .iter()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max();
    let Some(changed) = changed else {
        return false;
    };
    now.duration_since(changed).is_ok_and(|d| d >= limit)
        && matches!(src.turn_done(s), Ok(Some(true)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::PiSource;

    const MIN: Duration = Duration::from_secs(60);
    const LIMIT: Duration = Duration::from_secs(30 * 60);
    const USER: &str = r#"{"type":"message","id":"u1","timestamp":"2026-11-20T10:01:00.000Z","message":{"role":"user","content":[{"type":"text","text":"看看"}],"timestamp":1795168860000}}"#;
    const DONE: &str = r#"{"type":"message","id":"a1","timestamp":"2026-11-20T10:02:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"好，随时叫我。"}],"stopReason":"stop","timestamp":1795168920000}}"#;
    const CALL: &str = r#"{"type":"message","id":"a2","timestamp":"2026-11-20T10:02:00.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"c1","name":"bash","arguments":{}}],"stopReason":"toolUse","timestamp":1795168920000}}"#;

    /// 手写的 pi 会话文件，返回来源、对话和文件的修改时间（假时钟从它算起）。
    fn pi(name: &str, lines: &[&str]) -> (PiSource, Session, SystemTime) {
        let root =
            std::env::temp_dir().join(format!("huitai-reclaim-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("sessions/--Users-demo-p--");
        std::fs::create_dir_all(&dir).unwrap();
        let head = r#"{"type":"session","version":3,"id":"aaaaaaaa-0000-4000-8000-0000000000b1","timestamp":"2026-11-20T10:00:00.000Z","cwd":"/Users/demo/p"}"#;
        let body: Vec<&str> = std::iter::once(head).chain(lines.iter().copied()).collect();
        let path = dir.join("2026-11-20T10-00-00-000Z_aaaaaaaa-0000-4000-8000-0000000000b1.jsonl");
        std::fs::write(&path, body.join("\n") + "\n").unwrap();
        let src = PiSource::new(root.join("sessions"), Trash::new(root.join("trash")));
        let s = src.scan().sessions.pop().unwrap();
        let at = std::fs::metadata(&path).unwrap().modified().unwrap();
        (src, s, at)
    }

    #[test]
    fn done_and_quiet_for_the_limit_is_reclaimed() {
        let (src, s, t0) = pi("done", &[USER, DONE]);
        assert!(!record_idle(&src, &s, t0 + 29 * MIN, LIMIT), "不到时长不收");
        assert!(record_idle(&src, &s, t0 + 30 * MIN, LIMIT));
    }

    #[test]
    fn mid_turn_or_new_question_is_kept() {
        let (src, s, t0) = pi("call", &[USER, CALL]);
        assert!(
            !record_idle(&src, &s, t0 + 600 * MIN, LIMIT),
            "最后是工具调用"
        );
        let (src, s, t0) = pi("asked", &[USER, DONE, USER]);
        assert!(!record_idle(&src, &s, t0 + 600 * MIN, LIMIT), "又问了一句");
    }

    #[test]
    fn vendors_without_a_record_judgement_are_left_to_the_daemon() {
        let (_, mut s, t0) = pi("claude", &[USER, DONE]);
        s.vendor = Vendor::Claude;
        let claude = crate::sources::ClaudeSource::new(
            std::env::temp_dir().join("huitai-reclaim-none"),
            Trash::new(std::env::temp_dir().join("huitai-reclaim-none-trash")),
        );
        assert!(!record_idle(&claude, &s, t0 + 600 * MIN, LIMIT));
    }
}
