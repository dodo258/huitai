//! 鼠标：滚轮、单击双击、右键。

use super::*;

impl App {
    pub fn mouse(&mut self, ev: MouseEvent, now: Instant) -> Vec<Effect> {
        let pos = Position::new(ev.column, ev.row);
        // 整段对话开着：滚轮翻它，点击不穿透到下面的列表。确认提示、菜单这些照常。
        if self.prompt.is_none()
            && self.overlay == Overlay::None
            && let Some(r) = &mut self.reader
        {
            match ev.kind {
                MouseEventKind::ScrollDown => r.scroll(WHEEL_STEP as isize),
                MouseEventKind::ScrollUp => r.scroll(-(WHEEL_STEP as isize)),
                _ => {}
            }
            return Vec::new();
        }
        match ev.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = ev.kind == MouseEventKind::ScrollDown;
                if self.overlay != Overlay::None || self.prompt.is_some() {
                    return Vec::new();
                }
                if self.areas.preview.contains(pos) {
                    self.preview_scroll = if down {
                        self.preview_scroll.saturating_sub(WHEEL_STEP)
                    } else {
                        self.preview_scroll + WHEEL_STEP
                    };
                } else {
                    let step = WHEEL_STEP as isize;
                    self.move_by(if down { step } else { -step });
                }
                Vec::new()
            }
            MouseEventKind::Down(MouseButton::Left) => self.click(pos, now),
            MouseEventKind::Down(MouseButton::Right) => self.right_click(pos),
            _ => Vec::new(),
        }
    }

    /// 列表里第几行在 `pos` 这一格（搜索时是第几条结果）；不在列表里或下面没有行为 `None`。
    fn row_at(&self, pos: Position) -> Option<usize> {
        if !self.areas.list.contains(pos) {
            return None;
        }
        let offset = match (&self.board, &self.search) {
            (Some(b), _) => b.offset,
            (None, Some(s)) if self.in_hits() => s.offset,
            _ => self.list().offset,
        };
        let y = usize::from(pos.y - self.areas.list.y);
        // 第一行被钉住的组标题盖着时，点到的是那个组标题。
        if y == 0
            && !self.in_hits()
            && self.board.is_none()
            && self.areas.list.height >= 2
            && let Some(h) = self.list().sticky()
        {
            return Some(h);
        }
        // 有动态的对话占两行：从第一行往下数到点的那一格。
        let heights = self.line_heights();
        let mut top = 0;
        for (i, h) in heights.iter().enumerate().skip(offset) {
            if y < top + h {
                return Some(i);
            }
            top += h;
        }
        None
    }

    /// 右键：选中这一行，在点的地方弹出它能做的操作。
    fn right_click(&mut self, pos: Position) -> Vec<Effect> {
        self.status = None;
        if self.prompt.is_some()
            || matches!(
                self.overlay,
                Overlay::Help
                    | Overlay::Settings { .. }
                    | Overlay::Balance { .. }
                    | Overlay::CliFailures { .. }
            )
        {
            return Vec::new();
        }
        self.overlay = Overlay::None;
        let Some(i) = self.row_at(pos) else {
            return Vec::new();
        };
        self.touched = true;
        self.last_click = None;
        self.select(i);
        let sel = self.selected_kind();
        if !self.menu_items(sel).is_empty() {
            self.overlay = Overlay::Menu {
                sel,
                x: pos.x,
                y: pos.y,
                cursor: 0,
            };
        }
        Vec::new()
    }

    fn click(&mut self, pos: Position, now: Instant) -> Vec<Effect> {
        self.status = None;
        if let Overlay::Menu { sel, .. } = self.overlay {
            // 点在某一项上就执行它，点别处只关菜单。
            let m = self.areas.menu;
            if m.contains(pos) {
                return self.run_menu_item(sel, usize::from(pos.y - m.y));
            }
            self.overlay = Overlay::None;
            return Vec::new();
        }
        if self.overlay != Overlay::None {
            if let Overlay::Settings { .. } = self.overlay {
                self.overlay = Overlay::None;
                return vec![Effect::SaveConfig(self.config.clone())];
            }
            self.overlay = Overlay::None;
            return Vec::new();
        }
        if self.prompt.is_some() {
            return Vec::new();
        }
        self.touched = true;
        // 点的是顶栏、底栏这些地方（不是列表行）：上一次单击作废，免得接着点列表里同一屏幕行的另一条被当成双击打开。
        let bars = [
            self.areas.background,
            self.areas.status,
            self.areas.elsewhere,
            self.areas.path,
            self.areas.update,
        ];
        if bars.iter().any(|r| r.contains(pos))
            || self.areas.tabs.iter().any(|(_, r)| r.contains(pos))
        {
            self.last_click = None;
        }
        if self.areas.background.contains(pos) {
            return self.toggle_background();
        }
        if self.areas.status.contains(pos) {
            return self.next_waiting();
        }
        if self.areas.elsewhere.contains(pos) {
            return self.next_elsewhere();
        }
        if self.areas.path.contains(pos) {
            return self.with_dir(Effect::Finder);
        }
        if self.areas.update.contains(pos) {
            return self.press_update();
        }
        if let Some((v, _)) = self.areas.tabs.iter().find(|(_, r)| r.contains(pos)) {
            let v = *v;
            self.board = None;
            return self.set_vendor(v);
        }
        let Some(i) = self.row_at(pos) else {
            // 点在列表行以外（顶栏、分隔线、空白处）：上一次单击作废，接着点回列表不算双击。
            self.last_click = None;
            return Vec::new();
        };
        let double = self
            .last_click
            .is_some_and(|(p, t)| p.y == pos.y && now.duration_since(t) <= DOUBLE_CLICK);
        self.select(i);
        // 单击组标题就收起/展开。
        if matches!(self.selected_row(), Some(Row::Header(_))) {
            self.last_click = None;
            return self.toggle_selected_group();
        }
        if double {
            self.last_click = None;
            return self.open(false);
        }
        self.last_click = Some((pos, now));
        Vec::new()
    }
}
