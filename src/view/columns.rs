//! The column view (`columns.rs`): a row of fixed-width columns, the left ones (from the root
//! down to the active folder's parent), the active one (the folder's own list, with every
//! handler the list view has), then the selected folder's contents or the selected file's
//! preview. Only the columns on screen are built, and in each only its visible rows.
//! For assistive technology each column is a list box named after its folder, in a group.

use std::path::PathBuf;

use super::*;
use crate::columns::{Column, DIVIDER, Load};

/// Pane `c`'s columns, in place of the header and list.
pub(super) fn columns(s: &State, f: &Folder, c: Ctx, cx: &mut Cx<State>, active: Option<String>) -> El {
    let (id, l) = (c.id, c.l);
    let Some(cols) = f.cols.as_deref() else {
        // Before the first sync (a frame at most): the active folder alone.
        return list(f, Ctx { narrow: true, ..c }, active);
    };
    let key = format!("columns-{id}");
    if let Some([w, _]) = cx.layout_size(&key) {
        f.cols_w.set(w);
    }
    let w = s.col_w;
    let mut strip = row()
        .key(key)
        .role("group")
        .label(format!("Columns, {} folders", cols.slots() - usize::from(cols.preview)))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .on_wheel(column_wheel(id));
    let shown = cols.shown();
    let active_at = cols.active();
    // Where each column on screen is, for a drag let go over one (geometry).
    let hits = shown
        .clone()
        .filter_map(|k| {
            let (key, uid, path, dir_id) = if k < active_at {
                let c = &cols.left[k];
                (c.key(), c.uid, c.path.clone(), c.dir_id)
            } else if k == active_at {
                (f.list_key(), f.col_uid?, f.cwd.clone(), f.dir_id)
            } else {
                let c = cols.right.as_ref()?;
                (c.key(), c.uid, c.path.clone(), c.dir_id)
            };
            Some(crate::columns::Hit { rect: cx.layout_with(&key, |r| r)?, uid, path, dir_id })
        })
        .collect();
    cols.hits.replace(hits);
    for k in shown.clone() {
        let column = if k < active_at {
            let view = cx.layout_with(&cols.left[k].key(), |r| r).unwrap_or_default();
            col().key(slot_key(cols.left[k].uid)).w(w).h_full().child(other_column(&cols.left[k], c, w, view))
        } else if k == active_at {
            // The element of the column it was (keyed by its number), so a column keeps its
            // element, rows, scroll and any gesture in it when it becomes the active one.
            col().key(slot_key(f.col_uid.unwrap_or(0))).w(w).h_full().child(list(f, Ctx { narrow: true, ..c }, active.clone()))
        } else if let Some(r) = &cols.right {
            let view = cx.layout_with(&r.key(), |r| r).unwrap_or_default();
            col().key(slot_key(r.uid)).w(w).h_full().child(other_column(r, c, w, view))
        } else {
            // The selected file's preview, over the rest of the width.
            let panel = match &s.preview {
                Some(p) if c.focused && s.col_preview => preview_panel(s, p, l, cx, None),
                _ => el(),
            };
            col().key("column-preview").grow(1.0).min_w(w).h_full().child(panel)
        };
        strip = strip.child(column);
        // After every column but the preview.
        if k <= active_at || cols.right.is_some() {
            strip = strip.child(divider(l));
        }
    }
    if shown.start > 0 {
        // Columns scrolled off to the left: a way back for the mouse (the wheel scrolls too).
        let n = shown.start;
        strip = strip.child(
            text(format!("‹ {n}"))
                .key("columns-back")
                .role("button")
                .label(format!("Show {n} more column{} to the left", if n == 1 { "" } else { "s" }))
                .font(SMALL)
                .color(l.t.text_secondary)
                // At the bottom left, over the first column's foot (rows start at the top).
                .pin(Some(4.0), None, None, Some(4.0))
                .px(6.0)
                .py(2.0)
                .radius(4.0)
                .fill(l.t.bg)
                .border(1.0, l.t.separator)
                .hover(l.t.hover)
                .cursor(Cursor::Pointer)
                .raise()
                .on_press_tx(move |s: &mut State, tx| s.columns_back(id, tx)),
        );
    }
    strip
}

/// A column's slot in the strip, by the number of its element: the same whether it's the
/// active column (the folder's list) or another.
fn slot_key(uid: u64) -> String {
    format!("colw-{uid}")
}

/// A sideways wheel over pane `id`'s columns scrolls them by whole columns (a vertical one
/// scrolls the column under the pointer, as usual). On each column's own viewport too: Rune
/// offers a wheel to handlers only up to the nearest scroll view.
pub(super) fn column_wheel(id: u64) -> impl Fn(&mut State, rune::WheelEvent, &mut rune::Tx) -> bool + 'static {
    move |s: &mut State, ev: rune::WheelEvent, tx: &mut rune::Tx| s.columns_wheel(id, &ev, tx)
}

/// The line after a column, in a strip the pointer drags to change every column's width.
fn divider(l: Look) -> El {
    el().w(DIVIDER).h_full().center().cursor(Cursor::ResizeColumn).a11y_hidden().child(el().w(1.0).h_full().fill(l.t.separator)).on_drag(
        |s: &mut State, d: &mut Drag, tx: &mut rune::Tx| {
            tx.instant();
            match d.phase {
                Phase::Down => s.col_resize = Some(s.col_w),
                _ if d.cancelled() => {
                    if let Some(w) = s.col_resize.take() {
                        s.col_w = w;
                    }
                }
                Phase::Up => {
                    s.col_resize = None;
                    s.sync_preview();
                    s.flush_columns(tx);
                }
                _ => {
                    if let Some(w) = s.col_resize {
                        s.col_w = (w + d.dx).clamp(crate::columns::COL_MIN, crate::columns::COL_MAX).round();
                    }
                }
            }
        },
    )
}

/// A column other than the active one: its folder's items, the highlighted one grey (the
/// active column has the accent). A press anywhere in it makes it the active column first; one
/// in its blank space that moves then drags out a selection rectangle there, as in the active
/// column (`view` is its viewport, as last laid out).
fn other_column(column: &Column, c: Ctx, w: f32, view: [f32; 4]) -> El {
    let col_el = col;
    let col = column;
    let (id, l) = (c.id, c.l);
    let path = col.path.clone();
    let here = path.clone();
    let menu_at = path.clone();
    let drop_in = path.clone();
    let base = col_el()
        .key(col.key())
        .role("listbox")
        .label(format!("Files in {}", tab_label(&col.path)))
        .w(w)
        .h_full()
        .on_press_tx(move |s: &mut State, tx| {
            s.activate_column(id, &here, None, tx);
        })
        .on_secondary_press(move |s: &mut State, press, tx| {
            if s.activate_column(id, &menu_at, None, tx) {
                s.context_menu(None, press, tx);
            }
        })
        .on_scroll_offset({
            let at = col.scroll.clone();
            move |p: ScrollPos| at.set(p.offset[1])
        })
        .on_drag({
            let at = path.clone();
            move |s: &mut State, d: &mut Drag, tx: &mut rune::Tx| {
                // The press makes the column active (its element becomes the active list's, so
                // the gesture goes on there), then it's that list's rectangle.
                if d.phase == Phase::Down && !s.activate_column(id, &at, None, tx) {
                    return;
                }
                s.band_drag(id, d, crate::band::Layout::List { row_h: ROW_H }, view, true, tx);
            }
        })
        .on_drop_files(move |s: &mut State, files, tx| {
            tx.instant();
            // Into the folder this column listed, checked on the worker: a folder put at its
            // path since is refused.
            let dest = match column_of(s, id, &drop_in) {
                Some(c) => c.dest(),
                None => crate::ops::Dest { path: drop_in.clone(), id: None },
            };
            s.drop_into(files, dest, Default::default(), tx);
        });
    match &col.load {
        Load::Ready if col.visible.is_empty() => base.center().child(text("Empty folder").font(SMALL).color(l.t.text_secondary)),
        // Read, or read again with the last listing still shown.
        _ if col.listed() => {
            let rows_key = col.rows_key();
            let uid = col.uid;
            base.scroll(ScrollSpec { stick_bottom: false }).on_wheel(column_wheel(id)).child(
                col_el()
                    .key(rows_key)
                    .w_full()
                    .growth(GrowthSpec { spring: rune::GROWTH, edge: rune::edge::NO_MASK })
                    .virtual_rows(
                        col.visible.len(),
                        ROW_H,
                        move |s: &State, i| match column_by(s, id, uid).and_then(|c| c.entry(i)) {
                            Some(e) => Cow::Borrowed(e.key.as_str()),
                            None => Cow::Borrowed(""),
                        },
                        move |s: &State, i| match (s.pane(id), column_by(s, id, uid)) {
                            (Some(p), Some(col)) => column_row(&p.f, col, i, c),
                            _ => el(),
                        },
                    )
                    .virtual_rows_version(col.version),
            )
        }
        Load::Failed(e) => base.center().px(10.0).child(text(e.clone()).font(SMALL).color(l.t.text_secondary).wrap()),
        // On its way: empty, so a fast read doesn't flash anything.
        Load::Ready | Load::Loading | Load::Off => base,
    }
}

/// Pane `id`'s column numbered `uid`.
fn column_by(s: &State, id: u64, uid: u64) -> Option<&Column> {
    s.pane(id)?.f.cols.as_ref()?.by_uid(uid)
}

/// Pane `id`'s column for `path`.
fn column_of<'a>(s: &'a State, id: u64, path: &std::path::Path) -> Option<&'a Column> {
    s.pane(id)?.f.cols.as_ref()?.column(path)
}

/// Row `i` of a column other than the active one.
fn column_row(f: &Folder, col: &Column, i: usize, c: Ctx) -> El {
    let (id, l) = (c.id, c.l);
    let Some(e) = col.entry(i) else { return el() };
    let picked = col.pick.as_ref() == Some(&e.os);
    let target = col.path.join(&e.os);
    let drop_here = f.cols.as_ref().and_then(|c| c.drop_target.as_ref()) == Some(&target);
    let path = col.path.clone();
    let name = e.os.clone();
    let (p1, n1, p2, n2, p3, n3) = (path.clone(), name.clone(), path.clone(), name.clone(), path.clone(), name.clone());
    let glyph = folder_mark(text(if e.is_dir { "▸" } else { " " }).font(BODY).color(l.accent).w(10.0), e.is_dir);
    let mut label = e.name.clone();
    if e.is_link {
        label.push_str("  ↗");
    }
    let mut r = row()
        .key(e.key.clone())
        .role("option")
        .focus_selected(picked)
        .position_in_set(i + 1, col.visible.len())
        .w_full()
        .h(ROW_H)
        .px(10.0)
        .gap(6.0)
        .center_y()
        .radius(4.0)
        .child(glyph)
        .child(
            text(label)
                .font(BODY)
                .color(if e.hidden && !picked { l.t.text.alpha(0.55) } else { l.t.text })
                .truncate_middle()
                .grow(1.0)
                .min_w(0.0),
        )
        .on_press_tx(move |s: &mut State, tx| {
            s.activate_column(id, &p1, Some(&n1), tx);
        })
        .on_double_press_tx(move |s: &mut State, tx| {
            if s.activate_column(id, &p2, Some(&n2), tx) && s.rename.is_none() {
                s.open_selected();
                s.sync_preview();
                s.flush_columns(tx);
            }
        })
        .on_secondary_press(move |s: &mut State, press, tx| {
            if s.activate_column(id, &p3, Some(&n3), tx) {
                let row = s.cur().visible.iter().position(|&k| s.cur().entries[k as usize].os == n3);
                s.context_menu(row, press, tx);
            }
        });
    r = column_drags(r, c, path.clone(), name.clone());
    if e.is_dir {
        // Into the folder this row lists, as its column read it (checked on the worker).
        let (into, hover) = (col.row_dest(e), target);
        r = r
            .on_drop_files(move |s: &mut State, files, tx| {
                tx.instant();
                s.drop_into(files, into.clone(), Default::default(), tx);
            })
            .on_drop_hover(move |s: &mut State, over, tx| {
                tx.instant();
                if let Some(ix) = s.pane_ix(id)
                    && let Some(cols) = s.panes[ix].f.cols.as_mut()
                {
                    cols.drop_target = over.then(|| hover.clone());
                }
            });
    }
    if drop_here {
        r.fill(l.accent.alpha(0.25)).border(1.5, l.accent)
    } else if picked {
        r.fill(l.t.control_off)
    } else {
        r.hover(l.t.hover)
    }
}

/// A row's drags in a column other than the active one: Arbor's own in the window (onto another
/// column, a folder row, the sidebar, the other pane), which first makes the column active
/// with the item selected, then drags as the active column's rows do. Never the system's:
/// it can't land in the window it came from (RUNE-GAPS.md), and the columns are one window.
fn column_drags(r: El, c: Ctx, path: PathBuf, name: std::ffi::OsString) -> El {
    let id = c.id;
    let split = c.split;
    r.on_drag(move |s: &mut State, d: &mut Drag, tx: &mut rune::Tx| {
        if d.phase == Phase::Down && !s.activate_column(id, &path, Some(&name), tx) {
            return;
        }
        // Where the item is now, in the folder that's active.
        let f = s.cur();
        if f.cwd != path {
            return;
        }
        let Some(i) = f.visible.iter().position(|&k| f.entries[k as usize].os == name) else { return };
        let version = f.version;
        inside_drag(s, d, tx, id, i, version, split);
    })
}
