//! `ai doctor`：自检。三家 CLI 装好没有、会话目录读不读得了、额度每个渠道能不能用、
//! 当前终端能不能开新标签页、守护进程是哪一版。只读，不开窗口、不弹授权框。

use std::fmt::Write as _;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::core::{Config, QuotaReading, QuotaValue, Vendor};
use crate::host::{self, Build, DaemonInfo, version};
use crate::opener::{KakuOpener, Terminal, exec};
use crate::quota::{ChannelReport, QuotaReader};
use crate::sources::{self, Source, Trash};
use crate::ui::{text, view};

/// 找 CLI 用的登录 shell 最多等多久（nvm、oh-my-zsh 这类配置启动慢）。
const CLI_TIMEOUT: Duration = Duration::from_secs(15);
/// `kaku cli list` 最多等多久。
const KAKU_TIMEOUT: Duration = Duration::from_secs(5);
/// 登录 shell 输出里标出 CLI 路径的那一行（shell 配置自己打印的东西在它前面，跳过）。
const PATH_MARK: &str = "HUITAI_PATH=";

/// 一项检查的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Ok,
    /// 能用，但有要留意的地方。
    Warn,
    Fail,
    /// 不适用（比如没用过这家）。
    Skip,
}

impl Mark {
    fn symbol(self) -> &'static str {
        match self {
            Mark::Ok => "✓",
            Mark::Warn => "!",
            Mark::Fail => "✗",
            Mark::Skip => "–",
        }
    }
}

/// 报告里的一行：结论、名字、说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub mark: Mark,
    pub name: String,
    pub detail: String,
}

fn item(mark: Mark, name: impl Into<String>, detail: impl Into<String>) -> Item {
    Item {
        mark,
        name: name.into(),
        detail: detail.into(),
    }
}

/// 一节：标题和若干行。
pub struct Section {
    pub title: String,
    pub items: Vec<Item>,
}

/// 本机环境下跑全部检查，返回给人看的文字。
pub fn report(dir: &Path) -> String {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_default();
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_owned());
    let env = |k: &str| std::env::var(k).ok();
    let srcs = sources::default_sources(&home, Trash::new(dir.join("trash")));
    let config = Config::load(&dir.join("config.json"));

    // 找 CLI 要起登录 shell，和扫描、读额度互不相干，并行做。
    let (clis, scans, quotas) = thread::scope(|s| {
        let clis = s.spawn(|| cli_section(&shell, &home, &srcs));
        let scans = s.spawn(|| sessions_section(&srcs, &home));
        let quotas = quota_section();
        (
            clis.join().unwrap_or_else(|_| crashed("三家 CLI")),
            scans.join().unwrap_or_else(|_| crashed("会话目录")),
            quotas,
        )
    });
    let me = version::current();
    let sections = [
        clis,
        scans,
        quotas,
        terminal_section(&Terminal::current(), &env, config.as_ref().ok(), kaku_probe),
        Section {
            title: "守护进程".to_owned(),
            items: vec![daemon_item(me, host::probe(dir))],
        },
    ];
    let mut out = format!("会话台 {}，协议 {}\n", me.label(), me.protocol);
    if let Err(e) = &config {
        let _ = writeln!(out, "{} 设置文件读不了：{e}", Mark::Fail.symbol());
    }
    out + &render(&sections)
}

fn crashed(title: &str) -> Section {
    Section {
        title: title.to_owned(),
        items: vec![item(Mark::Fail, "检查", "检查本身出错了")],
    }
}

/// 排版：每节一个标题，下面每行「标记 名字  说明」，名字按显示宽度对齐。
pub fn render(sections: &[Section]) -> String {
    let mut out = String::new();
    for sec in sections {
        let _ = writeln!(out, "\n{}", sec.title);
        let w = sec
            .items
            .iter()
            .map(|i| text::width(&i.name))
            .max()
            .unwrap_or(0);
        for i in &sec.items {
            let pad = " ".repeat(w - text::width(&i.name));
            let line = format!("  {} {}{pad}  {}", i.mark.symbol(), i.name, i.detail);
            let _ = writeln!(out, "{}", line.trim_end());
        }
    }
    out
}

// ---- 三家 CLI ----

/// 和开对话时一样用登录 shell（`$SHELL -lc`）在家目录找，找得到再问版本。
fn cli_section(shell: &str, home: &Path, srcs: &[Box<dyn Source>]) -> Section {
    let items = thread::scope(|s| {
        let jobs: Vec<_> = srcs
            .iter()
            .map(|src| {
                let program = src.new_cmd(home).program;
                s.spawn(move || {
                    let found = exec::run(
                        Path::new(shell),
                        &["-lc", &find_script(&program)],
                        CLI_TIMEOUT,
                    )
                    .map(|out| parse_find(&out));
                    cli_item(src.vendor(), &program, found)
                })
            })
            .collect();
        jobs.into_iter()
            .map(|j| {
                j.join()
                    .unwrap_or_else(|_| item(Mark::Fail, "?", "检查本身出错了"))
            })
            .collect()
    });
    Section {
        title: "三家 CLI（用登录 shell 找，和开对话时一样）".to_owned(),
        items,
    }
}

/// 登录 shell 里跑的那一行：打印路径，找得到就打印版本（stderr 也收，有的 CLI 把版本写在那里）。
pub fn find_script(program: &str) -> String {
    format!(
        "p=$(command -v {program}); printf '\\n{PATH_MARK}%s\\n' \"$p\"; \
         if [ -n \"$p\" ]; then {program} --version 2>&1; fi; exit 0"
    )
}

/// 找到的 CLI：路径和版本那一行（问不出版本时为空）。
#[derive(Debug, PartialEq, Eq)]
pub enum Found {
    Missing,
    At { path: String, version: String },
}

/// 解析 [`find_script`] 的输出。标记行之前是 shell 配置自己打印的东西，不看。
pub fn parse_find(out: &str) -> Found {
    let mut lines = out.lines();
    let Some(path) = lines.by_ref().find_map(|l| l.strip_prefix(PATH_MARK)) else {
        return Found::Missing;
    };
    let path = path.trim();
    if path.is_empty() {
        return Found::Missing;
    }
    let version = lines
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    Found::At {
        path: path.to_owned(),
        version: version.to_owned(),
    }
}

pub fn cli_item(vendor: Vendor, program: &str, found: io::Result<Found>) -> Item {
    let name = format!("{} {}", vendor.mark(), vendor.name());
    match found {
        Ok(Found::At { path, version }) if version.is_empty() => {
            item(Mark::Warn, name, format!("{path}（问不出版本）"))
        }
        Ok(Found::At { path, version }) => item(Mark::Ok, name, format!("{version}  {path}")),
        Ok(Found::Missing) => item(
            Mark::Fail,
            name,
            format!("没找到 {program} 命令：没装，或登录 shell 的 PATH 里没有它"),
        ),
        Err(e) => item(Mark::Fail, name, format!("登录 shell 没跑完：{e}")),
    }
}

// ---- 会话目录 ----

/// 每家扫一遍：目录在不在、能不能读、列出多少条、有几个文件读不了。
fn sessions_section(srcs: &[Box<dyn Source>], home: &Path) -> Section {
    let items = srcs
        .iter()
        .map(|src| {
            let root = src.watch_roots().into_iter().next().unwrap_or_default();
            let started = Instant::now();
            let found = std::fs::read_dir(&root).map(|_| {
                let scan = src.scan();
                (scan.sessions.len(), scan.problems)
            });
            sessions_item(
                src.vendor(),
                &text::tilde(&root, home),
                found,
                started.elapsed(),
            )
        })
        .collect();
    Section {
        title: "会话目录（只读）".to_owned(),
        items,
    }
}

pub fn sessions_item(
    vendor: Vendor,
    root: &str,
    found: io::Result<(usize, Vec<String>)>,
    took: Duration,
) -> Item {
    let name = vendor.name();
    match found {
        Err(e) if e.kind() == ErrorKind::NotFound => item(
            Mark::Skip,
            name,
            format!("{root} 不存在（没用过这家就是这样）"),
        ),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => item(
            Mark::Fail,
            name,
            format!("{root} 没有读权限：{e}（终端要有「完全磁盘访问权限」或改目录权限）"),
        ),
        Err(e) => item(Mark::Fail, name, format!("{root} 读不了：{e}")),
        Ok((n, problems)) => {
            let mut detail = format!("{root}  {n} 条对话，扫一遍 {} ms", took.as_millis());
            if problems.is_empty() {
                return item(Mark::Ok, name, detail);
            }
            let _ = write!(
                detail,
                "；{} 个文件读不了，第一个：{}",
                problems.len(),
                problems[0]
            );
            item(Mark::Warn, name, detail)
        }
    }
}

// ---- 额度 ----

/// 每家每个渠道都试一遍（Claude 的用量接口会联网问一次，和会话台顶栏一样）。
fn quota_section() -> Section {
    let Some(reader) = QuotaReader::system() else {
        return Section {
            title: "额度渠道".to_owned(),
            items: vec![item(Mark::Fail, "额度", "取不到 HOME，读不了")],
        };
    };
    let now = SystemTime::now();
    let items = Vendor::ALL
        .iter()
        .flat_map(|&v| quota_items(v, &reader.probe(v), now))
        .collect();
    Section {
        title: "额度渠道（按顺序试，前一个取不到就用下一个）".to_owned(),
        items,
    }
}

/// 一家的各渠道。顶栏用的是第一个能用的，标出来。
pub fn quota_items(vendor: Vendor, reports: &[ChannelReport], now: SystemTime) -> Vec<Item> {
    let used = reports.iter().position(|r| r.result.is_ok());
    reports
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let name = format!("{} {}", vendor.name(), r.source);
            match &r.result {
                Ok(reading) => {
                    let tag = if Some(i) == used {
                        "（顶栏用这个）"
                    } else {
                        ""
                    };
                    let detail = format!(
                        "{}，{}{tag}",
                        reading_text(reading),
                        text::ago(reading.at, now)
                    );
                    item(Mark::Ok, name, detail)
                }
                Err(e) => item(Mark::Skip, name, format!("取不到：{e}")),
            }
        })
        .chain(used.is_none().then(|| {
            item(
                Mark::Warn,
                vendor.name(),
                "每个渠道都取不到，顶栏显示「暂时取不到」",
            )
        }))
        .collect()
}

fn reading_text(r: &QuotaReading) -> String {
    match &r.value {
        QuotaValue::Windows(ws) => ws
            .iter()
            .map(|w| format!("{} 已用 {:.0}%", w.label, w.used_percent))
            .collect::<Vec<_>>()
            .join(" · "),
        QuotaValue::TokensToday(n) => format!("今天 {} token", view::tokens(*n)),
        QuotaValue::Unavailable => "暂时取不到".to_owned(),
    }
}

// ---- 终端 ----

/// Ghostty 从 1.3 起有 AppleScript 字典，开新标签页靠它。
const GHOSTTY_MIN: (u32, u32) = (1, 3);

/// 当前终端能不能开新标签页；`kaku` 是问 Kaku 的办法（测试里换成假的）。
pub fn terminal_section(
    term: &Terminal,
    env: &dyn Fn(&str) -> Option<String>,
    config: Option<&Config>,
    kaku: fn(&Path) -> io::Result<String>,
) -> Section {
    let mut items = vec![terminal_item(term, env, kaku)];
    let preferred = config.and_then(|c| c.preferred_terminal.as_deref());
    items.push(item(
        Mark::Skip,
        "首选终端",
        match preferred {
            Some(p) => format!(
                "{}（开机打开、⌥空格新开会话台时用它）",
                Terminal::from_name(p)
            ),
            None => "跟随当前终端（开机打开、⌥空格新开会话台时用它；`,` 设置里可改）".to_owned(),
        },
    ));
    Section {
        title: "终端".to_owned(),
        items,
    }
}

fn terminal_item(
    term: &Terminal,
    env: &dyn Fn(&str) -> Option<String>,
    kaku: fn(&Path) -> io::Result<String>,
) -> Item {
    let name = format!("当前 {term}");
    match term {
        Terminal::Ghostty => {
            let v = env("TERM_PROGRAM_VERSION").unwrap_or_default();
            match major_minor(&v) {
                Some(mm) if mm < GHOSTTY_MIN => item(
                    Mark::Fail,
                    name,
                    format!(
                        "{v} 太旧：开新标签页要 1.3 以上（AppleScript），现在 ⏎ 会失败，先用 w 在当前窗口打开"
                    ),
                ),
                _ => item(
                    Mark::Ok,
                    name,
                    format!(
                        "{} 能开新标签页（AppleScript；第一次会弹「想要控制 Ghostty」的系统授权，点允许）",
                        if v.is_empty() { "版本未知，" } else { &v }
                    ),
                ),
            }
        }
        Terminal::Kaku => {
            let cli = KakuOpener::new(env).cli().to_path_buf();
            match kaku(&cli) {
                Ok(_) => item(
                    Mark::Ok,
                    name,
                    format!("能开新标签页（{} 连得上）", cli.display()),
                ),
                Err(e) => item(
                    Mark::Fail,
                    name,
                    format!("kaku cli 不通，⏎ 开不了新标签页：{e}"),
                ),
            }
        }
        Terminal::Other(Some(p)) if p.eq_ignore_ascii_case("tmux") => item(
            Mark::Warn,
            name,
            "在 tmux 里：⏎ 在当前窗口打开，不开新标签页（新标签页只支持 Ghostty、Kaku）",
        ),
        Terminal::Other(_) => item(
            Mark::Warn,
            name,
            "不支持开新标签页：⏎ 在当前窗口打开，对话结束回到会话台（新标签页只支持 Ghostty、Kaku）",
        ),
    }
}

/// 「1.4.0」「1.3.0-main+abc」→ (1, 4)。
fn major_minor(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// 问 Kaku 有哪些窗格：只读，Kaku 界面不在时报错，不另起后台 mux。
fn kaku_probe(cli: &Path) -> io::Result<String> {
    let args = KakuOpener::list_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    exec::run(cli, &args, KAKU_TIMEOUT)
}

// ---- 守护进程 ----

pub fn daemon_item(me: &Build, probe: io::Result<DaemonInfo>) -> Item {
    let mark = match &probe {
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            Mark::Skip
        }
        Err(_) => Mark::Fail,
        Ok(info) if info.build.as_ref().is_some_and(|b| same_build(me, b)) => Mark::Ok,
        Ok(_) => Mark::Warn,
    };
    item(mark, "状态", describe(me, probe))
}

fn same_build(me: &Build, b: &Build) -> bool {
    b.version == me.version && b.exe == me.exe && b.built == me.built
}

/// 守护进程那一行。
pub fn describe(me: &Build, probe: io::Result<DaemonInfo>) -> String {
    let info = match probe {
        Ok(info) => info,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            return "没在跑（打开对话时自动拉起）".to_owned();
        }
        Err(e) => return format!("连不上：{e}"),
    };
    let (who, verdict) = match &info.build {
        None => ("旧版（没有版本握手）".to_owned(), older(info.hosted)),
        Some(b) => {
            let verdict = if me.newer_than(b) {
                older(info.hosted)
            } else if b.newer_than(me) {
                "比当前程序新：你用的 ai 是旧的，看看 `which ai`".to_owned()
            } else if same_build(me, b) {
                "和当前程序一致".to_owned()
            } else {
                let exe = b
                    .exe
                    .as_ref()
                    .map_or_else(|| "未知".to_owned(), |p| p.display().to_string());
                format!("版本号相同，但不是同一个程序文件（它是 {exe}）")
            };
            (format!("{}，协议 {}", b.label(), b.protocol), verdict)
        }
    };
    format!(
        "{who}，进程 {}，托管 {} 条对话 —— {verdict}",
        info.pid, info.hosted
    )
}

fn older(hosted: usize) -> String {
    if hosted == 0 {
        "比当前程序旧，下次打开对话时换成新版".to_owned()
    } else {
        format!("比当前程序旧，它托管的 {hosted} 条对话结束后自动换新")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::QuotaWindow;

    fn build(version: &str, built: u64) -> Build {
        Build {
            protocol: 1,
            version: version.to_owned(),
            exe: Some(PathBuf::from("/bin/ai")),
            built,
        }
    }

    fn info(build: Option<Build>, hosted: usize) -> io::Result<DaemonInfo> {
        Ok(DaemonInfo {
            build,
            pid: 42,
            hosted,
        })
    }

    #[test]
    fn says_whether_daemon_matches() {
        let me = build("0.2.0", 10);
        let same = describe(&me, info(Some(me.clone()), 1));
        assert!(
            same.contains("0.2.0") && same.contains("和当前程序一致"),
            "{same}"
        );
        let old = describe(&me, info(Some(build("0.1.0", 10)), 2));
        assert!(
            old.contains("0.1.0") && old.contains("2 条对话结束后自动换新"),
            "{old}"
        );
        let reinstalled = describe(&me, info(Some(build("0.2.0", 5)), 0));
        assert!(
            reinstalled.contains("下次打开对话时换成新版"),
            "{reinstalled}"
        );
        let newer = describe(&me, info(Some(build("0.3.0", 1)), 0));
        assert!(newer.contains("比当前程序新"), "{newer}");
        let legacy = describe(&me, info(None, 3));
        assert!(
            legacy.contains("没有版本握手") && legacy.contains("3 条"),
            "{legacy}"
        );
    }

    #[test]
    fn says_when_daemon_is_not_running() {
        let me = build("0.2.0", 10);
        let gone = describe(&me, Err(io::Error::from(ErrorKind::ConnectionRefused)));
        assert!(gone.contains("没在跑"), "{gone}");
        let gone = describe(&me, Err(io::Error::from(ErrorKind::NotFound)));
        assert!(gone.contains("没在跑"), "{gone}");
    }

    #[test]
    fn daemon_marks() {
        let me = build("0.2.0", 10);
        assert_eq!(daemon_item(&me, info(Some(me.clone()), 0)).mark, Mark::Ok);
        assert_eq!(daemon_item(&me, info(None, 0)).mark, Mark::Warn);
        assert_eq!(
            daemon_item(&me, Err(io::Error::from(ErrorKind::NotFound))).mark,
            Mark::Skip
        );
        assert_eq!(
            daemon_item(&me, Err(io::Error::other("坏了"))).mark,
            Mark::Fail
        );
    }

    #[test]
    fn find_script_runs_in_a_real_shell() {
        let out = exec::run(
            Path::new("/bin/sh"),
            &["-c", &find_script("sh")],
            CLI_TIMEOUT,
        )
        .unwrap();
        assert!(
            matches!(parse_find(&out), Found::At { ref path, .. } if path.ends_with("/sh")),
            "{out}"
        );
        let out = exec::run(
            Path::new("/bin/sh"),
            &["-c", &find_script("huitai-no-such-cli")],
            CLI_TIMEOUT,
        )
        .unwrap();
        assert_eq!(parse_find(&out), Found::Missing, "{out}");
    }

    #[test]
    fn parse_find_skips_shell_noise() {
        let out = "欢迎回来\nHUITAI_PATH=/opt/bin/claude\n\n2.1.330 (Claude Code)\n";
        assert_eq!(
            parse_find(out),
            Found::At {
                path: "/opt/bin/claude".to_owned(),
                version: "2.1.330 (Claude Code)".to_owned()
            }
        );
        assert_eq!(parse_find("HUITAI_PATH=\n"), Found::Missing);
        assert_eq!(parse_find("没有标记"), Found::Missing);
    }

    #[test]
    fn cli_items() {
        let ok = cli_item(
            Vendor::Codex,
            "codex",
            Ok(Found::At {
                path: "/b/codex".to_owned(),
                version: "codex-cli 0.171.0".to_owned(),
            }),
        );
        assert_eq!(ok.mark, Mark::Ok);
        assert!(ok.detail.contains("codex-cli 0.171.0"), "{ok:?}");
        let missing = cli_item(Vendor::Pi, "pi", Ok(Found::Missing));
        assert_eq!(missing.mark, Mark::Fail);
        assert!(missing.detail.contains("没找到 pi"), "{missing:?}");
        let timeout = cli_item(Vendor::Pi, "pi", Err(io::Error::from(ErrorKind::TimedOut)));
        assert_eq!(timeout.mark, Mark::Fail);
    }

    #[test]
    fn session_items() {
        let t = Duration::from_millis(12);
        let gone = sessions_item(
            Vendor::Pi,
            "~/.pi",
            Err(io::Error::from(ErrorKind::NotFound)),
            t,
        );
        assert_eq!(gone.mark, Mark::Skip);
        let denied = sessions_item(
            Vendor::Pi,
            "~/.pi",
            Err(io::Error::from(ErrorKind::PermissionDenied)),
            t,
        );
        assert_eq!(denied.mark, Mark::Fail);
        let ok = sessions_item(Vendor::Claude, "~/.claude/projects", Ok((3, vec![])), t);
        assert_eq!(ok.mark, Mark::Ok);
        assert!(ok.detail.contains("3 条对话"), "{ok:?}");
        let warn = sessions_item(
            Vendor::Claude,
            "~/.claude/projects",
            Ok((3, vec!["a.jsonl: 坏了".to_owned(), "b".to_owned()])),
            t,
        );
        assert_eq!(warn.mark, Mark::Warn);
        assert!(
            warn.detail.contains("2 个文件读不了，第一个：a.jsonl"),
            "{warn:?}"
        );
    }

    #[test]
    fn quota_marks_the_channel_in_use() {
        let now = SystemTime::now();
        let reading = |value| QuotaReading {
            value,
            source: String::new(),
            at: now,
        };
        let reports = vec![
            ChannelReport {
                source: "状态栏",
                result: Err("缓存过期".to_owned()),
            },
            ChannelReport {
                source: "用量接口",
                result: Ok(reading(QuotaValue::Windows(vec![QuotaWindow {
                    label: "5 小时".to_owned(),
                    used_percent: 12.4,
                    window_minutes: Some(300),
                    resets_at: None,
                }]))),
            },
            ChannelReport {
                source: "本机统计",
                result: Ok(reading(QuotaValue::TokensToday(12_345))),
            },
        ];
        let items = quota_items(Vendor::Claude, &reports, now);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].mark, Mark::Skip);
        assert!(items[0].detail.contains("缓存过期"));
        assert!(
            items[1].detail.contains("5 小时 已用 12%") && items[1].detail.contains("顶栏用这个"),
            "{:?}",
            items[1]
        );
        assert!(
            items[2].detail.contains("今天 1.2 万 token") && !items[2].detail.contains("顶栏"),
            "{:?}",
            items[2]
        );

        let none = quota_items(
            Vendor::Pi,
            &[ChannelReport {
                source: "本机统计",
                result: Err("没有".to_owned()),
            }],
            now,
        );
        assert_eq!(none.last().unwrap().mark, Mark::Warn);
    }

    fn no_kaku(_: &Path) -> io::Result<String> {
        Err(io::Error::other("Kaku 没开"))
    }

    fn yes_kaku(_: &Path) -> io::Result<String> {
        Ok("[]".to_owned())
    }

    #[test]
    fn terminals() {
        let env =
            |v: &'static str| move |k: &str| (k == "TERM_PROGRAM_VERSION").then(|| v.to_owned());
        let first = |t: &Terminal, e: &dyn Fn(&str) -> Option<String>, k| {
            terminal_section(t, e, None, k).items.remove(0)
        };
        assert_eq!(
            first(&Terminal::Ghostty, &env("1.4.0"), no_kaku).mark,
            Mark::Ok
        );
        assert_eq!(
            first(&Terminal::Ghostty, &env("1.3.0-main+abc"), no_kaku).mark,
            Mark::Ok
        );
        assert_eq!(
            first(&Terminal::Ghostty, &env("1.2.3"), no_kaku).mark,
            Mark::Fail
        );
        assert_eq!(first(&Terminal::Kaku, &env(""), yes_kaku).mark, Mark::Ok);
        let down = first(&Terminal::Kaku, &env(""), no_kaku);
        assert!(
            down.mark == Mark::Fail && down.detail.contains("Kaku 没开"),
            "{down:?}"
        );
        let tmux = first(&Terminal::Other(Some("tmux".to_owned())), &env(""), no_kaku);
        assert!(
            tmux.mark == Mark::Warn && tmux.detail.contains("tmux"),
            "{tmux:?}"
        );
        assert_eq!(
            first(&Terminal::Other(None), &env(""), no_kaku).mark,
            Mark::Warn
        );

        let cfg = Config {
            preferred_terminal: Some("kaku".to_owned()),
            ..Config::default()
        };
        let sec = terminal_section(&Terminal::Ghostty, &env("1.4.0"), Some(&cfg), no_kaku);
        assert!(
            sec.items[1].detail.starts_with("Kaku"),
            "{:?}",
            sec.items[1]
        );
    }

    #[test]
    fn render_aligns_names() {
        let out = render(&[Section {
            title: "终端".to_owned(),
            items: vec![
                item(Mark::Ok, "当前 Ghostty", "能开"),
                item(Mark::Skip, "首选终端", "跟随"),
            ],
        }]);
        assert_eq!(
            out,
            "\n终端\n  ✓ 当前 Ghostty  能开\n  – 首选终端      跟随\n"
        );
    }
}
