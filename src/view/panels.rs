//! The side panel where the quick preview or Get Info shows.

use super::*;

/// The docked preview's width bounds (one pane).
pub(super) const PREVIEW_MIN_W: f32 = 240.0;
pub(super) const PREVIEW_MAX_W: f32 = 640.0;
/// A text preview's line height and side padding.
pub(super) const LINE_H: f32 = 17.0;
pub(super) const LINE_PAD: f32 = 12.0;

/// The quick preview panel: the item's name and what the listing knows about it at once, then
/// its contents when the worker has them (see `preview.rs`). `width`: docked beside one pane;
/// `None` takes the other pane's place.
/// The panel beside the list: the quick preview, or Get Info.
pub(super) fn side_panel(s: &State, l: Look, cx: &mut Cx<State>, width: Option<f32>) -> El {
    match (&s.preview, &s.info) {
        (Some(p), _) => preview_panel(s, p, l, cx, width),
        (None, Some(i)) => info_panel(s, i, l, width),
        (None, None) => el(),
    }
}

/// Get Info (`info.rs`): the item's name, then what the disk says about it, a row each, and
/// for a folder or several items the total as it streams in. Esc or ⌘I closes it.
pub(super) fn info_panel(s: &State, i: &crate::info::Info, l: Look, width: Option<f32>) -> El {
    use crate::info::fmt_date;
    let mut panel = col().key("info").role("complementary").label("Get Info").h_full().min_w(0.0).fill(l.t.bg);
    panel = match width {
        Some(w) => panel.w(w),
        None => panel.grow(1.0),
    };
    let single = i.paths.len() == 1;
    let meta = i.meta.as_ref().and_then(|m| m.as_ref().ok());
    let kind = match meta {
        Some(m) => m.kind.clone(),
        None if single => "…".into(),
        None => {
            let part = |n: usize, one: &str, many: &str| (n > 0).then(|| format!("{n} {}", if n == 1 { one } else { many }));
            [part(i.files, "file", "files"), part(i.dirs, "folder", "folders")].into_iter().flatten().collect::<Vec<_>>().join(", ")
        }
    };
    let head = col()
        .key("info-head")
        .w_full()
        .px(14.0)
        .py(9.0)
        .gap(3.0)
        .child(text(i.name.clone()).role("heading").level(2).font(TextStyle::new(13.0, 600.0)).color(l.t.text).truncate_middle())
        .child(text(kind.clone()).font(SMALL).color(l.t.text_secondary).truncate());
    let label_w = 84.0;
    let line = |label: &str, value: String| {
        row()
            .w_full()
            .gap(10.0)
            .child(text(label.to_string()).font(SMALL).color(l.t.text_secondary).w(label_w).align(Align::End).pt(1.0))
            .child(text(value).font(BODY).color(l.t.text).wrap().grow(1.0).min_w(0.0).selectable(true))
    };
    let mut body =
        col().key("info-body").w_full().grow(1.0).min_h(0.0).px(10.0).py(12.0).gap(7.0).scroll(ScrollSpec { stick_bottom: false });
    if let Some(Err(e)) = &i.meta {
        body = body.child(text(format!("Couldn't read it: {e}")).font(BODY).color(l.t.text_secondary).wrap());
    }
    body = body.child(line("Kind", kind));
    let at = i.paths.first().and_then(|p| if single { Some(p.as_path()) } else { p.parent() });
    if let Some(p) = at {
        body = body.child(line(if single { "Where" } else { "In" }, p.display().to_string()));
    }
    let total = i.total.as_ref().and_then(|t| t.lock().ok().map(|t| t.clone()));
    match (&total, meta) {
        (Some(t), _) => {
            body = body.child(line("Size", t.line()).key("info-total").role("status"));
            if let Some(c) = t.caveats() {
                body = body.child(line("", c));
            }
        }
        (None, Some(m)) if !m.is_dir => {
            body = body.child(line("Size", format!("{} ({} bytes)", fs::fmt_size(m.size), crate::info::group(m.size))));
        }
        _ if !single => body = body.child(line("Size", fs::fmt_size(i.size))),
        _ => {}
    }
    if let Some(m) = meta {
        body = body
            .child(line("Created", fmt_date(m.created)))
            .child(line("Modified", fmt_date(m.modified)))
            .child(line("Accessed", fmt_date(m.accessed)));
        if let Some(t) = &m.link {
            body = body.child(line("Link to", t.clone()));
        }
        if let Some(mode) = &m.mode {
            body = body.child(line("Permissions", mode.clone()));
        }
        if let Some(o) = &m.owner {
            body = body.child(line("Owner", o.clone()));
        }
        if let Some(a) = &m.attrs {
            body = body.child(line("Attributes", a.clone()));
        }
    }
    let button = |label: &'static str, key: &'static str| {
        text(label)
            .key(key)
            .role("button")
            .font(BODY)
            .color(l.t.text)
            .px(10.0)
            .py(3.0)
            .radius(5.0)
            .fill(l.t.control_off)
            .hover(l.t.hover)
            .cursor(Cursor::Pointer)
    };
    let mut buttons = row().key("info-actions").w_full().px(14.0).py(10.0).gap(8.0);
    buttons = buttons.child(button("Copy Path", "info-copy").on_press_tx(|s: &mut State, tx| {
        tx.instant();
        s.info_copy_path(tx);
    }));
    // Rename is the list's own (undoable), for the one item selected.
    if single && s.cur().selected().is_some() && s.cur().marked.len() <= 1 {
        buttons = buttons.child(button("Rename…", "info-rename").on_press_tx(|s: &mut State, tx| {
            tx.instant();
            if s.unavailable(Cmd::Rename).is_none() {
                s.run(Cmd::Rename, tx);
            }
        }));
    }
    panel.child(head).child(hr(l)).child(body).child(hr(l)).child(buttons)
}

pub(super) fn preview_panel(s: &State, p: &Preview, l: Look, cx: &mut Cx<State>, width: Option<f32>) -> El {
    let mut panel = col().key("preview").role("complementary").label("Preview").h_full().min_w(0.0).fill(l.t.bg);
    panel = match width {
        Some(w) => panel.w(w),
        None => panel.grow(1.0),
    };
    // The next image decodes to fit what's laid out now (device pixels).
    let scale = cx.env().scale.max(1.0);
    let body_size = cx.layout_size("preview-body");
    if let Some([w, h]) = body_size {
        s.preview_fit.set([(w * scale).round().max(1.0) as u32, (h * scale).round().max(1.0) as u32]);
    }
    let Some(t) = &p.target else {
        return panel.center().child(text("Nothing selected").font(BODY).color(l.t.text_secondary));
    };
    let mut meta = vec![if t.is_dir { "Folder".to_string() } else { fs::kind_of(&t.name).to_string() }];
    match &p.body {
        Body::Card(c) if c.items.is_some() => {
            let n = c.items.unwrap_or(0);
            meta.push(format!("{n} item{}", if n == 1 { "" } else { "s" }));
        }
        // An OS thumbnail's size is the thumbnail's, not the file's: the kind says enough.
        Body::Image(img) if img.format != Format::Thumbnail => meta.push(format!("{} × {} {}", img.width, img.height, img.format.name())),
        _ => {}
    }
    if !t.is_dir {
        meta.push(fs::fmt_size(t.size));
    }
    meta.push(fs::fmt_time(t.mtime_ms, s.cur().now));
    let mut title = row().gap(8.0).center_y().child(
        text(t.name.clone())
            .role("heading")
            .level(2)
            .font(TextStyle::new(13.0, 600.0))
            .color(l.t.text)
            .truncate_middle()
            .grow(1.0)
            .min_w(0.0),
    );
    if p.is_loading() {
        title = title.child(text("Loading…").font(SMALL).color(l.t.text_secondary).role("status"));
    }
    let mut head = col()
        .key("preview-head")
        .w_full()
        .px(14.0)
        .py(9.0)
        .gap(3.0)
        .child(title)
        .child(text(meta.join(" · ")).font(SMALL).color(l.t.text_secondary).truncate());
    if let Some(link) = &p.link {
        head = head.child(text(format!("↗ {link}")).font(SMALL).color(l.t.text_secondary).truncate_middle());
    }
    let body = col().key("preview-body").w_full().grow(1.0).min_h(0.0);
    let body = match &p.body {
        Body::Empty | Body::Loading => body,
        Body::Error(why) => body.center().px(20.0).child(text(why.clone()).font(BODY).color(l.t.text_secondary).wrap()),
        Body::Card(c) => {
            let mut card = body.px(14.0).py(12.0).gap(10.0);
            if let Some(note) = &c.note {
                card = card.child(text(note.clone()).font(BODY).color(l.t.text_secondary).wrap());
            }
            card
        }
        Body::Image(img) => {
            // At most its own size, fitted into the body with a margin.
            let [bw, bh] = body_size.unwrap_or([400.0, 400.0]).map(|v| (v - 24.0).max(1.0));
            let (iw, ih) = (img.shown[0] as f32 / scale, img.shown[1] as f32 / scale);
            let own = (img.width as f32 / scale).max(iw);
            let k = (bw / iw).min(bh / ih).min(own / iw);
            body.center().child(
                el().key("preview-image")
                    .role("img")
                    .label(t.name.clone())
                    .w((iw * k).floor().max(1.0))
                    .h((ih * k).floor().max(1.0))
                    .pixels(&p.pixels),
            )
        }
        Body::Text(t) => {
            let mut b = body;
            if let Some(note) = &t.note {
                b = b
                    .child(text(note.clone()).font(SMALL).color(l.t.text_secondary).px(LINE_PAD).py(5.0).w_full().fill(l.stripe))
                    .child(hr(l));
            }
            if t.lines.is_empty() {
                return panel.child(head).child(hr(l)).child(b);
            }
            let char_w = rune::text::measure("0", &[], CODE, None).0;
            let rows = col()
                .key("preview-lines")
                .w_full()
                .min_w(t.cols as f32 * char_w + 2.0 * LINE_PAD)
                .py(6.0)
                .growth(GrowthSpec { spring: rune::GROWTH, edge: rune::edge::NO_MASK })
                .virtual_rows(
                    t.lines.len(),
                    LINE_H,
                    |_: &State, i| Cow::Owned(format!("l{i}")),
                    move |s: &State, i| match s.preview.as_ref().map(|p| &p.body) {
                        Some(Body::Text(t)) => text(t.line(i).to_string()).font(CODE).color(l.t.text).h(LINE_H).px(LINE_PAD),
                        _ => el(),
                    },
                )
                .virtual_rows_version(s.preview_lands);
            // A new file starts at the top. Tab reaches it (it owns its keys, so it's a Tab stop
            // of its own), and then ↑↓, ←→, Page Up/Down, Home and End scroll it; anything else
            // goes on to the window. Rune draws its focus ring.
            let (lands, key) = (s.preview_lands, format!("preview-text-{}", s.preview_lands));
            let scrolled = s.preview_scroll.clone();
            let keys = key.clone();
            b.child(
                col()
                    .key(key)
                    .role("document")
                    .label("Contents")
                    .focusable(true)
                    .on_key(move |_: &mut State, ev: &rune::KeyEvent, tx: &mut rune::Tx| {
                        // Where this file's text is scrolled to (a new one starts at the top).
                        let (of, at) = scrolled.get();
                        preview_scroll_key(&keys, if of == lands { at } else { [0.0; 2] }, ev, tx)
                    })
                    .grow(1.0)
                    .min_h(0.0)
                    .w_full()
                    .scroll_xy()
                    .on_scroll_offset({
                        let at = s.preview_scroll.clone();
                        move |pos: ScrollPos| at.set((lands, pos.offset))
                    })
                    .child(rows),
            )
        }
    };
    panel.child(head).child(hr(l)).child(body)
}

/// The text preview's keys while it has focus: arrows scroll three lines (or columns), Page
/// Up/Down a page, Home and End to the ends. `at` is where it's scrolled to now.
pub(super) fn preview_scroll_key(key: &str, at: [f32; 2], ev: &rune::KeyEvent, tx: &mut rune::Tx) -> bool {
    use rune::Key;
    let m = ev.mods;
    if m.cmd || m.ctrl || m.alt || m.shift {
        return false;
    }
    tx.instant();
    let step = 3.0 * LINE_H;
    let [x, y] = at;
    match ev.key {
        Key::Up => tx.scroll_to_instant(key, x, (y - step).max(0.0)),
        Key::Down => tx.scroll_to_instant(key, x, y + step),
        Key::Left => tx.scroll_to_instant(key, (x - step).max(0.0), y),
        Key::Right => tx.scroll_to_instant(key, x + step, y),
        Key::PageUp => tx.scroll_page(key, -1),
        Key::PageDown => tx.scroll_page(key, 1),
        Key::Home => tx.scroll_to_instant(key, 0.0, 0.0),
        Key::End => tx.scroll_to_instant(key, x, f32::MAX),
        _ => return false,
    }
    true
}
