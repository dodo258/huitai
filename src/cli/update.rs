//! 版本提醒和 `ai update`：查 GitHub Releases 最新版、认安装来源、下载校验后原子替换自己。
//!
//! 联网只用系统自带的 `curl`（`-q` 不读 `~/.curlrc`，不带任何用户信息），超时由 curl 的 `--max-time` 管。
//! 发布信息和版本比较在 `core::release`；没有仓库（`release::REPO` 为 `None`）时这里什么都不做。

use std::fs;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, ErrorKind};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::core::{exec, release};

/// 记上次查的时间和查到的最新版本，在数据目录里。
const CACHE_FILE: &str = "update-check.json";
/// `ai update` 查最新版的超时（用户自己敲的，比顶栏提示宽松）。
const UPDATE_QUERY_TIMEOUT: Duration = Duration::from_secs(15);
/// 下载新版程序的超时。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// 校验和文件很小。
const SUM_TIMEOUT: Duration = Duration::from_secs(30);
/// 算 sha256 的超时（`shasum` 读本地文件）。
const SHASUM_TIMEOUT: Duration = Duration::from_secs(30);

/// 会话台是怎么装上的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// 程序文件在 Homebrew 前缀的 Cellar 里；带上这个前缀（找它的 `brew`）。
    Homebrew(PathBuf),
    /// 在 `$CARGO_HOME/bin` 里。
    Cargo,
    /// 从源码构建的开发版（在 `target/debug` 或 `target/release` 里）。
    Dev,
    /// 直接下载放在某个目录里。
    Direct,
}

/// 按（已解开链接的）程序路径认安装来源。
pub fn detect_source(exe: &Path, brew_prefixes: &[PathBuf], cargo_home: Option<&Path>) -> Source {
    if let Some(prefix) = brew_prefixes
        .iter()
        .find(|p| exe.starts_with(p.join("Cellar")))
    {
        return Source::Homebrew(prefix.clone());
    }
    if cargo_home.is_some_and(|h| exe.parent() == Some(&h.join("bin"))) {
        return Source::Cargo;
    }
    let parts: Vec<_> = exe.components().map(|c| c.as_os_str()).collect();
    if parts
        .windows(2)
        .any(|w| w[0] == "target" && (w[1] == "debug" || w[1] == "release"))
    {
        return Source::Dev;
    }
    Source::Direct
}

/// 可能的 Homebrew 前缀：`$HOMEBREW_PREFIX`、Apple 芯片的 `/opt/homebrew`、Intel 的 `/usr/local`。
fn brew_prefixes() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::env::var_os("HOMEBREW_PREFIX")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .collect();
    out.extend(["/opt/homebrew", "/usr/local"].map(PathBuf::from));
    out.into_iter()
        .map(|p| fs::canonicalize(&p).unwrap_or(p))
        .collect()
}

fn cargo_home() -> Option<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".cargo"))
        })?;
    Some(fs::canonicalize(&home).unwrap_or(home))
}

fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|e| format!("找不到自己的路径: {e}"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ---- 顶栏提示 ----

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Cache {
    /// 上次查的时间（Unix 秒），查失败也记，保证一天最多查一次。
    checked_at: Option<u64>,
    /// 上次查到的最新版本 tag。
    latest: Option<String>,
}

/// 有新版就返回它的版本号（给顶栏用）。离上次查不满 24 小时就用上次查到的，不联网；
/// 到了才调 `fetch` 查一次，失败不提示、沿用上次的结果。
pub fn check(
    data_dir: &Path,
    now: u64,
    current: &str,
    fetch: impl FnOnce() -> Result<String, String>,
) -> Option<String> {
    let path = data_dir.join(CACHE_FILE);
    let mut cache: Cache = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if release::check_due(cache.checked_at, now) {
        if let Ok(tag) = fetch() {
            cache.latest = Some(tag);
        }
        cache.checked_at = Some(now);
        // 写不进去只是下次再查一次，不打扰。
        let _ = fs::create_dir_all(data_dir)
            .and_then(|()| fs::write(&path, serde_json::to_vec(&cache).unwrap_or_default()));
    }
    let latest = cache.latest?;
    release::is_newer(&latest, current).then(|| release::display_version(&latest).to_owned())
}

/// 会话台启动时在后台调：开关都开着、仓库定了才查。
pub fn board_check(data_dir: &Path) -> Option<String> {
    let off = std::env::var(release::NO_CHECK_ENV).ok();
    if release::disabled_by_env(off.as_deref()) {
        return None;
    }
    let repo = release::REPO?;
    let url = release::latest_url(release::API_BASE, repo);
    check(data_dir, now_secs(), env!("CARGO_PKG_VERSION"), || {
        fetch_release(&url, release::CHECK_TIMEOUT).map(|r| r.tag_name)
    })
}

// ---- 联网 ----

/// GitHub「最新发布」接口里用到的字段。
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

/// 用 curl 发一个 GET；`out` 给了就写进文件，否则返回内容。HTTP 错误（4xx/5xx）算失败。
fn curl(url: &str, timeout: Duration, out: Option<&Path>) -> Result<String, String> {
    let max = timeout.as_secs().max(1).to_string();
    // -q 必须第一个：不读 ~/.curlrc，免得带上别的头。
    let mut args = vec![
        "-q",
        "-fsSL",
        "--proto",
        "=https,http",
        "--max-time",
        &max,
        "--noproxy",
        "127.0.0.1,localhost",
        "-H",
        "Accept: application/vnd.github+json",
    ];
    let out = out.map(|p| p.to_string_lossy().into_owned());
    if let Some(p) = &out {
        args.extend(["-o", p]);
    }
    args.push(url);
    // curl 自己按 --max-time 停；外面多留几秒兜底。
    exec::run(
        Path::new("/usr/bin/curl"),
        &args,
        None,
        timeout + Duration::from_secs(5),
    )
    .map_err(|e| format!("下载 {url} 失败：{e}"))
}

pub fn fetch_release(url: &str, timeout: Duration) -> Result<Release, String> {
    let body = curl(url, timeout, None)?;
    serde_json::from_str(&body).map_err(|e| format!("发布信息看不懂: {e}"))
}

// ---- ai update ----

/// `ai update`：按安装来源升级，返回给用户看的结果。
pub fn run() -> Result<String, String> {
    let exe = current_exe()?;
    match detect_source(&exe, &brew_prefixes(), cargo_home().as_deref()) {
        Source::Homebrew(prefix) => brew_upgrade(&prefix),
        Source::Cargo => Ok(match release::REPO {
            Some(repo) => format!(
                "会话台是用 cargo install 装的，用这条命令升级：\n  {}",
                release::cargo_command(repo)
            ),
            None => no_release(),
        }),
        Source::Dev => Ok(format!(
            "{} 是从源码构建的开发版：在源码目录 git pull 后重新 cargo build。",
            exe.display()
        )),
        Source::Direct => match release::REPO {
            Some(repo) => update_direct(
                &release::latest_url(release::API_BASE, repo),
                env!("CARGO_PKG_VERSION"),
                &exe,
                std::env::consts::ARCH,
            ),
            None => Ok(no_release()),
        },
    }
}

fn no_release() -> String {
    "还没有发布渠道，暂时没有新版本可装。".to_owned()
}

/// Homebrew 装的：交给 `brew upgrade`，输出直接给用户看。
fn brew_upgrade(prefix: &Path) -> Result<String, String> {
    let brew = prefix.join("bin/brew");
    let status = Command::new(&brew)
        .args(["upgrade", release::BREW_FORMULA])
        .status()
        .map_err(|e| format!("{} 没跑起来: {e}", brew.display()))?;
    if status.success() {
        Ok(after_replace())
    } else {
        Err(format!(
            "brew upgrade {} 没成功（{status}）",
            release::BREW_FORMULA
        ))
    }
}

fn after_replace() -> String {
    "在跑的对话不受影响：后台守护进程等手上的对话都结束后自动换成新版。".to_owned()
}

/// 直接下载装的：查最新版，比当前新就下载本机的包、校验后替换 `exe`。
pub fn update_direct(
    api_url: &str,
    current: &str,
    exe: &Path,
    arch: &str,
) -> Result<String, String> {
    let latest = fetch_release(api_url, UPDATE_QUERY_TIMEOUT)?;
    let version = release::display_version(&latest.tag_name).to_owned();
    if !release::is_newer(&latest.tag_name, current) {
        return Ok(format!("已经是最新版 {current}。"));
    }
    install(&latest, exe, arch)?;
    Ok(format!("已更新到 {version}。{}", after_replace()))
}

/// 下载 `release` 里本机的包，sha256 对得上才换掉 `exe`；任何一步失败旧版都不动。
pub fn install(release: &Release, exe: &Path, arch: &str) -> Result<(), String> {
    let name = release::asset_name(arch);
    let sum_name = format!("{name}.sha256");
    let find = |n: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == n)
            .ok_or_else(|| format!("{} 没有本机（{arch}）的包 {n}", release.tag_name))
    };
    let (asset, sum_asset) = (find(&name)?, find(&sum_name)?);
    let sum = curl(&sum_asset.browser_download_url, SUM_TIMEOUT, None)?;
    let expected = parse_sum(&sum).ok_or_else(|| format!("{sum_name} 里没有 sha256"))?;

    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} 没有上级目录", exe.display()))?;
    // 暂存目录建在同一个目录里，rename 才是原子的；只有自己能进，别人没法预先放链接或掉包。
    let stage = StageDir::create(dir).map_err(|e| replace_error(dir, e))?;
    let temp = stage.0.join("ai");
    curl(&asset.browser_download_url, DOWNLOAD_TIMEOUT, Some(&temp))?;
    let actual = sha256(&temp)?;
    if actual != expected {
        return Err(format!(
            "下载的包校验不对（应为 {expected}，实际 {actual}），没有替换，旧版照常用"
        ));
    }
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))
        .map_err(|e| replace_error(&temp, e))?;
    fs::rename(&temp, exe).map_err(|e| replace_error(exe, e))?;
    Ok(())
}

fn replace_error(path: &Path, e: io::Error) -> String {
    let hint = if e.kind() == ErrorKind::PermissionDenied {
        "（没有写权限，可以用 sudo ai update）"
    } else {
        ""
    };
    format!("换不了 {}：{e}{hint}，旧版照常用", path.display())
}

/// 校验和文件的第一段：64 位十六进制（`shasum -a 256` 的输出格式）。
fn parse_sum(text: &str) -> Option<String> {
    let first = text.split_whitespace().next()?.to_ascii_lowercase();
    (first.len() == 64 && first.chars().all(|c| c.is_ascii_hexdigit())).then_some(first)
}

/// 用系统的 `shasum` 算文件的 sha256。
fn sha256(path: &Path) -> Result<String, String> {
    let file = path.to_string_lossy();
    let out = exec::run(
        Path::new("/usr/bin/shasum"),
        &["-a", "256", &file],
        None,
        SHASUM_TIMEOUT,
    )
    .map_err(|e| format!("算校验和失败：{e}"))?;
    parse_sum(&out).ok_or_else(|| "shasum 输出看不懂".to_owned())
}

/// 下载用的暂存目录：`<安装目录>/.ai-update-<随机数>`，0700，自己新建的（同名已存在就换个名字，绝不沿用）。
/// 用完连同里面没换上去的文件一起删掉。
struct StageDir(PathBuf);

impl StageDir {
    fn create(parent: &Path) -> io::Result<Self> {
        for _ in 0..16 {
            let tag = RandomState::new().hash_one((std::process::id(), SystemTime::now()));
            let path = parent.join(format!(".ai-update-{tag:016x}"));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            ErrorKind::AlreadyExists,
            "建不了暂存目录（名字一直被占）",
        ))
    }
}

impl Drop for StageDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const ARCH: &str = "aarch64";

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("huitai-update-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(&dir).unwrap()
    }

    /// 本地假服务器：按路径回固定内容，没有的回 404；记下收到的请求头。
    struct FakeServer {
        base: String,
        heads: Arc<Mutex<Vec<String>>>,
    }

    /// 先绑端口拿到地址，再用地址生成路由（发布信息里的下载地址要指回自己）。
    fn serve(routes: impl FnOnce(&str) -> HashMap<String, Vec<u8>>) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes = routes(&base);
        let heads = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&heads);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                seen.lock().unwrap().push(head);
                let (status, body) = match routes.get(&path) {
                    Some(b) => ("200 OK", b.clone()),
                    None => ("404 Not Found", b"nope".to_vec()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        FakeServer { base, heads }
    }

    fn sha_hex(data: &[u8], dir: &Path) -> String {
        let f = dir.join("sum-input");
        fs::write(&f, data).unwrap();
        let hex = sha256(&f).unwrap();
        fs::remove_file(&f).unwrap();
        hex
    }

    /// 发布 `tag`、包内容 `package`、校验和文件写 `sum` 的假服务器；`with_package` 为假时不放包。
    fn fake_release(tag: &str, package: &[u8], sum: &str, with_package: bool) -> FakeServer {
        let name = release::asset_name(ARCH);
        let (tag, package, sum) = (tag.to_owned(), package.to_vec(), sum.to_owned());
        serve(move |base| {
            let mut assets = vec![serde_json::json!({
                "name": format!("{name}.sha256"),
                "browser_download_url": format!("{base}/dl/{name}.sha256"),
            })];
            if with_package {
                assets.push(serde_json::json!({
                    "name": name,
                    "browser_download_url": format!("{base}/dl/{name}"),
                }));
            }
            let json = serde_json::json!({ "tag_name": tag, "assets": assets });
            HashMap::from([
                ("/latest".to_owned(), json.to_string().into_bytes()),
                (format!("/dl/{name}"), package),
                (
                    format!("/dl/{name}.sha256"),
                    format!("{sum}  {name}\n").into_bytes(),
                ),
            ])
        })
    }

    fn installed(dir: &Path) -> PathBuf {
        let exe = dir.join("ai");
        fs::write(&exe, b"old build").unwrap();
        exe
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with(".ai-update"))
            .collect()
    }

    #[test]
    fn verified_package_replaces_exe() {
        let dir = temp_dir("good");
        let exe = installed(&dir);
        let sum = sha_hex(b"new build", &dir);
        let srv = fake_release("v9.0.0", b"new build", &sum, true);
        let msg = update_direct(&format!("{}/latest", srv.base), "0.1.0", &exe, ARCH).unwrap();
        assert!(msg.contains("9.0.0"), "{msg}");
        assert_eq!(fs::read(&exe).unwrap(), b"new build");
        let mode = fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(leftovers(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// 别人在安装目录里预先放了指向别处的链接（按旧做法可猜的 `.ai-update-<pid>`）：
    /// 下载不能顺着链接写进别的文件，换上去的也得是自己校验过的那个普通文件。
    #[test]
    fn planted_link_in_install_dir_is_not_followed() {
        let dir = temp_dir("planted");
        let exe = installed(&dir);
        let victim = dir.join("victim");
        fs::write(&victim, b"keep me").unwrap();
        let planted = dir.join(format!(".ai-update-{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        let sum = sha_hex(b"new build", &dir);
        let srv = fake_release("v9.0.0", b"new build", &sum, true);
        update_direct(&format!("{}/latest", srv.base), "0.1.0", &exe, ARCH).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep me");
        assert!(fs::symlink_metadata(&exe).unwrap().file_type().is_file());
        assert_eq!(fs::read(&exe).unwrap(), b"new build");
        assert_eq!(
            leftovers(&dir),
            vec![planted.file_name().unwrap().to_string_lossy().into_owned()],
            "只剩别人放的链接，自己的暂存目录删掉"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checksum_mismatch_keeps_old_exe() {
        let dir = temp_dir("badsum");
        let exe = installed(&dir);
        let wrong = sha_hex(b"something else", &dir);
        let srv = fake_release("v9.0.0", b"tampered build", &wrong, true);
        let err = update_direct(&format!("{}/latest", srv.base), "0.1.0", &exe, ARCH).unwrap_err();
        assert!(err.contains("校验不对"), "{err}");
        assert_eq!(fs::read(&exe).unwrap(), b"old build");
        assert!(leftovers(&dir).is_empty(), "临时文件要删掉");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn broken_sum_file_or_missing_package_keeps_old_exe() {
        let dir = temp_dir("broken");
        let exe = installed(&dir);
        let srv = fake_release("v9.0.0", b"new build", "not-a-hash", true);
        let api = format!("{}/latest", srv.base);
        assert!(update_direct(&api, "0.1.0", &exe, ARCH).is_err());
        let sum = sha_hex(b"new build", &dir);
        let srv = fake_release("v9.0.0", b"new build", &sum, false);
        let err = update_direct(&format!("{}/latest", srv.base), "0.1.0", &exe, ARCH).unwrap_err();
        assert!(err.contains("没有本机"), "{err}");
        let srv = fake_release("v9.0.0", b"new build", &sum, true);
        let err = update_direct(&format!("{}/latest", srv.base), "0.1.0", &exe, "x86_64");
        assert!(err.is_err(), "别的架构的包不能装");
        assert_eq!(fs::read(&exe).unwrap(), b"old build");
        assert!(leftovers(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn server_down_keeps_old_exe() {
        let dir = temp_dir("down");
        let exe = installed(&dir);
        // 绑了再放掉：这个端口上没人听。
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let api = format!("http://127.0.0.1:{port}/latest");
        assert!(update_direct(&api, "0.1.0", &exe, ARCH).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"old build");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn same_version_does_not_download() {
        let dir = temp_dir("same");
        let exe = installed(&dir);
        let srv = fake_release("v0.1.0", b"new build", "x", true);
        let msg = update_direct(&format!("{}/latest", srv.base), "0.1.0", &exe, ARCH).unwrap();
        assert!(msg.contains("最新"), "{msg}");
        assert_eq!(fs::read(&exe).unwrap(), b"old build");
        assert_eq!(srv.heads.lock().unwrap().len(), 1, "只问了一次发布信息");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn query_is_a_plain_get_without_user_info() {
        let srv = fake_release("v9.0.0", b"", "x", true);
        let r = fetch_release(&format!("{}/latest", srv.base), Duration::from_secs(3)).unwrap();
        assert_eq!(r.tag_name, "v9.0.0");
        let heads = srv.heads.lock().unwrap();
        let head = heads[0].to_ascii_lowercase();
        assert!(head.starts_with("get /latest "), "{head}");
        for h in ["cookie", "authorization"] {
            assert!(!head.contains(h), "不该带 {h}：{head}");
        }
    }

    #[test]
    fn check_queries_at_most_once_a_day() {
        let dir = temp_dir("throttle");
        let calls = AtomicUsize::new(0);
        let fetch = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok("v0.2.0".to_owned())
        };
        let t0 = 1_000_000;
        assert_eq!(check(&dir, t0, "0.1.0", fetch), Some("0.2.0".to_owned()));
        // 一天之内不再联网，沿用上次查到的。
        assert_eq!(
            check(&dir, t0 + 3600, "0.1.0", fetch),
            Some("0.2.0".to_owned())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            check(&dir, t0 + 86_400, "0.1.0", fetch),
            Some("0.2.0".to_owned())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // 已经装上新版就不提示了。
        assert_eq!(check(&dir, t0 + 86_401, "0.2.0", fetch), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_check_is_silent_and_still_throttled() {
        let dir = temp_dir("fail");
        let calls = AtomicUsize::new(0);
        let fail = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("没网".to_owned())
        };
        assert_eq!(check(&dir, 1000, "0.1.0", fail), None);
        assert_eq!(check(&dir, 2000, "0.1.0", fail), None);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "失败也一天只试一次");
        // 之前查到过新版，这次失败照旧提示。
        let ok = || Ok("v0.3.0".to_owned());
        assert_eq!(
            check(&dir, 1000 + 86_400, "0.1.0", ok),
            Some("0.3.0".to_owned())
        );
        assert_eq!(
            check(&dir, 1000 + 2 * 86_400, "0.1.0", fail),
            Some("0.3.0".to_owned())
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn board_check_is_quiet_without_a_repo() {
        // 仓库没定时不联网也不写缓存。
        if release::REPO.is_none() {
            let dir = temp_dir("norepo");
            assert_eq!(board_check(&dir), None);
            assert!(!dir.join(CACHE_FILE).exists());
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn detects_install_source() {
        let brew = [PathBuf::from("/opt/homebrew"), PathBuf::from("/usr/local")];
        let cargo = PathBuf::from("/Users/me/.cargo");
        let src = |p: &str| detect_source(Path::new(p), &brew, Some(&cargo));
        assert_eq!(
            src("/opt/homebrew/Cellar/huitai/0.2.0/bin/ai"),
            Source::Homebrew(PathBuf::from("/opt/homebrew"))
        );
        assert_eq!(
            src("/usr/local/Cellar/huitai/0.2.0/bin/ai"),
            Source::Homebrew(PathBuf::from("/usr/local"))
        );
        assert_eq!(src("/Users/me/.cargo/bin/ai"), Source::Cargo);
        assert_eq!(src("/Users/me/code/huitai/target/release/ai"), Source::Dev);
        assert_eq!(src("/Users/me/code/huitai/target/debug/ai"), Source::Dev);
        // 手动放进 /usr/local/bin 的不是 Homebrew 装的。
        assert_eq!(src("/usr/local/bin/ai"), Source::Direct);
        assert_eq!(src("/Users/me/bin/ai"), Source::Direct);
        assert_eq!(
            detect_source(Path::new("/Users/me/.cargo/bin/ai"), &brew, None),
            Source::Direct
        );
    }

    #[test]
    fn parses_sum_files() {
        let hex = "a".repeat(64);
        assert_eq!(parse_sum(&format!("{hex}  ai\n")), Some(hex.clone()));
        assert_eq!(parse_sum(&hex.to_uppercase()), Some(hex));
        assert_eq!(parse_sum("abc  ai"), None);
        assert_eq!(parse_sum(""), None);
    }
}
