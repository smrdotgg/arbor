//! Cards over the window: palette, Jump, Select by Pattern, Open With, the Linux context menu, the confirm dialog and go-to completion.

use super::*;

/// Candidates the completion card lists at most; the rest are counted.
pub(super) const COMPLETE_ROWS: usize = 8;
pub(super) const COMPLETE_ROW_H: f32 = 22.0;

/// Go-to's Tab completion (`complete.rs`): the candidates in a small card over the status bar,
/// the one in the field highlighted. Not modal: typing goes on in the field, and any other key
/// ends it. Lists the eight around the current one.
pub(super) fn completion_view(s: &State, l: Look) -> Option<El> {
    let c = s.completion()?;
    let n = c.cands.len();
    let at = c.at.unwrap_or(0);
    let start = at.saturating_sub(COMPLETE_ROWS / 2).min(n.saturating_sub(COMPLETE_ROWS));
    let mut card = col()
        .key("complete")
        .role("listbox")
        .label("Completions")
        .pin(Some(12.0), None, None, Some(28.0 + 4.0))
        .min_w(220.0)
        .max_w(520.0)
        .py(4.0)
        .px(4.0)
        .fill(l.t.card)
        .border(1.0, l.t.card_border)
        .radius(7.0)
        .shadow(OVERLAY_SHADOW.0, OVERLAY_SHADOW.1, l.t.shadow);
    for (i, name) in c.cands.iter().enumerate().skip(start).take(COMPLETE_ROWS) {
        let on = c.at == Some(i);
        let r = row()
            .key(format!("complete-{i}"))
            .role("option")
            .focus_selected(on)
            .h(COMPLETE_ROW_H)
            .px(8.0)
            .center_y()
            .radius(4.0)
            .child(text(name.clone()).font(MONO).color(if on { l.sel_text } else { l.t.text }).truncate_middle().min_w(0.0));
        card = card.child(if on { r.fill(l.accent) } else { r });
    }
    let more = n - (start + COMPLETE_ROWS).min(n);
    let tab = |spec: &str| crate::commands::parse_chord(spec).map(|c| c.label()).unwrap_or_default();
    let foot = if more > 0 {
        format!("{more} more · {} next, {} back", tab("tab"), tab("shift+tab"))
    } else {
        format!("{} next, {} back", tab("tab"), tab("shift+tab"))
    };
    card = card.child(text(foot).font(SMALL).color(l.t.text_secondary).px(8.0).pt(3.0).pb(1.0));
    Some(card)
}

/// Open With (`openwith.rs`): the apps for the selection, the default first; ↑↓ and Enter, or a
/// click. Modal: Esc or a click outside closes it.
pub(super) fn open_with_view(o: &crate::openwith::OpenWithBox, l: Look) -> El {
    let what = match o.files.as_slice() {
        [one] => format!("Open “{}” with", one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
        many => format!("Open {} files with", many.len()),
    };
    let msg = |m: String| text(m).role("status").font(BODY).color(l.t.text_secondary).px(16.0).py(10.0).wrap();
    let mut list = col().key("open-with-list").role("listbox").label("Apps").w_full().px(6.0).py(4.0);
    match &o.apps {
        None => list = list.child(msg("Looking for apps…".into())),
        Some(Err(e)) => list = list.child(msg(e.clone())),
        Some(Ok(apps)) => {
            list = list.active_descendant(format!("app-{}", o.sel));
            for (i, h) in apps.iter().enumerate() {
                let on = i == o.sel;
                let (fg, dim) = if on { (l.sel_text, l.sel_text.alpha(0.8)) } else { (l.t.text, l.t.text_secondary) };
                let mut r = row()
                    .key(format!("app-{i}"))
                    .role("option")
                    .focus_selected(on)
                    .w_full()
                    .h(PALETTE_ROW_H)
                    .px(10.0)
                    .gap(10.0)
                    .center_y()
                    .radius(5.0)
                    .cursor(Cursor::Pointer)
                    .on_press_tx(move |s: &mut State, tx| {
                        tx.instant();
                        s.open_with_pick(i);
                    })
                    .child(text(h.name.clone()).font(BODY).color(fg).truncate().grow(1.0).min_w(0.0));
                if h.default {
                    r = r.child(text("Default").font(SMALL).color(dim));
                }
                list = list.child(if on { r.fill(l.accent) } else { r.hover(l.t.hover) });
            }
        }
    }
    let panel = overlay_card("open-with", "Open With", 420.0, l, |s: &mut State| s.close_open_with())
        .focusable(true)
        .no_focus_ring()
        .on_key(|s: &mut State, ev: &rune::KeyEvent, tx: &mut rune::Tx| s.open_with_key(ev, tx))
        .child(row().h(40.0).px(16.0).center_y().child(text(what).font(TextStyle::new(13.0, 600.0)).color(l.t.text).truncate()))
        .child(hr(l))
        .child(col().w_full().max_h(PALETTE_ROWS * PALETTE_ROW_H + 8.0).scroll(ScrollSpec { stick_bottom: false }).child(list));
    overlay_layer("open-with-layer", panel)
}

/// Rows the palette shows before it scrolls.
pub(super) const PALETTE_ROWS: f32 = 12.0;
pub(super) const PALETTE_ROW_H: f32 = 28.0;

/// The command palette: over the top of the window, a field and the ranked commands with their
/// keys right-aligned. Modal: a click outside closes it, as Esc does.
pub(super) fn palette_view(s: &State, l: Look) -> El {
    let Some(p) = &s.palette else { return el() };
    let hits = s.palette_hits();
    let sel = s.palette_sel(&hits);
    let field = text_input(&p.edit, TextStyle::new(15.0, 400.0))
        .key("palette-field")
        .label("Command")
        .active_descendant(hits.get(sel).map(|h| palette::row_key(h.cmd)).unwrap_or_default())
        .single_line()
        .placeholder("Type a command", l.t.text_secondary.alpha(0.7))
        .color(l.t.text)
        .caret(l.accent, l.accent.alpha(0.35))
        .on_edit(|s: &mut State| s.palette_edit())
        .on_input_change(|s: &mut State, tx| {
            s.palette_follow(tx);
        })
        .on_input_key(|s: &mut State, ev, tx| s.palette_field_key(ev, tx))
        .grow(1.0)
        .min_w(0.0);
    // Only the rows in view are built, as in the file list; the rest build as they scroll in.
    let count = hits.len();
    let hits = std::rc::Rc::new(hits);
    let keys = hits.clone();
    let rows = col()
        .key("palette-rows")
        .role("listbox")
        .label("Commands")
        .w_full()
        .px(6.0)
        .growth(GrowthSpec { spring: rune::GROWTH, edge: rune::edge::NO_MASK })
        .virtual_rows(
            count,
            PALETTE_ROW_H,
            move |_: &State, i| Cow::Owned(palette::row_key(keys[i].cmd)),
            move |_: &State, i| palette_row(&hits[i], i == sel, l),
        );
    let list = if count == 0 {
        col().key("palette-list").child(text("No matching commands").font(BODY).color(l.t.text_secondary).px(16.0).py(10.0))
    } else {
        let h = (count as f32).min(PALETTE_ROWS) * PALETTE_ROW_H + 8.0;
        col().key("palette-list").w_full().h(h).py(4.0).scroll(ScrollSpec { stick_bottom: false }).child(rows)
    };
    let mut panel = col()
        .key("palette")
        .label("Command Palette")
        .w_full()
        .max_w(560.0)
        .fill(l.t.card)
        .border(1.0, l.t.card_border)
        .radius(10.0)
        .shadow(OVERLAY_SHADOW.0, OVERLAY_SHADOW.1, l.t.shadow)
        .modal(true, |s: &mut State| s.palette = None)
        .child(row().h(44.0).px(14.0).center_y().child(field))
        .child(hr(l))
        .child(list);
    if let Some(why) = s.palette_why() {
        panel =
            panel.child(hr(l)).child(text(why).key("palette-why").role("status").font(SMALL).color(l.t.text_secondary).px(16.0).py(7.0));
    }
    col().key("palette-layer").pin(Some(0.0), Some(0.0), Some(0.0), None).items_center().px(20.0).pt(BAR_H + 6.0).child(panel)
}

/// The layer an overlay box sits in: over the top of the window, centred.
pub(super) fn overlay_layer(key: &'static str, panel: El) -> El {
    col().key(key).pin(Some(0.0), Some(0.0), Some(0.0), None).items_center().px(20.0).pt(BAR_H + 6.0).child(panel)
}

/// An overlay card's shadow: offset and blur. Kept small: the area a shadow covers is drawn
/// again whenever the card's contents change (each key in Batch Rename or the palette), and a
/// blur of 30 points cost 1.7 ms a key there in software rendering (BENCH.md).
pub(super) const OVERLAY_SHADOW: (f32, f32) = (4.0, 10.0);

/// An overlay box's frame: a card, modal (a click outside runs `close`).
pub(super) fn overlay_card(key: &'static str, name: &'static str, max_w: f32, l: Look, close: fn(&mut State)) -> El {
    col()
        .key(key)
        .label(name)
        .w_full()
        .max_w(max_w)
        .fill(l.t.card)
        .border(1.0, l.t.card_border)
        .radius(10.0)
        .shadow(OVERLAY_SHADOW.0, OVERLAY_SHADOW.1, l.t.shadow)
        .modal(true, close)
}

/// Jump to Folder: a field, then the folders ranked for it, the matched letters in bold.
pub(super) fn jump_view(s: &State, l: Look, card_w: f32) -> El {
    let Some(j) = &s.jump else { return el() };
    let field = text_input(&j.edit, TextStyle::new(15.0, 400.0))
        .key("jump-field")
        .label("Folder")
        .active_descendant(if j.hits.is_empty() { String::new() } else { crate::jump::row_key(j.sel()) })
        .single_line()
        .placeholder("Jump to a folder you've been to, or type a path", l.t.text_secondary.alpha(0.7))
        .color(l.t.text)
        .caret(l.accent, l.accent.alpha(0.35))
        .on_edit(|s: &mut State| s.jump_edit())
        .on_input_change(|s: &mut State, tx| s.jump_changed(tx))
        .on_input_key(|s: &mut State, ev, tx| s.jump_field_key(ev, tx))
        .grow(1.0)
        .min_w(0.0);
    let count = j.hits.len();
    let sel = j.sel();
    let path_mode = j.path_mode();
    let msg =
        |m: &str| col().key("jump-list").child(text(m.to_string()).role("status").font(BODY).color(l.t.text_secondary).px(16.0).py(10.0));
    let list = if count == 0 && j.loading {
        msg("Loading history…")
    } else if count == 0 && s.shared.history.borrow().agg.as_ref().is_none_or(|a| a.is_empty()) {
        msg("No folders yet: the folders you open show up here. Type a path to go there.")
    } else if count == 0 {
        msg("No folders match")
    } else {
        // Only the rows in view are built.
        let rows = col()
            .key("jump-rows")
            .role("listbox")
            .label("Folders")
            .w_full()
            .px(6.0)
            .growth(GrowthSpec { spring: rune::GROWTH, edge: rune::edge::NO_MASK })
            .virtual_rows(
                count,
                PALETTE_ROW_H,
                |_: &State, i| Cow::Owned(crate::jump::row_key(i)),
                move |s: &State, i| match s.jump.as_ref().and_then(|j| Some((j.hits.get(i)?, &j.words))) {
                    Some((h, words)) => jump_row(h, words, i, i == sel, path_mode && i == 0, card_w, l),
                    None => el(),
                },
            )
            // Keys are row numbers, the same whatever the rows hold.
            .virtual_rows_version(0u8);
        let h = (count as f32).min(PALETTE_ROWS) * PALETTE_ROW_H + 8.0;
        col().key("jump-list").w_full().h(h).py(4.0).scroll(ScrollSpec { stick_bottom: false }).child(rows)
    };
    let panel = overlay_card("jump", "Jump to Folder", 640.0, l, |s: &mut State| s.jump = None)
        .child(row().h(44.0).px(14.0).center_y().child(field))
        .child(hr(l))
        .child(list);
    overlay_layer("jump-layer", panel)
}

/// One Jump row: the folder as `~/…`, matched letters bold (the start is cut when it's long:
/// the folder's own name matters most). In go-to mode, the first row is the path typed.
/// `card_w`: the Jump card's width, for cutting the path to whole folders that fit.
pub(super) fn jump_row(h: &crate::jump::Hit, words: &[Vec<char>], i: usize, on: bool, typed: bool, card_w: f32, l: Look) -> El {
    let (fg, dim, mark) = if on { (l.sel_text, l.sel_text.alpha(0.75), l.sel_text) } else { (l.t.text, l.t.text_secondary, l.accent) };
    // The text's room: the card less its border, the list's and the row's padding, and "Go to".
    let room = card_w - 2.0 - 12.0 - 20.0 - if typed { advance("Go to", SMALL) + 10.0 } else { 0.0 };
    // Cut here to whole trailing folders that fit: Rune's own head truncation shapes the whole
    // path again for each width it tries (RUNE-GAPS.md), so it's only the fallback.
    let (shown, cut) = fit_path(&h.shown, room);
    let spans: Vec<rune::text::Span> =
        shift_marks(&h.marks(words), cut).into_iter().map(|(a, b)| rune::text::Span::new(a, b).bold().color(mark)).collect();
    // Read whole (the path as the history has it), not as cut to fit.
    let mut r = row()
        .role("option")
        .focus_selected(on)
        .label(if typed { format!("Go to {}", h.shown) } else { h.shown.to_string() })
        .w_full()
        .h(PALETTE_ROW_H)
        .px(10.0)
        .gap(10.0)
        .center_y()
        .radius(5.0)
        .cursor(Cursor::Pointer)
        .on_press_tx(move |s: &mut State, tx| s.jump_pick(i, tx));
    if typed {
        r = r.child(text("Go to").font(SMALL).color(dim));
    }
    r = r.child(text(shown.into_owned()).spans(spans).font(BODY).color(fg).truncate_head().grow(1.0).min_w(0.0));
    if on { r.fill(l.accent) } else { r.hover(l.t.hover) }
}

pub(super) const MENU_W: f32 = 250.0;
pub(super) const MENU_ROW_H: f32 = 24.0;
pub(super) const MENU_SEP_H: f32 = 9.0;
pub(super) const MENU_PAD: f32 = 5.0;

/// Where a menu `w` × `h` opens for a press at `x`, `y` in a window `size`: its top-left
/// corner at the pointer, flipped left or up where it would cross the window's edge (as native
/// menus do), and kept inside the window.
pub(crate) fn menu_origin(x: f32, y: f32, w: f32, h: f32, size: Option<[f32; 2]>) -> (f32, f32) {
    let [ww, wh] = size.unwrap_or([f32::MAX, f32::MAX]);
    let place = |at: f32, len: f32, room: f32| {
        let at = if at + len > room && at - len >= 0.0 { at - len } else { at };
        at.min(room - len).max(0.0).round()
    };
    (place(x, w, ww), place(y, h, wh))
}

/// The in-window context menu (`menu.rs`): a small card at the pointer with the native menus'
/// items, each with its keys; the highlighted one in the accent colour, what can't run dimmed.
/// Modal: a click outside closes it.
pub(super) fn menu_view(m: &crate::menu::Menu, size: Option<[f32; 2]>, l: Look) -> El {
    use crate::menu::Item;
    let h = m.items.iter().map(|it| if matches!(it, Item::Separator) { MENU_SEP_H } else { MENU_ROW_H }).sum::<f32>() + 2.0 * MENU_PAD;
    let (x, y) = menu_origin(m.x, m.y, MENU_W, h, size);
    let mut card = col()
        .key("menu")
        .pin(Some(x), Some(y), None, None)
        .w(MENU_W)
        .py(MENU_PAD)
        .px(MENU_PAD)
        .fill(l.t.card)
        .border(1.0, l.t.card_border)
        .radius(7.0)
        .shadow(4.0, 14.0, l.t.shadow)
        .modal(true, |s: &mut State| s.close_menu())
        .role("menu")
        .label("Actions")
        .active_descendant(m.sel.map(|i| format!("menu-{i}")).unwrap_or_default())
        .focusable(true)
        .on_key(|s: &mut State, ev: &rune::KeyEvent, tx: &mut rune::Tx| s.menu_key(ev, tx))
        .no_focus_ring();
    for (i, it) in m.items.iter().enumerate() {
        card = card.child(match it {
            Item::Separator => el().w_full().h(MENU_SEP_H).py(4.0).a11y_hidden().child(el().w_full().h(1.0).fill(l.t.separator)),
            Item::Act { title, on, keys, checked, .. } => {
                let hi = m.sel == Some(i);
                let (fg, dim) = match (hi, *on) {
                    (true, _) => (l.sel_text, l.sel_text.alpha(0.8)),
                    (false, true) => (l.t.text, l.t.text_secondary),
                    (false, false) => (l.t.text_secondary.alpha(0.55), l.t.text_secondary.alpha(0.45)),
                };
                let mut r = row()
                    .key(format!("menu-{i}"))
                    .role(if checked.is_some() { "menuitemcheckbox" } else { "menuitem" })
                    .label(title.clone())
                    .disabled(!*on)
                    .shortcut_hint(keys.clone())
                    .w_full()
                    .h(MENU_ROW_H)
                    .px(10.0)
                    .gap(16.0)
                    .center_y()
                    .radius(4.0);
                // A toggle's check, in a column of its own so the titles line up.
                if let Some(c) = *checked {
                    r = r.checked(c).child(text(if c { "✓" } else { "" }).font(BODY).color(fg).w(10.0).a11y_hidden());
                }
                let r = r
                    .child(text(title.clone()).font(BODY).color(fg).truncate().grow(1.0).min_w(0.0))
                    .child(text(keys.clone()).font(BODY).color(dim));
                match (hi, *on) {
                    (true, _) => r.fill(l.accent).on_press_tx(move |s: &mut State, tx| s.menu_pick(i, tx)),
                    (false, true) => r.hover(l.t.hover).cursor(Cursor::Pointer).on_press_tx(move |s: &mut State, tx| s.menu_pick(i, tx)),
                    (false, false) => r,
                }
            }
        });
    }
    card
}

/// Arbor's one dialog: before something that can't be undone (`confirm.rs`). A title, what it
/// does ("… This can't be undone."), Cancel and the destructive button; the focused one has a
/// ring, and Cancel has the focus when it opens. Modal: a click outside cancels. With nothing
/// to confirm it has no destructive button: Close, and "Open Privacy Settings" when offered.
pub(super) fn confirm_view(a: &crate::confirm::Ask, l: Look) -> El {
    use crate::confirm::Button;
    let key_of = |b: Button| match b {
        Button::Settings => "confirm-settings",
        Button::Cancel => "confirm-cancel",
        Button::Action => "confirm-action",
    };
    let button = |label: &'static str, which: Button| {
        let focused = a.focus == which;
        let yes = which == Button::Action;
        let key = key_of(which);
        // The label is a child of a row that centres it: a text element given a height draws
        // its line at the top of it, a few points above the middle.
        let b = row()
            .key(key)
            .role("button")
            .px(14.0)
            .h(28.0)
            .center_y()
            .radius(6.0)
            .cursor(Cursor::Pointer)
            .on_press_tx(move |s: &mut State, tx| s.press(which, tx))
            .child(text(label).font(TextStyle::new(13.0, 500.0)).color(if yes { Color::WHITE } else { l.t.text }));
        // Red only for what can't be undone; a system change that can be (the folder default)
        // gets the accent, and its focus ring the text colour so it still shows.
        let permanent = a.cmd.permanent();
        let b = match (yes, permanent) {
            (true, true) => b.fill(red()),
            (true, false) => b.fill(l.accent),
            _ => b.fill(l.t.control_off).hover(l.t.hover),
        };
        let ring = if yes && !permanent { l.t.text } else { l.accent };
        if focused { b.border(2.0, ring) } else { b }
    };
    // A focusable card that owns its keys: Rune focuses it when the modal opens, and its keys
    // come before Rune's focus navigation, so Tab, ← → and Enter are the dialog's
    // (`State::ask_key`), and the buttons aren't Tab stops of their own. For assistive
    // technology it's an alert dialog named by its title and described by what it says, and
    // the button with the dialog's focus is its active descendant, so a reader says which
    // one Enter presses.
    let panel = overlay_card("confirm", "", 420.0, l, |s: &mut State| s.asking = None)
        .role("alertdialog")
        .label(a.title.clone())
        .description(a.body.clone())
        .active_descendant(key_of(a.focus))
        .focusable(true)
        .on_key(|s: &mut State, ev: &rune::KeyEvent, tx: &mut rune::Tx| s.ask_key(ev, tx))
        .no_focus_ring()
        .px(20.0)
        .py(18.0)
        .gap(10.0)
        .child(text(a.title.clone()).font(TextStyle::new(15.0, 600.0)).color(l.t.text))
        .child(text(a.body.clone()).font(BODY).color(l.t.text_secondary).wrap().w_full())
        .child(
            row()
                .key("confirm-buttons")
                .w_full()
                .pt(6.0)
                .gap(8.0)
                .children(a.settings.then(|| button("Open Privacy Settings", Button::Settings)))
                .child(el().grow(1.0))
                .child(button(a.cancel_label(), Button::Cancel))
                .children(a.action.map(|label| button(label, Button::Action))),
        );
    overlay_layer("confirm-layer", panel)
}

/// Select by Pattern: a field, the live count, and what Enter, ⇧Enter and ⌥Enter do.
pub(super) fn pick_view(s: &State, l: Look) -> El {
    let Some(p) = &s.pick else { return el() };
    let field = text_input(&p.edit, TextStyle::new(15.0, 400.0))
        .key("pick-field")
        .label("Pattern")
        .single_line()
        .placeholder("Select by pattern: *.jpg, IMG_*, *.{png,jpg}", l.t.text_secondary.alpha(0.7))
        .color(l.t.text)
        .caret(l.accent, l.accent.alpha(0.35))
        .on_edit(|s: &mut State| s.pick_edit())
        .on_input_change(|s: &mut State, tx| {
            tx.instant();
            s.pick_refresh();
        })
        .on_input_key(|s: &mut State, ev, tx| s.pick_field_key(ev, tx))
        .grow(1.0)
        .min_w(0.0);
    let (count, color) = match &p.count {
        Err(e) => (e.clone(), l.t.text_secondary),
        Ok((_, rows)) if p.edit.text.trim().is_empty() => (format!("{rows} items here"), l.t.text_secondary),
        Ok((0, rows)) => (format!("None of {rows} match"), l.t.text_secondary),
        Ok((n, rows)) => (format!("{n} of {rows} match"), l.accent),
    };
    let key = |spec: &str| crate::commands::parse_chord(spec).map(|c| c.label()).unwrap_or_default();
    let hint = format!("{} Select   {} Add   {} Remove", key("enter"), key("shift+enter"), key("alt+enter"));
    let case = if s.pick_case() == crate::pattern::Case::Sensitive { "Aa" } else { "aA" };
    let foot = row()
        .key("pick-foot")
        .w_full()
        .h(32.0)
        .px(16.0)
        .gap(12.0)
        .center_y()
        .child(text(count).key("pick-count").role("status").font(NUM).color(color).truncate().grow(1.0).min_w(0.0))
        .child(text(hint).font(SMALL).color(l.t.text_secondary))
        .child(
            text(case)
                .label(if case == "Aa" { "Capitals count" } else { "Capitals don't count" })
                .font(SMALL)
                .color(l.t.text_secondary.alpha(0.7))
                .help(if case == "Aa" { "Capitals count (select.case_sensitive)" } else { "Capitals don't count (select.case_sensitive)" }),
        );
    let panel = overlay_card("pick", "Select by Pattern", 520.0, l, |s: &mut State| s.pick = None)
        .child(row().h(44.0).px(14.0).center_y().child(field))
        .child(hr(l))
        .child(foot);
    overlay_layer("pick-layer", panel)
}

/// One palette row: the title, and its keys right-aligned and dimmer. Commands that can't run
/// now are dimmed.
pub(super) fn palette_row(h: &palette::Hit, on: bool, l: Look) -> El {
    let (fg, dim) = match (on, h.enabled) {
        (true, true) => (l.sel_text, l.sel_text.alpha(0.75)),
        (true, false) => (l.t.text.alpha(0.55), l.t.text_secondary.alpha(0.55)),
        (false, true) => (l.t.text, l.t.text_secondary.alpha(0.8)),
        (false, false) => (l.t.text.alpha(0.4), l.t.text_secondary.alpha(0.4)),
    };
    let cmd = h.cmd;
    let r = row()
        .role("option")
        .focus_selected(on)
        .disabled(!h.enabled)
        .w_full()
        .h(PALETTE_ROW_H)
        .px(10.0)
        .gap(12.0)
        .center_y()
        .radius(5.0)
        .cursor(Cursor::Pointer)
        .on_press_tx(move |s: &mut State, tx| s.palette_pick(cmd, tx))
        .child(text(h.title.to_string()).font(BODY).color(fg).truncate().grow(1.0).min_w(0.0))
        .child(text(h.keys.clone()).font(NUM).color(dim).a11y_hidden())
        .shortcut_hint(h.keys.clone());
    if on && h.enabled {
        r.fill(l.accent)
    } else if on {
        r.fill(l.t.control_off)
    } else {
        r.hover(l.t.hover)
    }
}
