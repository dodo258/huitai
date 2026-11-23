//! Claude 的两个官方渠道：状态栏缓存、用量接口；以及给会话台启动的 Claude 注入状态栏的 `--settings`。

use std::collections::HashMap;
use std::fs::{self, DirBuilder};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::rules::Channel;
use super::{QuotaReader, curl, time, windows_from};
use crate::core::exec;
use crate::core::{QuotaReading, Vendor};

/// 取用量接口原始响应体的方式。默认实现读钥匙串里的登录凭证再用 curl 请求；测试里换成假数据。
pub type UsageFetch = dyn Fn(&UsageRequest) -> Result<String, String> + Send + Sync;

/// 一次用量接口请求要用到的规则。
#[derive(Debug)]
pub struct UsageRequest<'a> {
    pub url: &'a str,
    pub headers: &'a [String],
    pub keychain_service: &'a str,
    pub token_pointer: &'a str,
    pub expires_at_ms_pointer: &'a str,
    pub timeout: Duration,
    /// 判断凭证过期用的当前时间（Unix 秒）。
    pub now_secs: i64,
}

/// 读注入的状态栏命令写下的原始 JSON。读数时间取缓存文件的修改时间。
pub(super) fn read_statusline(r: &QuotaReader, ch: &Channel) -> Result<QuotaReading, String> {
    let Channel::Statusline {
        cache_file,
        max_age_minutes,
        windows,
    } = ch
    else {
        unreachable!("只给状态栏渠道调用")
    };
    let path = r.data_dir.join(cache_file);
    let mtime = fs::metadata(&path)
        .and_then(|m| m.modified())
        .map_err(|_| "还没有缓存（会话台里启动的 Claude 回复一次后才有）".to_owned())?;
    let age = time::to_secs(r.now) - time::to_secs(mtime);
    if age > (*max_age_minutes as i64) * 60 {
        return Err(format!("缓存已经 {} 分钟没更新", age / 60));
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("缓存读不了: {e}"))?;
    let doc: Value = serde_json::from_str(&text).map_err(|e| format!("缓存不是 JSON: {e}"))?;
    let value = windows_from(&doc, windows, r.now)?;
    Ok(r.reading(ch, value, mtime))
}

/// 官方用量接口。成功的读数缓存几分钟，免得频繁联网。
pub(super) fn read_usage_api(r: &QuotaReader, ch: &Channel) -> Result<QuotaReading, String> {
    let Channel::UsageApi {
        url,
        headers,
        keychain_service,
        token,
        expires_at_ms,
        timeout_secs,
        cache_file,
        cache_minutes,
        windows,
    } = ch
    else {
        unreachable!("只给用量接口渠道调用")
    };
    // 非默认目录的账号有自己那份钥匙串条目，绝不拿默认那份（别的账号的）。
    let service = match r.dirs.override_of(Vendor::Claude, &r.home) {
        None => keychain_service.clone(),
        Some((_, dir)) => account_service(keychain_service, &dir)?,
    };
    let cache = r.data_dir.join(cache_file);
    if let Some(cached) = fs::read_to_string(&cache)
        .ok()
        .and_then(|t| serde_json::from_str::<QuotaReading>(&t).ok())
    {
        let age = time::to_secs(r.now) - time::to_secs(cached.at);
        if (0..(*cache_minutes as i64) * 60).contains(&age) {
            return Ok(cached);
        }
    }
    let body = (r.fetch)(&UsageRequest {
        url,
        headers,
        keychain_service: &service,
        token_pointer: token,
        expires_at_ms_pointer: expires_at_ms,
        timeout: Duration::from_secs(*timeout_secs),
        now_secs: time::to_secs(r.now),
    })?;
    let doc: Value = serde_json::from_str(&body).map_err(|e| format!("响应不是 JSON: {e}"))?;
    let reading = r.reading(ch, windows_from(&doc, windows, r.now)?, r.now);
    // 缓存写不进去只是下次多联网一次，不影响这次读数。
    if DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&r.data_dir)
        .is_ok()
        && let Ok(json) = serde_json::to_string(&reading)
    {
        let _ = fs::write(&cache, json);
    }
    Ok(reading)
}

/// 非默认目录（设了 `CLAUDE_CONFIG_DIR`）的账号在钥匙串里的条目名：默认条目名后面接
/// `-<sha256(目录原样字符串) 的前 8 位十六进制>`（Claude Code 2.1.330 包里的 `m8()`：
/// `` `Claude Code${OAUTH_FILE_SUFFIX}-credentials${CLAUDE_CONFIG_DIR ? "-" + sha256(dir).hex.slice(0,8) : ""}` ``）。
/// 会话台启动这个账号时 `CLAUDE_CONFIG_DIR` 传的就是 `dir` 这个写法。
pub(super) fn account_service(base: &str, dir: &Path) -> Result<String, String> {
    let hex = sha256_hex(&dir.to_string_lossy())?;
    Ok(format!("{base}-{}", &hex[..8]))
}

/// 系统自带的 `shasum` 算 SHA-256（只给钥匙串条目名用，一个账号 5 分钟最多一次）。
fn sha256_hex(s: &str) -> Result<String, String> {
    let out = exec::run("shasum", &["-a", "256"], Some(s), Duration::from_secs(3))
        .map_err(|e| e.to_string())?;
    let hex = out.split_whitespace().next().unwrap_or_default();
    if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(hex.to_ascii_lowercase())
    } else {
        Err("shasum 输出不认识".to_owned())
    }
}

/// 钥匙串读一次最多管多久：几个账号、每分钟一次的额度刷新都不会高频读钥匙串（高频读会把 securityd 弄崩，
/// claude-swap#496）。读不到也记着，5 分钟内不再问。
const KEYCHAIN_TTL: Duration = Duration::from_secs(5 * 60);

/// 钥匙串条目名 → 读的时间和 access token（只在内存里，不落盘）或读不到的原因。
type KeychainCache = HashMap<String, (Instant, Result<(String, Option<i64>), String>)>;

static KEYCHAIN: Mutex<Option<KeychainCache>> = Mutex::new(None);

/// 从钥匙串读这个条目里的 access token 和过期时间（毫秒），见 [`KEYCHAIN_TTL`]。只读，不刷新令牌。
fn access_token(req: &UsageRequest) -> Result<(String, Option<i64>), String> {
    cached(&KEYCHAIN, req.keychain_service, Instant::now(), || {
        read_keychain(req)
    })
}

/// `service` 这一项 [`KEYCHAIN_TTL`] 内读过就用上次的结果（成功失败都算），否则调 `read` 重读并记下。
fn cached(
    cache: &Mutex<Option<KeychainCache>>,
    service: &str,
    now: Instant,
    read: impl FnOnce() -> Result<(String, Option<i64>), String>,
) -> Result<(String, Option<i64>), String> {
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some((at, got)) = cache.get(service)
        && now.saturating_duration_since(*at) < KEYCHAIN_TTL
    {
        return got.clone();
    }
    let got = read();
    cache.insert(service.to_owned(), (now, got.clone()));
    got
}

fn read_keychain(req: &UsageRequest) -> Result<(String, Option<i64>), String> {
    let creds = exec::run(
        "security",
        &["find-generic-password", "-s", req.keychain_service, "-w"],
        None,
        Duration::from_secs(3),
    )
    .map_err(|e| format!("钥匙串里没取到 Claude 登录凭证: {e}"))?;
    let creds: Value =
        serde_json::from_str(creds.trim()).map_err(|_| "登录凭证格式不认识".to_owned())?;
    let token = creds
        .pointer(req.token_pointer)
        .and_then(Value::as_str)
        .ok_or("登录凭证里没有 access token")?;
    let expires_ms = creds
        .pointer(req.expires_at_ms_pointer)
        .and_then(Value::as_i64);
    Ok((token.to_owned(), expires_ms))
}

/// 真实的用量接口请求：钥匙串取 Claude 登录凭证（只在内存里用），通过标准输入把请求头交给 curl，
/// 不放进命令行参数，别的进程看不到。凭证过期就不请求（刷新凭证是 Claude 自己的事，会话台不碰）。
pub(super) fn fetch_usage(req: &UsageRequest) -> Result<String, String> {
    let (token, expires_ms) = access_token(req)?;
    if expires_ms.is_some_and(|ms| ms / 1000 <= req.now_secs) {
        return Err("登录凭证已过期，等 Claude 下次启动时自己刷新".to_owned());
    }
    let (argv, header_lines) = usage_command(req, &token);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    exec::run(
        argv[0],
        &argv[1..],
        Some(&header_lines),
        req.timeout + Duration::from_secs(2),
    )
    .map_err(|e| format!("用量接口请求失败: {e}"))
}

/// 用量接口的 curl 命令（程序在第一个）和经标准输入给的请求头：和余额渠道一样用系统自带的 curl、第一个参数 `-q`
/// （[`curl::argv`]），`~/.curlrc` 里的追踪选项不会把凭证写进文件；凭证只在标准输入里，不进命令行参数。
pub fn usage_command(req: &UsageRequest, token: &str) -> (Vec<String>, String) {
    let mut header_lines = format!("Authorization: Bearer {token}\n");
    for h in req.headers {
        header_lines.push_str(h);
        header_lines.push('\n');
    }
    let max_time = req.timeout.as_secs().max(1).to_string();
    let argv = curl::argv(&[
        "-sS",
        "--fail",
        "--max-time",
        &max_time,
        "-H",
        "@-",
        req.url,
    ]);
    (argv.into_iter().map(str::to_owned).collect(), header_lines)
}

/// 生成 `--settings` 的 JSON：状态栏命令先把标准输入（Claude 给状态栏的数据）存进缓存，
/// 再把同一份数据交给用户原有的状态栏命令，输出原样显示。原有设置里的 padding 等字段保留。
pub(super) fn settings_arg(r: &QuotaReader, cwd: &Path) -> String {
    let cache_file = r
        .rules
        .channels(Vendor::Claude)
        .iter()
        .find_map(|ch| match ch {
            Channel::Statusline { cache_file, .. } => Some(cache_file.as_str()),
            _ => None,
        })
        .expect("rules.json 里 Claude 有状态栏渠道（rules 单测保证）");
    let original = original_statusline(r.dirs.dir(Vendor::Claude), cwd);
    let original_cmd = original
        .as_ref()
        .and_then(|o| o.get("command"))
        .and_then(Value::as_str);
    let mut status = original.clone().unwrap_or_default();
    status.insert("type".to_owned(), json!("command"));
    status.insert(
        "command".to_owned(),
        json!(wrapper_command(&r.data_dir, cache_file, original_cmd)),
    );
    json!({ "statusLine": status }).to_string()
}

/// 不加 `--settings` 时 Claude 会用的状态栏设置：项目本地 > 项目 > 用户，取第一个有命令的。
fn original_statusline(claude_dir: &Path, cwd: &Path) -> Option<Map<String, Value>> {
    [
        cwd.join(".claude/settings.local.json"),
        cwd.join(".claude/settings.json"),
        claude_dir.join("settings.json"),
    ]
    .iter()
    .filter_map(|p| fs::read_to_string(p).ok())
    .filter_map(|t| serde_json::from_str::<Value>(&t).ok())
    .find_map(|v| {
        let s = v.get("statusLine")?.as_object()?;
        s.get("command")?.as_str()?;
        Some(s.clone())
    })
}

fn wrapper_command(data_dir: &Path, cache_file: &str, original: Option<&str>) -> String {
    let dir = shell_quote(&data_dir.to_string_lossy());
    let cache = shell_quote(cache_file);
    let save = format!("mv -f \"$t\" \"$d\"/{cache}");
    let body = match original {
        // 原命令放进子 shell，它自己 exit 也不影响后面存缓存；单独一行，防止它以注释结尾。
        Some(cmd) => format!("(\n{cmd}\n) < \"$t\"; {save}"),
        None => save,
    };
    format!(
        "umask 077; d={dir}; t=\"$d/.statusline.$$\"; if mkdir -p \"$d\" && cat > \"$t\"; then {body}; fi"
    )
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 条目名的算法：和 `printf %s <目录> | shasum -a 256` 前 8 位一致。
    #[test]
    fn account_service_appends_dir_hash() {
        // sha256("/Users/me/.claude-work") 的前 8 位。
        let s = account_service(
            "Claude Code-credentials",
            Path::new("/Users/me/.claude-work"),
        );
        assert_eq!(s.unwrap(), "Claude Code-credentials-1e91dd84");
    }

    /// 钥匙串 5 分钟内只读一次（读不到也算），各条目分开记。
    #[test]
    fn keychain_is_read_at_most_once_per_five_minutes() {
        let cache = Mutex::new(None);
        let reads = std::cell::Cell::new(0);
        let read = || {
            reads.set(reads.get() + 1);
            Err::<(String, Option<i64>), _>("没有这一项".to_owned())
        };
        let t0 = Instant::now();
        assert!(cached(&cache, "a", t0, read).is_err());
        assert!(cached(&cache, "a", t0 + Duration::from_secs(299), read).is_err());
        assert_eq!(reads.get(), 1);
        let _ = cached(&cache, "b", t0, read);
        assert_eq!(reads.get(), 2, "别的条目单独读");
        let _ = cached(&cache, "a", t0 + KEYCHAIN_TTL, read);
        assert_eq!(reads.get(), 3, "过了 5 分钟重读");
    }

    #[test]
    fn quotes_single_quotes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
