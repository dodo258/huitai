//! 各家 CLI 认装法：每种装法一个手写目录树（临时 HOME，假程序和 package.json），认出的装法、读出的版本、
//! 跟哪里比、会怎么升级都对；认不出装法、npm 前缀对不上、目录不可写各一个。

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use huitai::clis::{
    Env, Feed, How, LoginEnv, Manager, Method, Problem, detect, feed_for, plan, probe, program,
};
use huitai::core::Vendor;
use huitai::sources::{Trash, default_sources};

/// 一棵临时目录树：`root/home` 当家目录，`root/brew` 当 Homebrew 前缀。
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("huitai-clis-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("home")).unwrap();
        Self {
            root: fs::canonicalize(&d).unwrap(),
        }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// 一个可执行的假程序。
    fn program(&self, rel: &str, body: &str) -> PathBuf {
        let p = self.p(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// `link` 指向 `target` 的链接（像 npm、brew 放进 bin 的那样）。
    fn link(&self, link: &str, target: &Path) -> PathBuf {
        let l = self.p(link);
        fs::create_dir_all(l.parent().unwrap()).unwrap();
        symlink(target, &l).unwrap();
        l
    }

    /// `dir` 下的一个 Node 包：package.json 带版本，加一个入口文件。
    fn package(&self, dir: &str, version: &str) -> PathBuf {
        let d = self.p(dir);
        fs::create_dir_all(&d).unwrap();
        fs::write(
            d.join("package.json"),
            format!(r#"{{"name":"x","version":"{version}"}}"#),
        )
        .unwrap();
        self.program(&format!("{dir}/bin/cli.js"), "echo js")
    }

    fn env(&self, npm_prefix: Option<&str>) -> Env {
        Env {
            home: self.home(),
            npm_prefix: npm_prefix.map(|p| self.p(p)),
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        // 不可写的那个测试改过权限，删之前改回来。
        let _ = std::process::Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&self.root)
            .status();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn npm(package: &'static str) -> Method {
    Method::Package {
        manager: Manager::Npm,
        package,
    }
}

const ARCH: &str = "aarch64";

#[test]
fn claude_native_cask_and_npm() {
    let t = Tree::new("claude");
    let real = t.program("home/.local/share/claude/versions/2.1.330", "");
    let path = t.link("home/.local/bin/claude", &real);
    let i = detect(Vendor::Claude, &path, &t.env(None));
    assert_eq!(i.method, Method::Native);
    assert_eq!(i.version.as_deref(), Some("2.1.330"));
    assert_eq!(i.problem, None);
    assert_eq!(feed_for(&i, ARCH), Some(Feed::ClaudeNative));
    let p = plan(&i, Some("2.1.340"));
    assert_eq!(
        (p.command.as_deref(), p.how),
        (Some("claude update"), How::SelfUpdates)
    );

    let real = t.program("brew/Caskroom/claude-code/2.1.323/claude", "");
    let path = t.link("brew/bin/claude", &real);
    let i = detect(Vendor::Claude, &path, &t.env(None));
    assert_eq!(i.method, Method::Cask("claude-code".into()));
    assert_eq!(i.version.as_deref(), Some("2.1.323"));
    assert_eq!(feed_for(&i, ARCH), Some(Feed::Cask("claude-code".into())));
    assert_eq!(
        plan(&i, None).command.as_deref(),
        Some("brew upgrade --cask claude-code")
    );

    let real = t.package("npm/lib/node_modules/@anthropic-ai/claude-code", "2.1.300");
    let path = t.link("npm/bin/claude", &real);
    let i = detect(Vendor::Claude, &path, &t.env(Some("npm")));
    assert_eq!(i.method, npm("@anthropic-ai/claude-code"));
    assert_eq!(i.version.as_deref(), Some("2.1.300"));
    assert_eq!(i.problem, None);
    assert_eq!(
        feed_for(&i, ARCH),
        Some(Feed::Npm("@anthropic-ai/claude-code"))
    );
    let p = plan(&i, Some("2.1.330"));
    assert_eq!(
        p.command.as_deref(),
        Some("npm install -g @anthropic-ai/claude-code@latest")
    );
    assert_eq!(p.how, How::SelfUpdates);
}

#[test]
fn codex_standalone_npm_and_cask() {
    let t = Tree::new("codex");
    let real = t.program(
        "home/.codex/packages/standalone/releases/0.171.0-aarch64-apple-darwin/bin/codex",
        "",
    );
    let path = t.link("home/.local/bin/codex", &real);
    let i = detect(Vendor::Codex, &path, &t.env(None));
    assert_eq!(i.method, Method::Native);
    assert_eq!(i.version.as_deref(), Some("0.171.0"));
    assert_eq!(
        feed_for(&i, ARCH),
        Some(Feed::Github {
            repo: "openai/codex",
            tag_prefix: "rust-v"
        })
    );
    let p = plan(&i, Some("0.172.0"));
    assert!(p.command.unwrap().contains("chatgpt.com/codex/install.sh"));
    assert_eq!(p.how, How::Ours, "Codex 只提示，没有自带自动安装");

    let real = t.package("npm/lib/node_modules/@openai/codex", "0.170.0");
    let path = t.link("npm/bin/codex", &real);
    let i = detect(Vendor::Codex, &path, &t.env(Some("npm")));
    assert_eq!(i.method, npm("@openai/codex"));
    assert_eq!(i.version.as_deref(), Some("0.170.0"));
    assert_eq!(feed_for(&i, ARCH), Some(Feed::Npm("@openai/codex")));

    let real = t.program("brew/Caskroom/codex/0.171.0/codex-aarch64-apple-darwin", "");
    let path = t.link("brew/bin/codex", &real);
    let i = detect(Vendor::Codex, &path, &t.env(None));
    assert_eq!(i.method, Method::Cask("codex".into()));
    assert_eq!(i.version.as_deref(), Some("0.171.0"));
}

#[test]
fn pi_npm_and_curl() {
    let t = Tree::new("pi");
    let real = t.package(
        "npm/lib/node_modules/@earendil-works/pi-coding-agent",
        "1.6.0",
    );
    let path = t.link("npm/bin/pi", &real);
    let i = detect(Vendor::Pi, &path, &t.env(Some("npm")));
    assert_eq!(i.method, npm("@earendil-works/pi-coding-agent"));
    assert_eq!(i.version.as_deref(), Some("1.6.0"));
    assert_eq!(feed_for(&i, ARCH), Some(Feed::Pi));
    assert_eq!(plan(&i, None).command.as_deref(), Some("pi update"));

    // curl 装的：路径里没有版本，`probe` 问 `--version`。
    let path = t.program("home/.pi/agent/install/bin/pi", "echo 'pi 1.5.2'");
    let i = detect(Vendor::Pi, &path, &t.env(None));
    assert_eq!(
        (i.method.clone(), i.version.clone()),
        (Method::Native, None)
    );
    let login = LoginEnv {
        path: vec![t.p("home/.pi/agent/install/bin"), PathBuf::from("/usr/bin")],
        npm_prefix: None,
    };
    let found = probe(&login, &t.home());
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].vendor, Vendor::Pi);
    assert_eq!(found[0].version.as_deref(), Some("1.5.2"));
}

#[test]
fn hermes_curl_and_brew() {
    let t = Tree::new("hermes");
    let real = t.program("home/.hermes/hermes-agent/venv/bin/hermes", "");
    let path = t.link("home/.local/bin/hermes", &real);
    let i = detect(Vendor::Hermes, &path, &t.env(None));
    assert_eq!(i.method, Method::Native);
    assert_eq!(
        feed_for(&i, ARCH),
        Some(Feed::Github {
            repo: "NousResearch/hermes-agent",
            tag_prefix: "v"
        })
    );
    assert_eq!(plan(&i, None).command.as_deref(), Some("hermes update"));

    let real = t.program("brew/Cellar/hermes-agent/0.22.4_1/bin/hermes", "");
    let path = t.link("brew/bin/hermes", &real);
    let i = detect(Vendor::Hermes, &path, &t.env(None));
    assert_eq!(i.method, Method::Formula("hermes-agent".into()));
    assert_eq!(i.version.as_deref(), Some("0.22.4"), "修订号去掉");
    assert_eq!(
        feed_for(&i, ARCH),
        Some(Feed::Formula("hermes-agent".into()))
    );
    assert_eq!(
        plan(&i, None).command.as_deref(),
        Some("brew upgrade hermes-agent")
    );
}

#[test]
fn kimi_native_npm_and_brew() {
    let t = Tree::new("kimi");
    let path = t.program("home/.kimi-code/bin/kimi", "echo 2.7.2");
    let i = detect(Vendor::Kimi, &path, &t.env(None));
    assert_eq!(i.method, Method::Native);
    assert_eq!(feed_for(&i, ARCH), Some(Feed::KimiNative));
    assert_eq!(plan(&i, None).command.as_deref(), Some("kimi upgrade"));

    let real = t.package("npm/lib/node_modules/@moonshot-ai/kimi-code", "2.1.1");
    let path = t.link("npm/bin/kimi", &real);
    let i = detect(Vendor::Kimi, &path, &t.env(Some("npm")));
    assert_eq!(i.method, npm("@moonshot-ai/kimi-code"));
    assert_eq!(i.version.as_deref(), Some("2.1.1"));

    let real = t.program("brew/Cellar/kimi-code/2.7.0/bin/kimi", "");
    let path = t.link("brew/bin/kimi", &real);
    let i = detect(Vendor::Kimi, &path, &t.env(None));
    assert_eq!(i.method, Method::Formula("kimi-code".into()));
    assert_eq!(i.version.as_deref(), Some("2.7.0"));
}

#[test]
fn gemini_by_package_manager_and_brew() {
    let t = Tree::new("gemini");
    let env = t.env(Some("npm"));
    let cases = [
        (
            "home/Library/pnpm/global/5/.pnpm/@google+gemini-cli@0.70.0/node_modules/@google/gemini-cli",
            "home/Library/pnpm/gemini",
            Manager::Pnpm,
        ),
        (
            "home/.bun/install/global/node_modules/@google/gemini-cli",
            "home/.bun/bin/gemini",
            Manager::Bun,
        ),
        (
            "home/.config/yarn/global/node_modules/@google/gemini-cli",
            "home/.yarn/bin/gemini",
            Manager::Yarn,
        ),
    ];
    for (dir, bin, manager) in cases {
        let real = t.package(dir, "0.70.0");
        let path = t.link(bin, &real);
        let i = detect(Vendor::Gemini, &path, &env);
        assert_eq!(
            i.method,
            Method::Package {
                manager,
                package: "@google/gemini-cli"
            },
            "{bin}"
        );
        assert_eq!(i.version.as_deref(), Some("0.70.0"), "{bin}");
        assert_eq!(i.problem, None, "只有 npm 要对 prefix：{bin}");
        assert_eq!(feed_for(&i, ARCH), Some(Feed::Npm("@google/gemini-cli")));
    }
    // Volta：bin 里是转发程序，包在 tools/image/packages 下。
    t.package(
        "home/.volta/tools/image/packages/@google/gemini-cli/lib/node_modules/@google/gemini-cli",
        "0.69.0",
    );
    let path = t.program("home/.volta/bin/gemini", "");
    let i = detect(Vendor::Gemini, &path, &env);
    assert_eq!(
        i.method,
        Method::Package {
            manager: Manager::Volta,
            package: "@google/gemini-cli"
        }
    );
    assert_eq!(i.version.as_deref(), Some("0.69.0"));
    assert_eq!(
        plan(&i, None).command.as_deref(),
        Some("volta install @google/gemini-cli@latest")
    );

    let real = t.program("brew/Cellar/gemini-cli/0.70.0/bin/gemini", "");
    let path = t.link("brew/bin/gemini", &real);
    let i = detect(Vendor::Gemini, &path, &env);
    assert_eq!(i.method, Method::Formula("gemini-cli".into()));
}

#[test]
fn opencode_v1_v2_and_brew() {
    let t = Tree::new("opencode");
    let path = t.program("home/.opencode/bin/opencode", "echo 2.3.1");
    let mut i = detect(Vendor::OpenCode, &path, &t.env(None));
    assert_eq!(i.method, Method::Native);
    i.version = Some("2.3.1".into());
    assert_eq!(feed_for(&i, ARCH), Some(Feed::Npm("@opencode/cli")));
    assert!(
        plan(&i, Some("2.3.2"))
            .command
            .unwrap()
            .contains("opencode.ai/install")
    );
    i.version = Some("1.19.0".into());
    assert_eq!(
        feed_for(&i, ARCH),
        Some(Feed::Npm("opencode-ai")),
        "1.x 跟 1.x 比，不跨大版本"
    );
    let p = plan(&i, Some("1.19.6"));
    assert_eq!(p.command.as_deref(), Some("opencode upgrade"));
    assert!(matches!(p.how, How::Manual(_)));

    let real = t.package("npm/lib/node_modules/@opencode/cli", "2.3.2");
    let path = t.link("npm/bin/opencode", &real);
    let i = detect(Vendor::OpenCode, &path, &t.env(Some("npm")));
    assert_eq!(i.method, npm("@opencode/cli"));
    assert_eq!(i.version.as_deref(), Some("2.3.2"));

    let real = t.package("npm2/lib/node_modules/opencode-ai", "1.19.6");
    let path = t.link("npm2/bin/opencode", &real);
    let i = detect(Vendor::OpenCode, &path, &t.env(Some("npm2")));
    assert_eq!(i.method, npm("opencode-ai"));
    assert_eq!(feed_for(&i, ARCH), Some(Feed::Npm("opencode-ai")));

    let real = t.program("brew/Cellar/opencode/1.19.6/bin/opencode", "");
    let path = t.link("brew/bin/opencode", &real);
    let i = detect(Vendor::OpenCode, &path, &t.env(None));
    assert_eq!(i.method, Method::Formula("opencode".into()));
    let p = plan(&i, Some("2.3.2"));
    assert!(
        matches!(&p.how, How::Manual(why) if why.contains("跨大版本")),
        "{p:?}"
    );
}

#[test]
fn antigravity_curl_and_cask() {
    let t = Tree::new("agy");
    let path = t.program("home/.local/bin/agy", "echo 1.5.14");
    let i = detect(Vendor::Antigravity, &path, &t.env(None));
    assert_eq!(i.method, Method::Native);
    assert_eq!(feed_for(&i, "aarch64"), Some(Feed::Agy("darwin_arm64")));
    assert_eq!(feed_for(&i, "x86_64"), Some(Feed::Agy("darwin_amd64")));
    assert_eq!(plan(&i, None).how, How::SelfUpdates);

    let real = t.program(
        "brew/Caskroom/antigravity-cli/1.5.14,5302690847260672/agy",
        "",
    );
    let path = t.link("brew/bin/agy", &real);
    let i = detect(Vendor::Antigravity, &path, &t.env(None));
    assert_eq!(i.method, Method::Cask("antigravity-cli".into()));
    assert_eq!(i.version.as_deref(), Some("1.5.14"), "逗号后的构建号去掉");
}

#[test]
fn unknown_method_only_shows_advice() {
    let t = Tree::new("unknown");
    let path = t.program("opt/tools/claude", "echo 2.1.0");
    let i = detect(Vendor::Claude, &path, &t.env(Some("npm")));
    assert_eq!(i.method, Method::Unknown);
    assert_eq!(i.version, None);
    assert_eq!(
        feed_for(&i, ARCH),
        None,
        "认不出装法就不查，免得跟错渠道误报"
    );
    let p = plan(&i, None);
    assert_eq!(p.command, None);
    assert!(
        matches!(&p.how, How::Manual(why) if why.contains("认不出")),
        "{p:?}"
    );
}

#[test]
fn npm_prefix_mismatch_only_shows_the_command() {
    let t = Tree::new("prefix");
    let real = t.package("nvm/v20/lib/node_modules/@openai/codex", "0.170.0");
    let path = t.link("nvm/v20/bin/codex", &real);
    // 现在 `npm prefix -g` 是另一个 Node 版本的前缀。
    let i = detect(Vendor::Codex, &path, &t.env(Some("nvm/v22")));
    assert_eq!(
        i.problem,
        Some(Problem::PrefixMismatch {
            install: t.p("nvm/v20"),
            current: Some(t.p("nvm/v22")),
        })
    );
    let p = plan(&i, Some("0.171.0"));
    assert_eq!(
        p.command.as_deref(),
        Some("npm install -g @openai/codex@latest")
    );
    assert!(
        matches!(&p.how, How::Manual(why) if why.contains("npm prefix -g")),
        "{p:?}"
    );
    // 登录 shell 里找不到 npm 也一样只提示。
    let i = detect(Vendor::Codex, &path, &t.env(None));
    assert!(matches!(
        i.problem,
        Some(Problem::PrefixMismatch { current: None, .. })
    ));
}

#[test]
fn unwritable_install_dir_only_shows_the_command() {
    let t = Tree::new("readonly");
    let real = t.package("npm/lib/node_modules/@anthropic-ai/claude-code", "2.1.300");
    let path = t.link("npm/bin/claude", &real);
    let modules = t.p("npm/lib/node_modules");
    fs::set_permissions(&modules, fs::Permissions::from_mode(0o555)).unwrap();
    let i = detect(Vendor::Claude, &path, &t.env(Some("npm")));
    assert_eq!(i.problem, Some(Problem::NotWritable(modules)));
    let p = plan(&i, Some("2.1.330"));
    assert!(
        matches!(&p.how, How::Manual(why) if why.contains("不可写")),
        "{p:?}"
    );
}

/// 命令名和各家来源新建对话用的是同一个。
#[test]
fn programs_match_sources() {
    let t = Tree::new("programs");
    for src in default_sources(&t.home(), Trash::new(t.p("trash"))) {
        assert_eq!(
            program(src.vendor()),
            src.new_cmd(&t.home()).program,
            "{:?}",
            src.vendor()
        );
    }
}
