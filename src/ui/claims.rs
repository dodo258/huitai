//! 会话台新建、托管在临时 id 下的对话认回列表里的哪条（新建时定不了 id 的那几家，见 `VendorInfo::presets_new_id`）。
//!
//! - [`Claims`]：按时间可能是同一条的（真实 id，临时 id）再看证据，认准了才认领（不做 IO，测试直接喂数据）；
//! - [`hosted_alias`]、[`time_pairs`]：一条对话按时间会不会是哪个临时 id 那条、打开时实际托管在哪个 id 下；
//! - [`load_claims`]、[`save_claims`]：认领记录 `claims.json` 落盘（几个会话台之间持锁读改写）。
//!
//! 对话是什么时候生成的问各家的来源（[`crate::sources::created_at`]），这里不按厂商分支。

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::core::{Session, SessionId, Vendor};
use crate::host::HostedInfo;
use crate::index::Entry;
use crate::sources;

/// 新建时定不了 id 的那几家（Codex、Hermes），新对话的临时托管 id 是 `new-<存储名>-<毫秒时间戳>`。
pub(super) const TEMP_PREFIX: &str = "new-";
/// 认领记录（临时 id → 认领到的那条，见 [`Claims`]）在数据目录里的文件名。
pub(super) const CLAIMS_FILE: &str = "claims.json";
/// 几个会话台改认领记录时排队用的锁文件（和记录在同一目录）。
const CLAIMS_LOCK: &str = "claims.json.lock";
/// 等别的会话台改完认领记录最多等多久。
const CLAIMS_LOCK_WAIT: Duration = Duration::from_secs(2);
/// 认领记录留多久（按临时 id 里的新建时间算）。
const CLAIMS_KEEP: Duration = Duration::from_secs(30 * 24 * 3600);

/// 对话多了消息的那一刻前后这么久之内，临时 id 那个进程有过输出，才算「是它写的」。
pub const CLAIM_WINDOW: Duration = Duration::from_secs(10);
/// 对上这么多次（且一次都没对不上）才认领。
pub const CLAIM_WITNESSES: u32 = 2;
/// 临时 id 那个进程的输出时刻留多久（对话的变化在这之内都对完了）。
const CLAIM_KEEP: Duration = Duration::from_secs(600);

/// 一条认领：临时 id 下的进程 `pid` 写的就是列表里的 `real`。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Bound {
    pub real: SessionId,
    pub pid: u32,
}

/// 会话台看着的一个临时 id 进程：从什么时候开始看的、进程号、最近的输出。
#[derive(Debug)]
struct Watch {
    since: SystemTime,
    pid: u32,
    outputs: VecDeque<Output>,
}

/// 临时 id 那个进程的一次输出：什么时候，当过证据没有（一次输出只能给一次变化作证）。
#[derive(Debug)]
struct Output {
    at: SystemTime,
    used: bool,
}

/// 会话台新建、托管在临时 id 下的对话（新建时定不了 id 的几家）认回列表里的哪条。
///
/// 只按时间对得上不够（审计 01：另一个终端在同项目同时新建的那条也对得上）。运行层把按时间可能是它的
/// （真实 id，临时 id）对交给 [`Claims::settle`]，这里再看证据：那条对话每多一批消息，临时 id 那个进程
/// 前后 [`CLAIM_WINDOW`] 之内都在出输出；对上 [`CLAIM_WITNESSES`] 次、一次都没对不上，且两边都只剩
/// 对方一个可能，才认领（[`Bound`]）。有一次对话多了消息而那个进程一声不响，就肯定不是它。
/// 每次输出只能给一次变化作证、用过作废（审计 v0.1.4 第 6 条：进程只出一次声，另一条对话在这前后各变一次，
/// 不能算两份证据）；窗口里的输出都用过了的变化不算证据，也不否掉。
/// 认不准的宁可当两条：不认领。
#[derive(Debug, Default)]
pub struct Claims {
    watches: HashMap<SessionId, Watch>,
    /// 列表里的对话上次看到的消息数。
    counts: HashMap<SessionId, usize>,
    /// 等着对证的变化：哪条对话、它最后活动的时刻。等那一刻过去 [`CLAIM_WINDOW`] 再对（输出可能晚到）。
    pending: Vec<(SessionId, SystemTime)>,
    /// （真实 id，临时 id）对上过几次。
    witnessed: HashMap<(SessionId, SessionId), u32>,
    /// （真实 id，临时 id）肯定不是。
    refuted: HashSet<(SessionId, SessionId)>,
    /// 临时 id → 认领到的那条。
    bound: HashMap<SessionId, Bound>,
}

impl Claims {
    /// 从落盘的认领接着来（会话台重开、守护进程中断后认回用）。
    pub fn restore(bound: HashMap<SessionId, Bound>) -> Self {
        Self {
            bound,
            ..Self::default()
        }
    }

    /// 接上别的会话台落盘的认领（审计 v0.1.4 第 13 条：别的窗口新建、认领的，这边也要认得）。
    /// 自己已有认领的临时 id、自己有证据否掉的、进程号和这边看着的对不上的、那条已认领在别的临时 id 下的，不接。
    pub fn adopt(&mut self, saved: HashMap<SessionId, Bound>) {
        // 记录里同一条认在几个临时 id 下（记坏了）：哪个都不接。
        let twice = |real: &SessionId| saved.values().filter(|b| b.real == *real).count() > 1;
        let mut take = Vec::new();
        for (temp, b) in &saved {
            let stale = self.watches.get(temp).is_some_and(|w| w.pid != b.pid);
            let taken = self
                .bound
                .iter()
                .any(|(t, x)| x.real == b.real && t != temp);
            if !self.bound.contains_key(temp)
                && !stale
                && !taken
                && !twice(&b.real)
                && !self.refuted.contains(&(b.real.clone(), temp.clone()))
            {
                take.push((temp.clone(), b.clone()));
            }
        }
        self.bound.extend(take);
    }

    /// 全部认领：临时 id → 那条。
    pub fn bound(&self) -> &HashMap<SessionId, Bound> {
        &self.bound
    }

    /// 临时 id `temp` 的进程 `pid` 在跑（`now` 这一刻知道的）。换了进程（CLI 退出后又接上，起的是新对话）：
    /// 之前认领的那条不再是它，返回 `true`（认领变了）。
    pub fn watch(&mut self, temp: &SessionId, pid: u32, now: SystemTime) -> bool {
        if self.watches.get(temp).is_some_and(|w| w.pid == pid) {
            return false;
        }
        self.watches.insert(
            temp.clone(),
            Watch {
                since: now,
                pid,
                outputs: VecDeque::new(),
            },
        );
        match self.bound.get(temp) {
            Some(b) if b.pid != pid => {
                let real = b.real.clone();
                self.bound.remove(temp);
                self.refuted.insert((real, temp.clone()));
                true
            }
            _ => false,
        }
    }

    /// 临时 id 那个进程不在了（退出、守护进程没了）：不再看它。认领留着，守护进程中断后认回要用。
    pub fn unwatch(&mut self, temp: &SessionId) {
        self.watches.remove(temp);
    }

    /// 守护进程没了或托管列表整个换了：之前看着的都不算数，重新开始看。
    pub fn unwatch_all(&mut self) {
        self.watches.clear();
    }

    /// 正看着的临时 id 和它的进程号。
    pub fn running(&self) -> impl Iterator<Item = (&SessionId, u32)> {
        self.watches.iter().map(|(id, w)| (id, w.pid))
    }

    /// 临时 id 那个进程在 `at` 有输出。
    pub fn output(&mut self, temp: &SessionId, at: SystemTime) {
        let Some(w) = self.watches.get_mut(temp) else {
            return;
        };
        w.outputs.push_back(Output { at, used: false });
        while w
            .outputs
            .front()
            .is_some_and(|o| at.duration_since(o.at).is_ok_and(|d| d > CLAIM_KEEP))
        {
            w.outputs.pop_front();
        }
    }

    /// 列表里一条对话现在有 `count` 条消息、最后活动在 `at`。消息数变了就记下等对证。
    pub fn entry_seen(&mut self, real: &SessionId, count: usize, at: SystemTime) {
        if self.counts.insert(real.clone(), count) == Some(count) || self.watches.is_empty() {
            return;
        }
        self.pending.push((real.clone(), at));
    }

    /// 有到时候该对证的变化。
    pub fn due(&self, now: SystemTime) -> bool {
        self.pending.iter().any(|(_, at)| ripe(*at, now))
    }

    /// `real` 还可能是 `temp` 那条：没被否掉，两边也都没认领别的。
    pub fn open(&self, real: &SessionId, temp: &SessionId) -> bool {
        !self.refuted.contains(&(real.clone(), temp.clone()))
            && self.bound.get(temp).is_none_or(|b| b.real == *real)
            && !self.bound.iter().any(|(t, b)| b.real == *real && t != temp)
    }

    /// 列表里的 `real` 认领在哪个临时 id 下（不管那个进程还在不在）。
    pub fn binding_of(&self, real: &SessionId) -> Option<(&SessionId, &Bound)> {
        self.bound.iter().find(|(_, b)| b.real == *real)
    }

    /// 对证到时候的变化、能认领的认领。`pairs` 是按时间可能是同一条的（真实 id，临时 id）。
    /// 返回认领变了的临时 id（运行层据此落盘）。
    pub fn settle(&mut self, now: SystemTime, pairs: &[(SessionId, SessionId)]) -> Vec<SessionId> {
        let mut changed = Vec::new();
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|(_, at)| ripe(*at, now));
        self.pending = later;
        for (real, at) in due {
            for (_, temp) in pairs.iter().filter(|(r, _)| *r == real) {
                let Some(w) = self.watches.get_mut(temp) else {
                    continue;
                };
                // 开始看它之前的变化不知道它那时出没出声：不算证据。
                if at < w.since {
                    continue;
                }
                let key = (real.clone(), temp.clone());
                let mut near = w
                    .outputs
                    .iter_mut()
                    .filter(|o| gap(o.at, at) <= CLAIM_WINDOW)
                    .peekable();
                if near.peek().is_some() {
                    // 离这次变化最近、还没作过证的那次输出作证，用过作废；都用过了就不算证据。
                    if let Some(o) = near.filter(|o| !o.used).min_by_key(|o| gap(o.at, at)) {
                        o.used = true;
                        *self.witnessed.entry(key).or_default() += 1;
                    }
                    continue;
                }
                self.refuted.insert(key);
                if self.bound.get(temp).is_some_and(|b| b.real == real) {
                    self.bound.remove(temp);
                    changed.push(temp.clone());
                }
            }
        }
        for (real, temp) in pairs {
            if self.bound.contains_key(temp) || !self.open(real, temp) {
                continue;
            }
            let Some(pid) = self.watches.get(temp).map(|w| w.pid) else {
                continue;
            };
            let enough = self
                .witnessed
                .get(&(real.clone(), temp.clone()))
                .is_some_and(|n| *n >= CLAIM_WITNESSES);
            let rival = pairs
                .iter()
                .any(|(r, t)| (r == real) != (t == temp) && self.open(r, t));
            if enough && !rival {
                self.bound.insert(
                    temp.clone(),
                    Bound {
                        real: real.clone(),
                        pid,
                    },
                );
                changed.push(temp.clone());
            }
        }
        changed
    }
}

/// `at` 那一刻的变化可以对证了（输出最晚也该到了）。
fn ripe(at: SystemTime, now: SystemTime) -> bool {
    now.duration_since(at).is_ok_and(|d| d >= CLAIM_WINDOW)
}

/// 两个时刻相差多久。
fn gap(a: SystemTime, b: SystemTime) -> Duration {
    a.duration_since(b).unwrap_or_else(|e| e.duration())
}

/// 一条托管在临时 id 下的新对话（新建时定不了 id 的那几家，见 `VendorInfo::presets_new_id`）。
#[derive(Debug, Clone)]
pub(super) struct NewSession {
    pub(super) vendor: Vendor,
    pub(super) cwd: PathBuf,
    /// 新建那一刻这个项目里已有的这家对话（这些肯定不是新建的那条）；守护进程报来的旧对话不知道，是空的。
    pub(super) existing: HashSet<SessionId>,
}

/// 打开一条对话时它实际托管在哪个 id 下。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Hosted {
    /// 用它自己的 id（没托管就新登记）。
    Own,
    /// 它就是托管在这个 id 下的那条：会话台新建时的临时 id，或 v0.1.3 记的原生 id（见 [`native_owners`]）。
    Alias(SessionId),
    /// 它可能是托管着、还在跑的那条，但确认不了是哪条（或是不是）。
    Ambiguous,
}

/// 守护进程里以厂商自己的 id 托管、记下的（没有账号前缀、不是临时 id）：v0.1.3 不分账号，托管和中断记录都记原生 id；
/// 新版只有默认账号这样记。
pub(super) fn native_hosted(id: &SessionId) -> bool {
    id.split_account().0.is_empty() && temp_started(id).is_none()
}

/// 以原生 id `id` 托管在项目 `cwd` 的这家对话，对到列表里的哪几条（审计 v0.1.4 第 7 条）。
/// 列表里有 id 原样相同的那条（默认账号的）就是它：新版托管默认账号的对话也记原生 id，记录里分不出是
/// v0.1.3 托管的还是新版托管的（没有账号字段），照新版当它（工作账号里同项目的同 id 副本照旧各开各的）。
/// 没有时（v0.1.3 在自定义目录下托管、升级后那条带了账号前缀）按厂商、原生 id、项目找各账号里的同名对话：
/// 同项目的正好一条才是它；几条是认不准，一条也不接；没有就是空的。
pub(super) fn native_owners<'a>(
    id: &SessionId,
    vendor: Vendor,
    cwd: &Path,
    listed: impl IntoIterator<Item = &'a Session>,
) -> Vec<&'a Session> {
    let same: Vec<&Session> = listed
        .into_iter()
        .filter(|s| s.vendor == vendor && s.id.native() == id.as_str())
        .collect();
    if let Some(exact) = same.iter().find(|s| s.id == *id) {
        return vec![exact];
    }
    same.into_iter().filter(|s| s.cwd == cwd).collect()
}

/// `s` 会不会是临时 id `temp` 那条、`since` 那一刻在项目 `cwd` 里新建的对话：同一个账号、同一项目、
/// id 是那之后才生成的（新建之前就有的对话，不管后来在别处聊没聊，都不可能是它），
/// 且不在新建那一刻已有的对话 `existing` 里。id 看不出生成时间的一律不认。
pub(super) fn is_new_since(
    s: &Session,
    temp: &SessionId,
    cwd: &Path,
    since: SystemTime,
    existing: Option<&HashSet<SessionId>>,
) -> bool {
    s.account == temp.split_account().0
        && s.cwd == cwd
        && sources::created_at(s).is_some_and(|c| c >= since)
        && !existing.is_some_and(|e| e.contains(&s.id))
}

/// 在跑的临时 id `h` 按时间会不会是 `x` 那条（[`is_new_since`]）。自动运行的对话（`codex exec` 这类）
/// 不是会话台里新开的那个交互进程写的，不算。
fn may_be(h: &HostedInfo, x: &Session, known: &HashMap<SessionId, NewSession>) -> bool {
    h.vendor == x.vendor
        && h.pid.is_some()
        && !x.automatic
        && temp_started(&h.id).is_some_and(|t| {
            is_new_since(x, &h.id, &h.cwd, t, known.get(&h.id).map(|n| &n.existing))
        })
}

/// 新建时定不了 id 的几家，新对话托管在临时 id 下；它写出会话记录后列表里出现的是真实 id。
/// 一条不在托管里的对话，只有认领过（[`Claims`]：按时间对得上、且有证据是那个进程写的），且认领时的
/// 进程还在跑，才认作那个临时 id、接到那个进程上（不再起第二个 `resume`）。按时间可能是它、又没认领、
/// 也没被证据否掉的，是 [`Hosted::Ambiguous`]：绝不接错。
/// 以原生 id 托管着的（v0.1.3 托管的）按 [`native_owners`] 对到 `listed`（这家列表里的全部对话）：
/// 对上唯一一条就接到那个进程上，对上几条都是认不准，不再起第二个 CLI。
pub(super) fn hosted_alias(
    s: &Session,
    hosted: &[HostedInfo],
    known: &HashMap<SessionId, NewSession>,
    claims: &Claims,
    listed: &[Entry],
) -> Hosted {
    for h in hosted
        .iter()
        .filter(|h| h.vendor == s.vendor && native_hosted(&h.id))
    {
        // 以自己（带账号前缀）的 id 托管着的另有其主，不算。
        let others = listed
            .iter()
            .map(|e| &e.session)
            .filter(|x| x.id == h.id || !hosted.iter().any(|y| y.id == x.id));
        let owners = native_owners(&h.id, h.vendor, &h.cwd, others);
        if !owners.iter().any(|o| o.id == s.id) {
            continue;
        }
        return match owners.as_slice() {
            [_] if h.id == s.id => Hosted::Own,
            [_] => Hosted::Alias(h.id.clone()),
            _ => Hosted::Ambiguous,
        };
    }
    if hosted.iter().any(|h| h.id == s.id) {
        return Hosted::Own;
    }
    if let Some((temp, b)) = claims.binding_of(&s.id)
        && hosted.iter().any(|h| h.id == *temp && h.pid == Some(b.pid))
    {
        return Hosted::Alias(temp.clone());
    }
    if hosted
        .iter()
        .any(|h| may_be(h, s, known) && claims.open(&s.id, &h.id))
    {
        return Hosted::Ambiguous;
    }
    Hosted::Own
}

/// 按时间可能是同一条的（真实 id，临时 id）：`listed` 里没以自己的 id 托管着（`own` 为假）的对话 ×
/// 在跑的临时 id `temps`。交给 [`Claims::settle`] 对证。
pub(super) fn time_pairs(
    listed: &[Entry],
    temps: &[HostedInfo],
    known: &HashMap<SessionId, NewSession>,
    own: impl Fn(&SessionId) -> bool,
) -> Vec<(SessionId, SessionId)> {
    listed
        .iter()
        .filter(|e| !own(&e.session.id))
        .flat_map(|e| {
            temps
                .iter()
                .filter(|h| may_be(h, &e.session, known))
                .map(|h| (e.session.id.clone(), h.id.clone()))
        })
        .collect()
}

/// 读落盘的认领记录；没有这个文件是空的。新建超过 [`CLAIMS_KEEP`] 的临时 id 不要了。
pub(super) fn load_claims(dir: &Path, now: SystemTime) -> io::Result<HashMap<SessionId, Bound>> {
    let path = dir.join(CLAIMS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}：{e}", path.display()))),
    };
    let mut bound: HashMap<SessionId, Bound> = serde_json::from_str(&text)
        .map_err(|e| io::Error::other(format!("{}：{e}", path.display())))?;
    bound.retain(|temp, _| {
        temp_started(temp).is_some_and(|t| now.duration_since(t).is_ok_and(|d| d < CLAIMS_KEEP))
    });
    Ok(bound)
}

/// 把这几个临时 id 的认领写进记录：有就写、没有就删。别的会话台写的其余条目原样留着。
/// 读、改、写整段持跨进程锁（审计 v0.1.4 第 12 条：两个窗口各读旧文件再替换，后写的盖掉先写的）；
/// 旧记录读不出来就报错、不写，不拿空表把别人的认领盖掉。
pub(super) fn save_claims(
    dir: &Path,
    changed: &[SessionId],
    bound: &HashMap<SessionId, Bound>,
) -> io::Result<()> {
    let _lock = lock_claims(dir)?;
    let mut all = load_claims(dir, SystemTime::now())?;
    for temp in changed {
        match bound.get(temp) {
            Some(b) => all.insert(temp.clone(), b.clone()),
            None => all.remove(temp),
        };
    }
    let path = dir.join(CLAIMS_FILE);
    let tmp = dir.join(format!("{CLAIMS_FILE}.{}.tmp", std::process::id()));
    let json = serde_json::to_vec(&all).map_err(io::Error::other)?;
    std::fs::write(&tmp, json)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| io::Error::new(e.kind(), format!("{}：{e}", path.display())))
}

/// 拿认领记录的独占锁（flock）；返回的文件关掉时锁随之释放。别的会话台占着就等，最多 [`CLAIMS_LOCK_WAIT`]。
fn lock_claims(dir: &Path) -> io::Result<std::fs::File> {
    let path = dir.join(CLAIMS_LOCK);
    let with_path = |e: io::Error| io::Error::new(e.kind(), format!("{}：{e}", path.display()));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(with_path)?;
    let deadline = Instant::now() + CLAIMS_LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(with_path(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "别的会话台一直占着认领记录",
                )));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(with_path(e)),
        }
    }
}

/// 临时 id 里记的新建时间。
pub(super) fn temp_started(id: &SessionId) -> Option<SystemTime> {
    let (key, ms) = id.native().strip_prefix(TEMP_PREFIX)?.rsplit_once('-')?;
    Vendor::ALL
        .iter()
        .any(|v| v.key() == key && !v.info().presets_new_id)
        .then_some(())?;
    let ms: u64 = ms.parse().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
}
