//! The grid views: a virtual grid of fixed cells, an icon (or a thumbnail, once one lands) over
//! each item's name (As Icons), or a larger one beside its name, kind and size (As Tiles). Only the rows on screen (and Rune's mounted window around them) are built;
//! which rows are visible drives the thumbnails (`tiles.rs`). Everything else, presses, drags,
//! drops, the context menu and the inline rename, is the list's, through the same functions.

use super::*;
use crate::grid::{Geom, INSET, PAD};

/// The icon view's body inside the list's scroll viewport `base`.
pub(super) fn cells(f: &Folder, c: Ctx, base: El) -> El {
    let id = c.id;
    let g = grid_geom(f, c);
    // The arrows and revealing the cursor need the columns (geometry, written as drawn).
    f.grid_cols.set(g.cols);
    let n = f.visible.len();
    let cols = g.cols;
    base.child(super::list::follow_band(
        col()
            .key(f.rows_key())
            .w_full()
            .growth(GrowthSpec { spring: rune::GROWTH, edge: rune::edge::NO_MASK })
            .virtual_rows(
                g.rows(n),
                g.cell[1],
                // A row is keyed by its first item; the columns are in the version, so rows are
                // looked at again when a resize moves items between them.
                move |s: &State, r| match s.pane(id) {
                    Some(p) => match p.f.visible.get(r * cols) {
                        Some(&e) => Cow::Borrowed(p.f.entries[e as usize].key.as_str()),
                        None => Cow::Borrowed(""),
                    },
                    None => Cow::Borrowed(""),
                },
                move |s: &State, r| match s.pane(id) {
                    Some(p) => grid_row(s, &p.f, r, g, c),
                    None => el(),
                },
            )
            .virtual_rows_version((f.version, cols))
            // The rows on screen are the only ones thumbnailed; scrolling cancels the rest.
            .on_visible_rows(move |s: &mut State, rows| s.tiles_visible(id, rows)),
        c,
    ))
}

/// The grid's layout in pane `c`: its view and icon size, in the list's width as last laid out.
pub(super) fn grid_geom(f: &Folder, c: Ctx) -> Geom {
    let w = if c.list[2] > 0.0 {
        c.list[2]
    } else if c.w < f32::MAX {
        c.w
    } else {
        800.0
    };
    f.view.geom(c.icons, w).unwrap_or_else(|| Geom::new(c.icons, w))
}

fn grid_row(s: &State, f: &Folder, r: usize, g: Geom, c: Ctx) -> El {
    let n = f.visible.len();
    // Each item sits in its cell less the inset: the row's padding and gaps make the gutters,
    // with no element per cell around it.
    let mut row = row().role("none").w_full().h(g.cell[1]).pl(PAD + INSET).py(INSET).gap(2.0 * INSET);
    for i in r * g.cols..((r + 1) * g.cols).min(n) {
        row = row.child(cell(s, f, i, g, c));
    }
    row
}

/// Item `i`'s cell: its icon or thumbnail, and its name (or the rename field) under it.
fn cell(s: &State, f: &Folder, i: usize, g: Geom, c: Ctx) -> El {
    let (id, l) = (c.id, c.l);
    let e = &f.entries[f.visible[i] as usize];
    let selected = f.is_selected(i);
    let cursor = !selected && i == f.sel && !f.marked.is_empty();
    let drop_here = f.drop_target.as_ref() == Some(&e.os);
    let version = f.version;
    let tiles = f.view == crate::grid::ViewMode::Tiles;
    let size = f.view.icon(c.icons);
    let renaming = s.rename.as_ref().filter(|r| r.os == e.os && c.focused);
    let (w, h) = (g.cell[0] - 2.0 * INSET, g.cell[1] - 2.0 * INSET);
    let picture = match f.tiles.as_ref().and_then(|t| t.shown.get(&e.os)) {
        Some((px, [pw, ph])) => {
            // At most the icon's box, at the device pixels it was decoded for.
            let (pw, ph) = (*pw as f32 / c.scale, *ph as f32 / c.scale);
            let k = (size / pw).min(size / ph).min(1.0);
            el().w(size).h(size).center().child(el().w((pw * k).max(1.0)).h((ph * k).max(1.0)).pixels(px).radius(2.0)).a11y_hidden()
        }
        None => type_icon(e, size, l).a11y_hidden(),
    };
    let (fg, label_fill) = match (selected, c.focused) {
        (true, true) => (l.sel_text, Some(l.accent)),
        (true, false) => (l.t.text, Some(l.t.control_off)),
        _ => (if e.hidden { l.t.text.alpha(0.55) } else { l.t.text }, None),
    };
    let label: El = match renaming {
        Some(r) => text_input(&r.edit, SMALL)
            .key("rename")
            .single_line()
            .color(l.t.text)
            .caret(l.accent, l.accent.alpha(0.35))
            .on_edit(|s: &mut State| s.rename_edit())
            .on_input_key(|s: &mut State, ev, tx| s.rename_field_key(ev, tx))
            .on_submit(|s: &mut State, tx| {
                tx.instant();
                s.commit_rename(tx);
            })
            .w_full()
            .px(3.0)
            .radius(3.0)
            .fill(l.t.bg)
            .border(1.0, l.accent),
        None => {
            let mut name = e.pending.clone().unwrap_or_else(|| e.name.clone());
            if e.is_link {
                name.push_str(" ↗");
            }
            let t = if tiles {
                text(name).font(BODY).color(fg).truncate_middle().px(4.0).py(1.0).radius(4.0).min_w(0.0)
            } else {
                text(name).font(SMALL).color(fg).align(Align::Center).max_lines(2).px(4.0).py(1.0).radius(4.0).min_w(0.0)
            };
            match label_fill {
                Some(fill) => t.fill(fill),
                None => t,
            }
        }
    };
    let item = if tiles {
        // The icon on the left; the name, then its kind, then its size and date (what fits).
        let dim = if selected && c.focused { l.t.text } else { l.t.text_secondary };
        let size_line = if e.is_dir {
            fs::fmt_time(e.mtime_ms, f.now)
        } else {
            format!("{} · {}", fs::fmt_size(e.size), fs::fmt_time(e.mtime_ms, f.now))
        };
        let details = col()
            .grow(1.0)
            .min_w(0.0)
            .gap(1.0)
            .child(row().w_full().child(label.shrink()))
            .child(text(tile_kind(e)).font(SMALL).color(dim).truncate().px(4.0).min_w(0.0))
            .child(text(size_line).font(NUM).color(dim).truncate().px(4.0).min_w(0.0));
        row().key(e.key.clone()).w(w).h(h).gap(8.0).px(6.0).center_y().child(picture).child(details)
    } else {
        col().key(e.key.clone()).w(w).h(h).gap(4.0).items_center().child(picture).child(label)
    };
    let mut item = item
        .role("option")
        .focus_selected(selected)
        .position_in_set(i + 1, f.visible.len())
        .radius(6.0)
        .on_press_tx(move |s: &mut State, tx| press_item(s, tx, id, i, version))
        .on_double_press_tx(move |s: &mut State, tx| open_item(s, tx, id, i, version))
        .on_secondary_press(move |s: &mut State, press, tx| {
            if in_pane(s, id, tx) && s.cur().version == version {
                s.context_menu(Some(i), press, tx);
            }
        });
    if renaming.is_none() {
        item = item_drags(item, c, i, version);
    }
    item = folder_drop(item, f, c, e);
    if drop_here {
        item.fill(l.accent.alpha(0.25)).border(1.5, l.accent)
    } else if selected {
        item.fill(l.t.text.alpha(0.06))
    } else if cursor {
        item.border(1.0, l.accent.alpha(0.5)).hover(l.t.hover)
    } else {
        item.hover(l.t.hover)
    }
}

/// The broad kinds an item gets its own icon for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Folder,
    Image,
    Video,
    Audio,
    Text,
    Code,
    Archive,
    Document,
    App,
    Other,
}

/// An item's broad kind, from its extension (nothing is read).
pub(crate) fn kind(e: &crate::fs::Entry) -> Kind {
    if e.is_dir {
        return if fs::ext(&e.lower) == "app" { Kind::App } else { Kind::Folder };
    }
    match fs::ext(&e.lower) {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "heif" | "avif" | "svg" | "ico" | "icns" | "psd"
        | "dng" | "cr2" | "cr3" | "nef" | "arw" | "raf" | "orf" => Kind::Image,
        "mp4" | "m4v" | "mov" | "avi" | "mkv" | "webm" | "wmv" | "flv" | "mpg" | "mpeg" | "3gp" => Kind::Video,
        "mp3" | "m4a" | "aac" | "wav" | "flac" | "ogg" | "opus" | "aif" | "aiff" | "wma" | "mid" | "midi" => Kind::Audio,
        "txt" | "md" | "markdown" | "log" | "csv" | "tsv" | "ini" | "cfg" | "conf" => Kind::Text,
        "rs" | "c" | "h" | "cc" | "cpp" | "hpp" | "py" | "js" | "mjs" | "ts" | "tsx" | "jsx" | "go" | "java" | "kt" | "swift" | "rb"
        | "php" | "sh" | "zsh" | "bash" | "fish" | "ps1" | "toml" | "yaml" | "yml" | "json" | "xml" | "html" | "htm" | "css" | "scss"
        | "sql" | "lua" | "zig" | "cs" | "m" | "mm" => Kind::Code,
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "dmg" | "iso" | "jar" => Kind::Archive,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp" | "pages" | "numbers" | "key" | "epub" | "rtf" => {
            Kind::Document
        }
        "exe" | "msi" | "deb" | "rpm" | "appimage" | "apk" | "pkg" => Kind::App,
        _ => Kind::Other,
    }
}

/// A tile's kind line: the broad kind in words, then the extension ("Image · PNG").
fn tile_kind(e: &crate::fs::Entry) -> String {
    let word = match kind(e) {
        Kind::Folder => return "Folder".into(),
        Kind::App if e.is_dir => return "Application".into(),
        Kind::Image => "Image",
        Kind::Video => "Video",
        Kind::Audio => "Audio",
        Kind::Text => "Text",
        Kind::Code => "Code",
        Kind::Archive => "Archive",
        Kind::Document => "Document",
        Kind::App => "Application",
        Kind::Other => "File",
    };
    match fs::ext(&e.lower) {
        "" => word.into(),
        x => format!("{word} · {}", x.to_uppercase()),
    }
}

/// Each kind's colour: what tells them apart at a glance.
fn kind_color(k: Kind) -> Color {
    let (l, c, h) = match k {
        Kind::Folder => (0.70, 0.12, 250.0),
        Kind::Image => (0.64, 0.14, 155.0),
        Kind::Video => (0.60, 0.17, 25.0),
        Kind::Audio => (0.60, 0.15, 310.0),
        Kind::Text => (0.62, 0.02, 250.0),
        Kind::Code => (0.56, 0.15, 265.0),
        Kind::Archive => (0.70, 0.12, 75.0),
        Kind::Document => (0.65, 0.15, 45.0),
        Kind::App => (0.50, 0.16, 280.0),
        Kind::Other => (0.72, 0.01, 250.0),
    };
    rune::oklch(l, c, h)
}

/// An item's icon, `size` points square, drawn from plain shapes (as few as tell them apart: a
/// grid builds a few screens of them): a folder is a tab over a body; a file is a page with a
/// band of its kind's colour carrying its extension (from medium size up); an app is a rounded
/// tile with a dot.
pub(super) fn type_icon(e: &crate::fs::Entry, size: f32, l: Look) -> El {
    let k = kind(e);
    let color = kind_color(k);
    let s = size;
    let boxed = el().w(s).h(s);
    match k {
        Kind::Folder => boxed
            .child(el().absolute(s * 0.06, s * 0.14).w(s * 0.4).h(s * 0.16).radius(s * 0.05).fill(color.alpha(0.75)))
            .child(el().absolute(s * 0.06, s * 0.24).w(s * 0.88).h(s * 0.62).radius(s * 0.07).fill(color)),
        Kind::App => {
            let tile = el().absolute(s * 0.12, s * 0.12).w(s * 0.76).h(s * 0.76).radius(s * 0.18).fill(color);
            boxed
                .child(tile)
                .child(el().absolute(s * 0.34, s * 0.34).w(s * 0.32).h(s * 0.32).radius(s * 0.16).fill(Color::WHITE.alpha(0.85)))
        }
        _ => {
            let (pw, ph) = (s * 0.7, s * 0.9);
            let (px, py) = ((s - pw) / 2.0, (s - ph) / 2.0);
            let mut b = boxed.child(el().absolute(px, py).w(pw).h(ph).radius(s * 0.05).fill(l.t.card).border(1.0, l.t.separator));
            let band = el().absolute(px, py + ph * 0.58).w(pw).h(ph * 0.24).fill(color);
            let ext = fs::ext(&e.lower);
            b = if s >= 48.0 && !ext.is_empty() && ext.len() <= 5 {
                let label = text(ext.to_uppercase())
                    .font(TextStyle::new((s * 0.15).round(), 700.0))
                    .color(Color::WHITE)
                    .align(Align::Center)
                    .w(pw)
                    .truncate();
                b.child(band.center().child(label))
            } else {
                b.child(band)
            };
            b
        }
    }
}
