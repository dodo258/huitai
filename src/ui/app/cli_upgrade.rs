//! 各家 CLI 升级：顶栏「更新」按钮（U 或点一下）、升级进度、没升上的原文（点开看）、自动升级的结果进状态栏。

use super::*;

/// 后台线程查完一轮：有新版的（厂商 → 新版本号）和其中能一键升的（顺序照 [`Vendor::ALL`]）。每次是完整的一份。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliReport {
    pub newer: HashMap<Vendor, String>,
    pub ready: Vec<Vendor>,
}

/// 升完一家：成没成、给人看的话（没成带命令和原文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliResult {
    pub vendor: Vendor,
    pub ok: bool,
    pub text: String,
}

/// 「更新」按钮这会儿在干什么。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CliUpgrade {
    #[default]
    Idle,
    Running {
        done: usize,
        total: usize,
    },
    /// 上一次一键升级有没升上的（原文留着，点开看）。
    Failed(Vec<CliResult>),
}

impl App {
    /// 顶栏按钮上的字：有能一键升的写「更新」，升级中「更新中 2/4」，有没升上的「1 家没升上」；
    /// 都不是（或关了检查新版本）就不显示按钮。
    pub fn update_button(&self) -> Option<String> {
        if !self.config.check_updates {
            return None;
        }
        match &self.cli_upgrade {
            CliUpgrade::Running { done, total } => Some(format!("更新中 {done}/{total}")),
            CliUpgrade::Failed(f) => Some(format!("{} 家没升上", f.len())),
            CliUpgrade::Idle => (!self.cli_ready.is_empty()).then(|| "更新".to_owned()),
        }
    }

    /// 按 U 或点按钮：能升就开始升；正在升说一句；有没升上的打开原文。
    pub(super) fn press_update(&mut self) -> Vec<Effect> {
        match &self.cli_upgrade {
            CliUpgrade::Running { .. } => self.status = Some("正在升级，等它跑完".to_owned()),
            CliUpgrade::Failed(_) => self.overlay = Overlay::CliFailures { scroll: 0 },
            CliUpgrade::Idle if self.update_button().is_some() => return self.start_upgrade(),
            CliUpgrade::Idle => self.status = Some("各家 CLI 没有要升级的".to_owned()),
        }
        Vec::new()
    }

    fn start_upgrade(&mut self) -> Vec<Effect> {
        if self.cli_ready.is_empty() {
            self.status = Some("没有能一键升级的了，看 ai doctor".to_owned());
            return Vec::new();
        }
        self.cli_upgrade = CliUpgrade::Running {
            done: 0,
            total: self.cli_ready.len(),
        };
        vec![Effect::UpgradeClis]
    }

    /// 没升上的原文：↑↓ 翻，U 再试一次，Esc / q / 回车关掉（按钮留着，还能点开）。
    pub(super) fn cli_failures_key(&mut self, key: KeyEvent, scroll: usize) -> Vec<Effect> {
        let code = match key.code {
            KeyCode::Char(c) => KeyCode::Char(keys::halfwidth(c)),
            c => c,
        };
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.overlay = Overlay::CliFailures {
                    scroll: scroll.saturating_sub(1),
                };
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.overlay = Overlay::CliFailures { scroll: scroll + 1 };
            }
            KeyCode::Char('U') => {
                self.overlay = Overlay::None;
                return self.start_upgrade();
            }
            _ => self.overlay = Overlay::None,
        }
        Vec::new()
    }

    /// 后台线程查完一轮。之前没升上的那家已经不是旧版了（主人自己升了），从没升上的里去掉。
    pub fn set_cli_report(&mut self, report: CliReport) {
        if let CliUpgrade::Failed(f) = &mut self.cli_upgrade {
            f.retain(|r| report.newer.contains_key(&r.vendor));
            if f.is_empty() {
                self.cli_upgrade = CliUpgrade::Idle;
            }
        }
        self.cli_updates = report.newer;
        self.cli_ready = report.ready;
    }

    /// 一键升级升完一家。
    pub fn cli_progress(&mut self, done: usize, total: usize) {
        self.cli_upgrade = CliUpgrade::Running { done, total };
    }

    /// 一键升级全部跑完：都成了按钮消失、状态栏说一句；有没成的按钮变成「N 家没升上」。
    pub fn cli_upgrade_done(&mut self, results: Vec<CliResult>) {
        let done = self.forget_upgraded(&results);
        let failed: Vec<CliResult> = results.into_iter().filter(|r| !r.ok).collect();
        if failed.is_empty() {
            self.cli_upgrade = CliUpgrade::Idle;
            self.status = Some(if done.is_empty() {
                "没有要升级的了".to_owned()
            } else {
                format!("已升级：{}", done.join("、"))
            });
        } else {
            self.status = Some(format!("{} 家没升上，按 U 或点顶栏看原文", failed.len()));
            self.cli_upgrade = CliUpgrade::Failed(failed);
        }
    }

    /// 自动升级跑完的：成了的标记去掉；结果只在状态栏说一句（不弹系统通知），详细的在 `ai doctor`。
    pub fn cli_auto_done(&mut self, results: Vec<CliResult>) {
        let done = self.forget_upgraded(&results);
        let failed: Vec<&str> = results
            .iter()
            .filter(|r| !r.ok)
            .map(|r| r.vendor.name())
            .collect();
        let mut parts = Vec::new();
        if !done.is_empty() {
            parts.push(format!("已自动升级：{}", done.join("、")));
        }
        if !failed.is_empty() {
            parts.push(format!(
                "{} 自动升级没成（ai doctor 看原文，按 U 再试）",
                failed.join("、")
            ));
        }
        if !parts.is_empty() {
            self.status = Some(parts.join("；"));
        }
    }

    /// 升上了的那几家从「有新版」「能一键升」里去掉，返回「Claude Code 2.1.331」这样的说法。
    fn forget_upgraded(&mut self, results: &[CliResult]) -> Vec<String> {
        let mut done = Vec::new();
        for r in results.iter().filter(|r| r.ok) {
            let version = self.cli_updates.remove(&r.vendor);
            self.cli_ready.retain(|v| *v != r.vendor);
            done.push(match version {
                Some(v) => format!("{} {v}", r.vendor.name()),
                None => r.vendor.name().to_owned(),
            });
        }
        done
    }
}
