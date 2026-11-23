//! 各家 CLI 本身：认本机是怎么装的、当前版本，按装法查最新版（20 小时缓存、10 秒超时、失败不报），
//! 说清会怎么升级，能升的替主人跑官方升级命令（不用 sudo）。只依赖 `core`。
//!
//! - `locate`：拿 PATH（`ai doctor` 问登录 shell，会话台用自己的），找命令、补版本。
//! - `detect`：按路径认装法、读版本、看能不能写。
//! - `latest`：每种装法跟哪个数据源比、怎么解析。
//! - `check`：缓存和并行查询。
//! - `plan`：官方升级命令和为什么只提示。
//! - `upgrade`：跑官方升级命令（锁、超时、失败原文、跑完复查 PATH），哪些能一键升、自动升。

mod check;
mod detect;
mod latest;
mod locate;
mod plan;
mod upgrade;

pub use check::{CACHE_FILE, CHECK_EVERY, Status, TIMEOUT, check};
pub use detect::{Env, Install, Manager, Method, Problem, detect, program, version_in};
pub use latest::{Bases, Feed, feed_for};
pub use locate::{
    LoginEnv, current_env, login_env, login_script, parse_login, probe, probe_one, which, which_all,
};
pub use plan::{How, Plan, plan};
pub use upgrade::{
    Blocked, Outcome, RESULTS_FILE, Runner, SELF_UPDATE_GRACE, UPGRADE_TIMEOUT, auto_due,
    last_results, self_update_lock, summary, upgrade,
};

use std::path::Path;
use std::time::SystemTime;

use crate::core::release;

/// 本机各家此刻的样子：用哪份 PATH 找的，和每家查完的结果。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub login: LoginEnv,
    pub statuses: Vec<Status>,
}

/// 会话台后台用：没被 `HUITAI_NO_UPDATE_CHECK` 关掉时，用本进程的 PATH 找一遍各家（见 [`current_env`]）、
/// 查最新版（设置里的开关由调用方看）。被环境变量关掉是 `None`；别的哪一步失败都只是少几家，不报错。
pub fn snapshot(home: &Path, data_dir: &Path) -> Option<Snapshot> {
    let off = std::env::var(release::NO_CHECK_ENV).ok();
    if release::disabled_by_env(off.as_deref()) {
        return None;
    }
    let login = current_env();
    let statuses = check(
        data_dir,
        probe(&login, home),
        now_secs(),
        &Bases::official(),
        TIMEOUT,
    );
    Some(Snapshot { login, statuses })
}

/// 现在（Unix 秒）。
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
