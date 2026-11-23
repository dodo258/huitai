//! `ai update <家>`、`ai update --all`：按本机装法跑那家的官方升级命令（会话台自己的升级是不带参数的 `ai update`）。
//! 和 `ai doctor` 一样用登录 shell 的 PATH 找；点名的那家只要有新版、会话台能替它跑就升（自带自动更新的也升，
//! 主人点了名）；`--all` 只升界面上「更新」按钮会升的那几家。

use std::collections::HashSet;
use std::path::Path;

use crate::clis::{self, Runner, Status};
use crate::core::Vendor;
use crate::host;

/// 跑完的一家：给人看的一行（或几行，失败带原文），成没成。
pub type Line = (String, bool);

/// 升级；`vendor` 是 `None` 时升全部能升的。每家一跑完就交给 `say`。没有要升的也说一句。
pub fn run(vendor: Option<Vendor>, data_dir: &Path, mut say: impl FnMut(Line)) {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_owned());
    let login = match clis::login_env(Path::new(&shell)) {
        Ok(l) => l,
        Err(e) => return say((format!("登录 shell 没跑完，找不到各家 CLI：{e}"), false)),
    };
    let now = clis::now_secs();
    let statuses = clis::check(
        data_dir,
        clis::probe(&login, &home),
        now,
        &clis::Bases::official(),
        clis::TIMEOUT,
    );
    let targets = match pick(&statuses, vendor, now) {
        Ok(t) => t,
        Err(msg) => return say(msg),
    };
    // 守护进程答不上来就当每家都有对话在跑：npm 一类原地覆盖的这次不升。
    let hosted = host::client::hosted_vendors(data_dir)
        .unwrap_or_else(|_| Vendor::ALL.into_iter().collect::<HashSet<_>>());
    let runner = Runner {
        login: &login,
        home: &home,
        data_dir,
        timeout: clis::UPGRADE_TIMEOUT,
    };
    for s in targets {
        let latest = s.newer().unwrap_or_default();
        eprintln!("升级 {} {} → {latest} …", s.install.vendor, version(s));
        let r = clis::upgrade(
            &s.install,
            latest,
            hosted.contains(&s.install.vendor),
            false,
            &runner,
        );
        say(line(s.install.vendor, r));
    }
}

/// 要升哪几家。点名的：没装、已是最新、只能自己跑的给一句话（只能自己跑的带命令）；`--all`：能一键升的全部。
fn pick(statuses: &[Status], vendor: Option<Vendor>, now: u64) -> Result<Vec<&Status>, Line> {
    let Some(v) = vendor else {
        let all: Vec<&Status> = statuses
            .iter()
            .filter(|s| s.upgrade_now(now).is_some())
            .collect();
        if all.is_empty() {
            return Err((
                "没有要升级的（各家都是最新，或要自己跑命令：看 ai doctor）".to_owned(),
                true,
            ));
        }
        return Ok(all);
    };
    let Some(s) = statuses.iter().find(|s| s.install.vendor == v) else {
        return Err((format!("{v}：PATH 上没找到 {}", clis::program(v)), false));
    };
    let Some(latest) = s.newer() else {
        let why = match (&s.install.version, &s.latest) {
            (Some(cur), Some(_)) => format!("{v} 已是最新（{cur}）"),
            (None, _) => format!("{v}：问不出当前版本，没法比"),
            (Some(cur), None) => format!("{v}：当前 {cur}，查不到最新版（没联网或关了检查新版本）"),
        };
        return Err((why, s.latest.is_some() && s.install.version.is_some()));
    };
    let p = clis::plan(&s.install, Some(latest));
    match (p.how, p.command) {
        (clis::How::Manual(why), cmd) => Err((
            match cmd {
                Some(c) => format!("{v}：{why}。自己跑：{c}"),
                None => format!("{v}：{why}"),
            },
            false,
        )),
        _ => Ok(vec![s]),
    }
}

fn version(s: &Status) -> &str {
    s.install.version.as_deref().unwrap_or("?")
}

/// 一家的结果写成给人看的。
fn line(v: Vendor, r: Result<clis::Outcome, clis::Blocked>) -> Line {
    let (ok, text) = clis::summary(&r);
    (format!("{v}：{text}"), ok)
}
