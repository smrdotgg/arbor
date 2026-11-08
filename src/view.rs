//! The window: sidebar of places, a path bar, sortable column headers, the virtualized file
//! list and a status line that doubles as the filter / go-to field.

use std::borrow::Cow;
use std::path::{Component, PathBuf};

use rune::tok::{self, TextStyle, Theme};
use rune::{Align, Color, Cursor, Cx, El, GrowthSpec, ScrollSpec, col, el, row, text, text_input};

use crate::app::State;
use crate::commands::Cmd;
use crate::fs::{self, SortKey};
use crate::palette;

const ROW_H: f32 = 24.0;
/// Height of the top band (path bar, and the sidebar's traffic-light strip on macOS).
pub const BAR_H: f32 = 38.0;
const SIDEBAR_W: f32 = 168.0;
const KIND_W: f32 = 72.0;
const SIZE_W: f32 = 80.0;
const DATE_W: f32 = 132.0;

const BODY: TextStyle = TextStyle::new(13.0, 400.0);
const SMALL: TextStyle = TextStyle::new(11.5, 500.0);
const NUM: TextStyle = TextStyle::new(12.0, 400.0).features("\"tnum\"");
const CRUMB: TextStyle = TextStyle::new(13.0, 500.0);
const MONO: TextStyle = TextStyle::new(12.0, 400.0).mono();

/// Which optional columns fit the list's width; Name always gets at least ~180 pt.
#[derive(Clone, Copy)]
struct Cols {
    kind: bool,
    date: bool,
}

impl Cols {
    fn for_width(w: f32) -> Cols {
        let base = 24.0 + 180.0 + 12.0 + SIZE_W;
        let date = w >= base + 12.0 + DATE_W;
        Cols { date, kind: date && w >= base + 24.0 + DATE_W + KIND_W }
    }
}

#[derive(Clone, Copy)]
struct Look {
    t: Theme,
    accent: Color,
    sel_text: Color,
    sidebar: Color,
    stripe: Color,
}

fn look(dark: bool) -> Look {
    let t = tok::theme(dark);
    Look {
        t,
        accent: tok::accent(0, dark),
        sel_text: Color::WHITE,
        sidebar: if dark { t.card } else { t.control_track },
        stripe: t.text.alpha(if dark { 0.022 } else { 0.028 }),
    }
}

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    root(s, cx).on_paste_content(|s: &mut State, content, tx| {
        tx.instant();
        s.paste(content);
    })
}

fn root(s: &State, cx: &mut Cx<State>) -> El {
    let l = look(cx.system_dark());
    // Reads the laid-out size only; scrolling never re-runs the view.
    let cols = Cols::for_width(cx.layout_size("main").map_or(f32::MAX, |[w, _]| w));
    let window = row()
        .w_full()
        .h_full()
        .fill(l.t.bg)
        .child(sidebar(s, l, cx.titlebar_inset() > 0.0))
        .child(el().w(1.0).h_full().fill(l.t.separator))
        .child(
            col()
                .key("main")
                .grow(1.0)
                .min_w(0.0)
                .h_full()
                .child(pathbar(s, l))
                .child(hr(l))
                .child(header(s, l, cols))
                .child(hr(l))
                .child(list(s, l, cols))
                .child(hr(l))
                .child(status(s, l)),
        );
    if s.palette.is_some() { window.child(palette_view(s, l)) } else { window }
}

fn hr(l: Look) -> El {
    el().w_full().h(1.0).fill(l.t.separator)
}

fn sidebar(s: &State, l: Look, under_titlebar: bool) -> El {
    let mut c = col().key("sidebar").w(SIDEBAR_W).h_full().fill(l.sidebar).pb(10.0).px(8.0).gap(1.0);
    c = c.child(el().key("sidebar-drag").w_full().h(if under_titlebar { BAR_H } else { 10.0 }).window_drag());
    c = c.child(text("Places").font(SMALL).color(l.t.text_secondary).px(8.0).pb(4.0));
    for (i, (name, path)) in s.places.iter().enumerate() {
        let here = *path == s.cwd;
        let p = path.clone();
        let mut item = row()
            .key(format!("place-{i}"))
            .h(26.0)
            .px(8.0)
            .center_y()
            .radius(5.0)
            .cursor(Cursor::Pointer)
            .on_press_tx(move |s: &mut State, tx| {
                tx.instant();
                s.go_to(p.clone());
            })
            .child(text(name.clone()).font(BODY).color(l.t.text).truncate().grow(1.0).min_w(0.0));
        let hint = Cmd::ALL.get(Cmd::ALL.iter().position(|&c| c == Cmd::Place1).unwrap_or(0) + i).filter(|c| c.place() == Some(i));
        if let Some(&cmd) = hint {
            let label = s.keymap.label(cmd);
            if !label.is_empty() {
                item = item.child(text(label).font(SMALL).color(l.t.text_secondary.alpha(0.6)));
            }
        }
        item = if here { item.fill(l.t.control_off) } else { item.hover(l.t.hover) };
        c = c.child(item);
    }
    c
}

fn pathbar(s: &State, l: Look) -> El {
    let nav = |label: &'static str, key: &'static str, on: bool, f: fn(&mut State)| {
        text(label)
            .key(key)
            .font(TextStyle::new(16.0, 500.0))
            .color(if on { l.t.text } else { l.t.text_secondary.alpha(0.4) })
            .w(24.0)
            .h(24.0)
            .align(Align::Center)
            .radius(5.0)
            .hover(l.t.hover)
            .cursor(Cursor::Pointer)
            .on_press_tx(move |s: &mut State, tx| {
                tx.instant();
                f(s);
            })
    };
    let mut crumbs = row().key("crumbs").grow(1.0).min_w(0.0).center_y().gap(2.0);
    let mut acc = PathBuf::new();
    let comps: Vec<Component> = s.cwd.components().collect();
    let n = comps.len();
    for (i, comp) in comps.iter().enumerate() {
        acc.push(comp.as_os_str());
        if matches!(comp, Component::RootDir) && i > 0 {
            continue; // Windows: `C:` then `\`
        }
        let label = match comp {
            Component::RootDir => std::path::MAIN_SEPARATOR.to_string(),
            c => c.as_os_str().to_string_lossy().into_owned(),
        };
        let last = i + 1 == n;
        let target = acc.clone();
        if i > 0 && !matches!(comps[i - 1], Component::RootDir | Component::Prefix(_)) {
            crumbs = crumbs.child(text("›").font(CRUMB).color(l.t.text_secondary.alpha(0.5)));
        }
        crumbs = crumbs.child(
            text(label)
                .key(format!("crumb-{i}"))
                .font(CRUMB)
                .color(if last { l.t.text } else { l.t.text_secondary })
                .px(4.0)
                .py(2.0)
                .radius(4.0)
                .hover(l.t.hover)
                .cursor(Cursor::Pointer)
                .truncate()
                .min_w(if last { 0.0 } else { 12.0 })
                .on_press_tx(move |s: &mut State, tx| {
                    tx.instant();
                    s.go_to(target.clone());
                }),
        );
    }
    row()
        .key("pathbar")
        .w_full()
        .h(BAR_H)
        .window_drag()
        .px(8.0)
        .gap(2.0)
        .center_y()
        .child(nav("‹", "nav-back", !s.back.is_empty(), State::go_back))
        .child(nav("›", "nav-fwd", !s.fwd.is_empty(), State::go_forward))
        .child(nav("↑", "nav-up", s.cwd.parent().is_some(), State::parent))
        .child(el().w(6.0))
        .child(crumbs)
}

fn header(s: &State, l: Look, cols: Cols) -> El {
    let col_head = |label: &'static str, key: SortKey, w: Option<f32>, align: Align| {
        let on = s.sort.key == key;
        let arrow = if !on {
            ""
        } else if s.sort.desc {
            " ↓"
        } else {
            " ↑"
        };
        let mut t = text(format!("{label}{arrow}"))
            .key(format!("sort-{label}"))
            .font(SMALL)
            .color(if on { l.t.text } else { l.t.text_secondary })
            .align(align)
            .cursor(Cursor::Pointer)
            .on_press_tx(move |s: &mut State, tx| {
                tx.instant();
                s.set_sort(key);
                s.reveal(tx);
            });
        t = match w {
            Some(w) => t.w(w),
            None => t.grow(1.0).min_w(0.0),
        };
        t
    };
    let mut h =
        row().key("header").w_full().h(24.0).px(12.0).gap(12.0).center_y().child(col_head("Name", SortKey::Name, None, Align::Start));
    if cols.kind {
        h = h.child(col_head("Kind", SortKey::Kind, Some(KIND_W), Align::Start));
    }
    h = h.child(col_head("Size", SortKey::Size, Some(SIZE_W), Align::End));
    if cols.date {
        h = h.child(col_head("Modified", SortKey::Modified, Some(DATE_W), Align::Start));
    }
    h
}

fn list(s: &State, l: Look, cols: Cols) -> El {
    // The list itself is a drop target for the current folder, and right-clicking empty
    // space opens the menu with Paste and New Folder.
    let base = col()
        .key("list")
        .grow(1.0)
        .min_h(0.0)
        .w_full()
        .on_drop_files(|s: &mut State, files, tx| {
            tx.instant();
            let here = s.cwd.clone();
            s.drop_files(files, here, tx);
        })
        .on_secondary_press(|s: &mut State, press, tx| {
            tx.instant();
            s.context_menu(None, press, tx);
        });
    if s.visible.is_empty() {
        let msg = if s.entries.is_empty() { "Empty folder" } else { "No matches" };
        return base.center().child(text(msg).font(BODY).color(l.t.text_secondary));
    }
    base.scroll(ScrollSpec { stick_bottom: false }).child(
        col()
            .key("rows")
            .w_full()
            .growth(GrowthSpec { spring: rune::GROWTH, edge: rune::edge::NO_MASK })
            .virtual_rows(
                s.visible.len(),
                ROW_H,
                |s: &State, i| Cow::Borrowed(s.entries[s.visible[i] as usize].key.as_str()),
                move |s: &State, i| file_row(s, i, l, cols),
            )
            .virtual_rows_version(s.version),
    )
}

fn file_row(s: &State, i: usize, l: Look, cols: Cols) -> El {
    let e = &s.entries[s.visible[i] as usize];
    let selected = s.is_selected(i);
    // With several rows selected, the cursor row that isn't selected gets a soft highlight.
    let cursor = !selected && i == s.sel && !s.marked.is_empty();
    let drop_here = s.drop_target.as_ref() == Some(&e.os);
    let (fg, dim) = if selected { (l.sel_text, l.sel_text.alpha(0.8)) } else { (l.t.text, l.t.text_secondary) };
    let glyph = if e.is_dir { "▸" } else { " " };
    let name_color = if e.hidden && !selected { fg.alpha(0.55) } else { fg };
    let version = s.version;
    let renaming = s.rename.as_ref().filter(|r| r.os == e.os);
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
            let mut name = e.name.clone();
            if e.is_link {
                name.push_str("  ↗"); // In the bundled Inter, so it draws the same everywhere.
            }
            text(name).font(BODY).color(name_color).truncate_middle().grow(1.0).min_w(0.0)
        }
    };
    let mut r = row()
        .w_full()
        .h(ROW_H)
        .px(12.0)
        .gap(12.0)
        .center_y()
        .radius(4.0)
        .on_press_tx(move |s: &mut State, tx| {
            tx.instant();
            if s.version != version {
                return;
            }
            if s.rename.as_ref().is_some_and(|r| s.visible.get(i).is_none_or(|&e| s.entries[e as usize].os != r.os)) {
                s.commit_rename(tx);
                return;
            }
            let m = tx.mods();
            if m.shift {
                s.extend_to(i);
            } else if m.cmd {
                s.toggle_mark(i);
            } else if !(s.is_selected(i) && s.marked.len() > 1) {
                // A plain click inside a multi-selection keeps it, so it can be dragged.
                s.select(i);
            } else {
                s.sel = i;
            }
        })
        .on_double_press_tx(move |s: &mut State, tx| {
            tx.instant();
            if s.version == version && s.rename.is_none() {
                s.select(i);
                s.open_selected();
            }
        })
        .on_secondary_press(move |s: &mut State, press, tx| {
            tx.instant();
            if s.version == version {
                s.context_menu(Some(i), press, tx);
            }
        })
        .child(
            row()
                .grow(1.0)
                .min_w(0.0)
                .gap(6.0)
                .center_y()
                .child(text(glyph).font(BODY).color(if selected { fg } else { l.accent }).w(10.0))
                .child(name_el),
        );
    if renaming.is_none() {
        r = r.drag_out(move |s: &State| if s.version == version { s.drag_item(i) } else { None }).on_drag_end(
            |s: &mut State, outcome, tx| {
                tx.instant();
                if outcome == rune::DragOutcome::Moved {
                    s.reload();
                }
            },
        );
    }
    if e.is_dir {
        let target = s.cwd.join(&e.os);
        let os = e.os.clone();
        r = r
            .on_drop_files(move |s: &mut State, files, tx| {
                tx.instant();
                s.drop_files(files, target.clone(), tx);
            })
            .on_drop_hover(move |s: &mut State, over, tx| {
                tx.instant();
                s.drop_target = over.then(|| os.clone());
            });
    }
    if cols.kind {
        r = r.child(text(fs::kind(e).to_string()).font(NUM).color(dim).truncate().w(KIND_W));
    }
    r = r.child(text(if e.is_dir { "—".to_string() } else { fs::fmt_size(e.size) }).font(NUM).color(dim).align(Align::End).w(SIZE_W));
    if cols.date {
        r = r.child(text(fs::fmt_time(e.mtime_ms, s.now)).font(NUM).color(dim).w(DATE_W));
    }
    r = if drop_here {
        r.fill(l.accent.alpha(0.25)).border(1.5, l.accent)
    } else if selected {
        r.fill(l.accent)
    } else if cursor {
        r.fill(l.accent.alpha(0.18)).hover(l.t.hover)
    } else if i % 2 == 1 {
        r.fill(l.stripe).hover(l.t.hover)
    } else {
        r.hover(l.t.hover)
    };
    r
}

fn status(s: &State, l: Look) -> El {
    let mut bar = row().key("status").w_full().h(28.0).px(12.0).gap(10.0).center_y();
    if !s.filter.is_empty() {
        let label = if s.path_mode() { "Go to" } else { "Filter" };
        bar = bar
            .child(text(label).font(SMALL).color(l.accent))
            .child(text(format!("{}▏", s.filter)).font(MONO).color(l.t.text).truncate_head().min_w(0.0).shrink());
    }
    if let Some(job) = s.job_status() {
        bar = bar.child(text(job).font(NUM).color(l.accent).truncate().min_w(0.0).shrink());
    } else if let Some(n) = &s.note {
        bar = bar.child(text(n.clone()).font(SMALL).color(l.t.text_secondary).truncate().min_w(0.0).shrink());
    }
    bar = bar.child(el().grow(1.0));
    if s.is_loading() {
        bar = bar.child(text("Loading…").font(SMALL).color(l.t.text_secondary));
    }
    let hidden = s.entries.len() - s.entries.iter().filter(|e| !e.hidden).count();
    let count = if s.visible.len() == s.entries.len() || (!s.show_hidden && s.visible.len() + hidden == s.entries.len()) {
        format!("{} items", s.visible.len())
    } else {
        format!("{} of {} items", s.visible.len(), s.entries.len())
    };
    let picked = if s.marked.len() > 1 { format!("{} selected · ", s.marked.len()) } else { String::new() };
    let hint = if s.show_hidden { "hidden shown" } else { "" };
    bar.child(text(hint).font(SMALL).color(l.t.text_secondary.alpha(0.7)))
        .child(text(format!("{picked}{count}")).font(NUM).color(l.t.text_secondary))
}

/// Rows the palette shows before it scrolls.
const PALETTE_ROWS: f32 = 12.0;
const PALETTE_ROW_H: f32 = 28.0;

/// The command palette: over the top of the window, a field and the ranked commands with their
/// keys right-aligned. Modal: a click outside closes it, as Esc does.
fn palette_view(s: &State, l: Look) -> El {
    let Some(p) = &s.palette else { return el() };
    let hits = s.palette_hits();
    let sel = s.palette_sel(&hits);
    let field = text_input(&p.edit, TextStyle::new(15.0, 400.0))
        .key("palette-field")
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
    let rows =
        col().key("palette-rows").w_full().px(6.0).growth(GrowthSpec { spring: rune::GROWTH, edge: rune::edge::NO_MASK }).virtual_rows(
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
        .w_full()
        .max_w(560.0)
        .fill(l.t.card)
        .border(1.0, l.t.card_border)
        .radius(10.0)
        .shadow(10.0, 30.0, l.t.shadow)
        .modal(true, |s: &mut State| s.palette = None)
        .child(row().h(44.0).px(14.0).center_y().child(field))
        .child(hr(l))
        .child(list);
    if let Some(why) = s.palette_why() {
        panel = panel.child(hr(l)).child(text(why).font(SMALL).color(l.t.text_secondary).px(16.0).py(7.0));
    }
    col().key("palette-layer").pin(Some(0.0), Some(0.0), Some(0.0), None).items_center().px(20.0).pt(BAR_H + 6.0).child(panel)
}

/// One palette row: the title, and its keys right-aligned and dimmer. Commands that can't run
/// now are dimmed.
fn palette_row(h: &palette::Hit, on: bool, l: Look) -> El {
    let (fg, dim) = match (on, h.enabled) {
        (true, true) => (l.sel_text, l.sel_text.alpha(0.75)),
        (true, false) => (l.t.text.alpha(0.55), l.t.text_secondary.alpha(0.55)),
        (false, true) => (l.t.text, l.t.text_secondary.alpha(0.8)),
        (false, false) => (l.t.text.alpha(0.4), l.t.text_secondary.alpha(0.4)),
    };
    let cmd = h.cmd;
    let r = row()
        .w_full()
        .h(PALETTE_ROW_H)
        .px(10.0)
        .gap(12.0)
        .center_y()
        .radius(5.0)
        .cursor(Cursor::Pointer)
        .on_press_tx(move |s: &mut State, tx| s.palette_pick(cmd, tx))
        .child(text(h.title.to_string()).font(BODY).color(fg).truncate().grow(1.0).min_w(0.0))
        .child(text(h.keys.clone()).font(NUM).color(dim));
    if on && h.enabled {
        r.fill(l.accent)
    } else if on {
        r.fill(l.t.control_off)
    } else {
        r.hover(l.t.hover)
    }
}
