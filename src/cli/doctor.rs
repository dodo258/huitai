//! `ai doctor`：自检。各家 CLI 装好没有、怎么装的、有没有新版、会怎么升级，会话目录读不读得了、额度每个渠道能不能用、余额和订阅额度各渠道通不通、
//! Claude 会删多少天没动的对话、会话台给它们的备份占多大，当前终端能不能开新标签页、守护进程是哪一版。只读，不开窗口、不弹授权框。

use std::fmt::Write as _;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::clis;
use crate::core::{Account, Config, QuotaReading, QuotaValue, Vendor, exec, release};
use crate::host::{self, Build, DaemonInfo, version};
use crate::opener::{CmuxOpener, KakuOpener, Terminal};
use crate::quota::{self, BalanceReport, ChannelReport, OriginAccount, QuotaReader};
use crate::sources::claude::{self, RetentionFrom};
use crate::sources::{self, Source, Trash, backup};
use crate::ui::{balance, text, view};

/// `kaku cli list` 最多等多久。
const KAKU_TIMEOUT: Duration = Duration::from_secs(5);

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

    // 关了「检查新版本」的，各家 CLI 也不联网查最新版。
    let check = config.as_ref().map_or(true, |c| c.check_updates)
        && !release::disabled_by_env(env(release::NO_CHECK_ENV).as_deref());
    // 找 CLI 要起登录 shell，和扫描、读额度互不相干，并行做。
    let (clis, scans, quotas, balances) = thread::scope(|s| {
        let clis = s.spawn(|| cli_section(&shell, &home, dir, check));
        let scans = s.spawn(|| sessions_section(&srcs, &home));
        let balances = s.spawn(|| balance_section(&home));
        let quotas = quota_section();
        (
            clis.join().unwrap_or_else(|_| crashed("各家 CLI")),
            scans.join().unwrap_or_else(|_| crashed("会话目录")),
            quotas,
            balances
                .join()
                .unwrap_or_else(|_| crashed("余额和订阅额度")),
        )
    });
    let me = version::current();
    let sections = [
        clis,
        scans,
        quotas,
        balances,
        claude_backup_section(
            &sources::local_accounts(&home),
            dir,
            &home,
            config.as_ref().is_ok_and(|c| c.backup_all_claude),
        ),
        terminal_section(&Terminal::current(), &env, config.as_ref().ok(), cli_probe),
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

// ---- Claude 对话保留和备份 ----

/// 每个 Claude 账号多少天没动的对话会被 Claude 删（只读它 `settings.json` 的 `cleanupPeriodDays`），
/// 会话台的备份有几份、占多大（`<数据目录>/backups/claude/`，各账号一起算）。
pub fn claude_backup_section(accounts: &[Account], dir: &Path, home: &Path, all: bool) -> Section {
    let claude: Vec<&Account> = accounts
        .iter()
        .filter(|a| a.vendor == Vendor::Claude)
        .collect();
    let mut items: Vec<Item> = claude
        .iter()
        .map(|a| {
            let r = claude::retention(&a.dir);
            let name = if claude.len() > 1 {
                format!("保留期（{}）", a.name)
            } else {
                "保留期".to_owned()
            };
            let said = format!("{} 天没动的 Claude 对话会被删", r.days);
            let settings = text::tilde(&a.dir.join("settings.json"), home);
            match r.from {
                RetentionFrom::Default => item(
                    Mark::Ok,
                    name,
                    format!("{said}（{settings} 没设 cleanupPeriodDays，官方默认 30）"),
                ),
                RetentionFrom::Set => item(
                    Mark::Ok,
                    name,
                    format!("{said}（{settings} 的 cleanupPeriodDays）"),
                ),
                RetentionFrom::Invalid(v) => item(
                    Mark::Warn,
                    name,
                    format!(
                        "{settings} 的 cleanupPeriodDays 不合法（{v}，要不小于 1 的整数），按默认 30 天算：{said}"
                    ),
                ),
            }
        })
        .collect();
    let root = dir.join(backup::BACKUP_DIR);
    let (n, bytes) = backup::usage(&root);
    let what = if all {
        "备全部 Claude 对话"
    } else {
        "只备置顶、改名、归档的（设置里可改成全部）"
    };
    items.push(item(
        Mark::Ok,
        "备份",
        format!(
            "{n} 份，占 {}，{what}；在 {}",
            size_label(bytes),
            text::tilde(&root, home)
        ),
    ));
    Section {
        title: "Claude 对话保留和备份".to_owned(),
        items,
    }
}

/// 「312 KB」「4.2 MB」「1.3 GB」。
fn size_label(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K * K {
        format!("{:.0} KB", (b / K).ceil())
    } else if b < K * K * K {
        format!("{:.1} MB", b / K / K)
    } else {
        format!("{:.1} GB", b / K / K / K)
    }
}

// ---- 各家 CLI ----

/// 和开对话时一样用登录 shell 的 PATH 找，认出装法和版本；开着「检查新版本」时再查最新版（20 小时缓存，10 秒超时）。
fn cli_section(shell: &str, home: &Path, dir: &Path, check: bool) -> Section {
    let title = "各家 CLI（用登录 shell 的 PATH 找，和开对话时一样）".to_owned();
    let login = match clis::login_env(Path::new(shell)) {
        Ok(l) => l,
        Err(e) => {
            return Section {
                title,
                items: vec![item(Mark::Fail, "登录 shell", format!("没跑完：{e}"))],
            };
        }
    };
    let installs = clis::probe(&login, home);
    let statuses = if check {
        clis::check(
            dir,
            installs,
            now_secs(),
            &clis::Bases::official(),
            clis::TIMEOUT,
        )
    } else {
        installs
            .into_iter()
            .map(|install| clis::Status {
                install,
                latest: None,
                seen_at: None,
            })
            .collect()
    };
    let last = clis::last_results(dir);
    let items = Vendor::ALL
        .iter()
        .map(|&v| match statuses.iter().find(|s| s.install.vendor == v) {
            Some(s) => cli_item(s, check, last.get(&v)),
            None => item(
                Mark::Fail,
                format!("{} {}", v.mark(), v.name()),
                format!(
                    "没找到 {} 命令：没装，或登录 shell 的 PATH 里没有它",
                    clis::program(v)
                ),
            ),
        })
        .collect();
    Section { title, items }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// 一家：装法 · 当前版本 · 最新版 · 会怎么升级，最后是路径；升级过的下一行接着写上次升级（自动还是手动、
/// 几点、成没成，没成带原文）。有新版、问不出版本、认不出装法是「!」。
pub fn cli_item(status: &clis::Status, checked: bool, last: Option<&clis::Outcome>) -> Item {
    let i = &status.install;
    let name = format!("{} {}", i.vendor.mark(), i.vendor.name());
    let current = i.version.as_deref().unwrap_or("问不出版本");
    let newer = status.newer();
    let latest = match (checked, newer, status.latest.as_deref()) {
        (false, _, _) => "没查最新版（关了检查新版本）".to_owned(),
        (true, Some(v), _) => format!("有新版 {v}"),
        (true, None, Some(_)) => "已是最新".to_owned(),
        (true, None, None) => "最新版没查到".to_owned(),
    };
    let plan = clis::plan(i, status.latest.as_deref());
    let mark = if newer.is_some() || i.version.is_none() || i.method == clis::Method::Unknown {
        Mark::Warn
    } else {
        Mark::Ok
    };
    let mut detail = format!(
        "{} · {current} · {latest} · 升级：{}  {}",
        i.method.label(i.vendor),
        plan.describe(),
        i.path.display()
    );
    if let Some(o) = last {
        let when = jiff::Timestamp::from_second(i64::try_from(o.at).unwrap_or(0))
            .map(|t| {
                let d = jiff::tz::TimeZone::system().to_datetime(t);
                format!(
                    "{:02}-{:02} {:02}:{:02}",
                    d.month(),
                    d.day(),
                    d.hour(),
                    d.minute()
                )
            })
            .unwrap_or_default();
        let how = if o.auto {
            "自动升级"
        } else {
            "手动升级"
        };
        let result = if o.ok {
            o.message.clone()
        } else {
            format!("没升上：{}", o.message)
        };
        // 原文可能有多行：缩进接在下面，和别的项对齐看得清。
        let result = result.replace('\n', "\n      ");
        detail.push_str(&format!(
            "\n      上次{how}（{when}，→ {}）：{result}",
            o.target
        ));
    }
    item(mark, name, detail)
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

/// 余额和订阅额度：本机各账号配置和环境变量里的每把密钥查一遍（有 5 分钟缓存）。
fn balance_section(home: &Path) -> Section {
    let report = quota::balances(&sources::local_accounts(home));
    Section {
        title: "余额和订阅额度（密钥只读自 pi、Hermes 配置和环境变量，只发给各自那家）".to_owned(),
        items: balance_items(&report, SystemTime::now()),
    }
}

/// 每把密钥一行：哪家、读数或取不到的原因、密钥在哪（不写密钥本身）；写成命令的列为跳过。
pub fn balance_items(report: &BalanceReport, now: SystemTime) -> Vec<Item> {
    let place = |o: &quota::KeyOrigin| match &o.account {
        Some(OriginAccount { vendor, name, .. }) => {
            format!("{} {name} · {}", vendor.name(), o.place)
        }
        None => o.place.clone(),
    };
    let mut items: Vec<Item> = report
        .rows
        .iter()
        .map(|r| {
            let note = if r.provider.unofficial() {
                "，非公开接口"
            } else {
                ""
            };
            let at = place(&r.origins[0]);
            match &r.result {
                Ok(b) => item(
                    Mark::Ok,
                    r.provider.name(),
                    format!(
                        "{}，{}（{at}{note}）",
                        balance::plain(&b.value),
                        text::ago(b.at, now)
                    ),
                ),
                Err(e) => item(
                    Mark::Warn,
                    r.provider.name(),
                    format!("取不到：{e}（{at}{note}）"),
                ),
            }
        })
        .collect();
    items.extend(
        report
            .skipped
            .iter()
            .map(|s| item(Mark::Skip, place(&s.origin), s.reason.clone())),
    );
    if report.rows.is_empty() {
        items.push(item(
            Mark::Skip,
            "余额",
            "没找到 DeepSeek、Moonshot、OpenRouter、Kimi Code 的密钥",
        ));
    }
    items
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

/// 当前终端能不能开新标签页；`kaku` 是问终端命令行工具（Kaku、cmux）的办法（测试里换成假的）。
pub fn terminal_section(
    term: &Terminal,
    env: &dyn Fn(&str) -> Option<String>,
    config: Option<&Config>,
    kaku: fn(&Path, &[String]) -> io::Result<String>,
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
    kaku: fn(&Path, &[String]) -> io::Result<String>,
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
            match kaku(&cli, &KakuOpener::list_args()) {
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
        Terminal::Cmux => {
            let cli = CmuxOpener::new(env).cli().to_path_buf();
            match kaku(&cli, &CmuxOpener::ping_args()) {
                Ok(_) => item(
                    Mark::Ok,
                    name,
                    "能开新标签页（cmux 命令行连得上）；开机打开、⌥空格新开会话台不支持 cmux",
                ),
                Err(e) => item(
                    Mark::Fail,
                    name,
                    format!("cmux 命令行不通，⏎ 开不了新标签页，先用 w 在当前窗口打开：{e}"),
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

/// 跑终端的命令行工具问一句只读的话（Kaku 列窗格、cmux ping），连不上就报错。
fn cli_probe(cli: &Path, args: &[String]) -> io::Result<String> {
    exec::run(cli, args, None, KAKU_TIMEOUT)
}

// ---- 守护进程 ----

pub fn daemon_item(me: &Build, probe: io::Result<DaemonInfo>) -> Item {
    let mark = match &probe {
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            Mark::Skip
        }
        Err(_) => Mark::Fail,
        Ok(info) if same_build(me, &info.build) => Mark::Ok,
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
    let b = &info.build;
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
    format!(
        "{}，协议 {}，进程 {}，托管 {} 条对话 —— {verdict}",
        b.label(),
        b.protocol,
        info.pid,
        info.hosted
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

    fn info(build: Build, hosted: usize) -> io::Result<DaemonInfo> {
        Ok(DaemonInfo {
            build,
            pid: 42,
            hosted,
        })
    }

    #[test]
    fn says_whether_daemon_matches() {
        let me = build("0.2.0", 10);
        let same = describe(&me, info(me.clone(), 1));
        assert!(
            same.contains("0.2.0") && same.contains("和当前程序一致"),
            "{same}"
        );
        let old = describe(&me, info(build("0.1.0", 10), 2));
        assert!(
            old.contains("0.1.0") && old.contains("2 条对话结束后自动换新"),
            "{old}"
        );
        let reinstalled = describe(&me, info(build("0.2.0", 5), 0));
        assert!(
            reinstalled.contains("下次打开对话时换成新版"),
            "{reinstalled}"
        );
        let newer = describe(&me, info(build("0.3.0", 1), 0));
        assert!(newer.contains("比当前程序新"), "{newer}");
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
        assert_eq!(daemon_item(&me, info(me.clone(), 0)).mark, Mark::Ok);
        assert_eq!(
            daemon_item(&me, info(build("0.1.0", 10), 0)).mark,
            Mark::Warn
        );
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
    fn cli_items() {
        let status = |version: Option<&str>, latest: Option<&str>| clis::Status {
            install: clis::Install {
                vendor: Vendor::Codex,
                path: PathBuf::from("/b/codex"),
                real: PathBuf::from("/b/lib/node_modules/@openai/codex/bin/codex.js"),
                method: clis::Method::Package {
                    manager: clis::Manager::Npm,
                    package: "@openai/codex",
                },
                version: version.map(str::to_owned),
                problem: None,
            },
            latest: latest.map(str::to_owned),
            seen_at: None,
        };
        let ok = cli_item(&status(Some("0.171.0"), Some("0.171.0")), true, None);
        assert_eq!(ok.mark, Mark::Ok);
        assert_eq!(
            ok.detail,
            "npm 全局 · 0.171.0 · 已是最新 · 升级：npm install -g @openai/codex@latest  /b/codex"
        );
        let new = cli_item(&status(Some("0.171.0"), Some("0.172.0")), true, None);
        assert_eq!(new.mark, Mark::Warn);
        assert!(new.detail.contains("有新版 0.172.0"), "{new:?}");
        let off = cli_item(&status(Some("0.171.0"), None), false, None);
        assert!(off.detail.contains("没查最新版"), "{off:?}");
        let unasked = cli_item(&status(None, Some("0.172.0")), true, None);
        assert_eq!(unasked.mark, Mark::Warn);
        assert!(unasked.detail.contains("问不出版本"), "{unasked:?}");

        // 升级过的：下一行写上次升级，没成的带原文（多行缩进接着）。
        let tried = clis::Outcome {
            vendor: Vendor::Codex,
            from: Some("0.171.0".into()),
            target: "0.172.0".into(),
            command: "npm install -g @openai/codex@latest".into(),
            ok: false,
            message: "npm install -g @openai/codex@latest 退出码 243：\nnpm error code EACCES"
                .into(),
            at: 1_800_000_000,
            auto: true,
        };
        let failed = cli_item(
            &status(Some("0.171.0"), Some("0.172.0")),
            true,
            Some(&tried),
        );
        assert!(
            failed.detail.contains("\n      上次自动升级（"),
            "{failed:?}"
        );
        assert!(
            failed.detail.ends_with(
                "，→ 0.172.0）：没升上：npm install -g @openai/codex@latest 退出码 243：\n      npm error code EACCES"
            ),
            "{failed:?}"
        );
        let done = clis::Outcome {
            ok: true,
            auto: false,
            message: "已升到 0.172.0".into(),
            ..tried
        };
        let ok = cli_item(&status(Some("0.172.0"), Some("0.172.0")), true, Some(&done));
        assert!(ok.detail.contains("上次手动升级（"), "{ok:?}");
        assert!(ok.detail.ends_with("：已升到 0.172.0"), "{ok:?}");
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

    fn no_kaku(_: &Path, _: &[String]) -> io::Result<String> {
        Err(io::Error::other("Kaku 没开"))
    }

    fn yes_kaku(_: &Path, _: &[String]) -> io::Result<String> {
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
        assert_eq!(first(&Terminal::Cmux, &env(""), yes_kaku).mark, Mark::Ok);
        let cmux_down = first(&Terminal::Cmux, &env(""), no_kaku);
        assert!(
            cmux_down.mark == Mark::Fail && cmux_down.detail.contains("w 在当前窗口"),
            "{cmux_down:?}"
        );
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

    #[test]
    fn balance_lists_each_key_without_the_key() {
        use crate::quota::{
            Balance, BalanceRow, BalanceValue, KeyOrigin, Money, Provider, Skipped,
        };
        let now = SystemTime::now();
        let pi = KeyOrigin {
            account: Some(OriginAccount {
                vendor: Vendor::Pi,
                key: String::new(),
                name: "默认".to_owned(),
            }),
            place: "auth.json · deepseek".to_owned(),
        };
        let env = KeyOrigin {
            account: None,
            place: "环境变量 KIMI_API_KEY".to_owned(),
        };
        let report = BalanceReport {
            rows: vec![
                BalanceRow {
                    provider: Provider::DeepSeek,
                    origins: vec![pi.clone()],
                    result: Ok(Balance {
                        value: BalanceValue::Money {
                            amounts: vec![Money {
                                currency: "CNY".to_owned(),
                                amount: -1.5,
                            }],
                            available: false,
                        },
                        at: now,
                    }),
                },
                BalanceRow {
                    provider: Provider::KimiCode,
                    origins: vec![env],
                    result: Err("HTTP 500".to_owned()),
                },
            ],
            skipped: vec![Skipped {
                origin: pi,
                reason: "密钥写成了命令，会话台不执行".to_owned(),
            }],
            defaults: Vec::new(),
        };
        let items = balance_items(&report, now);
        assert_eq!(items.len(), 3);
        assert_eq!(
            (items[0].mark, items[0].detail.as_str()),
            (
                Mark::Ok,
                "-¥1.50 欠费，刚刚（pi 默认 · auth.json · deepseek）"
            )
        );
        assert_eq!(items[1].mark, Mark::Warn);
        assert_eq!(
            items[1].detail,
            "取不到：HTTP 500（环境变量 KIMI_API_KEY，非公开接口）"
        );
        assert_eq!(items[2].mark, Mark::Skip);

        let none = balance_items(&BalanceReport::default(), now);
        assert_eq!(none.len(), 1);
        assert!(none[0].detail.contains("没找到"));
    }
}
