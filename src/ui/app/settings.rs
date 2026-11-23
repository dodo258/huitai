//! 设置页的按键和改设置。

use super::*;

impl App {
    pub(super) fn settings_key(&mut self, key: KeyEvent, cursor: usize) -> Vec<Effect> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.overlay = Overlay::Settings {
                    cursor: cursor.saturating_sub(1),
                };
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.overlay = Overlay::Settings {
                    cursor: (cursor + 1).min(SETTINGS_ITEMS - 1),
                };
            }
            KeyCode::Left | KeyCode::Char('h') => return self.change_setting(cursor, false),
            KeyCode::Right | KeyCode::Char('l') => return self.change_setting(cursor, true),
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(',' | 'q') => {
                self.overlay = Overlay::None;
                return vec![Effect::SaveConfig(self.config.clone())];
            }
            _ => {}
        }
        Vec::new()
    }

    fn change_setting(&mut self, cursor: usize, forward: bool) -> Vec<Effect> {
        match cursor {
            0 => {
                let now = self.config.preferred_terminal.as_deref();
                let i = TERMINAL_CHOICES.iter().position(|c| *c == now).unwrap_or(0);
                let n = TERMINAL_CHOICES.len();
                let next = if forward {
                    (i + 1) % n
                } else {
                    (i + n - 1) % n
                };
                self.config.preferred_terminal = TERMINAL_CHOICES[next].map(str::to_owned);
            }
            1 => {
                let d = self.config.trash_days;
                self.config.trash_days = if forward {
                    (d + 1).min(365)
                } else {
                    d.saturating_sub(1).max(1)
                };
            }
            4 => self.config.blink_waiting = !self.config.blink_waiting,
            5 => self.config.auto_ascii_input = !self.config.auto_ascii_input,
            6 => self.config.backup_all_claude = !self.config.backup_all_claude,
            7 => self.config.check_updates = !self.config.check_updates,
            8 => self.config.auto_upgrade_clis = !self.config.auto_upgrade_clis,
            3 => {
                let now = self.config.idle_reclaim_minutes;
                // 手改 config.json 写了列表外的数：从不小于它的那一档接着走。
                let i = IDLE_CHOICES
                    .iter()
                    .position(|m| *m >= now)
                    .unwrap_or(IDLE_CHOICES.len() - 1);
                let n = IDLE_CHOICES.len();
                let next = if forward {
                    (i + 1) % n
                } else {
                    (i + n - 1) % n
                };
                self.config.idle_reclaim_minutes = IDLE_CHOICES[next];
            }
            // 开关：←→ 都是切换。是否成功由运行层写登录项后定，失败会改回来。
            _ => {
                self.autostart = !self.autostart;
                return vec![Effect::SetAutostart(self.autostart)];
            }
        }
        Vec::new()
    }
}
