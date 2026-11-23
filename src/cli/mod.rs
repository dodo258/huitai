//! 命令入口：`ai`、`ai doctor`、`ai attach <id>`、`ai export <id>`、`ai list`、`ai search <词>`、`ai daemon`、`ai update`。

mod cli_upgrade;
mod daemon;
mod doctor;
mod export;
mod query;
mod summon;
mod update;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::core::{Config, SessionId, Vendor};
use crate::host;

#[derive(Debug, Parser)]
#[command(name = "ai", version, about = "会话台：按厂商、按项目列出本机 AI 对话")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// 检查各家 CLI、会话目录、额度渠道、终端和守护进程
    Doctor,
    /// 在当前标签页接上一条托管对话（给新标签页用）
    Attach {
        /// 对话 id
        id: SessionId,
    },
    /// 把一条对话导出成 Markdown（不给 -o 就打到标准输出）
    Export {
        /// 对话 id
        id: SessionId,
        /// 存到这个文件（已有就覆盖）
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// 列出对话（只读，不进界面）
    ///
    /// 最近的在前，一条一行：时间、厂商、账号、状态、项目、标题、id。读会话台上次扫到的索引，不扫描、不改文件。
    List {
        /// 只要这一家（claude、codex、pi、hermes、kimi、gemini、opencode、antigravity）
        #[arg(long, value_name = "名", value_parser = query::parse_vendor)]
        vendor: Option<Vendor>,
        /// 输出 JSON 数组（字段见 README「命令行查询」）
        #[arg(long)]
        json: bool,
    },
    /// 搜对话标题和正文（只读，不进界面）
    ///
    /// 结果和顺序同界面里按 / 搜；正文命中的，下一行缩进写命中的那段正文。读会话台上次扫到的索引，不扫描、不改文件。
    Search {
        /// 要搜的词（几个词用空格分开，每个都要命中）
        #[arg(required = true, num_args = 1..)]
        words: Vec<String>,
        /// 输出 JSON 数组（字段见 README「命令行查询」）
        #[arg(long)]
        json: bool,
    },
    /// 启动守护进程（通常由 ai 自动拉起）
    Daemon,
    /// 升级会话台自己（不带参数，按安装方式：Homebrew、cargo install 或直接下载）；带厂商名或 --all 升级各家 CLI
    ///
    /// `ai update claude`：按本机装法跑那家的官方升级命令（不用 sudo，跑完确认 PATH 上第一个就是新版）。
    /// `ai update --all`：升界面上「更新」按钮会升的那几家。
    Update {
        /// 升这一家 CLI（claude、codex、pi、hermes、kimi、gemini、opencode、antigravity）
        #[arg(value_name = "家", value_parser = query::parse_vendor, conflicts_with = "all")]
        vendor: Option<Vendor>,
        /// 升全部能升的各家 CLI
        #[arg(long)]
        all: bool,
    },
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match &cli.command {
        Some(Command::Attach { id }) => attach(id),
        Some(Command::Export { id, output }) => export(id, output.as_deref()),
        Some(Command::List { vendor, json }) => query("ai list", query::list(*vendor), *json, ""),
        Some(Command::Search { words, json }) => {
            let q = words.join(" ");
            query("ai search", query::search(&q), *json, &q)
        }
        Some(Command::Daemon) => daemon(),
        Some(Command::Doctor) => doctor(),
        Some(Command::Update {
            vendor: None,
            all: false,
        }) => update(),
        Some(Command::Update { vendor, .. }) => update_clis(*vendor),
        None => board(),
    }
}

fn doctor() -> ExitCode {
    match data_dir() {
        Ok(dir) => {
            print!("{}", doctor::report(&dir));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ai doctor: {e}");
            ExitCode::FAILURE
        }
    }
}

fn data_dir() -> Result<PathBuf, String> {
    Config::data_dir().ok_or_else(|| "取不到 HOME，找不到数据目录".to_owned())
}

fn attach(id: &SessionId) -> ExitCode {
    let result = data_dir().and_then(|dir| {
        let exe = std::env::current_exe().map_err(|e| format!("找不到自己的路径: {e}"))?;
        let mut launch = std::process::Command::new(exe);
        launch.arg("daemon");
        host::client::attach_terminal(&dir, id, launch).map_err(|e| e.to_string())
    });
    match result {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("ai attach: {e}");
            ExitCode::FAILURE
        }
    }
}

fn export(id: &SessionId, output: Option<&std::path::Path>) -> ExitCode {
    match export::run(id, output) {
        Ok(Some(path)) => {
            eprintln!("已存到 {}", path.display());
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ai export: {e}");
            ExitCode::FAILURE
        }
    }
}

/// 打出 `ai list` / `ai search` 的结果。没有结果时 JSON 是 `[]`，纯文本什么都不打、在标准错误说一句。
fn query(name: &str, rows: std::io::Result<Vec<query::Row>>, json: bool, words: &str) -> ExitCode {
    use std::io::Write;
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{name}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let out = if json {
        query::json(&rows)
    } else {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Ok(query::text(&rows, &home))
    };
    if rows.is_empty() && !json {
        if words.is_empty() {
            eprintln!("{name}: 没有对话");
        } else {
            eprintln!("{name}: 没搜到「{words}」");
        }
    }
    match out.and_then(|o| std::io::stdout().lock().write_all(o.as_bytes())) {
        Ok(()) => ExitCode::SUCCESS,
        // 下游（head 之类）先关了管道：不算错。
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{name}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn update() -> ExitCode {
    match update::run() {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ai update: {e}");
            ExitCode::FAILURE
        }
    }
}

fn update_clis(vendor: Option<Vendor>) -> ExitCode {
    let dir = match data_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ai update: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut ok = true;
    cli_upgrade::run(vendor, &dir, |(text, good)| {
        ok &= good;
        println!("{text}");
    });
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn board() -> ExitCode {
    // 有没有新版在后台线程里查（开关在界面的设置里），取不到数据目录就不查。
    let check: crate::ui::UpdateCheck =
        Box::new(|| Config::data_dir().and_then(|dir| update::board_check(&dir)));
    match crate::ui::run(check) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ai: {e}");
            ExitCode::FAILURE
        }
    }
}

fn daemon() -> ExitCode {
    match data_dir().and_then(|dir| daemon::run(&dir).map_err(|e| e.to_string())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ai daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("ai").chain(args.iter().copied()))
    }

    #[test]
    fn definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_all_subcommands() {
        assert_eq!(parse(&[]).unwrap().command, None);
        assert_eq!(parse(&["doctor"]).unwrap().command, Some(Command::Doctor));
        assert_eq!(parse(&["daemon"]).unwrap().command, Some(Command::Daemon));
        assert_eq!(
            parse(&["update"]).unwrap().command,
            Some(Command::Update {
                vendor: None,
                all: false
            })
        );
        assert_eq!(
            parse(&["update", "Claude"]).unwrap().command,
            Some(Command::Update {
                vendor: Some(Vendor::Claude),
                all: false
            })
        );
        assert_eq!(
            parse(&["update", "--all"]).unwrap().command,
            Some(Command::Update {
                vendor: None,
                all: true
            })
        );
        assert!(parse(&["update", "claude", "--all"]).is_err());
        assert!(parse(&["update", "nope"]).is_err());
        assert_eq!(
            parse(&["attach", "abc-123"]).unwrap().command,
            Some(Command::Attach {
                id: SessionId::from("abc-123")
            })
        );
    }

    #[test]
    fn parses_export() {
        assert_eq!(
            parse(&["export", "abc-123"]).unwrap().command,
            Some(Command::Export {
                id: SessionId::from("abc-123"),
                output: None
            })
        );
        assert_eq!(
            parse(&["export", "abc-123", "-o", "x.md"]).unwrap().command,
            Some(Command::Export {
                id: SessionId::from("abc-123"),
                output: Some(PathBuf::from("x.md"))
            })
        );
        assert!(parse(&["export"]).is_err());
    }

    #[test]
    fn parses_list_and_search() {
        assert_eq!(
            parse(&["list"]).unwrap().command,
            Some(Command::List {
                vendor: None,
                json: false
            })
        );
        assert_eq!(
            parse(&["list", "--vendor", "Kimi Code", "--json"])
                .unwrap()
                .command,
            Some(Command::List {
                vendor: Some(Vendor::Kimi),
                json: true
            })
        );
        assert!(parse(&["list", "--vendor", "nope"]).is_err());
        assert_eq!(
            parse(&["search", "登录", "bug", "--json"]).unwrap().command,
            Some(Command::Search {
                words: vec!["登录".into(), "bug".into()],
                json: true
            })
        );
        assert!(parse(&["search"]).is_err());
    }

    #[test]
    fn attach_requires_id() {
        assert!(parse(&["attach"]).is_err());
    }

    #[test]
    fn rejects_unknown_subcommand() {
        assert!(parse(&["bogus"]).is_err());
    }

    #[test]
    fn version_flag_is_handled_by_clap() {
        let err = parse(&["--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }
}
