//! 认安装方式：拿 PATH 上找到的命令，解开链接看真身在哪，按路径认是哪种装法、读出版本、看升级时要写的目录能不能写。
//! 只读文件，不跑任何程序（问 `--version` 在 `locate`）。

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::core::Vendor;

/// 每家的命令名和各渠道里的包名。
pub(super) struct Cli {
    pub program: &'static str,
    /// npm 一类包管理器里的包名，前面的优先（OpenCode：v2 `@opencode/cli`、v1 `opencode-ai`）。
    pub packages: &'static [&'static str],
    /// Homebrew formula 名。
    pub formulae: &'static [&'static str],
    /// Homebrew cask 名。
    pub casks: &'static [&'static str],
}

/// 每家一行，命令名和 `sources` 里新建对话用的一致（测试 `programs_match_sources` 对着查）。
pub(super) fn cli(vendor: Vendor) -> Cli {
    let c = |program, packages, formulae, casks| Cli {
        program,
        packages,
        formulae,
        casks,
    };
    match vendor {
        Vendor::Claude => c(
            "claude",
            &["@anthropic-ai/claude-code"],
            &[],
            &["claude-code", "claude-code@latest"],
        ),
        Vendor::Codex => c("codex", &["@openai/codex"], &[], &["codex"]),
        Vendor::Pi => c(
            "pi",
            &["@earendil-works/pi-coding-agent"],
            &["pi-coding-agent"],
            &[],
        ),
        Vendor::Hermes => c("hermes", &[], &["hermes-agent"], &[]),
        Vendor::Kimi => c("kimi", &["@moonshot-ai/kimi-code"], &["kimi-code"], &[]),
        Vendor::Gemini => c("gemini", &["@google/gemini-cli"], &["gemini-cli"], &[]),
        Vendor::OpenCode => c(
            "opencode",
            &["@opencode/cli", "opencode-ai"],
            &["opencode", "opencode-v2"],
            &[],
        ),
        Vendor::Antigravity => c("agy", &[], &[], &["antigravity-cli"]),
        // 不再支持，`Vendor::ALL` 里没有它，走不到这里。
        Vendor::Qwen => c("qwen", &[], &[], &[]),
    }
}

/// 这家的命令名（`claude`、`agy`……）。
pub fn program(vendor: Vendor) -> &'static str {
    cli(vendor).program
}

/// Node 包管理器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
    Volta,
}

impl Manager {
    pub fn name(self) -> &'static str {
        match self {
            Manager::Npm => "npm",
            Manager::Pnpm => "pnpm",
            Manager::Yarn => "yarn",
            Manager::Bun => "bun",
            Manager::Volta => "volta",
        }
    }

    /// 全局装某个包的最新版。npm 官方说升 Claude Code 用 `install -g …@latest`，不要 `update -g`。
    pub fn install_cmd(self, package: &str) -> String {
        match self {
            Manager::Npm => format!("npm install -g {package}@latest"),
            Manager::Pnpm => format!("pnpm add -g {package}@latest"),
            Manager::Yarn => format!("yarn global add {package}@latest"),
            Manager::Bun => format!("bun add -g {package}@latest"),
            Manager::Volta => format!("volta install {package}@latest"),
        }
    }
}

/// 装法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// 官方安装器装的：原生版、独立版、curl 安装脚本（具体是哪个看厂商）。
    Native,
    /// Node 包管理器全局装的。
    Package {
        manager: Manager,
        package: &'static str,
    },
    /// Homebrew formula（`Cellar/<name>/<版本>`）。
    Formula(String),
    /// Homebrew cask（`Caskroom/<name>/<版本>`）。
    Cask(String),
    /// 认不出。
    Unknown,
}

impl Method {
    /// 给人看的装法。
    pub fn label(&self, vendor: Vendor) -> String {
        match self {
            Method::Native => match vendor {
                Vendor::Claude => "原生安装".to_owned(),
                Vendor::Codex => "独立安装".to_owned(),
                Vendor::Kimi | Vendor::OpenCode => "官方安装器".to_owned(),
                _ => "官方安装脚本".to_owned(),
            },
            Method::Package { manager, .. } => format!("{} 全局", manager.name()),
            Method::Formula(name) => format!("Homebrew {name}"),
            Method::Cask(name) => format!("Homebrew cask {name}"),
            Method::Unknown => "认不出装法".to_owned(),
        }
    }
}

/// 升级时会挡住的情况（挡住了就只给命令，让主人自己跑）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// npm 装在 `install` 前缀下，可现在 `npm prefix -g` 是 `current`（nvm、volta、fnm 切过版本）；`None` 是找不到 npm。
    PrefixMismatch {
        install: PathBuf,
        current: Option<PathBuf>,
    },
    /// 升级要写的目录不可写（多半要 sudo，我们不用 sudo）。
    NotWritable(PathBuf),
}

/// 本机装着的一家。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub vendor: Vendor,
    /// PATH 上找到的那个。
    pub path: PathBuf,
    /// 解开链接后的真身。
    pub real: PathBuf,
    pub method: Method,
    /// 当前版本（`v` 去掉）；路径里读不出时由 `locate` 问 `--version` 补上，问不出是 `None`。
    pub version: Option<String>,
    pub problem: Option<Problem>,
}

/// 认的时候要用的本机情况。
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub home: PathBuf,
    /// 登录 shell 里 `npm prefix -g` 的结果；没有 npm 是 `None`。
    pub npm_prefix: Option<PathBuf>,
}

/// 认一家：`path` 是 PATH 上找到的命令。
pub fn detect(vendor: Vendor, path: &Path, env: &Env) -> Install {
    let real = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let home = fs::canonicalize(&env.home).unwrap_or_else(|_| env.home.clone());
    let spec = cli(vendor);
    let (method, version, target) = brew(&real, "Cellar", spec.formulae)
        .map(|(name, ver, dir)| (Method::Formula(name), Some(strip_revision(&ver)), dir))
        .or_else(|| {
            brew(&real, "Caskroom", spec.casks).map(|(name, ver, dir)| {
                let ver = ver.split(',').next().unwrap_or_default().to_owned();
                (Method::Cask(name), Some(ver), dir)
            })
        })
        .or_else(|| node_package(&real, spec.packages))
        .or_else(|| volta(path, &home, spec.packages))
        .or_else(|| native(vendor, &real, &home))
        .unwrap_or((Method::Unknown, None, None));
    let problem = match (&method, target) {
        (Method::Package { manager, .. }, Some(dir)) => {
            prefix_problem(*manager, &dir, env.npm_prefix.as_deref())
                .or_else(|| (!writable(&dir)).then(|| Problem::NotWritable(dir.clone())))
        }
        (_, Some(dir)) => (!writable(&dir)).then_some(Problem::NotWritable(dir)),
        _ => None,
    };
    Install {
        vendor,
        path: path.to_owned(),
        real,
        method,
        version: version.filter(|v| !v.is_empty()),
        problem,
    }
}

type Found = (Method, Option<String>, Option<PathBuf>);

/// `<前缀>/Cellar/<名>/<版本>/…`、`<前缀>/Caskroom/<名>/<版本>/…`：名字、版本、升级要写的目录（`<前缀>/Cellar`）。
fn brew(real: &Path, room: &str, names: &[&str]) -> Option<(String, String, Option<PathBuf>)> {
    let parts = normal_parts(real);
    let i = parts.iter().rposition(|p| p == room)?;
    let name = parts.get(i + 1)?;
    let ver = parts.get(i + 2)?;
    names.contains(&name.as_str()).then(|| {
        let dir: PathBuf = std::iter::once("/".to_owned())
            .chain(parts[..=i].iter().cloned())
            .collect();
        (name.clone(), ver.clone(), Some(dir))
    })
}

/// `…/node_modules/<包>/…`：哪个包管理器、版本（包里的 package.json）、升级要写的目录（那个 node_modules）。
fn node_package(real: &Path, packages: &[&'static str]) -> Option<Found> {
    let parts = normal_parts(real);
    let (at, package) = packages.iter().find_map(|&pkg| {
        let segs: Vec<&str> = pkg.split('/').collect();
        (0..parts.len()).rev().find_map(|i| {
            (parts[i] == "node_modules"
                && parts.len() > i + segs.len()
                && segs.iter().zip(&parts[i + 1..]).all(|(a, b)| a == b))
            .then_some((i, pkg))
        })
    })?;
    let root: PathBuf = std::iter::once("/".to_owned())
        .chain(parts[..=at].iter().cloned())
        .collect();
    let text = real.to_string_lossy();
    let manager = if text.contains("/.bun/") {
        Manager::Bun
    } else if text.contains("/pnpm/") || text.contains("/.pnpm/") {
        Manager::Pnpm
    } else if text.contains("/yarn/global/") || text.contains("/.yarn/") {
        Manager::Yarn
    } else if text.contains("/.volta/") {
        Manager::Volta
    } else {
        Manager::Npm
    };
    let version = package_version(&root.join(package));
    Some((Method::Package { manager, package }, version, Some(root)))
}

/// Volta 的 `~/.volta/bin/<命令>` 是个转发程序，真身里看不出包；包在 `~/.volta/tools/image/packages/<包>/`。
fn volta(path: &Path, home: &Path, packages: &[&'static str]) -> Option<Found> {
    let bin = home.join(".volta/bin");
    let dir =
        fs::canonicalize(path.parent()?).unwrap_or_else(|_| path.parent().unwrap_or(path).into());
    if dir != bin {
        return None;
    }
    let package = packages
        .iter()
        .copied()
        .find(|p| home.join(".volta/tools/image/packages").join(p).exists())
        .or_else(|| packages.first().copied())?;
    let image = home.join(".volta/tools/image/packages").join(package);
    let version = package_version(&image.join("lib/node_modules").join(package));
    Some((
        Method::Package {
            manager: Manager::Volta,
            package,
        },
        version,
        Some(home.join(".volta/tools/image/packages")),
    ))
}

/// 各家官方安装器放的位置。
fn native(vendor: Vendor, real: &Path, home: &Path) -> Option<Found> {
    let under = |rel: &str| real.starts_with(home.join(rel));
    let parent = real.parent().map(Path::to_owned);
    match vendor {
        // `~/.local/share/claude/versions/<版本>` 本身就是程序。
        Vendor::Claude => {
            let parts = normal_parts(real);
            let i = parts.windows(4).position(|w| {
                w[0] == ".local" && w[1] == "share" && w[2] == "claude" && w[3] == "versions"
            })?;
            let version = parts.get(i + 4).cloned();
            Some((Method::Native, version, parent))
        }
        // `~/.codex/packages/standalone/releases/<版本>-<目标平台>/…`
        Vendor::Codex => {
            let parts = normal_parts(real);
            let i = parts
                .windows(3)
                .position(|w| w[0] == "packages" && w[1] == "standalone" && w[2] == "releases")?;
            let dir = parts.get(i + 3)?;
            let version = ["-aarch64-", "-x86_64-", "-arm64-"]
                .iter()
                .find_map(|t| dir.find(t).map(|at| dir[..at].to_owned()))
                .unwrap_or_else(|| dir.clone());
            let releases: PathBuf = std::iter::once("/".to_owned())
                .chain(parts[..=i + 2].iter().cloned())
                .collect();
            Some((Method::Native, Some(version), Some(releases)))
        }
        Vendor::Pi if under(".pi/agent/install") => Some((Method::Native, None, parent)),
        Vendor::Hermes if under(".hermes/hermes-agent") => Some((
            Method::Native,
            None,
            Some(home.join(".hermes/hermes-agent")),
        )),
        Vendor::Kimi if under(".kimi-code") => Some((Method::Native, None, parent)),
        Vendor::OpenCode if under(".opencode") => Some((Method::Native, None, parent)),
        Vendor::Antigravity if under(".local/bin") => Some((Method::Native, None, parent)),
        _ => None,
    }
}

/// npm 装的要和现在 `npm prefix -g` 对得上（`<前缀>/lib/node_modules`），不然 `npm install -g` 会装到别处去。
fn prefix_problem(
    manager: Manager,
    node_modules: &Path,
    current: Option<&Path>,
) -> Option<Problem> {
    if manager != Manager::Npm {
        return None;
    }
    let lib = node_modules.parent()?;
    if lib.file_name()? != "lib" {
        return None;
    }
    let install = lib.parent()?.to_owned();
    let same = current
        .map(|c| fs::canonicalize(c).unwrap_or_else(|_| c.to_owned()))
        .is_some_and(|c| c == install);
    (!same).then(|| Problem::PrefixMismatch {
        install,
        current: current.map(Path::to_owned),
    })
}

/// 自己能不能往这个目录里写（`access(W_OK)`，和真去写的权限判断一致）。
fn writable(dir: &Path) -> bool {
    let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c` 是以 NUL 结尾的合法 C 字符串，活到调用结束；access 只读不写内存。
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// 包目录里 package.json 的 `version`。
fn package_version(dir: &Path) -> Option<String> {
    let text = fs::read(dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&text).ok()?;
    v.get("version")?.as_str().map(str::to_owned)
}

/// Homebrew 的修订号（`1.2.3_1`）去掉。
fn strip_revision(ver: &str) -> String {
    match ver.rsplit_once('_') {
        Some((v, rev)) if !rev.is_empty() && rev.chars().all(|c| c.is_ascii_digit()) => {
            v.to_owned()
        }
        _ => ver.to_owned(),
    }
}

fn normal_parts(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// 从 `--version` 的输出里取第一个像版本号的词（`codex-cli 0.171.0`、`Hermes Agent v0.23.3`、`2.1.330 (Claude Code)`）。
pub fn version_in(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | '@'))
        .map(|w| w.trim_start_matches(['v', 'V']))
        .find(|w| {
            let mut parts = w.split('.');
            parts.clone().count() >= 2
                && parts
                    .all(|p| !p.is_empty() && p.chars().next().is_some_and(|c| c.is_ascii_digit()))
        })
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_words() {
        assert_eq!(
            version_in("2.1.330 (Claude Code)").as_deref(),
            Some("2.1.330")
        );
        assert_eq!(
            version_in("codex-cli 0.171.0\n").as_deref(),
            Some("0.171.0")
        );
        assert_eq!(
            version_in("Hermes Agent v0.23.3 (2026)").as_deref(),
            Some("0.23.3")
        );
        assert_eq!(version_in("no version here"), None);
        assert_eq!(strip_revision("0.70.0_1"), "0.70.0");
        assert_eq!(strip_revision("1.2.3"), "1.2.3");
    }
}
