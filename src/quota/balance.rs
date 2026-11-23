//! 余额渠道：API 余额（DeepSeek、Moonshot、OpenRouter）和订阅额度（Kimi Code）。
//!
//! 密钥只读自 pi / Hermes 的配置和环境变量（[`super::keys`]），只发给它自己那家：请求地址的域名必须在
//! [`Provider::hosts`] 里，否则不发。每把密钥的读数缓存 5 分钟（缓存文件里只有读数和密钥的截断哈希指纹，没有密钥；
//! 同一来源换了密钥就是另一条，不会显示上一把的读数），请求超时 5 秒，
//! 被限流（HTTP 429）就退避，越退越久。错误信息只写 HTTP 状态，不带响应正文（有的服务会在正文里回显密钥）。

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::keys::{self, FoundKey, KeyOrigin, Secret, Skipped};
use super::{curl, time, window_label};
use crate::core::exec;
use crate::core::{Account, Config, QuotaWindow, Vendor};

/// 能查余额或订阅额度的几家。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[serde(rename = "deepseek")]
    DeepSeek,
    MoonshotCn,
    MoonshotIntl,
    #[serde(rename = "openrouter")]
    OpenRouter,
    KimiCode,
}

impl Provider {
    pub const ALL: [Provider; 5] = [
        Provider::DeepSeek,
        Provider::MoonshotCn,
        Provider::MoonshotIntl,
        Provider::OpenRouter,
        Provider::KimiCode,
    ];

    /// `rules.json` 里 `balance.providers` 下的名字。
    pub fn key(self) -> &'static str {
        match self {
            Provider::DeepSeek => "deepseek",
            Provider::MoonshotCn => "moonshot_cn",
            Provider::MoonshotIntl => "moonshot_intl",
            Provider::OpenRouter => "openrouter",
            Provider::KimiCode => "kimi_code",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::DeepSeek => "DeepSeek",
            Provider::MoonshotCn => "Moonshot",
            Provider::MoonshotIntl => "Moonshot 国际",
            Provider::OpenRouter => "OpenRouter",
            Provider::KimiCode => "Kimi Code",
        }
    }

    /// 这家的域名：密钥只发往这些域名，pi 自定义 provider 的 `baseUrl` 也按它认。
    pub fn hosts(self) -> &'static [&'static str] {
        match self {
            Provider::DeepSeek => &["api.deepseek.com"],
            Provider::MoonshotCn => &["api.moonshot.cn"],
            Provider::MoonshotIntl => &["api.moonshot.ai"],
            Provider::OpenRouter => &["openrouter.ai"],
            Provider::KimiCode => &["api.kimi.com"],
        }
    }

    /// 没有公开文档的接口（照官方开源工具的写法接），界面上标「非公开接口，可能失效」。
    pub fn unofficial(self) -> bool {
        !BalanceRules::builtin().provider(self).public
    }
}

/// 一次读数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Balance {
    pub value: BalanceValue,
    pub at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BalanceValue {
    /// 余额（可能有几种币）；`available` 为假是余额不够调用或欠费，标红。
    Money {
        amounts: Vec<Money>,
        available: bool,
    },
    /// 按用量计费的额度（OpenRouter）：已用多少，有限额时还剩多少。
    Credit {
        used: f64,
        limit: Option<f64>,
        remaining: Option<f64>,
        currency: String,
    },
    /// 订阅额度窗口（5 小时、周）。
    Windows(Vec<QuotaWindow>),
}

/// 有限额时这一期的用量（面板、顶栏、`ai doctor` 共用）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreditPeriod {
    pub used: f64,
    pub left: f64,
    pub limit: f64,
}

impl CreditPeriod {
    /// 已用比例（0 到 1）；限额 0 算用完。
    pub fn ratio(&self) -> f64 {
        if self.limit > 0.0 {
            (self.used / self.limit).clamp(0.0, 1.0)
        } else {
            1.0
        }
    }
}

impl BalanceValue {
    /// [`BalanceValue::Credit`] 设了限额时这一期用了多少：OpenRouter 的 `usage` 是累计用量（不分期），
    /// 限额可以按天、周、月重置，所以这一期已用 = 限额 − 本期剩余（`limit_remaining`）；没给剩余才拿累计用量估。
    /// 限额 0 就是用完了。没设限额（`limit` 为空）或不是这一种读数是 `None`。
    pub fn credit_period(&self) -> Option<CreditPeriod> {
        let BalanceValue::Credit {
            used,
            limit,
            remaining,
            ..
        } = self
        else {
            return None;
        };
        let limit = (*limit)?.max(0.0);
        let left = remaining.unwrap_or(limit - used).clamp(0.0, limit);
        Some(CreditPeriod {
            used: limit - left,
            left,
            limit,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Money {
    pub currency: String,
    pub amount: f64,
}

/// 一把密钥查到的结果。
#[derive(Debug, Clone)]
pub struct BalanceRow {
    /// 最后问的是哪家（几家依次试时是认证通过的那家，都没通过是第一家）。
    pub provider: Provider,
    pub origins: Vec<KeyOrigin>,
    pub result: Result<Balance, String>,
}

/// 全部密钥的结果，加上没读的（写成命令的）和各 pi / Hermes 账号默认用哪家。
#[derive(Debug, Clone, Default)]
pub struct BalanceReport {
    pub rows: Vec<BalanceRow>,
    pub skipped: Vec<Skipped>,
    /// （厂商，账号短名）→ 这个账号默认用的那家。一个账号可以有几条：默认的名字对应同一公司的几家
    /// （Hermes 的 `kimi-coding` 是 Kimi Code、Moonshot 国际、国内），读数是其中哪家都算。
    pub defaults: Vec<((Vendor, String), Provider)>,
}

impl BalanceReport {
    /// 这个账号默认 provider 的读数（顶栏给 pi / Hermes 显示）：密钥要是从这个账号的配置里读到的，
    /// 或者来自环境变量；没订套餐、取不到都算没有。
    pub fn for_account(&self, vendor: Vendor, key: &str) -> Option<(&BalanceRow, &Balance)> {
        let ps: Vec<Provider> = self
            .defaults
            .iter()
            .filter(|((v, k), _)| *v == vendor && k == key)
            .map(|(_, p)| *p)
            .collect();
        let mine = |r: &&BalanceRow| {
            r.origins.iter().any(|o| match &o.account {
                Some(a) => a.vendor == vendor && a.key == key,
                None => false,
            })
        };
        let env = |r: &&BalanceRow| r.origins.iter().any(|o| o.account.is_none());
        let of_p = || self.rows.iter().filter(|r| ps.contains(&r.provider));
        let row = of_p().find(mine).or_else(|| of_p().find(env))?;
        row.result.as_ref().ok().map(|b| (row, b))
    }
}

/// 本机：读所有账号配置里的密钥和环境变量，逐个查（有缓存的用缓存）。
pub fn balances(accounts: &[Account]) -> BalanceReport {
    let env = |name: &str| std::env::var(name).ok();
    let found = keys::find(accounts, &env);
    let rows = match Config::data_dir() {
        Some(dir) => BalanceReader::new(dir, SystemTime::now()).read_all(&found.keys),
        None => Vec::new(),
    };
    BalanceReport {
        rows,
        skipped: found.skipped,
        defaults: defaults(accounts),
    }
}

/// 各 pi / Hermes 账号默认用哪家（不在我们能查的几家里的不列）。
pub fn defaults(accounts: &[Account]) -> Vec<((Vendor, String), Provider)> {
    accounts
        .iter()
        .flat_map(|a| {
            keys::default_providers(a)
                .into_iter()
                .map(|p| ((a.vendor, a.key.clone()), p))
        })
        .collect()
}

/// 发一次 GET：地址、`Authorization` 那一行头、超时 → HTTP 状态码和响应正文。
pub type HttpFetch = dyn Fn(&str, &str, Duration) -> Result<(u16, String), String> + Send + Sync;

/// 查余额的环境：缓存放哪、现在几点、怎么发请求。
pub struct BalanceReader {
    data_dir: PathBuf,
    now: SystemTime,
    rules: &'static BalanceRules,
    fetch: Box<HttpFetch>,
    /// 测试用：所有请求改发到这个地址，路径前面加上原来的域名（`http://127.0.0.1:端口/api.deepseek.com/user/balance`）。
    origin: Option<String>,
}

impl BalanceReader {
    pub fn new(data_dir: PathBuf, now: SystemTime) -> Self {
        Self {
            data_dir,
            now,
            rules: BalanceRules::builtin(),
            fetch: Box::new(curl_get),
            origin: None,
        }
    }

    pub fn with_fetch(mut self, fetch: Box<HttpFetch>) -> Self {
        self.fetch = fetch;
        self
    }

    /// 见 [`BalanceReader::origin`]。
    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }

    pub fn read_all(&self, found: &[FoundKey]) -> Vec<BalanceRow> {
        let mut cache = Cache::load(&self.cache_path());
        let rows = found.iter().map(|k| self.read(k, &mut cache)).collect();
        // 换掉的密钥（指纹变了）留下的旧条目没人再读：一天没更新、也不在退避里的就删掉。
        let now = time::to_secs(self.now);
        cache
            .entries
            .retain(|_, e| now - e.at < STALE_SECS || e.backoff_until.is_some_and(|t| t > now));
        // 缓存写不进去只是下次多问一次，不影响这次读数。
        let _ = cache.save(&self.data_dir, &self.cache_path());
        rows
    }

    fn cache_path(&self) -> PathBuf {
        self.data_dir.join(&self.rules.cache_file)
    }

    fn read(&self, key: &FoundKey, cache: &mut Cache) -> BalanceRow {
        // 来源 + 密钥指纹：同一个变量、同一个文件位置换了密钥（两个终端各设一把、轮换）不会串用读数和退避。
        let id = format!("{}#{}", key.origins[0].id(), key.secret.fingerprint());
        let now = time::to_secs(self.now);
        let row = |provider, result| BalanceRow {
            provider,
            origins: key.origins.clone(),
            result,
        };
        let cached = cache.entries.get(&id).cloned();
        if let Some(c) = &cached {
            let fresh = (0..self.rules.cache_minutes as i64 * 60).contains(&(now - c.at));
            let same = key.providers.contains(&c.provider);
            if same && fresh {
                return row(c.provider, c.result.clone());
            }
            if same && c.backoff_until.is_some_and(|t| t > now) {
                // 退避中：有上次的读数就接着用，没有就说还在等。
                let result = match &c.result {
                    Ok(b) => Ok(b.clone()),
                    Err(_) => Err(format!(
                        "被限流，{} 分钟后再试",
                        ((c.backoff_until.unwrap_or(now) - now + 59) / 60)
                    )),
                };
                return row(c.provider, result);
            }
        }
        let mut provider = key.providers[0];
        let mut outcome = Outcome::Failed("没有可试的地址".to_owned());
        for &p in &key.providers {
            provider = p;
            outcome = self.ask(p, &key.secret);
            // 认证不过换同一公司的下一个站点（国内 / 国际），别的结果就是定论。
            if !matches!(outcome, Outcome::Unauthorized) {
                break;
            }
        }
        if matches!(outcome, Outcome::Unauthorized) {
            provider = key.providers[0];
        }
        let step = cached
            .as_ref()
            .filter(|c| c.provider == provider)
            .map_or(0, |c| c.backoff_step);
        let (result, backoff) = match outcome {
            Outcome::Ok(v) => (
                Ok(Balance {
                    value: v,
                    at: self.now,
                }),
                None,
            ),
            Outcome::Unauthorized => (Err("密钥无效或没有权限".to_owned()), None),
            Outcome::Failed(e) => (Err(e), None),
            Outcome::RateLimited => {
                let minutes =
                    (self.rules.backoff_minutes << step.min(8)).min(self.rules.backoff_max_minutes);
                let last = cached
                    .as_ref()
                    .filter(|c| c.provider == provider)
                    .and_then(|c| c.result.clone().ok());
                let result = last.ok_or(format!("被限流，{minutes} 分钟后再试"));
                (result, Some((now + minutes as i64 * 60, step + 1)))
            }
        };
        let entry = CacheEntry {
            provider,
            // 退避时沿用上次读数的时间，过了退避期就重问。
            at: match (&backoff, &cached) {
                (Some(_), Some(c)) => c.at,
                _ => now,
            },
            result: result.clone(),
            backoff_until: backoff.map(|b| b.0),
            backoff_step: backoff.map_or(0, |b| b.1),
        };
        cache.entries.insert(id, entry);
        row(provider, result)
    }

    /// 问一家一次。
    fn ask(&self, p: Provider, secret: &Secret) -> Outcome {
        let rule = self.rules.provider(p);
        let Some(host) = host_of(&rule.url) else {
            return Outcome::Failed("规则里的地址不对".to_owned());
        };
        // 只发给自己那家：规则地址的域名必须是这家的。
        if !p.hosts().contains(&host) {
            return Outcome::Failed(format!("地址 {host} 不是 {} 的，不发密钥", p.name()));
        }
        let url = match &self.origin {
            Some(o) => format!("{o}/{host}{}", path_of(&rule.url)),
            None => rule.url.clone(),
        };
        let auth = format!("Authorization: Bearer {}", secret.expose());
        let timeout = Duration::from_secs(self.rules.timeout_secs);
        let (status, body) = match (self.fetch)(&url, &auth, timeout) {
            Ok(r) => r,
            Err(e) => return Outcome::Failed(e),
        };
        match status {
            200..=299 => {}
            401 | 403 => return Outcome::Unauthorized,
            429 => return Outcome::RateLimited,
            s => return Outcome::Failed(format!("HTTP {s}")),
        }
        let Ok(doc) = serde_json::from_str::<Value>(&body) else {
            return Outcome::Failed("响应不是 JSON".to_owned());
        };
        match rule.parse.read(&doc, self.now) {
            Ok(v) => Outcome::Ok(v),
            Err(e) => Outcome::Failed(e),
        }
    }
}

enum Outcome {
    Ok(BalanceValue),
    Unauthorized,
    RateLimited,
    Failed(String),
}

fn host_of(url: &str) -> Option<&str> {
    url.split("://").nth(1)?.split(['/', ':']).next()
}

fn path_of(url: &str) -> &str {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.find('/').map_or("", |i| &rest[i..])
}

/// 真实请求：系统自带的 curl，`-q` 不读 `~/.curlrc`（[`curl::argv`]）；带密钥的那行头经标准输入给（`-H @-`），
/// 不进命令行参数，别的进程看不到。不跟随跳转（密钥不会被带到别的域名）。错误信息不带 curl 的输出。
fn curl_get(url: &str, auth: &str, timeout: Duration) -> Result<(u16, String), String> {
    let max = timeout.as_secs().max(1).to_string();
    let argv = curl::argv(&[
        "-sS",
        "--proto",
        "=https,http",
        "--max-time",
        &max,
        "--noproxy",
        "127.0.0.1,localhost",
        "-H",
        "@-",
        "-H",
        "Accept: application/json",
        "-w",
        "\n%{http_code}",
        url,
    ]);
    let out = exec::run(
        argv[0],
        &argv[1..],
        Some(&format!("{auth}\n")),
        timeout + Duration::from_secs(2),
    )
    .map_err(|_| format!("请求失败或超过 {} 秒没响应", timeout.as_secs()))?;
    let (body, code) = out.rsplit_once('\n').ok_or("响应不完整")?;
    let code = code.trim().parse::<u16>().map_err(|_| "响应不完整")?;
    if code == 0 {
        return Err("连不上".to_owned());
    }
    Ok((code, body.to_owned()))
}

// ---- 缓存 ----

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    provider: Provider,
    /// 读数时间（Unix 秒）。
    at: i64,
    result: Result<Balance, String>,
    #[serde(default)]
    backoff_until: Option<i64>,
    #[serde(default)]
    backoff_step: u32,
}

/// 缓存条目多久没更新就删（[`BalanceReader::read_all`]）。
const STALE_SECS: i64 = 24 * 60 * 60;

/// 缓存文件：密钥来源的名字（[`KeyOrigin::id`]）加密钥指纹（[`Secret::fingerprint`]，不含密钥）→ 上次的结果。
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    entries: BTreeMap<String, CacheEntry>,
}

impl Cache {
    /// 逐条解码：旧版写的、这一版不认识的条目（不再支持的智谱 GLM 等）跳过，不让整份缓存作废。
    fn load(path: &Path) -> Self {
        #[derive(Deserialize)]
        struct Raw {
            entries: BTreeMap<String, Value>,
        }
        let raw = fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Raw>(&t).ok());
        let entries = raw
            .map(|r| r.entries)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(k, v)| Some((k, serde_json::from_value(v).ok()?)))
            .collect();
        Self { entries }
    }

    /// 先写临时文件再改名，几个会话台同时写也不会读到半截；目录和文件只有自己能读。
    fn save(&self, dir: &Path, path: &Path) -> std::io::Result<()> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        let json = serde_json::to_string(self).map_err(std::io::Error::other)?;
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(json.as_bytes())?;
        fs::rename(&tmp, path)
    }
}

// ---- 规则 ----

#[derive(Debug, Deserialize)]
struct BalanceRules {
    timeout_secs: u64,
    cache_minutes: u64,
    backoff_minutes: u64,
    backoff_max_minutes: u64,
    cache_file: String,
    providers: HashMap<String, ProviderRule>,
}

impl BalanceRules {
    fn builtin() -> &'static BalanceRules {
        static RULES: OnceLock<BalanceRules> = OnceLock::new();
        RULES.get_or_init(|| {
            let mut doc: Value = serde_json::from_str(include_str!("rules.json"))
                .expect("quota/rules.json 格式不对");
            serde_json::from_value(doc["balance"].take()).expect("rules.json 的 balance 格式不对")
        })
    }

    fn provider(&self, p: Provider) -> &ProviderRule {
        self.providers
            .get(p.key())
            .expect("rules.json 里每家都有（rules 单测保证）")
    }
}

/// 一家的规则；密钥一律用 `Authorization: Bearer <key>` 发。
#[derive(Debug, Deserialize)]
struct ProviderRule {
    url: String,
    public: bool,
    parse: Parse,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Parse {
    /// 一串币种和金额，外加一个整体「能不能用」。
    MoneyList {
        list: String,
        currency: String,
        amount: String,
        available: String,
    },
    /// 一个金额，币种固定；`debt` 那个值是负数时算欠费。
    Money {
        amount: String,
        #[serde(default)]
        debt: Option<String>,
        currency: String,
    },
    Credit {
        usage: String,
        limit: String,
        remaining: String,
        currency: String,
    },
    Windows {
        windows: Vec<WindowSpec>,
    },
}

/// 一组额度窗口在 JSON 里的位置。
#[derive(Debug, Deserialize)]
struct WindowSpec {
    /// 数组（逐项看）或单个对象。
    items: String,
    /// 数字在每项的这个子对象里（没有就在项本身）。
    #[serde(default)]
    detail: Option<String>,
    /// 已用百分比按 `used`/`limit` 或 (`limit`−`remaining`)/`limit` 算。
    #[serde(default)]
    used: Option<String>,
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    remaining: Option<String>,
    /// 依次找的重置时间：数字（秒）或 RFC 3339。
    #[serde(default)]
    resets_at: Vec<String>,
    /// 窗口长度 = `duration` × `units` 里 `unit` 对应的分钟数（单位按包含哪个键认）。
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    units: BTreeMap<String, u32>,
    /// 固定的窗口名（Kimi 的周汇总没有窗口长度）。
    #[serde(default)]
    label: Option<String>,
}

/// 数字或数字字符串。
fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|f: &f64| f.is_finite())
}

const UNRECOGNIZED: &str = "没有认得的字段（格式可能变了）";

impl Parse {
    fn read(&self, doc: &Value, now: SystemTime) -> Result<BalanceValue, String> {
        match self {
            Parse::MoneyList {
                list,
                currency,
                amount,
                available,
            } => {
                let items = doc
                    .pointer(list)
                    .and_then(Value::as_array)
                    .ok_or(UNRECOGNIZED)?;
                let amounts: Vec<Money> = items
                    .iter()
                    .filter_map(|i| {
                        Some(Money {
                            currency: i.pointer(currency)?.as_str()?.to_owned(),
                            amount: num(i.pointer(amount))?,
                        })
                    })
                    .collect();
                let ok = doc.pointer(available).and_then(Value::as_bool);
                if amounts.is_empty() && ok.is_none() {
                    return Err(UNRECOGNIZED.to_owned());
                }
                let available = ok.unwrap_or(true) && amounts.iter().all(|m| m.amount >= 0.0);
                Ok(BalanceValue::Money { amounts, available })
            }
            Parse::Money {
                amount,
                debt,
                currency,
            } => {
                let a = num(doc.pointer(amount)).ok_or(UNRECOGNIZED)?;
                let owed = debt
                    .as_ref()
                    .and_then(|d| num(doc.pointer(d)))
                    .is_some_and(|d| d < 0.0);
                Ok(BalanceValue::Money {
                    amounts: vec![Money {
                        currency: currency.clone(),
                        amount: a,
                    }],
                    available: a > 0.0 && !owed,
                })
            }
            Parse::Credit {
                usage,
                limit,
                remaining,
                currency,
            } => Ok(BalanceValue::Credit {
                used: num(doc.pointer(usage)).ok_or(UNRECOGNIZED)?,
                limit: num(doc.pointer(limit)),
                remaining: num(doc.pointer(remaining)),
                currency: currency.clone(),
            }),
            Parse::Windows { windows } => {
                let now = time::to_secs(now);
                let mut out: Vec<QuotaWindow> = windows
                    .iter()
                    .flat_map(|w| w.read(doc))
                    .filter(|w| w.resets_at.is_none_or(|t| time::to_secs(t) > now))
                    .collect();
                if out.is_empty() {
                    return Err(UNRECOGNIZED.to_owned());
                }
                // 短的窗口在前（5 小时、周）。
                out.sort_by_key(|w| w.window_minutes.unwrap_or(u32::MAX));
                Ok(BalanceValue::Windows(out))
            }
        }
    }
}

impl WindowSpec {
    fn read(&self, doc: &Value) -> Vec<QuotaWindow> {
        let items: Vec<&Value> = match doc.pointer(&self.items) {
            Some(Value::Array(a)) => a.iter().collect(),
            Some(v @ Value::Object(_)) => vec![v],
            _ => return Vec::new(),
        };
        items.into_iter().filter_map(|i| self.one(i)).collect()
    }

    fn one(&self, item: &Value) -> Option<QuotaWindow> {
        let d = self
            .detail
            .as_ref()
            .and_then(|p| item.pointer(p))
            .unwrap_or(item);
        let get = |p: &Option<String>| p.as_ref().and_then(|p| num(d.pointer(p)));
        let limit = get(&self.limit).filter(|l| *l > 0.0)?;
        let used = get(&self.used).or_else(|| Some(limit - get(&self.remaining)?))?;
        let percent = used / limit * 100.0;
        let resets_at = self
            .resets_at
            .iter()
            .find_map(|p| d.pointer(p).and_then(reset_secs))
            .map(time::from_secs);
        let window_minutes = self.minutes(item);
        Some(QuotaWindow {
            label: self
                .label
                .clone()
                .unwrap_or_else(|| window_label(window_minutes)),
            used_percent: percent.clamp(0.0, 100.0) as f32,
            window_minutes,
            resets_at,
        })
    }

    fn minutes(&self, item: &Value) -> Option<u32> {
        let n = num(item.pointer(self.duration.as_ref()?))?;
        let unit = item.pointer(self.unit.as_ref()?)?.as_str()?;
        let per = self
            .units
            .iter()
            .find(|(k, _)| unit.contains(k.as_str()))
            .map(|(_, m)| *m)?;
        u32::try_from((n * f64::from(per)).round() as i64).ok()
    }
}

/// 重置时间：数字当秒；字符串当 RFC 3339（Kimi 带纳秒，`parse_rfc3339` 舍去小数）。
fn reset_secs(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => Some(n.as_f64()? as i64),
        Value::String(s) => time::parse_rfc3339(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_provider_has_a_rule_on_its_own_host() {
        let rules = BalanceRules::builtin();
        for p in Provider::ALL {
            let r = rules.provider(p);
            assert!(r.url.starts_with("https://"), "{p:?}");
            assert!(p.hosts().contains(&host_of(&r.url).unwrap()), "{p:?}");
        }
        assert!(Provider::KimiCode.unofficial());
        assert!(!Provider::DeepSeek.unofficial());
    }

    /// 旧版写的缓存里有这一版不认识的 provider（智谱 GLM 不再支持）：跳过那几条，别的照用，不整份作废。
    #[test]
    fn cache_skips_entries_this_version_does_not_know() {
        let dir = std::env::temp_dir().join(format!("huitai-balance-cache-{}", std::process::id()));
        let path = dir.join("balance.json");
        let mut cache = Cache::default();
        cache.entries.insert(
            "env:DEEPSEEK_API_KEY#1".to_owned(),
            CacheEntry {
                provider: Provider::DeepSeek,
                at: 7,
                result: Err("HTTP 500".to_owned()),
                backoff_until: None,
                backoff_step: 0,
            },
        );
        cache.save(&dir, &path).unwrap();
        let mut doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        doc["entries"]["env:GLM_API_KEY#2"] = json!({"provider": "glm_intl", "at": 8, "result": {"Ok": {"value": "no_plan", "at": {"secs_since_epoch": 8, "nanos_since_epoch": 0}}}});
        fs::write(&path, doc.to_string()).unwrap();
        let loaded = Cache::load(&path);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(loaded.entries.len(), 1, "{:?}", loaded.entries);
        assert_eq!(
            loaded.entries["env:DEEPSEEK_API_KEY#1"].provider,
            Provider::DeepSeek
        );
    }

    #[test]
    fn url_parts() {
        assert_eq!(
            host_of("https://api.kimi.com/coding/x"),
            Some("api.kimi.com")
        );
        assert_eq!(path_of("https://api.kimi.com/coding/x"), "/coding/x");
        assert_eq!(reset_secs(&json!(1778753640)), Some(1778753640));
        assert_eq!(
            reset_secs(&json!("2025-12-23T05:24:18.443553353Z")),
            time::parse_rfc3339("2025-12-23T05:24:18Z")
        );
    }
}
