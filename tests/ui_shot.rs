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
/// 首图单独的尺寸：宽到顶栏八家写得下全名，高到列表和预览刚好装满、下面不留空。
const HERO_COLS: u16 = 144;
const HERO_ROWS: u16 = 18;
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

const HERMES: &[Fake] = &[
    Fake {
        project: "projects/api",
        title: "整理接口文档的目录结构",
        mins: 25,
        turns: &[
            (
                "接口文档按模块分目录，旧链接要能跳过去",
                "已按订单、用户、支付三块分好目录，旧路径都加了重定向。",
            ),
            (
                "顺便把示例请求都跑一遍",
                "跑了 14 个示例，2 个字段名过期，已改。",
            ),
        ],
    },
    Fake {
        project: "projects/blog",
        title: "把草稿里的图片压一遍",
        mins: 60 * 30,
        turns: &[(
            "草稿里的图片太大了",
            "压好了，平均小了六成，肉眼看不出差别。",
        )],
    },
];

const KIMI: &[Fake] = &[
    Fake {
        project: "projects/blog",
        title: "首页图片改成懒加载",
        mins: 18,
        turns: &[
            ("首页图片改成懒加载", "首屏以下的图片都加了 loading=lazy。"),
            (
                "再量一下首屏时间",
                "首屏从 2.4 秒降到 1.6 秒，主要省在图片请求上。",
            ),
        ],
    },
    Fake {
        project: "projects/shop",
        title: "商品详情页加骨架屏",
        mins: 60 * 8,
        turns: &[(
            "商品详情页加载时先显示骨架屏",
            "加好了，图片和价格区各一块，数据到了淡出。",
        )],
    },
];

const GEMINI: &[Fake] = &[
    Fake {
        project: "projects/blog",
        title: "文章页加目录，能折叠",
        mins: 40,
        turns: &[
            ("文章页加一个目录", "按 h2、h3 生成了目录，放在正文右侧。"),
            ("目录要能折叠", "加了折叠按钮，状态记在 localStorage。"),
        ],
    },
    Fake {
        project: "projects/api",
        title: "看 README 的部署步骤对不对",
        mins: 60 * 50,
        turns: &[("看 README 的部署步骤对不对", "第三步少了 npm run build。")],
    },
];

const OPENCODE: &[Fake] = &[
    Fake {
        project: "projects/shop",
        title: "给登录页加验证码",
        mins: 40,
        turns: &[
            (
                "给登录页加验证码，别太难认",
                "加好了，用的滑块，改了 src/login.tsx。",
            ),
            ("移动端也试一下", "移动端拖动正常，补了 3 个测试。"),
        ],
    },
    Fake {
        project: "projects/blog",
        title: "评论换成自建的",
        mins: 60 * 50,
        turns: &[("评论换成自建的", "换好了，旧评论导进来了 128 条。")],
    },
];

const ANTIGRAVITY: &[Fake] = &[
    Fake {
        project: "projects/shop",
        title: "下单接口加幂等",
        mins: 70,
        turns: &[
            (
                "下单接口加幂等，重复点提交会下两单",
                "按请求头里的幂等键去重，重复的直接回上次的结果。",
            ),
            ("键存多久", "存 24 小时，过期自动清掉，补了 3 个测试。"),
        ],
    },
    Fake {
        project: "projects/blog",
        title: "RSS 输出全文",
        mins: 60 * 50,
        turns: &[("RSS 输出全文", "feed.xml 改成输出全文了。")],
    },
];

/// OpenCode 的会话 id：`ses_` + 12 位十六进制 + 14 位字母数字（形状对就行）。
fn opencode_id(n: usize) -> String {
    format!("ses_e11a9a1f{n:04x}AbCdEfGhIjKlMn")
}

/// Hermes 的会话 id：`YYYYMMDD_HHMMSS_<十六进制>`（时间部分只要形状对）。
fn hermes_id(n: usize) -> String {
    format!("20261120_1000{n:02}_{n:06x}")
}

/// SQL 里的字符串字面量。
fn sql(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

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

/// 一条 Gemini CLI 对话：`<数据目录>/tmp/<短名>/.project_root` + `chats/session-*.jsonl`（格式见 tests/fixtures/gemini）。
fn write_gemini(home: &Path, data: &Path, slug: &str, id: &str, f: &Fake) {
    let cwd = home.join(f.project);
    fs::create_dir_all(&cwd).unwrap();
    let ts = times(f);
    let slug = data.join("tmp").join(slug);
    fs::create_dir_all(&slug).unwrap();
    fs::write(slug.join(".project_root"), cwd.to_string_lossy().as_bytes()).unwrap();
    let mut lines = vec![format!(
        r#"{{"sessionId":"{id}","projectHash":"h","startTime":"{}","lastUpdated":"{}","kind":"main"}}"#,
        ts[0],
        ts[ts.len() - 1]
    )];
    for (j, (q, a)) in f.turns.iter().enumerate() {
        lines.push(format!(
            r#"{{"id":"u{j}","timestamp":"{}","type":"user","content":[{{"text":{}}}]}}"#,
            ts[j * 2],
            json(q)
        ));
        lines.push(format!(
            r#"{{"id":"g{j}","timestamp":"{}","type":"gemini","content":{}}}"#,
            ts[j * 2 + 1],
            json(a)
        ));
    }
    lines.push(format!(r#"{{"$set":{{"summary":{}}}}}"#, json(f.title)));
    write_session(
        &slug.join(format!("chats/session-2026-11-20T10-00-{}.jsonl", &id[..8])),
        &lines,
        f,
    );
}

/// 每轮的时间：最后一条在 `mins` 分钟前，往前每条隔 2 分钟。
fn times(f: &Fake) -> Vec<String> {
    let end = now_secs() - f.mins * 60;
    let n = f.turns.len() as i64 * 2;
    (0..n).map(|i| rfc3339(end - (n - 1 - i) * 120)).collect()
}

/// 一段工具调用：AI 说一句、跑命令、看返回、读文件、看返回（Claude 会话行）。
fn tool_lines(id: &str, ts: &str, cwd: &str) -> Vec<String> {
    let line = |kind: &str, content: serde_json::Value| {
        serde_json::json!({
            "type": kind,
            "message": {"role": kind, "content": content},
            "timestamp": ts,
            "cwd": cwd,
            "sessionId": id,
            "entrypoint": "cli",
        })
        .to_string()
    };
    vec![
        line(
            "assistant",
            serde_json::json!([
                {"type": "text", "text": "先看看首屏请求了哪些接口、各花了多久。"},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "npm run trace -- /checkout"}},
            ]),
        ),
        line(
            "user",
            serde_json::json!([{"type": "tool_result", "tool_use_id": "t1",
                "content": "GET /api/address   1.8s  同步\nGET /api/cart      0.4s\nGET /api/coupons   0.6s"}]),
        ),
        line(
            "assistant",
            serde_json::json!([
                {"type": "tool_use", "id": "t2", "name": "Read", "input": {"file_path": "src/pages/checkout/load.ts"}},
            ]),
        ),
        line(
            "user",
            serde_json::json!([{"type": "tool_result", "tool_use_id": "t2",
                "content": "const address = await fetchAddress()\nconst cart = await fetchCart()\nconst coupons = await fetchCoupons()"}]),
        ),
    ]
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
            // 「结算页首屏加载慢」中间有两次工具调用：整段对话里折成一行、t 展开（截图 18、19）。
            if f.title.starts_with("结算页首屏") {
                lines.extend(tool_lines(&id, &ts[j * 2], &cwd_s));
            }
            lines.push(format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":{}}}]}},"uuid":"a{j}","timestamp":"{}","cwd":{},"sessionId":"{id}","gitBranch":"main","entrypoint":"{entrypoint}"}}"#,
                json(a),
                ts[j * 2 + 1],
                json(&cwd_s)
            ));
        }
        // 第二条（实时预览那轮会响铃变成等你回复）停下时 Claude 写了一句总结：列表里它下面一行写这句。
        if i == 1 {
            lines.push(format!(
                r#"{{"type":"system","subtype":"turn_duration","durationMs":372000,"timestamp":"{}","sessionId":"{id}"}}"#,
                ts[ts.len() - 1]
            ));
            lines.push(format!(
                r#"{{"type":"system","subtype":"away_summary","content":"优惠券接口换成 v2 了，等你确认要不要删旧接口。","timestamp":"{}","sessionId":"{id}"}}"#,
                ts[ts.len() - 1]
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
        // 第一条（实时预览那轮在跑）有计划、正跑着测试；第二条（那轮会响铃）上一轮被打断（失败，红色）。
        if i == 0 {
            let plan = r#"{\"plan\":[{\"step\":\"读订单列表接口\",\"status\":\"completed\"},{\"step\":\"补空列表的测试\",\"status\":\"completed\"},{\"step\":\"补分页边界的测试\",\"status\":\"in_progress\"},{\"step\":\"跑全部测试\",\"status\":\"pending\"},{\"step\":\"更新接口文档\",\"status\":\"pending\"}]}"#;
            lines.extend([
                format!(
                    r#"{{"type":"response_item","payload":{{"type":"function_call","name":"update_plan","arguments":"{plan}","call_id":"p1"}}}}"#
                ),
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"p1","output":"Plan updated"}}"#.to_owned(),
                r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"cargo\",\"test\",\"-p\",\"orders\"]}","call_id":"c1"}}"#.to_owned(),
            ]);
        }
        if i == 1 {
            lines.push(
                r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted","duration_ms":95000}}"#
                    .to_owned(),
            );
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

    // Hermes：一个 state.db（表结构见 tests/fixtures/hermes/state.sql），用系统 sqlite3 建。
    let mut script = String::from(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, created_source TEXT, \
         started_at REAL NOT NULL, cwd TEXT, git_branch TEXT, git_repo_root TEXT, title TEXT, \
         last_activity_at REAL, end_reason TEXT, parent_session_id TEXT, \
         archived INTEGER NOT NULL DEFAULT 0, hidden INTEGER NOT NULL DEFAULT 0);\n\
         CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, \
         role TEXT NOT NULL, content TEXT, tool_calls TEXT, timestamp REAL NOT NULL, \
         active INTEGER NOT NULL DEFAULT 1);\n",
    );
    for (i, f) in HERMES.iter().enumerate() {
        let cwd = home.join(f.project);
        fs::create_dir_all(&cwd).unwrap();
        let id = hermes_id(i);
        let end = now_secs() - f.mins * 60;
        let n = f.turns.len() as i64 * 2;
        script.push_str(&format!(
            "INSERT INTO sessions (id, source, created_source, started_at, cwd, title, last_activity_at) \
             VALUES ({}, 'local', 'tui', {}, {}, {}, {end});\n",
            sql(&id),
            end - n * 120,
            sql(&cwd.to_string_lossy()),
            sql(f.title)
        ));
        for (j, (q, a)) in f.turns.iter().enumerate() {
            for (k, (role, text)) in [("user", q), ("assistant", a)].iter().enumerate() {
                let at = end - (n - 1 - (j as i64 * 2 + k as i64)) * 120;
                script.push_str(&format!(
                    "INSERT INTO messages (session_id, role, content, timestamp) VALUES ({}, '{role}', {}, {at});\n",
                    sql(&id),
                    sql(text)
                ));
            }
        }
    }
    let db = home.join(".hermes/state.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let out = std::process::Command::new("/usr/bin/sqlite3")
        .arg(&db)
        .arg(&script)
        .output()
        .expect("建 Hermes 库要 sqlite3（macOS 自带）");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Kimi Code：每条一个目录，state.json 记标题和目录，正文在 agents/main/wire.jsonl（见 tests/fixtures/kimi）。
    for (i, f) in KIMI.iter().enumerate() {
        let cwd = home.join(f.project);
        fs::create_dir_all(&cwd).unwrap();
        let id = format!("session_{}", uuid(0x500 + i));
        let end_ms = (now_secs() - f.mins * 60) * 1000;
        let n = f.turns.len() as i64 * 2;
        let at = |k: i64| end_ms - (n - 1 - k) * 120_000;
        let mut lines = vec![format!(
            r#"{{"type":"metadata","protocol_version":"1.5","created_at":{}}}"#,
            at(0)
        )];
        for (j, (q, a)) in f.turns.iter().enumerate() {
            let k = j as i64 * 2;
            lines.push(format!(
                r#"{{"type":"context.append_message","agentId":"main","message":{{"role":"user","content":[{{"type":"text","text":{}}}],"origin":{{"kind":"user"}}}},"time":{}}}"#,
                json(q),
                at(k)
            ));
            lines.push(format!(
                r#"{{"type":"context.append_loop_event","agentId":"main","event":{{"type":"content.part","turnId":"{j}","step":1,"part":{{"type":"text","text":{}}}}},"time":{}}}"#,
                json(a),
                at(k + 1)
            ));
        }
        let dir = home
            .join(".kimi-code/sessions")
            .join(format!(
                "wd_{}_{i:012x}",
                cwd.file_name().unwrap().to_string_lossy()
            ))
            .join(&id);
        write_lines(
            &dir.join("state.json"),
            &[format!(
                r#"{{"id":"{id}","version":2,"title":{},"titleKind":"generated","cwd":{},"createdAt":{},"updatedAt":{end_ms},"archived":false,"isCustomTitle":false}}"#,
                json(f.title),
                json(&cwd.to_string_lossy()),
                at(0)
            )],
        );
        write_session(&dir.join("agents/main/wire.jsonl"), &lines, f);
    }

    // Gemini CLI：见 write_gemini。
    for (i, f) in GEMINI.iter().enumerate() {
        write_gemini(
            home,
            &home.join(".gemini"),
            &format!("g{i}"),
            &uuid(0x600 + i),
            f,
        );
    }

    // OpenCode：一个 opencode.db，2.x 的表（表结构见 tests/fixtures/opencode/opencode.sql）。时间是毫秒。
    let mut script = String::from(
        "CREATE TABLE session_v2 (id text PRIMARY KEY, project_id text NOT NULL, parent_id text, \
         slug text NOT NULL, directory text NOT NULL, title text, version text NOT NULL, \
         time_created integer NOT NULL, time_updated integer NOT NULL, time_archived integer);\n\
         CREATE TABLE session_message (id text PRIMARY KEY, session_id text NOT NULL, type text NOT NULL, \
         seq integer NOT NULL, time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL);\n",
    );
    for (i, f) in OPENCODE.iter().enumerate() {
        let cwd = home.join(f.project);
        fs::create_dir_all(&cwd).unwrap();
        let id = opencode_id(i);
        let end = (now_secs() - f.mins * 60) * 1000;
        let n = f.turns.len() as i64 * 2;
        script.push_str(&format!(
            "INSERT INTO session_v2 VALUES ({}, 'prj', NULL, 's', {}, {}, '2.3.2', {}, {end}, NULL);\n",
            sql(&id),
            sql(&cwd.to_string_lossy()),
            sql(f.title),
            end - n * 120_000,
        ));
        for (j, (q, a)) in f.turns.iter().enumerate() {
            for (k, (kind, data)) in [
                ("user", format!(r#"{{"text":{}}}"#, json(q))),
                (
                    "assistant",
                    format!(r#"{{"content":[{{"type":"text","text":{}}}]}}"#, json(a)),
                ),
            ]
            .iter()
            .enumerate()
            {
                let seq = j as i64 * 2 + k as i64;
                let at = end - (n - 1 - seq) * 120_000;
                script.push_str(&format!(
                    "INSERT INTO session_message VALUES ('msg_{i}_{seq}', {}, '{kind}', {seq}, {at}, {at}, {});\n",
                    sql(&id),
                    sql(data)
                ));
            }
        }
    }
    let db = home.join(".local/share/opencode/opencode.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let out = std::process::Command::new("/usr/bin/sqlite3")
        .arg(&db)
        .arg(&script)
        .output()
        .expect("建 OpenCode 库要 sqlite3（macOS 自带）");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Antigravity：`~/.gemini/antigravity-cli/conversation_summaries.db`，摘要表加全文表，正文按步的字节偏移切
    // （格式见 tests/fixtures/antigravity）。用户第 4j 步、AI 第 4j+2 步。
    let mut script = String::from(
        "CREATE TABLE conversation_summaries (conversation_id text PRIMARY KEY, title text, \
         last_modified_time datetime, workspace_uris text, last_user_input_step_index integer);\n\
         CREATE VIRTUAL TABLE conversations_fts USING fts5(conversation_id UNINDEXED, \
         chunk_index UNINDEXED, archived UNINDEXED, user_step_byte_offsets UNINDEXED, \
         agent_step_byte_offsets UNINDEXED, title, user_body, agent_body);\n",
    );
    for (i, f) in ANTIGRAVITY.iter().enumerate() {
        let cwd = home.join(f.project);
        fs::create_dir_all(&cwd).unwrap();
        let id = uuid(0x900 + i);
        let uri = format!(r#"["file://{}"]"#, cwd.to_string_lossy());
        let (mut ub, mut ab, mut uo, mut ao) = (String::new(), String::new(), vec![], vec![]);
        for (j, (q, a)) in f.turns.iter().enumerate() {
            uo.push(format!("[{},{}]", j * 4, ub.len()));
            ao.push(format!("[{},{}]", j * 4 + 2, ab.len()));
            ub.push_str(&format!("{q}\n"));
            ab.push_str(&format!("{a}\n"));
        }
        script.push_str(&format!(
            "INSERT INTO conversation_summaries VALUES ({}, {}, datetime({}, 'unixepoch') || '+00:00', {}, {});\n\
             INSERT INTO conversations_fts VALUES ({0}, -1, 0, '', '', {1}, '', '');\n\
             INSERT INTO conversations_fts VALUES ({0}, 0, 0, '[{}]', '[{}]', '', {}, {});\n",
            sql(&id),
            sql(f.title),
            now_secs() - f.mins * 60,
            sql(&uri),
            f.turns.len() * 4 - 4,
            uo.join(","),
            ao.join(","),
            sql(&ub),
            sql(&ab),
        ));
    }
    let db = home.join(".gemini/antigravity-cli/conversation_summaries.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let out = std::process::Command::new("/usr/bin/sqlite3")
        .arg(&db)
        .arg(&script)
        .output()
        .expect("建 Antigravity 库要 sqlite3（macOS 自带）");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

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

    // 注入的状态栏命令按对话另存的那份：头两条 Claude 对话报了上下文占用（72%、86%，后一条到了提醒色）。
    let context = data.join("claude-context");
    fs::create_dir_all(&context).unwrap();
    for (n, used) in [(0x100, 72), (0x101, 86)] {
        let id = uuid(n);
        fs::write(
            context.join(format!("{id}.json")),
            format!(r#"{{"session_id":"{id}","context_window":{{"used_percentage":{used}}}}}"#),
        )
        .unwrap();
    }

    // 余额：pi 的 auth.json 里一把编的 DeepSeek 密钥、models.json 里一把 Kimi Code 的和一条写成命令的（跳过）。
    // 余额缓存预先写好并处在退避期：会话台直接用缓存，绝不拿这两把编的密钥去请求真接口。
    fs::write(
        home.join(".pi/agent/auth.json"),
        r#"{"deepseek": {"type": "api_key", "key": "sk-fake-ui-shot"}}"#,
    )
    .unwrap();
    fs::write(
        home.join(".pi/agent/models.json"),
        r#"{"providers": {"kimi-coding": {"apiKey": "sk-kimi-fake-ui-shot"}, "moonshotai-cn": {"apiKey": "!pass show moonshot"}}}"#,
    )
    .unwrap();
    let at = format!(
        r#"{{"secs_since_epoch": {}, "nanos_since_epoch": 0}}"#,
        now_secs() - 180
    );
    let hold = now_secs() + 86_400;
    // 缓存键是「来源#密钥指纹」：指纹 = SHA-256("huitai-balance-key:" + 密钥) 的前 16 位十六进制，
    // 上面两把假密钥的指纹用 `printf 'huitai-balance-key:<密钥>' | shasum -a 256 | cut -c1-16` 算出。
    fs::write(
        data.join("balance.json"),
        format!(
            r#"{{"entries": {{
                "pi:|auth.json · deepseek#d3758ae8eb51b60e": {{"provider": "deepseek", "at": {a}, "backoff_until": {hold},
                    "result": {{"Ok": {{"at": {at}, "value": {{"money": {{"amounts": [{{"currency": "CNY", "amount": 36.42}}], "available": true}}}}}}}}}},
                "pi:|models.json · kimi-coding#b5f2cd602db6a815": {{"provider": "kimi_code", "at": {a}, "backoff_until": {hold},
                    "result": {{"Ok": {{"at": {at}, "value": {{"windows": [
                        {{"label": "5 小时", "used_percent": 83.0, "window_minutes": 300, "resets_at": null}},
                        {{"label": "周", "used_percent": 31.0, "window_minutes": null, "resets_at": null}}]}}}}}}}}
            }}}}"#,
            a = now_secs() - 180
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
    cols: u16,
    rows: u16,
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
        Run::start_sized(home, light, COLS, ROWS)
    }

    fn start_sized(home: &Path, light: bool, cols: u16, rows: u16) -> Run {
        let (mut m, mut s) = (-1, -1);
        let mut ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
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
            // 测试不联网：不查新版本；万一还有别的 https 请求（余额缓存没生效之类），经一个关着的本机端口直接失败。
            .env("HUITAI_NO_UPDATE_CHECK", "1")
            .env("https_proxy", "http://127.0.0.1:9")
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
            &Size(usize::from(cols), usize::from(rows)),
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
            cols,
            rows,
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
            format!(
                "{},{}",
                u32::from(self.cols) * CELL_W,
                u32::from(self.rows) * CELL_H
            ),
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
        let live = Self::bare(home);
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

    /// 只起守护进程，不托管对话。
    fn bare(home: &Path) -> Self {
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

    /// 像在标签页里看了一眼：连上再断开（刚做完的变成等你回复）。
    fn peek(&self, home: &Path, id: String) {
        let dir = home.join("Library/Application Support/huitai");
        let env = [("HOME", home.to_str().unwrap()), ("TERM", "xterm-256color")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let id = huitai::core::SessionId::from(id);
        drop(huitai::host::attach(&dir, &id, env, 100, 16).expect("attach"));
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
    // Hermes：黄铜色印章和标签，没有额度渠道，顶栏右边不写额度。
    run.send(b"\tl");
    run.wait_for("整理接口文档的目录结构");
    run.wait_for("2 个字段名过期");
    run.save(&out, "3b-hermes");
    // Kimi Code：玫红印章和标签。启动目录 shop 里有它的对话，切过去那组自动展开选中。
    run.send(b"\t");
    run.wait_for("商品详情页加骨架屏");
    run.wait_for("数据到了淡出");
    run.save(&out, "3c-kimi");
    // Gemini：天蓝色印章和标签，和 Codex 的蓝、「做完」的青要分得开；没有额度渠道。
    run.send(b"\tl");
    run.wait_for("按 h2、h3 生成了目录");
    run.save(&out, "3d-gemini");

    // OpenCode：青绿色印章和标签，没有额度渠道（启动目录 shop 里有它的对话，组默认展开）。
    run.send(b"\t");
    run.wait_for("给登录页加验证码");
    run.wait_for("补了 3 个测试");
    run.save(&out, "3d-opencode");

    // Antigravity：黄绿色印章和标签，和 OpenCode 的青绿、Hermes 的黄铜分得开；没有额度渠道。
    run.send(b"\t");
    run.wait_for("下单接口加幂等");
    run.wait_for("补了 3 个测试");
    run.save(&out, "3f-antigravity");
    run.send(b"\t?");
    run.wait_for("任意键关闭");
    run.save(&out, "4-help");
    run.press(b"\x1b");
    run.send("/结算".as_bytes());
    run.wait_for("条 · ⏎ 打开");
    // 正文也命中的结果下面那行命中片段是后台读回来的：等画面停下再截。
    run.settle(Duration::from_millis(500));
    run.save(&out, "5-search");
    // ⌃A 各家一起搜：换个几家都说过的词，每行前面标那家的标记；回到一家再按一次。
    run.press(b"\x15");
    run.send("测试".as_bytes());
    run.wait_for("条 · ⏎ 打开");
    run.press(b"\x01");
    run.wait_for("搜索结果 · 各家");
    run.wait_for("⌃A 只搜");
    run.settle(Duration::from_millis(500));
    run.save(&out, "5b-search-all");
    run.press(b"\x01");
    run.wait_for("⌃A 搜各家");
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
    run.send(b"m");
    run.wait_for("Tab 补全");
    run.wait_for("~/projects/landing");
    run.send(b"a");
    run.wait_for("3 个");
    run.press(b"\x1b[B");
    run.save(&out, "11-newdir");
    run.press(b"\x1b");
    // 底栏随选中项换提示：项目组标题、停着的对话（在跑的那种见 7-live）。
    run.send(b"1k");
    run.wait_for("⏎ 收起   n 新对话   m 新项目   e 改名");
    run.save(&out, "13-bar-group");
    run.send(b"jj");
    run.wait_for("⏎ 接着聊   v 看全文   e 改名   d 删除");
    run.save(&out, "14-bar-stopped");
    // 整段对话：下一条「结算页首屏加载慢」中间有工具调用，先截折叠的，t 展开再截一张，Esc 回列表。
    run.send(b"j");
    run.wait_for("结算页首屏要 3 秒");
    run.send(b"v");
    run.wait_for("对话全文");
    run.wait_for("工具 ×2");
    run.save(&out, "18-reader");
    run.send(b"t");
    run.wait_for("npm run trace");
    run.save(&out, "19-reader-tools");
    // s 存成 Markdown：落到临时 HOME 的 Downloads，状态栏写存到哪了。
    run.send(b"s");
    run.wait_for("已存到 ~/Downloads/");
    run.save(&out, "19-reader-exported");
    let saved: Vec<PathBuf> = fs::read_dir(home.join("Downloads"))
        .expect("导出建了 Downloads")
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(saved.len(), 1, "{saved:?}");
    let md = fs::read_to_string(&saved[0]).unwrap();
    assert!(md.contains("结算页首屏要 3 秒"), "{md}");
    assert!(md.contains("- 工具 "), "{md}");
    run.press(b"\x1b");
    run.wait_for("⏎ 接着聊   v 看全文");
    // 右键菜单：在「结算页首屏加载慢」那一行上右键（SGR 鼠标序列，坐标从 1 起）。
    let y = run
        .text()
        .lines()
        .position(|l| l.contains("结算页首屏加载慢"))
        .expect("列表里有这条")
        + 1;
    run.send(format!("\x1b[<2;12;{y}M\x1b[<2;12;{y}m").as_bytes());
    run.wait_for("在当前窗口打开");
    run.wait_for("撞额度后到点自动继续");
    run.save(&out, "15-menu");
    run.press(b"\x1b");
    // $：各账号额度和余额（缓存里预先写好的，见 fake_home）。
    run.send(b"$");
    run.wait_for("额度和余额");
    run.wait_for("¥36.42");
    run.wait_for("跳过 pi 默认");
    run.save(&out, "17-balance");
    run.press(b"\x1b");
    // s：这时没有托管的对话，汇总是空的，底栏只给 Esc 返回。
    run.send(b"s");
    run.wait_for("没有待处理或在跑的对话");
    run.wait_for("待处理 0 · 在跑 0");
    run.save(&out, "16a-board-empty");
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
    // 只在正文里出现的词：结果行下面那行命中片段（预览里也有这个词，片段那行再出现一次）。
    run.send("/usecheckout".as_bytes());
    run.wait_for("1 条 · ⏎ 打开");
    run.settle(Duration::from_millis(500));
    let screen = run.text();
    assert!(
        screen.matches("useCheckout").count() >= 2,
        "结果行下面要有命中片段：\n{screen}"
    );
    run.save(&out, "6a-light-search-body");
    run.press(b"\x1b");
    // 浅色下的 Hermes 黄铜色：压暗一档，和「等你」的黄分开（项目组在第一轮已经展开，展开状态是记住的）。
    run.send(b"\t\t\t");
    run.wait_for("整理接口文档的目录结构");
    run.wait_for("2 个字段名过期");
    run.save(&out, "6b-light-hermes");
    run.send(b"\t");
    run.wait_for("商品详情页加骨架屏");
    run.wait_for("数据到了淡出");
    run.save(&out, "6c-light-kimi");
    run.send(b"\t");
    run.wait_for("文章页加目录，能折叠");
    run.save(&out, "6d-light-gemini");

    // 浅色下的 OpenCode 青绿：压暗一档，白底上看得清。
    run.send(b"\t");
    run.wait_for("给登录页加验证码");
    run.wait_for("补了 3 个测试");
    run.save(&out, "6d-light-opencode");
    run.send(b"\t");
    run.wait_for("下单接口加幂等");
    run.wait_for("补了 3 个测试");
    run.save(&out, "6f-light-antigravity");
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
    // Codex 第二条：上一轮被打断，和 Claude 那条一起响铃停下（失败，红色）。
    daemon.host(&home, uuid(0x201), Vendor::Codex, CODEX[1].project, &bell);
    let run = Run::start(&home, false);
    run.wait_for("此刻的画面");
    run.wait_for("Running…");
    run.wait_for("5 在跑");
    fs::write(&go, "").unwrap();
    run.wait_for("◆2 待处理");
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
    run.send(b"\t");
    run.wait_for("整理接口文档的目录结构");
    run.save(&out, "11b-seal-hermes");
    run.send(b"\t");
    run.wait_for("商品详情页加骨架屏");
    run.save(&out, "11c-seal-kimi");
    run.send(b"\t");
    run.wait_for("文章页加目录，能折叠");
    run.save(&out, "11d-seal-gemini");
    run.send(b"\t");
    run.wait_for("给登录页加验证码");
    run.save(&out, "11d-seal-opencode");
    run.send(b"\t");
    run.wait_for("下单接口加幂等");
    run.save(&out, "11f-seal-antigravity");
    // !：跳到等你回复的那条，跨家换回 Claude。
    run.send(b"!");
    run.wait_for("等你回复 · ");
    run.save(&out, "12-jump-waiting");
    // s：三家等你回复和在跑的列在一起，按 要输入 > 失败 > 做完待看 > 在跑 排；在跑和刚停的两行一条。
    run.send(b"s");
    run.wait_for("待处理和在跑的 · 各家");
    run.wait_for("待处理 2 · 在跑 3");
    run.wait_for("失败 · 被打断");
    run.wait_for("优惠券接口换成 v2 了");
    run.wait_for("shell · cargo test -p orders");
    run.save(&out, "16-board");
    // 选中在跑的那条 Codex：预览标题写跑了多久，消息上面是计划块。
    let row = run
        .text()
        .lines()
        .position(|l| l.contains("给订单列表接口补单元测试"))
        .expect("汇总里有在跑的 Codex")
        + 1;
    run.send(format!("\x1b[<0;12;{row}M\x1b[<0;12;{row}m").as_bytes());
    run.wait_for("计划 2/5");
    run.wait_for("在跑 · ");
    run.save(&out, "17-plan");
    run.send(b"\x1b");
    run.quit();
    // 同一条在 18 行的矮窗口里：计划收成一行「计划 2/5 · ▸ 当前步骤」，下面多留几行给消息。
    let run = Run::start_sized(&home, false, COLS, 18);
    run.wait_for("此刻的画面");
    run.send(b"s");
    run.wait_for("待处理和在跑的 · 各家");
    let row = run
        .text()
        .lines()
        .position(|l| l.contains("给订单列表接口补单元测试"))
        .expect("汇总里有在跑的 Codex")
        + 1;
    run.send(format!("\x1b[<0;12;{row}M\x1b[<0;12;{row}m").as_bytes());
    run.wait_for("计划 2/5 · ▸");
    run.wait_for("在跑 · ");
    run.save(&out, "17b-plan-short");
    run.send(b"\x1b");
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

    // 多账号：设置里加了第二个 Claude 账号（~/.claude-work），它的对话混在同一个列表里按项目排，
    // 每条标题后面暗色小字写账号名；设置里列出 Claude 的两个账号。单独一轮，前面的截图不受影响。
    work_account(&home);
    let run = Run::start(&home, false);
    run.wait_for("对账脚本跑出来少了一笔");
    run.wait_for("工作号");
    run.save(&out, "22-accounts");
    run.send(b",");
    run.wait_for("默认 · 工作号");
    run.save(&out, "22b-accounts-settings");
    run.send(b"\x1b");
    run.quit();
    stop_daemon(&home);

    // 首图（README、官网的 board-dark/board-light）：单独一轮，见 hero。
    hero(&home, &out);

    println!("截图 HTML 写在 {}", out.display());
}

/// 第二个 Claude 账号：`~/.claude-work` 里 shop 项目一条对话，设置文件里起名「工作号」。
fn work_account(home: &Path) {
    let f = Fake {
        project: "projects/shop",
        title: "对账脚本跑出来少了一笔",
        mins: 12,
        turns: &[(
            "对账脚本跑出来少了一笔，帮我看看",
            "少的那笔是跨月退款：脚本按下单日期归月，退款按到账日期。改成都按到账日期后对上了。",
        )],
    };
    let cwd = home.join(f.project);
    let cwd_s = cwd.to_string_lossy();
    let id = uuid(0x900);
    let ts = times(&f);
    let (q, a) = f.turns[0];
    let lines = vec![
        format!(
            r#"{{"type":"custom-title","customTitle":{},"sessionId":"{id}"}}"#,
            json(f.title)
        ),
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":{}}},"uuid":"u0","timestamp":"{}","cwd":{},"sessionId":"{id}","entrypoint":"cli"}}"#,
            json(q),
            ts[0],
            json(&cwd_s)
        ),
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":{}}}]}},"uuid":"a0","timestamp":"{}","cwd":{},"sessionId":"{id}","entrypoint":"cli"}}"#,
            json(a),
            ts[1],
            json(&cwd_s)
        ),
    ];
    let dir = home.join(".claude-work/projects").join(encode(&cwd));
    write_session(&dir.join(format!("{id}.jsonl")), &lines, &f);
    let config = home.join("Library/Application Support/huitai/config.json");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        config,
        r#"{"accounts":[{"vendor":"claude","name":"工作号","dir":"~/.claude-work"}]}"#,
    )
    .unwrap();
}

/// 首图里 Gemini CLI 第二个账号（`GEMINI_CLI_HOME=~/gemini-work`，设置里叫「工作号」）的两条对话：一条在跑、一条刚做完。
const GEMINI_WORK: &[Fake] = &[
    Fake {
        project: "projects/shop",
        title: "支付回调验签失败，查一下",
        mins: 4,
        turns: &[(
            "支付回调验签一直失败，帮我查一下",
            "签名是按原始请求体算的，框架先把 body 解析成 JSON 再序列化，字段顺序变了。",
        )],
    },
    Fake {
        project: "projects/shop",
        title: "订单导出加按月筛选",
        mins: 9,
        turns: &[(
            "订单导出加一个按月筛选",
            "加好了，月份下拉默认本月，导出文件名带上月份。",
        )],
    },
];

/// 首图里默认账号多出的几条（shop 里两个账号的对话混在一起）。
const GEMINI_HERO: &[Fake] = &[
    Fake {
        project: "projects/shop",
        title: "商品图换成 WebP",
        mins: 60 * 5,
        turns: &[(
            "商品图都换成 WebP，旧浏览器要能回退",
            "换好了，用 picture 标签带 JPEG 回退，图片总量小了四成。",
        )],
    },
    Fake {
        project: "projects/blog",
        title: "文章图片加说明文字",
        mins: 60 * 28,
        turns: &[(
            "文章里的图片加上说明文字",
            "用 figure 和 figcaption 包起来了，说明取自图片的 alt。",
        )],
    },
    Fake {
        project: "projects/api",
        title: "接口加请求日志",
        mins: 60 * 24 * 3,
        turns: &[(
            "每个请求记一行日志",
            "记了方法、路径、状态码和耗时，慢于 1 秒的标黄。",
        )],
    },
];

/// 首图里在跑那条的画面（像 Gemini CLI 跑测试时的样子）。
const GEMINI_SCREEN: &str = "\x1b[38;2;95;160;230m✦\x1b[0m \x1b[1mGemini CLI\x1b[0m  ~/projects/shop\r\n\
    \r\n\
    \x1b[2m>\x1b[0m 支付回调验签一直失败，帮我查一下\r\n\
    \r\n\
    ✦ 签名按原始 body 算，框架改了字段顺序。\r\n\
    \x20 \x1b[32m✓\x1b[0m ReadFile src/pay/callback.ts\r\n\
    \x20 \x1b[32m✓\x1b[0m Edit src/pay/callback.ts\r\n\
    \x20 \x1b[38;2;95;160;230m⠼\x1b[0m Shell npm test -- pay\r\n\
    \r\n\
    \x1b[38;2;95;160;230m⠼ 正在跑测试…\x1b[0m \x1b[2m(esc to cancel, 12s)\x1b[0m\r\n";

/// 首图：Gemini CLI 这一页，尺寸是 HERO_COLS×HERO_ROWS（不跟别的截图共用）。它没有额度渠道，顶栏写得下八家全名；
/// 两个账号的对话混在一个列表里（每条标题后写账号名），状态有在跑、等你回复、刚做完，别家还有两条在跑、一条别处开着算进顶栏。
/// 深浅各一张（`hero-dark`、`hero-light`），拷成 docs/images、site/img 的 board-dark.png、board-light.png。
fn hero(home: &Path, out: &Path) {
    use huitai::core::{Account, Vendor};
    use huitai::host::{self, interrupt};

    // 设置里写的是 GEMINI_CLI_HOME 的值（Gemini CLI 的家目录），数据在它下面的 .gemini/tmp。
    let work = home.join("gemini-work/.gemini");
    for (i, f) in GEMINI_WORK.iter().enumerate() {
        write_gemini(home, &work, &format!("w{i}"), &uuid(0x950 + i), f);
    }
    for (i, f) in GEMINI_HERO.iter().enumerate() {
        write_gemini(
            home,
            &home.join(".gemini"),
            &format!("h{i}"),
            &uuid(0x610 + i),
            f,
        );
    }
    // 设置：两家各加一个「工作号」；「等你」关掉慢闪，截图不会碰上暗的那半拍。
    fs::write(
        home.join("Library/Application Support/huitai/config.json"),
        r#"{"blink_waiting":false,"accounts":[{"vendor":"claude","name":"工作号","dir":"~/.claude-work"},{"vendor":"gemini","name":"工作号","dir":"~/gemini-work"}]}"#,
    )
    .unwrap();
    // 第二个账号的对话 id 带账号前缀（SessionId::in_account），托管时用同一个 id 列表才认得出。
    let key = Account::key_for(Vendor::Gemini, &work.join("tmp"), home);
    // 一直有输出（空格加退格，画面不变）才一直算在跑，不然安静 20 秒就当做完了。
    let screen = home.join("hero-screen.bin");
    fs::write(&screen, GEMINI_SCREEN).unwrap();
    let busy = |file: &Path| {
        format!(
            "stty raw -echo; cat '{}'; while :; do sleep 1; printf ' \\b'; done",
            file.display()
        )
    };
    let (gemini_busy, quiet_busy) = (busy(&screen), busy(Path::new("/dev/null")));

    // 桌面端开着一条 Claude 对话：顶栏多一个「●1 别处」（官网图例里有它）。做法同上面浅色那一轮。
    let _desktop = KillOnDrop(
        Command::new("/bin/bash")
            .args([
                "-c",
                &format!("exec -a 'claude --resume {}' sleep 600", uuid(0x106)),
            ])
            .spawn()
            .expect("起假的桌面端进程"),
    );
    // 会话台只从这一次连上以后的响铃知道谁停了，深浅两张各用一个新的守护进程从头来一遍。
    for (light, name) in [(false, "hero-dark"), (true, "hero-light")] {
        // 上一个守护进程退干净、清掉「被中断」的记录（不然会话台一打开就停在被中断组上）。
        let dir = home.join("Library/Application Support/huitai");
        let deadline = Instant::now() + Duration::from_secs(5);
        while host::request(&dir, &host::Request::Ping).is_ok() {
            assert!(Instant::now() < deadline, "上一个守护进程没退出");
            thread::sleep(Duration::from_millis(20));
        }
        for path in [
            interrupt::running_path(&dir),
            interrupt::interrupted_path(&dir),
        ] {
            let _ = fs::remove_file(path);
        }
        let daemon = LiveHost::bare(home);
        let go = home.join(format!("{name}-go"));
        let bell = format!(
            "stty raw -echo; while [ ! -e '{}' ]; do sleep 0.1; done; printf '\\007'; sleep 600",
            go.display()
        );
        let work_id =
            |i: usize| huitai::core::SessionId::in_account(&key, &uuid(0x950 + i)).to_string();
        daemon.host(
            home,
            work_id(0),
            Vendor::Gemini,
            GEMINI_WORK[0].project,
            &gemini_busy,
        );
        daemon.host(
            home,
            uuid(0x100),
            Vendor::Claude,
            CLAUDE[0].project,
            &quiet_busy,
        );
        daemon.host(
            home,
            uuid(0x200),
            Vendor::Codex,
            CODEX[0].project,
            &quiet_busy,
        );
        daemon.host(
            home,
            work_id(1),
            Vendor::Gemini,
            GEMINI_WORK[1].project,
            &bell,
        );
        daemon.host(home, uuid(0x600), Vendor::Gemini, GEMINI[0].project, &bell);
        hero_shot(&daemon, home, out, light, name, &go);
        drop(daemon);
    }
}

/// 打开会话台切到 Gemini，等两条响铃、blog 那条看过一眼，截一张。
fn hero_shot(daemon: &LiveHost, home: &Path, out: &Path, light: bool, name: &str, go: &Path) {
    let run = Run::start_sized(home, light, HERO_COLS, HERO_ROWS);
    // 先等列表读出来，不然 Tab 会在界面起来前被吞掉。
    run.wait_for(CLAUDE[0].title);
    for _ in 0..huitai::core::Vendor::ALL.len() {
        if run.text().contains(GEMINI_WORK[0].title) {
            break;
        }
        run.press(b"\t");
    }
    run.wait_for(GEMINI_WORK[0].title);
    run.wait_for("正在跑测试");
    run.wait_for("1 别处");
    // 等响铃的两条还没出声时也算在跑。
    run.wait_for("5 在跑");
    // 两条一起响铃停下（没人看着，算刚做完），再像在标签页里看过一眼那样连上又断开 blog 那条：它变成等你回复。
    fs::write(go, "").unwrap();
    run.wait_for("3 在跑");
    run.wait_for("◆2 待处理");
    daemon.peek(home, uuid(0x600));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(run.text().contains('✓') && run.text().matches('◆').count() >= 2) {
        assert!(
            Instant::now() < deadline,
            "没等到刚做完和等你回复两种状态：\n{}",
            run.text()
        );
        thread::sleep(Duration::from_millis(20));
    }
    // 等「刚停下」那一下整行高亮淡完再截。
    thread::sleep(huitai::ui::app::FLASH + Duration::from_millis(300));
    assert_eq!(
        run.text().matches("工作号").count(),
        2,
        "两条带账号名：\n{}",
        run.text()
    );
    let text = run.text();
    let top = text.lines().next().unwrap_or_default();
    for v in huitai::core::Vendor::ALL {
        assert!(
            top.contains(&format!("{} {}", v.mark(), v.name())),
            "顶栏要写下每家全名，{} 缩掉了：\n{text}",
            v.name()
        );
    }
    let blank = text.lines().filter(|l| l.trim().is_empty()).count();
    assert!(blank <= 2, "首图不留大片空行（{blank} 行空）：\n{text}");
    run.save(out, name);
    run.quit();
}

/// 测试起的子进程：离开作用域（包括测试中途 panic）时杀掉并收尸。
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
