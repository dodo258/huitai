//! 识别会话台跑在哪个终端里。

use std::fmt;

/// 会话台认识的终端。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    /// Ghostty：AppleScript 开新标签页。
    Ghostty,
    /// Kaku（WezTerm 分支）：`kaku cli spawn` 开新标签页。
    Kaku,
    /// 其他终端（含 tmux 里）：在当前窗口 attach。带上 `TERM_PROGRAM` 便于 doctor 报告。
    Other(Option<String>),
}

impl Terminal {
    /// 按环境变量识别。`env` 取一个变量的值，便于测试时传假环境。
    ///
    /// Ghostty 设 `TERM_PROGRAM=ghostty`，Kaku 0.30 设 `TERM_PROGRAM=Kaku`（2026-11-20 本机实测）。
    /// 在 tmux 里 `TERM_PROGRAM=tmux`，开新标签页会落到 tmux 外面，按其他终端处理。
    pub fn detect(env: impl Fn(&str) -> Option<String>) -> Self {
        let program = env("TERM_PROGRAM").filter(|p| !p.is_empty());
        match program.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some("ghostty") => Terminal::Ghostty,
            Some("kaku") => Terminal::Kaku,
            _ => Terminal::Other(program),
        }
    }

    /// 识别当前进程所在的终端。
    pub fn current() -> Self {
        Self::detect(|k| std::env::var(k).ok())
    }

    /// 按名字取（配置里的首选终端：`"ghostty"`、`"kaku"`，大小写不限）。
    pub fn from_name(name: &str) -> Self {
        Self::detect(|_| Some(name.to_owned()))
    }

    /// 能不能开新标签页。
    pub fn has_tabs(&self) -> bool {
        !matches!(self, Terminal::Other(_))
    }
}

impl fmt::Display for Terminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Terminal::Ghostty => f.write_str("Ghostty"),
            Terminal::Kaku => f.write_str("Kaku"),
            Terminal::Other(Some(p)) => f.write_str(p),
            Terminal::Other(None) => f.write_str("未知终端"),
        }
    }
}
