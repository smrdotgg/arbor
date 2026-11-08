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
use crate::preview::{Body, Preview};
use crate::search::Search;

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
/// The text preview: the platform's own monospaced font (Rune's `Mono` slot is IBM Plex Mono,
/// which native builds don't bundle). Loaded the first time a text preview shows.
const CODE: TextStyle = TextStyle::new(12.0, 400.0).family(tok::Family::SystemMono);

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
    // With the preview open in a split window, it takes the other pane's place (as a
    // two-panel commander's quick view does).
    let hidden = |ix: usize| s.preview.is_some() && ix != s.active;
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
        if let Some(pv) = s.preview.as_ref().filter(|_| split.is_some() && hidden(ix)) {
            panes = panes.child(preview_panel(s, pv, l, cx, None));
            continue;
        }
        let w = cx.layout_size(&format!("pane-{}", p.id())).map_or(f32::MAX, |[w, _]| w);
        // With the other pane covered by the preview, this one draws as if alone (its rows
        // drag out to other apps; there's no pane to drop on).
        let split = split.filter(|_| s.preview.is_none());
        let c = Ctx { id: p.id(), l, cols: Cols::for_width(w), focused: ix == s.active || split.is_none(), split, w };
        panes = panes.child(pane(s, p, c));
    }
    // One pane: the preview docks on its right, and the list stays usable beside it.
    if let Some(pv) = s.preview.as_ref().filter(|_| split.is_none()) {
        let main = cx.layout_size("panes").map_or(980.0 - SIDEBAR_W, |[w, _]| w);
        let w = (main * 0.42).clamp(PREVIEW_MIN_W, PREVIEW_MAX_W).min(main - PREVIEW_MIN_W).round();
        panes = panes.child(el().w(1.0).h_full().fill(l.t.separator)).child(preview_panel(s, pv, l, cx, Some(w)));
    }
    let window = row()
        .w_full()
        .h_full()
        .fill(l.t.bg)
        .child(sidebar(s, l, cx.titlebar_inset() > 0.0))
        .child(el().w(1.0).h_full().fill(l.t.separator))
        .child(col().key("main").grow(1.0).min_w(0.0).h_full().child(panes).child(hr(l)).child(status(s, l)));
    if let Some(a) = &s.asking {
        window.child(confirm_view(a, l))
    } else if s.palette.is_some() {
        window.child(palette_view(s, l))
    } else if s.jump.is_some() {
        window.child(jump_view(s, l))
    } else if s.pick.is_some() {
        window.child(pick_view(s, l))
    } else {
        window
    }
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
        col = col.child(hr(l)).child(tabbar(p, l, c.w));
    }
    if let Some(search) = &f.search {
        col = col.child(hr(l)).child(search_bar(f, search, c));
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
    // As their keys do: in a search, Back leaves it and Up waits until it has.
    let nav = |label: &'static str, key: &'static str, on: bool, cmd: Cmd| {
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
                if in_pane(s, id, tx) && s.unavailable(cmd).is_none() {
                    s.run(cmd, tx);
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
        .child(nav("‹", "nav-back", !f.back.is_empty() || f.searching(), Cmd::Back))
        .child(nav("›", "nav-fwd", !f.fwd.is_empty(), Cmd::Forward))
        .child(nav("↑", "nav-up", f.cwd.parent().is_some() && !f.searching(), Cmd::Parent))
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
    let parts: Vec<Part> = path
        .components()
        .filter_map(|c| match c {
            Component::Prefix(p) => Some(Part::Prefix(p.as_os_str())),
            Component::RootDir => Some(Part::Root),
            Component::Normal(n) => Some(Part::Normal(n)),
            Component::CurDir | Component::ParentDir => None,
        })
        .collect();
    crumbs_from(&parts, std::path::MAIN_SEPARATOR_STR)
}

/// A path component, as [`crumbs_from`] needs it.
#[derive(Clone, Copy)]
enum Part<'a> {
    /// A Windows drive (`C:`) or share (`\\server\share`).
    Prefix(&'a std::ffi::OsStr),
    Root,
    Normal(&'a std::ffi::OsStr),
}

/// The breadcrumbs of a path's parts, joined with `sep`. A drive or share and the root after it
/// are one crumb, which opens the drive's root (`C:\`), not the drive-relative `C:` (the
/// current folder on that drive). Pure path logic, so it's tested for Windows paths everywhere.
fn crumbs_from(parts: &[Part], sep: &str) -> Vec<Crumb> {
    let mut acc = std::ffi::OsString::new();
    let mut out: Vec<Crumb> = Vec::new();
    let mut prev: Option<Part> = None;
    for (i, &part) in parts.iter().enumerate() {
        let label = match part {
            Part::Prefix(p) => {
                acc.push(p);
                if matches!(parts.get(i + 1), Some(Part::Root)) {
                    acc.push(sep);
                }
                p.to_string_lossy().into_owned()
            }
            // After a drive, its root is already in the drive's crumb.
            Part::Root if matches!(prev, Some(Part::Prefix(_))) => {
                prev = Some(part);
                continue;
            }
            Part::Root => {
                acc.push(sep);
                sep.to_string()
            }
            Part::Normal(n) => {
                if !acc.is_empty() && !acc.to_string_lossy().ends_with(sep) {
                    acc.push(sep);
                }
                acc.push(n);
                n.to_string_lossy().into_owned()
            }
        };
        let sep = matches!(prev, Some(Part::Normal(_)));
        out.push(Crumb { label, target: PathBuf::from(&acc), sep });
        prev = Some(part);
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

/// The narrowest a tab gets before the strip shows fewer tabs, the gap between tabs, and the
/// width of a "‹ 3" / "5 ›" marker for the tabs that don't fit.
const TAB_MIN: f32 = 64.0;
const TAB_GAP: f32 = 4.0;
const TAB_MORE: f32 = 34.0;
/// What the tab bar spends besides tabs: its padding, and "+" with the gap before it.
const TABBAR_CHROME: f32 = 2.0 * 6.0 + 22.0 + TAB_GAP;

/// Which of `n` tabs the strip shows in `avail` points: all of them while each can be at least
/// [`TAB_MIN`] wide, else as many as fit beside the two markers, centred on the `active` one
/// (which is always shown).
fn tab_window(n: usize, active: usize, avail: f32) -> std::ops::Range<usize> {
    let fit = |w: f32| ((w + TAB_GAP) / (TAB_MIN + TAB_GAP)).floor().max(1.0) as usize;
    if n <= fit(avail) {
        return 0..n;
    }
    let slots = fit(avail - 2.0 * (TAB_MORE + TAB_GAP)).min(n);
    let start = active.saturating_sub((slots - 1) / 2).min(n - slots);
    start..start + slots
}

/// The pane's tabs, only when it has two or more: each shows its folder's name (the whole
/// path in a tooltip). A click shows it, a middle click closes it, "+" opens a new one. When
/// they don't all fit, the strip shows the ones around the active tab, with "‹ 3" and "5 ›"
/// for how many are hidden on each side (a click shows the nearest of them); "+" stays.
fn tabbar(p: &Pane, l: Look, width: f32) -> El {
    let id = p.id();
    let mut bar = row().key("tabs").w_full().h(TABS_H).px(6.0).gap(TAB_GAP).center_y();
    let n = p.tabs.len();
    let shown = tab_window(n, p.tab, width - TABBAR_CHROME);
    let more = |label: String, key: &'static str, to: usize, hidden: usize| {
        text(label)
            .key(key)
            .font(TAB)
            .color(l.t.text_secondary)
            .w(TAB_MORE)
            .h(22.0)
            .align(Align::Center)
            .radius(5.0)
            .hover(l.t.hover)
            .cursor(Cursor::Pointer)
            .help(format!("{hidden} more tab{}", if hidden == 1 { "" } else { "s" }))
            .on_press_tx(move |s: &mut State, tx| {
                tx.instant();
                let Some(ix) = s.pane_ix(id) else { return };
                s.activate(ix, tx);
                s.show_tab(ix, to, tx);
            })
    };
    if shown.start > 0 {
        bar = bar.child(more(format!("‹ {}", shown.start), "tabs-before", shown.start - 1, shown.start));
    }
    let after = n - shown.end;
    for j in shown.clone() {
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
    if after > 0 {
        bar = bar.child(more(format!("{after} ›"), "tabs-after", shown.end, after));
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

/// A search's query field, over its results.
fn search_bar(f: &Folder, s: &Search, c: Ctx) -> El {
    let (id, l) = (c.id, c.l);
    let field = text_input(&s.edit, BODY)
        .key("search-field")
        .single_line()
        .placeholder(format!("Search names in {}", tab_label(&f.cwd)), l.t.text_secondary.alpha(0.7))
        .color(l.t.text)
        .caret(l.accent, l.accent.alpha(0.35))
        .on_edit(move |s: &mut State| s.search_edit(id))
        .on_input_change(move |s: &mut State, tx| s.search_changed(id, tx))
        .on_input_key(move |s: &mut State, ev, tx| s.search_field_key(id, ev, tx))
        .grow(1.0)
        .min_w(0.0);
    let state = if s.walking() {
        "Searching…"
    } else if s.stopped {
        "Stopped"
    } else {
        ""
    };
    row()
        .key("search")
        .w_full()
        .h(SEARCH_H)
        .px(12.0)
        .gap(10.0)
        .center_y()
        .child(text("Search").font(SMALL).color(l.accent))
        .child(field)
        .child(text(state).font(SMALL).color(l.t.text_secondary))
}

/// The search bar's height.
const SEARCH_H: f32 = 32.0;
/// How much of the Name column's room a search's Folder column gets (names matter more).
const FOLDER_GROW: f32 = 0.6;

fn header(f: &Folder, c: Ctx) -> El {
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
        h = h.child(text("Folder").key("head-folder").font(SMALL).color(l.t.text_secondary).grow(FOLDER_GROW).min_w(0.0));
    } else if c.cols.kind {
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
    if f.searching() {
        // Where it is: its folder below the search's root (the root itself shows as "—").
        let at = std::path::Path::new(&e.os).parent().filter(|p| !p.as_os_str().is_empty());
        let at = at.map_or_else(|| "—".to_string(), |p| p.display().to_string());
        r = r.child(text(at).font(BODY).color(dim).truncate_head().grow(FOLDER_GROW).min_w(0.0));
    } else if cols.kind {
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
    if let Some(line) = State::search_status(f) {
        let picked = if f.marked.len() > 1 { format!("{} selected · ", f.marked.len()) } else { String::new() };
        let hint = if s.show_hidden { "hidden shown" } else { "" };
        return bar
            .child(text(hint).font(SMALL).color(l.t.text_secondary.alpha(0.7)))
            .child(text(format!("{picked}{line}")).font(NUM).color(l.t.text_secondary).truncate().min_w(0.0).shrink());
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

/// The docked preview's width bounds (one pane).
const PREVIEW_MIN_W: f32 = 240.0;
const PREVIEW_MAX_W: f32 = 640.0;
/// A text preview's line height and side padding.
const LINE_H: f32 = 17.0;
const LINE_PAD: f32 = 12.0;

/// The quick preview panel: the item's name and what the listing knows about it at once, then
/// its contents when the worker has them (see `preview.rs`). `width`: docked beside one pane;
/// `None` takes the other pane's place.
fn preview_panel(s: &State, p: &Preview, l: Look, cx: &mut Cx<State>, width: Option<f32>) -> El {
    let mut panel = col().key("preview").h_full().min_w(0.0).fill(l.t.bg);
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
        Body::Image(img) => meta.push(format!("{} × {} {}", img.width, img.height, img.format.name())),
        _ => {}
    }
    if !t.is_dir {
        meta.push(fs::fmt_size(t.size));
    }
    meta.push(fs::fmt_time(t.mtime_ms, s.cur().now));
    let mut title = row()
        .gap(8.0)
        .center_y()
        .child(text(t.name.clone()).font(TextStyle::new(13.0, 600.0)).color(l.t.text).truncate_middle().grow(1.0).min_w(0.0));
    if p.is_loading() {
        title = title.child(text("Loading…").font(SMALL).color(l.t.text_secondary));
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
            body.center().child(el().key("preview-image").w((iw * k).floor().max(1.0)).h((ih * k).floor().max(1.0)).pixels(&p.pixels))
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
            // A new file starts at the top.
            b.child(col().key(format!("preview-text-{}", s.preview_lands)).grow(1.0).min_h(0.0).w_full().scroll_xy().child(rows))
        }
    };
    panel.child(head).child(hr(l)).child(body)
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

/// The layer an overlay box sits in: over the top of the window, centred.
fn overlay_layer(key: &'static str, panel: El) -> El {
    col().key(key).pin(Some(0.0), Some(0.0), Some(0.0), None).items_center().px(20.0).pt(BAR_H + 6.0).child(panel)
}

/// An overlay box's frame: a card, modal (a click outside runs `close`).
fn overlay_card(key: &'static str, max_w: f32, l: Look, close: fn(&mut State)) -> El {
    col()
        .key(key)
        .w_full()
        .max_w(max_w)
        .fill(l.t.card)
        .border(1.0, l.t.card_border)
        .radius(10.0)
        .shadow(10.0, 30.0, l.t.shadow)
        .modal(true, close)
}

/// Jump to Folder: a field, then the folders ranked for it, the matched letters in bold.
fn jump_view(s: &State, l: Look) -> El {
    let Some(j) = &s.jump else { return el() };
    let field = text_input(&j.edit, TextStyle::new(15.0, 400.0))
        .key("jump-field")
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
    let msg = |m: &str| col().key("jump-list").child(text(m.to_string()).font(BODY).color(l.t.text_secondary).px(16.0).py(10.0));
    let list = if count == 0 && j.loading {
        msg("Loading history…")
    } else if count == 0 && s.history.agg.as_ref().is_none_or(|a| a.is_empty()) {
        msg("No folders yet: the folders you open show up here. Type a path to go there.")
    } else if count == 0 {
        msg("No folders match")
    } else {
        // Only the rows in view are built.
        let rows = col()
            .key("jump-rows")
            .w_full()
            .px(6.0)
            .growth(GrowthSpec { spring: rune::GROWTH, edge: rune::edge::NO_MASK })
            .virtual_rows(
                count,
                PALETTE_ROW_H,
                |_: &State, i| Cow::Owned(crate::jump::row_key(i)),
                move |s: &State, i| match s.jump.as_ref().and_then(|j| Some((j.hits.get(i)?, &j.words))) {
                    Some((h, words)) => jump_row(h, words, i, i == sel, path_mode && i == 0, l),
                    None => el(),
                },
            )
            // Keys are row numbers, the same whatever the rows hold.
            .virtual_rows_version(0u8);
        let h = (count as f32).min(PALETTE_ROWS) * PALETTE_ROW_H + 8.0;
        col().key("jump-list").w_full().h(h).py(4.0).scroll(ScrollSpec { stick_bottom: false }).child(rows)
    };
    let panel = overlay_card("jump", 640.0, l, |s: &mut State| s.jump = None)
        .child(row().h(44.0).px(14.0).center_y().child(field))
        .child(hr(l))
        .child(list);
    overlay_layer("jump-layer", panel)
}

/// One Jump row: the folder as `~/…`, matched letters bold (the start is cut when it's long:
/// the folder's own name matters most). In go-to mode, the first row is the path typed.
fn jump_row(h: &crate::jump::Hit, words: &[Vec<char>], i: usize, on: bool, typed: bool, l: Look) -> El {
    let (fg, dim, mark) = if on { (l.sel_text, l.sel_text.alpha(0.75), l.sel_text) } else { (l.t.text, l.t.text_secondary, l.accent) };
    let spans: Vec<rune::text::Span> = h.marks(words).iter().map(|&(a, b)| rune::text::Span::new(a, b).bold().color(mark)).collect();
    let mut r = row()
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
    r = r.child(text(h.shown.to_string()).spans(spans).font(BODY).color(fg).truncate_head().grow(1.0).min_w(0.0));
    if on { r.fill(l.accent) } else { r.hover(l.t.hover) }
}

/// Arbor's one dialog: before something that can't be undone (`confirm.rs`). A title, what it
/// does ("… This can't be undone."), Cancel and the destructive button; the focused one has a
/// ring, and Cancel has the focus when it opens. Modal: a click outside cancels.
fn confirm_view(a: &crate::confirm::Ask, l: Look) -> El {
    use crate::confirm::Button;
    let red = rune::oklch(0.60, 0.19, 27.0);
    let button = |label: &'static str, key: &'static str, which: Button| {
        let focused = a.focus == which;
        let yes = which == Button::Action;
        let b = text(label)
            .key(key)
            .font(TextStyle::new(13.0, 500.0))
            .color(if yes { Color::WHITE } else { l.t.text })
            .px(14.0)
            .h(28.0)
            .center_y()
            .radius(6.0)
            .cursor(Cursor::Pointer)
            .on_press_tx(move |s: &mut State, tx| s.answer(yes, tx));
        let b = if yes { b.fill(red) } else { b.fill(l.t.control_off).hover(l.t.hover) };
        if focused { b.border(2.0, l.accent) } else { b }
    };
    // A focusable card that owns its keys: Rune focuses it when the modal opens, and its keys
    // come before Rune's focus navigation, so Tab, ← → and Enter are the dialog's
    // (`State::ask_key`), and the buttons aren't Tab stops of their own.
    let panel = overlay_card("confirm", 420.0, l, |s: &mut State| s.asking = None)
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
                .child(el().grow(1.0))
                .child(button("Cancel", "confirm-cancel", Button::Cancel))
                .child(button(a.action, "confirm-action", Button::Action)),
        );
    overlay_layer("confirm-layer", panel)
}

/// Select by Pattern: a field, the live count, and what Enter, ⇧Enter and ⌥Enter do.
fn pick_view(s: &State, l: Look) -> El {
    let Some(p) = &s.pick else { return el() };
    let field = text_input(&p.edit, TextStyle::new(15.0, 400.0))
        .key("pick-field")
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
        .child(text(count).font(NUM).color(color).truncate().grow(1.0).min_w(0.0))
        .child(text(hint).font(SMALL).color(l.t.text_secondary))
        .child(text(case).font(SMALL).color(l.t.text_secondary.alpha(0.7)).help(if case == "Aa" {
            "Capitals count (select.case_sensitive)"
        } else {
            "Capitals don't count (select.case_sensitive)"
        }));
    let panel = overlay_card("pick", 520.0, l, |s: &mut State| s.pick = None)
        .child(row().h(44.0).px(14.0).center_y().child(field))
        .child(hr(l))
        .child(foot);
    overlay_layer("pick-layer", panel)
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

    /// A drive's crumb opens its root (`C:\`), never the drive-relative `C:`; a share's opens
    /// `\\server\share\`. Pure path logic, so it runs on every platform.
    #[test]
    fn windows_drive_and_share_crumbs_open_their_roots() {
        use std::ffi::OsStr;
        let targets = |parts: &[Part]| crumbs_from(parts, "\\").into_iter().map(|c| (c.label, c.target, c.sep)).collect::<Vec<_>>();
        let p = |s: &str| PathBuf::from(s);
        let c = Part::Prefix(OsStr::new("C:"));
        assert_eq!(targets(&[c, Part::Root]), [("C:".into(), p("C:\\"), false)]);
        assert_eq!(
            targets(&[c, Part::Root, Part::Normal(OsStr::new("Users")), Part::Normal(OsStr::new("x"))]),
            [("C:".into(), p("C:\\"), false), ("Users".into(), p("C:\\Users"), false), ("x".into(), p("C:\\Users\\x"), true)]
        );
        let share = Part::Prefix(OsStr::new("\\\\server\\share"));
        assert_eq!(
            targets(&[share, Part::Root, Part::Normal(OsStr::new("dir"))]),
            [("\\\\server\\share".into(), p("\\\\server\\share\\"), false), ("dir".into(), p("\\\\server\\share\\dir"), false)]
        );
        // Unix, for comparison: the root, then each folder.
        let unix = crumbs_from(&[Part::Root, Part::Normal(OsStr::new("home")), Part::Normal(OsStr::new("smr"))], "/");
        assert_eq!(unix.iter().map(|c| c.target.clone()).collect::<Vec<_>>(), [p("/"), p("/home"), p("/home/smr")]);
    }

    /// The same, through `Path` on Windows itself.
    #[cfg(windows)]
    #[test]
    fn windows_paths_crumb_to_rooted_targets() {
        let t = |s: &str| crumbs_of(Path::new(s)).into_iter().map(|c| c.target).collect::<Vec<_>>();
        assert_eq!(t(r"C:\"), [PathBuf::from(r"C:\")]);
        assert_eq!(t(r"C:\Users\x"), [PathBuf::from(r"C:\"), PathBuf::from(r"C:\Users"), PathBuf::from(r"C:\Users\x")]);
        assert_eq!(t(r"\\server\share\dir"), [PathBuf::from(r"\\server\share\"), PathBuf::from(r"\\server\share\dir")]);
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

    /// Many tabs, or a narrow pane: the active tab is always in the strip, with markers for
    /// the rest; few tabs all show.
    #[test]
    fn the_active_tab_always_shows() {
        let narrow = 235.0 - TABBAR_CHROME; // a pane of a split 640 pt window
        let normal = 811.0 - TABBAR_CHROME; // an unsplit 980 pt window
        assert_eq!(tab_window(3, 2, normal), 0..3);
        assert_eq!(tab_window(8, 0, normal), 0..8);
        for (n, avail) in [(8, narrow), (28, normal), (100, normal), (5, 40.0)] {
            for active in 0..n {
                let w = tab_window(n, active, avail);
                assert!(w.contains(&active), "{n} tabs, {active} active in {avail}: {w:?}");
                assert!(w.len() < n, "{n} in {avail} don't all fit");
                let markers = (w.start > 0) as usize + (w.end < n) as usize;
                let used = w.len() as f32 * (TAB_MIN + TAB_GAP) + markers as f32 * (TAB_MORE + TAB_GAP) - TAB_GAP;
                assert!(used <= avail || w.len() == 1, "{n} tabs, {active} active: {used} of {avail}");
            }
        }
        assert_eq!(tab_window(28, 27, normal).end, 28);
        assert_eq!(tab_window(28, 0, normal).start, 0);
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
