use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs};

use serde::{Deserialize, Serialize};

use super::AccountSetting;

/// 会话台自己的设置，存在数据目录的 `config.json`。缺的字段取默认值。
/// 手动加的账号另放同目录的 `accounts.json`（[`ACCOUNTS_FILE`]）：0.1.3 存设置时会把它不认得的字段丢掉。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 首选终端（如 "ghostty"、"kaku"）；`None` 表示跟随当前终端。
    pub preferred_terminal: Option<String>,
    /// 全局呼出键。
    pub hotkey: String,
    /// 回收站保留天数。
    pub trash_days: u32,
    /// 列表里显示自动运行（无头）的对话；默认收进组标题的「另有 N 条后台」。
    pub show_background: bool,
    /// 托管对话闲置（没有标签页连着、等你回复或已完成、这么多分钟没有新输出）就结束它的进程；0 表示不收回。
    pub idle_reclaim_minutes: u32,
    /// 新建项目的默认上级目录（`~` 开头指家目录）：N 里只输一个名字时建在它下面，它的子目录也列进候选。
    pub projects_root: String,
    /// 顶栏「◆N 等你」慢闪（1.6 秒一次）；关掉就常亮。
    pub blink_waiting: bool,
    /// 会话台在前台时把中文等输入法切成英文键盘布局，离开时切回（终端要支持焦点上报）。
    pub auto_ascii_input: bool,
    /// 查会话台（每天最多一次，GitHub）和各家 CLI（每家 20 小时最多一次）有没有新版本，有就在顶栏提示（环境变量 `HUITAI_NO_UPDATE_CHECK=1` 也能关）。
    pub check_updates: bool,
    /// 各家 CLI 自动升级：查到新版、那家没有托管对话在跑时，按本机装法跑官方升级命令（不跨大版本、不用 sudo；
    /// 自带自动更新的几家等它自己装，新版出来 24 小时还没装上才升）。要「检查新版本」开着才有用。
    pub auto_upgrade_clis: bool,
    /// 备份全部 Claude 对话（Claude Code 会删掉超过 `cleanupPeriodDays` 天没动的）；关着只备在会话台里置顶、改名、归档过的。
    pub backup_all_claude: bool,
    /// 手动加的账号（同一家另一个数据目录）。默认位置、环境变量指的目录和 Hermes 的 profile 不用加，自动认。
    /// 全部账号存在 `accounts.json`（0.1.3 不读、不会丢）；`config.json` 里 `accounts` 那一项是用户手动管的，
    /// 读的时候照它同步（见 [`Config::load`]）。
    #[serde(skip_serializing)]
    pub accounts: Vec<AccountSetting>,
    /// `config.json` 里 `accounts` 那一项：存设置时照写回去，用户在那里加的、删的下次读时都同步进 `accounts`。
    #[serde(skip)]
    pub accounts_in_config: Vec<AccountSetting>,
    /// `accounts.json` 里解不了的条目（以后的版本加的厂商）：不用，原样写回。
    #[serde(skip)]
    pub unknown_accounts: Vec<serde_json::Value>,
}

/// 账号放在 `config.json` 同目录的这个文件里，0.1.3 不读它。
pub const ACCOUNTS_FILE: &str = "accounts.json";

#[derive(Default, Serialize, Deserialize)]
struct AccountsFile {
    version: u32,
    /// 按条解，见 [`Config::unknown_accounts`]。
    accounts: Vec<serde_json::Value>,
    /// 上次同步时 `config.json` 里 `accounts` 那一项：对比出用户在那里删了哪些。
    #[serde(default)]
    from_config: Vec<serde_json::Value>,
    /// 上次同步时 `config.json` 里别的设置：`accounts` 那一项不见了而别的设置没变，是用户删的；
    /// 别的设置也变了，多半是 0.1.3 存设置时丢的（它只在改了设置时存），账号留着。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settings: Option<serde_json::Value>,
}

/// `config.json` 的写法：设置加上 `accounts` 那一项（空的不写）。
#[derive(Serialize)]
struct ConfigFile<'a> {
    #[serde(flatten)]
    settings: &'a Config,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    accounts: &'a [AccountSetting],
}

/// 只看 `config.json` 里有没有 `accounts` 这一项。
#[derive(Deserialize)]
struct AccountsKey {
    accounts: Option<serde::de::IgnoredAny>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            preferred_terminal: None,
            hotkey: "alt+space".to_owned(),
            trash_days: 30,
            show_background: false,
            idle_reclaim_minutes: 30,
            projects_root: "~/projects".to_owned(),
            blink_waiting: true,
            auto_ascii_input: true,
            check_updates: true,
            auto_upgrade_clis: true,
            backup_all_claude: false,
            accounts: Vec::new(),
            accounts_in_config: Vec::new(),
            unknown_accounts: Vec::new(),
        }
    }
}

impl Config {
    /// 会话台数据目录：`~/Library/Application Support/huitai`。取不到 `HOME` 时为 `None`。
    pub fn data_dir() -> Option<PathBuf> {
        let home = env::var_os("HOME").filter(|h| !h.is_empty())?;
        Some(PathBuf::from(home).join("Library/Application Support/huitai"))
    }

    /// 默认配置文件路径。
    pub fn default_path() -> Option<PathBuf> {
        Self::data_dir().map(|d| d.join("config.json"))
    }

    /// 闲置多久收回；`None` 表示不收回。
    pub fn idle_reclaim(&self) -> Option<Duration> {
        (self.idle_reclaim_minutes > 0)
            .then(|| Duration::from_secs(u64::from(self.idle_reclaim_minutes) * 60))
    }

    /// 读配置和同目录的 `accounts.json`；文件不存在时取默认值。账号以 `accounts.json` 为准，再照 `config.json` 里
    /// `accounts` 那一项同步：比上次同步多出来的加上，少了的删掉。这一项整个不见了：别的设置也变了算 0.1.3 存设置
    /// 时丢的，账号留着；别的设置没变算用户删的，上次从这里同步来的账号删掉。有变化就写回 `accounts.json`；
    /// 写不出来不算错（账号还在 `config.json` 里，下次再写）。
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(with_path(e, path)),
        };
        let mut cfg: Self = match &text {
            Some(t) => serde_json::from_str(t).map_err(|e| invalid(path, "配置", e))?,
            None => Self::default(),
        };
        let in_key = text
            .as_deref()
            .and_then(|t| serde_json::from_str::<AccountsKey>(t).ok())
            .and_then(|k| k.accounts)
            .map(|_| std::mem::take(&mut cfg.accounts));
        let accounts_path = path.with_file_name(ACCOUNTS_FILE);
        let file = match fs::read_to_string(&accounts_path) {
            Ok(t) => Some(
                serde_json::from_str::<AccountsFile>(&t)
                    .map_err(|e| invalid(&accounts_path, "账号设置", e))?,
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(with_path(e, &accounts_path)),
        };
        let existed = file.is_some();
        let file = file.unwrap_or_default();
        let mut accounts = Vec::new();
        for raw in file.accounts {
            match AccountSetting::deserialize(&raw) {
                Ok(a) => accounts.push(a),
                Err(_) => cfg.unknown_accounts.push(raw),
            }
        }
        let prev_from: Vec<AccountSetting> = file
            .from_config
            .iter()
            .filter_map(|raw| AccountSetting::deserialize(raw).ok())
            .collect();
        let settings = cfg.settings_value()?;
        let new_from = match in_key {
            Some(list) => Some(list),
            None if file.settings.as_ref() == Some(&settings) => Some(Vec::new()),
            None => None,
        };
        let before = accounts.clone();
        // 设置快照只在照 `config.json` 同步过账号时推进；0.1.3 丢了 `accounts` 那一项时沿用旧的，
        // 否则下次读到同样缺这一项、设置又和快照一样，就会当成用户删的把账号清掉。
        let (from, snapshot) = match new_from {
            Some(new_from) => {
                accounts.retain(|a| new_from.contains(a) || !prev_from.contains(a));
                for a in &new_from {
                    if !accounts.contains(a) {
                        accounts.push(a.clone());
                    }
                }
                (new_from, Some(settings))
            }
            None => (prev_from.clone(), file.settings.clone()),
        };
        let changed = accounts != before || from != prev_from || snapshot != file.settings;
        cfg.accounts = accounts;
        cfg.accounts_in_config = from;
        if changed && (existed || !cfg.accounts.is_empty()) {
            let _ = cfg.save_accounts(&accounts_path, snapshot);
        }
        Ok(cfg)
    }

    /// 写配置：账号写进同目录的 `accounts.json`，`config.json` 里照旧写 `accounts` 那一项（用户在那里管的那些）。
    /// 自动建上级目录。
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| with_path(e, dir))?;
        }
        let accounts_path = path.with_file_name(ACCOUNTS_FILE);
        if !self.accounts.is_empty()
            || !self.accounts_in_config.is_empty()
            || !self.unknown_accounts.is_empty()
            || accounts_path.exists()
        {
            self.save_accounts(&accounts_path, Some(self.settings_value()?))?;
        }
        let file = ConfigFile {
            settings: self,
            accounts: &self.accounts_in_config,
        };
        let json = serde_json::to_string_pretty(&file).map_err(io::Error::other)?;
        fs::write(path, json).map_err(|e| with_path(e, path))
    }

    /// 除账号以外的设置（和 `config.json` 里写的一样），用来认出 `config.json` 被别的版本改过。
    fn settings_value(&self) -> io::Result<serde_json::Value> {
        serde_json::to_value(self).map_err(io::Error::other)
    }

    /// 写 `accounts.json`；`settings` 是记下的设置快照（见 [`AccountsFile::settings`]）。
    fn save_accounts(&self, path: &Path, settings: Option<serde_json::Value>) -> io::Result<()> {
        let values = |list: &[AccountSetting]| -> io::Result<Vec<serde_json::Value>> {
            list.iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()
                .map_err(io::Error::other)
        };
        let mut accounts = values(&self.accounts)?;
        accounts.extend(self.unknown_accounts.iter().cloned());
        let file = AccountsFile {
            version: 1,
            accounts,
            from_config: values(&self.accounts_in_config)?,
            settings,
        };
        let json = serde_json::to_string_pretty(&file).map_err(io::Error::other)?;
        // 读设置时也会写它：先写临时文件再改名，别的进程同时读到的总是完整的一份。
        let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
        let res = fs::write(&tmp, json).and_then(|()| fs::rename(&tmp, path));
        if res.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        res.map_err(|e| with_path(e, path))
    }
}

fn invalid(path: &Path, what: &str, e: serde_json::Error) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidData,
        format!("{}: {what}格式不对: {e}", path.display()),
    )
}

fn with_path(e: io::Error, path: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("huitai-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_file_gives_default() {
        let dir = temp_dir("missing");
        assert_eq!(
            Config::load(&dir.join("config.json")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("nested/config.json");
        let cfg = Config {
            preferred_terminal: Some("kaku".to_owned()),
            hotkey: "cmd+shift+space".to_owned(),
            trash_days: 7,
            show_background: true,
            idle_reclaim_minutes: 45,
            projects_root: "/work".to_owned(),
            blink_waiting: false,
            auto_ascii_input: false,
            check_updates: false,
            auto_upgrade_clis: false,
            backup_all_claude: true,
            accounts: vec![AccountSetting {
                vendor: crate::core::Vendor::Codex,
                name: "工作号".to_owned(),
                dir: "~/.codex-work".to_owned(),
            }],
            accounts_in_config: Vec::new(),
            unknown_accounts: Vec::new(),
        };
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn partial_file_fills_defaults() {
        let dir = temp_dir("partial");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, r#"{"trash_days": 3}"#).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.trash_days, 3);
        assert_eq!(cfg.hotkey, Config::default().hotkey);
        assert_eq!(cfg.projects_root, "~/projects");
        assert_eq!(cfg.idle_reclaim(), Some(Duration::from_secs(30 * 60)));
        assert!(cfg.blink_waiting, "旧配置没有这一项时默认闪");
        assert!(cfg.check_updates, "旧配置没有这一项时默认查新版本");
        assert!(
            cfg.auto_upgrade_clis,
            "旧配置没有这一项时默认自动升级各家 CLI"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn zero_idle_minutes_turns_reclaim_off() {
        let cfg = Config {
            idle_reclaim_minutes: 0,
            ..Config::default()
        };
        assert_eq!(cfg.idle_reclaim(), None);
    }

    #[test]
    fn malformed_file_is_an_error_naming_the_path() {
        let dir = temp_dir("malformed");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, "{not json").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert!(err.to_string().contains("config.json"));
        fs::remove_dir_all(&dir).unwrap();
    }

    fn work() -> AccountSetting {
        AccountSetting {
            vendor: crate::core::Vendor::Codex,
            name: "工作号".to_owned(),
            dir: "~/.codex-work".to_owned(),
        }
    }

    fn other() -> AccountSetting {
        AccountSetting {
            vendor: crate::core::Vendor::Claude,
            name: "备用".to_owned(),
            dir: "~/.claude-b".to_owned(),
        }
    }

    /// 审查退回第 1 轮的复现：照设置页提示在 config.json 里加账号，再从那里删（删一条、删整项都算），
    /// 读出来就没有了，accounts.json 也跟着删。以前只往 accounts.json 里加、不删。
    #[test]
    fn accounts_removed_in_config_json_are_removed() {
        let dir = temp_dir("remove-in-config");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(
            &path,
            r#"{"accounts": [
                {"vendor": "codex", "name": "工作号", "dir": "~/.codex-work"},
                {"vendor": "claude", "name": "备用", "dir": "~/.claude-b"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(Config::load(&path).unwrap().accounts, [work(), other()]);
        assert!(dir.join(ACCOUNTS_FILE).is_file());

        // 删一条。
        fs::write(
            &path,
            r#"{"accounts": [{"vendor": "codex", "name": "工作号", "dir": "~/.codex-work"}]}"#,
        )
        .unwrap();
        assert_eq!(Config::load(&path).unwrap().accounts, [work()]);

        // 删整个 accounts 这一项（别的设置没动）。
        fs::write(&path, "{}").unwrap();
        assert_eq!(Config::load(&path).unwrap().accounts, []);
        let text = fs::read_to_string(dir.join(ACCOUNTS_FILE)).unwrap();
        assert!(!text.contains("codex-work"), "{text}");
        assert_eq!(Config::load(&path).unwrap().accounts, []);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// 0.1.3 存设置（改了一项设置、丢掉 accounts）：账号留着，新版再存设置时把 accounts 那一项写回 config.json，
    /// 用户还能在那里删。
    #[test]
    fn old_version_dropping_the_key_keeps_accounts_and_save_writes_it_back() {
        let dir = temp_dir("v013-drop");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(
            &path,
            r#"{"trash_days": 30, "accounts": [{"vendor": "codex", "name": "工作号", "dir": "~/.codex-work"}]}"#,
        )
        .unwrap();
        Config::load(&path).unwrap();
        // 0.1.3：改了回收站天数，写回时没有 accounts。
        let v013 = Config {
            trash_days: 7,
            ..Config::default()
        };
        fs::write(&path, serde_json::to_string_pretty(&v013).unwrap()).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!((cfg.trash_days, cfg.accounts.clone()), (7, vec![work()]));

        cfg.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("codex-work"),
            "config.json 里又有 accounts：{text}"
        );
        // 写回去以后照样能在 config.json 里删。
        let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
        v.as_object_mut().unwrap().remove("accounts");
        fs::write(&path, v.to_string()).unwrap();
        assert_eq!(Config::load(&path).unwrap().accounts, []);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// 不是从 config.json 来的账号（accounts.json 里本来就有的），config.json 里删东西不影响它。
    #[test]
    fn accounts_not_from_config_json_stay() {
        let dir = temp_dir("not-from-config");
        let path = dir.join("config.json");
        Config {
            accounts: vec![work()],
            ..Config::default()
        }
        .save(&path)
        .unwrap();
        fs::write(
            &path,
            r#"{"accounts": [{"vendor": "claude", "name": "备用", "dir": "~/.claude-b"}]}"#,
        )
        .unwrap();
        assert_eq!(Config::load(&path).unwrap().accounts, [work(), other()]);
        fs::write(&path, r#"{"accounts": []}"#).unwrap();
        assert_eq!(Config::load(&path).unwrap().accounts, [work()]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn data_dir_is_under_application_support() {
        if let Some(dir) = Config::data_dir() {
            assert!(dir.ends_with("Library/Application Support/huitai"));
        }
    }
}
