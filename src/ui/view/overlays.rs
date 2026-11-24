//! 浮层：目录候选、右键菜单、帮助页、设置页。

use super::list::{key_hints, select_bg};
use super::*;

/// N 的目录候选：贴在底栏上方、和列表一样宽，最上面固定一行「＋ 新建」，下面最多 [`newdir::SHOWN`] 个候选；
/// ↑↓ 选中的那行高亮，没选就是「＋ 新建」那行。
pub(super) fn dir_candidates(
    buf: &mut Buffer,
    list: Rect,
    app: &App,
    picker: &DirPicker,
    t: &Theme,
    accent: Color,
) {
    let matches = picker.matches();
    let fixed = usize::from(picker.new_row.is_some());
    let rows = (fixed + matches.len().min(newdir::SHOWN)).max(1) as u16;
    let h = (rows + 1).min(list.height);
    if h == 0 {
        return;
    }
    let area = Rect {
        y: list.bottom() - h,
        height: h,
        ..list
    };
    fill(buf, area, t.bar);
    let line = |i: u16| Rect {
        x: area.x + 1,
        y: area.y + i,
        width: area.width.saturating_sub(2),
        height: 1,
    };
    let count = match (matches.len(), &picker.new_row) {
        (0, Some(NewRow::Blank)) => vec![],
        (0, new) => {
            let enter = match new {
                Some(_) => "⏎ 新建",
                None => "⏎ 用输入的路径",
            };
            let mut v = vec![sp("没有对得上的 · ", fg(t.faint))];
            v.extend(key_hints(enter, t, accent));
            v
        }
        (n, _) => vec![sp(format!("{n} 个"), fg(t.faint))],
    };
    row(buf, line(0), vec![sp(" 目录", bold(t.muted))], count);
    // 选中的那行整行铺底色，返回写字的区域。
    let pick = |buf: &mut Buffer, i: usize, selected: bool| {
        let r = line(1 + i as u16);
        let full = Rect {
            x: area.x,
            width: area.width,
            ..r
        };
        select_bg(buf, full, selected, t, accent);
        r
    };
    let room = usize::from(h - 1);
    if let Some(new) = &picker.new_row
        && room > 0
    {
        let selected = picker.picked.is_none();
        let r = pick(buf, 0, selected);
        row(
            buf,
            r,
            new_row(app, new, usize::from(r.width), selected, t, accent),
            vec![],
        );
    }
    // 「＋ 新建」不跟着滚；选中的超出可见行时候选往下滚。
    let room = room.saturating_sub(fixed);
    let offset = picker.picked.map_or(0, |p| (p + 1).saturating_sub(room));
    for (k, &ci) in matches.iter().enumerate().skip(offset).take(room) {
        let selected = picker.picked == Some(k);
        let r = pick(buf, fixed + k - offset, selected);
        let style = if selected { bold(t.title) } else { fg(t.text) };
        let shown = truncate_middle(
            &picker.candidates[ci].shown,
            usize::from(r.width.saturating_sub(2)),
        );
        row(buf, r, vec![sp(format!(" {shown}"), style)], vec![]);
    }
}

/// 「＋ 新建」那行的字：窄了路径截中间，还没输入时连路径也放不下就只留怎么新建。
fn new_row(
    app: &App,
    new: &NewRow,
    w: usize,
    selected: bool,
    t: &Theme,
    accent: Color,
) -> Vec<Span<'static>> {
    /// 路径截中间后至少留这么多格才写。
    const MIN_ROOT: usize = 8;
    let text = if selected { bold(t.title) } else { fg(t.text) };
    let mut spans = vec![sp(" ＋ ", bold(accent))];
    match new {
        NewRow::Blank => {
            spans.push(sp("新建项目", text));
            spans.push(sp("：输入名字", fg(t.muted)));
            const AT: &str = "，建在 ";
            let room = w.saturating_sub(line_width(&spans) + width(AT) + 1);
            let root = tilde(&app.projects_root(), &app.home);
            if room >= width(&root).min(MIN_ROOT) {
                spans.push(sp(AT, fg(t.muted)));
                spans.push(sp(truncate_middle(&root, room), fg(t.muted)));
            }
        }
        NewRow::Dir(dir) => {
            spans.push(sp("新建 ", text));
            let room = w.saturating_sub(line_width(&spans) + 1);
            spans.push(sp(truncate_middle(&tilde(dir, &app.home), room), text));
        }
    }
    spans
}

/// 右键菜单：左上角贴着点的位置，放不下就往左、往上挪。返回各项所在的区域（一项一行），给鼠标定位用。
pub(super) fn menu(
    buf: &mut Buffer,
    area: Rect,
    items: &[MenuItem],
    (x, y): (u16, u16),
    cursor: usize,
    t: &Theme,
    accent: Color,
) -> Rect {
    let label_w = items.iter().map(|i| width(i.label)).max().unwrap_or(0);
    let keys_w = items.iter().map(|i| width(i.keys)).max().unwrap_or(0);
    // 左右各留两格，标签和按键之间至少四格；上下各留一行。
    let w = ((label_w + keys_w + 8) as u16).min(area.width);
    let h = (items.len() as u16 + 2).min(area.height);
    // 从点的那一行下面弹出，不挡住被点的这一行。
    let boxr = Rect {
        x: x.min(area.right().saturating_sub(w)),
        y: y.saturating_add(1).min(area.bottom().saturating_sub(h)),
        width: w,
        height: h,
    };
    // 左边紧挨着的是宽字符（中文）时它会盖住菜单第一格，先擦掉。
    if boxr.x > area.x {
        for yy in boxr.top()..boxr.bottom() {
            if let Some(c) = buf.cell_mut((boxr.x - 1, yy))
                && width(c.symbol()) > 1
            {
                c.set_symbol(" ");
            }
        }
    }
    fill(buf, boxr, t.bar);
    let rows = Rect {
        y: boxr.y + 1,
        height: h.saturating_sub(2),
        ..boxr
    };
    for (i, item) in items.iter().enumerate().take(usize::from(rows.height)) {
        let r = Rect {
            y: rows.y + i as u16,
            height: 1,
            ..rows
        };
        let selected = i == cursor;
        select_bg(buf, r, selected, t, accent);
        let style = if selected { bold(t.title) } else { fg(t.text) };
        row(
            buf,
            Rect {
                x: r.x + 2,
                width: r.width.saturating_sub(4),
                ..r
            },
            vec![sp(item.label, style)],
            vec![sp(item.keys, bold(t.muted))],
        );
    }
    rows
}

pub(super) fn help(buf: &mut Buffer, area: Rect, t: &Theme, accent: Color) {
    let (left_secs, right_secs) = HELP.split_at(2);
    // 每组：组名、各键、空一行；最后一组后面的空行不占高度。「更多技巧」放在标题行，30 行的终端也放得下全部按键。
    let col_h = |secs: &[(&str, &[super::keys::KeyHelp])]| {
        secs.iter().map(|(_, ks)| ks.len() + 2).sum::<usize>() - 1
    };
    let h = col_h(left_secs).max(col_h(right_secs)) as u16 + 2;
    let boxr = centered(area, 78, h + 2);
    fill(buf, boxr, t.bar);
    let inner = Rect {
        x: boxr.x + 3,
        y: boxr.y + 1,
        width: boxr.width.saturating_sub(6),
        height: boxr.height.saturating_sub(1),
    };
    row(
        buf,
        Rect { height: 1, ..inner },
        vec![sp("全部按键", bold(t.title))],
        vec![sp("更多技巧见 README · 任意键关闭", fg(t.faint))],
    );
    let col_w = inner.width / 2;
    for (ci, secs) in [left_secs, right_secs].into_iter().enumerate() {
        let col = Rect {
            x: inner.x + col_w * ci as u16,
            y: inner.y + 2,
            width: col_w,
            height: inner.height.saturating_sub(2),
        };
        let mut lines = Vec::new();
        for (name, keys) in secs {
            lines.push(Line::from(sp(*name, bold(t.muted))));
            for k in *keys {
                let pad = 8usize.saturating_sub(width(k.keys));
                lines.push(Line::from(vec![
                    sp(format!("{}{}", k.keys, " ".repeat(pad)), bold(accent)),
                    sp(k.label, fg(t.text)),
                ]));
            }
            lines.push(Line::default());
        }
        put_lines(buf, col, lines);
    }
}

/// 闲置收回的时长：没有标签页连着、等你回复、这么久没输出就结束进程（对话记录保留）。
fn idle_label(minutes: u32) -> String {
    match minutes {
        0 => "不收回".to_owned(),
        m if m % 60 == 0 => format!("{} 小时", m / 60),
        m => format!("{m} 分钟"),
    }
}

pub(super) fn settings(
    buf: &mut Buffer,
    area: Rect,
    app: &App,
    cursor: usize,
    t: &Theme,
    accent: Color,
) {
    let cfg = &app.config;
    // 有几个账号的那几家各占一行：标记、各账号名（主账号在前）。
    let multi: Vec<(Vendor, String)> = Vendor::ALL
        .into_iter()
        .filter_map(|v| {
            let names: Vec<&str> = app
                .accounts
                .iter()
                .filter(|a| a.vendor == v)
                .map(|a| a.name.as_str())
                .collect();
            (names.len() > 1).then(|| (v, names.join(" · ")))
        })
        .collect();
    let account_lines = multi.len().max(1) as u16;
    let boxr = centered(area, 64, 18 + account_lines);
    fill(buf, boxr, t.bar);
    let inner = Rect {
        x: boxr.x + 3,
        y: boxr.y + 1,
        width: boxr.width.saturating_sub(6),
        height: boxr.height.saturating_sub(2),
    };
    // 框装不下的行高度为 0，不画（窗口很矮时）。
    let line = |i: u16| Rect {
        y: inner.y + i,
        height: u16::from(i < inner.height),
        ..inner
    };
    row(buf, line(0), vec![sp("设置", bold(t.title))], vec![]);
    let terminal = match TERMINAL_CHOICES
        .iter()
        .find(|c| **c == cfg.preferred_terminal.as_deref())
    {
        Some(None) | None => "跟随当前终端".to_owned(),
        Some(Some("ghostty")) => "Ghostty".to_owned(),
        Some(Some(other)) => {
            let mut s = other.to_string();
            if let Some(f) = s.get_mut(0..1) {
                f.make_ascii_uppercase();
            }
            s
        }
    };
    let items = [
        ("首选终端", terminal),
        ("回收站保留", format!("{} 天", cfg.trash_days)),
        (
            "开机打开",
            if app.autostart { "开" } else { "关" }.to_owned(),
        ),
        ("闲置收回", idle_label(cfg.idle_reclaim_minutes)),
        (
            "等你闪烁",
            if cfg.blink_waiting { "开" } else { "关" }.to_owned(),
        ),
        (
            "自动英文",
            if cfg.auto_ascii_input {
                "开 · 进会话台切英文，离开切回"
            } else {
                "关"
            }
            .to_owned(),
        ),
        (
            "备份对话",
            if cfg.backup_all_claude {
                "全部 Claude 对话"
            } else {
                "只备置顶、改名、归档的 Claude 对话"
            }
            .to_owned(),
        ),
        (
            "检查新版本",
            if cfg.check_updates {
                "开 · 会话台和各家 CLI，每天最多查一次"
            } else {
                "关"
            }
            .to_owned(),
        ),
        (
            "自动升级",
            match (cfg.auto_upgrade_clis, cfg.check_updates) {
                (true, true) => "开 · 各家 CLI 没有对话在跑时升，不跨大版本",
                (true, false) => "开 · 要先开「检查新版本」",
                (false, _) => "关 · 顶栏「更新」手动升",
            }
            .to_owned(),
        ),
        (
            "回会话台",
            match cfg.back_key.label() {
                Some(key) => format!("{key} · 对话里按它回来，对话在后台继续"),
                None => "关 · 对话结束才回到会话台".to_owned(),
            },
        ),
    ];
    for (i, (name, value)) in items.iter().enumerate() {
        let r = line(2 + i as u16);
        let selected = i == cursor;
        let full = Rect {
            x: boxr.x + 1,
            width: boxr.width.saturating_sub(2),
            ..r
        };
        select_bg(buf, full, selected, t, accent);
        let vstyle = if selected { bold(t.title) } else { fg(t.text) };
        row(
            buf,
            r,
            vec![
                sp(format!("{name:<8}"), fg(t.muted)),
                sp("‹ ", fg(t.faint)),
                sp(value.clone(), vstyle),
                sp(" ›", fg(t.faint)),
            ],
            vec![],
        );
    }
    row(
        buf,
        line(12),
        vec![
            sp(format!("{:<8}", "呼出键"), fg(t.muted)),
            sp(cfg.hotkey.clone(), fg(t.text)),
            sp("  在 config.json 里改", fg(t.faint)),
        ],
        vec![],
    );
    if multi.is_empty() {
        row(
            buf,
            line(13),
            vec![
                sp(format!("{:<8}", "账号"), fg(t.muted)),
                sp("每家一个", fg(t.text)),
                sp("  多账号在 config.json 的 accounts 里加", fg(t.faint)),
            ],
            vec![],
        );
    }
    let room = usize::from(inner.width).saturating_sub(12);
    for (i, (v, names)) in multi.iter().enumerate() {
        let label = if i == 0 { "账号" } else { "" };
        row(
            buf,
            line(13 + i as u16),
            vec![
                sp(format!("{label:<8}"), fg(t.muted)),
                sp(format!("{} ", v.mark()), fg(t.accent(*v))),
                sp(truncate(names, room), fg(t.text)),
            ],
            vec![],
        );
    }
    row(
        buf,
        line(14 + account_lines),
        vec![sp("↑↓ 选择 · ←→ 修改 · Esc 保存并关闭", fg(t.faint))],
        vec![],
    );
    row(
        buf,
        line(15 + account_lines),
        vec![sp(
            "其余在 ~/Library/Application Support/huitai/config.json",
            fg(t.faint),
        )],
        vec![],
    );
}

/// 一键升级没升上的那几家：每家一段（厂商名、命令和输出原文），内容比框高时 ↑↓ 翻。
pub(super) fn cli_failures(buf: &mut Buffer, area: Rect, app: &App, scroll: usize, t: &Theme) {
    let failed = match &app.cli_upgrade {
        CliUpgrade::Failed(f) => f.as_slice(),
        _ => &[],
    };
    // 宽 100（终端窄了跟着收），高按内容来：标题、空行、原文、空行、提示，屏幕放不下就翻。
    let width_of = |w: u16| usize::from(w.min(100).saturating_sub(6));
    let mut lines: Vec<Line<'static>> = Vec::new();
    for r in failed {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::from(sp(r.vendor.name(), bold(t.accent(r.vendor)))));
        for l in wrap(&r.text, width_of(area.width.saturating_sub(2))) {
            lines.push(Line::from(sp(l, fg(t.text))));
        }
    }
    let want = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(6);
    let boxr = centered(area, 100, want);
    fill(buf, boxr, t.bar);
    let inner = Rect {
        x: boxr.x + 3,
        y: boxr.y + 1,
        width: boxr.width.saturating_sub(6),
        height: boxr.height.saturating_sub(2),
    };
    if inner.height < 4 {
        return;
    }
    row(
        buf,
        Rect { height: 1, ..inner },
        vec![sp("没升上的", bold(t.title))],
        vec![sp("原文照录，可以复制命令自己跑", fg(t.faint))],
    );
    let body = inner.height - 3;
    let max_scroll = lines.len().saturating_sub(usize::from(body));
    let body_area = Rect {
        y: inner.y + 2,
        height: body,
        ..inner
    };
    put_lines(
        buf,
        body_area,
        lines
            .into_iter()
            .skip(scroll.min(max_scroll))
            .take(usize::from(body))
            .collect(),
    );
    let hint = if max_scroll > 0 {
        "↑↓ 翻 · U 再试一次 · Esc 关闭"
    } else {
        "U 再试一次 · Esc 关闭"
    };
    row(
        buf,
        Rect {
            y: inner.bottom() - 1,
            height: 1,
            ..inner
        },
        vec![sp(hint, fg(t.faint))],
        vec![],
    );
}
