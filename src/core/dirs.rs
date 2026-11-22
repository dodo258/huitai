use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::Vendor;

/// 各家 CLI 存数据的目录。默认在家目录下；用户用环境变量改过的（多账号、放别的盘），跟着 CLI 自己的规则走：
/// 变量名和默认位置见 [`crate::core::VendorInfo`]（Claude Code 认 `CLAUDE_CONFIG_DIR`，Codex 认 `CODEX_HOME`，
/// pi 认 `PI_CODING_AGENT_DIR`，Hermes 认 `HERMES_HOME`，Gemini CLI 认 `GEMINI_CLI_HOME`——它换的是家目录）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorDirs {
    /// 按 [`Vendor::index`] 放。
    dirs: [PathBuf; Vendor::COUNT],
}

impl VendorDirs {
    /// `env` 取环境变量；空值当没设。值里开头的 `~/` 换成家目录，相对路径按 `cwd`（启动目录）转成绝对路径，
    /// 末尾的 `/` 和中间的 `.` 去掉：
    /// 记进回收站、交给守护进程的都是绝对路径，换个目录启动也指向同一处。
    pub fn new(home: &Path, cwd: &Path, env: impl Fn(&str) -> Option<OsString>) -> Self {
        let pick = |v: Vendor| {
            let info = v.info();
            env(info.dir_env)
                .filter(|v| !v.is_empty())
                // 按路径分段重新拼一遍：末尾的 `/`、中间的 `.` 不让同一个目录变成两组。
                .map(|v| {
                    let dir: PathBuf = cwd
                        .join(expand_home(home, PathBuf::from(v)))
                        .components()
                        .collect();
                    if info.dir_env_is_home {
                        dir.join(info.default_dir)
                    } else {
                        dir
                    }
                })
                .unwrap_or_else(|| home.join(info.default_dir))
        };
        Self {
            dirs: Vendor::ALL.map(pick),
        }
    }

    /// 这家的数据目录。
    pub fn dir(&self, vendor: Vendor) -> &Path {
        &self.dirs[vendor.index()]
    }

    /// 按当前进程的环境变量和工作目录。
    pub fn from_env(home: &Path) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| home.to_path_buf());
        Self::new(home, &cwd, |k| std::env::var_os(k))
    }

    /// 都在家目录下的默认位置（没设环境变量）。
    pub fn defaults(home: &Path) -> Self {
        Self::new(home, home, |_| None)
    }

    /// 这家的目录和对应的环境变量名。
    pub fn of(&self, vendor: Vendor) -> (&'static str, &Path) {
        (vendor.info().dir_env, self.dir(vendor))
    }

    /// 这家不在默认位置时，启动它的 CLI 要显式带上的环境变量和值：交给守护进程的命令不一定继承同一份环境
    /// （守护进程的环境来自第一次拉起它的那个终端）。在默认位置就不带。
    /// 变量指家目录的（[`crate::core::VendorInfo::dir_env_is_home`]），值是数据目录往上退掉 `default_dir` 那几层。
    pub fn override_of(&self, vendor: Vendor, home: &Path) -> Option<(&'static str, &Path)> {
        let (key, dir) = self.of(vendor);
        if dir == Self::defaults(home).of(vendor).1 {
            return None;
        }
        let info = vendor.info();
        let value = if info.dir_env_is_home {
            dir.ancestors()
                .nth(Path::new(info.default_dir).components().count())?
        } else {
            dir
        };
        Some((key, value))
    }

    /// 有哪家不在默认位置时，这组目录的短名字（8 位十六进制，同一组目录永远一样）：
    /// 列表缓存、额度缓存按它分开放，换一组目录启动不会看到另一组的对话和额度。都在默认位置是 `None`。
    pub fn profile(&self, home: &Path) -> Option<String> {
        if *self == Self::defaults(home) {
            return None;
        }
        // FNV-1a：std 的哈希不保证跨版本稳定，这里要跨版本不变。
        // 最早的三家总是算进去，后来加的只在改过目录时才算：老用户的名字不变，缓存不用重建。
        let defaults = Self::defaults(home);
        let mut h: u32 = 0x811c_9dc5;
        let counted = Vendor::ALL.into_iter().filter(|&v| {
            matches!(v, Vendor::Claude | Vendor::Codex | Vendor::Pi)
                || self.dir(v) != defaults.dir(v)
        });
        for p in counted.map(|v| self.dir(v)) {
            for b in p.as_os_str().as_encoded_bytes().iter().chain(&[0]) {
                h ^= u32::from(*b);
                h = h.wrapping_mul(0x0100_0193);
            }
        }
        Some(format!("{h:08x}"))
    }

    /// 会话台自己的缓存（列表缓存、额度缓存）放哪：都在默认位置就是数据目录本身，
    /// 否则是数据目录下 `profiles/<profile>`，两组目录互不串。
    pub fn cache_dir(&self, data_dir: &Path, home: &Path) -> PathBuf {
        match self.profile(home) {
            Some(p) => data_dir.join("profiles").join(p),
            None => data_dir.to_path_buf(),
        }
    }

    /// 把写成「家目录下默认位置」的相对路径（`.claude/…`、`.codex/…`、`.pi/agent/…`）换到实际目录；
    /// 别的相对路径照旧接在家目录后面。
    pub fn resolve(&self, home: &Path, rel: &str) -> PathBuf {
        let rel = Path::new(rel);
        for v in Vendor::ALL {
            if let Ok(rest) = rel.strip_prefix(v.info().default_dir) {
                return self.dir(v).join(rest);
            }
        }
        home.join(rel)
    }
}

fn expand_home(home: &Path, p: PathBuf) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.into())
    }

    #[test]
    fn defaults_under_home() {
        let d = VendorDirs::new(Path::new("/h"), Path::new("/cwd"), env_of(&[]));
        assert_eq!(d.dir(Vendor::Claude), Path::new("/h/.claude"));
        assert_eq!(d.dir(Vendor::Codex), Path::new("/h/.codex"));
        assert_eq!(d.dir(Vendor::Pi), Path::new("/h/.pi/agent"));
        assert_eq!(d.dir(Vendor::Hermes), Path::new("/h/.hermes"));
        assert_eq!(d.dir(Vendor::Gemini), Path::new("/h/.gemini/tmp"));
        assert_eq!(
            d.resolve(Path::new("/h"), ".pi/agent/sessions"),
            Path::new("/h/.pi/agent/sessions")
        );
    }

    #[test]
    fn env_overrides_and_tilde() {
        let d = VendorDirs::new(
            Path::new("/h"),
            Path::new("/cwd"),
            env_of(&[
                ("CLAUDE_CONFIG_DIR", "~/work/.claude-b"),
                ("CODEX_HOME", "/data/codex"),
                ("PI_CODING_AGENT_DIR", "/data/pi"),
            ]),
        );
        let h = Path::new("/h");
        assert_eq!(
            d.resolve(h, ".claude/projects"),
            Path::new("/h/work/.claude-b/projects")
        );
        assert_eq!(
            d.resolve(h, ".codex/sessions"),
            Path::new("/data/codex/sessions")
        );
        assert_eq!(
            d.resolve(h, ".pi/agent/sessions"),
            Path::new("/data/pi/sessions")
        );
        assert_eq!(d.resolve(h, "other/x"), Path::new("/h/other/x"));
    }

    #[test]
    fn empty_value_means_unset() {
        let d = VendorDirs::new(
            Path::new("/h"),
            Path::new("/cwd"),
            env_of(&[("CODEX_HOME", "")]),
        );
        assert_eq!(d.dir(Vendor::Codex), Path::new("/h/.codex"));
    }

    #[test]
    fn prefix_must_be_whole_component() {
        let d = VendorDirs::new(
            Path::new("/h"),
            Path::new("/cwd"),
            env_of(&[("CLAUDE_CONFIG_DIR", "/c")]),
        );
        assert_eq!(
            d.resolve(Path::new("/h"), ".claude-other/x"),
            Path::new("/h/.claude-other/x")
        );
    }

    #[test]
    fn relative_value_is_resolved_against_launch_dir() {
        let d = VendorDirs::new(
            Path::new("/h"),
            Path::new("/work/a"),
            env_of(&[("PI_CODING_AGENT_DIR", ".pi-profile")]),
        );
        assert_eq!(d.dir(Vendor::Pi), Path::new("/work/a/.pi-profile"));
    }

    #[test]
    fn overrides_and_profile_only_when_moved() {
        let h = Path::new("/h");
        let plain = VendorDirs::new(h, h, env_of(&[]));
        assert_eq!(plain.profile(h), None);
        assert_eq!(plain.override_of(Vendor::Codex, h), None);

        let moved = VendorDirs::new(h, h, env_of(&[("CODEX_HOME", "/data/codex")]));
        assert_eq!(
            moved.override_of(Vendor::Codex, h),
            Some(("CODEX_HOME", Path::new("/data/codex")))
        );
        assert_eq!(moved.override_of(Vendor::Claude, h), None);
        let key = moved.profile(h).expect("改过目录就有名字");
        assert_eq!(key.len(), 8);
        // 同一组目录永远同一个名字，换一组就不一样。
        assert_eq!(moved.profile(h), Some(key.clone()));
        let other = VendorDirs::new(h, h, env_of(&[("CODEX_HOME", "/data/codex-b")]));
        assert_ne!(other.profile(h), Some(key));
        // 显式写成默认位置，等于没改。
        let same = VendorDirs::new(h, h, env_of(&[("CLAUDE_CONFIG_DIR", "/h/.claude")]));
        assert_eq!(same.profile(h), None);
    }

    /// 名字是缓存目录名：加了新厂商以后，只改过最早三家目录的老用户名字不变（值按加 Hermes 之前的算法算出），
    /// 只改了新厂商目录的也有自己的名字。
    #[test]
    fn profile_name_is_stable_across_new_vendors() {
        let h = Path::new("/h");
        let moved = VendorDirs::new(h, h, env_of(&[("CODEX_HOME", "/data/codex")]));
        assert_eq!(moved.profile(h).as_deref(), Some("7f687171"));
        let hermes = VendorDirs::new(h, h, env_of(&[("HERMES_HOME", "/data/hermes")]));
        assert_eq!(hermes.dir(Vendor::Hermes), Path::new("/data/hermes"));
        let key = hermes.profile(h).expect("改过 Hermes 目录也算");
        assert_ne!(Some(key), moved.profile(h));
        assert_eq!(
            hermes.resolve(h, ".hermes/state.db"),
            Path::new("/data/hermes/state.db")
        );
    }

    /// `GEMINI_CLI_HOME` 换的是 Gemini CLI 的家目录：数据在它下面的 `.gemini/tmp`，
    /// 命令前带的还是变量原来的值。
    #[test]
    fn gemini_variable_names_a_home() {
        let h = Path::new("/h");
        let d = VendorDirs::new(h, Path::new("/work"), env_of(&[("GEMINI_CLI_HOME", "g2/")]));
        assert_eq!(d.dir(Vendor::Gemini), Path::new("/work/g2/.gemini/tmp"));
        assert_eq!(
            d.override_of(Vendor::Gemini, h),
            Some(("GEMINI_CLI_HOME", Path::new("/work/g2")))
        );
        assert!(d.profile(h).is_some());
        // 指回家目录本身就是默认位置。
        let same = VendorDirs::new(h, h, env_of(&[("GEMINI_CLI_HOME", "/h")]));
        assert_eq!(same.override_of(Vendor::Gemini, h), None);
        assert_eq!(same.profile(h), None);
    }

    #[test]
    fn trailing_slash_is_the_same_dir() {
        let h = Path::new("/h");
        let a = VendorDirs::new(h, h, env_of(&[("CODEX_HOME", "/data/codex")]));
        let b = VendorDirs::new(h, h, env_of(&[("CODEX_HOME", "/data/codex/")]));
        let c = VendorDirs::new(h, h, env_of(&[("CODEX_HOME", "/data/./codex")]));
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a.profile(h), b.profile(h));
    }
}
