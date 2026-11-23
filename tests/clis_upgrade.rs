//! 各家 CLI 升级执行：假的包管理器脚本放进临时 PATH，测会跑哪条命令、成功后复查 PATH、失败原文、超时、
//! 被别的同名程序遮住、各种挡住的情况（自带更新器的锁、会话台自己的锁、只能手动的），轮到每家时现查有没有在跑的
//! （批中途开的托管对话、别处的进程、查不了都跳过），
//! 装的是查过的那一版（npm 写死版本，Homebrew 升之前重查），以及哪些能一键升、自动升（自带更新的等 24 小时、
//! 同一版自动只试一次、跨大版本不升）。不真跑任何安装。

mod clis_support;

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use clis_support::Fake;
use huitai::clis::{
    Bases, Install, LoginEnv, Manager, Method, Outcome, RESULTS_FILE, Runner, SELF_UPDATE_GRACE,
    Status, auto_due, check, last_results, now_secs, plan, probe_one, processes, upgrade,
    upgrade_from,
};
use huitai::core::Vendor;

const NOW: u64 = 1_800_000_000;

/// 临时目录：`home` 当家目录，`npm` 当 npm 全局前缀，`tools` 放假的 npm，`data` 是会话台数据目录。
struct Tree {
    root: PathBuf,
    home: PathBuf,
    data: PathBuf,
}

impl Tree {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("huitai-upgrade-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("home")).unwrap();
        fs::create_dir_all(d.join("data")).unwrap();
        let root = fs::canonicalize(&d).unwrap();
        Self {
            home: root.join("home"),
            data: root.join("data"),
            root,
        }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn script(&self, rel: &str, body: &str) -> PathBuf {
        let p = self.p(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// npm 全局装的一个包（`npm/lib/node_modules/<包>`），`npm/bin/<命令>` 链到它的入口。
    fn npm_package(&self, package: &str, program: &str, version: &str) -> PathBuf {
        let dir = format!("npm/lib/node_modules/{package}");
        self.write_version(package, version);
        let entry = self.script(&format!("{dir}/bin/cli.js"), "echo js");
        let link = self.p(&format!("npm/bin/{program}"));
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&entry, &link).unwrap();
        link
    }

    fn write_version(&self, package: &str, version: &str) {
        let d = self.p(&format!("npm/lib/node_modules/{package}"));
        fs::create_dir_all(&d).unwrap();
        fs::write(
            d.join("package.json"),
            format!(r#"{{"name":"{package}","version":"{version}"}}"#),
        )
        .unwrap();
    }

    /// 假 npm：把参数记进 `npm-args`，再跑 `body`。
    fn fake_npm(&self, body: &str) {
        let log = self.p("npm-args");
        self.script(
            "tools/npm",
            &format!("echo \"$@\" >> '{}'\n{body}", log.display()),
        );
    }

    fn npm_args(&self) -> String {
        fs::read_to_string(self.p("npm-args")).unwrap_or_default()
    }

    /// Homebrew 装的 gemini-cli（`brew/Cellar/gemini-cli/<版本>`），`brew/bin/gemini` 链过去。
    fn brew_gemini(&self, version: &str) -> PathBuf {
        let real = self.script(&format!("brew/Cellar/gemini-cli/{version}/bin/gemini"), "");
        let link = self.p("brew/bin/gemini");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        let _ = fs::remove_file(&link);
        symlink(real, &link).unwrap();
        link
    }

    /// 假 brew：把参数记进 `brew-args`，装上 `version`。
    fn fake_brew(&self, version: &str) {
        let cellar = self.p("brew/Cellar/gemini-cli");
        self.script(
            "tools/brew",
            &format!(
                "echo \"$@\" >> '{log}'\n\
                 mkdir -p '{cellar}/{version}/bin' && cp '{link}' '{cellar}/{version}/bin/gemini'\n\
                 ln -sf '{cellar}/{version}/bin/gemini' '{link}'",
                log = self.p("brew-args").display(),
                cellar = cellar.display(),
                link = self.p("brew/bin/gemini").display(),
            ),
        );
    }

    fn brew_args(&self) -> String {
        fs::read_to_string(self.p("brew-args")).unwrap_or_default()
    }

    /// PATH：`first` 里的目录在前，然后 npm/bin、tools、系统目录。
    fn login(&self, first: &[&str]) -> LoginEnv {
        let mut path: Vec<PathBuf> = first.iter().map(|d| self.p(d)).collect();
        path.extend([self.p("npm/bin"), self.p("tools")]);
        path.extend(["/usr/bin", "/bin"].map(PathBuf::from));
        LoginEnv {
            path,
            npm_prefix: Some(self.p("npm")),
        }
    }

    fn install(&self, vendor: Vendor, link: &Path, login: &LoginEnv) -> Install {
        probe_one(vendor, link, login, &self.p("home"))
    }

    fn runner<'a>(&'a self, login: &'a LoginEnv, timeout: Duration) -> Runner<'a> {
        Runner {
            login,
            home: &self.home,
            data_dir: &self.data,
            timeout,
            hosted: &no_hosted,
            processes: &no_processes,
        }
    }
}

fn no_hosted() -> io::Result<HashSet<Vendor>> {
    Ok(HashSet::new())
}

fn no_processes() -> io::Result<String> {
    Ok(String::new())
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::process::Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&self.root)
            .status();
        let _ = fs::remove_dir_all(&self.root);
    }
}

const T: Duration = Duration::from_secs(20);

/// Codex 用 npm 装的、0.171.0，假 npm 一跑就把版本改成 0.172.0。
fn codex_tree(name: &str) -> (Tree, LoginEnv, Install) {
    let t = Tree::new(name);
    let link = t.npm_package("@openai/codex", "codex", "0.171.0");
    let pkg = t.p("npm/lib/node_modules/@openai/codex/package.json");
    t.fake_npm(&format!(
        "printf '%s' '{{\"name\":\"@openai/codex\",\"version\":\"0.172.0\"}}' > '{}'\necho 'added 1 package'",
        pkg.display()
    ));
    let login = t.login(&[]);
    let install = t.install(Vendor::Codex, &link, &login);
    (t, login, install)
}

#[test]
fn runs_the_official_command_and_checks_path_afterwards() {
    let (t, login, install) = codex_tree("ok");
    assert_eq!(install.version.as_deref(), Some("0.171.0"));
    let r = upgrade(&install, "0.172.0", false, &t.runner(&login, T)).unwrap();
    assert!(r.ok, "{r:?}");
    assert_eq!(r.message, "已升到 0.172.0");
    assert_eq!(r.command, "npm install -g @openai/codex@0.172.0");
    assert_eq!(t.npm_args().trim(), "install -g @openai/codex@0.172.0");
    // 记进升级记录，doctor 和自动升级看得到；锁放掉了。
    let last = last_results(&t.p("data"));
    assert_eq!(last.get(&Vendor::Codex), Some(&r));
    assert!(t.p("data").join(RESULTS_FILE).exists());
    assert!(!t.p("data/cli-upgrade-codex.lock").exists());
}

#[test]
fn failure_keeps_the_raw_output() {
    let (t, login, install) = codex_tree("fail");
    t.fake_npm("echo 'npm error code EACCES' >&2\necho 'npm error path /x' >&2\nexit 243");
    let r = upgrade(&install, "0.172.0", false, &t.runner(&login, T)).unwrap();
    assert!(!r.ok);
    assert!(
        r.message
            .starts_with("npm install -g @openai/codex@0.172.0 退出码 243：\n"),
        "{}",
        r.message
    );
    assert!(
        r.message
            .contains("npm error code EACCES\nnpm error path /x"),
        "{}",
        r.message
    );
    assert!(!last_results(&t.p("data"))[&Vendor::Codex].ok);
}

#[test]
fn times_out_and_says_so() {
    let (t, login, install) = codex_tree("timeout");
    t.fake_npm("sleep 30");
    let started = std::time::Instant::now();
    let r = upgrade(
        &install,
        "0.172.0",
        false,
        &t.runner(&login, Duration::from_millis(300)),
    )
    .unwrap();
    assert!(!r.ok);
    assert!(
        r.message.contains("超过 0.3 秒没跑完，已结束"),
        "{}",
        r.message
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn command_succeeded_but_version_did_not_change() {
    let (t, login, install) = codex_tree("same");
    t.fake_npm("echo 'up to date'");
    let r = upgrade(&install, "0.172.0", false, &t.runner(&login, T)).unwrap();
    assert!(!r.ok);
    assert!(
        r.message.contains("还是 0.171.0（要 0.172.0）"),
        "{}",
        r.message
    );
    assert!(
        r.message.contains("up to date"),
        "带上命令输出：{}",
        r.message
    );
}

#[test]
fn new_version_hidden_behind_another_copy_on_path() {
    let (t, _, install) = codex_tree("shadow");
    // PATH 上排在 npm 前面还有一份老的 codex（问 --version 答 0.171.0）。
    t.script("old/codex", "echo 'codex-cli 0.171.0'");
    let login = t.login(&["old"]);
    let r = upgrade(&install, "0.172.0", false, &t.runner(&login, T)).unwrap();
    assert!(!r.ok);
    assert!(r.message.contains("被它遮住了"), "{}", r.message);
    assert!(
        r.message
            .contains(&t.p("npm/bin/codex").display().to_string()),
        "说出新版在哪：{}",
        r.message
    );
}

#[test]
fn blocked_cases_do_not_run_anything() {
    let (t, login, install) = codex_tree("blocked");
    let runner = t.runner(&login, T);

    // 有托管对话在用：不换文件。
    let hosted = Runner {
        hosted: &|| Ok([Vendor::Codex].into()),
        ..t.runner(&login, T)
    };
    let b = upgrade(&install, "0.172.0", false, &hosted).unwrap_err();
    assert!(b.reason.contains("托管的对话在用它"), "{b:?}");
    assert_eq!(
        b.command.as_deref(),
        Some("npm install -g @openai/codex@0.172.0")
    );

    // 跨大版本：只提示。
    let b = upgrade(&install, "1.0.0", false, &runner).unwrap_err();
    assert!(b.reason.contains("跨大版本"), "{b:?}");

    // 别的会话台拿着锁（进程还在）。
    let lock = t.p("data/cli-upgrade-codex.lock");
    fs::write(&lock, std::process::id().to_string()).unwrap();
    let b = upgrade(&install, "0.172.0", false, &runner).unwrap_err();
    assert!(b.reason.contains("正在升级"), "{b:?}");
    assert!(lock.exists(), "别人的锁不动");
    assert_eq!(t.npm_args(), "", "挡住的都没跑 npm");

    // 拿锁的进程已经不在了：当残留拿过来。
    fs::write(&lock, "999999").unwrap();
    let r = upgrade(&install, "0.172.0", false, &runner).unwrap();
    assert!(r.ok, "{r:?}");
    assert!(!lock.exists());
}

#[test]
fn not_writable_is_manual() {
    let (t, login, _) = codex_tree("readonly");
    let nm = t.p("npm/lib/node_modules");
    fs::set_permissions(&nm, fs::Permissions::from_mode(0o555)).unwrap();
    let install = t.install(Vendor::Codex, &t.p("npm/bin/codex"), &login);
    let b = upgrade(&install, "0.172.0", false, &t.runner(&login, T)).unwrap_err();
    assert!(b.reason.contains("不可写"), "{b:?}");
    assert_eq!(t.npm_args(), "");
}

#[test]
fn waits_for_claude_own_updater_lock() {
    let t = Tree::new("claude-lock");
    let link = t.npm_package("@anthropic-ai/claude-code", "claude", "2.1.330");
    t.fake_npm("exit 0");
    let login = t.login(&[]);
    let install = t.install(Vendor::Claude, &link, &login);
    let lock = t.p("home/.claude/.update.lock");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::write(&lock, "123").unwrap();
    let runner = t.runner(&login, T);
    let b = upgrade(&install, "2.1.340", false, &runner).unwrap_err();
    assert!(b.reason.contains("它自己正在更新"), "{b:?}");
    assert_eq!(t.npm_args(), "");

    // 锁是 5 分钟前的：它自己也当残留，照常升。
    let old = SystemTime::now() - Duration::from_secs(10 * 60);
    fs::File::options()
        .write(true)
        .open(&lock)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let r = upgrade(&install, "2.1.340", false, &runner).unwrap();
    assert_eq!(
        t.npm_args().trim(),
        "install -g @anthropic-ai/claude-code@2.1.340"
    );
    assert!(!r.ok, "假 npm 没改版本：{r:?}");
}

fn status(install: Install, latest: &str, seen_at: u64) -> Status {
    Status {
        install,
        latest: Some(latest.to_owned()),
        seen_at: Some(seen_at),
    }
}

#[test]
fn which_ones_upgrade_now_and_automatically() {
    let t = Tree::new("rules");
    let login = t.login(&[]);
    let codex = t.install(
        Vendor::Codex,
        &t.npm_package("@openai/codex", "codex", "0.171.0"),
        &login,
    );
    let claude = t.install(
        Vendor::Claude,
        &t.npm_package("@anthropic-ai/claude-code", "claude", "2.1.330"),
        &login,
    );

    // Codex 没有自带安装：有新版就能升。
    let s = status(codex.clone(), "0.172.0", NOW);
    assert_eq!(
        s.upgrade_now(NOW).as_deref(),
        Some("npm install -g @openai/codex@0.172.0")
    );
    assert!(auto_due(&s, NOW, None));
    // 已是最新、跨大版本：不升。
    assert_eq!(status(codex.clone(), "0.171.0", NOW).upgrade_now(NOW), None);
    let major = status(codex.clone(), "1.0.0", NOW);
    assert_eq!(major.upgrade_now(NOW), None);
    assert!(!auto_due(&major, NOW, None));

    // Claude（npm 装的自带自动更新）：新版出来不满 24 小时先交给它自己，满了还是旧版就照常升。
    let c = status(claude, "2.1.340", NOW);
    assert_eq!(c.upgrade_now(NOW + SELF_UPDATE_GRACE - 1), None);
    assert!(!auto_due(&c, NOW + 60, None));
    assert_eq!(
        c.upgrade_now(NOW + SELF_UPDATE_GRACE).as_deref(),
        Some("npm install -g @anthropic-ai/claude-code@2.1.340")
    );
    assert!(auto_due(&c, NOW + SELF_UPDATE_GRACE, None));

    // 同一版自动升过一次（不管成没成）就不再自动试；手动升的失败不算，下一版照常。
    let tried = |auto: bool, ok: bool, target: &str| Outcome {
        vendor: Vendor::Codex,
        from: Some("0.171.0".into()),
        target: target.into(),
        command: "npm install -g @openai/codex@0.172.0".into(),
        ok,
        message: String::new(),
        at: NOW,
        auto,
    };
    assert!(!auto_due(&s, NOW, Some(&tried(true, false, "0.172.0"))));
    assert!(!auto_due(&s, NOW, Some(&tried(true, true, "0.172.0"))));
    assert!(auto_due(&s, NOW, Some(&tried(false, false, "0.172.0"))));
    assert!(auto_due(&s, NOW, Some(&tried(true, false, "0.170.0"))));
}

/// 缓存里最新是 1.3.0，缓存有效期内仓库出了 2.0.0：npm 装的写死 1.3.0，不会装上 2.0.0。
#[test]
fn npm_installs_the_checked_version_not_a_new_major() {
    let t = Tree::new("exact");
    let fake = Fake::start();
    let bases = Bases::at(&fake.origin);
    let link = t.npm_package("@openai/codex", "codex", "1.2.0");
    // 假 npm 当仓库：`@latest` 装 2.0.0，写了版本就装那一版。
    t.fake_npm(&format!(
        "v=${{3##*@}}; [ \"$v\" = latest ] && v=2.0.0\n\
         printf '{{\"version\":\"%s\"}}' \"$v\" > '{}'",
        t.p("npm/lib/node_modules/@openai/codex/package.json")
            .display()
    ));
    let login = t.login(&[]);
    let install = t.install(Vendor::Codex, &link, &login);
    let look = || check(&t.data, vec![install.clone()], NOW, &bases, T).remove(0);
    fake.route("/@openai/codex/latest", 200, r#"{"version":"1.3.0"}"#);
    look();
    fake.route("/@openai/codex/latest", 200, r#"{"version":"2.0.0"}"#);
    let s = look();
    assert_eq!(s.newer(), Some("1.3.0"), "不满 20 小时，用缓存里的");

    let runner = t.runner(&login, T);
    let r = upgrade_from(&s.install, "1.3.0", true, &runner, &bases).unwrap();
    assert!(r.ok, "{r:?}");
    assert_eq!(r.message, "已升到 1.3.0");
    assert_eq!(t.npm_args().trim(), "install -g @openai/codex@1.3.0");
    let now = t.install(Vendor::Codex, &link, &login);
    assert_eq!(now.version.as_deref(), Some("1.3.0"));
}

/// 每种包管理器都写死查到的那一版；没有比本机新的就不写死（免得给人看的命令是降级）。
#[test]
fn every_package_manager_names_the_version() {
    let cmd = |manager, current: &str, latest| {
        let install = Install {
            vendor: Vendor::Gemini,
            path: "/x/gemini".into(),
            real: "/x/gemini".into(),
            method: Method::Package {
                manager,
                package: "@google/gemini-cli",
            },
            version: Some(current.to_owned()),
            problem: None,
        };
        plan(&install, latest).command.unwrap()
    };
    for (manager, install) in [
        (Manager::Npm, "npm install -g"),
        (Manager::Pnpm, "pnpm add -g"),
        (Manager::Yarn, "yarn global add"),
        (Manager::Bun, "bun add -g"),
        (Manager::Volta, "volta install"),
    ] {
        assert_eq!(
            cmd(manager, "1.2.0", Some("v1.3.0")),
            format!("{install} @google/gemini-cli@1.3.0")
        );
    }
    for latest in [None, Some("1.2.0"), Some("1.1.0"), Some("1.3.0;x")] {
        assert_eq!(
            cmd(Manager::Npm, "1.2.0", latest),
            "npm install -g @google/gemini-cli@latest",
            "{latest:?}"
        );
    }
}

/// 排队的工夫它自己更新到更新的版本了：不拿写死的旧版把它装回去，也不让人自己跑那条。
#[test]
fn does_not_downgrade_one_that_updated_itself_meanwhile() {
    let (t, login, install) = codex_tree("meanwhile");
    t.write_version("@openai/codex", "0.173.0");
    let b = upgrade(&install, "0.172.0", true, &t.runner(&login, T)).unwrap_err();
    assert!(b.reason.contains("本机已是 0.173.0"), "{b:?}");
    assert_eq!(b.command, None);
    assert_eq!(t.npm_args(), "");
    assert!(!t.p("data/cli-upgrade-codex.lock").exists());
}

/// Homebrew 不能指定版本：升之前重查。仓库出了新大版本就只提示，并写回缓存（按钮和自动升级跟着没有）；
/// 没查到也不升；还是同一个大版本就升到重查到的那一版。
#[test]
fn homebrew_rechecks_right_before_upgrading() {
    let t = Tree::new("brew");
    let fake = Fake::start();
    let bases = Bases::at(&fake.origin);
    let api = "/api/formula/gemini-cli.json";
    let publish = |v: &str| fake.route(api, 200, &format!(r#"{{"versions":{{"stable":"{v}"}}}}"#));
    let link = t.brew_gemini("1.2.0");
    let login = t.login(&["brew/bin"]);
    let install = t.install(Vendor::Gemini, &link, &login);
    assert_eq!(install.method, Method::Formula("gemini-cli".into()));
    let now = now_secs();
    let look = || check(&t.data, vec![install.clone()], now, &bases, T).remove(0);
    publish("1.3.0");
    assert!(auto_due(&look(), now, None));
    let runner = t.runner(&login, T);
    let go = || upgrade_from(&install, "1.3.0", true, &runner, &bases);

    // 重查没查到：不升，缓存不动。
    fake.route(api, 500, "oops");
    let b = go().unwrap_err();
    assert!(b.reason.contains("没查到它此刻的最新版"), "{b:?}");
    assert_eq!(b.command.as_deref(), Some("brew upgrade gemini-cli"));
    assert_eq!(look().newer(), Some("1.3.0"));

    // 出了 2.0.0：只提示。
    t.fake_brew("2.0.0");
    publish("2.0.0");
    let b = go().unwrap_err();
    assert!(b.reason.contains("跨大版本（1 → 2）"), "{b:?}");
    assert_eq!(t.brew_args(), "", "没跑 brew");
    let s = look();
    assert_eq!(s.newer(), Some("2.0.0"), "重查到的写回缓存");
    assert_eq!(s.upgrade_now(now), None);
    assert!(!auto_due(&s, now, None));

    // 仓库撤回到本机这一版：没有要升的。
    publish("1.2.0");
    let b = go().unwrap_err();
    assert!(b.reason.contains("不比它新"), "{b:?}");
    assert_eq!(t.brew_args(), "");

    // 同一个大版本里又出了一版：升到它，记的也是它（自动升级按这一版只试一次）。
    t.fake_brew("1.4.0");
    publish("1.4.0");
    let r = go().unwrap();
    assert!(r.ok, "{r:?}");
    assert_eq!(
        (r.target.as_str(), r.message.as_str()),
        ("1.4.0", "已升到 1.4.0")
    );
    assert_eq!(t.brew_args().trim(), "upgrade gemini-cli");
}

/// Codex 和 Kimi 都是 npm 装的，各有新版；假 npm 装什么就改什么的版本，并留下 `upgrading`（第一家已经在升了）。
fn two_vendors(name: &str) -> (Tree, LoginEnv, Vec<(Install, &'static str)>) {
    let t = Tree::new(name);
    let codex = t.npm_package("@openai/codex", "codex", "0.171.0");
    let kimi = t.npm_package("@moonshot-ai/kimi-code", "kimi", "0.70.0");
    t.fake_npm(&format!(
        "touch '{}'\nspec=$3; printf '{{\"version\":\"%s\"}}' \"${{spec##*@}}\" > '{}/'\"${{spec%@*}}\"/package.json",
        t.p("upgrading").display(),
        t.p("npm/lib/node_modules").display()
    ));
    let login = t.login(&[]);
    let batch = vec![
        (t.install(Vendor::Codex, &codex, &login), "0.172.0"),
        (t.install(Vendor::Kimi, &kimi, &login), "0.71.0"),
    ];
    (t, login, batch)
}

/// 升第一家时开了第二家的托管对话：轮到第二家时现问，跳过它并说明原因；自动、手动都一样。
#[test]
fn conversation_opened_mid_batch_skips_that_vendor() {
    for auto in [true, false] {
        let (t, login, batch) = two_vendors(&format!("mid-hosted-{auto}"));
        let started = t.p("upgrading");
        let runner = Runner {
            hosted: &|| {
                Ok(if started.exists() {
                    [Vendor::Kimi].into()
                } else {
                    HashSet::new()
                })
            },
            ..t.runner(&login, T)
        };
        let mut results = batch
            .iter()
            .map(|(install, target)| upgrade(install, target, auto, &runner));
        assert!(results.next().unwrap().unwrap().ok);
        let b = results.next().unwrap().unwrap_err();
        assert_eq!(b.vendor, Vendor::Kimi);
        assert!(b.reason.contains("会话台托管的对话在用它"), "{b:?}");
        assert_eq!(
            b.command.as_deref(),
            Some("npm install -g @moonshot-ai/kimi-code@0.71.0")
        );
        assert_eq!(t.npm_args().trim(), "install -g @openai/codex@0.172.0");
        assert!(!t.p("data/cli-upgrade-kimi.lock").exists(), "锁放掉了");
        assert!(!last_results(&t.data).contains_key(&Vendor::Kimi));
    }
}

/// 升第一家时别的终端里开了第二家（不归会话台托管）：同样跳过，说出进程号。
#[test]
fn outside_process_mid_batch_skips_that_vendor() {
    let (t, login, batch) = two_vendors("mid-outside");
    let started = t.p("upgrading");
    let runner = Runner {
        processes: &|| {
            Ok(if started.exists() {
                "  501 -zsh\n 4242 kimi-code\n".to_owned()
            } else {
                "  501 -zsh\n".to_owned()
            })
        },
        ..t.runner(&login, T)
    };
    let (codex, kimi) = (&batch[0], &batch[1]);
    assert!(upgrade(&codex.0, codex.1, true, &runner).unwrap().ok);
    let b = upgrade(&kimi.0, kimi.1, true, &runner).unwrap_err();
    assert!(
        b.reason.contains("别处有 1 个它的进程在跑（进程号 4242）"),
        "{b:?}"
    );
    assert_eq!(t.npm_args().trim(), "install -g @openai/codex@0.172.0");
}

/// 托管列表问不到、本机进程查不了：都当在跑，不升。
#[test]
fn cannot_tell_counts_as_running() {
    let (t, login, install) = codex_tree("unknown");
    let broken = Runner {
        hosted: &|| Err(io::Error::other("握手应答不对")),
        ..t.runner(&login, T)
    };
    let b = upgrade(&install, "0.172.0", true, &broken).unwrap_err();
    assert!(b.reason.contains("托管的对话"), "{b:?}");
    assert!(b.reason.contains("握手应答不对"), "{b:?}");
    let broken = Runner {
        processes: &|| Err(io::Error::other("ps 启动失败")),
        ..t.runner(&login, T)
    };
    let b = upgrade(&install, "0.172.0", false, &broken).unwrap_err();
    assert!(b.reason.contains("查不了本机有没有它的进程在跑"), "{b:?}");
    assert!(b.reason.contains("ps 启动失败"), "{b:?}");
    assert_eq!(t.npm_args(), "");
}

/// 一行 `ps` 算不算在用 `install`（算就挡住、什么都不跑）。
fn blocks(t: &Tree, login: &LoginEnv, install: &Install, target: &str, command: &str) -> bool {
    let runner = Runner {
        processes: &|| Ok(format!("  501 -zsh\n 4242 {command}\n")),
        // 没挡住的接着跑升级命令，马上结束就行。
        ..t.runner(login, T)
    };
    match upgrade(install, target, false, &runner) {
        Ok(_) => false,
        Err(b) => {
            assert!(b.reason.contains("进程号 4242"), "{command}：{b:?}");
            true
        }
    }
}

/// 按可执行文件路径认：直接起的、解释器起的、包里附带的程序、只剩名字的都算；别处装的另一份、只是参数里提到的不算。
#[test]
fn running_is_told_by_the_executable_path() {
    let (t, login, install) = codex_tree("paths");
    t.fake_npm("exit 1");
    let link = t.p("npm/bin/codex").display().to_string();
    let pkg = t.p("npm/lib/node_modules/@openai/codex");
    let entry = pkg.join("bin/cli.js").display().to_string();
    let pkg = pkg.display();
    let other = t.p("elsewhere-codex");
    symlink(&entry, &other).unwrap();
    for yes in [
        "codex",
        "codex resume 019a1b2c",
        &link,
        &format!("{link} --model x"),
        &format!("node {link}"),
        &format!("/opt/homebrew/bin/node --max-old-space-size=8192 --no-warnings {entry} exec"),
        &format!("{pkg}/node_modules/@openai/codex-darwin-arm64/vendor/bin/codex"),
        &format!("{} mcp-server", other.display()),
    ] {
        assert!(blocks(&t, &login, &install, "0.172.0", yes), "{yes}");
    }
    for no in [
        "/Users/me/.codex/packages/app-server-daemon/releases/0.171.0-aarch64-apple-darwin/bin/codex app-server",
        "/Applications/Codex.app/Contents/Resources/codex app-server",
        "/Applications/My App.app/Contents/MacOS/codex helper",
        &format!("{pkg}-sdk/bin/codex"),
        &format!("vim {entry}"),
        &format!("claude -p 看看{link}"),
        &format!("node /srv/run.js {link}"),
        "node /srv/server.js codex",
        "codexbar",
        "grep codex",
    ] {
        assert!(!blocks(&t, &login, &install, "0.172.0", no), "{no}");
    }
}

/// 改了自己进程名的（Node 的 `process.title` 盖掉命令行）按名字认。
#[test]
fn running_is_told_by_process_title() {
    let (t, login, batch) = two_vendors("titles");
    t.fake_npm("exit 1");
    let (kimi, target) = &batch[1];
    for yes in ["kimi-code", "kimi", "kimi -S session_x"] {
        assert!(blocks(&t, &login, kimi, target, yes), "{yes}");
    }
    assert!(!blocks(&t, &login, kimi, target, "codex"));
    let link = t.npm_package("@earendil-works/pi-coding-agent", "pi", "1.5.2");
    let pi = t.install(Vendor::Pi, &link, &login);
    for yes in ["pi", "pi-rpc"] {
        // pi 升之前还要联网重查，在跑的先挡住。
        assert!(blocks(&t, &login, &pi, "1.5.3", yes), "{yes}");
    }
}

/// Homebrew、官方安装器装的也一样：认的是这个名字的整个目录，跑着的是旧版本也算。
#[test]
fn homebrew_and_native_installs_wait_too() {
    let t = Tree::new("other-methods");
    let fake = Fake::start();
    let bases = Bases::at(&fake.origin);
    let link = t.brew_gemini("1.2.0");
    t.fake_brew("1.3.0");
    fake.route(
        "/api/formula/gemini-cli.json",
        200,
        r#"{"versions":{"stable":"1.3.0"}}"#,
    );
    let login = t.login(&["brew/bin"]);
    let gemini = t.install(Vendor::Gemini, &link, &login);
    let cellar = t.p("brew/Cellar/gemini-cli");
    let old = format!(
        " 77 node {}/1.1.0/libexec/lib/gemini.js\n 78 /Applications/Gemini.app/Contents/MacOS/Gemini\n",
        cellar.display()
    );
    let runner = Runner {
        processes: &|| Ok(old.clone()),
        ..t.runner(&login, T)
    };
    let b = upgrade_from(&gemini, "1.3.0", true, &runner, &bases).unwrap_err();
    assert!(b.reason.contains("1 个它的进程在跑（进程号 77）"), "{b:?}");
    assert_eq!(t.brew_args(), "");
    assert_eq!(b.command.as_deref(), Some("brew upgrade gemini-cli"));
    let r = upgrade_from(&gemini, "1.3.0", true, &t.runner(&login, T), &bases).unwrap();
    assert!(r.ok, "{r:?}");

    // Claude 原生版：`~/.local/share/claude/versions/<版本>`，旧版还在跑；家目录名里有空格也认得出。
    let real = t.script("my home/.local/share/claude/versions/2.1.330", "");
    let link = t.p("my home/.local/bin/claude");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(&real, &link).unwrap();
    let claude = probe_one(Vendor::Claude, &link, &login, &t.p("my home"));
    assert_eq!(claude.method, Method::Native);
    let versions = real.parent().unwrap().display().to_string();
    for running in [
        format!("{versions}/2.1.329 -p hi"),
        format!("{} --resume x", link.display()),
        "claude".to_owned(),
    ] {
        assert!(
            blocks(&t, &login, &claude, "2.1.340", &running),
            "{running}"
        );
    }

    // Volta：真身是转发程序，认的是放包的目录。
    let packages = ".volta/tools/image/packages/@openai/codex";
    let entry = t.script(
        &format!("home/{packages}/lib/node_modules/@openai/codex/bin/codex.js"),
        "",
    );
    fs::write(
        entry
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("package.json"),
        r#"{"version":"0.171.0"}"#,
    )
    .unwrap();
    let shim = t.script("home/.volta/bin/codex", "");
    let codex = t.install(Vendor::Codex, &shim, &login);
    assert!(matches!(
        codex.method,
        Method::Package {
            manager: Manager::Volta,
            ..
        }
    ));
    let node = format!("node {}", entry.display());
    assert!(blocks(&t, &login, &codex, "0.172.0", &node));
    assert!(!blocks(
        &t,
        &login,
        &codex,
        "0.172.0",
        &t.p("home/.volta/bin/npm").display().to_string()
    ));
}

/// 真起一个进程、用真的 `ps` 查：认得出来，什么都不跑。
#[test]
fn real_ps_sees_a_real_process() {
    let (t, login, install) = codex_tree("real-ps");
    let mut child = Command::new("/bin/sleep")
        .arg0(t.p("npm/bin/codex"))
        .arg("60")
        .spawn()
        .unwrap();
    let runner = Runner {
        processes: &processes,
        ..t.runner(&login, T)
    };
    let r = upgrade(&install, "0.172.0", false, &runner);
    let listed = processes().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        listed
            .lines()
            .any(|l| l.trim_start().starts_with(&format!("{} ", child.id()))),
        "ps 里有它"
    );
    let b = r.unwrap_err();
    assert!(b.reason.contains("它的进程在跑"), "{b:?}");
    assert_eq!(t.npm_args(), "");
}
