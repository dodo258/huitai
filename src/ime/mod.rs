//! 输入法：会话台在前台时切到英文键盘布局，离开时切回原来的输入源（方案 docs/specs/keys-and-ime.md 第 1 条）。
//! 只依赖 `core`。对外接口：[`InputSources`]（读、切输入源）和 [`Switcher`]（何时切）。
//!
//! 真实现 [`Tis`] 走 Carbon TIS（`TISCopyCurrentKeyboardInputSource` / `TISSelectInputSource`），
//! 只能在主线程调；测试用假实现，不碰本机真的输入法。

#[cfg(target_os = "macos")]
mod tis;

#[cfg(target_os = "macos")]
pub use tis::Tis;

use std::io;

/// 优先切到的英文键盘布局（ABC，其次 U.S.）；都没启用就取用户启用的第一个能打 ASCII 的键盘布局。
pub const PREFERRED_ASCII: [&str; 2] = ["com.apple.keylayout.ABC", "com.apple.keylayout.US"];

/// 一个输入源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSource {
    /// 如 `com.apple.inputmethod.SCIM.ITABC`（拼音）、`com.apple.keylayout.ABC`。
    pub id: String,
    /// 能直接打出 ASCII（英文键盘布局）；中文、日文等输入法为假。
    pub ascii: bool,
}

/// 读和切输入源。
pub trait InputSources {
    /// 当前输入源；读不到为 `None`。
    fn current(&self) -> Option<InputSource>;
    /// 用户启用的、能打 ASCII 的键盘布局 id。
    fn ascii_layouts(&self) -> Vec<String>;
    fn select(&self, id: &str) -> io::Result<()>;
}

/// 从启用的 ASCII 键盘布局里挑一个：先 [`PREFERRED_ASCII`]，否则第一个。
pub fn pick_ascii(layouts: &[String]) -> Option<String> {
    PREFERRED_ASCII
        .iter()
        .find(|p| layouts.iter().any(|l| l == *p))
        .map(|p| (*p).to_owned())
        .or_else(|| layouts.first().cloned())
}

/// 什么时候切：会话台拿到焦点切英文，失去焦点、交出终端、打开对话标签页、退出时切回。
///
/// 只切回自己切走的：切回时当前输入源已经不是我们切到的那个（用户在会话台里自己换过），就不动它。
pub struct Switcher<S> {
    sources: S,
    enabled: bool,
    focused: bool,
    /// 切走前的输入源和切到的英文布局。
    saved: Option<(String, String)>,
}

impl<S: InputSources> Switcher<S> {
    /// `enabled` 是设置「进会话台自动切英文输入」。刚开始当作不在前台，等焦点事件（或 [`Switcher::focus_gained`]）。
    pub fn new(sources: S, enabled: bool) -> Self {
        Self {
            sources,
            enabled,
            focused: false,
            saved: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 终端报会话台拿到了焦点。
    pub fn focus_gained(&mut self) -> io::Result<()> {
        self.focused = true;
        self.switch_to_ascii()
    }

    /// 终端报会话台失去了焦点。
    pub fn focus_lost(&mut self) -> io::Result<()> {
        self.focused = false;
        self.restore()
    }

    /// 暂时离开（打开对话标签页、交出终端给对话或编辑器、退出）：切回原来的输入源，焦点状态不变。
    pub fn leave(&mut self) -> io::Result<()> {
        self.restore()
    }

    /// 交出的终端拿回来了：还在前台就再切英文。
    pub fn back(&mut self) -> io::Result<()> {
        if self.focused {
            return self.switch_to_ascii();
        }
        Ok(())
    }

    /// 设置里开关改了：关掉立刻切回；打开时在前台就切英文。
    pub fn set_enabled(&mut self, on: bool) -> io::Result<()> {
        if on == self.enabled {
            return Ok(());
        }
        if on {
            self.enabled = true;
            return self.back();
        }
        let r = self.restore();
        self.enabled = false;
        r
    }

    fn switch_to_ascii(&mut self) -> io::Result<()> {
        if !self.enabled || self.saved.is_some() {
            return Ok(());
        }
        let Some(cur) = self.sources.current() else {
            return Ok(());
        };
        if cur.ascii {
            return Ok(());
        }
        let Some(target) = pick_ascii(&self.sources.ascii_layouts()) else {
            return Ok(());
        };
        self.sources.select(&target)?;
        self.saved = Some((cur.id, target));
        Ok(())
    }

    fn restore(&mut self) -> io::Result<()> {
        let Some((original, target)) = self.saved.take() else {
            return Ok(());
        };
        if self.sources.current().is_some_and(|c| c.id == target) {
            self.sources.select(&original)?;
        }
        Ok(())
    }
}
