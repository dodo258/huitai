//! 会怎么升级：按装法给出官方升级命令，和它能不能交给会话台去跑（跑在 `upgrade`）。

use crate::core::{Vendor, release};

use super::detect::{Install, Method, Problem};

/// 一家的升级说法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// 官方升级命令；认不出装法时没有。
    pub command: Option<String>,
    pub how: How,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// 它自带自动更新（Claude 原生/npm、Kimi 原生/npm、Gemini npm、agy curl），先交给它自己。
    SelfUpdates,
    /// 可以由会话台替它跑。
    Ours,
    /// 只给命令、让主人自己跑，后面是原因。
    Manual(String),
}

/// 这家这么装的，升级用什么命令；`latest` 是查到的最新版（看跨不跨大版本；能指定版本的命令就装这一版）。
pub fn plan(install: &Install, latest: Option<&str>) -> Plan {
    let (command, self_updates) = command(install, latest);
    let manual = |why: String| Plan {
        command: command.clone(),
        how: How::Manual(why),
    };
    if command.is_none() {
        return manual(format!(
            "认不出是怎么装的（{}），请按它的官方说明升级",
            install.real.display()
        ));
    }
    match &install.problem {
        Some(Problem::PrefixMismatch {
            install: at,
            current,
        }) => {
            return manual(match current {
                Some(c) => format!(
                    "装在 {}，可现在 npm prefix -g 是 {}（切过 Node 版本），自己确认后再跑",
                    at.display(),
                    c.display()
                ),
                None => format!("装在 {}，可登录 shell 里找不到 npm", at.display()),
            });
        }
        Some(Problem::NotWritable(dir)) => {
            return manual(format!("{} 不可写（会话台不用 sudo）", dir.display()));
        }
        None => {}
    }
    if let (Some(cur), Some(new)) = (major(install.version.as_deref()), major(latest))
        && new > cur
    {
        return manual(format!("新版跨大版本（{cur} → {new}），只提示"));
    }
    // OpenCode 1.x 只提示（方案只让 2.x 同大版本自动升）。
    if install.vendor == Vendor::OpenCode && major(install.version.as_deref()) == Some(1) {
        return manual("OpenCode 1.x 只提示".to_owned());
    }
    Plan {
        command,
        how: if self_updates {
            How::SelfUpdates
        } else {
            How::Ours
        },
    }
}

/// 升级命令里能写死装哪一版：包管理器装的能；pi 走 `pi update`，Homebrew 和各家安装器只装它们那边此刻的最新版。
pub(super) fn takes_version(install: &Install) -> bool {
    matches!(install.method, Method::Package { .. }) && install.vendor != Vendor::Pi
}

/// 官方升级命令，和这种装法它自己会不会自动装新版。
fn command(install: &Install, latest: Option<&str>) -> (Option<String>, bool) {
    let v = install.vendor;
    let cmd = |s: &str| Some(s.to_owned());
    match &install.method {
        Method::Formula(name) => (Some(format!("brew upgrade {name}")), false),
        Method::Cask(name) => (Some(format!("brew upgrade --cask {name}")), false),
        Method::Unknown => (None, false),
        // pi 按它自己认的装法升级；curl 装的只能交给它，我们不碰 `~/.pi/agent/*`。
        _ if v == Vendor::Pi => (cmd("pi update"), false),
        Method::Package { manager, package } => (
            Some(manager.install_cmd(package, wanted(install, latest))),
            matches!(v, Vendor::Claude | Vendor::Kimi | Vendor::Gemini),
        ),
        Method::Native => match v {
            Vendor::Claude => (cmd("claude update"), true),
            Vendor::Codex => (
                cmd("curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh"),
                false,
            ),
            Vendor::Hermes => (cmd("hermes update"), false),
            Vendor::Kimi => (cmd("kimi upgrade"), true),
            Vendor::OpenCode => match major(install.version.as_deref()) {
                Some(1) => (cmd("opencode upgrade"), false),
                _ => (cmd("curl -fsSL https://opencode.ai/install | bash"), false),
            },
            Vendor::Antigravity => (
                cmd("curl -fsSL https://antigravity.google/cli/install.sh | bash"),
                true,
            ),
            _ => (None, false),
        },
    }
}

/// 包管理器装哪一版：查到的 `latest` 比本机新就装它（比过的一定全是数字，能放进命令）；不然没有要升的，
/// 只是给人看的命令，写 `latest`。
fn wanted<'a>(install: &Install, latest: Option<&'a str>) -> &'a str {
    latest
        .filter(|l| install.older_than(l))
        .map_or("latest", release::display_version)
}

/// 大版本号。
fn major(version: Option<&str>) -> Option<u64> {
    version?
        .trim_start_matches(['v', 'V'])
        .split('.')
        .next()?
        .parse()
        .ok()
}

impl Plan {
    /// `ai doctor` 里「会怎么升级」那一段。
    pub fn describe(&self) -> String {
        match (&self.command, &self.how) {
            (Some(c), How::SelfUpdates) => {
                format!("{c}（它自带自动更新；新版出来 24 小时还没装上，会话台照常升）")
            }
            (Some(c), How::Ours) => c.clone(),
            (Some(c), How::Manual(why)) => format!("{why}：{c}"),
            (None, How::Manual(why)) => why.clone(),
            (None, _) => String::new(),
        }
    }
}
