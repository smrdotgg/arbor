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
use crate::preview::{Body, Format, Preview};
use crate::search::Search;

mod batch;
mod grid;
mod list;
mod overlays;
mod pane;
mod panels;
mod sidebar;
#[cfg(test)]
mod tests;

use batch::*;
use list::*;
#[cfg(test)]
pub(crate) use overlays::menu_origin;
use overlays::*;
use pane::*;
use panels::*;
use sidebar::*;

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

impl Ctx {
    /// Search's folder column: its share (`FOLDER_GROW` to the name's 1) of what the fixed
    /// columns, padding and gaps leave, so a long folder can be cut to fit before Rune lays it
    /// out (`fit_path`). `None` before the pane's first layout.
    fn folder_w(&self) -> Option<f32> {
        if self.w == f32::MAX {
            return None;
        }
        let fixed = 24.0 + SIZE_W + 12.0 * 2.0 + if self.cols.date { DATE_W + 12.0 } else { 0.0 };
        Some(((self.w - fixed) * FOLDER_GROW / (1.0 + FOLDER_GROW)).floor().max(24.0))
    }
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

    /// Pane `id`'s rect.
    fn rect(self, id: u64) -> Option<[f32; 4]> {
        [self.left, self.right].into_iter().find(|&(_, i)| i == id).map(|(r, _)| r)
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
    /// The list's viewport as last laid out (window points: x, y, w, h; zero before the first
    /// layout), for drag-select and the grid's columns.
    list: [f32; 4],
    /// How big the icon view's icons are.
    icons: crate::grid::IconSize,
    /// Device pixels per point.
    scale: f32,
}

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    root(s, cx).on_paste_content(|s: &mut State, content, tx| {
        tx.instant();
        s.paste(content);
    })
}

fn root(s: &State, cx: &mut Cx<State>) -> El {
    let l = look(cx.system_dark());
    let scale = cx.env().scale.max(1.0);
    // What the icon view's thumbnails decode to: the icon's side in device pixels.
    s.tile_fit.set((s.icon_size.icon() * scale).round() as u32);
    // Reads laid-out positions and sizes only; scrolling never re-runs the view.
    // With the preview open in a split window, it takes the other pane's place (as a
    // two-panel commander's quick view does).
    // Get Info's panel goes where the preview does (only one is open).
    let panel_open = s.preview.is_some() || s.info.is_some();
    let hidden = |ix: usize| panel_open && ix != s.active;
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
        if split.is_some() && hidden(ix) {
            panes = panes.child(side_panel(s, l, cx, None));
            continue;
        }
        let w = cx.layout_size(&format!("pane-{}", p.id())).map_or(f32::MAX, |[w, _]| w);
        // With the other pane covered by the preview, this one draws as if alone (its rows
        // drag out to other apps; there's no pane to drop on).
        let split = split.filter(|_| !panel_open);
        let list = cx.layout_with(&p.f.list_key(), |r| r).unwrap_or_default();
        let c = Ctx {
            id: p.id(),
            l,
            cols: Cols::for_width(w),
            focused: ix == s.active || split.is_none(),
            split,
            w,
            list,
            icons: s.icon_size,
            scale,
        };
        panes = panes.child(pane(s, p, c));
    }
    // One pane: the preview docks on its right, and the list stays usable beside it.
    if panel_open && split.is_none() {
        let main = cx.layout_size("panes").map_or(980.0 - SIDEBAR_W, |[w, _]| w);
        let w = (main * 0.42).clamp(PREVIEW_MIN_W, PREVIEW_MAX_W).min(main - PREVIEW_MIN_W).round();
        panes = panes.child(el().w(1.0).h_full().fill(l.t.separator)).child(side_panel(s, l, cx, Some(w)));
    }
    let mut window = row()
        .w_full()
        .h_full()
        .fill(l.t.bg)
        .child(sidebar(s, l, cx.titlebar_inset() > 0.0))
        .child(el().w(1.0).h_full().fill(l.t.separator))
        .child(
            col().key("main").grow(1.0).min_w(0.0).h_full().child(panes).child(hr(l)).child(status(s, l)).children(completion_view(s, l)),
        );
    if let Some(band) = band_mark(s, l) {
        window = window.child(band);
    }
    if let Some(a) = &s.asking {
        window.child(confirm_view(a, l))
    } else if let Some(m) = &s.menu {
        // The window's size, from last frame's layout: the sidebar, its separator and `main`.
        let size = cx.layout_size("main").map(|[w, h]| [SIDEBAR_W + 1.0 + w, h]);
        window.child(menu_view(m, size, l))
    } else if s.palette.is_some() {
        window.child(palette_view(s, l))
    } else if s.jump.is_some() {
        // The card's laid-out width (last frame's; at most 640), for cutting long paths.
        let card = cx.layout_size("jump").map_or(640.0, |[w, _]| w);
        window.child(jump_view(s, l, card))
    } else if s.pick.is_some() {
        window.child(pick_view(s, l))
    } else if let Some(b) = &s.batch {
        window.child(batch_view(s, b, l))
    } else if let Some(o) = &s.open_with {
        window.child(open_with_view(o, l))
    } else {
        window
    }
}

fn hr(l: Look) -> El {
    el().w_full().h(1.0).fill(l.t.separator)
}

fn status(s: &State, l: Look) -> El {
    let f = s.cur();
    let mut bar = row().key("status").w_full().h(28.0).px(12.0).gap(10.0).center_y();
    if !f.filter.is_empty() {
        let label = if f.path_mode() { "Go to" } else { "Filter" };
        bar = bar.child(text(label).font(SMALL).color(l.accent).a11y_hidden()).child(
            text(format!("{}▏", f.filter))
                .key("filter")
                .label(format!("{label}: {}", f.filter))
                .font(MONO)
                .color(l.t.text)
                .truncate_head()
                .min_w(0.0)
                .shrink(),
        );
    }
    // Progress isn't announced (it changes many times a second); the result is: a note is a
    // live region, polite, or assertive for an error (`State::alert`).
    if let Some(job) = s.job_status() {
        bar = bar.child(text(job).key("progress").font(NUM).color(l.accent).truncate().min_w(0.0).shrink());
    } else if let Some(n) = &s.note {
        let role = if s.alert.as_ref() == Some(n) { "alert" } else { "status" };
        bar = bar.child(text(n.clone()).key("note").role(role).font(SMALL).color(l.t.text_secondary).truncate().min_w(0.0).shrink());
    }
    bar = bar.child(el().grow(1.0));
    if f.is_loading() {
        bar = bar.child(text("Loading…").font(SMALL).color(l.t.text_secondary));
    }
    if let Some(line) = State::search_status(f) {
        let picked = if f.marked.len() > 1 { format!("{} selected · ", f.marked.len()) } else { String::new() };
        let hint = if s.show_hidden { "hidden shown" } else { "" };
        return bar
            .child(text(hint).font(SMALL).color(l.t.text_secondary.alpha(0.7)).a11y_hidden())
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
    bar.child(text(hint).font(SMALL).color(l.t.text_secondary.alpha(0.7)).a11y_hidden())
        .child(text(format!("{picked}{count}")).font(NUM).color(l.t.text_secondary))
}

/// What a long path shows from its start when cut: an ellipsis and a separator (the path's
/// own, `/` or `\\`, the same length).
const CUT: &str = "…/";

/// `path` cut to its trailing folders that fit in `room` points of `BODY` text, after "…/", and
/// how many of its bytes were cut from the front (0: it fits whole). Widths are estimated from
/// per-character advances (see [`advance`]), with a little room to spare, so the result almost
/// always fits and Rune's own head truncation, kept as the fallback, almost never runs.
pub(crate) fn fit_path(path: &str, room: f32) -> (Cow<'_, str>, usize) {
    fit_path_with(path, room, &|c| char_advance(c, BODY))
}

fn fit_path_with<'a>(path: &'a str, room: f32, adv: &dyn Fn(char) -> f32) -> (Cow<'a, str>, usize) {
    // Kerning and rounding can make a string a little wider than its advances add up to.
    let room = room * 0.97;
    let width = |s: &str| s.chars().map(adv).sum::<f32>();
    if width(path) <= room {
        return (Cow::Borrowed(path), 0);
    }
    let budget = room - width(CUT);
    // From the end, the widest run of whole trailing folders that fits; at least the last one.
    let (mut w, mut cut) = (0.0, None);
    for (i, c) in path.char_indices().rev() {
        if matches!(c, '/' | '\\') && i + 1 < path.len() {
            if w > budget && cut.is_some() {
                break;
            }
            cut = Some(i + 1);
        }
        w += adv(c);
    }
    match cut {
        // "…/" or "…\\", as the path writes its separators.
        Some(at) if at > 0 => (Cow::Owned(format!("…{}{}", &path[at - 1..at], &path[at..])), at),
        _ => (Cow::Borrowed(path), 0),
    }
}

/// Byte ranges into a path moved to where they are once its first `cut` bytes became "…/"
/// (ranges in the cut part go; one across the cut keeps its visible part).
fn shift_marks(marks: &[(usize, usize)], cut: usize) -> Vec<(usize, usize)> {
    if cut == 0 {
        return marks.to_vec();
    }
    marks.iter().filter(|&&(_, b)| b > cut).map(|&(a, b)| (a.max(cut) - cut + CUT.len(), b - cut + CUT.len())).collect()
}

/// A character's advance in `style`, in points: printable ASCII from a table measured once per
/// thread (a property of the font, as the text preview's monospaced width is), anything else
/// as the widest of them, which keeps estimates on the safe side.
fn char_advance(c: char, style: TextStyle) -> f32 {
    thread_local! {
        static TABLE: std::cell::RefCell<Vec<(TextStyle, [f32; 96])>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    TABLE.with_borrow_mut(|t| {
        let i = match t.iter().position(|(s, _)| *s == style) {
            Some(i) => i,
            None => {
                let mut a = [0.0; 96];
                for (k, ch) in (' '..='~').enumerate() {
                    a[k] = rune::text::advance(&ch.to_string(), style);
                }
                a[95] = a[..95].iter().copied().fold(0.0, f32::max);
                t.push((style, a));
                t.len() - 1
            }
        };
        let a = &t[i].1;
        if (' '..='~').contains(&c) { a[c as usize - ' ' as usize] } else { a[95] }
    })
}

/// The width of `s` in `style` from [`char_advance`].
fn advance(s: &str, style: TextStyle) -> f32 {
    s.chars().map(|c| char_advance(c, style)).sum()
}
