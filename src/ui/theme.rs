//! 界面颜色，全部集中在这里。深色值取自方案「界面」节；浅色是同一套角色的对应值。
//! 强调色一次只用一个，随当前标签（厂商）切换。
//!
//! 颜色按角色分，同一个颜色只表示一类东西：
//! - 这家的强调色（Claude 橙、Codex 蓝、pi 紫、Hermes 黄铜、OpenCode 青绿、Antigravity 黄绿，色值在 `core::vendor` 的表里）：能按的——印章、选中细条、序号、按键提示；项目名带三成。
//!   顶栏标签照 gitui、ratatui Tabs：不垫底色，标签之间淡竖线，当前这家强调色加粗加下划线；底色只给印章。
//! - 状态色只给状态灯和状态计数：在跑绿、等你黄、刚做完青、别处柔绿、已停灰。青色不用蓝，免得和 Codex 的蓝混。
//! - 红色只给快用完的额度和出错。
//! - 其余是灰阶：标题、正文、次要（表头、组名以外的说明）、暗字（时间、路径、账号名、动态行）。
//!   四级灰阶在底色、选中底色、栏底色、预览底色和展开组的垫色上对比度都不低于 4.5:1（WCAG AA），
//!   相邻两级至少差 1.2 倍，主次靠亮度梯度和粗细分，不靠把暗字压到看不清（测试 `grays_readable_and_ordered`）。
//! - 当字用的状态色（在跑、等你、别处）在底色、选中底色、栏底色、预览底色上同样不低于 4.5:1（测试 `state_words_readable`）。
//!   已停是故意压暗的灯，不算。

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use ratatui::style::Color;

use crate::core::{SessionState, Vendor};

/// 一套颜色。字段名是角色，不是色值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// 列表区底色。
    pub base: Color,
    /// 顶栏、底栏、浮层底色。
    pub bar: Color,
    /// 预览区底色。
    pub preview: Color,
    /// 选中行的淡底色。
    pub select: Color,
    /// 正文。
    pub text: Color,
    /// 标题。
    pub title: Color,
    /// 次要文字（组名、元信息）。
    pub muted: Color,
    /// 暗文字（时间、路径提示、计数）。
    pub faint: Color,
    /// 浅色那套：厂商强调色取 [`crate::core::VendorInfo`] 里浅色的值。
    light: bool,
    running: Color,
    waiting: Color,
    done: Color,
    stopped: Color,
    elsewhere: Color,
    alert: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl Theme {
    pub const DARK: Theme = Theme {
        base: rgb(0x17181c),
        bar: rgb(0x1d1e23),
        preview: rgb(0x131417),
        select: rgb(0x24262c),
        text: rgb(0xc4c2b9),
        title: rgb(0xf2efe7),
        muted: rgb(0xaaa79f),
        faint: rgb(0x918e86),
        light: false,
        running: rgb(0x78c98f),
        waiting: rgb(0xe8b85a),
        done: rgb(0x5ec4d6),
        stopped: rgb(0x4d4b46),
        elsewhere: rgb(0x5c9a6c),
        alert: rgb(0xe5534b),
    };

    /// 浅色终端：中性灰底（不用奶油色），强调色压暗一档保证白底上的对比度。
    pub const LIGHT: Theme = Theme {
        base: rgb(0xf4f4f2),
        bar: rgb(0xe9e9e6),
        preview: rgb(0xfbfbfa),
        select: rgb(0xe2e1dc),
        text: rgb(0x302f2c),
        title: rgb(0x141518),
        muted: rgb(0x4b4a45),
        faint: rgb(0x64625c),
        light: true,
        running: rgb(0x166e38),
        waiting: rgb(0x815800),
        done: rgb(0x16808f),
        stopped: rgb(0xb5b2aa),
        elsewhere: rgb(0x466b4f),
        alert: rgb(0xc23a2f),
    };

    /// 这家的强调色，深浅两套都在 `core::vendor` 的表里。
    pub fn accent(&self, vendor: Vendor) -> Color {
        let info = vendor.info();
        rgb(if self.light {
            info.accent_light
        } else {
            info.accent_dark
        })
    }

    pub fn state(&self, state: SessionState) -> Color {
        match state {
            SessionState::Running => self.running,
            SessionState::Waiting => self.waiting,
            SessionState::Done => self.done,
            SessionState::Stopped => self.stopped,
        }
    }

    /// 慢闪暗半拍时状态符号的颜色：同一个色往顶栏底色退六成，色相不变、只是暗下去。
    pub fn blink_dim(&self, color: Color) -> Color {
        mix(color, self.bar, 0.6)
    }

    /// 展开的组标题整行垫的底色：列表底色带一点这家的强调色，深浅色下都看得出，又和选中行的中性灰分得开。
    pub fn band(&self, accent: Color) -> Color {
        mix(self.base, accent, 0.10)
    }

    /// 别处在用（桌面端、别的终端里活着的对话）：比「在跑」柔一档的绿，看得出活着，又和会话台自己托管的分开。
    pub fn elsewhere(&self) -> Color {
        self.elsewhere
    }

    /// 顶栏「更新」按钮的底色：能升级时垫一层「刚做完」的青，升级中淡一些，有没升上的垫红。字用标题色。
    pub fn update_button(&self, kind: UpdateButton) -> Color {
        match kind {
            UpdateButton::Ready => mix(self.bar, self.done, 0.45),
            UpdateButton::Running => mix(self.bar, self.done, 0.25),
            UpdateButton::Failed => mix(self.bar, self.alert, 0.40),
        }
    }

    /// 快用完了（额度 95% 起）：比 Claude 的橙红更正的红，只留给真要当心的地方。
    pub fn alert(&self) -> Color {
        self.alert
    }

    /// 跟随终端深浅色：`HUITAI_THEME=light|dark` 优先，其次问终端背景色（OSC 11），
    /// 再看 `COLORFGBG`，都没有就用深色。必须在进原始模式、开备用屏之前调用。
    pub fn detect() -> Theme {
        let env = |k: &str| std::env::var(k).ok();
        let light = match env("HUITAI_THEME").as_deref() {
            Some("light") => Some(true),
            Some("dark") => Some(false),
            _ => None,
        }
        .or_else(|| query_background().map(is_light))
        .or_else(|| env("COLORFGBG").and_then(|v| colorfgbg_is_light(&v)))
        .unwrap_or(false);
        if light { Theme::LIGHT } else { Theme::DARK }
    }
}

/// 背景色亮度过半算浅色。
/// 「更新」按钮的三种样子（底色不同）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateButton {
    Ready,
    Running,
    Failed,
}

pub fn is_light((r, g, b): (u8, u8, u8)) -> bool {
    let lum = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
    lum > 128.0
}

/// `COLORFGBG=15;0` 这种：最后一段是背景色号，7 和 9–15 是浅色。
pub fn colorfgbg_is_light(value: &str) -> Option<bool> {
    let bg: u8 = value.rsplit(';').next()?.trim().parse().ok()?;
    Some(bg == 7 || bg >= 9)
}

/// 解析终端对 OSC 11 的回答：`ESC ] 11 ; rgb:RRRR/GGGG/BBBB` 加 BEL 或 ST（每段 1–4 位十六进制）。
pub fn parse_osc11(reply: &[u8]) -> Option<(u8, u8, u8)> {
    let text = std::str::from_utf8(reply).ok()?;
    let rest = &text[text.find("rgb:")? + 4..];
    let end = rest
        .find(|c: char| !(c.is_ascii_hexdigit() || c == '/'))
        .unwrap_or(rest.len());
    let mut parts = rest[..end].split('/').map(|p| {
        let v = u32::from_str_radix(p, 16).ok()?;
        let max = (1u32 << (4 * p.len().clamp(1, 4))) - 1;
        u8::try_from(v * 255 / max).ok()
    });
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// 问终端背景色，最多等 100ms。不是终端或终端不回答就是 `None`。
fn query_background() -> Option<(u8, u8, u8)> {
    use ratatui::crossterm::terminal;
    let stdin = io::stdin();
    // SAFETY: isatty 只读取 fd 状态。
    if unsafe { libc::isatty(stdin.as_raw_fd()) } != 1 {
        return None;
    }
    terminal::enable_raw_mode().ok()?;
    let reply = ask_osc11(stdin.as_raw_fd());
    let _ = terminal::disable_raw_mode();
    parse_osc11(&reply?)
}

fn ask_osc11(fd: i32) -> Option<Vec<u8>> {
    let mut out = io::stdout();
    out.write_all(b"\x1b]11;?\x1b\\").ok()?;
    out.flush().ok()?;
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut reply = Vec::new();
    let mut buf = [0u8; 64];
    while !(reply.ends_with(b"\x07") || reply.ends_with(b"\x1b\\")) {
        let left = deadline.checked_duration_since(Instant::now())?;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = i32::try_from(left.as_millis()).unwrap_or(100);
        // SAFETY: pfd 是有效的单个 pollfd。
        if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
            return None;
        }
        let n = io::stdin().lock().read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        reply.extend_from_slice(&buf[..n]);
    }
    Some(reply)
}

/// `a` 往 `b` 走 `k`（0–1）。主题色都是 RGB；别的颜色没法混，原样用 `a`。
pub fn mix(a: Color, b: Color, k: f32) -> Color {
    let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (a, b) else {
        return a;
    };
    let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * k).round() as u8;
    Color::Rgb(m(ar, br), m(ag, bg), m(ab, bb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osc11_replies() {
        assert_eq!(
            parse_osc11(b"\x1b]11;rgb:1717/1818/1c1c\x07"),
            Some((0x17, 0x18, 0x1c))
        );
        assert_eq!(
            parse_osc11(b"\x1b]11;rgb:ff/fe/fd\x1b\\"),
            Some((0xff, 0xfe, 0xfd))
        );
        assert_eq!(parse_osc11(b"garbage"), None);
    }

    #[test]
    fn decides_light_or_dark() {
        assert!(!is_light((0x17, 0x18, 0x1c)));
        assert!(is_light((0xf4, 0xf4, 0xf2)));
        assert_eq!(colorfgbg_is_light("15;0"), Some(false));
        assert_eq!(colorfgbg_is_light("0;default;15"), Some(true));
        assert_eq!(colorfgbg_is_light("x"), None);
    }

    /// WCAG 2 的对比度：(亮 + 0.05) / (暗 + 0.05)。
    fn contrast(a: Color, b: Color) -> f64 {
        let lum = |c: Color| {
            let Color::Rgb(r, g, b) = c else {
                panic!("主题色都是 RGB：{c:?}")
            };
            let ch = |v: u8| {
                let v = f64::from(v) / 255.0;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
        };
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// 账号名、动态行、时间这些暗字在列表会出现的每种底色上都要读得清（不低于 4.5:1），
    /// 四级灰阶从标题到暗字依次变暗、相邻两级至少差 1.2 倍，主次层级还在。
    #[test]
    fn grays_readable_and_ordered() {
        for t in [Theme::DARK, Theme::LIGHT] {
            let mut grounds = vec![t.base, t.select, t.bar, t.preview];
            grounds.extend(Vendor::ALL.map(|v| t.band(t.accent(v))));
            let levels = [t.title, t.text, t.muted, t.faint];
            for bg in &grounds {
                for fg in levels {
                    let c = contrast(fg, *bg);
                    assert!(c >= 4.5, "{fg:?} 在 {bg:?} 上只有 {c:.2}:1");
                }
                for pair in levels.windows(2) {
                    let (hi, lo) = (contrast(pair[0], *bg), contrast(pair[1], *bg));
                    assert!(
                        hi >= lo * 1.2,
                        "{:?}（{hi:.2}）和 {:?}（{lo:.2}）在 {bg:?} 上分不开",
                        pair[0],
                        pair[1]
                    );
                }
            }
        }
    }

    /// 在跑、等你、别处这三个状态色会当字用（顶栏计数、列表的「等回复」、动态行），在顶栏和列表会出现的底色上都要读得清。
    #[test]
    fn state_words_readable() {
        for t in [Theme::DARK, Theme::LIGHT] {
            let words = [
                t.state(SessionState::Running),
                t.state(SessionState::Waiting),
                t.elsewhere(),
            ];
            for bg in [t.bar, t.base, t.select, t.preview] {
                for fg in words {
                    let c = contrast(fg, bg);
                    assert!(c >= 4.5, "{fg:?} 在 {bg:?} 上只有 {c:.2}:1");
                }
            }
        }
    }

    #[test]
    fn accents_differ_per_vendor() {
        let t = Theme::DARK;
        assert_ne!(t.accent(Vendor::Claude), t.accent(Vendor::Codex));
        assert_eq!(t.accent(Vendor::Claude), Color::Rgb(0xd9, 0x77, 0x57));
        assert_eq!(
            Theme::LIGHT.accent(Vendor::Claude),
            Color::Rgb(0xc0, 0x60, 0x3f)
        );
        assert_eq!(t.accent(Vendor::Hermes), Color::Rgb(0xbf, 0xae, 0x3a));
    }
}
