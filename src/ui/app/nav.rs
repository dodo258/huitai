//! 选中：当前选的是哪条、上下移动、换家、展开收起分组。

use super::*;

impl App {
    pub fn selected_entry(&self) -> Option<&Entry> {
        if self.board.is_some() {
            let entries = self.board_entries();
            return self
                .board_index(&entries)
                .and_then(|i| entries.get(i).copied());
        }
        if let Some(s) = self.search.as_ref().filter(|_| self.in_hits()) {
            return s.hits.get(s.selected);
        }
        let l = self.list();
        match l.rows.get(l.selected)? {
            Row::Item { entry, .. } => l.entries.get(*entry),
            Row::Header(_) => None,
        }
    }

    pub fn selected_group(&self) -> Option<&Group> {
        if self.in_hits() || self.board.is_some() {
            return None;
        }
        let l = self.list();
        match l.rows.get(l.selected)? {
            Row::Header(g) | Row::Item { group: g, .. } => l.groups.get(*g),
        }
    }

    /// 选中的是组标题、在跑的对话还是停着的对话（底栏提示和右键菜单按它换）。
    pub fn selected_kind(&self) -> Selected {
        if let Some(e) = self.selected_entry() {
            let id = &e.session.id;
            if self.state_of(id) != SessionState::Stopped || self.attached.contains(id) {
                return Selected::Running;
            }
            return Selected::Stopped;
        }
        match self.selected_row() {
            Some(Row::Header(g)) => {
                let key = &self.list().groups[g].key;
                Selected::Group {
                    expanded: !self.folds.is_collapsed(key),
                    project: matches!(key, GroupKey::Project(_)),
                }
            }
            _ => Selected::Nothing,
        }
    }

    /// 选中的东西所在的项目目录。
    pub fn selected_dir(&self) -> Option<PathBuf> {
        if let Some(e) = self.selected_entry() {
            return Some(e.session.cwd.clone());
        }
        match &self.selected_group()?.key {
            GroupKey::Project(p) => Some(p.clone()),
            _ => None,
        }
    }

    fn list_len(&self) -> usize {
        if self.board.is_some() {
            return self.board_entries().len();
        }
        match &self.search {
            Some(s) if self.in_hits() => s.hits.len(),
            _ => self.list().rows.len(),
        }
    }

    pub(super) fn select(&mut self, i: usize) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let i = i.min(len - 1);
        let before = self.selected_entry().map(|e| e.session.id.clone());
        if self.board.is_some() {
            let id = self.board_entries().get(i).map(|e| e.session.id.clone());
            if let Some(b) = &mut self.board {
                b.selected = i;
                b.id = id;
            }
        } else if self.in_hits() {
            if let Some(s) = &mut self.search {
                s.selected = i;
            }
        } else {
            self.list_mut().selected = i;
        }
        if self.selected_entry().map(|e| e.session.id.clone()) != before {
            self.preview_scroll = 0;
            self.git = None;
        }
    }

    fn selected_index(&self) -> usize {
        if self.board.is_some() {
            return self.board_index(&self.board_entries()).unwrap_or(0);
        }
        match &self.search {
            Some(s) if self.in_hits() => s.selected,
            _ => self.list().selected,
        }
    }

    pub(super) fn move_by(&mut self, delta: isize) {
        let i = self.selected_index().saturating_add_signed(delta);
        self.select(i);
    }

    pub(super) fn page(&self) -> isize {
        self.areas.list.height.max(2) as isize - 1
    }

    /// Tab 换家：汇总开着就先关上，回到那家的列表。
    pub(super) fn switch_vendor(&mut self, forward: bool) -> Vec<Effect> {
        self.board = None;
        let n = self.shown.len().max(1);
        let i = self
            .shown
            .iter()
            .position(|v| *v == self.vendor)
            .unwrap_or(0);
        let next = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        match self.shown.get(next) {
            Some(&v) => self.set_vendor(v),
            None => Vec::new(),
        }
    }

    /// 换到 `v` 这家；各家一起搜着时也回到只搜这家（点着的就是这家也一样）。
    pub(super) fn set_vendor(&mut self, v: Vendor) -> Vec<Effect> {
        let all = self.search.as_ref().is_some_and(|s| s.all);
        if v == self.vendor && !all {
            return Vec::new();
        }
        if v != self.vendor {
            self.vendor = v;
            self.preview_scroll = 0;
            self.git = None;
        }
        match &mut self.search {
            Some(s) => {
                s.all = false;
                self.research()
            }
            None => Vec::new(),
        }
    }

    /// 选中的是组标题就收起/展开它。
    pub(super) fn toggle_selected_group(&mut self) -> Vec<Effect> {
        match self.selected_row() {
            Some(Row::Header(g)) => {
                let collapsed = self.folds.is_collapsed(&self.list().groups[g].key);
                self.set_collapsed(g, !collapsed)
            }
            _ => Vec::new(),
        }
    }

    /// 列表（不是搜索结果）里选中的那一行。
    pub(super) fn selected_row(&self) -> Option<Row> {
        if self.in_hits() || self.board.is_some() {
            return None;
        }
        let l = self.list();
        l.rows.get(l.selected).copied()
    }

    /// 收起或展开当前这家的第 `gi` 组并选中它的标题；项目组的展开状态记进标注。
    pub(super) fn set_collapsed(&mut self, gi: usize, collapsed: bool) -> Vec<Effect> {
        let key = self.list().groups[gi].key.clone();
        if self.folds.is_collapsed(&key) == collapsed {
            return Vec::new();
        }
        self.folds.set(&key, collapsed);
        self.reflow_all();
        let l = self.list_mut();
        if let Some(i) = l.rows.iter().position(|r| *r == Row::Header(gi)) {
            l.selected = i;
        }
        match key {
            GroupKey::Project(project) => vec![Effect::SetExpanded {
                project,
                expanded: !collapsed,
            }],
            _ => Vec::new(),
        }
    }

    pub(super) fn collapse(&mut self) -> Vec<Effect> {
        match self.selected_row() {
            Some(Row::Header(g) | Row::Item { group: g, .. }) => self.set_collapsed(g, true),
            None => Vec::new(),
        }
    }

    /// `→`：组标题上展开（展开着就进第一条），对话上看整段对话。
    pub(super) fn expand(&mut self) -> Vec<Effect> {
        let Some(Row::Header(g)) = self.selected_row() else {
            if self.selected_entry().is_some() {
                self.open_reader();
            }
            return Vec::new();
        };
        if self.folds.is_collapsed(&self.list().groups[g].key) {
            return self.set_collapsed(g, false);
        }
        self.move_by(1);
        Vec::new()
    }

    pub(super) fn jump(&mut self, n: u8) -> Vec<Effect> {
        if self.in_hits() {
            return Vec::new();
        }
        let Some(g) = self.list().groups.iter().position(|g| g.number == Some(n)) else {
            return Vec::new();
        };
        let effects = self.set_collapsed(g, false);
        // 选中组里第一条。
        let l = self.list_mut();
        if let Some(i) = l.rows.iter().position(|r| *r == Row::Header(g)) {
            l.selected = i;
        }
        self.move_by(1);
        effects
    }
}
