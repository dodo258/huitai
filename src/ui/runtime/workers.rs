//! 后台线程：预览、此刻、整段对话、命中片段、托管画面、额度、余额、进程、各家 CLI 新版、Claude 对话备份、看过、守护进程事件。
//! 每个线程收请求、把结果用 [`Msg`] 交回事件循环。

use super::*;

pub(super) fn forward<T: Send + 'static>(rx: Receiver<T>, tx: Sender<Msg>, wrap: fn(T) -> Msg) {
    thread::spawn(move || {
        for m in rx {
            if tx.send(wrap(m)).is_err() {
                return;
            }
        }
    });
}

/// 读预览：只读最新要的那条，前面没来得及读的跳过。
pub(super) fn preview_worker(ctx: Ctx, tx: Sender<Msg>) -> Sender<Entry> {
    let (req, rx) = mpsc::channel::<Entry>();
    thread::spawn(move || {
        let sources = ctx.sources();
        while let Ok(mut e) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                e = newer;
            }
            let s = &e.session;
            let messages = sources
                .as_ref()
                .map_err(|err| io::Error::new(err.kind(), err.to_string()))
                .and_then(|src| source_of(src, s)?.load_preview(s, PREVIEW_MESSAGES))
                .map_err(|err| err.to_string());
            let p = Preview {
                at: s.last_active,
                messages,
            };
            if tx.send(Msg::Preview(s.id.clone(), p)).is_err() {
                return;
            }
        }
    });
    req
}

/// 读「此刻」：每条最多 [`MOMENT_EVERY`] 读一次，会话文件变了才读；新加进来的马上读。
pub(super) fn moment_worker(ctx: Ctx, tx: Sender<Msg>) -> Sender<Vec<Entry>> {
    let (req, rx) = mpsc::channel::<Vec<Entry>>();
    thread::spawn(move || {
        let Ok(sources) = ctx.sources() else {
            return;
        };
        let mut targets: Vec<Entry> = Vec::new();
        let mut read: HashMap<SessionId, (Stamp, Instant)> = HashMap::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(t) => targets = rx.try_iter().last().unwrap_or(t),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            read.retain(|id, _| targets.iter().any(|e| e.session.id == *id));
            for e in &targets {
                let s = &e.session;
                let Ok(src) = source_of(&sources, s) else {
                    continue;
                };
                // 读之前取：读的时候又写进来的，下一轮看得出变了。
                let now = stamp(&src.moment_files(s));
                if !moment_due(read.get(&s.id), &now, Instant::now()) {
                    continue;
                }
                let Ok(m) = src.load_moment(s) else {
                    // 没读成不记版本（空的和谁都不等）：过 MOMENT_EVERY 再试。
                    read.insert(s.id.clone(), (Stamp::new(), Instant::now()));
                    continue;
                };
                read.insert(s.id.clone(), (now, Instant::now()));
                if tx.send(Msg::Moment(s.id.clone(), m)).is_err() {
                    return;
                }
            }
        }
    });
    req
}

/// 「此刻」读的那些文件（[`Source::moment_files`]：会话文件，或对话所在的库，连同 `-wal`）的大小和修改时间。
pub(super) type Stamp = Vec<Option<(u64, SystemTime)>>;

pub(super) fn stamp(files: &[PathBuf]) -> Stamp {
    files
        .iter()
        .map(|p| {
            let m = std::fs::metadata(p).ok()?;
            Some((m.len(), m.modified().ok()?))
        })
        .collect()
}

/// 该不该再读一次：没读过就读；读过的要文件变了、而且离上次已过 [`MOMENT_EVERY`]。
pub(super) fn moment_due(last: Option<&(Stamp, Instant)>, now: &Stamp, at: Instant) -> bool {
    last.is_none_or(|(old, when)| old != now && at.duration_since(*when) >= MOMENT_EVERY)
}

/// 读整段对话：和预览一样只读最新要的那条。大文件只读最后 32MB，几百毫秒，不挡界面。
pub(super) fn transcript_worker(ctx: Ctx, tx: Sender<Msg>) -> Sender<Entry> {
    let (req, rx) = mpsc::channel::<Entry>();
    thread::spawn(move || {
        let sources = ctx.sources();
        while let Ok(mut e) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                e = newer;
            }
            let s = &e.session;
            let t = sources
                .as_ref()
                .map_err(|err| io::Error::new(err.kind(), err.to_string()))
                .and_then(|src| source_of(src, s)?.load_transcript(s))
                .map_err(|err| err.to_string());
            if tx
                .send(Msg::Transcript(s.id.clone(), s.last_active, t))
                .is_err()
            {
                return;
            }
        }
    });
    req
}

/// 搜索结果行下的命中片段：界面每次给一批（查询词、当前屏看得见的几条），挨条读回正文找。
/// 来了别的查询词的一批，还没读的旧批次就丢掉（界面换查询词时也清掉了它们的占位）。
pub(super) fn snippet_worker(index: Index, tx: Sender<Msg>) -> Sender<(String, Vec<Entry>)> {
    let (req, rx) = mpsc::channel::<(String, Vec<Entry>)>();
    thread::spawn(move || {
        let mut queue: VecDeque<(String, Entry)> = VecDeque::new();
        let take = |queue: &mut VecDeque<(String, Entry)>,
                    (query, entries): (String, Vec<Entry>)| {
            queue.retain(|(q, _)| *q == query);
            queue.extend(entries.into_iter().map(|e| (query.clone(), e)));
        };
        loop {
            if queue.is_empty() {
                let Ok(batch) = rx.recv() else { return };
                take(&mut queue, batch);
            }
            while let Ok(batch) = rx.try_recv() {
                take(&mut queue, batch);
            }
            let Some((query, e)) = queue.pop_front() else {
                continue;
            };
            let snip = index.snippet(&e.session, &query).ok().flatten();
            if tx.send(Msg::Snippet(query, e.session.id, snip)).is_err() {
                return;
            }
        }
    });
    req
}

/// 实时预览：有目标时每 [`LIVE_EVERY`] 向守护进程取一次那条对话的屏幕，画面没变守护进程只回「没变」，
/// 不发给界面；目标为 `None` 时阻塞等下一个目标，不碰守护进程。
pub(super) fn live_worker(dir: PathBuf, tx: Sender<Msg>) -> Sender<Option<SessionId>> {
    let (req, rx) = mpsc::channel::<Option<SessionId>>();
    thread::spawn(move || {
        let mut target: Option<SessionId> = None;
        // 上次发给界面的画面版本；`None` 表示还没发过或上次没取到。
        let mut shown: Option<u64> = None;
        let mut failed = false;
        loop {
            let next = match target {
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(_) => rx.recv_timeout(LIVE_EVERY),
            };
            match next {
                Ok(mut t) => {
                    while let Ok(newer) = rx.try_recv() {
                        t = newer;
                    }
                    if t != target {
                        (target, shown, failed) = (t, None, false);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let Some(id) = target.as_ref() else { continue };
            let msg = match host::client::snapshot(&dir, id, shown) {
                Ok(Snapshot::Unchanged) => continue,
                Ok(Snapshot::Screen(s)) => {
                    (shown, failed) = (Some(s.version), false);
                    Msg::Live(id.clone(), Some(s))
                }
                // 取不到只告诉界面一次，之后照样定时再试（守护进程可能马上回来）。
                Err(_) if failed => continue,
                Err(_) => {
                    (shown, failed) = (None, true);
                    Msg::Live(id.clone(), None)
                }
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    });
    req
}

/// 额度：切到哪个账号（哪家、选中对话的账号）就读哪个，之后每分钟重读当前那个。
pub(super) fn quota_worker(tx: Sender<Msg>) -> Sender<Account> {
    let (req, rx) = mpsc::channel::<Account>();
    thread::spawn(move || {
        let Ok(mut current) = rx.recv() else {
            return;
        };
        loop {
            let reading = quota::quota(&current);
            if tx
                .send(Msg::Quota(current.vendor, current.key.clone(), reading))
                .is_err()
            {
                return;
            }
            match rx.recv_timeout(QUOTA_EVERY) {
                Ok(a) => current = a,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    req
}

/// 余额和订阅额度：启动时读一次，之后每 [`BALANCE_EVERY`] 一次（每把密钥自己有 5 分钟缓存）；
/// 收到请求（`$` 打开）就马上读，并把全部账号的额度也读一遍给面板用。
pub(super) fn balance_worker(accounts: Arc<Vec<Account>>, tx: Sender<Msg>) -> Sender<()> {
    let (req, rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut all = false;
        loop {
            if tx.send(Msg::Balance(quota::balances(&accounts))).is_err() {
                return;
            }
            if all {
                for a in accounts.iter().filter(|a| quota::supported(a.vendor)) {
                    let reading = quota::quota(a);
                    if tx
                        .send(Msg::Quota(a.vendor, a.key.clone(), reading))
                        .is_err()
                    {
                        return;
                    }
                }
            }
            match rx.recv_timeout(BALANCE_EVERY) {
                Ok(()) => all = true,
                Err(RecvTimeoutError::Timeout) => all = false,
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // 连按几次 `$` 只读一遍。
            while rx.try_recv().is_ok() {}
        }
    });
    req
}

/// 每 [`CONTEXT_EVERY`] 读一次各个 Claude 账号里每条对话的上下文占用，变了才报（全部重报）。
pub(super) fn context_worker(accounts: Arc<Vec<Account>>, tx: Sender<Msg>) {
    thread::spawn(move || {
        let claude: Vec<&Account> = accounts
            .iter()
            .filter(|a| a.vendor == Vendor::Claude)
            .collect();
        if claude.is_empty() {
            return;
        }
        let mut seen: Vec<quota::ContextFiles> =
            claude.iter().map(|_| Default::default()).collect();
        let mut last = HashMap::new();
        loop {
            let mut now = HashMap::new();
            for (a, seen) in claude.iter().zip(&mut seen) {
                for (id, c) in quota::contexts(a, seen) {
                    now.insert(SessionId::in_account(&a.key, &id), c);
                }
            }
            if now != last {
                if tx.send(Msg::Context(now.clone())).is_err() {
                    return;
                }
                last = now;
            }
            thread::sleep(CONTEXT_EVERY);
        }
    });
}

/// 每 [`PROCS_EVERY`] 看一次本机进程，把参数里带着会话 id 的报上来；`ps` 跑不了这一轮就跳过。
pub(super) fn procs_worker(tx: Sender<Msg>) {
    thread::spawn(move || {
        loop {
            if let Ok(out) = exec::run("/bin/ps", &["-axww", "-o", "command="], None, CHILD_TIMEOUT)
            {
                let ids = sources::live::session_ids(&out)
                    .into_iter()
                    .map(SessionId::from)
                    .collect();
                if tx.send(Msg::Procs(ids)).is_err() {
                    return;
                }
            }
            thread::sleep(PROCS_EVERY);
        }
    });
}

/// 交给各家 CLI 线程的请求。
pub(super) enum CliRequest {
    /// 顶栏「更新」：把能一键升的都升一遍。
    UpgradeAll,
}

/// 各家 CLI：起来时查一次，之后每小时再看一次（真去联网由 `clis` 的 20 小时缓存管），每轮把有新版的、
/// 能一键升的报上去；设置里开着自动升级时顺手把能自动升的升了（那家有托管对话在跑就等，10 分钟后再看）。
/// 收到 [`CliRequest::UpgradeAll`] 就一家家升、报进度。升级都在这一个线程里，同一个会话台不会同时跑两个升级。
/// 会话台开着时主人升了级，下一轮找到的就是新版本，标记跟着消失；主人在设置里关了「检查新版本」，
/// 下一轮起就不找、不联网（每轮先看一眼设置文件）。
pub(super) fn cli_versions_worker(
    home: PathBuf,
    data_dir: PathBuf,
    tx: Sender<Msg>,
) -> Sender<CliRequest> {
    const EVERY: Duration = Duration::from_secs(60 * 60);
    const RETRY: Duration = Duration::from_secs(10 * 60);
    let (req, rx) = mpsc::channel::<CliRequest>();
    thread::spawn(move || {
        let config = data_dir.join("config.json");
        loop {
            let snap = config_flag(&config, "check_updates", true)
                .then(|| clis::snapshot(&home, &data_dir))
                .flatten();
            let now = clis::now_secs();
            if tx
                .send(Msg::CliUpdates(cli_report(snap.as_ref(), now)))
                .is_err()
            {
                return;
            }
            let mut wait = EVERY;
            if let Some(snap) = snap
                .as_ref()
                .filter(|_| config_flag(&config, "auto_upgrade_clis", true))
            {
                let (results, pending) = auto_upgrade(snap, &home, &data_dir, now);
                if pending {
                    wait = RETRY;
                }
                if !results.is_empty() {
                    if tx.send(Msg::CliAuto(results)).is_err() {
                        return;
                    }
                    // 升完马上重查一遍，按钮和标记跟着变。
                    wait = Duration::ZERO;
                }
            }
            match rx.recv_timeout(wait) {
                Ok(CliRequest::UpgradeAll) => {
                    while rx.try_recv().is_ok() {}
                    let results = upgrade_all(&home, &data_dir, &tx);
                    if tx.send(Msg::CliDone(results)).is_err() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    req
}

/// 有新版的、能一键升的。
fn cli_report(snap: Option<&clis::Snapshot>, now: u64) -> CliReport {
    let statuses = snap.map_or(&[][..], |s| s.statuses.as_slice());
    CliReport {
        newer: statuses
            .iter()
            .filter_map(|s| Some((s.install.vendor, s.newer()?.to_owned())))
            .collect(),
        ready: statuses
            .iter()
            .filter(|s| s.upgrade_now(now).is_some())
            .map(|s| s.install.vendor)
            .collect(),
    }
}

/// 自动升级这一轮：能自动升的（[`clis::auto_due`]）、此刻没有托管对话在跑的那几家，一家家升。
/// 返回升了的结果，和还有没有因为对话在跑、锁被占着、问不到守护进程而等着的（等着的 10 分钟后再看）。
fn auto_upgrade(
    snap: &clis::Snapshot,
    home: &Path,
    data_dir: &Path,
    now: u64,
) -> (Vec<CliResult>, bool) {
    let last = clis::last_results(data_dir);
    let due: Vec<&clis::Status> = snap
        .statuses
        .iter()
        .filter(|s| clis::auto_due(s, now, last.get(&s.install.vendor)))
        .collect();
    if due.is_empty() {
        return (Vec::new(), false);
    }
    let Ok(hosted) = host::client::hosted_vendors(data_dir) else {
        return (Vec::new(), true);
    };
    let runner = clis::Runner {
        login: &snap.login,
        home,
        data_dir,
        timeout: clis::UPGRADE_TIMEOUT,
    };
    let mut pending = false;
    let mut out = Vec::new();
    for s in due {
        let vendor = s.install.vendor;
        if hosted.contains(&vendor) {
            pending = true;
            continue;
        }
        let Some(target) = s.newer() else {
            continue;
        };
        let r = clis::upgrade(&s.install, target, false, true, &runner);
        if r.is_err() {
            // 被挡住（它自己在更新、别的会话台在升）：没跑，过会儿再看。
            pending = true;
            continue;
        }
        let (ok, text) = clis::summary(&r);
        out.push(CliResult { vendor, ok, text });
    }
    (out, pending)
}

/// 一键升级：重新找一遍（最新版用缓存），把能一键升的一家家升，每升完一家报一次进度。
fn upgrade_all(home: &Path, data_dir: &Path, tx: &Sender<Msg>) -> Vec<CliResult> {
    let Some(snap) = clis::snapshot(home, data_dir) else {
        return Vec::new();
    };
    let now = clis::now_secs();
    let targets: Vec<&clis::Status> = snap
        .statuses
        .iter()
        .filter(|s| s.upgrade_now(now).is_some())
        .collect();
    // 守护进程答不上来就当每家都有对话在跑：npm 一类原地覆盖的这次不升。
    let hosted = host::client::hosted_vendors(data_dir)
        .unwrap_or_else(|_| Vendor::ALL.into_iter().collect());
    let runner = clis::Runner {
        login: &snap.login,
        home,
        data_dir,
        timeout: clis::UPGRADE_TIMEOUT,
    };
    let total = targets.len();
    let _ = tx.send(Msg::CliProgress(0, total));
    let mut out = Vec::new();
    for (i, s) in targets.into_iter().enumerate() {
        let vendor = s.install.vendor;
        let target = s.newer().unwrap_or_default();
        let r = clis::upgrade(&s.install, target, hosted.contains(&vendor), false, &runner);
        let (ok, text) = clis::summary(&r);
        out.push(CliResult { vendor, ok, text });
        let _ = tx.send(Msg::CliProgress(i + 1, total));
    }
    out
}

/// 设置文件里一个开关。只读这一项，不走 `Config::load`（它会顺手同步写 `accounts.json`，
/// 后台线程定时读不该写文件）；文件不在、读不了、看不懂都按默认 `default`，和界面读不到设置时一样。
fn config_flag(config: &Path, key: &str, default: bool) -> bool {
    std::fs::read(config)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get(key)?.as_bool())
        .unwrap_or(default)
}

/// 给 Claude 对话做备份：起来时备一轮，之后每 [`BACKUP_EVERY`] 一轮，收到请求（改了标注、存了设置）马上一轮。
/// 每轮先看一眼设置里「备份全部 Claude 对话」和各账号的 `cleanupPeriodDays`。备不成的原因变了才报到状态栏。
pub(super) fn backup_worker(
    index: Index,
    accounts: Arc<Vec<Account>>,
    data_dir: PathBuf,
    tx: Sender<Msg>,
) -> Sender<()> {
    let (req, rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        let config = data_dir.join("config.json");
        let mut checked = HashSet::new();
        let mut last_problems = Vec::new();
        loop {
            let all = config_flag(&config, "backup_all_claude", false);
            let retention = sources::claude::retention_by_account(&accounts);
            let r = index.back_up_claude(all, &retention, SystemTime::now(), &mut checked);
            if r.problems != last_problems {
                if let Some(p) = r.problems.first() {
                    let more = match r.problems.len() {
                        1 => String::new(),
                        n => format!("（共 {n} 条没备上）"),
                    };
                    if tx
                        .send(Msg::Status(format!("备份没做成：{p}{more}")))
                        .is_err()
                    {
                        return;
                    }
                }
                last_problems = r.problems;
            }
            match rx.recv_timeout(BACKUP_EVERY) {
                Ok(()) => while rx.try_recv().is_ok() {},
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    req
}

/// 告诉守护进程用户看过了哪条对话。守护进程没在跑就算了（它没在跑也就没有要提醒的）。
pub(super) fn seen_worker(dir: PathBuf) -> Sender<SessionId> {
    let (req, rx) = mpsc::channel::<SessionId>();
    thread::spawn(move || {
        for id in rx {
            let _ = host::client::request(&dir, &Request::Seen { id });
        }
    });
    req
}

/// 以会话台身份订阅守护进程的状态事件和代弹提醒；守护进程没在跑就每隔一会儿再试。
/// 守护进程是不认订阅的旧版时照样报「后台是旧版」，改为每隔 `HOSTED_POLL` 用 `List` 重读托管对话；
/// 它的对话都结束后轮询停下，下一轮把它换成新版。
/// 只有上一个守护进程留下了在跑记录（可能是断电、重启后第一次打开）时才主动拉起它，让它认出被中断的对话。
pub(super) fn host_watcher(ctx: Ctx, board: BoardInfo, away: Arc<Mutex<bool>>, tx: Sender<Msg>) {
    thread::spawn(move || {
        let dir = ctx.data_dir.clone();
        let left = host::interrupt::load(&host::interrupt::running_path(&dir));
        if left.is_ok_and(|l| !l.is_empty())
            && let Err(e) = host::client::ensure_daemon(&dir, ctx_launch(&ctx.exe))
        {
            let _ = tx.send(Msg::Status(format!(
                "守护进程没起来，认不出被中断的对话：{e}"
            )));
        }
        loop {
            let old = old_daemon(&ctx, &tx);
            if let Ok(mut feed) = host::client::board_feed(&dir, board.clone(), HOSTED_POLL) {
                if tx.send(Msg::OldHost(old || feed.polling())).is_err() {
                    return;
                }
                if let Ok(Response::List(infos)) = host::client::request(&dir, &Request::List)
                    && tx.send(Msg::Hosted(infos)).is_err()
                {
                    return;
                }
                if let Ok(list) = interrupted_list(&dir)
                    && tx.send(Msg::Interrupted(list)).is_err()
                {
                    return;
                }
                while let Ok(Some(item)) = feed.recv() {
                    let sent = match item {
                        BoardItem::Event(ev) => pass_host_event(ev, &away, &tx, &system_alert),
                        BoardItem::Hosted(infos) => tx.send(Msg::Hosted(infos)).map_err(|_| ()),
                    };
                    if sent.is_err() {
                        return;
                    }
                }
                if tx.send(Msg::HostGone).is_err() {
                    return;
                }
            }
            thread::sleep(HOST_RETRY);
        }
    });
}

/// 在跑的守护进程比当前程序旧吗。旧版且空着就顺手换成新版（换不成在底栏说一声）。没在跑算不旧。
fn old_daemon(ctx: &Ctx, tx: &Sender<Msg>) -> bool {
    match host::probe(&ctx.data_dir) {
        Ok(info) if info.replaceable() => {
            match host::ensure_daemon(&ctx.data_dir, ctx_launch(&ctx.exe)) {
                Ok(info) => info.outdated(),
                Err(e) => {
                    let _ = tx.send(Msg::Status(format!("后台换新版没成功：{e}")));
                    true
                }
            }
        }
        Ok(info) => info.outdated(),
        Err(_) => false,
    }
}

/// 托管事件交给主循环；会话台交出终端期间（`away`）主循环收不到，代弹提醒改交 `fallback`（系统通知）。
/// 判断和转发持着同一把锁，`Loop::suspend` 置位后再清队列，两边不会漏掉或压住提醒。主循环没了返回错误。
pub(super) fn pass_host_event(
    ev: HostEvent,
    away: &Mutex<bool>,
    tx: &Sender<Msg>,
    fallback: &dyn Fn(Message),
) -> Result<(), ()> {
    let away = lock(away);
    match ev {
        HostEvent::Alert { title, body, .. } if *away => {
            fallback(Message { title, body });
            Ok(())
        }
        ev => tx.send(Msg::Host(ev, SystemTime::now())).map_err(|_| ()),
    }
}

/// 系统通知，放到别的线程发（osascript 最多等 5 秒，不卡住调用方）。
pub(super) fn system_alert(msg: Message) {
    thread::spawn(move || {
        if let Err(e) = SystemNotifier.show(&msg) {
            eprintln!("huitai: 系统通知没发出去: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 主人在设置里关了「检查新版本」后，后台线程下一轮就不再找各家、不再联网（审查退回第 1 轮）。
    #[test]
    fn cli_checks_follow_the_saved_setting() {
        let dir = std::env::temp_dir().join(format!("huitai-cli-checks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.json");
        let cli_checks_on = |c: &Path| config_flag(c, "check_updates", true);
        assert!(cli_checks_on(&config), "没有设置文件按默认开");
        assert!(
            config_flag(&config, "auto_upgrade_clis", true),
            "自动升级默认开"
        );
        Config {
            check_updates: false,
            ..Config::default()
        }
        .save(&config)
        .unwrap();
        assert!(!cli_checks_on(&config), "存了关就不查");
        Config::default().save(&config).unwrap();
        assert!(cli_checks_on(&config));
        std::fs::write(&config, "坏了").unwrap();
        assert!(cli_checks_on(&config), "看不懂按默认开");
        Config {
            auto_upgrade_clis: false,
            ..Config::default()
        }
        .save(&config)
        .unwrap();
        assert!(
            !config_flag(&config, "auto_upgrade_clis", true),
            "关了自动升级就不自动升"
        );
        assert!(cli_checks_on(&config));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
