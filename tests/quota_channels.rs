//! 额度各渠道的样例测试：每个渠道正常读数，格式变了或过期时自动降级到下一个渠道。
//! 样例在 tests/quota_fixtures/（手写脱敏），复制到临时目录后按测试需要设修改时间。

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use huitai::core::{QuotaReading, QuotaValue, Vendor};
use huitai::quota::{QuotaReader, UsageRequest};

/// 2026-11-20T12:00:00Z；测试一律按 UTC 算「今天」。
const NOW: u64 = 1_795_176_000;

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/quota_fixtures")
}

/// 每个测试一个干净的临时目录，内含 home/ 和 data/。
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("huitai-quota-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    /// 把样例目录复制成 home，所有文件修改时间设为 NOW 前一分钟（「今天改过」）。
    fn with_home(self, fixture: &str) -> Self {
        copy_dir(&fixtures().join(fixture), &self.home());
        self
    }

    /// 把样例文件放进数据目录，修改时间设为 `mtime`。
    fn put_data(&self, fixture: &str, name: &str, mtime: SystemTime) {
        let dst = self.data().join(name);
        fs::copy(fixtures().join(fixture), &dst).unwrap();
        set_mtime(&dst, mtime);
    }

    fn reader(&self) -> QuotaReader {
        QuotaReader::new(self.home(), self.data(), at(NOW), 0)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), &to).unwrap();
            set_mtime(&to, at(NOW - 60));
        }
    }
}

fn set_mtime(path: &Path, t: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn fake_fetch(fixture: &'static str) -> Box<huitai::quota::UsageFetch> {
    Box::new(move |req: &UsageRequest| {
        assert!(req.url.ends_with("/api/oauth/usage"));
        assert_eq!(req.now_secs, NOW as i64);
        Ok(fs::read_to_string(fixtures().join(fixture)).unwrap())
    })
}

fn failing_fetch() -> Box<huitai::quota::UsageFetch> {
    Box::new(|_: &UsageRequest| Err("断网".to_owned()))
}

/// 读数里的窗口：(标签, 百分比, 重置时间秒)。
fn windows(r: &QuotaReading) -> Vec<(String, f32, Option<u64>)> {
    let QuotaValue::Windows(ws) = &r.value else {
        panic!("不是窗口读数: {r:?}")
    };
    ws.iter()
        .map(|w| {
            let reset = w
                .resets_at
                .map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_secs());
            (w.label.clone(), w.used_percent, reset)
        })
        .collect()
}

// ---------- Claude ----------

#[test]
fn claude_statusline_cache_is_first_choice() {
    let sb = Sandbox::new("cl-statusline").with_home("claude/home");
    sb.put_data(
        "claude/statusline.json",
        "claude-statusline.json",
        at(NOW - 120),
    );
    let r = sb.reader().with_fetch(failing_fetch()).read(Vendor::Claude);
    assert_eq!(r.source, "状态栏");
    assert_eq!(r.at, at(NOW - 120), "读数时间取缓存写入时间");
    assert_eq!(
        windows(&r),
        vec![
            ("5 小时".to_owned(), 23.5, Some(1_795_190_000)),
            ("7 天".to_owned(), 41.2, Some(1_795_500_000)),
        ]
    );
}

#[test]
fn claude_stale_statusline_falls_back_to_usage_api_and_caches_it() {
    let sb = Sandbox::new("cl-stale").with_home("claude/home");
    sb.put_data(
        "claude/statusline.json",
        "claude-statusline.json",
        at(NOW - 3600),
    );
    let r = sb
        .reader()
        .with_fetch(fake_fetch("claude/usage.json"))
        .read(Vendor::Claude);
    assert_eq!(r.source, "用量接口");
    assert_eq!(r.at, at(NOW));
    assert_eq!(
        windows(&r),
        vec![
            ("5 小时".to_owned(), 3.0, Some(1_795_186_800)),
            ("7 天".to_owned(), 20.0, Some(1_795_564_800)),
        ]
    );
    // 缓存期内不再联网：接口坏了也拿到同一个读数。
    let again = sb.reader().with_fetch(failing_fetch()).read(Vendor::Claude);
    assert_eq!(again, r);
}

#[test]
fn claude_renamed_statusline_fields_fall_back_to_usage_api() {
    let sb = Sandbox::new("cl-sl-renamed").with_home("claude/home");
    sb.put_data(
        "claude/statusline-renamed.json",
        "claude-statusline.json",
        at(NOW - 60),
    );
    let r = sb
        .reader()
        .with_fetch(fake_fetch("claude/usage.json"))
        .read(Vendor::Claude);
    assert_eq!(r.source, "用量接口");
}

#[test]
fn claude_falls_back_to_local_tokens_when_api_fails_or_changes() {
    for fetch in [failing_fetch(), fake_fetch("claude/usage-renamed.json")] {
        let sb = Sandbox::new("cl-tokens").with_home("claude/home");
        // 昨天改过的文件不看，即使里面有今天时间戳的行。
        set_mtime(
            &sb.home().join(".claude/projects/-Users-me-other/old.jsonl"),
            at(NOW - 86400),
        );
        let r = sb.reader().with_fetch(fetch).read(Vendor::Claude);
        assert_eq!(r.source, "本机统计");
        // 100（msg_a 去重）+ 3 + 1000（子代理）+ 10；昨天的行、坏行、用户行不算。
        assert_eq!(r.value, QuotaValue::TokensToday(1113));
    }
}

#[test]
fn claude_all_channels_failing_is_unavailable_not_an_error() {
    let sb = Sandbox::new("cl-none").with_home("claude/home-renamed");
    let r = sb.reader().with_fetch(failing_fetch()).read(Vendor::Claude);
    assert_eq!(r, QuotaReading::unavailable(at(NOW)));

    let empty = Sandbox::new("cl-empty");
    assert_eq!(
        empty.reader().read(Vendor::Claude).value,
        QuotaValue::Unavailable
    );
}

#[test]
fn claude_probe_reports_every_channel() {
    let sb = Sandbox::new("cl-probe").with_home("claude/home");
    let reports = sb
        .reader()
        .with_fetch(failing_fetch())
        .probe(Vendor::Claude);
    let sources: Vec<_> = reports.iter().map(|r| r.source).collect();
    assert_eq!(sources, ["状态栏", "用量接口", "本机统计"]);
    assert!(
        reports[0]
            .result
            .as_ref()
            .unwrap_err()
            .contains("还没有缓存")
    );
    assert!(reports[1].result.as_ref().unwrap_err().contains("断网"));
    assert!(reports[2].result.is_ok());
}

// ---------- Claude 状态栏注入 ----------

fn settings_status(json: &str) -> serde_json::Map<String, serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    v["statusLine"].as_object().unwrap().clone()
}

/// 用 sh 跑注入的状态栏命令，喂一份状态栏数据，返回输出。
fn run_statusline(command: &str, input: &str) -> String {
    let mut child = Command::new("sh")
        .args(["-c", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn claude_settings_without_original_statusline_only_caches() {
    let sb = Sandbox::new("cl-settings-none");
    let project = sb.root.join("proj");
    let status = settings_status(&sb.reader().claude_settings(&project));
    assert_eq!(status["type"], "command");
    let input = fs::read_to_string(fixtures().join("claude/statusline.json")).unwrap();
    let out = run_statusline(status["command"].as_str().unwrap(), &input);
    assert_eq!(out, "");
    let cached = sb.data().join("claude-statusline.json");
    assert_eq!(fs::read_to_string(&cached).unwrap(), input);
}

#[test]
fn claude_settings_keeps_original_statusline_and_its_options() {
    // 数据目录带空格和单引号，验证引号处理。
    let sb = Sandbox::new("cl-settings-orig");
    let data = sb.root.join("Application Support/it's huitai");
    let home = sb.home();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(
        home.join(".claude/settings.json"),
        r#"{"model":"x","statusLine":{"type":"command","command":"printf 'user:'; wc -c | tr -d ' ' # 注释结尾","padding":2}}"#,
    )
    .unwrap();
    let reader = QuotaReader::new(home.clone(), data.clone(), at(NOW), 0);
    let project = sb.root.join("proj");
    let status = settings_status(&reader.claude_settings(&project));
    assert_eq!(status["padding"], 2, "原有选项保留");
    let input = fs::read_to_string(fixtures().join("claude/statusline.json")).unwrap();
    let out = run_statusline(status["command"].as_str().unwrap(), &input);
    assert_eq!(
        out.trim(),
        format!("user:{}", input.len()),
        "原命令拿到同一份数据，输出原样"
    );
    assert_eq!(
        fs::read_to_string(data.join("claude-statusline.json")).unwrap(),
        input
    );

    // 项目本地设置优先于用户设置；原命令自己 exit 也不影响写缓存。
    fs::create_dir_all(project.join(".claude")).unwrap();
    fs::write(
        project.join(".claude/settings.local.json"),
        r#"{"statusLine":{"type":"command","command":"echo local; exit 3"}}"#,
    )
    .unwrap();
    fs::remove_file(data.join("claude-statusline.json")).unwrap();
    let status = settings_status(&reader.claude_settings(&project));
    assert!(status.get("padding").is_none());
    let out = run_statusline(status["command"].as_str().unwrap(), &input);
    assert_eq!(out, "local\n");
    assert!(data.join("claude-statusline.json").exists());
}

// ---------- Codex ----------

#[test]
fn codex_reads_latest_rollout_rate_limits() {
    let sb = Sandbox::new("cx-rollout").with_home("codex/home");
    let sessions = sb.home().join(".codex/sessions");
    // 旧文件改得更早；最新文件末尾那条 rate_limits 为 null，要往前找到带额度的那条。
    set_mtime(
        &sessions.join("2026/11/19/rollout-2026-11-19T08-00-00-old.jsonl"),
        at(NOW - 3 * 3600),
    );
    let r = sb.reader().read(Vendor::Codex);
    assert_eq!(r.source, "会话记录");
    assert_eq!(r.at, at(1_795_170_600), "读数时间取事件时间戳 10:30");
    assert_eq!(
        windows(&r),
        vec![
            ("7 天".to_owned(), 9.0, Some(1_795_321_074)),
            ("5 小时".to_owned(), 30.5, Some(1_795_190_000)),
        ]
    );
}

#[test]
fn codex_counts_cumulative_tokens_since_midnight() {
    let sb = Sandbox::new("cx-probe").with_home("codex/home");
    let reports = sb.reader().probe(Vendor::Codex);
    assert_eq!(reports[1].source, "本机统计");
    // 旧文件 800 − 昨天的 500 = 300；新文件 0 → 1000（重复事件不重复算）。
    assert_eq!(
        reports[1].result.as_ref().unwrap().value,
        QuotaValue::TokensToday(1300)
    );
}

#[test]
fn codex_renamed_or_expired_rate_limits_fall_back_to_tokens() {
    for (fixture, tokens) in [("codex/home-renamed", 42), ("codex/home-expired", 77)] {
        let sb = Sandbox::new("cx-fallback").with_home(fixture);
        let r = sb.reader().read(Vendor::Codex);
        assert_eq!(r.source, "本机统计", "{fixture}");
        assert_eq!(r.value, QuotaValue::TokensToday(tokens), "{fixture}");
    }
    let empty = Sandbox::new("cx-empty");
    assert_eq!(
        empty.reader().read(Vendor::Codex).value,
        QuotaValue::Unavailable
    );
}

// ---------- pi ----------

#[test]
fn pi_counts_todays_assistant_tokens() {
    let sb = Sandbox::new("pi-tokens").with_home("pi/home");
    let r = sb.reader().read(Vendor::Pi);
    assert_eq!(r.source, "本机统计");
    assert_eq!(r.at, at(NOW));
    // 2899 + 101；昨天的、工具结果、坏行、空文件不算。
    assert_eq!(r.value, QuotaValue::TokensToday(3000));
}

#[test]
fn pi_tokens_respect_local_timezone() {
    let sb = Sandbox::new("pi-tz").with_home("pi/home");
    // 北京时间：今天从 2026-11-19T16:00Z 起，昨天 22:00Z 那条 5000 也算今天。
    let r = QuotaReader::new(sb.home(), sb.data(), at(NOW), 8 * 3600).read(Vendor::Pi);
    assert_eq!(r.value, QuotaValue::TokensToday(8000));
}

#[test]
fn pi_renamed_fields_or_missing_dir_are_unavailable() {
    let sb = Sandbox::new("pi-renamed").with_home("pi/home-renamed");
    assert_eq!(
        sb.reader().read(Vendor::Pi),
        QuotaReading::unavailable(at(NOW))
    );
    let empty = Sandbox::new("pi-empty");
    assert_eq!(
        empty.reader().read(Vendor::Pi).value,
        QuotaValue::Unavailable
    );
}

#[test]
fn moved_claude_config_dir_asks_only_its_own_keychain_entry() {
    // 改过 CLAUDE_CONFIG_DIR 的账号：用量接口只读它自己那份钥匙串条目
    // （`Claude Code-credentials-<sha256(目录) 前 8 位>`），绝不拿默认条目（别的账号的）。
    let sb = Sandbox::new("cl-moved").with_home("claude/home");
    sb.put_data(
        "claude/statusline.json",
        "claude-statusline.json",
        at(NOW - 3600),
    );
    let moved = huitai::core::VendorDirs::new(&sb.home(), &sb.home(), |k| {
        (k == "CLAUDE_CONFIG_DIR").then(|| "/elsewhere/.claude-work".into())
    });
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = asked.clone();
    let r = sb
        .reader()
        .with_dirs(moved)
        .with_fetch(Box::new(move |req| {
            seen.lock().unwrap().push(req.keychain_service.to_owned());
            Err("测试里不联网".to_owned())
        }))
        .read(Vendor::Claude);
    // sha256("/elsewhere/.claude-work") = b7db8dd3…
    assert_eq!(
        *asked.lock().unwrap(),
        ["Claude Code-credentials-b7db8dd3".to_owned()]
    );
    assert_ne!(r.source, "用量接口");
}

/// Claude 用量接口的请求：系统自带的 curl、第一个参数 `-q`，`.curlrc` 里开的追踪不会把凭证写进文件；凭证只走标准输入。
/// 对照：同一条命令去掉 `-q` 时追踪文件里就有凭证（证明这个 `.curlrc` 确实会被读）。
#[test]
fn usage_request_ignores_curlrc_trace() {
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;

    const TOKEN: &str = "sk-ant-oat-fake-0042";
    let sb = Sandbox::new("curlrc");
    let rc = sb.root.join("rc");
    fs::create_dir_all(&rc).unwrap();
    let trace = rc.join("trace.txt");
    fs::write(
        rc.join(".curlrc"),
        format!("trace-ascii = \"{}\"\n", trace.display()),
    )
    .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/api/oauth/usage", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                line.clear();
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
            let _ = reader.read(&mut [0; 1]);
        }
    });
    let headers = ["anthropic-beta: oauth-2025-04-20".to_owned()];
    let req = UsageRequest {
        url: &url,
        headers: &headers,
        keychain_service: "unused",
        token_pointer: "/unused",
        expires_at_ms_pointer: "/unused",
        timeout: Duration::from_secs(5),
        now_secs: NOW as i64,
    };
    let (argv, stdin) = huitai::quota::usage_command(&req, TOKEN);
    assert_eq!(argv[0], "/usr/bin/curl", "{argv:?}");
    assert_eq!(argv[1], "-q", "-q 只有放在第一个参数才生效：{argv:?}");
    assert!(
        argv.iter().all(|a| !a.contains(TOKEN)),
        "凭证不进命令行参数"
    );
    assert!(stdin.contains(TOKEN));

    let run = |args: &[String]| {
        let mut child = Command::new(&argv[0])
            .args(args)
            .env("CURL_HOME", &rc)
            .env("HOME", &rc)
            .env("XDG_CONFIG_HOME", &rc)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "{}");
    };
    run(&argv[1..]);
    assert!(!trace.exists(), "-q 时不读 .curlrc，不该有追踪文件");
    run(&argv[2..]);
    let traced = fs::read_to_string(&trace).expect("不带 -q 时 .curlrc 生效");
    assert!(traced.contains(TOKEN), "对照：不带 -q 时凭证进了追踪文件");
}
