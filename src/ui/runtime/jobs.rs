//! 动作线程：开标签页、新建、恢复、删除这些会动外部的活（[`Job`]），由 [`Worker`] 一件件做。

use super::*;

/// 交给动作线程的活。
pub(super) enum Job {
    Open(Box<Entry>),
    New(Vendor, PathBuf),
    RestoreTabs,
    ResumeInterrupted(Vec<Entry>),
    Dismiss(SessionId),
    /// 列表里被中断的这条已经在它自己的 id 下重新跑起来了：把守护进程用别的 id（临时 id）记的那条被中断记录清掉。
    Restarted(SessionId),
    /// 补取托管列表（交回 [`Msg::Learned`]）。
    Learn,
    Kill(SessionId),
    Trash(Box<Entry>),
    Untrash(Vendor),
    Finder(PathBuf),
    Git(PathBuf),
    Copy(Box<Entry>),
    Export(Box<Entry>),
}

/// 开标签页（已开着就切过去）；既没开出也没切到任何标签页、却失败了，就调 `undo` 撤掉为它做的准备，
/// 并把撤销没成的原因接在错误后面。标签页开了、只是记录没存下的失败不撤：那个标签页里的 attach 还要用这条登记。
pub(super) fn open_or_undo(
    tabs: io::Result<Tabs>,
    opener: &dyn Opener,
    id: &SessionId,
    cmd: &Command,
    title: &str,
    undo: impl FnOnce() -> io::Result<()>,
) -> io::Result<Outcome> {
    let watched = Watched {
        inner: opener,
        reached: Cell::new(false),
    };
    let result = tabs.and_then(|mut t| t.open_or_focus(&watched, id, cmd, title));
    let Err(e) = result else {
        return result;
    };
    if watched.reached.get() {
        return Err(e);
    }
    match undo() {
        Ok(()) => Err(e),
        Err(u) => Err(io::Error::new(
            e.kind(),
            format!("{e}（托管登记没撤掉：{u}）"),
        )),
    }
}

/// 记下有没有真的开出或切到一个标签页。
struct Watched<'a> {
    inner: &'a dyn Opener,
    reached: Cell<bool>,
}

impl Opener for Watched<'_> {
    fn open(&self, cmd: &Command, title: &str) -> io::Result<opener::Opened> {
        let r = self.inner.open(cmd, title);
        if r.is_ok() {
            self.reached.set(true);
        }
        r
    }

    fn focus(&self, handle: &opener::TabHandle) -> io::Result<bool> {
        let r = self.inner.focus(handle);
        if matches!(r, Ok(true)) {
            self.reached.set(true);
        }
        r
    }
}

/// 开标签页、结束、回收站这类可能慢的动作，排队在一个线程里做，结果用一句话报回。
pub(super) fn job_worker(ctx: Ctx, index: Index, tx: Sender<Msg>) -> Sender<Job> {
    let (req, rx) = mpsc::channel::<Job>();
    thread::spawn(move || {
        let opener = opener::opener_for(&ctx.terminal, |k| std::env::var(k).ok());
        let mut worker = Worker {
            ctx,
            index,
            opener,
            tx,
        };
        for job in rx {
            let msg = worker.run(job);
            if worker.tx.send(msg).is_err() {
                return;
            }
        }
    });
    req
}

pub(super) struct Worker {
    pub(super) ctx: Ctx,
    pub(super) index: Index,
    pub(super) opener: Box<dyn Opener>,
    pub(super) tx: Sender<Msg>,
}

impl Worker {
    fn tabs(&self) -> io::Result<Tabs> {
        Tabs::load(&self.ctx.data_dir.join("tabs.json"))
    }

    fn run(&mut self, job: Job) -> Msg {
        let result = match job {
            Job::Git(dir) => {
                let args = [
                    OsStr::new("-C"),
                    dir.as_os_str(),
                    "status".as_ref(),
                    "--short".as_ref(),
                ];
                let out = exec::run("git", &args, None, CHILD_TIMEOUT).map_err(|e| e.to_string());
                return Msg::Git(dir, out);
            }
            Job::Open(e) => self.open(&e),
            Job::New(v, cwd) => self.new_session(v, &cwd),
            Job::RestoreTabs => self.restore_tabs(),
            Job::ResumeInterrupted(entries) => self.resume_interrupted(&entries),
            Job::Dismiss(id) => self.dismiss(&[id]).map(|()| "已移出「被中断」".to_owned()),
            Job::Restarted(id) => match self.dismiss_quietly(&[id]) {
                Ok(list) => return Msg::Interrupted(list),
                Err(e) => Err(e),
            },
            Job::Learn => return Msg::Learned(hosted_list(&self.ctx)),
            Job::Kill(id) => self.kill(&id),
            Job::Trash(e) => self.trash(&e),
            Job::Untrash(v) => self.untrash(v),
            Job::Finder(dir) => exec::run("open", &[&dir], None, CHILD_TIMEOUT)
                .map(|_| "已在 Finder 打开".to_owned()),
            Job::Copy(e) => self.copy_resume(&e),
            Job::Export(e) => self.export(&e),
        };
        Msg::Status(result.unwrap_or_else(|e| format!("没做成：{e}")))
    }

    /// `c`：恢复命令（厂商 CLI 自己的，不经会话台托管）写进剪贴板。
    fn copy_resume(&self, e: &Entry) -> io::Result<String> {
        let s = &e.session;
        let cmd = source_of(&self.ctx.sources()?, s)?.resume_cmd(s);
        let env = self.ctx.env_prefix(s.vendor, &s.id);
        pbcopy(&resume_line(&cmd, &env))?;
        Ok("已复制恢复命令".to_owned())
    }

    /// 查看视图里 `s`：存成 Markdown 放进 `~/Downloads`，状态栏写存到哪了。
    fn export(&self, e: &Entry) -> io::Result<String> {
        let s = &e.session;
        let md = export::render(
            source_of(&self.ctx.sources()?, s)?,
            e,
            &self.ctx.accounts,
            &self.ctx.label(&s.cwd),
        )?;
        let stem = export::file_stem(e.title(), s.last_active, &export::local_offset);
        let path = export::save(&export::downloads(&self.ctx.home), &stem, &md)?;
        Ok(format!("已存到 {}", tilde(&path, &self.ctx.home)))
    }

    pub(super) fn open(&self, e: &Entry) -> io::Result<String> {
        let s = &e.session;
        let (id, cmd, fresh) = attach_existing(&self.ctx, &self.ctx.sources()?, s)?;
        let tab = opener::tab_title(s.vendor, &self.ctx.label(&s.cwd), Some(e.title()));
        // 被中断的单条 ⏎ 只打开不发话；CLI 真起来了守护进程才把它移出被中断（见 `Loop::host_event`）。
        let outcome = self.open_tab(&id, &cmd, &tab, fresh)?;
        let title = truncate(e.title(), 24);
        Ok(match outcome {
            Outcome::Focused => format!("已切到「{title}」"),
            Outcome::Opened => format!("已在新标签页打开「{title}」"),
            Outcome::Inline => "已回到会话台".to_owned(),
        })
    }

    pub(super) fn new_session(&self, v: Vendor, cwd: &Path) -> io::Result<String> {
        let existing = ids_in(&self.index, v, cwd);
        let (id, cmd) = register_new(&self.ctx, &self.ctx.sources()?, v, cwd, existing)?;
        let tab = opener::tab_title(v, &self.ctx.label(cwd), None);
        self.open_tab(&id, &cmd, &tab, true)?;
        Ok(format!("已在 {} 新开 {v} 对话", self.ctx.label(cwd)))
    }

    /// 把上次开着的标签全部重开。守护进程可能重启过，先把每条重新登记托管。
    fn restore_tabs(&self) -> io::Result<String> {
        let records = self.tabs()?.records().clone();
        if records.is_empty() {
            return Ok("没有记着的标签页".to_owned());
        }
        let sources = self.ctx.sources()?;
        let hosted: HashSet<SessionId> = hosted_list(&self.ctx).into_iter().map(|h| h.id).collect();
        for v in Vendor::ALL {
            for e in self.index.list(v)? {
                if records.contains_key(&e.session.id) {
                    register_resume(&self.ctx, &sources, &e.session)?;
                }
            }
        }
        // 一条一条开，一条失败不影响别的；没开成的撤掉这次新登记的托管。
        let failed = records
            .iter()
            .filter(|(id, rec)| {
                let fresh = !hosted.contains(*id);
                self.open_tab(id, &rec.command(), &rec.title, fresh)
                    .is_err()
            })
            .count();
        Ok(match failed {
            0 => format!("已恢复 {} 个标签页", records.len()),
            n => format!("恢复了 {} 个，{n} 个没打开", records.len() - n),
        })
    }

    /// R：被中断的对话全部在新标签页重开，命令带那句「继续上次没做完的事……」。
    /// 开了标签页不算恢复：CLI 真起来了，守护进程才把它移出被中断（见 `Loop::host_event`）；
    /// attach 没接上、CLI 没起来的，留在被中断里还能再按 R。
    pub(super) fn resume_interrupted(&self, entries: &[Entry]) -> io::Result<String> {
        let sources = self.ctx.sources()?;
        let hosted: HashSet<SessionId> = hosted_list(&self.ctx).into_iter().map(|h| h.id).collect();
        let mut opened = Vec::new();
        let mut failed = Vec::new();
        for e in entries {
            let s = &e.session;
            let tab = opener::tab_title(s.vendor, &self.ctx.label(&s.cwd), Some(e.title()));
            let result = register_continue(&self.ctx, &sources, s).and_then(|(cmd, delivery)| {
                self.open_tab(&s.id, &cmd, &tab, !hosted.contains(&s.id))
                    .map(|_| delivery)
            });
            match result {
                Ok(delivery) => opened.push((s.vendor, delivery)),
                Err(err) => failed.push(format!("「{}」{err}", truncate(e.title(), 16))),
            }
        }
        Ok(resume_report(&opened, &failed))
    }

    /// 开标签页（已开着就切过去）。`fresh`：这次才登记的托管；标签页没开成时撤掉它，
    /// 不留一条永远没人来 attach 的登记（它会挡住旧守护进程换新版退出）。
    fn open_tab(
        &self,
        id: &SessionId,
        cmd: &Command,
        title: &str,
        fresh: bool,
    ) -> io::Result<Outcome> {
        open_or_undo(self.tabs(), self.opener.as_ref(), id, cmd, title, || {
            if fresh {
                self.cancel_unstarted(id)
            } else {
                Ok(())
            }
        })
    }

    /// 撤掉一条还没起过进程的托管登记；已经起了进程的（标签页其实开了、attach 接上了）不动。
    fn cancel_unstarted(&self, id: &SessionId) -> io::Result<()> {
        let unstarted = hosted_list(&self.ctx)
            .iter()
            .any(|h| h.id == *id && h.pid.is_none());
        if !unstarted {
            return Ok(());
        }
        lock(&self.ctx.known).remove(id);
        match host::client::request(&self.ctx.data_dir, &Request::Kill { id: id.clone() })? {
            Response::Ok => Ok(()),
            Response::Error(e) => Err(io::Error::other(e)),
            other => Err(io::Error::other(format!("守护进程回答不对：{other:?}"))),
        }
    }

    /// 把这些对话（列表里的会话 id）移出被中断，再把最新的被中断列表报给界面。
    fn dismiss(&self, ids: &[SessionId]) -> io::Result<()> {
        let list = self.dismiss_quietly(ids)?;
        let _ = self.tx.send(Msg::Interrupted(list));
        Ok(())
    }

    /// 把这些对话（列表里的会话 id）移出被中断，返回最新的被中断列表。
    fn dismiss_quietly(&self, ids: &[SessionId]) -> io::Result<Vec<RunRecord>> {
        let host_ids: Vec<SessionId> = {
            let map = lock(&self.ctx.interrupted);
            ids.iter()
                .map(|id| map.get(id).unwrap_or(id).clone())
                .collect()
        };
        match host::client::request(&self.ctx.data_dir, &Request::Dismiss { ids: host_ids })? {
            Response::Ok => {}
            Response::Error(e) => return Err(io::Error::other(e)),
            other => return Err(io::Error::other(format!("守护进程回答不对：{other:?}"))),
        }
        interrupted_list(&self.ctx.data_dir)
    }

    fn kill(&self, id: &SessionId) -> io::Result<String> {
        match host::client::request(&self.ctx.data_dir, &Request::Kill { id: id.clone() })? {
            Response::Ok => {
                self.tabs()?.forget(id)?;
                Ok("已结束后台进程".to_owned())
            }
            Response::Error(e) => Err(io::Error::other(e)),
            other => Err(io::Error::other(format!("守护进程回答不对：{other:?}"))),
        }
    }

    fn trash(&self, e: &Entry) -> io::Result<String> {
        let sources = self.ctx.sources()?;
        let entry = source_of(&sources, &e.session)?.trash(&e.session)?;
        self.relist(e.session.vendor);
        let title = truncate(e.title(), 24);
        Ok(if entry.hide_only {
            format!("已隐藏「{title}」，u 恢复")
        } else {
            format!("「{title}」已进回收站，u 恢复")
        })
    }

    /// 恢复这家最近进回收站的一条。
    fn untrash(&self, v: Vendor) -> io::Result<String> {
        let trash = Trash::default_location()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "取不到回收站位置"))?;
        let Some(entry) = trash.list()?.into_iter().find(|t| t.vendor == v) else {
            return Ok(format!("回收站里没有 {v} 的对话"));
        };
        let sources = self.ctx.sources()?;
        source_for(&sources, v)?.restore(&entry)?;
        self.relist(v);
        let title = self
            .index
            .list(v)?
            .into_iter()
            .find(|e| e.session.id.native() == entry.id)
            .map(|e| truncate(e.title(), 24))
            .unwrap_or(entry.id);
        Ok(format!("已恢复「{title}」"))
    }

    fn relist(&self, v: Vendor) {
        self.index.refresh_vendor(v);
        let _ = self.tx.send(Msg::Index(Update::Listed(v)));
    }
}

/// 把文字放进系统剪贴板。
fn pbcopy(text: &str) -> io::Result<()> {
    let no_args: [&str; 0] = [];
    exec::run("pbcopy", &no_args, Some(text), CHILD_TIMEOUT).map(drop)
}
