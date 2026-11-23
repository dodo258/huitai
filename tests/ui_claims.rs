//! 会话台新建、托管在临时 id 下的对话认回列表里的哪条（审计 `docs/audits/2026-11-23-v0.1.3.md` 第 01、03 条）。
//! 01：只按时间对得上不认领，要有「是那个进程写的」证据，认不准宁可当两条；
//! 03：认领到的真实 id 参与状态、实时预览、已读、x 结束（都按临时 id）。

mod ui_support;

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use huitai::core::{SessionId, SessionState, Vendor};
use huitai::ui::app::{Bound, CLAIM_WINDOW, Claims, Effect, Prompt};
use ratatui::crossterm::event::KeyCode;

use ui_support::*;

const TEMP: &str = "new-kimi-1000000000";
const PID: u32 = 4242;

fn id(s: &str) -> SessionId {
    SessionId::from(s)
}

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// 会话台从第 1000 秒开始看着临时 id 那个进程（这时它刚起、空着）。
fn watching() -> Claims {
    let mut c = Claims::default();
    c.watch(&id(TEMP), PID, at(1000));
    c
}

/// 对证 `at` 那一刻之前的变化（等满 [`CLAIM_WINDOW`]）。
fn settle(c: &mut Claims, now: u64, pairs: &[(SessionId, SessionId)]) -> Vec<SessionId> {
    c.settle(at(now) + CLAIM_WINDOW, pairs)
}

/// 审计 01 原样：会话台新开 A（临时 id），一直空着；同项目另一个终端新开 B、聊了两轮后退出。
/// 按时间 B 是 A 唯一可能的那条，以前直接认领，回车接进 A、中断恢复把「继续」发给 B。
/// 现在：B 每次多消息时 A 都一声不响，B 肯定不是 A，不认领，也不再算「可能是」。
#[test]
fn conversation_written_while_the_temp_process_was_silent_is_never_claimed() {
    let mut c = watching();
    let b = id("kimi-b");
    let pairs = [(b.clone(), id(TEMP))];
    // A 起来时画了一次界面，之后一直没输出。
    c.output(&id(TEMP), at(1001));
    c.entry_seen(&b, 2, at(1100));
    assert!(settle(&mut c, 1100, &pairs).is_empty());
    c.entry_seen(&b, 4, at(1200));
    assert!(settle(&mut c, 1200, &pairs).is_empty());
    assert!(c.bound().is_empty(), "不能认领：{:?}", c.bound());
    assert!(!c.open(&b, &id(TEMP)), "B 被证据否掉，从 B 打开时是它自己");
    // 之后 A 碰巧在 B 多消息时出了声，也不再认（否掉的不翻案）。
    for t in [1300, 1400, 1500] {
        c.output(&id(TEMP), at(t));
        c.entry_seen(&b, (t / 50) as usize, at(t));
        settle(&mut c, t, &pairs);
    }
    assert!(c.bound().is_empty());
}

/// 正常情况：只有会话台新开的这一个，每次那条对话多消息时临时 id 那个进程都在出输出：对上两次就认领。
#[test]
fn conversation_written_while_the_temp_process_was_busy_is_claimed() {
    let mut c = watching();
    let real = id("kimi-a");
    let pairs = [(real.clone(), id(TEMP))];
    c.output(&id(TEMP), at(1050));
    c.output(&id(TEMP), at(1052));
    c.entry_seen(&real, 1, at(1051));
    assert!(settle(&mut c, 1051, &pairs).is_empty(), "对上一次还不认");
    assert!(c.open(&real, &id(TEMP)), "还可能是它（认不准，界面上不接）");
    c.output(&id(TEMP), at(1080));
    c.entry_seen(&real, 2, at(1083));
    assert_eq!(settle(&mut c, 1083, &pairs), vec![id(TEMP)]);
    assert_eq!(
        c.bound().get(&id(TEMP)),
        Some(&Bound {
            real: real.clone(),
            pid: PID
        })
    );
    assert_eq!(c.binding_of(&real).map(|(t, _)| t.clone()), Some(id(TEMP)));
}

/// 审计 v0.1.4 第 6 条原样：同账号、同项目，会话台新建的 A（临时 id）只输出了一次；外部终端里的对话 B
/// 在这次输出前后各多了一批消息，两次都落在这一次输出的 [`CLAIM_WINDOW`] 里。以前这一次输出算两份证据，
/// B 被认领到 A 下，之后从 B 回车接进 A、x 结束的是 A。现在一次输出只给一次变化作证：B 只对上一次，不认领，
/// 仍是「可能是」（界面上不接、不当别处在用）。
#[test]
fn one_output_is_not_two_witnesses() {
    let mut c = watching();
    let b = id("kimi-b");
    let pairs = [(b.clone(), id(TEMP))];
    c.output(&id(TEMP), at(1100));
    c.entry_seen(&b, 2, at(1096));
    c.entry_seen(&b, 4, at(1104));
    assert!(settle(&mut c, 1104, &pairs).is_empty());
    assert!(c.bound().is_empty(), "一次输出不能认领：{:?}", c.bound());
    assert!(c.open(&b, &id(TEMP)), "没对不上，仍是认不准");
    // 之后各有各的输出作证，才认领。
    c.output(&id(TEMP), at(1200));
    c.entry_seen(&b, 6, at(1201));
    assert_eq!(settle(&mut c, 1201, &pairs), vec![id(TEMP)]);
}

/// 审计 v0.1.4 第 13 条：别的会话台落盘的认领接过来；自己认领着的、进程号对不上的、记录里一条认在两处的不接。
#[test]
fn adopts_claims_saved_by_another_board() {
    let real = id("kimi-a");
    let other = id("new-kimi-1000000001");
    let bound = |temp: &SessionId, pid| {
        (
            temp.clone(),
            Bound {
                real: real.clone(),
                pid,
            },
        )
    };
    let mut c = watching();
    c.adopt(HashMap::from([bound(&id(TEMP), PID)]));
    assert_eq!(c.binding_of(&real).map(|(t, _)| t.clone()), Some(id(TEMP)));
    // 这边已经认给 TEMP 了：同一条在别的临时 id 下的不接。
    c.adopt(HashMap::from([bound(&other, 7)]));
    assert!(!c.bound().contains_key(&other));
    // 进程号和这边看着的对不上（那次认领是上一个进程的）：不接。
    let mut c = watching();
    c.adopt(HashMap::from([bound(&id(TEMP), PID + 1)]));
    assert!(c.bound().is_empty());
    // 记录里同一条认在两个临时 id 下：哪个都不接。
    let mut c = watching();
    c.adopt(HashMap::from([bound(&id(TEMP), PID), bound(&other, 7)]));
    assert!(c.bound().is_empty());
}

/// 认不准时宁可当两条：同项目两条都可能是它（两条都在它出声时多了消息）时谁也不认；
/// 会话台开始看着它之前的变化（会话台刚打开）不算证据。
#[test]
fn ambiguous_or_unobserved_conversations_are_not_claimed() {
    let mut c = watching();
    let (a, b) = (id("kimi-a"), id("kimi-b"));
    let pairs = [(a.clone(), id(TEMP)), (b.clone(), id(TEMP))];
    for t in [1050, 1080, 1110] {
        c.output(&id(TEMP), at(t));
        c.entry_seen(&a, (t / 10) as usize, at(t));
        c.entry_seen(&b, (t / 10) as usize, at(t + 1));
        settle(&mut c, t + 1, &pairs);
    }
    assert!(c.bound().is_empty(), "两条都可能是：{:?}", c.bound());

    let mut c = watching();
    let old = id("kimi-old");
    // 会话台打开时列表里已有它（最后活动在开始看之前）：不知道那时进程出没出声。
    c.entry_seen(&old, 3, at(990));
    c.entry_seen(&old, 4, at(995));
    settle(&mut c, 995, &[(old.clone(), id(TEMP))]);
    assert!(c.bound().is_empty());
    assert!(c.open(&old, &id(TEMP)));
}

/// 临时 id 下换了进程（CLI 退出后又接上，起的是新对话）：之前认领的那条不再是它。
#[test]
fn claim_is_dropped_when_the_temp_id_gets_a_new_process() {
    let real = id("kimi-a");
    let mut c = Claims::restore(HashMap::from([(
        id(TEMP),
        Bound {
            real: real.clone(),
            pid: PID,
        },
    )]));
    // 同一个进程（会话台重开）：认领留着。
    assert!(!c.watch(&id(TEMP), PID, at(2000)));
    assert!(c.binding_of(&real).is_some());
    // 新进程：认领拿掉，旧那条也不再算可能是它。
    assert!(c.watch(&id(TEMP), PID + 1, at(2100)));
    assert!(c.bound().is_empty());
    assert!(!c.open(&real, &id(TEMP)));
}

/// 审计 03：认领到的对话（列表里是真实 id）状态、等你回复计数、实时预览、已读、x 结束都按临时 id。
#[test]
fn claimed_conversation_uses_its_temp_host_for_state_preview_and_kill() {
    let mut app = demo_app(Vendor::Codex);
    let (real, temp) = (id("x-api-2"), id("new-codex-1000000000"));
    app.states.insert(temp.clone(), SessionState::Waiting);
    app.attached.insert(temp.clone());
    let (_, waiting_before) = app.global_counts();
    app.set_aliases(HashMap::from([(real.clone(), temp.clone())]));
    assert_eq!(app.state_of(&real), SessionState::Waiting);
    assert!(
        app.attached.contains(&real),
        "标签页接着临时 id：这条也算开着"
    );
    assert_eq!(app.global_counts().1, waiting_before + 1);
    assert_eq!(app.hosted_id(&real), &temp);

    app.key(key(KeyCode::Right));
    app.key(key(KeyCode::Right));
    select_id(&mut app, "x-api-2");
    assert_eq!(
        app.live_target(),
        Some(temp.clone()),
        "实时预览取临时 id 的画面"
    );
    assert!(app.key(key(KeyCode::Char('x'))).is_empty());
    assert!(
        matches!(&app.prompt, Some(Prompt::ConfirmKill { id, .. }) if *id == temp),
        "x 要结束的是临时 id 那个进程：{:?}",
        app.prompt
    );
    assert_eq!(
        app.key(key(KeyCode::Char('y'))),
        vec![Effect::Kill(temp.clone())]
    );

    // 认领拿掉了：回到它自己（已停）。
    app.set_aliases(HashMap::new());
    assert_eq!(app.state_of(&real), SessionState::Stopped);
    assert!(!app.attached.contains(&real));
    assert!(app.attached.contains(&temp));
}
