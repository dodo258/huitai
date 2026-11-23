//! 查最新版：按装法选数据源（brew 装的跟 brew 比，npm 装的跟 npm 比），拼地址、解析返回。全是不鉴权的公开接口。

use crate::core::Vendor;

use super::detect::{Install, Method};

/// 一个查最新版的数据源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Feed {
    /// npm registry 的 `<包>/latest`。
    Npm(&'static str),
    /// Homebrew formula 的 `versions.stable`。
    Formula(String),
    /// Homebrew cask 的 `version`（逗号前）。
    Cask(String),
    /// GitHub「最新发布」的 tag，去掉前缀（Codex 是 `rust-v`）。只给 Codex 独立安装和 Hermes 用，带 ETag。
    Github {
        repo: &'static str,
        tag_prefix: &'static str,
    },
    /// Claude 原生安装的发布渠道：正文就是版本号。
    ClaudeNative,
    /// Kimi Code 原生安装：正文就是版本号（GitHub 的 latest 是桌面版，不能用）。
    KimiNative,
    /// pi 的 `api/latest-version`：`{"version": …}`。
    Pi,
    /// Antigravity 安装脚本读的清单（`darwin_arm64` / `darwin_amd64`）：`{"version": …}`。
    Agy(&'static str),
}

/// 各数据源的地址（测试换成本地假服务器）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bases {
    pub npm: String,
    pub brew: String,
    pub github: String,
    pub claude: String,
    pub kimi: String,
    pub pi: String,
    pub agy: String,
}

impl Bases {
    pub fn official() -> Self {
        Self {
            npm: "https://registry.npmjs.org".to_owned(),
            brew: "https://formulae.brew.sh".to_owned(),
            github: "https://api.github.com".to_owned(),
            claude: "https://downloads.claude.ai".to_owned(),
            kimi: "https://cdn.kimi.com".to_owned(),
            pi: "https://pi.dev".to_owned(),
            agy: "https://antigravity-cli-auto-updater-974169037036.us-central1.run.app".to_owned(),
        }
    }

    /// 全部指到同一个地址（测试用）。
    pub fn at(origin: &str) -> Self {
        let o = origin.trim_end_matches('/').to_owned();
        Self {
            npm: o.clone(),
            brew: o.clone(),
            github: o.clone(),
            claude: o.clone(),
            kimi: o.clone(),
            pi: o.clone(),
            agy: o,
        }
    }
}

impl Feed {
    /// 存进缓存的名字：装法变了（换了数据源）就不用旧结果。
    pub fn key(&self) -> String {
        match self {
            Feed::Npm(p) => format!("npm:{p}"),
            Feed::Formula(n) => format!("formula:{n}"),
            Feed::Cask(n) => format!("cask:{n}"),
            Feed::Github { repo, .. } => format!("github:{repo}"),
            Feed::ClaudeNative => "claude".to_owned(),
            Feed::KimiNative => "kimi".to_owned(),
            Feed::Pi => "pi".to_owned(),
            Feed::Agy(platform) => format!("agy:{platform}"),
        }
    }

    pub fn url(&self, b: &Bases) -> String {
        match self {
            Feed::Npm(p) => format!("{}/{p}/latest", b.npm),
            Feed::Formula(n) => format!("{}/api/formula/{n}.json", b.brew),
            Feed::Cask(n) => format!("{}/api/cask/{n}.json", b.brew),
            Feed::Github { repo, .. } => format!("{}/repos/{repo}/releases/latest", b.github),
            Feed::ClaudeNative => format!("{}/claude-code-releases/latest", b.claude),
            Feed::KimiNative => format!("{}/kimi-code/latest", b.kimi),
            Feed::Pi => format!("{}/api/latest-version", b.pi),
            Feed::Agy(platform) => format!("{}/manifests/{platform}.json", b.agy),
        }
    }

    /// 请求要带的头（GitHub 要 Accept）。
    pub fn headers(&self) -> &'static [&'static str] {
        match self {
            Feed::Github { .. } => &["Accept: application/vnd.github+json"],
            _ => &[],
        }
    }

    /// 带 ETag 问（GitHub 不鉴权每小时只给 60 次，没变的 304 不算次数）。
    pub fn uses_etag(&self) -> bool {
        matches!(self, Feed::Github { .. })
    }

    /// 从返回的正文里取版本号；格式不认识是 `None`。
    pub fn parse(&self, body: &str) -> Option<String> {
        let json = || serde_json::from_str::<serde_json::Value>(body).ok();
        let text = |v: &serde_json::Value| v.as_str().map(|s| s.trim().to_owned());
        let version = match self {
            Feed::Npm(_) | Feed::Pi | Feed::Agy(_) => text(json()?.get("version")?),
            Feed::Formula(_) => text(json()?.get("versions")?.get("stable")?),
            Feed::Cask(_) => text(json()?.get("version")?)
                .map(|v| v.split(',').next().unwrap_or_default().to_owned()),
            Feed::Github { tag_prefix, .. } => {
                let tag = text(json()?.get("tag_name")?)?;
                Some(tag.strip_prefix(tag_prefix).unwrap_or(&tag).to_owned())
            }
            Feed::ClaudeNative | Feed::KimiNative => {
                Some(body.lines().next().unwrap_or_default().trim().to_owned())
            }
        }?;
        let looks_like = version
            .trim_start_matches(['v', 'V'])
            .split('.')
            .all(|p| p.chars().next().is_some_and(|c| c.is_ascii_digit()));
        (looks_like && !version.is_empty()).then_some(version)
    }
}

/// 这家这种装法跟哪里比。认不出装法的不查（不知道该跟哪个渠道比，比错了反而误报）。`arch` 是 `std::env::consts::ARCH`。
pub fn feed_for(install: &Install, arch: &str) -> Option<Feed> {
    let method = &install.method;
    match method {
        Method::Formula(n) => return Some(Feed::Formula(n.clone())),
        Method::Cask(n) => return Some(Feed::Cask(n.clone())),
        Method::Unknown => return None,
        _ => {}
    }
    let package = match method {
        Method::Package { package, .. } => Some(*package),
        _ => None,
    };
    Some(match install.vendor {
        Vendor::Claude => package.map_or(Feed::ClaudeNative, Feed::Npm),
        Vendor::Codex => package.map_or(
            Feed::Github {
                repo: "openai/codex",
                tag_prefix: "rust-v",
            },
            Feed::Npm,
        ),
        // pi 自己查新版也是问这个接口，不管怎么装的。
        Vendor::Pi => Feed::Pi,
        Vendor::Hermes => Feed::Github {
            repo: "NousResearch/hermes-agent",
            tag_prefix: "v",
        },
        Vendor::Kimi => package.map_or(Feed::KimiNative, Feed::Npm),
        Vendor::Gemini => Feed::Npm(package?),
        // 官方安装器装的看当前大版本：1.x 跟 `opencode-ai`，2.x 跟 `@opencode/cli`，不跨大版本比。
        Vendor::OpenCode => Feed::Npm(package.unwrap_or_else(|| {
            match install.version.as_deref().and_then(|v| v.split('.').next()) {
                Some("1") => "opencode-ai",
                _ => "@opencode/cli",
            }
        })),
        Vendor::Antigravity => Feed::Agy(if arch == "x86_64" {
            "darwin_amd64"
        } else {
            "darwin_arm64"
        }),
        Vendor::Qwen => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_feed() {
        assert_eq!(
            Feed::Npm("x")
                .parse(r#"{"name":"x","version":"2.1.330"}"#)
                .as_deref(),
            Some("2.1.330")
        );
        assert_eq!(
            Feed::Formula("g".into())
                .parse(r#"{"versions":{"stable":"0.70.0","head":null}}"#)
                .as_deref(),
            Some("0.70.0")
        );
        assert_eq!(
            Feed::Cask("a".into())
                .parse(r#"{"version":"1.5.14,5302690847260672"}"#)
                .as_deref(),
            Some("1.5.14")
        );
        let codex = Feed::Github {
            repo: "openai/codex",
            tag_prefix: "rust-v",
        };
        assert_eq!(
            codex.parse(r#"{"tag_name":"rust-v0.171.0"}"#).as_deref(),
            Some("0.171.0")
        );
        assert_eq!(
            Feed::ClaudeNative.parse("2.1.330\n").as_deref(),
            Some("2.1.330")
        );
        assert_eq!(
            Feed::Pi
                .parse(r#"{"ok":true,"version":"1.6.0","packageName":"p"}"#)
                .as_deref(),
            Some("1.6.0")
        );
        // 认不出的格式：不当成版本号。
        assert_eq!(Feed::ClaudeNative.parse("<html>error</html>"), None);
        assert_eq!(Feed::Npm("x").parse("not json"), None);
        assert_eq!(Feed::Npm("x").parse(r#"{"version":""}"#), None);
    }
}
