//! The sidebar: favorites, then Locations (volumes), each a place row with its context menu and drags.

use super::*;

pub(super) fn sidebar(s: &State, l: Look, under_titlebar: bool) -> El {
    // A source list: each place is an option, the folder on screen the selected one. Favorites
    // first (the user's, in their order), then Locations (mounted volumes). It scrolls when
    // they don't all fit; folders dropped on it become favorites.
    let mut outer = col()
        .key("sidebar-pane")
        .w(SIDEBAR_W)
        .h_full()
        .fill(l.sidebar)
        .on_drop_files(|s: &mut State, files, tx| {
            tx.instant();
            s.side.drop_hover = false;
            s.drop_on_sidebar(files);
        })
        .on_drop_hover(|s: &mut State, over, tx| {
            tx.instant();
            s.side.drop_hover = over;
        });
    if s.side.drop_hover {
        outer = outer.border(2.0, l.accent);
    }
    outer = outer.child(el().key("sidebar-drag").w_full().h(if under_titlebar { BAR_H } else { 10.0 }).window_drag());
    let section = |name: &'static str| text(name).font(SMALL).color(l.t.text_secondary).px(8.0).pt(6.0).pb(4.0).a11y_hidden();
    let mut c = col()
        .key("sidebar")
        .role("listbox")
        .label("Places")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .pb(10.0)
        .px(8.0)
        .gap(1.0)
        .scroll(ScrollSpec { stick_bottom: false });
    c = c.child(section("Favorites"));
    for i in 0..s.places.len() {
        c = c.child(place_row(s, i, l));
    }
    if let Some(path) = &s.trash_place {
        c = c.child(trash_row(s, path, l));
    }
    if !s.volumes.is_empty() {
        c = c.child(section("Locations"));
        for i in s.places.len()..s.place_count() {
            c = c.child(place_row(s, i, l));
        }
    }
    outer.child(c)
}

/// Sidebar item `i` (favorites first, then volumes): a click goes there, a right-click opens
/// its menu, a favorite drags up or down to reorder, a volume that can be ejected has ⏏.
pub(super) fn place_row(s: &State, i: usize, l: Look) -> El {
    let Some((name, path)) = s.place_at(i) else { return el() };
    let fav = i < s.places.len();
    let here = path == s.cur().cwd;
    let missing = fav && s.side.is_missing(&path);
    let color = if missing { l.t.text_secondary.alpha(0.5) } else { l.t.text };
    let mut item = row()
        .key(format!("place-{i}"))
        .role("option")
        .focus_selected(here)
        .h(26.0)
        .px(8.0)
        .gap(6.0)
        .center_y()
        .radius(5.0)
        .cursor(Cursor::Pointer)
        .on_press_tx(move |s: &mut State, tx| {
            tx.instant();
            s.go_place(i);
        })
        .on_secondary_press(move |s: &mut State, press, tx| {
            tx.instant();
            s.sidebar_menu(i, press, tx);
        })
        .child(text(name).font(BODY).color(color).truncate().grow(1.0).min_w(0.0));
    if missing {
        item = item.help(format!("{} isn't there any more", fs::pretty(&path)));
    }
    if fav {
        item = item.on_drag(move |s: &mut State, d: &mut Drag, tx| s.drag_favorite(i, d, tx));
    }
    let hint = Cmd::ALL.get(Cmd::ALL.iter().position(|&c| c == Cmd::Place1).unwrap_or(0) + i).filter(|c| c.place() == Some(i));
    if let Some(&cmd) = hint {
        let label = s.keymap.label(cmd);
        if !label.is_empty() {
            // Read as the place's shortcut, not as part of its name.
            item = item.child(text(label.clone()).font(SMALL).color(l.t.text_secondary.alpha(0.6)).a11y_hidden()).shortcut_hint(label);
        }
    }
    if let Some(v) = i.checked_sub(s.places.len()).filter(|&v| s.volumes.get(v).is_some_and(|v| v.eject)) {
        let (name, path) = (s.volumes[v].name.clone(), s.volumes[v].path.clone());
        item = item.child(
            text("⏏")
                .key(format!("eject-{v}"))
                .role("button")
                .label(format!("Eject {name}"))
                .font(BODY)
                .color(l.t.text_secondary)
                .w(18.0)
                .align(Align::Center)
                .radius(4.0)
                .hover(l.t.hover)
                .help(format!("Eject “{name}”"))
                .on_press_tx(move |s: &mut State, tx| {
                    tx.instant();
                    s.eject_at(&path);
                }),
        );
    }
    match s.side.drag {
        // Where the dragged favorite would land, and the favorite itself, dimmed.
        Some((from, _)) if fav && from == i => item.fill(l.t.control_off).opacity(0.6),
        Some((_, to)) if fav && to == i => item.fill(l.accent.alpha(0.22)),
        _ if here => item.fill(l.t.control_off),
        _ => item.hover(l.t.hover),
    }
}

/// The Trash (Linux's home trash, macOS's `~/.Trash`; none on Windows): below the favorites,
/// not one of them (no ⌘ number, no reordering). A click opens it; its menu also empties it.
pub(super) fn trash_row(s: &State, path: &std::path::Path, l: Look) -> El {
    let here = path == s.cur().cwd;
    let item = row()
        .key("place-trash")
        .role("option")
        .focus_selected(here)
        .h(26.0)
        .px(8.0)
        .gap(6.0)
        .center_y()
        .radius(5.0)
        .cursor(Cursor::Pointer)
        .on_press_tx(|s: &mut State, tx| {
            tx.instant();
            if s.unavailable(Cmd::GoTrash).is_none() {
                s.run(Cmd::GoTrash, tx);
            }
        })
        .on_secondary_press(|s: &mut State, press, tx| {
            tx.instant();
            s.trash_menu(press, tx);
        })
        .child(text(crate::trash::BIN).font(BODY).color(l.t.text).truncate().grow(1.0).min_w(0.0));
    if here { item.fill(l.t.control_off) } else { item.hover(l.t.hover) }
}
