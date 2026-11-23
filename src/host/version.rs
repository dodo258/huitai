//! 这个程序是哪一版：协议版本、程序版本号和程序文件，用在客户端和守护进程的握手（`Request::Hello`）里。
//!
//! 新旧的判断：先比版本号；版本号一样、又是同一个程序文件时，比文件的修改时间——`cargo install` 不改版本号
//! 也会盖掉程序文件，守护进程启动时记下的修改时间比现在文件上的旧，就是旧版。程序文件不同（开发时的
//! debug 和装好的 release）只比版本号。
//!
//! 环境变量 `HUITAI_FAKE_VERSION=<版本号>`（只给测试用，伪造另一个版本的构建）：报这个版本号。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use super::protocol::{Build, PROTOCOL};

pub const FAKE_VERSION: &str = "HUITAI_FAKE_VERSION";

/// 本进程是哪一版。第一次调用时读程序文件的修改时间，守护进程启动时就要调一次（之后文件可能被新版盖掉）。
pub fn current() -> &'static Build {
    static CURRENT: OnceLock<Build> = OnceLock::new();
    CURRENT.get_or_init(|| {
        let exe = std::env::current_exe().and_then(fs::canonicalize).ok();
        let built = exe.as_ref().map_or(0, |p| modified(p));
        detect(|k| std::env::var(k).ok(), exe, built)
    })
}

/// 程序文件现在的修改时间（Unix 秒），读不到为 0。
fn modified(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

pub fn detect(env: impl Fn(&str) -> Option<String>, exe: Option<PathBuf>, built: u64) -> Build {
    let set = |k| env(k).filter(|v: &String| !v.is_empty());
    Build {
        protocol: PROTOCOL,
        version: set(FAKE_VERSION).unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned()),
        exe,
        built,
    }
}

/// 版本号里的数字段：`0.10.2-beta` → `[0, 10, 2]`。
fn numbers(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().unwrap_or(0)
        })
        .collect()
}

impl Build {
    /// 比 `other` 新：版本号更大，或者版本号一样、同一个程序文件、文件更新。
    pub fn newer_than(&self, other: &Build) -> bool {
        match numbers(&self.version).cmp(&numbers(&other.version)) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => {
                self.exe.is_some() && self.exe == other.exe && self.built > other.built
            }
        }
    }

    /// 给人看：`0.1.0（程序文件 11-21 00:55）`。
    pub fn label(&self) -> String {
        match local_minute(self.built) {
            Some(t) => format!("{}（程序文件 {t}）", self.version),
            None => self.version.clone(),
        }
    }
}

/// Unix 秒 → 本地时间 `MM-DD HH:MM`；0 或转不了返回 `None`。
fn local_minute(secs: u64) -> Option<String> {
    if secs == 0 {
        return None;
    }
    let t = libc::time_t::try_from(secs).ok()?;
    // SAFETY: libc::tm 是纯数据结构，全零是合法值。
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: 两个指针都指向本地变量，localtime_r 只写 tm；失败时返回空指针。
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    Some(format!(
        "{:02}-{:02} {:02}:{:02}",
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(version: &str, exe: Option<&str>, built: u64) -> Build {
        Build {
            protocol: PROTOCOL,
            version: version.to_owned(),
            exe: exe.map(PathBuf::from),
            built,
        }
    }

    #[test]
    fn version_numbers_decide_first() {
        let new = build("0.10.0", Some("/a/ai"), 1);
        let old = build("0.9.3", Some("/a/ai"), 99);
        assert!(new.newer_than(&old));
        assert!(!old.newer_than(&new));
        assert!(build("1.0.0-beta", None, 0).newer_than(&build("0.9", None, 0)));
    }

    #[test]
    fn same_version_same_file_compares_file_time() {
        let installed_again = build("0.1.0", Some("/a/ai"), 200);
        let running = build("0.1.0", Some("/a/ai"), 100);
        assert!(installed_again.newer_than(&running));
        assert!(!running.newer_than(&installed_again));
        assert!(!running.newer_than(&running.clone()));
    }

    #[test]
    fn different_files_with_same_version_are_not_ordered() {
        let debug = build("0.1.0", Some("/repo/target/debug/ai"), 300);
        let release = build("0.1.0", Some("/usr/local/bin/ai"), 100);
        assert!(!debug.newer_than(&release));
        assert!(!release.newer_than(&debug));
        assert!(!build("0.1.0", None, 300).newer_than(&build("0.1.0", None, 100)));
    }

    #[test]
    fn env_fakes_version() {
        let env = |k: &str| (k == FAKE_VERSION).then(|| "0.0.1".to_owned());
        let b = detect(env, None, 5);
        assert_eq!(
            (b.protocol, b.version.as_str(), b.built),
            (PROTOCOL, "0.0.1", 5)
        );
        let real = detect(|_| Some(String::new()), None, 0);
        assert_eq!(real.protocol, PROTOCOL);
        assert_eq!(real.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn label_shows_file_time_when_known() {
        assert_eq!(build("0.1.0", None, 0).label(), "0.1.0");
        let label = build("0.1.0", None, 1_700_000_000).label();
        assert!(label.starts_with("0.1.0（程序文件 "), "{label}");
    }
}
