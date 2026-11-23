//! 各家 CLI 升级执行：假的包管理器脚本放进临时 PATH，测会跑哪条命令、成功后复查 PATH、失败原文、超时、
//! 被别的同名程序遮住、各种挡住的情况（自带更新器的锁、会话台自己的锁、托管对话在跑、只能手动的），
//! 以及哪些能一键升、自动升（自带更新的等 24 小时、同一版自动只试一次、跨大版本不升）。不真跑任何安装。

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use huitai::clis::{
    Install, LoginEnv, Outcome, RESULTS_FILE, Runner, SELF_UPDATE_GRACE, Status, auto_due,
    last_results, probe_one, upgrade,
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
        }
    }
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
    let r = upgrade(&install, "0.172.0", false, false, &t.runner(&login, T)).unwrap();
    assert!(r.ok, "{r:?}");
    assert_eq!(r.message, "已升到 0.172.0");
    assert_eq!(r.command, "npm install -g @openai/codex@latest");
    assert_eq!(t.npm_args().trim(), "install -g @openai/codex@latest");
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
    let r = upgrade(&install, "0.172.0", false, false, &t.runner(&login, T)).unwrap();
    assert!(!r.ok);
    assert!(
        r.message
            .starts_with("npm install -g @openai/codex@latest 退出码 243：\n"),
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
    let r = upgrade(&install, "0.172.0", false, false, &t.runner(&login, T)).unwrap();
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
    let r = upgrade(&install, "0.172.0", false, false, &t.runner(&login, T)).unwrap();
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

    // 有托管对话在用 npm 装的：不原地覆盖。
    let b = upgrade(&install, "0.172.0", true, false, &runner).unwrap_err();
    assert!(b.reason.contains("托管的对话在用它"), "{b:?}");
    assert_eq!(
        b.command.as_deref(),
        Some("npm install -g @openai/codex@latest")
    );

    // 跨大版本：只提示。
    let b = upgrade(&install, "1.0.0", false, false, &runner).unwrap_err();
    assert!(b.reason.contains("跨大版本"), "{b:?}");

    // 别的会话台拿着锁（进程还在）。
    let lock = t.p("data/cli-upgrade-codex.lock");
    fs::write(&lock, std::process::id().to_string()).unwrap();
    let b = upgrade(&install, "0.172.0", false, false, &runner).unwrap_err();
    assert!(b.reason.contains("正在升级"), "{b:?}");
    assert!(lock.exists(), "别人的锁不动");
    assert_eq!(t.npm_args(), "", "挡住的都没跑 npm");

    // 拿锁的进程已经不在了：当残留拿过来。
    fs::write(&lock, "999999").unwrap();
    let r = upgrade(&install, "0.172.0", false, false, &runner).unwrap();
    assert!(r.ok, "{r:?}");
    assert!(!lock.exists());
}

#[test]
fn not_writable_is_manual() {
    let (t, login, _) = codex_tree("readonly");
    let nm = t.p("npm/lib/node_modules");
    fs::set_permissions(&nm, fs::Permissions::from_mode(0o555)).unwrap();
    let install = t.install(Vendor::Codex, &t.p("npm/bin/codex"), &login);
    let b = upgrade(&install, "0.172.0", false, false, &t.runner(&login, T)).unwrap_err();
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
    let b = upgrade(&install, "2.1.340", false, false, &runner).unwrap_err();
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
    let r = upgrade(&install, "2.1.340", false, false, &runner).unwrap();
    assert_eq!(
        t.npm_args().trim(),
        "install -g @anthropic-ai/claude-code@latest"
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
        Some("npm install -g @openai/codex@latest")
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
        Some("npm install -g @anthropic-ai/claude-code@latest")
    );
    assert!(auto_due(&c, NOW + SELF_UPDATE_GRACE, None));

    // 同一版自动升过一次（不管成没成）就不再自动试；手动升的失败不算，下一版照常。
    let tried = |auto: bool, ok: bool, target: &str| Outcome {
        vendor: Vendor::Codex,
        from: Some("0.171.0".into()),
        target: target.into(),
        command: "npm install -g @openai/codex@latest".into(),
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
