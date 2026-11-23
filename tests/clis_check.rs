//! 查各家 CLI 最新版：本地假 HTTP 服务测解析、超时、失败不报、20 小时缓存、换装法作废缓存、GitHub 带 ETag。

mod clis_support;

use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clis_support::Fake;

use huitai::clis::{Bases, CACHE_FILE, CHECK_EVERY, Install, Manager, Method, check};
use huitai::core::Vendor;

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("huitai-clis-check-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn install(vendor: Vendor, method: Method, version: &str) -> Install {
    Install {
        vendor,
        path: PathBuf::from("/x/bin/cli"),
        real: PathBuf::from("/x/bin/cli"),
        method,
        version: Some(version.to_owned()),
        problem: None,
    }
}

fn npm(vendor: Vendor, package: &'static str, version: &str) -> Install {
    install(
        vendor,
        Method::Package {
            manager: Manager::Npm,
            package,
        },
        version,
    )
}

const T: Duration = Duration::from_secs(10);
const NOW: u64 = 1_800_000_000;

#[test]
fn parses_every_feed_from_the_server() {
    let fake = Fake::start();
    fake.route("/@openai/codex/latest", 200, r#"{"version":"0.172.0"}"#);
    fake.route(
        "/api/formula/gemini-cli.json",
        200,
        r#"{"versions":{"stable":"0.71.0"}}"#,
    );
    fake.route(
        "/api/cask/antigravity-cli.json",
        200,
        r#"{"version":"1.5.15,5514532022517760"}"#,
    );
    fake.route("/claude-code-releases/latest", 200, "2.1.340\n");
    fake.route("/kimi-code/latest", 200, "2.7.2\n");
    fake.route(
        "/api/latest-version",
        200,
        r#"{"ok":true,"version":"1.6.1"}"#,
    );
    fake.route(
        "/repos/NousResearch/hermes-agent/releases/latest",
        200,
        r#"{"tag_name":"v0.23.3"}"#,
    );
    let dir = temp_dir("feeds");
    let installs = vec![
        install(Vendor::Claude, Method::Native, "2.1.330"),
        npm(Vendor::Codex, "@openai/codex", "0.171.0"),
        install(Vendor::Pi, Method::Native, "1.6.0"),
        install(Vendor::Hermes, Method::Native, "0.23.3"),
        install(Vendor::Kimi, Method::Native, "2.1.1"),
        install(
            Vendor::Gemini,
            Method::Formula("gemini-cli".into()),
            "0.70.0",
        ),
        install(
            Vendor::Antigravity,
            Method::Cask("antigravity-cli".into()),
            "1.5.14",
        ),
    ];
    let got: Vec<_> = check(&dir, installs, NOW, &Bases::at(&fake.origin), T)
        .into_iter()
        .map(|s| {
            (
                s.install.vendor,
                s.latest.clone(),
                s.newer().map(str::to_owned),
            )
        })
        .collect();
    let some = |s: &str| Some(s.to_owned());
    assert_eq!(
        got,
        vec![
            (Vendor::Claude, some("2.1.340"), some("2.1.340")),
            (Vendor::Codex, some("0.172.0"), some("0.172.0")),
            (Vendor::Pi, some("1.6.1"), some("1.6.1")),
            (Vendor::Hermes, some("0.23.3"), None),
            (Vendor::Kimi, some("2.7.2"), some("2.7.2")),
            (Vendor::Gemini, some("0.71.0"), some("0.71.0")),
            (Vendor::Antigravity, some("1.5.15"), some("1.5.15")),
        ]
    );
}

#[test]
fn caches_for_twenty_hours_and_per_install_method() {
    let fake = Fake::start();
    let path = "/@openai/codex/latest";
    fake.route(path, 200, r#"{"version":"0.172.0"}"#);
    let dir = temp_dir("cache");
    let bases = Bases::at(&fake.origin);
    let run = |now| {
        check(
            &dir,
            vec![npm(Vendor::Codex, "@openai/codex", "0.171.0")],
            now,
            &bases,
            T,
        )
    };
    assert_eq!(run(NOW)[0].latest.as_deref(), Some("0.172.0"));
    assert!(dir.join(CACHE_FILE).exists());

    // 不满 20 小时：不联网，用缓存里的。
    fake.route(path, 200, r#"{"version":"0.173.0"}"#);
    let later = NOW + CHECK_EVERY.as_secs() - 1;
    assert_eq!(run(later)[0].latest.as_deref(), Some("0.172.0"));
    assert_eq!(fake.hits(path).len(), 1);

    // 满了再问。
    let due = NOW + CHECK_EVERY.as_secs();
    assert_eq!(run(due)[0].latest.as_deref(), Some("0.173.0"));
    assert_eq!(fake.hits(path).len(), 2);

    // 换了装法（npm → cask）：缓存作废，马上跟新渠道比。
    fake.route("/api/cask/codex.json", 200, r#"{"version":"0.170.0"}"#);
    let cask = install(Vendor::Codex, Method::Cask("codex".into()), "0.170.0");
    let s = check(&dir, vec![cask], due + 1, &bases, T);
    assert_eq!(s[0].latest.as_deref(), Some("0.170.0"));
    assert_eq!(s[0].newer(), None);
}

#[test]
fn failures_are_quiet_and_keep_the_last_answer() {
    let fake = Fake::start();
    let path = "/@google/gemini-cli/latest";
    let dir = temp_dir("fail");
    let bases = Bases::at(&fake.origin);
    let gemini = || vec![npm(Vendor::Gemini, "@google/gemini-cli", "0.70.0")];

    // 从没查成功过：没有最新版，不报错。
    fake.route(path, 500, "oops");
    let s = check(&dir, gemini(), NOW, &bases, T);
    assert_eq!((s.len(), s[0].latest.clone()), (1, None));

    fake.route(path, 200, r#"{"version":"0.71.0"}"#);
    let next = NOW + CHECK_EVERY.as_secs();
    assert_eq!(
        check(&dir, gemini(), next, &bases, T)[0].latest.as_deref(),
        Some("0.71.0")
    );

    // 之后服务器坏了、回了看不懂的东西：沿用上次的。
    fake.route(path, 200, "<html>维护中</html>");
    let again = next + CHECK_EVERY.as_secs();
    assert_eq!(
        check(&dir, gemini(), again, &bases, T)[0].latest.as_deref(),
        Some("0.71.0")
    );
    fake.route(path, 404, "{}");
    let more = again + CHECK_EVERY.as_secs();
    assert_eq!(
        check(&dir, gemini(), more, &bases, T)[0].latest.as_deref(),
        Some("0.71.0")
    );

    // 连不上（端口没人听）。
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let nobody = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let s = check(&temp_dir("nobody"), gemini(), NOW, &Bases::at(&nobody), T);
    assert_eq!(s[0].latest, None);

    // 缓存文件坏了：当作没查过，照常查。
    fs::write(dir.join(CACHE_FILE), "坏了").unwrap();
    fake.route(path, 200, r#"{"version":"0.72.0"}"#);
    assert_eq!(
        check(&dir, gemini(), more, &bases, T)[0].latest.as_deref(),
        Some("0.72.0")
    );
}

#[test]
fn slow_server_times_out() {
    let fake = Fake::start();
    fake.route("/@openai/codex/latest", 0, "");
    let started = Instant::now();
    let s = check(
        &temp_dir("timeout"),
        vec![npm(Vendor::Codex, "@openai/codex", "0.171.0")],
        NOW,
        &Bases::at(&fake.origin),
        Duration::from_secs(1),
    );
    assert_eq!(s[0].latest, None);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn github_feeds_send_the_etag_back() {
    let fake = Fake::start();
    let path = "/repos/openai/codex/releases/latest";
    fake.route_etag(path, 200, r#"{"tag_name":"rust-v0.172.0"}"#, "W/\"abc\"");
    let dir = temp_dir("etag");
    let bases = Bases::at(&fake.origin);
    let codex = || vec![install(Vendor::Codex, Method::Native, "0.171.0")];
    assert_eq!(
        check(&dir, codex(), NOW, &bases, T)[0].latest.as_deref(),
        Some("0.172.0")
    );
    let next = NOW + CHECK_EVERY.as_secs();
    // 没变：回 304，沿用缓存里的版本。
    let s = check(&dir, codex(), next, &bases, T);
    assert_eq!(s[0].latest.as_deref(), Some("0.172.0"));
    assert_eq!(fake.hits(path), vec![String::new(), "W/\"abc\"".to_owned()]);

    // 别的数据源不带 ETag。
    let npm_path = "/@openai/codex/latest";
    fake.route_etag(npm_path, 200, r#"{"version":"0.172.0"}"#, "\"n1\"");
    let dir = temp_dir("etag-npm");
    let n = || vec![npm(Vendor::Codex, "@openai/codex", "0.171.0")];
    check(&dir, n(), NOW, &bases, T);
    check(&dir, n(), next, &bases, T);
    assert_eq!(fake.hits(npm_path), vec![String::new(), String::new()]);
}

#[test]
fn unknown_methods_and_unparsable_versions_are_not_new() {
    let fake = Fake::start();
    let dir = temp_dir("unknown");
    let s = check(
        &dir,
        vec![install(Vendor::Claude, Method::Unknown, "2.1.0")],
        NOW,
        &Bases::at(&fake.origin),
        T,
    );
    assert_eq!((s[0].latest.clone(), s[0].newer()), (None, None));
    assert!(fake.seen.lock().unwrap().is_empty(), "认不出装法不联网");

    fake.route("/@openai/codex/latest", 200, r#"{"version":"0.172.0"}"#);
    let mut codex = npm(Vendor::Codex, "@openai/codex", "x");
    codex.version = None;
    let s = check(&dir, vec![codex], NOW, &Bases::at(&fake.origin), T);
    assert_eq!(s[0].latest.as_deref(), Some("0.172.0"));
    assert_eq!(s[0].newer(), None, "本机版本问不出就不标新版");
}

/// 本机从什么时候起落后记进缓存（自带更新的几家要等满 24 小时）：同一版再查不变，又出了更新的版本、本机还没跟上也不变；
/// 旧缓存里没记的从读到那一刻算起。
#[test]
fn remembers_when_a_version_was_first_seen() {
    let fake = Fake::start();
    fake.route("/@openai/codex/latest", 200, r#"{"version":"0.172.0"}"#);
    let dir = temp_dir("seen");
    let bases = Bases::at(&fake.origin);
    let codex = || vec![npm(Vendor::Codex, "@openai/codex", "0.171.0")];
    let seen = |now| check(&dir, codex(), now, &bases, T)[0].seen_at;
    assert_eq!(seen(NOW), Some(NOW));
    let later = NOW + CHECK_EVERY.as_secs() + 1;
    assert_eq!(seen(later), Some(NOW), "同一版再查到不重算");
    fake.route("/@openai/codex/latest", 200, r#"{"version":"0.173.0"}"#);
    let again = later + CHECK_EVERY.as_secs() + 1;
    assert_eq!(
        seen(again),
        Some(NOW),
        "又出了新版、本机还是 0.171.0：落后从第一次查到新版算，不重算（审查退回第 1 轮）"
    );

    // 旧版会话台写的缓存没有 seen_at：读到时从现在算，不当成早就出来了。
    let key = huitai::clis::feed_for(&codex()[0], std::env::consts::ARCH)
        .unwrap()
        .key();
    fs::write(
        dir.join(CACHE_FILE),
        format!(r#"{{"codex":{{"feed":"{key}","checked_at":{NOW},"latest":"0.172.0"}}}}"#),
    )
    .unwrap();
    assert_eq!(seen(NOW + 5), Some(NOW + 5));
    assert_eq!(seen(NOW + 50), Some(NOW + 5), "记下后不再变");
    let _ = fs::remove_dir_all(&dir);
}
