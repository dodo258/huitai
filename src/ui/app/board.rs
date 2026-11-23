//! 「等你回复」汇总（`s`），和 `!` 跳到下一条等你回复的对话。

use super::*;

impl App {
    /// `!`：从选中处往下找下一条等你回复的自己的对话，这家找完接着找下一家（Tab 的顺序），绕一圈回来。
    /// 在别家就换过去，在收起的组里就展开它。
    /// 汇总开着时只在汇总里往下找（绕一圈回来），不离开汇总。
    pub(super) fn next_waiting(&mut self) -> Vec<Effect> {
        if self.board.is_some() {
            let entries = self.board_entries();
            let at = self.board_index(&entries).unwrap_or(0);
            let n = entries.len();
            let next = (1..=n)
                .map(|k| (at + k) % n)
                .find(|&i| waits(self.state_of(&entries[i].session.id)));
            match next {
                Some(i) => self.select(i),
                None => self.status = Some("没有等你回复的对话".to_owned()),
            }
            return Vec::new();
        }
        self.next_where(
            |app, e| !e.session.automatic && waits(app.state_of(&e.session.id)),
            "没有等你回复的对话",
        )
    }

    /// 单击顶栏「别处」：跳到下一条别处在用的对话，三家轮着找。
    pub(super) fn next_elsewhere(&mut self) -> Vec<Effect> {
        self.next_where(
            |app, e| !e.session.automatic && app.elsewhere(e),
            "没有别处在用的对话",
        )
    }

    /// 从选中的位置往后找下一条满足 `hit` 的对话（三家轮着找，到底了从头），切到那家、展开它的组、选中它。
    fn next_where(&mut self, hit: impl Fn(&Self, &Entry) -> bool, none: &str) -> Vec<Effect> {
        // 搜索时单击顶栏状态：先退出搜索回到列表再跳（搜索结果的行号和列表对不上）。
        // `!` 键在搜索框里是输入的字，到不了这里。
        self.search = None;
        self.board = None;
        // 只在还支持的各家（标签顺序）里找。
        let n = Vendor::ALL.len();
        let here = Vendor::ALL
            .iter()
            .position(|&v| v == self.vendor)
            .unwrap_or(0);
        // 位置按（第几家，第几组，组里第几条）排；组标题排在它第一条前面。
        let current = match self.selected_row() {
            Some(Row::Header(g)) => (0, g, None),
            Some(Row::Item { group, entry }) => {
                let at = self.list().groups[group]
                    .items
                    .iter()
                    .position(|&i| i == entry);
                (0, group, at)
            }
            None => (0, 0, None),
        };
        let mut targets = Vec::new();
        for k in 0..n {
            let l = &self.lists[Vendor::ALL[(here + k) % n].index()];
            for (gi, g) in l.groups.iter().enumerate() {
                for (p, &ei) in g.items.iter().enumerate() {
                    let e = &l.entries[ei];
                    if hit(self, e) {
                        targets.push(((k, gi, Some(p)), ei));
                    }
                }
            }
        }
        let Some(&((k, gi, _), ei)) = targets
            .iter()
            .find(|(at, _)| *at > current)
            .or_else(|| targets.first())
        else {
            self.status = Some(none.to_owned());
            return Vec::new();
        };
        let mut effects = self.set_vendor(Vendor::ALL[(here + k) % n]);
        // 目标在收着的「很久没聊」分区里：先打开分区，不然它的组整个不在列表里，选不中。
        if self.list().groups[gi].stale && self.folds.is_collapsed(&GroupKey::Stale) {
            self.folds.set(&GroupKey::Stale, false);
            self.reflow_all();
        }
        if self.folds.is_collapsed(&self.list().groups[gi].key) {
            effects.extend(self.set_collapsed(gi, false));
        }
        let item = Row::Item {
            group: gi,
            entry: ei,
        };
        if let Some(i) = self.list().rows.iter().position(|r| *r == item) {
            self.select(i);
        }
        effects
    }

    /// 汇总里列的对话：三家自己的、托管着没停的（和顶栏「在跑 / 等你」数的是同一批），
    /// 按 要输入 > 失败 > 做完待看 > 在跑 排（[`App::board_rank`]），各自最近聊过的在前。
    pub fn board_entries(&self) -> Vec<&Entry> {
        let mut out: Vec<&Entry> = self
            .lists
            .iter()
            .flat_map(|l| &l.entries)
            .filter(|e| {
                !e.session.automatic && self.state_of(&e.session.id) != SessionState::Stopped
            })
            .collect();
        out.sort_by_key(|e| (self.board_rank(e), Reverse(e.session.last_active)));
        out
    }

    /// 汇总里的先后：等你回复 0、失败 1、刚做完 2、在跑 3。
    fn board_rank(&self, e: &Entry) -> u8 {
        if self.failed(e) {
            return 1;
        }
        match self.state_of(&e.session.id) {
            SessionState::Waiting => 0,
            SessionState::Done => 2,
            _ => 3,
        }
    }

    /// 汇总里选中第几条（汇总没开为 `None`）：选中的那条还在就是它现在的位置，不在了停在原来的位置上。
    pub fn board_index(&self, entries: &[&Entry]) -> Option<usize> {
        let b = self.board.as_ref()?;
        let at =
            b.id.as_ref()
                .and_then(|id| entries.iter().position(|e| e.session.id == *id));
        Some(
            at.unwrap_or(b.selected)
                .min(entries.len().saturating_sub(1)),
        )
    }

    /// s：打开 / 关上汇总。打开时停在第一条（等得最近的那条等你回复）上。
    pub(super) fn toggle_board(&mut self) {
        if self.board.take().is_some() {
            return;
        }
        self.search = None;
        self.board = Some(Board::default());
        self.select(0);
    }
}
