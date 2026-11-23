//! 余额渠道的密钥来源（只读）：pi 的 `auth.json`、`models.json`，Hermes 的 `.env`，环境变量。
//!
//! - 只读文件，不改、不执行：pi 里写成 `"!命令"` 的密钥记成「跳过」，不跑那条命令。
//! - 密钥包在 [`Secret`] 里：`Debug` 只打星号、不能序列化，日志、缓存、错误信息里带不出去。
//! - 同一把密钥从几处读到只留一份（[`FoundKey::origins`] 记着都在哪），只查一次。
//! - 配了自己的服务地址（pi `models.json` 的 `baseUrl`，`*_BASE_URL` 环境变量、Hermes `.env`）的密钥按地址认是哪家；
//!   地址的域名不在 [`Provider::hosts`] 里（自建网关、代理）就不查，记成「跳过」：这把密钥是网关的，发给原厂就是泄露。
//!   同一把密钥在别处没配地址也不查。
//! - 地址和密钥可以分在两处（pi 只在 `models.json` 给内置 provider 配 `baseUrl`、密钥放环境变量是官方写法；
//!   Hermes `.env` 只写 `KIMI_BASE_URL`、密钥在进程环境里）：读环境变量前先把各账号配置里的地址汇成一张表
//!   （[`file_bases`]），表里这个变量有一处地址不是原厂的，环境变量里的这把就当网关的，不查。

use std::fmt;
use std::fs;
use std::path::Path;

use serde_json::Value;

use super::balance::Provider;
use crate::core::{Account, Vendor};

/// 一把密钥。只在内存里，发请求时交给 curl 的标准输入；别处只能看到星号。
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub(super) fn expose(&self) -> &str {
        &self.0
    }

    /// 认这把密钥用的指纹：SHA-256 的前 16 位十六进制（64 位），缓存里用它分开不同的密钥。
    /// 截断的哈希反推不出密钥（API 密钥本身至少上百位随机），密钥本身不落盘。
    pub(super) fn fingerprint(&self) -> String {
        let digest = sha256(format!("huitai-balance-key:{}", self.0).as_bytes());
        digest[..8].iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// 系统的 CommonCrypto（libSystem 里就有，不用另外链接）算 SHA-256。
fn sha256(data: &[u8]) -> [u8; 32] {
    unsafe extern "C" {
        fn CC_SHA256(data: *const u8, len: u32, md: *mut u8) -> *mut u8;
    }
    let mut out = [0u8; 32];
    let len = u32::try_from(data.len()).expect("密钥不会超过 4GB");
    // SAFETY: `data` 在调用期间有效、长 `len` 字节；`out` 正好是 CC_SHA256 要写的 32 字节（CC_SHA256_DIGEST_LENGTH）。
    unsafe {
        CC_SHA256(data.as_ptr(), len, out.as_mut_ptr());
    }
    out
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// 密钥是从哪读到的（给界面和 `ai doctor` 看，不含密钥本身）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyOrigin {
    /// 哪个账号的配置里读到的；环境变量是 `None`。
    pub account: Option<OriginAccount>,
    /// 文件和位置，如 `auth.json · deepseek`、`.env · DEEPSEEK_API_KEY`、`环境变量 DEEPSEEK_API_KEY`。
    pub place: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginAccount {
    pub vendor: Vendor,
    /// 账号短名（[`Account::key`]）。
    pub key: String,
    /// 账号显示名。
    pub name: String,
}

impl KeyOrigin {
    /// 缓存里认这把密钥用的名字：账号 + 位置，不含密钥的任何部分。
    pub fn id(&self) -> String {
        match &self.account {
            Some(a) => format!("{}:{}|{}", a.vendor.key(), a.key, self.place),
            None => format!("env|{}", self.place),
        }
    }
}

/// 读到的一把密钥。
#[derive(Debug, Clone)]
pub struct FoundKey {
    /// 依次试的几家（同一公司的不同站点，例如 Moonshot 国内、国际）：认证不过才试下一个。
    pub providers: Vec<Provider>,
    pub origins: Vec<KeyOrigin>,
    pub(super) secret: Secret,
}

/// 没读的密钥：写成命令的（不执行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub origin: KeyOrigin,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct Found {
    pub keys: Vec<FoundKey>,
    pub skipped: Vec<Skipped>,
    /// 配了别家服务地址的密钥（[`Found::add_routed`]）：读完以后同一把密钥在别处读到的也不查。
    gateway: Vec<Secret>,
}

/// 配了自己的服务地址、域名不是能查的那几家时的说法（不写地址：地址里可能带账号密码）。
const GATEWAY: &str = "配了自己的服务地址（不是原厂的），密钥不发给原厂";

impl Found {
    /// 先看密钥实际发往哪：`base` 是配的服务地址（没配或空串是 `None`）。配了就按地址的域名认是哪家，
    /// 认不出（自建网关、代理）就不查；没配按名字认的 `named`。名字也认不出的（别的 provider）不记。
    fn add_routed(
        &mut self,
        named: Vec<Provider>,
        base: Option<&str>,
        origin: KeyOrigin,
        secret: String,
    ) {
        let Some(base) = base.map(str::trim).filter(|b| !b.is_empty()) else {
            return self.add(named, origin, secret);
        };
        match provider_of_url(base) {
            Some(p) => self.add(p, origin, secret),
            None if named.is_empty() => {}
            None => {
                let secret = Secret(secret.trim().to_owned());
                if !secret.0.is_empty() && !self.gateway.contains(&secret) {
                    self.gateway.push(secret);
                }
                self.skipped.push(Skipped {
                    origin,
                    reason: GATEWAY.to_owned(),
                });
            }
        }
    }

    /// 网关的密钥在别处读到的（同一把，没配地址）也拿掉，记成跳过。
    fn drop_gateway_keys(&mut self) {
        let gateway = std::mem::take(&mut self.gateway);
        let (bad, good) = std::mem::take(&mut self.keys)
            .into_iter()
            .partition(|k| gateway.contains(&k.secret));
        self.keys = good;
        for k in bad {
            self.skipped
                .extend(k.origins.into_iter().map(|origin| Skipped {
                    origin,
                    reason: GATEWAY.to_owned(),
                }));
        }
    }

    fn add(&mut self, providers: Vec<Provider>, origin: KeyOrigin, secret: String) {
        let secret = secret.trim().to_owned();
        if providers.is_empty() || secret.is_empty() {
            return;
        }
        match self.keys.iter_mut().find(|k| k.secret.0 == secret) {
            Some(k) => k.origins.push(origin),
            None => self.keys.push(FoundKey {
                providers,
                origins: vec![origin],
                secret: Secret(secret),
            }),
        }
    }
}

/// 按顺序读：各 pi 账号的 `auth.json`、`models.json`，各 Hermes 账号（含 profile）的 `.env`，最后环境变量。
pub fn find(accounts: &[Account], env: &dyn Fn(&str) -> Option<String>) -> Found {
    let mut found = Found::default();
    let table = file_bases(accounts);
    for a in accounts {
        match a.vendor {
            Vendor::Pi => {
                pi_auth(&mut found, a, env);
                pi_models(&mut found, a, env);
            }
            Vendor::Hermes => hermes_env(&mut found, a, env),
            _ => {}
        }
    }
    for name in ENV_KEYS {
        if let Some(v) = env(name).filter(|v| !v.trim().is_empty()) {
            let origin = KeyOrigin {
                account: None,
                place: format!("环境变量 {name}"),
            };
            // 先查表：配置文件里给这个变量配了别家地址（网关），就按那个地址走（记成跳过）。
            let gateway = table
                .iter()
                .find(|(var, base)| var == name && provider_of_url(base).is_none())
                .map(|(_, base)| base.clone());
            let base = gateway.or_else(|| base_url_vars(name).into_iter().find_map(|b| env(&b)));
            found.add_routed(env_providers(name, &v), base.as_deref(), origin, v);
        }
    }
    found.drop_gateway_keys();
    found
}

/// 这个 pi / Hermes 账号默认用哪家（pi `settings.json` 的 `defaultProvider`，Hermes `config.yaml` 的
/// `model.provider`）：一个名字可能对应同一公司的几家（Hermes 的 `kimi-coding`），读数是其中哪家都算。
/// 不是我们能查余额的那几家就是空的。
pub fn default_providers(account: &Account) -> Vec<Provider> {
    let named = match account.vendor {
        Vendor::Pi => pi_default(account),
        Vendor::Hermes => fs::read_to_string(account.dir.join("config.yaml"))
            .ok()
            .and_then(|text| hermes_provider_named(&yaml_model_provider(&text)?)),
        _ => None,
    };
    named.unwrap_or_default()
}

fn pi_default(account: &Account) -> Option<Vec<Provider>> {
    let settings = read_json(&account.dir.join("settings.json"))?;
    let name = settings.get("defaultProvider")?.as_str()?;
    // 配了 baseUrl（自定义的 provider，或内置名字改了地址）按地址认，没配按名字认。
    match pi_base_url(account, name) {
        Some(base) => provider_of_url(&base),
        None => provider_named(name),
    }
}

fn origin(a: &Account, place: String) -> KeyOrigin {
    KeyOrigin {
        account: Some(OriginAccount {
            vendor: a.vendor,
            key: a.key.clone(),
            name: a.name.clone(),
        }),
        place,
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// pi `models.json` 里这个 provider 配的 `baseUrl`（pi 允许内置的名字也改地址）；空串当没配。
fn pi_base_url(a: &Account, name: &str) -> Option<String> {
    read_json(&a.dir.join("models.json"))?
        .pointer(&format!("/providers/{}/baseUrl", escape_pointer(name)))?
        .as_str()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_owned)
}

/// 各账号配置文件里配的服务地址 → （密钥变量名，地址）：pi `models.json` 的 `providers.<名>.baseUrl`
/// （有没有 `apiKey` 都收：按 pi 内置 provider 默认读的环境变量，加上 `apiKey` 里 `$变量` 引用的变量对上），
/// Hermes `.env` 的 `*_BASE_URL`（按 [`base_url_vars`] 对上密钥变量）。空地址不收。
fn file_bases(accounts: &[Account]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for a in accounts {
        match a.vendor {
            Vendor::Pi => {
                let doc = read_json(&a.dir.join("models.json"));
                let Some(Value::Object(providers)) = doc.as_ref().and_then(|d| d.get("providers"))
                else {
                    continue;
                };
                for (name, p) in providers {
                    let Some(base) = p.get("baseUrl").and_then(Value::as_str).map(str::trim) else {
                        continue;
                    };
                    if base.is_empty() {
                        continue;
                    }
                    let mut vars: Vec<String> =
                        pi_env_key(name).map(str::to_owned).into_iter().collect();
                    if let Some(raw) = p.get("apiKey").and_then(Value::as_str) {
                        vars.extend(template_vars(raw));
                    }
                    out.extend(vars.into_iter().map(|v| (v, base.to_owned())));
                }
            }
            Vendor::Hermes => {
                let Ok(text) = fs::read_to_string(a.dir.join(".env")) else {
                    continue;
                };
                for (k, v) in parse_dotenv(&text) {
                    let v = v.trim();
                    if !k.ends_with("_BASE_URL") || v.is_empty() {
                        continue;
                    }
                    for name in ENV_KEYS.iter().filter(|n| base_url_vars(n).contains(&k)) {
                        out.push(((*name).to_owned(), v.to_owned()));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// pi 内置 provider 没写 `apiKey` 时读的环境变量（pi-ai `env-api-keys.js`），只列我们认的几家。
fn pi_env_key(name: &str) -> Option<&'static str> {
    Some(match name {
        "deepseek" => "DEEPSEEK_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "moonshotai" | "moonshotai-cn" => "MOONSHOT_API_KEY",
        "kimi-coding" => "KIMI_API_KEY",
        _ => return None,
    })
}

/// pi `auth.json`：`{"<provider>": {"type": "api_key", "key": …} | {"type": "oauth", "access": …}}`。
fn pi_auth(found: &mut Found, a: &Account, env: &dyn Fn(&str) -> Option<String>) {
    let Some(Value::Object(doc)) = read_json(&a.dir.join("auth.json")) else {
        return;
    };
    for (name, entry) in doc {
        let Some(providers) = provider_named(&name) else {
            continue;
        };
        let at = origin(a, format!("auth.json · {name}"));
        let base = pi_base_url(a, &name);
        match entry.get("type").and_then(Value::as_str) {
            Some("oauth") => {
                if let Some(t) = entry.get("access").and_then(Value::as_str) {
                    found.add_routed(providers, base.as_deref(), at, t.to_owned());
                }
            }
            _ => {
                if let Some(raw) = entry.get("key").and_then(Value::as_str) {
                    pi_value(found, providers, base.as_deref(), at, raw, env);
                }
            }
        }
    }
}

/// pi `models.json`：`providers.<名>.apiKey`；配了 `baseUrl` 按它的域名认，没配按名字认。
fn pi_models(found: &mut Found, a: &Account, env: &dyn Fn(&str) -> Option<String>) {
    let Some(doc) = read_json(&a.dir.join("models.json")) else {
        return;
    };
    let Some(Value::Object(providers)) = doc.get("providers") else {
        return;
    };
    for (name, p) in providers {
        let Some(raw) = p.get("apiKey").and_then(Value::as_str) else {
            continue;
        };
        pi_value(
            found,
            provider_named(name).unwrap_or_default(),
            p.get("baseUrl").and_then(Value::as_str),
            origin(a, format!("models.json · {name}")),
            raw,
            env,
        );
    }
}

/// pi 的配置值：`!命令` 跳过（不执行）；`$变量`、`${变量}` 换成环境变量，`$$`、`$!` 是字面的 `$`、`!`；
/// 引用的变量没设就当没有这把密钥（和 pi 一样）。
fn pi_value(
    found: &mut Found,
    providers: Vec<Provider>,
    base: Option<&str>,
    at: KeyOrigin,
    raw: &str,
    env: &dyn Fn(&str) -> Option<String>,
) {
    if raw.starts_with('!') {
        if providers.is_empty() && base.and_then(provider_of_url).is_none() {
            return;
        }
        found.skipped.push(Skipped {
            origin: at,
            reason: "密钥写成了命令，会话台不执行".to_owned(),
        });
        return;
    }
    if let Some(v) = resolve_template(raw, env) {
        found.add_routed(providers, base, at, v);
    }
}

fn resolve_template(raw: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(c @ ('$' | '!')) = after.chars().next() {
            out.push(c);
            rest = &after[1..];
            continue;
        }
        let (name, next) = match after.strip_prefix('{') {
            Some(inner) => match inner.find('}') {
                Some(end) if is_var_name(&inner[..end]) => (&inner[..end], &inner[end + 1..]),
                _ => {
                    out.push('$');
                    rest = after;
                    continue;
                }
            },
            None => {
                let len = var_prefix_len(after);
                if len == 0 {
                    out.push('$');
                    rest = after;
                    continue;
                }
                (&after[..len], &after[len..])
            }
        };
        out.push_str(&env(name).filter(|v| !v.is_empty())?);
        rest = next;
    }
    out.push_str(rest);
    Some(out)
}

/// pi 配置值里 `$变量`、`${变量}` 引用的变量名（`!命令` 不算）。
fn template_vars(raw: &str) -> Vec<String> {
    if raw.starts_with('!') {
        return Vec::new();
    }
    let names = std::cell::RefCell::new(Vec::new());
    resolve_template(raw, &|n| {
        names.borrow_mut().push(n.to_owned());
        Some("x".to_owned())
    });
    names.into_inner()
}

fn var_prefix_len(s: &str) -> usize {
    let b = s.as_bytes();
    if b.first()
        .is_none_or(|c| !(c.is_ascii_alphabetic() || *c == b'_'))
    {
        return 0;
    }
    b.iter()
        .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
        .count()
}

fn is_var_name(s: &str) -> bool {
    !s.is_empty() && var_prefix_len(s) == s.len()
}

/// Hermes 的 `.env`：`KEY=VALUE` 一行一个，`#` 开头是注释，可以带 `export `、引号。
/// 地址变量（`KIMI_BASE_URL` 等）在 `.env` 或会话台自己的环境变量里配了都算（Hermes 两处都读）。
fn hermes_env(found: &mut Found, a: &Account, env: &dyn Fn(&str) -> Option<String>) {
    let Ok(text) = fs::read_to_string(a.dir.join(".env")) else {
        return;
    };
    let vars = parse_dotenv(&text);
    let get = |n: &str| {
        let in_file = vars.iter().rev().find(|(k, _)| k == n).map(|(_, v)| v);
        in_file
            .filter(|v| !v.trim().is_empty())
            .cloned()
            .or_else(|| env(n))
            .filter(|v| !v.trim().is_empty())
    };
    for (name, value) in &vars {
        if ENV_KEYS.contains(&name.as_str()) {
            let base = base_url_vars(name).iter().find_map(|b| get(b));
            let providers = env_providers(name, value);
            let at = origin(a, format!(".env · {name}"));
            found.add_routed(providers, base.as_deref(), at, value.clone());
        }
    }
}

fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("export ").unwrap_or(line);
            if line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            let v = v.trim();
            let v = match (v.chars().next(), v.len()) {
                (Some(q @ ('"' | '\'')), n) if n >= 2 && v.ends_with(q) => &v[1..n - 1],
                // 不带引号的值后面可以跟 ` # 注释`。
                _ => v.split(" #").next().unwrap_or(v).trim(),
            };
            Some((k.trim().to_owned(), v.to_owned()))
        })
        .collect()
}

/// Hermes `config.yaml` 里 `model:` 下面的 `provider:`（只认这一种两层写法，不引 YAML 库）。
fn yaml_model_provider(text: &str) -> Option<String> {
    let mut in_model = false;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if !line.starts_with([' ', '\t']) {
            in_model = line.trim_end() == "model:";
            continue;
        }
        if in_model && let Some(v) = line.trim().strip_prefix("provider:") {
            let v = v.split(" #").next().unwrap_or(v).trim();
            return Some(v.trim_matches(['"', '\'']).to_owned()).filter(|v| !v.is_empty());
        }
    }
    None
}

/// 认的环境变量（pi 和 Hermes 用的名字）。
const ENV_KEYS: [&str; 6] = [
    "DEEPSEEK_API_KEY",
    "OPENROUTER_API_KEY",
    "MOONSHOT_API_KEY",
    "KIMI_CN_API_KEY",
    "KIMI_API_KEY",
    "KIMI_CODING_API_KEY",
];

/// 这个密钥变量对应的服务地址变量：Hermes 的 provider 表（`hermes_cli/auth.py`：Kimi、DeepSeek 可以
/// 改地址），再加上同名前缀的 `<前缀>_BASE_URL`（宁可多认几个不查，也不把网关的密钥发给原厂）。
fn base_url_vars(name: &str) -> Vec<String> {
    let mut out: Vec<String> = match name {
        "KIMI_API_KEY" | "KIMI_CODING_API_KEY" => vec!["KIMI_BASE_URL".to_owned()],
        _ => Vec::new(),
    };
    if let Some(prefix) = name.strip_suffix("_API_KEY") {
        let own = format!("{prefix}_BASE_URL");
        if !out.contains(&own) {
            out.push(own);
        }
    }
    out
}

/// 环境变量名 → 依次试哪几家。
fn env_providers(name: &str, value: &str) -> Vec<Provider> {
    use Provider::*;
    match name {
        "DEEPSEEK_API_KEY" => vec![DeepSeek],
        "OPENROUTER_API_KEY" => vec![OpenRouter],
        // pi 的 moonshotai（国际）、moonshotai-cn（国内）共用这个变量：先国内，认证不过再国际。
        "MOONSHOT_API_KEY" => vec![MoonshotCn, MoonshotIntl],
        "KIMI_CN_API_KEY" => vec![MoonshotCn],
        // pi 的 kimi-coding 用它；Hermes 里它也可能是开放平台的 key（Kimi Code 的 key 以 sk-kimi- 开头）。
        "KIMI_API_KEY" if value.trim().starts_with("sk-kimi-") => vec![KimiCode],
        "KIMI_API_KEY" => vec![KimiCode, MoonshotIntl, MoonshotCn],
        "KIMI_CODING_API_KEY" => vec![KimiCode],
        _ => Vec::new(),
    }
}

/// pi / Hermes 里的 provider 名 → 依次试哪几家。
fn provider_named(name: &str) -> Option<Vec<Provider>> {
    use Provider::*;
    Some(match name {
        "deepseek" => vec![DeepSeek],
        "openrouter" => vec![OpenRouter],
        "moonshotai" | "moonshot" => vec![MoonshotIntl],
        "moonshotai-cn" | "moonshot-cn" | "kimi-cn" => vec![MoonshotCn],
        "kimi-coding" | "kimi-code" => vec![KimiCode],
        _ => return None,
    })
}

/// Hermes 的 provider 名（含别名，`hermes_cli/auth.py` 的 provider 表和别名表）→ 依次试哪几家。
/// `kimi-coding` 的端点随密钥变（`sk-kimi-` 走 Kimi Code，别的走 Moonshot 开放平台），和
/// [`env_providers`] 里 `KIMI_API_KEY` 的几家一致；`kimi-coding-cn` 是 Moonshot 国内。
fn hermes_provider_named(name: &str) -> Option<Vec<Provider>> {
    use Provider::*;
    Some(match name {
        "kimi-coding" | "kimi" | "kimi-for-coding" | "moonshot" => {
            vec![KimiCode, MoonshotIntl, MoonshotCn]
        }
        "kimi-coding-cn" | "kimi-cn" | "moonshot-cn" => vec![MoonshotCn],
        _ => return provider_named(name),
    })
}

/// 按 API 地址的域名认是哪家。
fn provider_of_url(url: &str) -> Option<Vec<Provider>> {
    let host = url.split("://").nth(1)?.split(['/', ':']).next()?;
    Provider::ALL
        .into_iter()
        .find(|p| p.hosts().contains(&host))
        .map(|p| vec![p])
}

fn escape_pointer(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |n| {
            pairs
                .iter()
                .find(|(k, _)| *k == n)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn template_resolves_vars_and_escapes() {
        let env = env_of(&[("A", "x"), ("B_2", "y")]);
        assert_eq!(resolve_template("$A-${B_2}", &env).as_deref(), Some("x-y"));
        assert_eq!(resolve_template("p$$q$!", &env).as_deref(), Some("p$q!"));
        assert_eq!(resolve_template("plain", &env).as_deref(), Some("plain"));
        assert_eq!(resolve_template("$-", &env).as_deref(), Some("$-"));
        assert_eq!(
            resolve_template("$MISSING", &env),
            None,
            "变量没设就没有密钥"
        );
        assert_eq!(template_vars("a$A-${B_2}$$C"), ["A", "B_2"]);
        assert!(template_vars("!echo $A").is_empty());
    }

    #[test]
    fn dotenv_and_yaml() {
        let env = parse_dotenv("# c\nexport A=\"q v\"\nB='s'\nC=raw # note\n\nD\n");
        assert_eq!(
            env,
            [("A", "q v"), ("B", "s"), ("C", "raw")].map(|(k, v)| (k.to_owned(), v.to_owned()))
        );
        let yaml = "agent:\n  provider: nope\nmodel:\n  default: x\n  provider: \"zai\"  # c\n";
        assert_eq!(yaml_model_provider(yaml).as_deref(), Some("zai"));
        assert_eq!(yaml_model_provider("model: x\n"), None);
    }

    #[test]
    fn providers_by_name_url_and_env() {
        assert_eq!(provider_named("deepseek"), Some(vec![Provider::DeepSeek]));
        assert_eq!(provider_named("magpie"), None);
        assert_eq!(
            provider_of_url("https://api.kimi.com/coding/v1"),
            Some(vec![Provider::KimiCode])
        );
        // 智谱 GLM 不再查：密钥变量、provider 名、地址都不认。
        assert_eq!(provider_named("zai"), None);
        assert_eq!(provider_named("zhipu"), None);
        assert!(env_providers("GLM_API_KEY", "x").is_empty());
        assert!(env_providers("ZHIPU_API_KEY", "x").is_empty());
        assert_eq!(provider_of_url("https://api.z.ai/api/coding/paas/v4"), None);
        assert_eq!(provider_of_url("http://127.0.0.1:3425/v1"), None);
        assert_eq!(
            env_providers("KIMI_API_KEY", "sk-kimi-x"),
            [Provider::KimiCode]
        );
        assert_eq!(env_providers("KIMI_API_KEY", "sk-x")[0], Provider::KimiCode);
    }

    #[test]
    fn secret_debug_hides_value() {
        let s = Secret("sk-very-secret".to_owned());
        assert_eq!(format!("{s:?}"), "Secret(***)");
        let mut f = Found::default();
        let o = KeyOrigin {
            account: None,
            place: "x".to_owned(),
        };
        f.add(
            vec![Provider::DeepSeek],
            o.clone(),
            "sk-very-secret".to_owned(),
        );
        f.add(vec![Provider::DeepSeek], o, " sk-very-secret ".to_owned());
        assert_eq!(f.keys.len(), 1, "同一把密钥只留一份");
        assert_eq!(f.keys[0].origins.len(), 2);
        assert!(!format!("{f:?}").contains("very-secret"));
    }
}
