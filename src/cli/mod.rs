//! 命令入口：`ai`、`ai doctor`、`ai attach <id>`、`ai daemon`、`ai update`。

mod daemon;
mod doctor;
mod summon;
mod update;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::core::{Config, SessionId};
use crate::host;

#[derive(Debug, Parser)]
#[command(name = "ai", version, about = "会话台：按厂商、按项目列出本机 AI 对话")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// 检查三家 CLI、会话目录、额度渠道、终端和守护进程
    Doctor,
    /// 在当前标签页接上一条托管对话（给新标签页用）
    Attach {
        /// 对话 id
        id: SessionId,
    },
    /// 启动守护进程（通常由 ai 自动拉起）
    Daemon,
    /// 升级到最新版（按安装方式：Homebrew、cargo install 或直接下载）
    Update,
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match &cli.command {
        Some(Command::Attach { id }) => attach(id),
        Some(Command::Daemon) => daemon(),
        Some(Command::Doctor) => doctor(),
        Some(Command::Update) => update(),
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
        assert_eq!(parse(&["update"]).unwrap().command, Some(Command::Update));
        assert_eq!(
            parse(&["attach", "abc-123"]).unwrap().command,
            Some(Command::Attach {
                id: SessionId::from("abc-123")
            })
        );
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
