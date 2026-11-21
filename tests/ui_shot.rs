//! 真终端截图（界面门 `.agentq/ui` 调用；手动跑：`SHOT_DIR=/tmp/shots cargo test --test ui_shot -- --ignored --nocapture`）。
//!
//! 在临时 HOME 里手写一套假对话（不读本机真实数据），在 PTY 里跑真的 `ai`，用 alacritty_terminal 解释它的输出，
//! 把画面连同颜色写成 HTML（每格定宽，中文占两格），再由脚本用无头 Chrome 转 PNG。全程不开窗口。
//! 终端背景色查询（OSC 11）由这里按深色/浅色作答，顺带验证跟随终端深浅色。
//! 同时实测按键到重绘：写入按键 → 收到第一个字节 / 这一帧写完（之后 50ms 没有输出）。

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use huitai::ui::app::SPINNER;

const COLS: u16 = 132;
const ROWS: u16 = 34;
const CELL_W: u32 = 9;
const CELL_H: u32 = 20;

// ---- 假数据 ----

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn json(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

/// 一条假对话：项目（相对家目录）、标题、多久以前、几轮问答。
struct Fake {
    project: &'static str,
    title: &'static str,
    mins: i64,
    turns: &'static [(&'static str, &'static str)],
}

const CLAUDE: &[Fake] = &[
    Fake {
        project: "projects/shop",
        title: "购物车结算按钮点了没反应",
        mins: 3,
        turns: &[
            (
                "购物车结算按钮点了没反应，帮我查一下",
                "找到了：结算按钮在优惠券还没算完时被禁用，算完后没有重新启用。我改了 useCheckout 里的状态更新，并补了一个测试。",
            ),
            (
                "好，顺便看看移动端是不是一样",
                "移动端走的是同一个组件，已经一起修好，e2e 测试全部通过。",
            ),
        ],
    },
    Fake {
        project: "projects/shop",
        title: "把优惠券接口换成新版",
        mins: 26,
        turns: &[(
            "优惠券接口要换成 v2",
            "已切到 /v2/coupons，旧字段 discount_rate 映射成 rate。",
        )],
    },
    Fake {
        project: "projects/shop",
        title: "结算页首屏加载慢，查一下原因",
        mins: 60 * 30,
        turns: &[(
            "结算页首屏要 3 秒",
            "主要是地址列表同步请求，改成并行后首屏 1.1 秒。",
        )],
    },
    Fake {
        project: "projects/blog",
        title: "给博客加 RSS 订阅",
        mins: 95,
        turns: &[("给博客加 RSS", "RSS 生成在 /feed.xml，包含最近 20 篇。")],
    },
    Fake {
        project: "projects/blog",
        title: "文章页代码块不换行",
        mins: 60 * 24 * 4,
        turns: &[(
            "代码块太长会撑出页面",
            "给 pre 加了横向滚动，移动端也正常。",
        )],
    },
    Fake {
        project: "projects/api",
        title: "接口加分页和游标",
        mins: 60 * 7,
        turns: &[("订单列表接口加分页", "加了 cursor 参数，返回 next_cursor。")],
    },
    Fake {
        project: "work/api",
        title: "订单服务超时排查",
        mins: 60 * 24 * 2,
        turns: &[(
            "订单服务偶尔超时",
            "数据库连接池只有 5 个，调到 20 后压测不再超时。",
        )],
    },
    Fake {
        project: "projects/shop/.claude/worktrees/1120-cart",
        title: "后台任务：购物车重构",
        mins: 40,
        turns: &[(
            "按 BACKLOG 重构购物车",
            "拆成 cart-store 和 cart-view 两块，测试已过。",
        )],
    },
    Fake {
        project: "",
        title: "brew 装的 python 找不到",
        mins: 60 * 24 * 9,
        turns: &[(
            "brew 装的 python 命令找不到",
            "PATH 里少了 /opt/homebrew/bin，已加进 .zprofile。",
        )],
    },
];

/// 后台任务（`claude -p`，会话行 `entrypoint` 为 `sdk-cli`）起的：列表默认藏进「另有 N 条后台」。
/// shop 里一条刚刚还在写（算在跑），ops 只有后台对话（整组不显示）。
const CLAUDE_AUTO: &[Fake] = &[
    Fake {
        project: "projects/shop",
        title: "按待办做：结算页骨架屏",
        // 刚写过：整套截图要跑一分多钟，写成 1 分钟前的话轮到「别处在跑」那张时已经过了 2 分钟。
        mins: 0,
        turns: &[(
            "按 BACKLOG 做：结算页加骨架屏",
            "骨架屏加好了，正在跑测试。",
        )],
    },
    Fake {
        project: "projects/shop",
        title: "门检查：跑测试和 clippy",
        mins: 60 * 6,
        turns: &[("跑 verify", "全部通过。")],
    },
    Fake {
        project: "projects/blog",
        title: "夜班复盘",
        mins: 60 * 20,
        turns: &[("复盘昨晚的提交", "写进 LESSONS.md 两条。")],
    },
    Fake {
        project: "projects/ops",
        title: "每天备份检查",
        mins: 60 * 3,
        turns: &[("检查备份", "备份都在。")],
    },
];

/// `codex exec` 起的（`session_meta` 的 `source` 为 `exec`）。
const CODEX_AUTO: &[Fake] = &[Fake {
    project: "projects/api",
    title: "门检查：补测试",
    mins: 30,
    turns: &[("跑测试", "全部通过。")],
}];

const CODEX: &[Fake] = &[
    Fake {
        project: "projects/api",
        title: "给订单列表接口补单元测试",
        mins: 8,
        turns: &[(
            "补订单列表接口的单元测试",
            "加了 6 个测试，覆盖空列表和分页边界，全部通过。",
        )],
    },
    Fake {
        project: "projects/api",
        title: "把 SQL 查询改成参数化",
        mins: 60 * 5,
        turns: &[("把拼接的 SQL 改成参数化", "12 处查询改完，没有行为变化。")],
    },
    Fake {
        project: "projects/ledger-cli",
        title: "导出 CSV 时金额精度丢了",
        mins: 60 * 26,
        turns: &[(
            "导出 CSV 金额少了一位小数",
            "导出时用了 f32，改成按分存整数再格式化。",
        )],
    },
];

const PI: &[Fake] = &[
    Fake {
        project: "projects/blog",
        title: "RSS 初稿",
        mins: 12,
        turns: &[("给博客加一个 RSS", "RSS 已生成在 /feed.xml。")],
    },
    Fake {
        project: "projects/blog",
        title: "首页加阅读时长",
        mins: 60 * 3,
        turns: &[(
            "首页每篇加阅读时长",
            "按每分钟 400 字估算，显示在标题下面。",
        )],
    },
];

fn uuid(n: usize) -> String {
    format!("{n:08x}-0000-4000-8000-{n:012x}")
}

fn encode(p: &Path) -> String {
    p.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn write_lines(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lines.join("\n") + "\n").unwrap();
}

/// 会话文件：写完把 mtime 拨回最后一条消息的时间，像真的停了那么久（不然刚写的文件都算「别处在用」）。
fn write_session(path: &Path, lines: &[String], f: &Fake) {
    write_lines(path, lines);
    let end = UNIX_EPOCH + Duration::from_secs((now_secs() - f.mins * 60) as u64);
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(end)
        .unwrap();
}

/// 每轮的时间：最后一条在 `mins` 分钟前，往前每条隔 2 分钟。
fn times(f: &Fake) -> Vec<String> {
    let end = now_secs() - f.mins * 60;
    let n = f.turns.len() as i64 * 2;
    (0..n).map(|i| rfc3339(end - (n - 1 - i) * 120)).collect()
}

fn fake_home(home: &Path) {
    let claude = CLAUDE.iter().map(|f| (f, "cli"));
    let claude = claude.chain(CLAUDE_AUTO.iter().map(|f| (f, "sdk-cli")));
    for (i, (f, entrypoint)) in claude.enumerate() {
        let cwd = home.join(f.project);
        fs::create_dir_all(&cwd).unwrap();
        let cwd_s = cwd.to_string_lossy();
        let id = uuid(0x100 + i);
        let ts = times(f);
        let mut lines = vec![format!(
            r#"{{"type":"custom-title","customTitle":{},"sessionId":"{id}"}}"#,
            json(f.title)
        )];
        for (j, (q, a)) in f.turns.iter().enumerate() {
            lines.push(format!(
                r#"{{"type":"user","message":{{"role":"user","content":{}}},"uuid":"u{j}","timestamp":"{}","cwd":{},"sessionId":"{id}","gitBranch":"main","entrypoint":"{entrypoint}"}}"#,
                json(q),
                ts[j * 2],
                json(&cwd_s)
            ));
            lines.push(format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":{}}}]}},"uuid":"a{j}","timestamp":"{}","cwd":{},"sessionId":"{id}","gitBranch":"main","entrypoint":"{entrypoint}"}}"#,
                json(a),
                ts[j * 2 + 1],
                json(&cwd_s)
            ));
        }
        let dir = home.join(".claude/projects").join(encode(&cwd));
        write_session(&dir.join(format!("{id}.jsonl")), &lines, f);
    }

    let mut index = Vec::new();
    let codex = CODEX.iter().map(|f| (f, "cli"));
    let codex = codex.chain(CODEX_AUTO.iter().map(|f| (f, "exec")));
    for (i, (f, source)) in codex.enumerate() {
        let cwd = home.join(f.project);
        fs::create_dir_all(&cwd).unwrap();
        let id = uuid(0x200 + i);
        let ts = times(f);
        let mut lines = vec![format!(
            r#"{{"timestamp":"{}","type":"session_meta","payload":{{"id":"{id}","cwd":{},"source":"{source}"}}}}"#,
            ts[0],
            json(&cwd.to_string_lossy())
        )];
        for (j, (q, a)) in f.turns.iter().enumerate() {
            for (k, (kind, text)) in [("UserMessage", q), ("AgentMessage", a)].iter().enumerate() {
                lines.push(format!(
                    r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"item_completed","item":{{"type":"{kind}","id":"i{j}{k}","content":[{{"type":"text","text":{}}}]}}}}}}"#,
                    ts[j * 2 + k],
                    json(text)
                ));
            }
        }
        if i == 0 {
            lines.push(format!(
                r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{{"primary":{{"used_percent":9.0,"window_minutes":10080,"resets_at":{}}},"secondary":{{"used_percent":31.0,"window_minutes":300,"resets_at":{}}},"plan_type":"plus"}}}}}}"#,
                ts[ts.len() - 1],
                now_secs() + 86_400 * 3,
                now_secs() + 3600
            ));
        }
        let name = format!("rollout-{}-{id}.jsonl", ts[0].replace(':', "-"));
        write_session(
            &home.join(".codex/sessions/2026/11/20").join(name),
            &lines,
            f,
        );
        index.push(format!(
            r#"{{"id":"{id}","thread_name":{}}}"#,
            json(f.title)
        ));
    }
    write_lines(&home.join(".codex/session_index.jsonl"), &index);

    for (i, f) in PI.iter().enumerate() {
        let cwd = home.join(f.project);
        let id = uuid(0x300 + i);
        let ts = times(f);
        let mut lines = vec![format!(
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"{}","cwd":{}}}"#,
            ts[0],
            json(&cwd.to_string_lossy())
        )];
        for (j, (q, a)) in f.turns.iter().enumerate() {
            for (k, (role, text)) in [("user", q), ("assistant", a)].iter().enumerate() {
                lines.push(format!(
                    r#"{{"type":"message","id":"e{j}{k}","timestamp":"{}","message":{{"role":"{role}","content":[{{"type":"text","text":{}}}],"usage":{{"input":40000,"output":2000,"totalTokens":42000}}}}}}"#,
                    ts[j * 2 + k],
                    json(text)
                ));
            }
        }
        lines.push(format!(
            r#"{{"type":"session_info","id":"n{i}","timestamp":"{}","name":{}}}"#,
            ts[ts.len() - 1],
            json(f.title)
        ));
        let dir = home
            .join(".pi/agent/sessions")
            .join(format!("--{}--", encode(&cwd).trim_start_matches('-')));
        write_session(
            &dir.join(format!("{}_{id}.jsonl", ts[0].replace(':', "-"))),
            &lines,
            f,
        );
    }

    // Claude 状态栏缓存（额度第一渠道），这样不会去碰钥匙串和联网。
    let data = home.join("Library/Application Support/huitai");
    fs::create_dir_all(&data).unwrap();
    fs::write(
        data.join("claude-statusline.json"),
        format!(
            r#"{{"rate_limits":{{"five_hour":{{"used_percentage":23.0,"resets_at":{}}},"seven_day":{{"used_percentage":41.0,"resets_at":{}}}}}}}"#,
            now_secs() + 3600,
            now_secs() + 86_400 * 4
        ),
    )
    .unwrap();
}

// ---- PTY 里的 ai ----

#[derive(Clone)]
struct Listener {
    master: Arc<Mutex<File>>,
    background: Rgb,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let reply = match event {
            Event::PtyWrite(s) => s,
            Event::ColorRequest(_, format) => format(self.background),
            _ => return,
        };
        let _ = self.master.lock().unwrap().write_all(reply.as_bytes());
    }
}

struct Size(usize, usize);

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.1
    }
    fn screen_lines(&self) -> usize {
        self.1
    }
    fn columns(&self) -> usize {
        self.0
    }
}

struct Run {
    master: Arc<Mutex<File>>,
    child: Child,
    term: Arc<Mutex<(Term<Listener>, Processor)>>,
    received: Arc<AtomicUsize>,
    last_output: Arc<Mutex<Instant>>,
    background: Rgb,
    foreground: Rgb,
}

/// 测试中途失败（等不到某段文字就 panic）时也要收掉 ai 进程，不然它占着一个 PTY 一直挂着。
impl Drop for Run {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Run {
    fn start(home: &Path, light: bool) -> Run {
        let (mut m, mut s) = (-1, -1);
        let mut ws = libc::winsize {
            ws_row: ROWS,
            ws_col: COLS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: 指针都指向有效的局部变量。
        let rc = unsafe {
            libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut ws,
            )
        };
        assert_eq!(rc, 0, "openpty 失败");
        // SAFETY: openpty 成功，两个描述符归我们所有。
        let (master, slave) = unsafe { (File::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ai"));
        cmd.env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("LANG", "zh_CN.UTF-8")
            .env("SHELL", "/bin/zsh")
            // 测试不联网：不查新版本。
            .env("HUITAI_NO_UPDATE_CHECK", "1")
            .current_dir(home.join("projects/shop"))
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: pre_exec 里只调 async-signal-safe 的 setsid 和 ioctl。
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().expect("启动 ai");
        let (background, foreground) = if light {
            (
                Rgb {
                    r: 0xff,
                    g: 0xff,
                    b: 0xff,
                },
                Rgb {
                    r: 0x2a,
                    g: 0x2a,
                    b: 0x2a,
                },
            )
        } else {
            (
                Rgb {
                    r: 0x1c,
                    g: 0x1c,
                    b: 0x1f,
                },
                Rgb {
                    r: 0xd0,
                    g: 0xd0,
                    b: 0xd0,
                },
            )
        };
        let mut reader = master.try_clone().unwrap();
        let master = Arc::new(Mutex::new(master));
        let listener = Listener {
            master: master.clone(),
            background,
        };
        let term = Term::new(
            Config::default(),
            &Size(usize::from(COLS), usize::from(ROWS)),
            listener,
        );
        let term = Arc::new(Mutex::new((term, Processor::new())));
        let received = Arc::new(AtomicUsize::new(0));
        let last_output = Arc::new(Mutex::new(Instant::now()));
        {
            let (term, received, last_output) =
                (term.clone(), received.clone(), last_output.clone());
            thread::spawn(move || {
                let mut buf = [0u8; 65536];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        return;
                    }
                    let mut g = term.lock().unwrap();
                    let (t, p) = &mut *g;
                    p.advance(t, &buf[..n]);
                    drop(g);
                    *last_output.lock().unwrap() = Instant::now();
                    received.fetch_add(n, Ordering::SeqCst);
                }
            });
        }
        Run {
            master,
            child,
            term,
            received,
            last_output,
            background,
            foreground,
        }
    }

    fn send(&self, bytes: &[u8]) {
        let mut m = self.master.lock().unwrap();
        m.write_all(bytes).unwrap();
        m.flush().unwrap();
    }

    /// 按一个键并等它处理完（Esc 后面紧跟别的键会被当成 ⌥ 组合键）。
    fn press(&self, bytes: &[u8]) {
        self.send(bytes);
        thread::sleep(Duration::from_millis(100));
        self.settle(Duration::from_millis(200));
    }

    fn text(&self) -> String {
        let g = self.term.lock().unwrap();
        let t = &g.0;
        let mut out = String::new();
        for l in 0..t.screen_lines() {
            for c in 0..t.columns() {
                let cell = &t.grid()[Line(l as i32)][Column(c)];
                if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    out.push(cell.c);
                }
            }
            out.push('\n');
        }
        out
    }

    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.text().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "15 秒没等到「{needle}」，当前画面：\n{}",
                self.text()
            );
            thread::sleep(Duration::from_millis(20));
        }
        self.settle(Duration::from_millis(300));
    }

    /// 等到画面 `quiet` 这么久没变。在跑的转圈一直在换帧，比较时把转圈的字符当成同一个，
    /// 不然有对话在跑时永远等不到安静。
    fn settle(&self, quiet: Duration) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let calm = || -> String {
            self.text()
                .chars()
                .map(|c| if SPINNER.contains(&c) { '◐' } else { c })
                .collect()
        };
        let mut last = calm();
        let mut since = Instant::now();
        while since.elapsed() < quiet && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
            let now = calm();
            if now != last {
                last = now;
                since = Instant::now();
            }
        }
    }

    /// 按一个键，返回（到第一个字节、到这一帧写完）的耗时。
    fn timed_key(&self, bytes: &[u8]) -> (Duration, Duration) {
        self.settle(Duration::from_millis(150));
        let before = self.received.load(Ordering::SeqCst);
        let t0 = Instant::now();
        self.send(bytes);
        let deadline = t0 + Duration::from_secs(2);
        while self.received.load(Ordering::SeqCst) == before {
            assert!(Instant::now() < deadline, "按键 2 秒没有重绘");
            std::hint::spin_loop();
        }
        let first = t0.elapsed();
        self.settle(Duration::from_millis(50));
        let last = self.last_output.lock().unwrap().duration_since(t0);
        (first, last)
    }

    fn html(&self) -> String {
        let g = self.term.lock().unwrap();
        let t = &g.0;
        let hex = |c: Rgb| format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b);
        let resolve = |c: Color, default: Rgb| match c {
            Color::Spec(rgb) => rgb,
            Color::Named(NamedColor::Background) => self.background,
            Color::Named(NamedColor::Foreground) => self.foreground,
            Color::Indexed(i) => xterm(i),
            Color::Named(n) => match n as u16 {
                i @ 0..=15 => xterm(i as u8),
                i if (NamedColor::DimBlack as u16..=NamedColor::DimWhite as u16).contains(&i) => {
                    xterm((i - NamedColor::DimBlack as u16) as u8)
                }
                _ => default,
            },
        };
        let mut body = String::new();
        for l in 0..t.screen_lines() {
            body.push_str("<div class=r>");
            for c in 0..t.columns() {
                let cell = &t.grid()[Line(l as i32)][Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let (mut fg, mut bg) = (
                    resolve(cell.fg, self.foreground),
                    resolve(cell.bg, self.background),
                );
                if cell.flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let w = if cell.flags.contains(Flags::WIDE_CHAR) {
                    CELL_W * 2
                } else {
                    CELL_W
                };
                let weight = if cell.flags.contains(Flags::BOLD) {
                    ";font-weight:600"
                } else {
                    ""
                };
                let ch = match cell.c {
                    '<' => "&lt;".to_owned(),
                    '>' => "&gt;".to_owned(),
                    '&' => "&amp;".to_owned(),
                    c => c.to_string(),
                };
                let _ = write!(
                    body,
                    "<span style=\"width:{w}px;color:{};background:{}{weight}\">{ch}</span>",
                    hex(fg),
                    hex(bg)
                );
            }
            body.push_str("</div>\n");
        }
        format!(
            "<!doctype html><meta charset=utf-8><style>\
             html,body{{margin:0;background:{bg}}}\
             .r{{height:{CELL_H}px;line-height:{CELL_H}px;white-space:pre;font:15px 'SF Mono',Menlo,'PingFang SC',monospace}}\
             .r span{{display:inline-block;height:{CELL_H}px;text-align:center;overflow:hidden;vertical-align:top}}\
             </style><body>{body}</body>",
            bg = hex(self.background)
        )
    }

    fn save(&self, dir: &Path, name: &str) {
        fs::write(dir.join(format!("{name}.html")), self.html()).unwrap();
        fs::write(
            dir.join(format!("{name}.size")),
            format!("{},{}", u32::from(COLS) * CELL_W, u32::from(ROWS) * CELL_H),
        )
        .unwrap();
        fs::write(dir.join(format!("{name}.txt")), self.text()).unwrap();
    }

    fn quit(mut self) {
        // 紧跟在 Esc 后面的 q 会被当成 ⌥q，先等前一个键处理完。
        self.press(b"");
        self.send(b"q");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("按 q 后 5 秒没退出");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// xterm 默认 256 色调色板（实时预览里 CLI 用的 16 色和 256 色）。
fn xterm(i: u8) -> Rgb {
    const BASE: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xcd, 0x00, 0x00),
        (0x00, 0xcd, 0x00),
        (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee),
        (0xcd, 0x00, 0xcd),
        (0x00, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    let (r, g, b) = match i {
        0..=15 => BASE[usize::from(i)],
        16..=231 => {
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            let n = i - 16;
            (level(n / 36), level(n / 6 % 6), level(n % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    };
    Rgb { r, g, b }
}

/// 假 CLI 画的一屏（像 Claude Code 跑测试时的样子）。
const LIVE_SCREEN: &str = "\x1b[38;2;217;119;87m✻\x1b[0m \x1b[1mClaude Code\x1b[0m  ~/projects/shop\r\n\
    \r\n\
    \x1b[2m>\x1b[0m 购物车结算按钮点了没反应，帮我查一下\r\n\
    \r\n\
    ⏺ 正在跑 e2e 测试 (npm run test:e2e -- --project=chromium --reporter=line --workers=4)\r\n\
    \x20 ⎿  \x1b[32m✓\x1b[0m checkout.spec.ts (12 tests) 4.2s\r\n\
    \x20    \x1b[32m✓\x1b[0m coupon.spec.ts (8 tests) 2.9s\r\n\
    \x20    \x1b[7m RUN \x1b[0m mobile.spec.ts\r\n\
    \r\n\
    \x1b[38;5;208m✶ Running…\x1b[0m \x1b[2m(esc to interrupt)\x1b[0m\r\n";

/// 假家目录里的守护进程，托管第一条 Claude 对话。drop 时结束。
struct LiveHost(Child);

impl LiveHost {
    fn start(home: &Path) -> Self {
        use huitai::core::Vendor;
        use huitai::host::{self, Request};
        let child = Command::new(env!("CARGO_BIN_EXE_ai"))
            .arg("daemon")
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            // 不抢全局呼出键、不弹系统通知。
            .env("HUITAI_NO_HOTKEY", "1")
            .env("HUITAI_NOTIFY_TO", "/dev/null")
            .spawn()
            .expect("起守护进程");
        let live = LiveHost(child);
        let dir = home.join("Library/Application Support/huitai");
        let deadline = Instant::now() + Duration::from_secs(5);
        while host::request(&dir, &Request::Ping).is_err() {
            assert!(Instant::now() < deadline, "守护进程没起来");
            thread::sleep(Duration::from_millis(20));
        }
        let screen = home.join("live-screen.bin");
        fs::write(&screen, LIVE_SCREEN).unwrap();
        let command = format!("stty raw -echo; cat '{}'; sleep 600", screen.display());
        live.host(
            home,
            uuid(0x100),
            Vendor::Claude,
            CLAUDE[0].project,
            &command,
        );
        live
    }

    /// 再托管一条假对话（`id` 要和假家目录里那条对话的 id 一样，列表才认得出它）。
    fn host(
        &self,
        home: &Path,
        id: String,
        vendor: huitai::core::Vendor,
        project: &str,
        command: &str,
    ) {
        use huitai::core::SessionId;
        use huitai::host::{self, Request, Response, SpawnSpec};
        let dir = home.join("Library/Application Support/huitai");
        let id = SessionId::from(id);
        let cwd = home.join(project);
        let spec = SpawnSpec {
            id: id.clone(),
            vendor,
            project: cwd.file_name().unwrap().to_string_lossy().into_owned(),
            cwd,
            command: command.to_owned(),
        };
        assert_eq!(
            host::request(&dir, &Request::Spawn(spec)).unwrap(),
            Response::Ok
        );
        let env = [
            ("SHELL", "/bin/sh"),
            ("PATH", "/usr/bin:/bin"),
            ("TERM", "xterm-256color"),
            ("LANG", "zh_CN.UTF-8"),
            ("HOME", home.to_str().unwrap()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        // attach 一次把假 CLI 启动起来，再关掉这个「标签页」（对话照跑）。
        drop(host::attach(&dir, &id, env, 100, 16).expect("attach"));
    }
}

impl Drop for LiveHost {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 写一份上一个守护进程留下的 running.json：Codex、pi 各一条在跑。
fn interrupted_left_behind(home: &Path) {
    use huitai::core::{SessionId, Vendor};
    use huitai::host::RunRecord;
    use huitai::host::interrupt;
    let dir = home.join("Library/Application Support/huitai");
    let rec = |id: String, vendor, fake: &Fake| RunRecord {
        id: SessionId::from(id),
        vendor,
        project: Path::new(fake.project)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        cwd: home.join(fake.project),
        started_at: now_secs() as u64 - 600,
    };
    let records = [
        rec(uuid(0x201), Vendor::Codex, &CODEX[1]),
        rec(uuid(0x301), Vendor::Pi, &PI[1]),
    ]
    .into_iter()
    .map(|r| (r.id.clone(), r))
    .collect();
    interrupt::save(&interrupt::running_path(&dir), &records).unwrap();
}

/// 结束会话台自己拉起的守护进程。
fn stop_daemon(home: &Path) {
    use huitai::host::{self, Request, Response};
    let dir = home.join("Library/Application Support/huitai");
    if let Ok(Response::Pong { pid }) = host::request(&dir, &Request::Ping) {
        // SAFETY: 只给这个假家目录里的守护进程发信号。
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

/// 测试结束（含失败）时删掉假家目录。
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn pct(v: &mut [Duration], p: usize) -> Duration {
    v.sort();
    v[(v.len() - 1) * p / 100]
}

#[test]
#[ignore = "截图和实测：.agentq/ui 调用，或手动跑"]
fn real_terminal_screenshots() {
    let out = std::env::var_os("SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/ui-shots"));
    fs::create_dir_all(&out).unwrap();
    // 假家目录不能放在临时目录或工作树里：那些路径会被归进「零散对话」「agentq 工作树」折叠组。
    let real_home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let home = real_home
        .join("Library/Caches")
        .join(format!("huitai-ui-shot-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    let _cleanup = Cleanup(home.clone());
    fake_home(&home);
    // projects_root（默认 ~/projects）下还没有对话的目录，N 的候选里也有。
    fs::create_dir_all(home.join("projects/landing")).unwrap();
    let home = home.canonicalize().unwrap();

    let run = Run::start(&home, false);
    run.wait_for("购物车结算按钮点了没反应");
    run.wait_for("5 小时");
    run.wait_for("找到了");
    run.save(&out, "1-claude");

    let (mut first, mut done) = (Vec::new(), Vec::new());
    for k in ["j", "j", "j", "k", "k", "k"].iter().cycle().take(60) {
        let (f, d) = run.timed_key(k.as_bytes());
        first.push(f);
        done.push(d);
    }
    println!(
        "真终端按键→重绘（{} 次，{COLS}×{ROWS}）：开始输出 中位 {:?} p99 {:?}；整帧写完 中位 {:?} p99 {:?}",
        first.len(),
        pct(&mut first, 50),
        pct(&mut first, 99),
        pct(&mut done, 50),
        pct(&mut done, 99)
    );

    // Codex、pi 里没有启动目录（shop）的项目：项目组默认收起，先截收起的样子，再 l 展开第一组。
    run.send(b"\t");
    run.wait_for("另有 1 条后台");
    run.save(&out, "2a-codex-collapsed");
    run.send(b"l");
    run.wait_for("给订单列表接口补单元测试");
    run.wait_for("7 天");
    run.save(&out, "2-codex");
    run.send(b"\tl");
    run.wait_for("首页加阅读时长");
    run.wait_for("token");
    run.save(&out, "3-pi");
    run.send(b"\t?");
    run.wait_for("任意键关闭");
    run.save(&out, "4-help");
    run.press(b"\x1b");
    run.send("/结算".as_bytes());
    run.wait_for("条 · ⏎ 打开");
    run.save(&out, "5-search");
    run.press(b"\x1b");
    // b：显示后台对话（记进设置），截完再按一次藏回去，后面几张照常。
    run.send(b"b");
    run.wait_for("按待办做：结算页骨架屏");
    run.wait_for("▸ ops");
    run.save(&out, "9-background");
    // 上一条是刚刚还在写的后台对话：别处在跑（柔绿转圈），回车先问一句（截完 Esc 取消，不真的打开）。
    run.send(b"k");
    run.wait_for("别处在跑");
    run.send(b"\r");
    run.wait_for("⏎ 仍然打开 · Esc 取消");
    run.save(&out, "10-elsewhere");
    run.press(b"\x1b");
    run.send(b"b");
    run.wait_for("已隐藏后台对话");
    // N：目录候选（三家的项目目录 + ~/projects 下还没对话的 landing），打 a 筛、↓ 选第一个。
    run.send(b"N");
    run.wait_for("Tab 补全");
    run.wait_for("~/projects/landing");
    run.send(b"a");
    run.wait_for("3 个");
    run.press(b"\x1b[B");
    run.save(&out, "11-newdir");
    run.press(b"\x1b");
    // 底栏随选中项换提示：项目组标题、停着的对话（在跑的那种见 7-live）。
    run.send(b"1k");
    run.wait_for("⏎ 收起   n 新对话   e 改名");
    run.save(&out, "13-bar-group");
    run.send(b"jj");
    run.wait_for("⏎ 接着聊   e 改名   d 删除");
    run.save(&out, "14-bar-stopped");
    // 右键菜单：在「结算页首屏加载慢」那一行上右键（SGR 鼠标序列，坐标从 1 起）。
    let y = run
        .text()
        .lines()
        .position(|l| l.contains("结算页首屏加载慢"))
        .expect("列表里有这条")
        + 1;
    run.send(format!("\x1b[<2;12;{y}M\x1b[<2;12;{y}m").as_bytes());
    run.wait_for("在当前窗口打开");
    run.save(&out, "15-menu");
    run.press(b"\x1b");
    run.quit();

    // 桌面端开着「订单服务超时排查」：起一个命令行里带着 `--resume <id>` 的进程，会话台从进程表认出来，
    // 顶栏多一个「●1 别处」。后面几张（浅色、实时预览）都带着它，截完杀掉。
    // `exec -a` 把 sleep 的进程名换成 `claude --resume <id>`：只有一个进程，ps 里看到的命令行带着 id；
    // 包进 KillOnDrop，测试中途失败也会被收掉。
    let desktop = KillOnDrop(
        std::process::Command::new("/bin/bash")
            .args([
                "-c",
                &format!("exec -a 'claude --resume {}' sleep 600", uuid(0x106)),
            ])
            .spawn()
            .expect("起假的桌面端进程"),
    );
    // 浅色终端：会话台问到白底，自动换浅色。
    let run = Run::start(&home, true);
    run.wait_for("1 别处");
    run.wait_for("购物车结算按钮点了没反应");
    run.wait_for("找到了");
    run.save(&out, "6-light");
    run.quit();

    // 实时预览：守护进程托管第一条对话（假 CLI 画一屏后空闲），选中它时预览区显示它此刻的屏幕。
    // 顺带看印章和全局状态：Codex、pi 各一条在跑，Claude 第二条等会话台连上后响铃（变成等你回复）。
    use huitai::core::Vendor;
    let daemon = LiveHost::start(&home);
    let idle = "stty raw -echo; printf 'ready\\r\\n'; sleep 600";
    daemon.host(&home, uuid(0x200), Vendor::Codex, CODEX[0].project, idle);
    daemon.host(&home, uuid(0x300), Vendor::Pi, PI[0].project, idle);
    let go = home.join("bell-go");
    let bell = format!(
        "stty raw -echo; while [ ! -e '{}' ]; do sleep 0.1; done; printf '\\007'; sleep 600",
        go.display()
    );
    daemon.host(&home, uuid(0x101), Vendor::Claude, CLAUDE[1].project, &bell);
    let run = Run::start(&home, false);
    run.wait_for("此刻的画面");
    run.wait_for("Running…");
    run.wait_for("4 在跑");
    fs::write(&go, "").unwrap();
    run.wait_for("◆1 等你");
    // 等「刚停下」那一下整行高亮淡完再截，不然看着像选中了两行。
    thread::sleep(huitai::ui::app::FLASH + Duration::from_millis(300));
    run.save(&out, "7-live");
    // 三家各一张：印章换成这家的颜色，状态还是三家合计。
    run.send(b"\t");
    run.wait_for("ledger-cli");
    run.save(&out, "10-seal-codex");
    run.send(b"\t");
    run.wait_for("token");
    run.save(&out, "11-seal-pi");
    // !：跳到等你回复的那条，跨家换回 Claude。
    run.send(b"!");
    run.wait_for("· ◆ 等你回复");
    run.save(&out, "12-jump-waiting");
    run.quit();
    drop(daemon);
    drop(desktop);

    // 中断恢复：上一个守护进程被杀（上面那个就是 SIGKILL 收掉的）时 running.json 里还记着两条在跑的对话；
    // 会话台打开时拉起新守护进程，它认出这两条是被中断的，会话台停在顶部「被中断」组上。
    interrupted_left_behind(&home);
    let run = Run::start(&home, false);
    run.wait_for("R 全部接着做");
    run.wait_for("这些对话还在跑");
    run.save(&out, "8-interrupted");
    run.quit();
    stop_daemon(&home);

    println!("截图 HTML 写在 {}", out.display());
}

/// 测试起的子进程：离开作用域（包括测试中途 panic）时杀掉并收尸。
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
