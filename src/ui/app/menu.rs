//! 右键菜单：开着时的按键、执行选中的那项。

use super::*;

impl App {
    /// 右键菜单开着时按键：↑↓ 选、⏎ 执行选中项、Esc 关。别的键先关菜单，再照普通模式处理（返回 `None`），
    /// 所以按菜单上标的字母和点那一项一样。
    pub(super) fn menu_key(
        &mut self,
        key: KeyEvent,
        sel: Selected,
        cursor: usize,
    ) -> Option<Vec<Effect>> {
        let Overlay::Menu { x, y, .. } = self.overlay else {
            return None;
        };
        let n = self.menu_items(sel).len();
        let at = |cursor| Overlay::Menu { sel, x, y, cursor };
        match key.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Up | KeyCode::Char('k') => self.overlay = at(cursor.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => {
                self.overlay = at((cursor + 1).min(n.saturating_sub(1)));
            }
            KeyCode::Enter => return Some(self.run_menu_item(sel, cursor)),
            _ => {
                self.overlay = Overlay::None;
                return None;
            }
        }
        Some(Vec::new())
    }

    /// 执行右键菜单第 `i` 项：关菜单，对选中项做这一项的动作。
    pub(super) fn run_menu_item(&mut self, sel: Selected, i: usize) -> Vec<Effect> {
        self.overlay = Overlay::None;
        match self.menu_items(sel).get(i) {
            Some(item) => self.act(item.action),
            None => Vec::new(),
        }
    }
}
