//! 发布信息和版本提醒的纯逻辑：仓库地址、包名、版本比较、24 小时节流、关闭开关。联网和替换在 `cli::update`。

use std::time::Duration;

/// 发布用的 GitHub 仓库（`owner/name`）。fork 的人构建时设 `HUITAI_REPO=owner/name` 换成自己的；
/// `None` 时版本提醒和 `ai update` 安静地什么都不做。
pub const REPO: Option<&str> = match option_env!("HUITAI_REPO") {
    Some(r) => Some(r),
    None => Some("dodo258/huitai"),
};
/// GitHub API 地址（测试换成本地假服务器）。
pub const API_BASE: &str = "https://api.github.com";
/// Homebrew 里的包名：`brew upgrade huitai`。
pub const BREW_FORMULA: &str = "huitai";
/// 最多多久查一次新版本。
pub const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// 查新版本的超时。
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
/// 设成 `1` 就不查新版本（和设置里关掉「检查新版本」一样）。
pub const NO_CHECK_ENV: &str = "HUITAI_NO_UPDATE_CHECK";

/// 本机对应的下载包名：`ai-aarch64-apple-darwin`；它的校验和在同名加 `.sha256` 的文件里。
pub fn asset_name(arch: &str) -> String {
    format!("ai-{arch}-apple-darwin")
}

/// `cargo install` 装的怎么升级。
pub fn cargo_command(repo: &str) -> String {
    format!("cargo install --locked --force --git https://github.com/{repo}")
}

/// 最新发布的接口地址。
pub fn latest_url(api_base: &str, repo: &str) -> String {
    format!(
        "{}/repos/{repo}/releases/latest",
        api_base.trim_end_matches('/')
    )
}

/// 环境变量关掉了检查（`HUITAI_NO_UPDATE_CHECK=1`；空和 `0` 不算关）。
pub fn disabled_by_env(value: Option<&str>) -> bool {
    value.is_some_and(|v| !matches!(v.trim(), "" | "0"))
}

/// 离上次查已满 24 小时（从没查过、或者上次的时间在将来——改过系统时间——也算到了）。
pub fn check_due(last_checked: Option<u64>, now: u64) -> bool {
    due_after(last_checked, now, CHECK_EVERY)
}

/// 离上次查已满 `every`（规则同 [`check_due`]）。
pub fn due_after(last_checked: Option<u64>, now: u64, every: Duration) -> bool {
    match last_checked {
        None => true,
        Some(last) => now < last || now - last >= every.as_secs(),
    }
}

/// `latest` 比 `current` 新。tag 前的 `v` 去掉，按点分的数字段比；认不出数字的不算新。
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (numbers(latest), numbers(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// 给人看的版本号：去掉 tag 前的 `v`。
pub fn display_version(tag: &str) -> &str {
    let t = tag.trim();
    t.strip_prefix(['v', 'V']).unwrap_or(t)
}

/// `v0.10.2` → `[0, 10, 2]`；带预发布后缀（`-beta`）或不是数字的认不出。末尾的 0 去掉，`1.0` 和 `1.0.0` 一样。
fn numbers(version: &str) -> Option<Vec<u64>> {
    let mut out = display_version(version)
        .split('.')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    while out.last() == Some(&0) {
        out.pop();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_compares_numbers_not_text() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("1.0.1", "1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("v1.0", "1.0.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn unparsable_versions_are_never_newer() {
        assert!(!is_newer("nightly", "0.1.0"));
        assert!(!is_newer("0.2.0-beta", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
        assert!(!is_newer("0.2.0", "garbage"));
    }

    #[test]
    fn check_is_due_once_a_day() {
        let day = CHECK_EVERY.as_secs();
        assert!(check_due(None, 1000));
        assert!(!check_due(Some(1000), 1000));
        assert!(!check_due(Some(1000), 1000 + day - 1));
        assert!(check_due(Some(1000), 1000 + day));
        assert!(
            check_due(Some(5000), 1000),
            "上次时间在将来（改过钟）就当到了"
        );
    }

    #[test]
    fn env_switch_turns_off_unless_empty_or_zero() {
        assert!(disabled_by_env(Some("1")));
        assert!(disabled_by_env(Some("true")));
        assert!(!disabled_by_env(Some("0")));
        assert!(!disabled_by_env(Some("")));
        assert!(!disabled_by_env(None));
    }

    #[test]
    fn urls_and_names() {
        assert_eq!(
            latest_url("http://127.0.0.1:9/", "me/huitai"),
            "http://127.0.0.1:9/repos/me/huitai/releases/latest"
        );
        assert_eq!(asset_name("aarch64"), "ai-aarch64-apple-darwin");
        assert_eq!(display_version("v1.2.3"), "1.2.3");
        assert!(cargo_command("me/huitai").contains("https://github.com/me/huitai"));
    }
}
