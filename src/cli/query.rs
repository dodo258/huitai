//! `ai list [--vendor 名] [--json]`、`ai search <词> [--json]`：不进界面，只读索引缓存直接出结果。
//!
//! 只读：缓存库只读打开（还没有就报错，不建），不扫描、不建全文索引、不搬旧标注、不改认领记录，
//! 守护进程没在跑也不拉起（状态全是 `stopped`）。结果就是会话台上次扫到的样子。
//!
//! JSON 字段是稳定接口（写在 README「命令行查询」）：只加不改。

use std::io;
use std::path::{Path, PathBuf};
use std::thread;

use serde::Serialize;

use crate::core::{Account, Config, Vendor};
use crate::export;
use crate::index::{Entry, Hit, Index};
use crate::sources::{self, DEFAULT_NAME, Trash};
use crate::ui;
use crate::ui::text::{tilde, truncate, truncate_middle, width};

/// 每家最多出多少条搜索结果（和界面一样）。
const SEARCH_LIMIT: usize = 300;
/// 纯文本里项目、标题最多占几格（JSON 里是全的）。
const PROJECT_COLS: usize = 40;
const TITLE_COLS: usize = 50;

/// 输出的一条对话。字段名见模块说明。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    /// 会话台用的 id：`ai export`、`ai attach` 认这个。非默认账号的带 `<账号>:` 前缀。
    pub id: String,
    /// 厂商（`claude`、`codex`……，同 [`Vendor::key`]）。
    pub vendor: &'static str,
    /// 账号显示名（默认位置的叫「默认」）。
    pub account: String,
    /// 项目目录（绝对路径）。
    pub project: PathBuf,
    pub title: String,
    /// `hosted`：会话台托管着、CLI 进程在跑；`stopped`：没有。
    pub state: &'static str,
    /// 最近时间，本地时区的 RFC 3339。
    pub last_active: String,
    pub archived: bool,
    /// 自动运行（`claude -p`、`codex exec` 这类）的对话。
    pub automatic: bool,
    /// 只有 `ai search` 有：命中的那段正文；只命中标题或取不到时是 `null`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<Option<String>>,
    #[serde(skip)]
    last_secs: i64,
}

/// 打开本机的索引（只读），连同账号和数据目录。
struct Ctx {
    index: Index,
    accounts: Vec<Account>,
    dir: PathBuf,
}

fn open() -> io::Result<Ctx> {
    let not_found = |what: &str| io::Error::new(io::ErrorKind::NotFound, format!("取不到{what}"));
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| not_found(" HOME"))?;
    let dir = Config::data_dir().ok_or_else(|| not_found("数据目录"))?;
    let trash = Trash::default_location().ok_or_else(|| not_found("回收站位置"))?;
    let accounts = sources::local_accounts(&home);
    let index =
        Index::open_read_only(&dir, sources::sources_for(&accounts, trash)).map_err(|e| match e
            .kind()
        {
            io::ErrorKind::NotFound => {
                io::Error::new(e.kind(), format!("{e}：先运行一次 ai，等列表出来后再查"))
            }
            io::ErrorKind::InvalidData => io::Error::new(
                e.kind(),
                format!("{e}：先运行一次 ai，让它把索引换成这一版"),
            ),
            _ => e,
        })?;
    for p in index.problems() {
        eprintln!("注意：{p}");
    }
    Ok(Ctx {
        index,
        accounts,
        dir,
    })
}

/// `ai list`：全部对话（`vendor` 给了就只要那一家），最近的在前。
pub fn list(vendor: Option<Vendor>) -> io::Result<Vec<Row>> {
    let ctx = open()?;
    let hosted = ui::hosted_now(&ctx.dir);
    let mut rows = Vec::new();
    for v in Vendor::ALL
        .into_iter()
        .filter(|v| vendor.is_none_or(|w| w == *v))
    {
        for e in ctx.index.list(v)? {
            rows.push(row(&e, &ctx.accounts, hosted.contains(&e.session.id), None));
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.last_secs));
    Ok(rows)
}

/// `ai search`：和界面里按 `/` 搜一样（标题命中的在前，之后正文按相关度），一家一家排，顺序同顶栏；
/// 正文也命中的带上命中片段。
pub fn search(query: &str) -> io::Result<Vec<Row>> {
    let ctx = open()?;
    let hosted = ui::hosted_now(&ctx.dir);
    let mut hits: Vec<Hit> = Vec::new();
    for v in Vendor::ALL {
        hits.extend(ctx.index.search(v, query, SEARCH_LIMIT)?);
    }
    let snippets = snippets(&ctx.index, &hits, query);
    Ok(hits
        .iter()
        .zip(snippets)
        .map(|(h, snip)| {
            let e = &h.entry;
            row(e, &ctx.accounts, hosted.contains(&e.session.id), Some(snip))
        })
        .collect())
}

/// 每条结果的命中片段（只命中标题的、读不出的是 `None`）。要读会话文件，分几个线程一起读。
fn snippets(index: &Index, hits: &[Hit], query: &str) -> Vec<Option<String>> {
    let threads = thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8);
    let per = hits.len().div_ceil(threads).max(1);
    thread::scope(|s| {
        let jobs: Vec<_> = hits
            .chunks(per)
            .map(|chunk| {
                let job = s.spawn(move || {
                    chunk
                        .iter()
                        .map(|h| {
                            h.in_body
                                .then(|| index.snippet(&h.entry.session, query).ok().flatten())
                                .flatten()
                                .map(|s| s.text)
                        })
                        .collect::<Vec<_>>()
                });
                (chunk.len(), job)
            })
            .collect();
        // 读片段的线程崩了，那几条就不带片段。
        jobs.into_iter()
            .flat_map(|(n, j)| j.join().unwrap_or_else(|_| vec![None; n]))
            .collect()
    })
}

fn row(e: &Entry, accounts: &[Account], hosted: bool, snippet: Option<Option<String>>) -> Row {
    let s = &e.session;
    let account = if s.account.is_empty() {
        DEFAULT_NAME.to_owned()
    } else {
        accounts
            .iter()
            .find(|a| a.vendor == s.vendor && a.key == s.account)
            .map_or_else(|| s.account.clone(), |a| a.name.clone())
    };
    let secs = export::unix(s.last_active);
    Row {
        id: s.id.to_string(),
        vendor: s.vendor.key(),
        account,
        project: s.cwd.clone(),
        title: e.title().to_owned(),
        state: if hosted { "hosted" } else { "stopped" },
        last_active: export::local_rfc3339(secs, export::local_offset(secs)),
        archived: e.note.archived,
        automatic: s.automatic,
        snippet,
        last_secs: secs,
    }
}

/// JSON 数组（每条一个对象，字段见 [`Row`]）。
pub fn json(rows: &[Row]) -> io::Result<String> {
    let mut out = serde_json::to_string_pretty(rows).map_err(io::Error::other)?;
    out.push('\n');
    Ok(out)
}

/// 对齐的纯文本，一条一行：时间、厂商、账号、状态、项目、标题、id（id 在最后，`awk '{print $NF}'` 取得到）。
/// 项目、标题太长按 [`PROJECT_COLS`]、[`TITLE_COLS`] 截断；控制字符换成空格。有片段的下一行缩进四格写片段。
pub fn text(rows: &[Row], home: &Path) -> String {
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|r| {
            let secs = r.last_secs;
            [
                export::local_minute(secs, export::local_offset(secs)),
                vendor_name(r.vendor).to_owned(),
                truncate(&r.account, 20),
                if r.state == "hosted" {
                    "托管中"
                } else {
                    "-"
                }
                .to_owned(),
                truncate_middle(&tilde(&r.project, home), PROJECT_COLS),
                truncate(&r.title, TITLE_COLS),
            ]
        })
        .collect();
    let mut widths = [0; 6];
    for c in &cells {
        for (w, cell) in widths.iter_mut().zip(c) {
            *w = (*w).max(width(cell));
        }
    }
    let mut out = String::new();
    for (r, c) in rows.iter().zip(&cells) {
        for (cell, w) in c.iter().zip(widths) {
            out.push_str(cell);
            out.push_str(&" ".repeat(w - width(cell) + 2));
        }
        out.push_str(&truncate(&r.id, usize::MAX));
        out.push('\n');
        if let Some(Some(snip)) = &r.snippet {
            out.push_str("    ");
            out.push_str(&truncate(snip, usize::MAX));
            out.push('\n');
        }
    }
    out
}

fn vendor_name(key: &str) -> &'static str {
    Vendor::ALL
        .into_iter()
        .find(|v| v.key() == key)
        .map_or("", Vendor::name)
}

/// `--vendor` 的值：厂商的键（`claude`）或显示名（`Kimi Code`），不分大小写、不管空格。
pub fn parse_vendor(s: &str) -> Result<Vendor, String> {
    let norm = |x: &str| x.replace([' ', '-'], "").to_lowercase();
    let want = norm(s);
    Vendor::ALL
        .into_iter()
        .find(|v| norm(v.key()) == want || norm(v.name()) == want)
        .ok_or_else(|| {
            let keys: Vec<&str> = Vendor::ALL.iter().map(|v| v.key()).collect();
            format!("不认识的厂商「{s}」，可以用：{}", keys.join("、"))
        })
}
