//! The file list: column headers, the virtualized rows (a folder's entries or search results) and their drags.

use super::*;

/// The search bar's height.
pub(super) const SEARCH_H: f32 = 32.0;
/// How much of the Name column's room a search's Folder column gets (names matter more).
pub(super) const FOLDER_GROW: f32 = 0.6;

pub(super) fn header(f: &Folder, c: Ctx) -> El {
    let (id, l) = (c.id, c.l);
    // Search results are in the order found until a header is clicked.
    let unsorted = f.search.as_ref().is_some_and(|s| !s.sorted);
    let col_head = |label: &'static str, key: SortKey, w: Option<f32>, align: Align| {
        let on = f.sort.key == key && !unsorted;
        let arrow = if !on {
            ""
        } else if f.sort.desc {
            " ↓"
        } else {
            " ↑"
        };
        let mut t = text(format!("{label}{arrow}"))
            .key(format!("sort-{label}"))
            .role("columnheader")
            .label(label)
            .sorted(on.then_some(!f.sort.desc))
            .font(SMALL)
            .color(if on { l.t.text } else { l.t.text_secondary })
            .align(align)
            .cursor(Cursor::Pointer)
            .on_press_tx(move |s: &mut State, tx| {
                if in_pane(s, id, tx) {
                    s.set_sort(key);
                    s.reveal(tx);
                }
            });
        t = match w {
            Some(w) => t.w(w),
            None => t.grow(1.0).min_w(0.0),
        };
        t
    };
    let mut h =
        row().key("header").w_full().h(24.0).px(12.0).gap(12.0).center_y().child(col_head("Name", SortKey::Name, None, Align::Start));
    if f.searching() {
        let head = text("Folder").key("head-folder").role("columnheader").font(SMALL).color(l.t.text_secondary).min_w(0.0);
        h = h.child(match c.folder_w() {
            Some(w) => head.w(w),
            None => head.grow(FOLDER_GROW),
        });
    } else if c.cols.kind {
        h = h.child(col_head("Kind", SortKey::Kind, Some(KIND_W), Align::Start));
    }
    h = h.child(col_head("Size", SortKey::Size, Some(SIZE_W), Align::End));
    if c.cols.date {
        h = h.child(col_head("Modified", SortKey::Modified, Some(DATE_W), Align::Start));
    }
    h
}

/// The active pane's cursor row, as a key path any list can name for assistive technology
/// (`El::active_descendant`): the row a screen reader reads while either list keeps focus, so
/// it follows the keys (Tab switches panes without moving Rune's focus).
pub(super) fn active_row(s: &State) -> Option<String> {
    let f = s.cur();
    f.selected().map(|e| format!("{}/{}", f.rows_key(), e.key))
}

pub(super) fn list(f: &Folder, c: Ctx, active: Option<String>) -> El {
    let id = c.id;
    // The list itself is a drop target for its folder, and right-clicking empty space opens
    // the menu with Paste and New Folder. For assistive technology it's a list box of the
    // folder's items, the selected ones selected; it can hold keyboard focus (a click or Tab
    // puts it there), and then hands every key to the window's own handler, so keys work the
    // same focused or not. Its selection shows focus, as in AppKit's tables: no ring.
    let mut base = col()
        .key(f.list_key())
        .role("listbox")
        .label(format!("Files in {}", tab_label(&f.cwd)))
        .focusable(true)
        .no_focus_ring()
        .on_key(|s: &mut State, ev: &rune::KeyEvent, tx: &mut rune::Tx| crate::timing::key(s, ev, tx))
        .grow(1.0)
        .min_h(0.0)
        .w_full()
        .on_drop_files(move |s: &mut State, files, tx| {
            tx.instant();
            if let Some(p) = s.pane(id) {
                // Results come from many folders: a drop needs a folder row to land on.
                if p.f.searching() {
                    s.note = Some("Drop onto a folder in the results, or leave the search (Esc) to drop here".into());
                    return;
                }
                let here = p.f.cwd.clone();
                s.drop_files(files, here, tx);
            }
        })
        .on_secondary_press(move |s: &mut State, press, tx| {
            if in_pane(s, id, tx) {
                s.context_menu(None, press, tx);
            }
        });
    if let Some(row) = active {
        base = base.active_descendant(row);
    }
    if f.visible.is_empty() {
        // A tab whose fresh read hasn't landed yet: its path, and an empty list until it does.
        let msg = if f.is_loading() {
            ""
        } else if let Some(s) = &f.search {
            if s.query.is_empty() || s.bad.is_some() || s.error.is_some() {
                ""
            } else if s.walking() {
                "Searching…"
            } else {
                "No matches"
            }
        } else if f.entries.is_empty() {
            "Empty folder"
        } else {
            "No matches"
        };
        return base.center().child(text(msg).font(BODY).color(c.l.t.text_secondary));
    }
    let scroll = f.scroll.clone();
    base.scroll(ScrollSpec { stick_bottom: false }).on_scroll_offset(move |at: ScrollPos| scroll.set(at.offset[1])).child(
        col()
            .key(f.rows_key())
            .w_full()
            .growth(GrowthSpec { spring: rune::GROWTH, edge: rune::edge::NO_MASK })
            .virtual_rows(
                f.visible.len(),
                ROW_H,
                move |s: &State, i| match s.pane(id) {
                    Some(p) => Cow::Borrowed(p.f.entries[p.f.visible[i] as usize].key.as_str()),
                    None => Cow::Borrowed(""),
                },
                move |s: &State, i| match s.pane(id) {
                    Some(p) => file_row(s, &p.f, i, c),
                    None => el(),
                },
            )
            .virtual_rows_version(f.version),
    )
}

pub(super) fn file_row(s: &State, f: &Folder, i: usize, c: Ctx) -> El {
    let (id, l, cols) = (c.id, c.l, c.cols);
    let e = &f.entries[f.visible[i] as usize];
    let selected = f.is_selected(i);
    // With several rows selected, the cursor row that isn't selected gets a soft highlight.
    let cursor = !selected && i == f.sel && !f.marked.is_empty();
    let drop_here = f.drop_target.as_ref() == Some(&e.os);
    // The other pane's selection is grey, as an unfocused list's.
    let (fg, dim) = if selected && c.focused { (l.sel_text, l.sel_text.alpha(0.8)) } else { (l.t.text, l.t.text_secondary) };
    let glyph = if e.is_dir { "▸" } else { " " };
    let name_color = if e.hidden && !selected { fg.alpha(0.55) } else { fg };
    let version = f.version;
    let renaming = s.rename.as_ref().filter(|r| r.os == e.os && c.focused);
    let name_el = match renaming {
        Some(r) => text_input(&r.edit, BODY)
            .key("rename")
            .single_line()
            .color(l.t.text)
            .caret(l.accent, l.accent.alpha(0.35))
            .on_edit(|s: &mut State| s.rename_edit())
            .on_submit(|s: &mut State, tx| {
                tx.instant();
                s.commit_rename(tx);
            })
            .grow(1.0)
            .min_w(0.0)
            .px(4.0)
            .radius(3.0)
            .fill(l.t.bg)
            .border(1.0, l.accent),
        None => {
            let mut name = e.pending.clone().unwrap_or_else(|| e.name.clone());
            if e.is_link {
                name.push_str("  ↗"); // In the bundled Inter, so it draws the same everywhere.
            }
            text(name).font(BODY).color(name_color).truncate_middle().grow(1.0).min_w(0.0)
        }
    };
    // For assistive technology an option of the list box, named from its cells (name, kind,
    // size, modified: what's on screen, at no cost until a reader asks), its place in the
    // folder ("3 of 120") from the virtual list, and selected when it is.
    let mut r = row()
        .role("option")
        .focus_selected(selected)
        .w_full()
        .h(ROW_H)
        .px(12.0)
        .gap(12.0)
        .center_y()
        .radius(4.0)
        .on_press_tx(move |s: &mut State, tx| {
            if !in_pane(s, id, tx) || s.cur().version != version {
                return;
            }
            let f = s.cur();
            if s.rename.as_ref().is_some_and(|r| f.visible.get(i).is_none_or(|&e| f.entries[e as usize].os != r.os)) {
                s.commit_rename(tx);
                return;
            }
            let m = tx.mods();
            // The results take the keys from the search field.
            let ix = s.active;
            s.focus_results(ix, tx);
            let f = s.cur_mut();
            if m.shift {
                f.extend_to(i);
            } else if m.cmd {
                f.toggle_mark(i);
            } else if !(f.is_selected(i) && f.marked.len() > 1) {
                // A plain click inside a multi-selection keeps it, so it can be dragged.
                f.select(i);
            } else {
                f.sel = i;
            }
            s.sync_preview();
        })
        .on_double_press_tx(move |s: &mut State, tx| {
            if in_pane(s, id, tx) && s.cur().version == version && s.rename.is_none() {
                s.cur_mut().select(i);
                s.open_selected();
            }
        })
        .on_secondary_press(move |s: &mut State, press, tx| {
            if in_pane(s, id, tx) && s.cur().version == version {
                s.context_menu(Some(i), press, tx);
            }
        })
        .child(
            row()
                .grow(1.0)
                .min_w(0.0)
                .gap(6.0)
                .center_y()
                .child(folder_mark(
                    text(glyph).font(BODY).color(if selected && c.focused { fg } else { l.accent }).w(10.0),
                    e.is_dir && !cols.kind,
                ))
                .child(name_el),
        );
    if renaming.is_none() {
        // Arbor's own drag inside the window: onto the other pane (split) or onto the sidebar
        // (which adds the folders). On macOS, unsplit, the row drags out to other apps instead
        // and this never runs (a system drag can't land in its own window: RUNE-GAPS.md).
        let split = c.split;
        let inside = move |s: &mut State, d: &mut Drag, tx: &mut rune::Tx| {
            tx.instant();
            let far = d.dx.hypot(d.dy) > DRAG_START;
            let at = d.window_point();
            let side = far && at[0] < SIDEBAR_W;
            let over = split.and_then(|p| p.at(at)).filter(|&p| p != id && far);
            if d.phase == Phase::Up {
                s.pane_drop = None;
                s.side.drop_hover = false;
                if d.cancelled() {
                    return;
                }
                if side {
                    s.drop_rows_on_sidebar(id, i, version);
                } else if let Some(to) = over {
                    s.drop_on_pane(id, i, version, to, tx);
                } else if far && split.is_none() {
                    // Unsplit, off macOS (where the row's drag is the system's): let go on a
                    // folder row or nowhere, as macOS's drag nothing took, it says how.
                    s.note = Some(crate::actions::DRAG_NOT_TAKEN.into());
                }
            } else if s.pane_drop != over || s.side.drop_hover != side {
                s.pane_drop = over;
                s.side.drop_hover = side;
            } else {
                tx.live_only();
            }
        };
        r = match c.split {
            // Out to other apps (macOS: Rune starts no system drag elsewhere, so there the
            // drag stays in the window).
            None => r
                .drag_out(move |s: &State| {
                    if s.pane(id)?.f.version != version {
                        return None;
                    }
                    // Never from a release: AppKit's session would wait for one forever.
                    if !crate::actions::os_drag_can_start() {
                        s.drag_refused.set(true);
                        return None;
                    }
                    s.drag_item(id, i)
                })
                .on_drag_end(|s: &mut State, outcome, tx| {
                    tx.instant();
                    s.drag_out_ended(outcome);
                })
                .on_drag(inside),
            // Split: within the window, onto the other pane. A system drag can't land in the
            // window it came from (RUNE-GAPS.md), so this one is Arbor's own.
            Some(_) => r.on_drag(inside),
        };
    }
    if e.is_dir {
        let target = f.cwd.join(&e.os);
        let os = e.os.clone();
        r = r
            .on_drop_files(move |s: &mut State, files, tx| {
                tx.instant();
                s.drop_files(files, target.clone(), tx);
            })
            .on_drop_hover(move |s: &mut State, over, tx| {
                tx.instant();
                if let Some(ix) = s.pane_ix(id) {
                    s.panes[ix].f.drop_target = over.then(|| os.clone());
                }
            });
    }
    if f.searching() {
        // Where it is: its folder below the search's root (the root itself shows as "—").
        let at = std::path::Path::new(&e.os).parent().filter(|p| !p.as_os_str().is_empty());
        let at = at.map_or_else(|| "—".to_string(), |p| p.display().to_string());
        // Cut to whole trailing folders that fit here first: Rune's own head truncation shapes
        // the whole string again for each width it tries (RUNE-GAPS.md), so it's only the
        // fallback.
        let folder = match c.folder_w() {
            Some(w) => text(fit_path(&at, w).0.into_owned()).w(w),
            None => text(at).grow(FOLDER_GROW),
        };
        r = r.child(folder.font(BODY).color(dim).truncate_head().min_w(0.0));
    } else if cols.kind {
        r = r.child(text(fs::kind(e).to_string()).font(NUM).color(dim).truncate().w(KIND_W));
    }
    let size = text(if e.is_dir { "—".to_string() } else { fs::fmt_size(e.size) }).font(NUM).color(dim).align(Align::End).w(SIZE_W);
    // A folder's "—" says nothing to a reader.
    r = r.child(if e.is_dir { size.a11y_hidden() } else { size });
    if cols.date {
        r = r.child(text(fs::fmt_time(e.mtime_ms, f.now)).font(NUM).color(dim).w(DATE_W));
    }
    r = if drop_here {
        r.fill(l.accent.alpha(0.25)).border(1.5, l.accent)
    } else if selected && c.focused {
        r.fill(l.accent)
    } else if selected {
        r.fill(l.t.control_off)
    } else if cursor {
        r.fill(l.accent.alpha(0.18)).hover(l.t.hover)
    } else if i % 2 == 1 {
        r.fill(l.stripe).hover(l.t.hover)
    } else {
        r.hover(l.t.hover)
    };
    r
}

/// The ▸ before a folder's name: read as "Folder" only while the Kind column (which says so)
/// is hidden; otherwise left out of what a reader hears.
pub(super) fn folder_mark(glyph: El, say: bool) -> El {
    if say { glyph.label("Folder") } else { glyph.a11y_hidden() }
}
