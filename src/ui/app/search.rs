//! 搜索：结果、命中片段、搜索框的输入。

use super::*;

impl App {
    /// 搜索结果，都当只命中了标题（没有命中片段），见 [`App::set_search_hits`]。
    pub fn set_hits(&mut self, vendor: Vendor, query: &str, hits: Vec<Entry>) {
        let hits = hits
            .into_iter()
            .map(|entry| Hit {
                entry,
                in_title: true,
                in_body: false,
            })
            .collect();
        self.set_search_hits(Some(vendor), query, hits);
    }

    /// 现在的搜索搜哪：当前这家，或 `None` 各家一起（[`Search::all`]）。
    pub fn search_scope(&self) -> Option<Vendor> {
        match &self.search {
            Some(s) if s.all => None,
            _ => Some(self.vendor),
        }
    }

    /// 搜索结果（`vendor` 为 `None` 是各家一起搜的）；不是现在的搜索范围、或查询词已经变了的旧结果丢掉。
    /// 同一个查询词再搜一遍（改了名、列表刷新）已经取到的命中片段留着，只有对话本身变了
    /// （正文更新，时间、条数跟着变）的那几条重新取。
    pub fn set_search_hits(&mut self, vendor: Option<Vendor>, query: &str, hits: Vec<Hit>) {
        if vendor != self.search_scope() {
            return;
        }
        if let Some(s) = &mut self.search
            && s.query == query
        {
            if s.snippets_for != query {
                s.snippets.clear();
                s.snippets_for = query.to_owned();
            }
            for old in &s.hits {
                let same = hits
                    .iter()
                    .find(|h| h.entry.session.id == old.session.id)
                    .is_some_and(|h| h.entry.session == old.session);
                if !same {
                    s.snippets.remove(&old.session.id);
                }
            }
            s.in_body = hits
                .iter()
                .filter(|h| h.in_body)
                .map(|h| h.entry.session.id.clone())
                .collect();
            s.hits = hits.into_iter().map(|h| h.entry).collect();
            s.selected = s.selected.min(s.hits.len().saturating_sub(1));
        }
    }

    /// 读的线程取回了一条的命中片段（`None`：没找到或读不了）；查询词已经变了的丢掉。
    pub fn set_snippet(&mut self, query: &str, id: SessionId, snippet: Option<Snippet>) {
        if let Some(s) = &mut self.search
            && s.query == query
            && s.snippets_for == query
        {
            let state = snippet.map_or(SnippetState::Missing, SnippetState::Found);
            s.snippets.insert(id, state);
        }
    }

    /// 搜索结果里这条的命中片段这一行：`None` 是不占这一行（只命中标题、或正文里没找到）。
    pub fn snippet_line(&self, id: &SessionId) -> Option<Option<&Snippet>> {
        let s = self.search.as_ref()?;
        if !s.in_body.contains(id) {
            return None;
        }
        match s.snippets.get(id) {
            Some(SnippetState::Missing) => None,
            Some(SnippetState::Found(snip)) => Some(Some(snip)),
            Some(SnippetState::Asked) | None => Some(None),
        }
    }

    /// 当前屏看得见、还没要过命中片段的搜索结果（标成已要），交给读的线程；没有就是 `None`。
    /// 只取看得见的：每条都要读一遍会话文件，搜索本身不等它。
    pub fn take_snippet_request(&mut self) -> Option<(String, Vec<Entry>)> {
        if !self.in_hits() {
            return None;
        }
        let room = usize::from(self.areas.list.height);
        let heights = self.line_heights();
        let s = self.search.as_mut()?;
        let mut want = Vec::new();
        let mut used = 0;
        for (e, h) in s.hits.iter().zip(&heights).skip(s.offset) {
            if used >= room {
                break;
            }
            used += h;
            let id = &e.session.id;
            if s.in_body.contains(id) && !s.snippets.contains_key(id) {
                s.snippets.insert(id.clone(), SnippetState::Asked);
                want.push(e.clone());
            }
        }
        (!want.is_empty()).then(|| (s.snippets_for.clone(), want))
    }

    pub(super) fn in_hits(&self) -> bool {
        self.search.as_ref().is_some_and(|s| !s.query.is_empty())
    }

    /// 搜索范围变了：清掉旧结果，按新范围重搜（还没输词就不搜）。
    pub(super) fn research(&mut self) -> Vec<Effect> {
        let vendor = self.search_scope();
        let Some(s) = &mut self.search else {
            return Vec::new();
        };
        s.hits.clear();
        s.selected = 0;
        s.offset = 0;
        if s.query.trim().is_empty() {
            return Vec::new();
        }
        vec![Effect::Search {
            vendor,
            query: s.query.clone(),
        }]
    }

    pub(super) fn search_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Esc => self.search = None,
            KeyCode::Enter => {
                return self.open(key.modifiers.contains(KeyModifiers::ALT));
            }
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::Right => self.open_reader(),
            KeyCode::PageUp => self.move_by(-self.page()),
            KeyCode::PageDown => self.move_by(self.page()),
            KeyCode::Tab => return self.switch_vendor(true),
            KeyCode::BackTab => return self.switch_vendor(false),
            KeyCode::Char('a') if ctrl => {
                if let Some(s) = &mut self.search {
                    s.all = !s.all;
                }
                return self.research();
            }
            KeyCode::Backspace => return self.edit_query(|q| _ = q.pop()),
            KeyCode::Char('u') if ctrl => return self.edit_query(String::clear),
            KeyCode::Char(c) if !ctrl => return self.edit_query(|q| q.push(c)),
            _ => {}
        }
        Vec::new()
    }

    pub(super) fn edit_query(&mut self, edit: impl FnOnce(&mut String)) -> Vec<Effect> {
        let Some(s) = &mut self.search else {
            return Vec::new();
        };
        edit(&mut s.query);
        self.research()
    }
}
