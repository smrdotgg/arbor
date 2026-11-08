//! The window: sidebar of places, then one or two panes side by side (each with a path bar,
//! tabs when it has more than one, sortable column headers and the virtualized file list),
//! and a status line for the active pane that doubles as the filter / go-to field.

use std::borrow::Cow;
use std::path::{Component, PathBuf};

use rune::tok::{self, TextStyle, Theme};
use rune::{Align, Color, Cursor, Cx, Drag, El, GrowthSpec, Phase, PointerButton, ScrollPos, ScrollSpec, col, el, row, text, text_input};

use crate::app::{Pane, State};
use crate::commands::Cmd;
use crate::folder::{Folder, tab_label};
use crate::fs::{self, SortKey};
use crate::palette;

const ROW_H: f32 = 24.0;
/// Height of the top band (path bar, and the sidebar's traffic-light strip on macOS).
pub const BAR_H: f32 = 38.0;
const SIDEBAR_W: f32 = 168.0;
const KIND_W: f32 = 72.0;
const SIZE_W: f32 = 80.0;
const DATE_W: f32 = 132.0;
const TABS_H: f32 = 30.0;
/// How far a row drag travels before letting go over the other pane drops there.
const DRAG_START: f32 = 6.0;

const BODY: TextStyle = TextStyle::new(13.0, 400.0);
const SMALL: TextStyle = TextStyle::new(11.5, 500.0);
const NUM: TextStyle = TextStyle::new(12.0, 400.0).features("\"tnum\"");
const CRUMB: TextStyle = TextStyle::new(13.0, 500.0);
const MONO: TextStyle = TextStyle::new(12.0, 400.0).mono();
const TAB: TextStyle = TextStyle::new(12.0, 500.0);

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

/// Where the two panes are (window points: x, y, width, height), for a row dragged from one to
/// the other. Only when split.
#[derive(Clone, Copy)]
struct Panes {
    left: ([f32; 4], u64),
    right: ([f32; 4], u64),
}

impl Panes {
    /// The pane under window point `p`: inside its rect, so a release outside the window (or
    /// over the sidebar or status bar) is over no pane.
    fn at(self, [x, y]: [f32; 2]) -> Option<u64> {
        let inside = |[px, py, w, h]: [f32; 4]| x >= px && x < px + w && y >= py && y < py + h;
        [self.left, self.right].into_iter().find(|&(r, _)| inside(r)).map(|(_, id)| id)
    }
}

/// How a pane draws: which pane, whether it's the active one of two, and its columns.
#[derive(Clone, Copy)]
struct Ctx {
    id: u64,
    l: Look,
    cols: Cols,
    /// Selection in the accent colour: the active pane, or the only one.
    focused: bool,
    split: Option<Panes>,
    /// The pane's laid-out width (`f32::MAX` before the first layout).
    w: f32,
}

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    root(s, cx).on_paste_content(|s: &mut State, content, tx| {
        tx.instant();
        s.paste(content);
    })
}

fn root(s: &State, cx: &mut Cx<State>) -> El {
    let l = look(cx.system_dark());
    // Reads laid-out positions and sizes only; scrolling never re-runs the view.
    let split = match s.panes.as_slice() {
        [a, b] => {
            // Before the first layout, an empty rect: no pane to drop on yet.
            let rect = |p: &Pane| cx.layout_with(&format!("pane-{}", p.id()), |r| r.map(f32::round)).unwrap_or_default();
            Some(Panes { left: (rect(a), a.id()), right: (rect(b), b.id()) })
        }
        _ => None,
    };
    let mut panes = row().key("panes").w_full().grow(1.0).min_h(0.0);
    for (ix, p) in s.panes.iter().enumerate() {
        if ix > 0 {
            panes = panes.child(el().w(1.0).h_full().fill(l.t.separator));
        }
        let w = cx.layout_size(&format!("pane-{}", p.id())).map_or(f32::MAX, |[w, _]| w);
        let c = Ctx { id: p.id(), l, cols: Cols::for_width(w), focused: ix == s.active || split.is_none(), split, w };
        panes = panes.child(pane(s, p, c));
    }
    let window = row()
        .w_full()
        .h_full()
        .fill(l.t.bg)
        .child(sidebar(s, l, cx.titlebar_inset() > 0.0))
        .child(el().w(1.0).h_full().fill(l.t.separator))
        .child(col().key("main").grow(1.0).min_w(0.0).h_full().child(panes).child(hr(l)).child(status(s, l)));
    if s.palette.is_some() { window.child(palette_view(s, l)) } else { window }
}

fn hr(l: Look) -> El {
    el().w_full().h(1.0).fill(l.t.separator)
}

fn sidebar(s: &State, l: Look, under_titlebar: bool) -> El {
    let mut c = col().key("sidebar").w(SIDEBAR_W).h_full().fill(l.sidebar).pb(10.0).px(8.0).gap(1.0);
    c = c.child(el().key("sidebar-drag").w_full().h(if under_titlebar { BAR_H } else { 10.0 }).window_drag());
    c = c.child(text("Places").font(SMALL).color(l.t.text_secondary).px(8.0).pb(4.0));
    let cwd = &s.cur().cwd;
    for (i, (name, path)) in s.places.iter().enumerate() {
        let here = path == cwd;
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

/// One pane. A click anywhere in it makes it the active one; every handler inside does that
/// first, so the command acts on the pane that was clicked.
fn pane(s: &State, p: &Pane, c: Ctx) -> El {
    let (id, l, f) = (c.id, c.l, &p.f);
    let mut col = col()
        .key(format!("pane-{id}"))
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .on_press_tx(move |s: &mut State, tx| {
            tx.instant();
            s.activate_id(id, tx);
        })
        .child(pathbar(f, l, c.w));
    if p.tabs.len() > 1 {
        col = col.child(hr(l)).child(tabbar(p, l));
    }
    col = col.child(hr(l)).child(header(f, c)).child(hr(l)).child(list(f, c));
    if c.split.is_some() {
        // The active pane: a thin accent line along its top (its selection is in the accent
        // colour too, the other pane's in grey).
        if c.focused {
            col = col.child(el().key("active-mark").pin(Some(0.0), Some(0.0), Some(0.0), None).h(2.0).fill(l.accent));
        }
        // A row from the other pane is dragged over this one.
        if s.pane_drop == Some(id) {
            col = col.child(
                el().key("drop-mark").pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0)).fill(l.accent.alpha(0.06)).border(2.0, l.accent),
            );
        }
    }
    col
}

/// Every handler in pane `id`: make it active first (false if it has closed since).
fn in_pane(s: &mut State, id: u64, tx: &mut rune::Tx) -> bool {
    tx.instant();
    s.activate_id(id, tx)
}

fn pathbar(f: &Folder, l: Look, width: f32) -> El {
    let id = f.id;
    let nav = |label: &'static str, key: &'static str, on: bool, go: fn(&mut State)| {
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
                if in_pane(s, id, tx) {
                    go(s);
                }
            })
    };
    let mut crumbs = row().key("crumbs").grow(1.0).min_w(0.0).center_y().gap(CRUMB_GAP);
    let all = crumbs_of(&f.cwd);
    let n = all.len();
    let sep = || text("›").font(CRUMB).color(l.t.text_secondary.alpha(0.5));
    let crumb = |label: String, key: String, target: PathBuf, last: bool| {
        text(label)
            .key(key)
            .font(CRUMB)
            .color(if last { l.t.text } else { l.t.text_secondary })
            .px(CRUMB_PAD)
            .py(2.0)
            .radius(4.0)
            .hover(l.t.hover)
            .cursor(Cursor::Pointer)
            .truncate()
            .on_press_tx(move |s: &mut State, tx| {
                if in_pane(s, id, tx) {
                    s.go_to(target.clone());
                }
            })
    };
    let shown = shown_crumbs(&all, width - PATHBAR_CHROME, measure_crumb);
    if let Shown::From { first, ellipsis: true } = shown {
        // The deepest hidden folder: one click up from the first crumb shown.
        let up = all[first - 1].target.clone();
        crumbs = crumbs.child(crumb("…".into(), "crumb-more".into(), up.clone(), false).help(fs::pretty(&up))).child(sep());
    }
    let first = match shown {
        Shown::All => 0,
        Shown::From { first, .. } => first,
    };
    for (i, c) in all.into_iter().enumerate().skip(first) {
        if c.sep && i > first {
            crumbs = crumbs.child(sep());
        }
        crumbs = crumbs.child(crumb(c.label, format!("crumb-{i}"), c.target, i + 1 == n));
    }
    row()
        .key("pathbar")
        .w_full()
        .h(BAR_H)
        .window_drag()
        .px(8.0)
        .gap(2.0)
        .center_y()
        .child(nav("‹", "nav-back", !f.back.is_empty(), State::go_back))
        .child(nav("›", "nav-fwd", !f.fwd.is_empty(), State::go_forward))
        .child(nav("↑", "nav-up", f.cwd.parent().is_some(), State::parent))
        .child(el().w(6.0))
        .child(crumbs)
}

/// Space between breadcrumbs (and their separators), and each crumb's padding on either side.
const CRUMB_GAP: f32 = 2.0;
const CRUMB_PAD: f32 = 4.0;
/// What the path bar spends besides the crumbs: its padding, three 24 pt buttons, a 6 pt
/// spacer, and the gaps between those five.
const PATHBAR_CHROME: f32 = 2.0 * 8.0 + 3.0 * 24.0 + 6.0 + 4.0 * 2.0;

/// One breadcrumb: its label, the folder it opens, and whether a "›" goes before it (not
/// after the root or a Windows drive).
struct Crumb {
    label: String,
    target: PathBuf,
    sep: bool,
}

fn crumbs_of(path: &std::path::Path) -> Vec<Crumb> {
    let comps: Vec<Component> = path.components().collect();
    let mut acc = PathBuf::new();
    let mut out = Vec::new();
    for (i, comp) in comps.iter().enumerate() {
        acc.push(comp.as_os_str());
        if matches!(comp, Component::RootDir) && i > 0 {
            continue; // Windows: `C:` then `\`
        }
        let label = match comp {
            Component::RootDir => std::path::MAIN_SEPARATOR.to_string(),
            c => c.as_os_str().to_string_lossy().into_owned(),
        };
        let sep = i > 0 && !matches!(comps[i - 1], Component::RootDir | Component::Prefix(_));
        out.push(Crumb { label, target: acc.clone(), sep });
    }
    out
}

/// Which breadcrumbs fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    All,
    /// From crumb `first` on, after one "…" crumb standing for the ones before (when there's
    /// room for it).
    From {
        first: usize,
        ellipsis: bool,
    },
}

/// Which crumbs fit in `avail` points, `width` giving a label's width in points. Rune can't be
/// told which children to shrink first (no flex-shrink weights), so left to itself every crumb
/// truncates, the current folder too (see `RUNE-GAPS.md`). Instead the leading crumbs fold into
/// one "…" crumb until the rest fit whole. The last (the current folder) is always shown, and
/// truncates only when it doesn't fit even alone.
fn shown_crumbs(crumbs: &[Crumb], avail: f32, width: impl Fn(&str) -> f32) -> Shown {
    let n = crumbs.len();
    // A cheap bound first: no glyph in a 13 pt UI font is wider than 1.3 em, so a wide pane
    // measures nothing.
    let bound: f32 = crumbs.iter().map(|c| c.label.chars().count() as f32 * CRUMB.size * 1.3 + 2.0 * CRUMB_PAD + 3.0 * CRUMB.size).sum();
    if bound <= avail || n < 2 {
        return Shown::All;
    }
    // Room to spare for rounding, so the last crumb is never cut by a fraction of a point.
    let avail = avail - 2.0;
    let item = |w: f32| w + 2.0 * CRUMB_PAD;
    let sep = width("›") + 2.0 * CRUMB_GAP;
    let widths: Vec<f32> = crumbs.iter().map(|c| item(width(&c.label))).collect();
    // Crumbs `k..` with a "›" before each but the first, and the gaps between them.
    let tail = |k: usize| -> f32 {
        (k..n)
            .map(|i| {
                widths[i]
                    + if i > k && crumbs[i].sep {
                        sep
                    } else if i > k {
                        CRUMB_GAP
                    } else {
                        0.0
                    }
            })
            .sum()
    };
    if tail(0) <= avail {
        return Shown::All;
    }
    let more = item(width("…")) + sep;
    match (1..n).find(|&k| more + tail(k) <= avail) {
        Some(first) => Shown::From { first, ellipsis: true },
        None => Shown::From { first: n - 1, ellipsis: false },
    }
}

/// A breadcrumb label's width in points, as Rune will lay it out.
fn measure_crumb(label: &str) -> f32 {
    rune::text::measure(label, &[], CRUMB, None).0
}

/// The pane's tabs, only when it has two or more: each shows its folder's name (the whole
/// path in a tooltip). A click shows it, a middle click closes it, "+" opens a new one.
fn tabbar(p: &Pane, l: Look) -> El {
    let id = p.id();
    let mut bar = row().key("tabs").w_full().h(TABS_H).px(6.0).gap(4.0).center_y();
    for j in 0..p.tabs.len() {
        let path = p.tab_path(j);
        let on = j == p.tab;
        let t = row()
            .key(format!("tab-{j}"))
            .h(22.0)
            .px(10.0)
            .grow(1.0)
            .min_w(0.0)
            .max_w(220.0)
            .center()
            .radius(5.0)
            .cursor(Cursor::Pointer)
            .help(fs::pretty(path))
            .pointer_buttons([PointerButton::Middle])
            .on_press_tx(move |s: &mut State, tx| {
                tx.instant();
                let Some(ix) = s.pane_ix(id) else { return };
                if tx.button() == PointerButton::Middle {
                    s.close_tab(ix, j, tx);
                } else {
                    s.activate(ix, tx);
                    s.show_tab(ix, j, tx);
                }
            })
            .child(text(tab_label(path)).font(TAB).color(if on { l.t.text } else { l.t.text_secondary }).truncate().min_w(0.0));
        bar = bar.child(if on { t.fill(l.t.control_off) } else { t.hover(l.t.hover) });
    }
    bar.child(
        text("+")
            .key("tab-new")
            .font(TextStyle::new(15.0, 500.0))
            .color(l.t.text_secondary)
            .w(22.0)
            .h(22.0)
            .align(Align::Center)
            .radius(5.0)
            .hover(l.t.hover)
            .cursor(Cursor::Pointer)
            .help("New Tab")
            .on_press_tx(move |s: &mut State, tx| {
                if in_pane(s, id, tx) {
                    s.new_tab(tx);
                }
            }),
    )
}

fn header(f: &Folder, c: Ctx) -> El {
    let (id, l) = (c.id, c.l);
    let col_head = |label: &'static str, key: SortKey, w: Option<f32>, align: Align| {
        let on = f.sort.key == key;
        let arrow = if !on {
            ""
        } else if f.sort.desc {
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
    if c.cols.kind {
        h = h.child(col_head("Kind", SortKey::Kind, Some(KIND_W), Align::Start));
    }
    h = h.child(col_head("Size", SortKey::Size, Some(SIZE_W), Align::End));
    if c.cols.date {
        h = h.child(col_head("Modified", SortKey::Modified, Some(DATE_W), Align::Start));
    }
    h
}

fn list(f: &Folder, c: Ctx) -> El {
    let id = c.id;
    // The list itself is a drop target for its folder, and right-clicking empty space opens
    // the menu with Paste and New Folder.
    let base = col()
        .key(f.list_key())
        .grow(1.0)
        .min_h(0.0)
        .w_full()
        .on_drop_files(move |s: &mut State, files, tx| {
            tx.instant();
            if let Some(p) = s.pane(id) {
                let here = p.f.cwd.clone();
                s.drop_files(files, here, tx);
            }
        })
        .on_secondary_press(move |s: &mut State, press, tx| {
            if in_pane(s, id, tx) {
                s.context_menu(None, press, tx);
            }
        });
    if f.visible.is_empty() {
        // A tab whose fresh read hasn't landed yet: its path, and an empty list until it does.
        let msg = if f.is_loading() {
            ""
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

fn file_row(s: &State, f: &Folder, i: usize, c: Ctx) -> El {
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
            if !in_pane(s, id, tx) || s.cur().version != version {
                return;
            }
            let f = s.cur();
            if s.rename.as_ref().is_some_and(|r| f.visible.get(i).is_none_or(|&e| f.entries[e as usize].os != r.os)) {
                s.commit_rename(tx);
                return;
            }
            let m = tx.mods();
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
                .child(text(glyph).font(BODY).color(if selected && c.focused { fg } else { l.accent }).w(10.0))
                .child(name_el),
        );
    if renaming.is_none() {
        match c.split {
            // Out to other apps (macOS: Rune starts no system drag elsewhere).
            None => {
                r = r.drag_out(move |s: &State| if s.pane(id)?.f.version == version { s.drag_item(id, i) } else { None }).on_drag_end(
                    |s: &mut State, outcome, tx| {
                        tx.instant();
                        if outcome == rune::DragOutcome::Moved {
                            s.reload();
                        }
                    },
                );
            }
            // Split: within the window, onto the other pane. A system drag can't land in the
            // window it came from (RUNE-GAPS.md), so this one is Arbor's own.
            Some(panes) => {
                r = r.on_drag(move |s: &mut State, d: &mut Drag, tx| {
                    tx.instant();
                    let far = d.dx.hypot(d.dy) > DRAG_START;
                    let over = panes.at(d.window_point()).filter(|&p| p != id && far);
                    if d.phase == Phase::Up {
                        match over {
                            Some(to) if !d.cancelled() => s.drop_on_pane(id, i, version, to, tx),
                            _ => s.pane_drop = None,
                        }
                    } else if s.pane_drop != over {
                        s.pane_drop = over;
                    } else {
                        tx.live_only();
                    }
                });
            }
        }
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
    if cols.kind {
        r = r.child(text(fs::kind(e).to_string()).font(NUM).color(dim).truncate().w(KIND_W));
    }
    r = r.child(text(if e.is_dir { "—".to_string() } else { fs::fmt_size(e.size) }).font(NUM).color(dim).align(Align::End).w(SIZE_W));
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

fn status(s: &State, l: Look) -> El {
    let f = s.cur();
    let mut bar = row().key("status").w_full().h(28.0).px(12.0).gap(10.0).center_y();
    if !f.filter.is_empty() {
        let label = if f.path_mode() { "Go to" } else { "Filter" };
        bar = bar
            .child(text(label).font(SMALL).color(l.accent))
            .child(text(format!("{}▏", f.filter)).font(MONO).color(l.t.text).truncate_head().min_w(0.0).shrink());
    }
    if let Some(job) = s.job_status() {
        bar = bar.child(text(job).font(NUM).color(l.accent).truncate().min_w(0.0).shrink());
    } else if let Some(n) = &s.note {
        bar = bar.child(text(n.clone()).font(SMALL).color(l.t.text_secondary).truncate().min_w(0.0).shrink());
    }
    bar = bar.child(el().grow(1.0));
    if f.is_loading() {
        bar = bar.child(text("Loading…").font(SMALL).color(l.t.text_secondary));
    }
    let hidden = f.entries.len() - f.entries.iter().filter(|e| !e.hidden).count();
    let count = if f.visible.len() == f.entries.len() || (!f.show_hidden && f.visible.len() + hidden == f.entries.len()) {
        format!("{} items", f.visible.len())
    } else {
        format!("{} of {} items", f.visible.len(), f.entries.len())
    };
    let picked = if f.marked.len() > 1 { format!("{} selected · ", f.marked.len()) } else { String::new() };
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 7 pt a character, standing in for text measurement.
    fn seven(s: &str) -> f32 {
        s.chars().count() as f32 * 7.0
    }

    const DEEP: &str = "/home/smr/arbor-smoke/m3demo/Photos";

    #[cfg(unix)]
    #[test]
    fn crumbs_are_the_path_with_separators_after_the_root() {
        let c = crumbs_of(Path::new(DEEP));
        assert_eq!(c.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["/", "home", "smr", "arbor-smoke", "m3demo", "Photos"]);
        assert_eq!(c.iter().map(|c| c.sep).collect::<Vec<_>>(), [false, false, true, true, true, true]);
        assert_eq!(c[2].target, Path::new("/home/smr"));
    }

    /// A narrow pane folds the leading crumbs into "…" rather than truncating every crumb,
    /// and the current folder stays whole until it alone doesn't fit.
    #[test]
    fn narrow_breadcrumbs_fold_from_the_left_and_keep_the_current_folder_whole() {
        let c = crumbs_of(Path::new(DEEP));
        let shown = |avail| shown_crumbs(&c, avail, seven);
        assert_eq!(shown(2000.0), Shown::All);
        assert_eq!(shown(313.0), Shown::All); // 311 pt of crumbs, 2 pt of slack
        // The right pane of a split 980 pt window: 404 pt, 302 of them for crumbs.
        assert_eq!(shown(302.0), Shown::From { first: 2, ellipsis: true });
        assert_eq!(shown(100.0), Shown::From { first: 5, ellipsis: true });
        // Not even "…" and the current folder: just the folder, which truncates.
        assert_eq!(shown(60.0), Shown::From { first: 5, ellipsis: false });
    }

    /// A release over the other pane drops there; one outside the window (past its right
    /// edge, below it, above it) or over the sidebar drops nowhere.
    #[test]
    fn a_drag_lands_only_inside_a_pane() {
        // A 980 × 620 window: sidebar to 169, panes side by side above a 28 pt status bar.
        let panes = Panes { left: ([169.0, 0.0, 405.0, 591.0], 1), right: ([575.0, 0.0, 405.0, 591.0], 2) };
        assert_eq!(panes.at([310.0, 77.0]), Some(1));
        assert_eq!(panes.at([700.0, 210.0]), Some(2));
        for outside in [[1200.0, 210.0], [700.0, 900.0], [700.0, -5.0], [100.0, 210.0], [700.0, 600.0], [980.0, 210.0]] {
            assert_eq!(panes.at(outside), None, "{outside:?}");
        }
    }

    /// With Rune's own measurement: in that same pane, the crumbs shown fit whole.
    #[test]
    fn measured_breadcrumbs_fit_a_split_pane() {
        let c = crumbs_of(Path::new(DEEP));
        let avail = 404.0 - PATHBAR_CHROME;
        let Shown::From { first, ellipsis: true } = shown_crumbs(&c, avail, measure_crumb) else { panic!("should fold") };
        let sep = measure_crumb("›") + 2.0 * CRUMB_GAP;
        let used: f32 = measure_crumb("…") + c[first..].iter().map(|c| measure_crumb(&c.label) + 2.0 * CRUMB_PAD + sep).sum::<f32>();
        assert!(used + 2.0 * CRUMB_PAD <= avail, "{used} of {avail}");
        assert!(measure_crumb("Photos") > 20.0);
    }
}
