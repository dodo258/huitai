//! 余额渠道的集成测试：本机假 HTTP 服务（127.0.0.1）扮演各家接口，真用 curl 请求，不对外发任何请求。
//! 密钥放在临时家目录的假 pi / Hermes 配置和假环境变量里（都是编的）。

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use huitai::core::{Account, Vendor};
use huitai::quota::{
    BalanceReader, BalanceReport, BalanceRow, BalanceValue, Found, Provider, defaults, find,
};

/// 2026-11-20T12:00:00Z。
const NOW: u64 = 1_795_176_000;

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// 编的密钥（每家一把，彼此不同，方便查有没有发错地方、有没有落盘）。
const DEEPSEEK: &str = "sk-fake-deepseek-0001";
const OPENROUTER: &str = "sk-or-v1-fake-0002";
const KIMI: &str = "sk-kimi-fake-0003";
const MOONSHOT: &str = "sk-fake-moonshot-0005";
const ALL_KEYS: [&str; 4] = [DEEPSEEK, OPENROUTER, KIMI, MOONSHOT];

/// 假服务：路径 → (状态码, 正文)；记下每次请求的路径和 Authorization 头。
struct Fake {
    origin: String,
    routes: Arc<Mutex<HashMap<String, (u16, String)>>>,
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

impl Fake {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<Mutex<HashMap<String, (u16, String)>>> = Arc::default();
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let (r, s) = (routes.clone(), seen.clone());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (r, s) = (r.clone(), s.clone());
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut first = String::new();
                    reader.read_line(&mut first).unwrap();
                    let path = first.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let mut auth = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(v) = line.strip_prefix("Authorization: ") {
                            auth = v.trim().to_owned();
                        }
                    }
                    s.lock().unwrap().push((path.clone(), auth));
                    let (code, body) = r
                        .lock()
                        .unwrap()
                        .get(&path)
                        .cloned()
                        .unwrap_or((404, "{}".to_owned()));
                    if code == 0 {
                        // 不回应，等客户端超时。
                        thread::sleep(Duration::from_secs(9));
                        return;
                    }
                    let mut stream = stream;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.flush();
                    let _ = reader.read(&mut [0; 1]);
                });
            }
        });
        Self {
            origin,
            routes,
            seen,
        }
    }

    fn route(&self, path: &str, code: u16, body: &str) {
        self.routes
            .lock()
            .unwrap()
            .insert(path.to_owned(), (code, body.to_owned()));
    }

    fn hits(&self, path: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .count()
    }
}

const P_DEEPSEEK: &str = "/api.deepseek.com/user/balance";
const P_MOONSHOT_CN: &str = "/api.moonshot.cn/v1/users/me/balance";
const P_MOONSHOT_AI: &str = "/api.moonshot.ai/v1/users/me/balance";
const P_OPENROUTER: &str = "/openrouter.ai/api/v1/key";
const P_KIMI: &str = "/api.kimi.com/coding/v1/usages";

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("huitai-balance-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home/.pi/agent")).unwrap();
        fs::create_dir_all(root.join("home/.hermes")).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.home().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn accounts(&self) -> Vec<Account> {
        let h = self.home();
        vec![
            Account::new(Vendor::Pi, "默认", h.join(".pi/agent"), &h),
            Account::new(Vendor::Hermes, "默认", h.join(".hermes"), &h),
        ]
    }

    /// 典型配置：pi 里 DeepSeek（auth.json）、Kimi Code（models.json 引用环境变量）、一个写成命令的；
    /// Hermes .env 里 OpenRouter（默认 provider）；环境变量里 Moonshot 和 Kimi 的实际值。
    fn typical(&self) -> Found {
        self.write(
            ".pi/agent/auth.json",
            &format!(r#"{{"deepseek": {{"type": "api_key", "key": "{DEEPSEEK}"}}}}"#),
        );
        self.write(
            ".pi/agent/models.json",
            &format!(
                r#"{{"providers": {{
                    "kimi-coding": {{"apiKey": "$MY_KIMI"}},
                    "moonshotai-cn": {{"apiKey": "!touch {}"}},
                    "magpie": {{"apiKey": "local", "baseUrl": "http://127.0.0.1:3425/v1"}}
                }}}}"#,
                self.root.join("ran-command").display()
            ),
        );
        self.write(
            ".pi/agent/settings.json",
            r#"{"defaultProvider": "deepseek"}"#,
        );
        self.write(
            ".hermes/.env",
            &format!("# Hermes\nexport OPENROUTER_API_KEY=\"{OPENROUTER}\"\nOTHER=x\n"),
        );
        self.write(".hermes/config.yaml", "model:\n  provider: openrouter\n");
        let env = |n: &str| match n {
            "MY_KIMI" => Some(KIMI.to_owned()),
            "MOONSHOT_API_KEY" => Some(MOONSHOT.to_owned()),
            // 和 pi auth.json 里同一把：只查一次。
            "DEEPSEEK_API_KEY" => Some(DEEPSEEK.to_owned()),
            _ => None,
        };
        find(&self.accounts(), &env)
    }

    fn reader(&self, fake: &Fake, now: u64) -> BalanceReader {
        BalanceReader::new(self.data(), at(now)).with_origin(fake.origin.clone())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn serve_typical(fake: &Fake) {
    fake.route(
        P_DEEPSEEK,
        200,
        r#"{"is_available": true, "balance_infos": [{"currency": "CNY", "total_balance": "110.00", "granted_balance": "10.00", "topped_up_balance": "100.00"}]}"#,
    );
    // Moonshot：国内 key 不对（401，正文回显密钥），国际通过，欠费（cash 为负）。
    fake.route(
        P_MOONSHOT_CN,
        401,
        &format!(r#"{{"error": {{"message": "Invalid Authentication {MOONSHOT}"}}}}"#),
    );
    fake.route(
        P_MOONSHOT_AI,
        200,
        r#"{"code": 0, "data": {"available_balance": 2.5, "voucher_balance": 2.5, "cash_balance": -3.2}, "scode": "0x0", "status": true}"#,
    );
    fake.route(
        P_OPENROUTER,
        200,
        r#"{"data": {"label": "sk-or-v1-abc...", "limit": 20, "limit_remaining": 12.5, "usage": 7.5, "is_free_tier": false}}"#,
    );
    fake.route(
        P_KIMI,
        200,
        r#"{"usage": {"limit": "100", "used": "20", "remaining": "80", "resetTime": "2026-11-25T05:24:18.443553353Z"},
            "limits": [{"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"}, "detail": {"limit": "50", "remaining": "45", "resetTime": "2026-11-20T14:00:00Z"}}]}"#,
    );
}

fn row(rows: &[BalanceRow], p: Provider) -> &BalanceRow {
    rows.iter()
        .find(|r| r.provider == p)
        .unwrap_or_else(|| panic!("没有 {p:?}：{rows:?}"))
}

fn value(rows: &[BalanceRow], p: Provider) -> &BalanceValue {
    &row(rows, p).result.as_ref().expect("应当读到").value
}

/// 各家返回格式：金额字符串、欠费、有限额的用量、两种窗口写法；密钥只发给自己那家。
#[test]
fn reads_each_vendor_format_and_sends_keys_only_to_their_own_host() {
    let sb = Sandbox::new("formats");
    let fake = Fake::start();
    serve_typical(&fake);
    let found = sb.typical();
    assert_eq!(found.keys.len(), 4, "同一把 DeepSeek 密钥只留一份");
    assert_eq!(found.skipped.len(), 1, "写成命令的跳过");
    assert!(found.skipped[0].origin.place.contains("moonshotai-cn"));
    let rows = sb.reader(&fake, NOW).read_all(&found.keys);

    let BalanceValue::Money { amounts, available } = value(&rows, Provider::DeepSeek) else {
        panic!()
    };
    assert_eq!(
        (amounts[0].currency.as_str(), amounts[0].amount),
        ("CNY", 110.0)
    );
    assert!(*available);
    assert_eq!(row(&rows, Provider::DeepSeek).origins.len(), 2);

    // Moonshot 国内 401 → 换国际站；欠费标红。
    let BalanceValue::Money { amounts, available } = value(&rows, Provider::MoonshotIntl) else {
        panic!()
    };
    assert_eq!(
        (amounts[0].currency.as_str(), amounts[0].amount),
        ("USD", 2.5)
    );
    assert!(!*available, "cash_balance 为负是欠费");

    assert_eq!(
        value(&rows, Provider::OpenRouter),
        &BalanceValue::Credit {
            used: 7.5,
            limit: Some(20.0),
            remaining: Some(12.5),
            currency: "USD".to_owned()
        }
    );

    let BalanceValue::Windows(w) = value(&rows, Provider::KimiCode) else {
        panic!()
    };
    assert_eq!(w.len(), 2);
    assert_eq!((w[0].label.as_str(), w[0].used_percent), ("5 小时", 10.0));
    assert_eq!((w[1].label.as_str(), w[1].used_percent), ("周", 20.0));

    // 每把密钥只出现在自己那家的地址上。
    let own: [(&str, &[&str]); 4] = [
        (DEEPSEEK, &[P_DEEPSEEK]),
        (OPENROUTER, &[P_OPENROUTER]),
        (KIMI, &[P_KIMI]),
        (MOONSHOT, &[P_MOONSHOT_CN, P_MOONSHOT_AI]),
    ];
    for (path, auth) in fake.seen.lock().unwrap().iter() {
        let (key, paths) = own
            .iter()
            .find(|(k, _)| auth.contains(k))
            .unwrap_or_else(|| panic!("{path} 收到了认不出的头"));
        assert!(paths.contains(&path.as_str()), "{key} 被发到了 {path}");
        assert_eq!(auth, &format!("Bearer {key}"));
    }
    assert_eq!(fake.hits(P_DEEPSEEK), 1, "同一把密钥只查一次");
    assert!(
        !sb.root.join("ran-command").exists(),
        "写成命令的密钥绝不执行"
    );
}

/// 失败、负余额、格式变了、密钥不对：每种都有明确结果，不 panic，错误里不带密钥和响应正文。
#[test]
fn failures_and_negative_balance() {
    let sb = Sandbox::new("failures");
    let fake = Fake::start();
    let found = sb.typical();
    fake.route(P_DEEPSEEK, 500, &format!(r#"{{"error": "{DEEPSEEK}"}}"#));
    fake.route(
        P_MOONSHOT_CN,
        200,
        r#"{"code": 0, "data": {"available_balance": -1.25, "voucher_balance": 0, "cash_balance": -1.25}, "status": true}"#,
    );
    fake.route(
        P_OPENROUTER,
        200,
        r#"{"data": {"limit": null, "limit_remaining": null, "usage": 3.21}}"#,
    );
    // Kimi 的格式变了。
    fake.route(P_KIMI, 200, r#"{"quota": {"percent": 5}}"#);
    let rows = sb.reader(&fake, NOW).read_all(&found.keys);

    let err = |p| row(&rows, p).result.clone().unwrap_err();
    assert_eq!(err(Provider::DeepSeek), "HTTP 500");
    let BalanceValue::Money { amounts, available } = value(&rows, Provider::MoonshotCn) else {
        panic!()
    };
    assert_eq!(amounts[0].amount, -1.25);
    assert!(!*available, "负余额标红");
    assert_eq!(
        value(&rows, Provider::OpenRouter),
        &BalanceValue::Credit {
            used: 3.21,
            limit: None,
            remaining: None,
            currency: "USD".to_owned()
        }
    );
    assert!(
        err(Provider::KimiCode).contains("格式"),
        "{}",
        err(Provider::KimiCode)
    );

    // Moonshot 国内、国际都认证不过：报第一家、说密钥不对。
    fake.route(P_MOONSHOT_CN, 403, "{}");
    fake.route(P_MOONSHOT_AI, 401, "{}");
    let rows = sb.reader(&fake, NOW + 3600).read_all(&found.keys);
    let moonshot = row(&rows, Provider::MoonshotCn);
    assert_eq!(moonshot.result.as_ref().unwrap_err(), "密钥无效或没有权限");

    for r in &rows {
        let text = format!("{r:?}");
        for k in ALL_KEYS {
            assert!(!text.contains(k), "{k} 出现在结果里：{text}");
        }
    }
}

/// 5 分钟缓存：5 分钟内不重问；429 退避：退避期内一次都不问，过了再问，连着被限流就等更久。
#[test]
fn caches_five_minutes_and_backs_off_on_429() {
    let sb = Sandbox::new("cache");
    let fake = Fake::start();
    serve_typical(&fake);
    let found = sb.typical();
    sb.reader(&fake, NOW).read_all(&found.keys);
    sb.reader(&fake, NOW + 299).read_all(&found.keys);
    assert_eq!(fake.hits(P_DEEPSEEK), 1, "5 分钟内用缓存");
    sb.reader(&fake, NOW + 300).read_all(&found.keys);
    assert_eq!(fake.hits(P_DEEPSEEK), 2, "过了 5 分钟重问");

    fake.route(P_DEEPSEEK, 429, "{}");
    let rows = sb.reader(&fake, NOW + 600).read_all(&found.keys);
    assert_eq!(fake.hits(P_DEEPSEEK), 3);
    // 被限流时接着显示上次的读数。
    assert!(matches!(
        value(&rows, Provider::DeepSeek),
        BalanceValue::Money { .. }
    ));
    sb.reader(&fake, NOW + 600 + 9 * 60).read_all(&found.keys);
    assert_eq!(fake.hits(P_DEEPSEEK), 3, "退避 10 分钟内不问");
    sb.reader(&fake, NOW + 600 + 10 * 60).read_all(&found.keys);
    assert_eq!(fake.hits(P_DEEPSEEK), 4, "过了退避期再问");
    sb.reader(&fake, NOW + 600 + 29 * 60).read_all(&found.keys);
    assert_eq!(fake.hits(P_DEEPSEEK), 4, "第二次被限流退避 20 分钟");
    fake.route(
        P_DEEPSEEK,
        200,
        r#"{"is_available": true, "balance_infos": [{"currency": "USD", "total_balance": "1.00"}]}"#,
    );
    let rows = sb.reader(&fake, NOW + 600 + 31 * 60).read_all(&found.keys);
    assert_eq!(fake.hits(P_DEEPSEEK), 5);
    let BalanceValue::Money { amounts, .. } = value(&rows, Provider::DeepSeek) else {
        panic!()
    };
    assert_eq!(amounts[0].currency, "USD");
}

/// 请求超时：5 秒就放弃，报超时，不带密钥。
#[test]
fn times_out_after_five_seconds() {
    let sb = Sandbox::new("timeout");
    let fake = Fake::start();
    sb.write(
        ".pi/agent/auth.json",
        &format!(r#"{{"deepseek": {{"type": "api_key", "key": "{DEEPSEEK}"}}}}"#),
    );
    fake.route(P_DEEPSEEK, 0, "");
    let found = find(&sb.accounts(), &|_| None);
    let started = std::time::Instant::now();
    let rows = sb.reader(&fake, NOW).read_all(&found.keys);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(8), "{took:?}");
    let e = rows[0].result.as_ref().unwrap_err();
    assert!(e.contains("5 秒"), "{e}");
}

/// 密钥不落盘：查完以后数据目录里每个文件（缓存）都不含任何一把密钥；缓存只有自己能读。
#[test]
fn keys_never_reach_disk() {
    let sb = Sandbox::new("disk");
    let fake = Fake::start();
    serve_typical(&fake);
    fake.route(
        P_OPENROUTER,
        429,
        &format!(r#"{{"error": "{OPENROUTER}"}}"#),
    );
    let found = sb.typical();
    sb.reader(&fake, NOW).read_all(&found.keys);
    let files = all_files(&sb.data());
    assert!(!files.is_empty(), "应当写了缓存");
    for f in files {
        let text = fs::read_to_string(&f).unwrap();
        for k in ALL_KEYS {
            assert!(!text.contains(k), "{k} 写进了 {}", f.display());
        }
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", f.display());
    }
    assert!(!format!("{found:?}").contains(DEEPSEEK), "Debug 里只有星号");
}

/// 顶栏用的：pi / Hermes 账号默认 provider 的读数。
#[test]
fn report_picks_default_provider_of_account() {
    let sb = Sandbox::new("defaults");
    let fake = Fake::start();
    serve_typical(&fake);
    let found = sb.typical();
    let accounts = sb.accounts();
    let report = BalanceReport {
        rows: sb.reader(&fake, NOW).read_all(&found.keys),
        skipped: found.skipped,
        defaults: defaults(&accounts),
    };
    let (row, _) = report
        .for_account(Vendor::Pi, "")
        .expect("pi 默认 deepseek");
    assert_eq!(row.provider, Provider::DeepSeek);
    let (row, _) = report
        .for_account(Vendor::Hermes, "")
        .expect("Hermes 默认 openrouter");
    assert_eq!(row.provider, Provider::OpenRouter);
    assert!(report.for_account(Vendor::Claude, "").is_none());
}

fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(all_files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// 配了自己服务地址（网关、代理）的密钥一律不查，也不发出去：pi 内置名字改了 `baseUrl`（auth.json 和 models.json
/// 两处）、Hermes `.env` 里的 `KIMI_BASE_URL`、会话台环境里的 `MOONSHOT_BASE_URL`；同一把网关密钥在环境变量里再出现也不查。
/// 地址是原厂的照查，按地址认是哪家。
#[test]
fn gateway_keys_are_never_sent_to_vendors() {
    const GW_DEEPSEEK: &str = "sk-gw-deepseek-0011";
    const GW_OPENROUTER: &str = "sk-gw-openrouter-0012";
    const GW_KIMI: &str = "sk-kimi-gw-0013";
    const GW_MOONSHOT: &str = "sk-gw-moonshot-0014";
    let sb = Sandbox::new("gateway");
    let fake = Fake::start();
    serve_typical(&fake);
    sb.write(
        ".pi/agent/auth.json",
        &format!(r#"{{"deepseek": {{"type": "api_key", "key": "{GW_DEEPSEEK}"}}}}"#),
    );
    sb.write(
        ".pi/agent/models.json",
        &format!(
            r#"{{"providers": {{
                "deepseek": {{"baseUrl": "https://gw.example.com/v1"}},
                "openrouter": {{"apiKey": "{GW_OPENROUTER}", "baseUrl": "http://127.0.0.1:3425/v1"}},
                "moonshotai": {{"apiKey": "{MOONSHOT}", "baseUrl": "https://api.moonshot.cn/v1"}}
            }}}}"#
        ),
    );
    sb.write(
        ".pi/agent/settings.json",
        r#"{"defaultProvider": "deepseek"}"#,
    );
    sb.write(
        ".hermes/.env",
        &format!("KIMI_API_KEY={GW_KIMI}\nKIMI_BASE_URL=https://kimi-gw.example.com/coding\nMOONSHOT_API_KEY={GW_MOONSHOT}\n"),
    );
    let env = |n: &str| match n {
        "MOONSHOT_BASE_URL" => Some("https://moonshot-gw.example.com/v1".to_owned()),
        // 和 pi auth.json 里那把网关密钥是同一把，这里没配地址也不查。
        "DEEPSEEK_API_KEY" => Some(GW_DEEPSEEK.to_owned()),
        _ => None,
    };
    let accounts = sb.accounts();
    let found = find(&accounts, &env);
    let rows = sb.reader(&fake, NOW).read_all(&found.keys);

    // 只剩 Moonshot：地址是国内站，只查国内。
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].provider, Provider::MoonshotCn);
    assert_eq!(fake.hits(P_MOONSHOT_AI), 0, "地址定了国内站，不试国际");
    for (path, auth) in fake.seen.lock().unwrap().iter() {
        for k in [GW_DEEPSEEK, GW_OPENROUTER, GW_KIMI, GW_MOONSHOT] {
            assert!(!auth.contains(k), "网关密钥 {k} 发给了 {path}");
        }
    }
    let places: Vec<&str> = found
        .skipped
        .iter()
        .map(|s| s.origin.place.as_str())
        .collect();
    for want in [
        "auth.json · deepseek",
        "models.json · openrouter",
        ".env · KIMI_API_KEY",
        ".env · MOONSHOT_API_KEY",
        "环境变量 DEEPSEEK_API_KEY",
    ] {
        assert!(places.contains(&want), "{want} 应记成跳过：{places:?}");
    }
    assert!(
        found
            .skipped
            .iter()
            .all(|s| !s.reason.contains("example.com"))
    );
    // pi 默认的 deepseek 改到了网关：顶栏不拿别处 DeepSeek 的读数顶替。
    assert!(
        !defaults(&accounts)
            .iter()
            .any(|((v, _), _)| *v == Vendor::Pi),
        "{:?}",
        defaults(&accounts)
    );
}

/// 同一个来源（同一个环境变量）换了一把密钥：5 分钟缓存和限流退避都不串到新密钥上，新密钥照问；缓存里没有密钥。
#[test]
fn cache_is_per_key_not_per_place() {
    const OTHER: &str = "sk-fake-deepseek-9999";
    let sb = Sandbox::new("rotate");
    let fake = Fake::start();
    serve_typical(&fake);
    let found_with = |k: &'static str| {
        find(&[], &move |n: &str| {
            (n == "DEEPSEEK_API_KEY").then(|| k.to_owned())
        })
    };
    let last_auth = || fake.seen.lock().unwrap().last().unwrap().1.clone();

    sb.reader(&fake, NOW).read_all(&found_with(DEEPSEEK).keys);
    assert_eq!(fake.hits(P_DEEPSEEK), 1);
    sb.reader(&fake, NOW + 60).read_all(&found_with(OTHER).keys);
    assert_eq!(fake.hits(P_DEEPSEEK), 2, "换了密钥不用上一把的缓存");
    assert_eq!(last_auth(), format!("Bearer {OTHER}"));
    sb.reader(&fake, NOW + 120)
        .read_all(&found_with(DEEPSEEK).keys);
    assert_eq!(fake.hits(P_DEEPSEEK), 2, "原来那把 5 分钟内照旧用缓存");

    // 一把被限流退避，另一把照问。
    fake.route(P_DEEPSEEK, 429, "{}");
    sb.reader(&fake, NOW + 400)
        .read_all(&found_with(DEEPSEEK).keys);
    assert_eq!(fake.hits(P_DEEPSEEK), 3);
    fake.route(
        P_DEEPSEEK,
        200,
        r#"{"is_available": true, "balance_infos": [{"currency": "USD", "total_balance": "1.00"}]}"#,
    );
    let rows = sb
        .reader(&fake, NOW + 420)
        .read_all(&found_with(OTHER).keys);
    assert_eq!(fake.hits(P_DEEPSEEK), 4, "退避只管被限流的那把");
    let BalanceValue::Money { amounts, .. } = value(&rows, Provider::DeepSeek) else {
        panic!()
    };
    assert_eq!(amounts[0].currency, "USD");

    for f in all_files(&sb.data()) {
        let text = fs::read_to_string(&f).unwrap();
        assert!(
            !text.contains(DEEPSEEK) && !text.contains(OTHER),
            "{}",
            f.display()
        );
    }
}

/// 地址配在文件里、密钥放在环境变量里（pi 官方写法：`models.json` 只给内置 provider 配 `baseUrl`；Hermes `.env`
/// 只写 `KIMI_BASE_URL`；自定义 provider 的 `apiKey` 引用环境变量）：环境变量里的这把是网关的，不发给原厂、记成跳过。地址是原厂的照查。
#[test]
fn gateway_address_in_file_key_in_env_is_not_sent() {
    const GW_DEEPSEEK: &str = "sk-gw-deepseek-0021";
    const GW_KIMI: &str = "sk-kimi-gw-0022";
    const GW_OPENROUTER: &str = "sk-gw-openrouter-0023";
    const GW_CORP: &str = "sk-gw-corp-0024";
    let sb = Sandbox::new("gateway-env");
    let fake = Fake::start();
    serve_typical(&fake);
    sb.write(
        ".pi/agent/models.json",
        r#"{"providers": {
            "deepseek": {"baseUrl": "https://gw.example.com/v1"},
            "openrouter": {"apiKey": "sk-literal-not-env", "baseUrl": "http://127.0.0.1:3425/v1"},
            "moonshotai-cn": {"baseUrl": "https://api.moonshot.cn/v1"},
            "corp": {"baseUrl": "https://corp-gw.example.com/v1", "apiKey": "${KIMI_CN_API_KEY}"}
        }}"#,
    );
    sb.write(
        ".hermes/.env",
        "KIMI_BASE_URL=https://kimi-gw.example.com/coding\n",
    );
    let env = |n: &str| match n {
        "DEEPSEEK_API_KEY" => Some(GW_DEEPSEEK.to_owned()),
        "KIMI_API_KEY" => Some(GW_KIMI.to_owned()),
        "OPENROUTER_API_KEY" => Some(GW_OPENROUTER.to_owned()),
        "KIMI_CN_API_KEY" => Some(GW_CORP.to_owned()),
        "MOONSHOT_API_KEY" => Some(MOONSHOT.to_owned()),
        _ => None,
    };
    let found = find(&sb.accounts(), &env);
    let rows = sb.reader(&fake, NOW).read_all(&found.keys);

    for (path, auth) in fake.seen.lock().unwrap().iter() {
        for k in [GW_DEEPSEEK, GW_KIMI, GW_OPENROUTER, GW_CORP] {
            assert!(!auth.contains(k), "网关密钥 {k} 发给了 {path}");
        }
    }
    let places: Vec<&str> = found
        .skipped
        .iter()
        .map(|s| s.origin.place.as_str())
        .collect();
    for want in [
        "环境变量 DEEPSEEK_API_KEY",
        "环境变量 KIMI_API_KEY",
        "环境变量 OPENROUTER_API_KEY",
        "环境变量 KIMI_CN_API_KEY",
    ] {
        assert!(places.contains(&want), "{want} 应记成跳过：{places:?}");
    }
    assert!(
        found
            .skipped
            .iter()
            .all(|s| !s.reason.contains("example.com"))
    );
    // 原厂地址（Moonshot 国内）照查。
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].origins[0].place, "环境变量 MOONSHOT_API_KEY");
    assert!(fake.hits(P_MOONSHOT_CN) >= 1);
}

/// Hermes 的 `kimi-coding` 随密钥走 Kimi Code 或 Moonshot 开放平台：旧的开放平台密钥查到 Moonshot 余额时顶栏照样认；
/// `kimi-coding-cn` 是 Moonshot 国内。
#[test]
fn hermes_kimi_providers_show_moonshot_balance() {
    const OLD_MOONSHOT: &str = "sk-fake-old-moonshot-0031";
    const CN_MOONSHOT: &str = "sk-fake-cn-moonshot-0032";
    let sb = Sandbox::new("hermes-kimi");
    let fake = Fake::start();
    serve_typical(&fake);
    // Kimi Code 不认这把旧密钥，Moonshot 国际站认。
    fake.route(P_KIMI, 401, "{}");
    let h = sb.home();
    sb.write(".hermes/.env", &format!("KIMI_API_KEY={OLD_MOONSHOT}\n"));
    sb.write(".hermes/config.yaml", "model:\n  provider: kimi-coding\n");
    sb.write(
        "hermes-cn/.env",
        &format!("KIMI_CN_API_KEY={CN_MOONSHOT}\n"),
    );
    sb.write(
        "hermes-cn/config.yaml",
        "model:\n  provider: kimi-coding-cn\n",
    );
    let cn = Account::new(Vendor::Hermes, "国内", h.join("hermes-cn"), &h);
    let accounts = vec![
        Account::new(Vendor::Hermes, "默认", h.join(".hermes"), &h),
        cn.clone(),
    ];
    fake.route(
        P_MOONSHOT_CN,
        200,
        r#"{"code": 0, "data": {"available_balance": 8.0, "voucher_balance": 0, "cash_balance": 8.0}, "scode": "0x0", "status": true}"#,
    );
    let found = find(&accounts, &|_| None);
    let report = BalanceReport {
        rows: sb.reader(&fake, NOW).read_all(&found.keys),
        skipped: found.skipped,
        defaults: defaults(&accounts),
    };
    let (row, _) = report
        .for_account(Vendor::Hermes, "")
        .expect("kimi-coding 的旧密钥查到 Moonshot 国际，顶栏要认");
    assert_eq!(row.provider, Provider::MoonshotIntl);
    assert_eq!(fake.hits(P_KIMI), 1, "先试 Kimi Code");
    let (row, _) = report
        .for_account(Vendor::Hermes, &cn.key)
        .expect("kimi-coding-cn 是 Moonshot 国内");
    assert_eq!(row.provider, Provider::MoonshotCn);
}
