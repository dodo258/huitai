//! 会话台设置：手动加的账号放在 0.1.3 不读的 `accounts.json`（审计 v0.1.4 第 9 条）。

use std::fs;
use std::path::{Path, PathBuf};

use huitai::core::{AccountSetting, Config, Vendor};

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("huitai-config-it-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn work() -> AccountSetting {
    AccountSetting {
        vendor: Vendor::Codex,
        name: "工作号".to_owned(),
        dir: "~/.codex-work".to_owned(),
    }
}

/// 0.1.3 存设置：读进它认得的字段，原样写回，不认得的（`accounts`）就丢了。
fn save_as_v013(path: &Path) {
    let mut v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    v.as_object_mut().unwrap().remove("accounts");
    v["trash_days"] = 9.into();
    fs::write(path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
}

/// 新版存的账号，0.1.3 存一次设置后还在。
#[test]
fn accounts_survive_an_old_version_saving_settings() {
    let dir = temp_dir("v013-save");
    let path = dir.join("config.json");
    let cfg = Config {
        accounts: vec![work()],
        ..Config::default()
    };
    cfg.save(&path).unwrap();
    save_as_v013(&path);
    let back = Config::load(&path).unwrap();
    assert_eq!(back.trash_days, 9);
    assert_eq!(back.accounts, [work()]);
}

/// 分文件之前的新版把账号写在 `config.json` 里：读的时候搬进 `accounts.json`，之后 0.1.3 存设置也不丢。
#[test]
fn accounts_in_config_json_are_moved_out() {
    let dir = temp_dir("v013-migrate");
    let path = dir.join("config.json");
    fs::write(
        &path,
        r#"{"trash_days": 3, "accounts": [{"vendor": "codex", "name": "工作号", "dir": "~/.codex-work"}]}"#,
    )
    .unwrap();
    assert_eq!(Config::load(&path).unwrap().accounts, [work()]);
    assert!(dir.join("accounts.json").is_file());
    save_as_v013(&path);
    assert_eq!(Config::load(&path).unwrap().accounts, [work()]);
}

/// `accounts.json` 里以后的版本加的厂商解不了：别的账号照读，那条原样写回。
#[test]
fn unknown_vendor_accounts_are_kept() {
    let dir = temp_dir("unknown-vendor");
    let path = dir.join("config.json");
    fs::write(
        dir.join("accounts.json"),
        r#"{"version": 1, "accounts": [
            {"vendor": "codex", "name": "工作号", "dir": "~/.codex-work"},
            {"vendor": "futurecli", "name": "新家", "dir": "~/.future"}
        ]}"#,
    )
    .unwrap();
    let cfg = Config::load(&path).unwrap();
    assert_eq!(cfg.accounts, [work()]);
    cfg.save(&path).unwrap();
    let text = fs::read_to_string(dir.join("accounts.json")).unwrap();
    assert!(text.contains("futurecli"), "{text}");
    assert_eq!(Config::load(&path).unwrap().accounts, [work()]);
}

/// 已经有 `accounts.json` 后又在 `config.json` 里手动加账号（设置页的提示就是这么说的）：照样认，并搬进 `accounts.json`。
#[test]
fn accounts_added_by_hand_in_config_json_are_merged() {
    let dir = temp_dir("hand-added");
    let path = dir.join("config.json");
    Config {
        accounts: vec![work()],
        ..Config::default()
    }
    .save(&path)
    .unwrap();
    let extra = AccountSetting {
        vendor: Vendor::Claude,
        name: "备用".to_owned(),
        dir: "~/.claude-b".to_owned(),
    };
    let mut v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    v["accounts"] = serde_json::json!([extra.clone()]);
    fs::write(&path, v.to_string()).unwrap();
    assert_eq!(
        Config::load(&path).unwrap().accounts,
        [work(), extra.clone()]
    );
    save_as_v013(&path);
    assert_eq!(Config::load(&path).unwrap().accounts, [work(), extra]);
}
