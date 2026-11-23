//! 键盘：按键分派、各动作、底栏输入和确认、`$` 面板滚动。

use super::*;

impl App {
    pub fn key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        self.status = None;
        self.touched = true;
        if self.prompt.is_some() {
            return self.prompt_key(key);
        }
        match self.overlay {
            Overlay::Help => {
                self.overlay = Overlay::None;
                return Vec::new();
            }
            Overlay::Settings { cursor } => return self.settings_key(key, cursor),
            Overlay::Balance { scroll } => {
                self.balance_key(key, scroll);
                return Vec::new();
            }
            Overlay::Menu { sel, cursor, .. } => {
                if let Some(effects) = self.menu_key(key, sel, cursor) {
                    return effects;
                }
            }
            Overlay::None => {}
        }
        if let Some(r) = &mut self.reader {
            return match r.key(key) {
                Outcome::Stay => Vec::new(),
                Outcome::Close => {
                    self.reader = None;
                    Vec::new()
                }
                Outcome::Open { here } => {
                    let e = (*r.entry).clone();
                    self.open_entry(e, here)
                }
                Outcome::Export => vec![Effect::Export(r.entry.clone())],
                Outcome::Quit => {
                    self.quit = true;
                    Vec::new()
                }
            };
        }
        if self.search.is_some() {
            return self.search_key(key);
        }
        if key.code == KeyCode::Esc && self.board.take().is_some() {
            return Vec::new();
        }
        if let Some(a) = action_for(key) {
            return self.act(a);
        }
        // 中文输入法没切成英文时打出的字：直接当搜索词。
        if let Some(c) = typed_text(key) {
            self.board = None;
            self.search = Some(Search::default());
            return self.edit_query(|q| q.push(c));
        }
        Vec::new()
    }

    pub(super) fn act(&mut self, a: Action) -> Vec<Effect> {
        match a {
            Action::NextVendor => return self.switch_vendor(true),
            Action::PrevVendor => return self.switch_vendor(false),
            Action::Up => self.move_by(-1),
            Action::Down => self.move_by(1),
            Action::PageUp => self.move_by(-self.page()),
            Action::PageDown => self.move_by(self.page()),
            Action::Top => self.select(0),
            Action::Bottom => self.select(usize::MAX),
            Action::Collapse => return self.collapse(),
            Action::Expand => return self.expand(),
            Action::ToggleBackground => return self.toggle_background(),
            Action::Search => {
                self.board = None;
                self.search = Some(Search::default());
            }
            // 汇总里按数字：回到列表跳到那个项目。
            Action::Jump(n) => {
                self.board = None;
                return self.jump(n);
            }
            Action::NextWaiting => return self.next_waiting(),
            Action::Board => self.toggle_board(),
            Action::CopyResume => match self.selected_entry() {
                Some(e) => return vec![Effect::CopyResume(Box::new(e.clone()))],
                None => self.status = Some("先选一条对话".to_owned()),
            },
            Action::Read => self.open_reader(),
            Action::Open => return self.open(false),
            Action::OpenHere => return self.open(true),
            // 选中一条对话时用它那家（汇总里可能是别家的）。
            Action::New => {
                let cwd = self
                    .selected_dir()
                    .unwrap_or_else(|| self.launch_dir.clone());
                let vendor = self
                    .selected_entry()
                    .map_or(self.vendor, |e| e.session.vendor);
                return vec![Effect::New { vendor, cwd }];
            }
            Action::NewElsewhere => {
                let dirs =
                    newdir::project_dirs(self.lists.iter().flat_map(|l| &l.entries), &self.home);
                self.prompt = Some(Prompt::NewDir {
                    vendor: self.vendor,
                    picker: DirPicker::new(&self.home, dirs),
                });
                return vec![Effect::ScanProjectsRoot(self.projects_root())];
            }
            // 有被中断的对话时 R 先接着做它们；没有才是恢复上次所有标签。
            Action::RestoreTabs => {
                let stopped = self.interrupted_entries();
                if stopped.is_empty() {
                    return vec![Effect::RestoreTabs];
                }
                return vec![Effect::ResumeInterrupted(stopped)];
            }
            Action::Rename if self.selected_entry().is_none() => self.ask_rename_project(),
            Action::Archive if self.selected_entry().is_none() => return self.archive_project(),
            Action::Rename => {
                if let Some(e) = self.selected_entry() {
                    self.prompt = Some(Prompt::Rename {
                        vendor: e.session.vendor,
                        id: e.session.id.clone(),
                        text: e.title().to_owned(),
                    });
                }
            }
            Action::Pin => {
                return self.annotate(|e| Edit::Pin(!e.note.pinned));
            }
            Action::Archive => {
                return self.annotate(|e| Edit::Archive(!e.note.archived));
            }
            Action::Trash => {
                if let Some(e) = self.selected_entry() {
                    return vec![Effect::Trash(Box::new(e.clone()))];
                }
            }
            Action::Untrash => return vec![Effect::Untrash(self.vendor)],
            Action::Kill => return self.ask_kill(),
            Action::Editor => return self.with_dir(Effect::Editor),
            Action::Finder => return self.with_dir(Effect::Finder),
            Action::GitStatus => return self.with_dir(Effect::GitStatus),
            Action::Settings => self.overlay = Overlay::Settings { cursor: 0 },
            Action::Balance => {
                self.overlay = Overlay::Balance { scroll: 0 };
                return vec![Effect::RefreshBalances];
            }
            Action::Help => self.overlay = Overlay::Help,
            Action::Quit => self.quit = true,
        }
        Vec::new()
    }

    /// 选中的组标题是项目组时，返回它的目录；不是就在底栏说一句。
    fn selected_project_header(&mut self) -> Option<PathBuf> {
        if let Some(Row::Header(g)) = self.selected_row()
            && let GroupKey::Project(p) = &self.list().groups[g].key
        {
            return Some(p.clone());
        }
        if self.selected_row().is_some() {
            self.status = Some("选中一个项目组再按".to_owned());
        }
        None
    }

    /// 项目组标题上按 e：给项目起显示名（输入框里先放现在的名字）。
    fn ask_rename_project(&mut self) {
        if let Some(project) = self.selected_project_header() {
            self.prompt = Some(Prompt::RenameProject {
                text: self.project_label(&project),
                project,
            });
        }
    }

    /// 项目组标题上按 a：整组移进归档，已归档的就取消。对话本身的标注不动。
    fn archive_project(&mut self) -> Vec<Effect> {
        let Some(project) = self.selected_project_header() else {
            return Vec::new();
        };
        let archived = !self.projects.get(&project).is_some_and(|n| n.archived);
        let name = self.project_label(&project);
        self.status = Some(if archived {
            format!("「{name}」整组移进归档（再按 a 取消）")
        } else {
            format!("「{name}」移出归档")
        });
        vec![Effect::AnnotateProject {
            project,
            edit: ProjectEdit::Archive(archived),
        }]
    }

    pub(super) fn with_dir(&mut self, make: fn(PathBuf) -> Effect) -> Vec<Effect> {
        match self.selected_dir() {
            Some(d) => vec![make(d)],
            None => {
                self.status = Some("先选一条对话或一个项目".to_owned());
                Vec::new()
            }
        }
    }

    fn annotate(&mut self, edit: impl Fn(&Entry) -> Edit) -> Vec<Effect> {
        match self.selected_entry() {
            Some(e) => vec![Effect::Annotate {
                vendor: e.session.vendor,
                id: e.session.id.clone(),
                edit: edit(e),
            }],
            None => Vec::new(),
        }
    }

    /// x：在跑的问一句再结束；被中断的（没在跑）直接移出「被中断」组。
    fn ask_kill(&mut self) -> Vec<Effect> {
        let Some(e) = self.selected_entry() else {
            return Vec::new();
        };
        let id = e.session.id.clone();
        if self.state_of(&id) == SessionState::Stopped && !self.attached.contains(&id) {
            if self.interrupted.contains(&id) {
                return vec![Effect::Dismiss(id)];
            }
            self.status = Some("这条对话没有在后台跑".to_owned());
            return Vec::new();
        }
        self.prompt = Some(Prompt::ConfirmKill {
            title: e.title().to_owned(),
            id: self.hosted_id(&id).clone(),
        });
        Vec::new()
    }

    /// 选中的对话打开整段对话；从搜索结果打开时带上搜索词（高亮、n 跳）。
    pub(super) fn open_reader(&mut self) {
        let Some(e) = self.selected_entry() else {
            self.status = Some("先选一条对话".to_owned());
            return;
        };
        let query = match &self.search {
            Some(s) if self.in_hits() => s.query.as_str(),
            _ => "",
        };
        self.reader = Some(Reader::new(e.clone(), query));
    }

    pub(super) fn open(&mut self, here: bool) -> Vec<Effect> {
        if let Some(e) = self.selected_entry() {
            return self.open_entry(e.clone(), here);
        }
        // 组标题上按回车：收起着就进去（展开并选中第一条），展开着就收起。
        let Some(Row::Header(g)) = self.selected_row() else {
            return Vec::new();
        };
        if !self.folds.is_collapsed(&self.list().groups[g].key) {
            return self.set_collapsed(g, true);
        }
        let effects = self.set_collapsed(g, false);
        let l = self.list_mut();
        if let Some(i) = l
            .rows
            .iter()
            .position(|r| matches!(r, Row::Item { group, .. } if *group == g))
        {
            l.selected = i;
        }
        effects
    }

    /// 打开这条对话；它正在别处运行就先问一句。没有本机项目目录的只能查看，给个提示不打开。
    fn open_entry(&mut self, e: Entry, here: bool) -> Vec<Effect> {
        if !e.session.resumable() {
            self.status = Some(VIEW_ONLY.to_owned());
            return Vec::new();
        }
        let ask = self.elsewhere(&e);
        let entry = Box::new(e);
        if ask {
            self.prompt = Some(Prompt::ConfirmOpen { entry, here });
            return Vec::new();
        }
        vec![Effect::Open { entry, here }]
    }

    fn prompt_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(prompt) = self.prompt.take() else {
            return Vec::new();
        };
        if let Prompt::ConfirmKill { id, .. } = prompt {
            return match key.code {
                KeyCode::Char('y') => vec![Effect::Kill(id)],
                _ => Vec::new(),
            };
        }
        if let Prompt::ConfirmOpen { entry, here } = prompt {
            return match key.code {
                KeyCode::Enter => vec![Effect::Open { entry, here }],
                _ => Vec::new(),
            };
        }
        if let Prompt::ConfirmCreate { vendor, dir } = prompt {
            return match key.code {
                KeyCode::Enter => vec![Effect::CreateDir { vendor, cwd: dir }],
                _ => Vec::new(),
            };
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let mut prompt = prompt;
        if let Prompt::NewDir { picker, .. } = &mut prompt {
            match key.code {
                KeyCode::Esc => return Vec::new(),
                KeyCode::Enter => return self.submit(prompt),
                KeyCode::Tab => _ = picker.complete(),
                KeyCode::Down => picker.down(),
                KeyCode::Up => picker.up(),
                KeyCode::Backspace => picker.edit(|t| _ = t.pop()),
                KeyCode::Char('u') if ctrl => picker.edit(String::clear),
                KeyCode::Char(c) if !ctrl => picker.edit(|t| t.push(c)),
                _ => {}
            }
            self.prompt = Some(prompt);
            return Vec::new();
        }
        let text = match &mut prompt {
            Prompt::Rename { text, .. } | Prompt::RenameProject { text, .. } => text,
            _ => unreachable!("上面已处理"),
        };
        match key.code {
            KeyCode::Esc => return Vec::new(),
            KeyCode::Enter => return self.submit(prompt),
            KeyCode::Backspace => _ = text.pop(),
            KeyCode::Char('u') if ctrl => text.clear(),
            KeyCode::Char(c) if !ctrl => text.push(c),
            _ => {}
        }
        self.prompt = Some(prompt);
        Vec::new()
    }

    fn submit(&mut self, prompt: Prompt) -> Vec<Effect> {
        match prompt {
            Prompt::Rename { vendor, id, text } => {
                let title = Some(text).filter(|t| !t.trim().is_empty());
                vec![Effect::Annotate {
                    vendor,
                    id,
                    edit: Edit::Rename(title),
                }]
            }
            Prompt::RenameProject { project, text } => vec![Effect::AnnotateProject {
                project,
                edit: ProjectEdit::Rename(Some(text).filter(|t| !t.trim().is_empty())),
            }],
            Prompt::NewDir { vendor, picker } => {
                let Some(cwd) = picker.target(&self.projects_root(), &self.launch_dir) else {
                    // 什么都没输：输入框留着。
                    self.prompt = Some(Prompt::NewDir { vendor, picker });
                    return Vec::new();
                };
                if cwd.is_dir() {
                    return vec![Effect::New { vendor, cwd }];
                }
                if cwd.exists() {
                    self.status = Some(format!("这不是目录：{}", tilde(&cwd, &self.home)));
                    return Vec::new();
                }
                self.prompt = Some(Prompt::ConfirmCreate { vendor, dir: cwd });
                Vec::new()
            }
            Prompt::ConfirmCreate { vendor, dir } => vec![Effect::CreateDir { vendor, cwd: dir }],
            Prompt::ConfirmKill { id, .. } => vec![Effect::Kill(id)],
            Prompt::ConfirmOpen { entry, here } => vec![Effect::Open { entry, here }],
        }
    }

    /// 额度面板：↑↓ 翻，`$`、Esc、q、⏎ 关。
    fn balance_key(&mut self, key: KeyEvent, scroll: usize) {
        let code = match key.code {
            KeyCode::Char(c) => KeyCode::Char(keys::halfwidth(c)),
            c => c,
        };
        self.overlay = match code {
            KeyCode::Up | KeyCode::Char('k') => Overlay::Balance {
                scroll: scroll.saturating_sub(1),
            },
            KeyCode::Down | KeyCode::Char('j') => Overlay::Balance { scroll: scroll + 1 },
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('$' | 'q') => Overlay::None,
            _ => Overlay::Balance { scroll },
        };
    }
}
