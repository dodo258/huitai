//! 多账号：一家几个数据目录各一个来源。认出本机有哪些账号（[`accounts`]），每个账号的来源包一层
//! [`AccountSource`]：读出来的对话标上账号、id 加账号前缀，交回给厂商适配器时去掉前缀。适配器本身不知道有账号。

use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::antigravity;
use super::{
    Command, Counted, Delivery, Moment, PreviewMessage, Progress, Saved, Scan, Source, Transcript,
    TrashEntry,
};
use crate::core::{Account, AccountSetting, Session, SessionId, Vendor, VendorDirs};

/// 默认位置那个账号的显示名。
pub const DEFAULT_NAME: &str = "默认";

/// 本机各家的账号，顺序按 [`Vendor::ALL`]，每家第一个是「主账号」（`n` 新建用它）：
/// 1. 环境变量指的目录（没设就是家目录下的默认位置）；
/// 2. 设了环境变量时，家目录下的默认位置（在的话）；
/// 3. 设置里手动加的（`~` 开头指家目录，相对路径按家目录）；
/// 4. 自动发现：Hermes 的 `profiles/<名>/`（上面几个 Hermes 目录下的）；Antigravity App 的 `~/.gemini/antigravity`（有摘要库的话）。
///
/// 数据根目录（适配器真正读的那层，见 [`data_root`]）按 canonical 路径去重，先出现的留下：
/// cc-switch、claude-swap 这类工具会把各账号的 `projects/` 链回 `~/.claude/projects`，不去重就每条列两遍。
pub fn accounts(
    home: &Path,
    cwd: &Path,
    env: impl Fn(&str) -> Option<OsString>,
    settings: &[AccountSetting],
) -> Vec<Account> {
    let dirs = VendorDirs::new(home, cwd, env);
    let defaults = VendorDirs::defaults(home);
    let mut out: Vec<Account> = Vec::new();
    let mut seen: HashSet<(Vendor, PathBuf)> = HashSet::new();
    let mut push = |out: &mut Vec<Account>, vendor: Vendor, name: String, dir: PathBuf| {
        let root = data_root(vendor, &dir);
        let real = root.canonicalize().unwrap_or(root);
        if seen.insert((vendor, real)) {
            out.push(Account::new(vendor, name, dir, home));
        }
    };
    for v in Vendor::ALL {
        let named = |dir: &Path| {
            settings
                .iter()
                .find(|s| s.vendor == v && setting_dir(s, home) == dir)
                .map(|s| s.name.clone())
        };
        let primary = dirs.dir(v).to_path_buf();
        let default = defaults.dir(v).to_path_buf();
        let name = named(&primary).unwrap_or_else(|| {
            if primary == default {
                DEFAULT_NAME.to_owned()
            } else {
                dir_name(&primary)
            }
        });
        push(&mut out, v, name, primary.clone());
        if primary != default && default.exists() {
            let name = named(&default).unwrap_or_else(|| DEFAULT_NAME.to_owned());
            push(&mut out, v, name, default.clone());
        }
        for s in settings.iter().filter(|s| s.vendor == v) {
            push(&mut out, v, s.name.clone(), setting_dir(s, home));
        }
        if v == Vendor::Hermes {
            let homes: Vec<PathBuf> = out
                .iter()
                .filter(|a| a.vendor == v)
                .map(|a| a.dir.clone())
                .collect();
            for (name, dir) in homes.iter().flat_map(|h| hermes_profiles(h)) {
                push(&mut out, v, name, dir);
            }
        }
        // Antigravity App 和 `agy` 各写各的目录，格式相同（见 `AntigravitySource`）：App 的那份当一个账号列出来。
        if v == Vendor::Antigravity {
            let app = home.join(ANTIGRAVITY_APP_DIR);
            if app.join(antigravity::SUMMARIES_DB).is_file() {
                push(&mut out, v, ANTIGRAVITY_APP_NAME.to_owned(), app);
            }
        }
    }
    out
}

/// Antigravity App 的数据目录（在家目录下）和它当账号时的显示名。
const ANTIGRAVITY_APP_DIR: &str = ".gemini/antigravity";
const ANTIGRAVITY_APP_NAME: &str = "App";

/// 设置里账号目录的绝对路径（规则同环境变量，相对路径按家目录）。
pub fn setting_dir(s: &AccountSetting, home: &Path) -> PathBuf {
    VendorDirs::dir_from_value(s.vendor, Path::new(&s.dir), home, home)
}

/// 目录名当显示名（`~/.claude-work` → `claude-work`）。
fn dir_name(dir: &Path) -> String {
    let n = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.to_string_lossy().into_owned());
    n.trim_start_matches('.').to_owned()
}

/// Hermes 的 profile：`<HERMES_HOME>/profiles/<名>/`，每个是一个完整的 `HERMES_HOME`（各有一个 `state.db`）。
/// 按名字排；读不了目录就当没有。
fn hermes_profiles(hermes_home: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(hermes_home.join("profiles")) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().join("state.db").is_file())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            (!name.starts_with('.')).then(|| (name, e.path()))
        })
        .collect();
    out.sort();
    out
}

/// 这家的适配器真正读的那层目录（去重用）：Claude 的 `projects`、Codex 和 pi、Kimi Code 的 `sessions`，
/// 别的家就是数据目录本身。
pub fn data_root(vendor: Vendor, dir: &Path) -> PathBuf {
    match vendor {
        Vendor::Claude => dir.join("projects"),
        Vendor::Codex | Vendor::Pi | Vendor::Kimi => dir.join("sessions"),
        Vendor::Hermes | Vendor::Gemini | Vendor::OpenCode | Vendor::Qwen | Vendor::Antigravity => {
            dir.to_path_buf()
        }
    }
}

/// 一个账号的来源：包着厂商适配器，进出时加、去账号前缀。默认账号（短名是空串）原样转发。
pub struct AccountSource {
    inner: Box<dyn Source>,
    key: String,
}

impl AccountSource {
    pub fn new(inner: Box<dyn Source>, account: &Account) -> Self {
        Self {
            inner,
            key: account.key.clone(),
        }
    }

    /// 读出来的对话：标上账号、id 加前缀。
    fn tag(&self, mut s: Session) -> Session {
        if !self.key.is_empty() {
            s.id = SessionId::in_account(&self.key, s.id.as_str());
            s.account.clone_from(&self.key);
        }
        s
    }

    /// 交给适配器的对话：id 换回厂商自己的。
    fn native<'a>(&self, s: &'a Session) -> Cow<'a, Session> {
        let (key, native) = s.id.split_account();
        if key.is_empty() {
            return Cow::Borrowed(s);
        }
        let mut s = s.clone();
        s.id = SessionId::from(native);
        Cow::Owned(s)
    }
}

impl Source for AccountSource {
    fn vendor(&self) -> Vendor {
        self.inner.vendor()
    }

    fn account(&self) -> &str {
        &self.key
    }

    fn parse_version(&self) -> u32 {
        self.inner.parse_version()
    }

    fn scan(&self) -> Scan {
        let mut scan = self.inner.scan();
        scan.sessions = scan.sessions.into_iter().map(|s| self.tag(s)).collect();
        scan
    }

    fn load_preview(&self, session: &Session, n: usize) -> io::Result<Vec<PreviewMessage>> {
        self.inner.load_preview(&self.native(session), n)
    }

    fn load_transcript(&self, session: &Session) -> io::Result<Transcript> {
        self.inner.load_transcript(&self.native(session))
    }

    fn load_moment(&self, session: &Session) -> io::Result<Moment> {
        self.inner.load_moment(&self.native(session))
    }

    fn moment_files(&self, session: &Session) -> Vec<PathBuf> {
        self.inner.moment_files(&self.native(session))
    }

    fn count_messages(&self, session: &Session) -> io::Result<usize> {
        self.inner.count_messages(&self.native(session))
    }

    fn count_since(&self, session: &Session, prev: Option<Progress>) -> io::Result<Counted> {
        self.inner.count_since(&self.native(session), prev)
    }

    fn resume_cmd(&self, session: &Session) -> Command {
        self.inner.resume_cmd(&self.native(session))
    }

    fn resume_with_message_cmd(&self, session: &Session, message: &str) -> Command {
        self.inner
            .resume_with_message_cmd(&self.native(session), message)
    }

    fn message_delivery(&self, session: &Session) -> Delivery {
        self.inner.message_delivery(&self.native(session))
    }

    fn new_cmd(&self, cwd: &Path) -> Command {
        self.inner.new_cmd(cwd)
    }

    fn created_at(&self, session: &Session) -> Option<SystemTime> {
        self.inner.created_at(&self.native(session))
    }

    fn trash(&self, session: &Session) -> io::Result<TrashEntry> {
        self.inner.trash(&self.native(session))
    }

    fn restore(&self, entry: &TrashEntry) -> io::Result<()> {
        self.inner.restore(entry)
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        self.inner.watch_roots()
    }

    fn affects(&self, path: &Path) -> bool {
        self.inner.affects(path)
    }

    fn session_files(&self) -> Option<io::Result<Vec<PathBuf>>> {
        self.inner.session_files()
    }

    fn read_files(&self, paths: &[PathBuf]) -> Vec<io::Result<Option<Session>>> {
        self.inner
            .read_files(paths)
            .into_iter()
            .map(|r| r.map(|s| s.map(|s| self.tag(s))))
            .collect()
    }

    fn stand_in(&self, gone: &Path) -> Option<PathBuf> {
        self.inner.stand_in(gone)
    }

    fn back_up(&self, session: &Session, check: bool) -> io::Result<Saved> {
        self.inner.back_up(&self.native(session), check)
    }

    fn put_back(&self, session: &Session) -> io::Result<PathBuf> {
        self.inner.put_back(&self.native(session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(vendor: Vendor, name: &str, dir: &str) -> AccountSetting {
        AccountSetting {
            vendor,
            name: name.to_owned(),
            dir: dir.to_owned(),
        }
    }

    #[test]
    fn primary_then_settings_with_names() {
        let h = Path::new("/nonexistent-huitai-home");
        let list = accounts(
            h,
            h,
            |k| (k == "CODEX_HOME").then(|| "/data/codex".into()),
            &[
                setting(Vendor::Claude, "工作号", "~/.claude-work"),
                setting(Vendor::Codex, "主力", "/data/codex/"),
            ],
        );
        let of = |v: Vendor| -> Vec<(&str, &Path, bool)> {
            list.iter()
                .filter(|a| a.vendor == v)
                .map(|a| (a.name.as_str(), a.dir.as_path(), a.is_default()))
                .collect()
        };
        assert_eq!(
            of(Vendor::Claude),
            [
                ("默认", h.join(".claude").as_path(), true),
                ("工作号", h.join(".claude-work").as_path(), false),
            ]
        );
        // 环境变量指的目录在设置里起过名就用那个名字；同一个目录只列一次。默认位置不在就不列。
        assert_eq!(
            of(Vendor::Codex),
            [("主力", Path::new("/data/codex"), false)]
        );
        assert_eq!(of(Vendor::Pi).len(), 1);
    }

    #[test]
    fn dir_names_for_unnamed_env_dirs() {
        assert_eq!(dir_name(Path::new("/h/.claude-b")), "claude-b");
        assert_eq!(dir_name(Path::new("/data/codex")), "codex");
    }
}
