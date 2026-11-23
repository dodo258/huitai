//! 升级：按装法跑官方命令（不用 sudo）。跑之前看计划能不能由会话台跑（目录可写、npm 前缀对得上、不跨大版本，
//! 见 `plan`）、那家自带更新器的锁、会话台自己的锁、这家此刻有没有在跑的（托管的对话和本机别处的进程），
//! 再定下装哪一版（能指定版本的就装查到的那一版，不能的马上重查最新版）；超时结束整组进程，失败把命令输出原文交回；跑完重新在 PATH 上找一遍，确认排第一个的就是新版。
//! 哪些家能一键升、能自动升也在这里定。

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::core::{Vendor, exec, release};

use super::check::{self, Status};
use super::detect::Install;
use super::latest::Bases;
use super::locate::{self, LoginEnv};
use super::plan::{How, plan, takes_version};
use super::running;

/// 一次升级最多跑多久（npm、brew、安装脚本下载慢时也够）。
pub const UPGRADE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// 自带自动更新的那几家：新版出来这么久本机还是旧版，就当它的自带更新没生效，会话台照常升。
pub const SELF_UPDATE_GRACE: u64 = 24 * 60 * 60;
/// 自带更新器的锁多久内算它正在更新（Claude Code 自己把 5 分钟前的锁当作残留，这里一样）。
const LOCK_FRESH: Duration = Duration::from_secs(5 * 60);
/// 升级记录：每家最后一次升级（自动或手动）的结果，`ai doctor` 和自动升级要看。
pub const RESULTS_FILE: &str = "cli-upgrades.json";
/// 失败原文最多留多少字（npm 出错能打几千行，留最后这一段）。
const OUTPUT_KEEP: usize = 4000;
/// 说「别处有进程在跑」时最多列几个进程号。
const PIDS_SHOWN: usize = 3;

impl Status {
    /// 一键升级和自动升级用的那条命令：有新版、会话台能替它跑（[`How::Ours`]）；自带自动更新的那几家
    /// （[`How::SelfUpdates`]）先交给它们自己，新版出来满 24 小时本机还是旧版才算。只给命令的（[`How::Manual`]）不算。
    pub fn upgrade_now(&self, now: u64) -> Option<String> {
        let latest = self.newer()?;
        let p = plan(&self.install, Some(latest));
        match p.how {
            How::Ours => p.command,
            How::SelfUpdates if self.overdue(now) => p.command,
            How::SelfUpdates | How::Manual(_) => None,
        }
    }

    /// 新版出来满 24 小时了（从第一次查到它算）。
    pub fn overdue(&self, now: u64) -> bool {
        self.seen_at
            .is_some_and(|at| now.saturating_sub(at) >= SELF_UPDATE_GRACE)
    }
}

/// 自动升级这一轮要不要升这家：能一键升（[`Status::upgrade_now`]），且这个新版还没自动升过
/// （同一版只自动试一次：失败了留给主人按「更新」或看 `ai doctor`；成了却还查到旧版也不反复跑）。
/// 有没有在跑的，轮到它升时 [`upgrade`] 再看。
pub fn auto_due(status: &Status, now: u64, last: Option<&Outcome>) -> bool {
    let Some(latest) = status.newer() else {
        return false;
    };
    status.upgrade_now(now).is_some() && !last.is_some_and(|o| o.auto && o.target == latest)
}

/// 跑过的一次升级。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub vendor: Vendor,
    /// 升级前的版本。
    #[serde(default)]
    pub from: Option<String>,
    /// 要升到的版本。
    pub target: String,
    pub command: String,
    pub ok: bool,
    /// 成功是一句话（「已升到 2.1.331」）；失败是原因加命令输出原文。
    pub message: String,
    /// 什么时候跑完的（Unix 秒）。
    pub at: u64,
    /// 是自动升级跑的。
    #[serde(default)]
    pub auto: bool,
}

/// 没跑：被挡住了（只给命令的、对话在用、有人正在升）。`command` 是让主人自己跑的那条（有的话）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub vendor: Vendor,
    pub reason: String,
    pub command: Option<String>,
}

/// 一家升级的结果写成给人看的（不带厂商名）：成没成，没成的带命令和原文、或让主人自己跑的命令。
pub fn summary(r: &Result<Outcome, Blocked>) -> (bool, String) {
    match r {
        Ok(o) if o.ok => (true, o.message.clone()),
        Ok(o) => (false, format!("没升上：{}", o.message)),
        Err(b) => match &b.command {
            Some(c) => (false, format!("没升：{}。自己跑：{c}", b.reason)),
            None => (false, format!("没升：{}", b.reason)),
        },
    }
}

/// 跑升级要用的本机情况。
pub struct Runner<'a> {
    /// PATH 和 npm 前缀（升级命令和复查都用它）。
    pub login: &'a LoginEnv,
    pub home: &'a Path,
    /// 会话台数据目录：自己的锁和升级记录放这里。
    pub data_dir: &'a Path,
    pub timeout: Duration,
    /// 此刻有会话台托管的对话在跑的那几家；问不到是 `Err`。每轮到一家现问一次，一批升到一半开的对话也看得到。
    pub hosted: &'a dyn Fn() -> io::Result<HashSet<Vendor>>,
    /// 本机此刻的进程（[`running::processes`]），也是每家现查。
    pub processes: &'a dyn Fn() -> io::Result<String>,
}

/// 把 `install` 升到 `target`（查到的最新版；装法不能指定版本的会重查，装的是重查到的那一版，跨大版本就不升）。
/// 这家此刻有托管对话、或本机别处有它的进程在跑就不升（换了文件，在跑的后读到新版会出错；Homebrew 会删旧版），
/// 查不了也不升。
/// 跑了就把结果记进 [`RESULTS_FILE`]。
pub fn upgrade(
    install: &Install,
    target: &str,
    auto: bool,
    r: &Runner,
) -> Result<Outcome, Blocked> {
    upgrade_from(install, target, auto, r, &Bases::official())
}

/// 同 [`upgrade`]，重查最新版问 `bases`（测试换成本地假服务器）。
pub fn upgrade_from(
    install: &Install,
    target: &str,
    auto: bool,
    r: &Runner,
    bases: &Bases,
) -> Result<Outcome, Blocked> {
    let command = runnable(install, target)?;
    let blocked = |reason: String| Blocked {
        vendor: install.vendor,
        reason,
        command: Some(command.clone()),
    };
    if let Some(lock) = self_update_lock(install.vendor, r.home) {
        return Err(blocked(format!(
            "它自己正在更新（{} 是 5 分钟内的），过会儿再试",
            lock.display()
        )));
    }
    let _guard = OwnLock::take(r.data_dir, install.vendor).map_err(blocked)?;
    // 排队升前面几家、等锁的工夫，它可能自己更新了：写死版本的命令会把它装回旧版，所以重新认一遍再定。
    let install = &locate::probe_one(install.vendor, &install.path, r.login, r.home);
    if let Some(why) = in_use(install, r) {
        return Err(blocked(why));
    }
    let target = if takes_version(install) {
        target.to_owned()
    } else {
        // 装上的是它那边此刻的最新版，`target` 可能是 20 小时前查的。
        check::recheck(
            r.data_dir,
            install,
            super::now_secs(),
            bases,
            check::TIMEOUT,
        )
        .ok_or_else(|| {
            blocked("升之前没查到它此刻的最新版，怕装上没看过的大版本，这次不升".to_owned())
        })?
    };
    if !install.older_than(&target) {
        let current = install.version.as_deref().unwrap_or("版本不明");
        return Err(Blocked {
            command: None,
            ..blocked(format!("本机已是 {current}，最新版 {target} 不比它新"))
        });
    }
    let command = runnable(install, &target)?;
    let (ok, message) = match run(&command, r) {
        Ok(out) => verify(install, &target, r, &out),
        Err(e) => (false, e),
    };
    let outcome = Outcome {
        vendor: install.vendor,
        from: install.version.clone(),
        target,
        command,
        ok,
        message,
        at: super::now_secs(),
        auto,
    };
    // 记不下只是 doctor 看不到这一次、自动升级可能再试一次，不影响升级本身。
    let _ = record(r.data_dir, &outcome);
    Ok(outcome)
}

/// 这家此刻有没有在跑的：有就是不升的原因。问不到托管列表、查不了进程都当在跑。
fn in_use(install: &Install, r: &Runner) -> Option<String> {
    match (r.hosted)() {
        Ok(hosted) if hosted.contains(&install.vendor) => {
            return Some(
                "有会话台托管的对话在用它，这时换文件会让在跑的出错，等对话结束再升".to_owned(),
            );
        }
        Ok(_) => {}
        Err(e) => {
            return Some(format!(
                "问不到有没有会话台托管的对话在用它（{e}），这次不升"
            ));
        }
    }
    let ps = match (r.processes)() {
        Ok(ps) => ps,
        Err(e) => {
            return Some(format!("查不了本机有没有它的进程在跑（{e}），这次不升"));
        }
    };
    let pids = running::pids(&ps, install, r.home);
    (!pids.is_empty()).then(|| {
        let shown = pids[..pids.len().min(PIDS_SHOWN)].join("、");
        let more = if pids.len() > PIDS_SHOWN { " 等" } else { "" };
        format!(
            "别处有 {} 个它的进程在跑（进程号 {shown}{more}），这时换文件会让在跑的出错，等它们结束再升",
            pids.len()
        )
    })
}

/// 把 `install` 升到 `target` 要跑的命令；只给命令、不替主人跑的是 `Err`。
fn runnable(install: &Install, target: &str) -> Result<String, Blocked> {
    let p = plan(install, Some(target));
    let (reason, command) = match (p.how, p.command) {
        (How::Ours | How::SelfUpdates, Some(c)) => return Ok(c),
        (How::Manual(why), command) => (why, command),
        (_, None) => ("认不出怎么升级".to_owned(), None),
    };
    Err(Blocked {
        vendor: install.vendor,
        reason,
        command,
    })
}

/// 用 `sh -c` 跑官方命令（有 `curl … | sh` 这种），PATH 用登录 shell 的，在家目录里跑，标准输入是空的
/// （要交互的安装器读到结尾会自己退出，不会卡住）。成功交回输出原文；失败、超时交回原因和原文。
fn run(command: &str, r: &Runner) -> Result<String, String> {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", command]).current_dir(r.home);
    if let Ok(path) = std::env::join_paths(&r.login.path) {
        cmd.env("PATH", path);
    }
    match exec::output(&mut cmd, r.timeout) {
        Ok(out) if out.success() => Ok(both(&out)),
        Ok(out) => Err(format!(
            "{command} {}：\n{}",
            out.code_text(),
            tail(&both(&out))
        )),
        Err(e) if e.kind() == io::ErrorKind::TimedOut => {
            let limit = match r.timeout.as_secs() {
                s if s >= 60 => format!("{} 分钟", s / 60),
                _ => format!("{:.1} 秒", r.timeout.as_secs_f64()),
            };
            Err(format!("{command} 超过 {limit}没跑完，已结束"))
        }
        Err(e) => Err(format!("{command} 没跑起来：{e}")),
    }
}

/// 跑完重新在 PATH 上找：排第一个的版本到了 `target` 才算成。没到时看后面有没有新版（被前面的遮住了），说清楚。
fn verify(install: &Install, target: &str, r: &Runner, out: &str) -> (bool, String) {
    let found = locate::which_all(super::program(install.vendor), &r.login.path);
    let mut copies = found
        .iter()
        .map(|p| locate::probe_one(install.vendor, p, r.login, r.home));
    let Some(first) = copies.next() else {
        return (
            false,
            format!(
                "命令跑完了，可 PATH 上找不到 {} 了：\n{}",
                super::program(install.vendor),
                tail(out)
            ),
        );
    };
    let reached = |v: &Option<String>| v.as_deref().is_some_and(|v| !release::is_newer(target, v));
    if reached(&first.version) {
        let v = first.version.as_deref().unwrap_or(target);
        return (true, format!("已升到 {v}"));
    }
    let now = first.version.as_deref().unwrap_or("版本不明");
    if let Some(new) = copies.find(|c| reached(&c.version)) {
        return (
            false,
            format!(
                "新版装到了 {}，可 PATH 上排在前面的 {} 还是 {now}，被它遮住了；调整 PATH 或删掉旧的那份",
                new.path.display(),
                first.path.display()
            ),
        );
    }
    (
        false,
        format!(
            "命令跑完了，可 PATH 上的 {} 还是 {now}（要 {target}）：\n{}",
            first.path.display(),
            tail(out)
        ),
    )
}

fn both(out: &exec::Output) -> String {
    let mut s = out.stdout.trim_end().to_owned();
    let err = out.stderr.trim_end();
    if !err.is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(err);
    }
    s
}

/// 只留最后 [`OUTPUT_KEEP`] 个字。
fn tail(text: &str) -> String {
    let n = text.chars().count();
    if n <= OUTPUT_KEEP {
        return text.to_owned();
    }
    let rest: String = text.chars().skip(n - OUTPUT_KEEP).collect();
    format!("…\n{rest}")
}

/// 这家自带更新器此刻拿着的锁（5 分钟内写过的）。Claude Code：`~/.claude/.update.lock`（它的程序里读出来的，
/// 5 分钟后它自己当残留删掉）；Kimi Code：`~/.kimi-code/updates/install.lock`；Antigravity：
/// `~/.gemini/antigravity-cli/updater/update.lock`。别的家没有自带安装，不用看。
pub fn self_update_lock(vendor: Vendor, home: &Path) -> Option<PathBuf> {
    let rel = match vendor {
        Vendor::Claude => ".claude/.update.lock",
        Vendor::Kimi => ".kimi-code/updates/install.lock",
        Vendor::Antigravity => ".gemini/antigravity-cli/updater/update.lock",
        _ => return None,
    };
    let path = home.join(rel);
    let modified = fs::metadata(&path).ok()?.modified().ok()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    (age < LOCK_FRESH).then_some(path)
}

/// 会话台自己的升级锁（数据目录里 `cli-upgrade-<家>.lock`，写着进程号）：两个会话台、或会话台和
/// `ai update` 不会同时升同一家。拿锁的进程已经不在了就当残留拿过来；放手时删掉。
struct OwnLock(PathBuf);

impl OwnLock {
    fn take(data_dir: &Path, vendor: Vendor) -> Result<Self, String> {
        let path = data_dir.join(format!("cli-upgrade-{}.lock", vendor.key()));
        let _ = fs::create_dir_all(data_dir);
        for _ in 0..2 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    let _ = write!(f, "{}", std::process::id());
                    return Ok(Self(path));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let holder = fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| s.trim().parse::<i32>().ok());
                    if holder.is_some_and(alive) {
                        return Err("另一个会话台或 ai update 正在升级它".to_owned());
                    }
                    let _ = fs::remove_file(&path);
                }
                Err(e) => return Err(format!("升级锁 {} 建不了：{e}", path.display())),
            }
        }
        Err(format!("升级锁 {} 拿不到", path.display()))
    }
}

impl Drop for OwnLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// 进程还在（别人的进程发不了信号也算在）。
fn alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: 信号 0 只检查进程在不在，不发信号、不碰内存。
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// 每家最后一次升级的结果。读不了是空的。
pub fn last_results(data_dir: &Path) -> BTreeMap<Vendor, Outcome> {
    fs::read(data_dir.join(RESULTS_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<Outcome>>(&b).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|o| (o.vendor, o))
        .collect()
}

/// 记下一次升级（换掉这家上一次的）。先写临时文件再改名。
fn record(data_dir: &Path, outcome: &Outcome) -> io::Result<()> {
    let mut all = last_results(data_dir);
    all.insert(outcome.vendor, outcome.clone());
    let list: Vec<&Outcome> = all.values().collect();
    let path = data_dir.join(RESULTS_FILE);
    let tmp = data_dir.join(format!(".{RESULTS_FILE}.{}", std::process::id()));
    fs::create_dir_all(data_dir)?;
    fs::write(&tmp, serde_json::to_vec_pretty(&list)?)?;
    fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}
