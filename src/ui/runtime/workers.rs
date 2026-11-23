//! 后台线程：预览、此刻、整段对话、命中片段、托管画面、额度、余额、进程、看过、守护进程事件。
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
