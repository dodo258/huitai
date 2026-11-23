//! 查最新版并缓存：每家每 20 小时最多问一次，每个请求 10 秒超时，失败不报、沿用上次查到的。
//! 缓存在会话台数据目录的 `cli-versions.json`，按厂商记数据源、上次查的时间、查到的版本和 ETag。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::core::{http, release};

use super::detect::Install;
use super::latest::{Bases, Feed, feed_for};

pub const CACHE_FILE: &str = "cli-versions.json";
/// 每家多久查一次（和 Codex 自己查新版的间隔一样）。
pub const CHECK_EVERY: Duration = Duration::from_secs(20 * 60 * 60);
/// 每个请求的超时。
pub const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Entry {
    /// 跟哪个数据源比的（[`Feed::key`]）；装法换了就作废。
    feed: String,
    checked_at: u64,
    latest: Option<String>,
    etag: Option<String>,
    /// 本机从什么时候起落后于最新版（Unix 秒，见 [`behind_since`]）：自带更新的几家落后满 24 小时，
    /// 就当它的自带更新没生效。0 是旧缓存里没记，读到时从现在算起。
    seen_at: u64,
}

/// 一家查完的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub install: Install,
    /// 这种装法的最新版；没查到（没数据源、从没查成功过）是 `None`。
    pub latest: Option<String>,
    /// 本机从什么时候起落后于最新版（Unix 秒）：从第一次查到比本机新的版本算，之后又出了更新的版本也不重算，
    /// 本机追上过才从下一个新版重新算。
    pub seen_at: Option<u64>,
}

impl Status {
    /// 比本机新的最新版。版本号认不出（预发布后缀之类）不算新，免得误报。
    pub fn newer(&self) -> Option<&str> {
        let latest = self.latest.as_deref()?;
        let current = self.install.version.as_deref()?;
        release::is_newer(latest, current).then_some(latest)
    }
}

/// 给每家查最新版：缓存里同一个数据源、不满 20 小时的直接用；到期的并行去问，问完写回缓存。
pub fn check(
    data_dir: &Path,
    installs: Vec<Install>,
    now: u64,
    bases: &Bases,
    timeout: Duration,
) -> Vec<Status> {
    let path = data_dir.join(CACHE_FILE);
    let mut cache: BTreeMap<String, Entry> = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let arch = std::env::consts::ARCH;
    let mut changed = false;
    let out = thread::scope(|s| {
        let jobs: Vec<_> = installs
            .into_iter()
            .map(|install| {
                let feed = feed_for(&install, arch);
                let old = cache
                    .get(install.vendor.key())
                    .filter(|e| feed.as_ref().is_some_and(|f| f.key() == e.feed))
                    .cloned();
                let job = feed
                    .clone()
                    .filter(|_| {
                        release::due_after(old.as_ref().map(|e| e.checked_at), now, CHECK_EVERY)
                    })
                    .map(|feed| {
                        let etag = old.as_ref().and_then(|e| e.etag.clone());
                        s.spawn(move || ask(&feed, bases, etag.as_deref(), timeout))
                    });
                (install, feed, old, job)
            })
            .collect();
        jobs.into_iter()
            .map(|(install, feed, old, job)| {
                let Some(job) = job else {
                    let Some(mut e) = old else {
                        return Status {
                            install,
                            latest: None,
                            seen_at: None,
                        };
                    };
                    if e.seen_at == 0 && e.latest.is_some() {
                        e.seen_at = now;
                        cache.insert(install.vendor.key().to_owned(), e.clone());
                        changed = true;
                    }
                    return Status {
                        install,
                        seen_at: e.latest.is_some().then_some(e.seen_at),
                        latest: e.latest,
                    };
                };
                let fresh = job.join().ok().flatten();
                let mut entry = old.unwrap_or_default();
                entry.feed = feed.map(|f| f.key()).unwrap_or_default();
                entry.checked_at = now;
                // 304（没变）或失败：沿用上次的。
                if let Some((latest, etag)) = fresh {
                    if latest.is_some() {
                        if latest != entry.latest {
                            entry.seen_at = behind_since(
                                entry.latest.as_deref(),
                                entry.seen_at,
                                install.version.as_deref(),
                                now,
                            );
                        }
                        entry.latest = latest;
                    }
                    if etag.is_some() {
                        entry.etag = etag;
                    }
                }
                if entry.seen_at == 0 && entry.latest.is_some() {
                    entry.seen_at = now;
                }
                cache.insert(install.vendor.key().to_owned(), entry.clone());
                changed = true;
                Status {
                    install,
                    seen_at: entry.latest.is_some().then_some(entry.seen_at),
                    latest: entry.latest,
                }
            })
            .collect()
    });
    if changed {
        // 写不进去只是下次再问一次，不打扰。先写临时文件再改名，两个会话台同时写也不会写出半个文件。
        let tmp = data_dir.join(format!(".{CACHE_FILE}.{}", std::process::id()));
        let _ = fs::create_dir_all(data_dir)
            .and_then(|()| fs::write(&tmp, serde_json::to_vec_pretty(&cache).unwrap_or_default()))
            .and_then(|()| fs::rename(&tmp, &path));
        let _ = fs::remove_file(&tmp);
    }
    out
}

/// 查到的最新版换了一个时，本机「落后」从什么时候算：上次查到的那一版就已经比本机新（本机一直没跟上），
/// 接着从那时算——不然新版每 20 小时出一个，计时每次清零，自带更新没生效的那家永远等不满 24 小时；
/// 第一次查到、或本机已经追上了上一版（这是新的一次落后），从现在算。
fn behind_since(old_latest: Option<&str>, old_since: u64, current: Option<&str>, now: u64) -> u64 {
    match (old_latest, current) {
        (Some(old), Some(cur)) if old_since > 0 && release::is_newer(old, cur) => old_since,
        _ => now,
    }
}

/// 问一次：成功给出（查到的版本，新的 ETag），304 时版本是 `None`；连不上、超时、格式不认识都是 `None`。
fn ask(
    feed: &Feed,
    bases: &Bases,
    etag: Option<&str>,
    timeout: Duration,
) -> Option<(Option<String>, Option<String>)> {
    let if_none = etag
        .filter(|_| feed.uses_etag())
        .map(|e| format!("If-None-Match: {e}"));
    let mut headers: Vec<&str> = feed.headers().to_vec();
    if let Some(h) = &if_none {
        headers.push(h);
    }
    let r = http::get(&feed.url(bases), &headers, None, timeout).ok()?;
    if r.status == 304 {
        return Some((None, None));
    }
    let latest = feed.parse(&r.body)?;
    Some((Some(latest), r.etag.filter(|_| feed.uses_etag())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const H: u64 = 60 * 60;

    /// 审查退回第 1 轮：新版每 20 小时出一个、本机一直是旧版时，落后的起点不跟着新版重算，满 24 小时就算到期。
    #[test]
    fn falling_behind_keeps_counting_across_new_releases() {
        // 第一次查到新版：从现在算。
        assert_eq!(behind_since(None, 0, Some("2.1.300"), NOW), NOW);
        // 20 小时后又出一版，本机还是 2.1.300：接着从第一次算。
        let t1 = behind_since(Some("2.1.301"), NOW, Some("2.1.300"), NOW + 20 * H);
        assert_eq!(t1, NOW);
        let t2 = behind_since(Some("2.1.302"), t1, Some("2.1.300"), NOW + 40 * H);
        assert_eq!(t2, NOW, "40 小时后已落后满 24 小时，不该又从头算");
        // 本机追上了上一版（2.1.302），再出新版：这是新的一次落后，从现在算。
        assert_eq!(
            behind_since(Some("2.1.302"), NOW, Some("2.1.302"), NOW + 60 * H),
            NOW + 60 * H
        );
        // 本机版本问不出、旧缓存没记起点：从现在算。
        assert_eq!(behind_since(Some("2.1.301"), NOW, None, NOW + H), NOW + H);
        assert_eq!(
            behind_since(Some("2.1.301"), 0, Some("2.1.300"), NOW + H),
            NOW + H
        );
    }
}
