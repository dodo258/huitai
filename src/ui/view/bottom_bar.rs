//! 底栏：按键提示、输入和确认。

use super::list::key_hints;
use super::*;

pub(super) fn bottom_bar(buf: &mut Buffer, area: Rect, app: &mut App, t: &Theme, accent: Color) {
    fill(buf, area, t.bar);
    app.areas.background = Rect::default();
    app.areas.path = Rect::default();
    let inner = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    };
    let cursor = sp("▏", fg(accent));
    if let Some(p) = &app.prompt {
        let (left, right) = match p {
            Prompt::Rename { text, .. } => (
                vec![
                    sp("改名  ", fg(t.muted)),
                    sp(text.clone(), fg(t.title)),
                    cursor,
                ],
                "⏎ 确定 · 留空恢复原名 · Esc 取消",
            ),
            Prompt::NewDir { vendor, picker } => {
                let (label, hints) =
                    new_dir_texts(*vendor, width(&picker.text), usize::from(inner.width));
                let mut left = vec![
                    sp(label, fg(t.muted)),
                    sp(picker.text.clone(), fg(t.title)),
                    cursor,
                ];
                // 没选候选时写出 ⏎ 会去哪（只输名字是 projects_root 下面）。
                if picker.picked.is_none()
                    && let Some(dir) = picker.target(&app.projects_root(), &app.launch_dir)
                {
                    left.push(sp(format!("  → {}", tilde(&dir, &app.home)), fg(t.faint)));
                }
                (left, hints)
            }
            Prompt::ConfirmCreate { dir, .. } => (
                vec![sp(
                    format!("新建目录 {}？", tilde(dir, &app.home)),
                    fg(t.state(SessionState::Waiting)),
                )],
                "⏎ 确认 · Esc 取消",
            ),
            Prompt::RenameProject { text, .. } => (
                vec![
                    sp("项目显示名  ", fg(t.muted)),
                    sp(text.clone(), fg(t.title)),
                    cursor,
                ],
                "⏎ 确定 · 留空恢复目录名 · 不动文件夹 · Esc 取消",
            ),
            Prompt::ConfirmKill { title, .. } => (
                vec![sp(
                    format!("结束「{}」的后台进程？", truncate(title, 30)),
                    fg(t.state(SessionState::Waiting)),
                )],
                "y 确定 · 其他键取消",
            ),
            Prompt::ConfirmOpen { .. } => (
                vec![sp(
                    "这条正在别处运行，再开会两边同时写",
                    fg(t.state(SessionState::Waiting)),
                )],
                "⏎ 仍然打开 · Esc 取消",
            ),
        };
        row(buf, inner, left, key_hints(right, t, accent));
        return;
    }
    if let Some(r) = &app.reader {
        r.bottom(buf, inner, app.status.as_deref(), t);
        return;
    }
    if app.board.is_some() {
        let entries = app.board_entries();
        let waiting = entries
            .iter()
            .filter(|e| app.state_of(&e.session.id) != SessionState::Running)
            .count();
        let running = entries.len() - waiting;
        let left = vec![
            sp("s ", bold(accent)),
            sp(format!("待处理 {waiting} · 在跑 {running}"), fg(t.title)),
        ];
        // 只给现在按了有用的键：空汇总只能返回，没有待处理的就不给 `!`。
        let keys = match (entries.len(), waiting) {
            (0, _) => "Esc 返回",
            (_, 0) => "⏎ 打开 · Esc 返回",
            _ => "⏎ 打开 · ! 下一条待处理 · Esc 返回",
        };
        let right = match &app.status {
            Some(s) => vec![sp(s.clone(), fg(t.text))],
            None => key_hints(keys, t, accent),
        };
        row(buf, inner, left, right);
        return;
    }
    if let Some(s) = &app.search {
        let left = vec![
            sp("/ ", bold(accent)),
            sp(s.query.clone(), fg(t.title)),
            cursor,
        ];
        let scope = if s.all { "各家" } else { app.vendor.name() };
        let count = if s.query.is_empty() {
            format!("在 {scope} 搜标题和正文")
        } else {
            format!("{scope} {} 条", s.hits.len())
        };
        let toggle = if s.all {
            format!("⌃A 只搜 {}", app.vendor.name())
        } else {
            "⌃A 搜各家".to_owned()
        };
        let mut right = vec![sp(format!("{count} · "), fg(t.faint))];
        right.extend(key_hints(
            &format!("⏎ 打开 · → 看全文 · {toggle} · Esc 返回"),
            t,
            accent,
        ));
        row(buf, inner, left, right);
        return;
    }
    let mut left = Vec::new();
    // 常驻的「后台 M 在跑」，单击等同 b。
    let running = app.background_running();
    if running > 0 {
        // 只给圆点上状态色，字用次要色：后台对话不是要你处理的事，不和自己的「在跑」抢眼。
        let label = format!("后台 {running} 在跑");
        app.areas.background = Rect {
            width: (width(&label) as u16 + 2).min(inner.width),
            ..inner
        };
        left.push(sp(
            format!("{} ", SPIN_MARK),
            fg(t.state(SessionState::Running)),
        ));
        left.push(sp(label, fg(t.muted)));
        left.push(sp("   ", fg(t.faint)));
    }
    // 按键用这家的强调色：能按的东西一个颜色。
    for k in keys::bar(app.selected_kind()) {
        left.push(sp(k.keys, bold(accent)));
        left.push(sp(format!(" {}   ", k.label), fg(t.muted)));
    }
    let right = match &app.status {
        Some(s) => vec![sp(s.clone(), fg(t.text))],
        // 选中的对话或项目组所在的目录，和左边隔两格；放不下截掉中间，最少留 16 格（左边的按键让位）。
        // 单击在访达里打开。
        None => app.selected_dir().map_or_else(Vec::new, |dir| {
            const LABEL: &str = "路径：";
            let room = usize::from(inner.width)
                .saturating_sub(line_width(&left) + 2 + width(LABEL))
                .max(16);
            let path = truncate_middle(&tilde(&dir, &app.home), room);
            vec![
                sp(format!("  {LABEL}"), fg(t.faint)),
                sp(path, fg(t.muted).add_modifier(Modifier::UNDERLINED)),
            ]
        }),
    };
    if app.status.is_none() && !right.is_empty() {
        let w = line_width(&right).min(usize::from(inner.width)) as u16;
        app.areas.path = Rect {
            x: inner.right() - w,
            width: w,
            ..inner
        };
    }
    row(buf, inner, left, right);
}

/// 目录输入框的提示语和右边的按键：输入的字要看得到，放不下时先去掉开哪家，再减按键，最后去掉提示语。
/// 按给输入留 [`INPUT_ROOM`] 格挑，打字时提示语不来回换。
fn new_dir_texts(vendor: Vendor, text: usize, w: usize) -> (String, &'static str) {
    const INPUT_ROOM: usize = 24;
    const KEYS: &str = "Tab 补全 · ↑↓ 选 · ⏎ 确定 · Esc 取消";
    const FEW_KEYS: &str = "⏎ 确定 · Esc 取消";
    const SHORT: &str = "新项目名或目录  ";
    let long = format!("新项目名或目录，开 {} 对话  ", vendor.name());
    // 光标一格，和右边的按键隔两格。
    let fits = |label: &str, input: usize, hints: &str| {
        let gap = if hints.is_empty() { 0 } else { 2 };
        width(label) + input + 1 + gap + width(hints) <= w
    };
    let room = text.max(INPUT_ROOM);
    for (label, hints) in [(long.as_str(), KEYS), (SHORT, KEYS), (SHORT, FEW_KEYS)] {
        if fits(label, room, hints) {
            return (label.to_owned(), hints);
        }
    }
    let label = if fits(SHORT, text, "") { SHORT } else { "" };
    (label.to_owned(), "")
}
