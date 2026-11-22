//! 识别会话台跑在哪个终端里。

use std::fmt;

/// 会话台认识的终端。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    /// Ghostty：AppleScript 开新标签页。
    Ghostty,
    /// Kaku（WezTerm 分支）：`kaku cli spawn` 开新标签页。
    Kaku,
    /// cmux（基于 Ghostty 的终端）：`cmux new-surface` 开新标签页。它只接受在它里面启动的进程控制，
    /// 所以只有在 cmux 里跑的会话台能开；开机打开、全局呼出键（从外面驱动）不支持它。
    Cmux,
    /// 其他终端（含 tmux 里）：在当前窗口 attach。带上 `TERM_PROGRAM` 便于 doctor 报告。
    Other(Option<String>),
}

impl Terminal {
    /// 按环境变量识别。`env` 取一个变量的值，便于测试时传假环境。
    ///
    /// Ghostty 设 `TERM_PROGRAM=ghostty`，Kaku 0.30 设 `TERM_PROGRAM=Kaku`（2026-11-20 本机实测）。
    /// 在 tmux 里 `TERM_PROGRAM=tmux`，开新标签页会落到 tmux 外面，按其他终端处理。
    /// cmux 0.65 里 `TERM_PROGRAM=ghostty`（它内嵌 Ghostty），只能靠它给每个标签页设的 `CMUX_SURFACE_ID`
    /// 认出来；不先认它，⏎ 会去指挥另一个独立的 Ghostty 程序（2026-11-22 本机实测）。只认这个标签页身份，
    /// 不认 `CMUX_SOCKET_PATH`：别的终端里为了用 cmux 命令行也可能设它（审计第 23 条）。
    pub fn detect(env: impl Fn(&str) -> Option<String>) -> Self {
        let program = env("TERM_PROGRAM").filter(|p| !p.is_empty());
        let lower = program.as_deref().map(str::to_ascii_lowercase);
        let in_cmux = env("CMUX_SURFACE_ID").is_some_and(|v| !v.is_empty());
        match lower.as_deref() {
            Some("tmux") => Terminal::Other(program),
            Some("cmux") => Terminal::Cmux,
            Some("ghostty") | None if in_cmux => Terminal::Cmux,
            Some("ghostty") => Terminal::Ghostty,
            Some("kaku") => Terminal::Kaku,
            _ => Terminal::Other(program),
        }
    }

    /// 识别当前进程所在的终端。
    pub fn current() -> Self {
        Self::detect(|k| std::env::var(k).ok())
    }

    /// 按名字取（配置里的首选终端：`"ghostty"`、`"kaku"`、`"cmux"`，大小写不限）。
    pub fn from_name(name: &str) -> Self {
        Self::detect(|k| (k == "TERM_PROGRAM").then(|| name.to_owned()))
    }

    /// 能不能开新标签页。
    pub fn has_tabs(&self) -> bool {
        !matches!(self, Terminal::Other(_))
    }

    /// 能不能从终端外面（开机登录项、守护进程的全局呼出键）开标签页。cmux 不接受外面的进程控制。
    pub fn drivable_from_outside(&self) -> bool {
        matches!(self, Terminal::Ghostty | Terminal::Kaku)
    }

    /// 登记给守护进程的名字（[`Terminal::from_name`] 认得回来）。
    pub fn name(&self) -> Option<String> {
        match self {
            Terminal::Ghostty => Some("ghostty".to_owned()),
            Terminal::Kaku => Some("kaku".to_owned()),
            Terminal::Cmux => Some("cmux".to_owned()),
            Terminal::Other(p) => p.clone(),
        }
    }
}

impl fmt::Display for Terminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Terminal::Ghostty => f.write_str("Ghostty"),
            Terminal::Kaku => f.write_str("Kaku"),
            Terminal::Cmux => f.write_str("cmux"),
            Terminal::Other(Some(p)) => f.write_str(p),
            Terminal::Other(None) => f.write_str("未知终端"),
        }
    }
}
