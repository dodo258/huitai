use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::Vendor;

/// 各家 CLI 存数据的目录。默认在家目录下；用户用环境变量改过的（多账号、放别的盘），跟着 CLI 自己的规则走：
/// 变量名和默认位置见 [`crate::core::VendorInfo`]（Claude Code 认 `CLAUDE_CONFIG_DIR`，Codex 认 `CODEX_HOME`，
/// pi 认 `PI_CODING_AGENT_DIR`，Hermes 认 `HERMES_HOME`，Gemini CLI 认 `GEMINI_CLI_HOME`——它换的是家目录）。
/// OpenCode 还有别的变量参与，规则照上游源码，见 [`VendorDirs::new`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorDirs {
    /// 按 [`Vendor::index`] 放（不再支持的家也占一格，见 [`Vendor::SLOTS`]）。
    dirs: [PathBuf; Vendor::SLOTS],
}

impl VendorDirs {
    /// `env` 取环境变量；空值当没设。值里开头的 `~/` 换成家目录，相对路径按 `cwd`（启动目录）转成绝对路径，
    /// 末尾的 `/` 和中间的 `.` 去掉：
    /// 记进回收站、交给守护进程的都是绝对路径，换个目录启动也指向同一处。
    ///
    /// OpenCode 另有规则（照上游源码 1.19.6 `database/database.ts`、`global.ts`）：数据目录是 `$XDG_DATA_HOME/opencode`
    /// （没设是 `~/.local/share/opencode`）；`OPENCODE_DB` 是绝对路径就用它，相对路径接在数据目录后面，不按启动目录。
    pub fn new(home: &Path, cwd: &Path, env: impl Fn(&str) -> Option<OsString>) -> Self {
        // 不认改目录变量的家（`dir_env` 是空串）一律在默认位置。
        let var = |k: &str| {
            (!k.is_empty())
                .then(|| env(k))
                .flatten()
                .filter(|v| !v.is_empty())
        };
        let pick = |v: Vendor| match v {
            Vendor::OpenCode => {
                let data = var(XDG_DATA_HOME)
                    .map(|x| absolute(home, cwd, Path::new(&x)))
                    .unwrap_or_else(|| home.join(".local/share"))
                    .join("opencode");
                let db = var(v.info().dir_env).unwrap_or_else(|| "opencode.db".into());
                // 绝对路径 `join` 后整个换掉，相对路径接在数据目录后面。
                absolute(home, &data, Path::new(&db))
            }
            _ => var(v.info().dir_env)
                .map(|value| Self::dir_from_value(v, Path::new(&value), home, cwd))
                .unwrap_or_else(|| home.join(v.info().default_dir)),
        };
        Self {
            dirs: Vendor::EVERY.map(pick),
        }
    }

    /// 0.1.3 及以前（多账号之前）按同一份环境算出的各家目录：每家只看 `dir_env`，相对路径都按启动目录
    /// （OpenCode 不看 `XDG_DATA_HOME`；Qwen Code 当时还支持，照样占一格）。只用来找那时的旧标注
    /// （[`VendorDirs::cache_dir`]），算法不能跟着新规则变。
    pub fn legacy(home: &Path, cwd: &Path, env: impl Fn(&str) -> Option<OsString>) -> Self {
        let pick = |v: Vendor| {
            let key = v.info().dir_env;
            (!key.is_empty())
                .then(|| env(key))
                .flatten()
                .filter(|value| !value.is_empty())
                .map(|value| Self::dir_from_value(v, Path::new(&value), home, cwd))
                .unwrap_or_else(|| home.join(v.info().default_dir))
        };
        Self {
            dirs: Vendor::EVERY.map(pick),
        }
    }

    /// 同 [`VendorDirs::legacy`]，按当前进程的环境变量和工作目录。
    pub fn legacy_from_env(home: &Path) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| home.to_path_buf());
        Self::legacy(home, &cwd, |k| std::env::var_os(k))
    }

    /// 换掉一家的数据目录（读某个账号的额度时，那家换成那个账号的目录）。
    pub fn with_dir(mut self, vendor: Vendor, dir: PathBuf) -> Self {
        self.dirs[vendor.index()] = dir;
        self
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

    /// 这家不在默认位置时，`dir_env` 要设成的值（[`VendorDirs::env_for`] 里设它的那一项；Claude 额度按它找钥匙串条目）。
    pub fn override_of(&self, vendor: Vendor, home: &Path) -> Option<(&'static str, PathBuf)> {
        let key = vendor.info().dir_env;
        Self::env_for(vendor, self.dir(vendor), home)
            .into_iter()
            .find_map(|(k, v)| (k == key).then_some(v).flatten().map(|v| (k, v)))
    }

    /// 数据目录 `dir`（一个账号）不在默认位置时，启动这家 CLI 要怎么设这家每个改目录的变量
    /// （[`crate::core::VendorInfo::dir_vars`]，按它的顺序）：交给守护进程的命令不一定继承同一份环境
    /// （守护进程的环境来自第一次拉起它的那个终端），同一家几个账号也各用各的目录。在默认位置是空的。
    /// 账号是按会话台自己的环境认出来的，所以按这份环境（[`VendorDirs::env_for_in`]）补全会话目录以外的那几个
    /// （OpenCode 的登录在 `XDG_DATA_HOME` 下）。
    pub fn env_for(vendor: Vendor, dir: &Path, home: &Path) -> Vec<EnvAssign> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| home.to_path_buf());
        Self::env_for_in(vendor, dir, home, &cwd, |k| std::env::var_os(k))
    }

    /// 同 [`VendorDirs::env_for`]，会话台自己的环境由 `env`、启动目录由 `cwd` 给。规则：
    /// - 只有一个变量的家：设成指这个目录的值（变量指家目录的，值是数据目录往上退掉 `default_dir` 那几层）。
    /// - OpenCode：库是 `<X>/opencode/opencode.db` 的设 `XDG_DATA_HOME=<X>`、清掉 `OPENCODE_DB`（登录和库一起搬）；
    ///   别的库路径设 `OPENCODE_DB`，`XDG_DATA_HOME` 照会话台环境里的（没设就清掉）。
    pub fn env_for_in(
        vendor: Vendor,
        dir: &Path,
        home: &Path,
        cwd: &Path,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Vec<EnvAssign> {
        let info = vendor.info();
        // CLI 不认改目录的变量（Antigravity）：带什么都没用，命令照默认位置起。
        if dir == home.join(info.default_dir) || info.dir_env.is_empty() {
            return Vec::new();
        }
        let at = |k: &str| {
            env(k)
                .filter(|v| !v.is_empty())
                .map(|v| absolute(home, cwd, Path::new(&v)))
        };
        match vendor {
            Vendor::OpenCode => {
                // 会话台环境里 `OPENCODE_DB` 指的就是这个库：登录照这份环境的 `XDG_DATA_HOME`。
                let from_db_var =
                    at(info.dir_env).is_some() && Self::new(home, cwd, &env).dir(vendor) == dir;
                match dir.ancestors().nth(2) {
                    Some(xdg) if !from_db_var && dir.ends_with(OPENCODE_DB_UNDER_XDG) => {
                        vec![
                            (info.dir_env, None),
                            (XDG_DATA_HOME, Some(xdg.to_path_buf())),
                        ]
                    }
                    _ => vec![
                        (info.dir_env, Some(dir.to_path_buf())),
                        (XDG_DATA_HOME, at(XDG_DATA_HOME)),
                    ],
                }
            }
            _ => {
                let value = if info.dir_env_is_home {
                    match dir
                        .ancestors()
                        .nth(Path::new(info.default_dir).components().count())
                    {
                        Some(v) => v,
                        None => return Vec::new(),
                    }
                } else {
                    dir
                };
                vec![(info.dir_env, Some(value.to_path_buf()))]
            }
        }
    }

    /// 环境变量的值换成数据目录：`~/` 换成家目录，相对路径按 `cwd` 转成绝对路径，末尾的 `/`、中间的 `.` 去掉；
    /// 变量指家目录的接上 `default_dir`。设置里手动加的账号目录也按这个规则。
    pub fn dir_from_value(vendor: Vendor, value: &Path, home: &Path, cwd: &Path) -> PathBuf {
        let dir = absolute(home, cwd, value);
        let info = vendor.info();
        if info.dir_env_is_home {
            dir.join(info.default_dir)
        } else {
            dir
        }
    }

    /// 有哪家不在默认位置时，这组目录的短名字（8 位十六进制，同一组目录永远一样）：多账号之前的版本（到 0.1.3）
    /// 按它分开放列表缓存和改名置顶，现在只用来找那份旧标注（[`VendorDirs::cache_dir`]），所以算法必须和旧版一样。
    /// 旧版只改了新几家目录时不算厂商名（`KIMI_CODE_HOME=/w` 和 `QWEN_HOME=/w` 同名），那两组当时就写进了同一份，
    /// 照旧名去找才找得到；现在的缓存按账号分（[`crate::core::Account::key_for`] 算上了厂商名），不再共用。
    /// 都在默认位置是 `None`。
    pub fn profile(&self, home: &Path) -> Option<String> {
        if *self == Self::defaults(home) {
            return None;
        }
        // FNV-1a：std 的哈希不保证跨版本稳定，这里要跨版本不变。
        // 最早的三家总是算进去，后来加的只在改过目录时才算：老用户的名字不变，缓存不用重建。
        let defaults = Self::defaults(home);
        let mut h: u32 = 0x811c_9dc5;
        // 按全部值（含不再支持的 Qwen Code）算：0.1.3 那时它在表里。
        let counted = Vendor::EVERY.into_iter().filter(|&v| {
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

    /// 多账号之前，改过数据目录时会话台自己的缓存（列表缓存、额度缓存、改名置顶）放哪：都在默认位置就是
    /// 数据目录本身，否则是数据目录下 `profiles/<profile>`。现在一个缓存库装全部账号，只在升级时从这里搬改名置顶。
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

/// OpenCode 的库默认放在 XDG 数据目录下。
pub(super) const XDG_DATA_HOME: &str = "XDG_DATA_HOME";

/// 启动 CLI 时一个改目录的变量怎么设：`Some(值)` 设成它，`None` 清掉（`env -u`）。
pub type EnvAssign = (&'static str, Option<PathBuf>);

/// OpenCode 的库在 XDG 数据目录下的位置（没设 `OPENCODE_DB` 时）。
const OPENCODE_DB_UNDER_XDG: &str = "opencode/opencode.db";

/// `~/` 换成家目录，相对路径接在 `base` 后面，末尾的 `/` 和中间的 `.` 去掉
/// （按路径分段重新拼一遍，不让同一个目录变成两组）。
fn absolute(home: &Path, base: &Path, value: &Path) -> PathBuf {
    base.join(expand_home(home, value.to_path_buf()))
        .components()
        .collect()
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
            Some(("CODEX_HOME", PathBuf::from("/data/codex")))
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
            Some(("GEMINI_CLI_HOME", PathBuf::from("/work/g2")))
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
