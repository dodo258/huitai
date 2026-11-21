use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs};

use serde::{Deserialize, Serialize};

/// 会话台自己的设置，存在数据目录的 `config.json`。缺的字段取默认值。
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
    /// 每天最多查一次 GitHub 上有没有新版本，有就在顶栏提示（环境变量 `HUITAI_NO_UPDATE_CHECK=1` 也能关）。
    pub check_updates: bool,
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

    /// 读配置；文件不存在时返回默认值。
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(with_path(e, path)),
        };
        serde_json::from_str(&text).map_err(|e| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("{}: 配置格式不对: {e}", path.display()),
            )
        })
    }

    /// 写配置，自动建上级目录。
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| with_path(e, dir))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(path, json).map_err(|e| with_path(e, path))
    }
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

    #[test]
    fn data_dir_is_under_application_support() {
        if let Some(dir) = Config::data_dir() {
            assert!(dir.ends_with("Library/Application Support/huitai"));
        }
    }
}
