//! 各家数据目录按上游规则解析（审计 v0.1.3 第 21、22 条）：OpenCode 的 `OPENCODE_DB` 相对路径接在
//! `$XDG_DATA_HOME/opencode` 后面、只设 `XDG_DATA_HOME` 也跟着走；启动命令带的变量和读的是同一处。
//! 旧版缓存名（只用来找旧标注）老三家、新几家都不变（不再支持的 Qwen Code 也照旧算），
//! 现在的缓存按账号分，同一个目录给两家用也不撞。

mod sources_common;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use huitai::core::{Account, Vendor, VendorDirs};
use huitai::sources::{account_env_prefix, accounts, default_account_env_prefix, env_prefix};
use sources_common::temp_dir;

const H: &str = "/h";

fn env_of(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
    let pairs = pairs.to_vec();
    move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.into())
}

/// 家目录 `/h`、在 `/launch` 启动。
fn dirs(pairs: &[(&'static str, &'static str)]) -> VendorDirs {
    VendorDirs::new(Path::new(H), Path::new("/launch"), env_of(pairs))
}

fn opencode(pairs: &[(&'static str, &'static str)]) -> PathBuf {
    dirs(pairs).dir(Vendor::OpenCode).to_path_buf()
}

#[test]
fn opencode_db_follows_xdg_data_home_and_opencode_db() {
    let cases: &[(&[(&str, &str)], &str)] = &[
        (&[], "/h/.local/share/opencode/opencode.db"),
        // 相对路径接在 OpenCode 的数据目录后面，不按启动目录。
        (
            &[("OPENCODE_DB", "work.db")],
            "/h/.local/share/opencode/work.db",
        ),
        (
            &[("OPENCODE_DB", "./sub/work.db")],
            "/h/.local/share/opencode/sub/work.db",
        ),
        (&[("OPENCODE_DB", "/data/oc.db")], "/data/oc.db"),
        (&[("OPENCODE_DB", "~/oc.db")], "/h/oc.db"),
        // 只设 XDG_DATA_HOME：库在它下面的 opencode/opencode.db。
        (&[("XDG_DATA_HOME", "/xdg")], "/xdg/opencode/opencode.db"),
        (&[("XDG_DATA_HOME", "/xdg/")], "/xdg/opencode/opencode.db"),
        // 相对的 XDG_DATA_HOME 按启动目录。
        (
            &[("XDG_DATA_HOME", "share")],
            "/launch/share/opencode/opencode.db",
        ),
        (
            &[("XDG_DATA_HOME", "/xdg"), ("OPENCODE_DB", "w.db")],
            "/xdg/opencode/w.db",
        ),
        (
            &[("XDG_DATA_HOME", "/xdg"), ("OPENCODE_DB", "/data/oc.db")],
            "/data/oc.db",
        ),
        // 空值当没设。
        (
            &[("XDG_DATA_HOME", ""), ("OPENCODE_DB", "")],
            "/h/.local/share/opencode/opencode.db",
        ),
        (
            &[("XDG_DATA_HOME", ""), ("OPENCODE_DB", "w.db")],
            "/h/.local/share/opencode/w.db",
        ),
    ];
    for (env, want) in cases {
        assert_eq!(opencode(env), Path::new(want), "{env:?}");
    }
}

/// 交给守护进程的命令带的是算对的绝对路径；落在默认位置的不带。只设了 `XDG_DATA_HOME` 的带它（登录跟着库走），
/// 设了 `OPENCODE_DB` 的带库、`XDG_DATA_HOME` 照会话台的环境（没设就清掉）。
#[test]
fn opencode_command_carries_the_resolved_db() {
    let h = Path::new(H);
    let pre =
        |pairs: &[(&'static str, &'static str)]| env_prefix(&dirs(pairs), Vendor::OpenCode, h);
    assert_eq!(
        pre(&[("OPENCODE_DB", "work.db")]),
        "env -u XDG_DATA_HOME OPENCODE_DB=/h/.local/share/opencode/work.db "
    );
    assert_eq!(
        pre(&[("XDG_DATA_HOME", "/xdg")]),
        "env -u OPENCODE_DB XDG_DATA_HOME=/xdg "
    );
    assert_eq!(pre(&[]), "");
    // 显式写成默认位置，等于没改。
    assert_eq!(pre(&[("XDG_DATA_HOME", "/h/.local/share")]), "");
    assert_eq!(pre(&[("OPENCODE_DB", "opencode.db")]), "");
    assert_eq!(dirs(&[("OPENCODE_DB", "opencode.db")]).profile(h), None);
}

/// 旧版（到 0.1.3）按这组名字分开放缓存；现在只用它找旧标注，算法不能变：老三家的名字和加新厂商之前一样，
/// 新几家同一个路径同名（旧版就是这么写的，照旧名才找得到；Qwen Code 不再支持，旧名照样按它算）。
#[test]
fn legacy_profile_names_are_unchanged() {
    let h = Path::new(H);
    let at =
        |pairs: &[(&'static str, &'static str)]| VendorDirs::new(h, h, env_of(pairs)).profile(h);
    assert_eq!(
        at(&[("CODEX_HOME", "/data/codex")]).as_deref(),
        Some("7f687171")
    );
    assert_eq!(at(&[]), None);
    let kimi = at(&[("KIMI_CODE_HOME", "/some/work")]);
    assert_eq!(kimi.as_deref(), Some("b0c6cb1a"));
    assert_eq!(at(&[("QWEN_HOME", "/some/work")]), kimi);
}

/// 现在的缓存按账号分：两家用同一个目录，账号短名（对话 id 前缀、额度缓存目录）也不一样。
#[test]
fn same_dir_for_two_vendors_gets_two_accounts() {
    let h = Path::new(H);
    let dir = Path::new("/some/work");
    let kimi = Account::key_for(Vendor::Kimi, dir, h);
    let hermes = Account::key_for(Vendor::Hermes, dir, h);
    assert_eq!(kimi.len(), 8);
    assert_ne!(kimi, hermes);
    let all: Vec<String> = Vendor::ALL
        .iter()
        .map(|&v| Account::key_for(v, dir, h))
        .collect();
    for (i, a) in all.iter().enumerate() {
        assert!(!all[i + 1..].contains(a), "{a} 撞了");
    }
}

/// 在 `sh` 里跑「命令前缀 + env」，进程继承 `inherited`（像守护进程、登录 shell 里设着的变量），
/// 返回 CLI 实际拿到的环境。
fn env_seen_by_cli(prefix: &str, inherited: &[(&str, &Path)]) -> Vec<(String, String)> {
    let mut sh = std::process::Command::new("/bin/sh");
    sh.arg("-c").arg(format!("{prefix}env")).env_clear();
    for (k, v) in inherited {
        sh.env(k, v);
    }
    let out = sh.output().expect("跑得起 sh");
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout)
        .expect("环境是 UTF-8")
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// 带着 `inherited` 这些变量的终端里跑 `prefix` 开头的命令，CLI 按自己的规则读的是哪个目录（见 `VendorDirs::new`）。
fn dir_seen_by_cli(
    prefix: &str,
    inherited: &[(&str, &Path)],
    home: &Path,
    vendor: Vendor,
) -> PathBuf {
    let seen = env_seen_by_cli(prefix, inherited);
    let d = VendorDirs::new(home, home, |k| {
        seen.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| OsString::from(v))
    });
    d.dir(vendor).to_path_buf()
}

/// 待办「默认账号的命令清掉所有改目录的变量」：继承了 `XDG_DATA_HOME`（OpenCode）
/// 时，几个账号里默认账号的恢复命令仍指回家目录下的默认位置；以前只清 `dir_env`，会跑去别的库。
#[test]
fn default_account_command_ignores_inherited_dir_vars() {
    let base = temp_dir("core-dirs-default-env");
    let home = base.join("h");
    let xdg = base.join("xdg");
    fs::create_dir_all(home.join(".local/share/opencode")).unwrap();
    fs::write(home.join(".local/share/opencode/opencode.db"), b"").unwrap();
    fs::create_dir_all(xdg.join("opencode")).unwrap();
    fs::write(xdg.join("opencode/opencode.db"), b"").unwrap();

    let x = xdg.clone();
    let all = accounts(
        &home,
        &home,
        move |k| match k {
            "XDG_DATA_HOME" => Some(x.clone().into_os_string()),
            _ => None,
        },
        &[],
    );
    let defaults = VendorDirs::defaults(&home);
    let (vendor, inherited) = (Vendor::OpenCode, [("XDG_DATA_HOME", xdg.as_path())]);
    let mine: Vec<&Account> = all.iter().filter(|a| a.vendor == vendor).collect();
    assert_eq!(
        mine.len(),
        2,
        "{vendor}：环境变量指的那个加默认的：{mine:?}"
    );
    let default = mine.iter().find(|a| a.is_default()).expect("有默认账号");
    assert_eq!(default.dir, defaults.dir(vendor));
    let prefix = default_account_env_prefix(vendor);
    assert_eq!(
        dir_seen_by_cli(&prefix, &inherited, &home, vendor),
        defaults.dir(vendor),
        "{vendor} 默认账号的命令 `{prefix}` 被继承的变量带走了"
    );
    // 各家所有改目录的变量一起设着也一样。
    let all_set: Vec<(&str, &Path)> = vendor
        .info()
        .dir_vars
        .iter()
        .map(|v| (*v, base.as_path()))
        .collect();
    assert_eq!(
        dir_seen_by_cli(&prefix, &all_set, &home, vendor),
        defaults.dir(vendor)
    );
    assert_eq!(
        default_account_env_prefix(Vendor::OpenCode),
        "env -u OPENCODE_DB -u XDG_DATA_HOME "
    );
    assert_eq!(
        default_account_env_prefix(Vendor::Codex),
        "env -u CODEX_HOME "
    );
}

/// 同一类：非默认账号带的变量会被继承的更高优先级变量盖过时，也先清掉：OpenCode 只设了 `XDG_DATA_HOME` 的账号
/// 带它、清掉 `OPENCODE_DB`。
#[test]
fn account_command_clears_vars_that_would_override_it() {
    let base = temp_dir("core-dirs-account-env");
    let home = base.join("h");
    let elsewhere = base.join("rt");
    let x = base.join("xdg");
    let d = VendorDirs::new(&home, &home, move |k| {
        (k == "XDG_DATA_HOME").then(|| x.clone().into_os_string())
    });
    let prefix = env_prefix(&d, Vendor::OpenCode, &home);
    assert_eq!(
        prefix,
        format!(
            "env -u OPENCODE_DB XDG_DATA_HOME={} ",
            base.join("xdg").display()
        )
    );
    assert_eq!(
        dir_seen_by_cli(
            &prefix,
            &[("XDG_DATA_HOME", elsewhere.as_path())],
            &home,
            Vendor::OpenCode
        ),
        base.join("xdg/opencode/opencode.db")
    );
}

/// 审计 v0.1.4 第 3 条：账号的会话目录和登录身份要跟着同一份环境走。OpenCode 只设了 `XDG_DATA_HOME` 的账号，
/// 新标签页继承了别的 `XDG_DATA_HOME` 时，CLI 读的库和 `auth.json` 都还在这个账号下（以前只带 `OPENCODE_DB`，
/// 登录跑到新标签页那份）。
#[test]
fn account_command_carries_login_dir_with_sessions() {
    let base = temp_dir("core-dirs-login-env");
    let home = base.join("h");
    let x = base.join("xdg-work");
    let other = base.join("xdg-personal");
    let work = Account::new(
        Vendor::OpenCode,
        "工作号",
        x.join("opencode/opencode.db"),
        &home,
    );
    let prefix = account_env_prefix(&work, &home);
    let seen = env_seen_by_cli(&prefix, &[("XDG_DATA_HOME", other.as_path())]);
    let var = |k: &str| seen.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(
        var("XDG_DATA_HOME"),
        Some(x.display().to_string()),
        "{prefix}"
    );
    assert_eq!(var("OPENCODE_DB"), None);
    assert_eq!(
        dir_seen_by_cli(
            &prefix,
            &[("XDG_DATA_HOME", other.as_path())],
            &home,
            Vendor::OpenCode
        ),
        work.dir
    );

    // 库在别处（`OPENCODE_DB` 指的）：登录照会话台环境里的 `XDG_DATA_HOME`，环境里没设就清掉继承的。
    let (h, cwd) = (Path::new(H), Path::new("/launch"));
    let db = Path::new("/data/work.db");
    assert_eq!(
        VendorDirs::env_for_in(
            Vendor::OpenCode,
            db,
            h,
            cwd,
            env_of(&[("OPENCODE_DB", "/data/work.db"), ("XDG_DATA_HOME", "/xw")])
        ),
        [
            ("OPENCODE_DB", Some(db.to_path_buf())),
            ("XDG_DATA_HOME", Some(PathBuf::from("/xw")))
        ]
    );
    assert_eq!(
        VendorDirs::env_for_in(
            Vendor::OpenCode,
            db,
            h,
            cwd,
            env_of(&[("OPENCODE_DB", "/data/work.db")])
        ),
        [
            ("OPENCODE_DB", Some(db.to_path_buf())),
            ("XDG_DATA_HOME", None)
        ]
    );
}

/// 审计 v0.1.4 第 10 条：同时设了 `CODEX_HOME` 和 `XDG_DATA_HOME`（或 `QWEN_RUNTIME_DIR`）时，旧版（0.1.3）的目录名
/// 只受 `CODEX_HOME` 影响；按旧规则算才找得到那份旧标注。`7f687171` 是旧版只设 `CODEX_HOME=/data/codex` 时的名字。
#[test]
fn legacy_profile_ignores_vars_old_versions_did_not_read() {
    let h = Path::new(H);
    let legacy = |pairs: &[(&'static str, &'static str)]| {
        VendorDirs::legacy(h, Path::new("/launch"), env_of(pairs))
    };
    let both = [("CODEX_HOME", "/data/codex"), ("XDG_DATA_HOME", "/xdg")];
    assert_eq!(legacy(&both).profile(h).as_deref(), Some("7f687171"));
    assert_ne!(
        dirs(&both).profile(h).as_deref(),
        Some("7f687171"),
        "新规则算进了 XDG"
    );
    assert_eq!(
        legacy(&both).cache_dir(Path::new("/d"), h),
        Path::new("/d/profiles/7f687171")
    );
    // Qwen：旧版只看 `QWEN_HOME`。
    let qwen = [("QWEN_HOME", "/w"), ("QWEN_RUNTIME_DIR", "/rt")];
    assert_eq!(legacy(&qwen), legacy(&[("QWEN_HOME", "/w")]));
    assert_eq!(legacy(&qwen).dir(Vendor::Qwen), Path::new("/w"));
    // OpenCode：旧版的 `OPENCODE_DB` 相对路径按启动目录。
    assert_eq!(
        legacy(&[("OPENCODE_DB", "w.db")]).dir(Vendor::OpenCode),
        Path::new("/launch/w.db")
    );
    assert_eq!(legacy(&[]), VendorDirs::defaults(h));
    assert_eq!(legacy(&[("XDG_DATA_HOME", "/xdg")]).profile(h), None);
}
