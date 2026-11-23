//! 「等你回复」汇总（`s`）。

use super::list::{Cells, Table, Tag, item, item_cells, list_head};
use super::*;

/// 「等你回复」汇总（`s`）：三家的对话混在一行行里，每行标题前是那家的标记，后面是项目名。
pub(super) fn board(
    buf: &mut Buffer,
    head: Rect,
    area: Rect,
    app: &App,
    offset: usize,
    t: &Theme,
    accent: Color,
) {
    const LABEL: &str = "等你回复和在跑的 · 各家";
    let line = |i: u16| Rect {
        y: area.y + i,
        height: 1,
        ..area
    };
    let entries = app.board_entries();
    if entries.is_empty() {
        list_head(buf, head, LABEL, &Table::new(&[], 0), t);
        row(
            buf,
            line(1),
            vec![sp("  没有等你回复或在跑的对话", fg(t.muted))],
            vec![],
        );
        row(
            buf,
            line(2),
            vec![sp(
                "  会话台托管的对话停下来要你回复时，会列在这里",
                fg(t.faint),
            )],
            vec![],
        );
        return;
    }
    let selected = app.board_index(&entries).unwrap_or(0);
    let cells: Vec<Cells> = entries.iter().map(|e| item_cells(app, e, t)).collect();
    let table = Table::new(&cells, usize::from(area.width));
    list_head(buf, head, LABEL, &table, t);
    let mut y = 0;
    for (i, (e, c)) in entries.iter().zip(cells).enumerate().skip(offset) {
        if y >= area.height {
            break;
        }
        let tag = Tag::VendorAndProject;
        item(
            buf,
            line(y),
            app,
            e,
            tag,
            i == selected,
            &table,
            c,
            t,
            accent,
        );
        if let Some(d) = app.doing(e) {
            y += 1;
            if y < area.height {
                activity::doing_row(buf, line(y), app, e, &d, i == selected, t, accent);
            }
        }
        y += 1;
    }
}
