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
        let c = Ctx { id: p.id(), l, cols: Cols::for_width(w), focused: ix == s.active || split.is_none(), split, w };
        panes = panes.child(pane(s, p, c));
    }
    // One pane: the preview docks on its right, and the list stays usable beside it.
    if panel_open && split.is_none() {
        let main = cx.layout_size("panes").map_or(980.0 - SIDEBAR_W, |[w, _]| w);
        let w = (main * 0.42).clamp(PREVIEW_MIN_W, PREVIEW_MAX_W).min(main - PREVIEW_MIN_W).round();
        panes = panes.child(el().w(1.0).h_full().fill(l.t.separator)).child(side_panel(s, l, cx, Some(w)));
    }
    let window = row()
        .w_full()
        .h_full()
        .fill(l.t.bg)
        .child(sidebar(s, l, cx.titlebar_inset() > 0.0))
        .child(el().w(1.0).h_full().fill(l.t.separator))
        .child(
            col().key("main").grow(1.0).min_w(0.0).h_full().child(panes).child(hr(l)).child(status(s, l)).children(completion_view(s, l)),
        );
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

fn sidebar(s: &State, l: Look, under_titlebar: bool) -> El {
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
fn place_row(s: &State, i: usize, l: Look) -> El {
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
        let name = s.volumes[v].name.clone();
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
                    s.eject(v);
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

/// One pane. A click anywhere in it makes it the active one; every handler inside does that
/// first, so the command acts on the pane that was clicked.
fn pane(s: &State, p: &Pane, c: Ctx) -> El {
    let (id, l, f) = (c.id, c.l, &p.f);
    // A region, not the button its press handler would make it.
    let region = match (c.split.is_some(), s.panes.first().is_some_and(|p| p.id() == id)) {
        (false, _) => "Files",
        (true, true) => "Left pane",
        (true, false) => "Right pane",
    };
    let mut col = col()
        .key(format!("pane-{id}"))
        .role("region")
        .label(region)
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
    col = col.child(hr(l)).child(header(f, c)).child(hr(l)).child(list(f, c, active_row(s)));
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
            .role("button")
            .label(cmd.title())
            .disabled(!on)
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
    let mut crumbs = row().key("crumbs").role("navigation").label("Path").grow(1.0).min_w(0.0).center_y().gap(CRUMB_GAP);
    let all = crumbs_of(&f.cwd);
    let n = all.len();
    let sep = || text("›").font(CRUMB).color(l.t.text_secondary.alpha(0.5)).a11y_hidden();
    let crumb = |label: String, key: String, target: PathBuf, last: bool| {
        text(label)
            .key(key)
            .role("link")
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
        crumbs = crumbs
            .child(crumb("…".into(), "crumb-more".into(), up.clone(), false).label("Enclosing folders").help(fs::pretty(&up)))
            .child(sep());
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
        .role("toolbar")
        .label("Navigation")
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
    let mut bar = row().key("tabs").role("tablist").label("Tabs").w_full().h(TABS_H).px(6.0).gap(TAB_GAP).center_y();
    let n = p.tabs.len();
    let shown = tab_window(n, p.tab, width - TABBAR_CHROME);
    let more = |label: String, key: &'static str, to: usize, hidden: usize| {
        text(label)
            .key(key)
            .role("button")
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
            .role("tab")
            .focus_selected(on)
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
            .role("button")
            .label("New Tab")
            .description("")
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
        .role("searchbox")
        .label("Search names")
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
        .child(text("Search").font(SMALL).color(l.accent).a11y_hidden())
        .child(field)
        .child(text(state).font(SMALL).color(l.t.text_secondary).role("status"))
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
fn active_row(s: &State) -> Option<String> {
    let f = s.cur();
    f.selected().map(|e| format!("{}/{}", f.rows_key(), e.key))
}

fn list(f: &Folder, c: Ctx, active: Option<String>) -> El {
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
                .drag_out(move |s: &State| if s.pane(id)?.f.version == version { s.drag_item(id, i) } else { None })
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
fn folder_mark(glyph: El, say: bool) -> El {
    if say { glyph.label("Folder") } else { glyph.a11y_hidden() }
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

/// Candidates the completion card lists at most; the rest are counted.
const COMPLETE_ROWS: usize = 8;
const COMPLETE_ROW_H: f32 = 22.0;

/// Go-to's Tab completion (`complete.rs`): the candidates in a small card over the status bar,
/// the one in the field highlighted. Not modal: typing goes on in the field, and any other key
/// ends it. Lists the eight around the current one.
fn completion_view(s: &State, l: Look) -> Option<El> {
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

/// The docked preview's width bounds (one pane).
const PREVIEW_MIN_W: f32 = 240.0;
const PREVIEW_MAX_W: f32 = 640.0;
/// A text preview's line height and side padding.
const LINE_H: f32 = 17.0;
const LINE_PAD: f32 = 12.0;

/// The quick preview panel: the item's name and what the listing knows about it at once, then
/// its contents when the worker has them (see `preview.rs`). `width`: docked beside one pane;
/// `None` takes the other pane's place.
/// The panel beside the list: the quick preview, or Get Info.
fn side_panel(s: &State, l: Look, cx: &mut Cx<State>, width: Option<f32>) -> El {
    match (&s.preview, &s.info) {
        (Some(p), _) => preview_panel(s, p, l, cx, width),
        (None, Some(i)) => info_panel(s, i, l, width),
        (None, None) => el(),
    }
}

/// Get Info (`info.rs`): the item's name, then what the disk says about it, a row each, and
/// for a folder or several items the total as it streams in. Esc or ⌘I closes it.
fn info_panel(s: &State, i: &crate::info::Info, l: Look, width: Option<f32>) -> El {
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

/// Open With (`openwith.rs`): the apps for the selection, the default first; ↑↓ and Enter, or a
/// click. Modal: Esc or a click outside closes it.
fn open_with_view(o: &crate::openwith::OpenWithBox, l: Look) -> El {
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

fn preview_panel(s: &State, p: &Preview, l: Look, cx: &mut Cx<State>, width: Option<f32>) -> El {
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
fn preview_scroll_key(key: &str, at: [f32; 2], ev: &rune::KeyEvent, tx: &mut rune::Tx) -> bool {
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
fn overlay_layer(key: &'static str, panel: El) -> El {
    col().key(key).pin(Some(0.0), Some(0.0), Some(0.0), None).items_center().px(20.0).pt(BAR_H + 6.0).child(panel)
}

/// An overlay card's shadow: offset and blur. Kept small: the area a shadow covers is drawn
/// again whenever the card's contents change (each key in Batch Rename or the palette), and a
/// blur of 30 points cost 1.7 ms a key there in software rendering (BENCH.md).
const OVERLAY_SHADOW: (f32, f32) = (4.0, 10.0);

/// An overlay box's frame: a card, modal (a click outside runs `close`).
fn overlay_card(key: &'static str, name: &'static str, max_w: f32, l: Look, close: fn(&mut State)) -> El {
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
fn jump_view(s: &State, l: Look, card_w: f32) -> El {
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
    } else if count == 0 && s.history.agg.as_ref().is_none_or(|a| a.is_empty()) {
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
fn jump_row(h: &crate::jump::Hit, words: &[Vec<char>], i: usize, on: bool, typed: bool, card_w: f32, l: Look) -> El {
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

const MENU_W: f32 = 250.0;
const MENU_ROW_H: f32 = 24.0;
const MENU_SEP_H: f32 = 9.0;
const MENU_PAD: f32 = 5.0;

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
fn menu_view(m: &crate::menu::Menu, size: Option<[f32; 2]>, l: Look) -> El {
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
            Item::Act { title, on, keys, .. } => {
                let hi = m.sel == Some(i);
                let (fg, dim) = match (hi, *on) {
                    (true, _) => (l.sel_text, l.sel_text.alpha(0.8)),
                    (false, true) => (l.t.text, l.t.text_secondary),
                    (false, false) => (l.t.text_secondary.alpha(0.55), l.t.text_secondary.alpha(0.45)),
                };
                let r = row()
                    .key(format!("menu-{i}"))
                    .role("menuitem")
                    .label(title.clone())
                    .disabled(!*on)
                    .shortcut_hint(keys.clone())
                    .w_full()
                    .h(MENU_ROW_H)
                    .px(10.0)
                    .gap(16.0)
                    .center_y()
                    .radius(4.0)
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
/// ring, and Cancel has the focus when it opens. Modal: a click outside cancels.
fn confirm_view(a: &crate::confirm::Ask, l: Look) -> El {
    use crate::confirm::Button;
    let button = |label: &'static str, key: &'static str, which: Button| {
        let focused = a.focus == which;
        let yes = which == Button::Action;
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
            .on_press_tx(move |s: &mut State, tx| s.answer(yes, tx))
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
        .active_descendant(if a.focus == Button::Action { "confirm-action" } else { "confirm-cancel" })
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

/// Rows Batch Rename's preview shows before it scrolls.
const BATCH_ROWS: f32 = 12.0;
/// The width of Batch Rename's labels ("Find", "Replace", "Number").
const BATCH_LABEL_W: f32 = 58.0;
/// Problems in Batch Rename, and the confirm dialog's destructive button.
fn red() -> Color {
    rune::oklch(0.60, 0.19, 27.0)
}

/// Batch Rename (`batch.rs`): the rule's fields, a live preview of every item's new name with
/// its problems, and the summary with Rename. Modal: a click outside closes it (its rule is
/// kept for next time).
fn batch_view(s: &State, b: &crate::batch::BatchBox, l: Look) -> El {
    use crate::batch::Field;
    let field = |f: Field, edit: &rune::TextEdit, key: &'static str, name: &'static str, hint: &str, w: Option<f32>| {
        let input = text_input(edit, BODY)
            .key(key)
            .label(name)
            .single_line()
            .placeholder(hint.to_string(), l.t.text_secondary.alpha(0.6))
            .color(l.t.text)
            .caret(l.accent, l.accent.alpha(0.35))
            .on_edit(move |s: &mut State| s.batch_edit(f))
            .on_input_change(|s: &mut State, tx| {
                tx.instant();
                s.batch_replan();
            })
            .on_input_key(|s: &mut State, ev, tx| s.batch_field_key(ev, tx))
            .grow(1.0)
            .min_w(0.0);
        let r = row()
            .h(28.0)
            .px(8.0)
            .center_y()
            .radius(5.0)
            .fill(l.t.bg)
            .border(1.0, if edit.focused { l.accent } else { l.t.card_border })
            .child(input);
        match w {
            Some(w) => r.w(w),
            None => r.grow(1.0).min_w(0.0),
        }
    };
    // The fields carry their own names; these labels would only say them again.
    let label = |t: &'static str| text(t).font(SMALL).color(l.t.text_secondary).w(BATCH_LABEL_W).a11y_hidden();
    let toggle =
        |t: &'static str, key: &'static str, name: &'static str, on: bool, help: &'static str, flip: fn(&mut crate::batch::BatchBox)| {
            let r = row()
                .key(key)
                .role("switch")
                .label(name)
                .checked(on)
                .h(26.0)
                .px(8.0)
                .center()
                .radius(5.0)
                .cursor(Cursor::Pointer)
                .help(help)
                .on_press_tx(move |s: &mut State, tx| {
                    tx.instant();
                    s.batch_set(flip);
                })
                .child(text(t).font(TextStyle::new(12.0, 600.0)).color(if on { Color::WHITE } else { l.t.text_secondary }));
            if on { r.fill(l.accent) } else { r.fill(l.t.control_off).hover(l.t.hover) }
        };
    let find = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(label("Find"))
        .child(field(
            Field::Find,
            &b.find,
            "batch-find",
            "Find",
            if b.regex { "Regular expression, e.g. ^IMG_(\\d+)" } else { "Text to find (empty: replace the whole name)" },
            None,
        ))
        .child(toggle(".*", "batch-regex", "Regular expression", b.regex, "Regular expression (⌥R)", |b| b.regex = !b.regex))
        .child(toggle("Aa", "batch-case", "Match capitals", b.case_sensitive, "Match capitals (⌥C)", |b| {
            b.case_sensitive = !b.case_sensitive
        }));
    let replace = row().w_full().gap(8.0).center_y().child(label("Replace")).child(field(
        Field::Replace,
        &b.replace,
        "batch-replace",
        "Replace with",
        if b.regex { "With: text, $1, {n}, {n:3}, {name}, {ext}, {parent}" } else { "With: text, {n}, {n:3}, {name}, {ext}, {parent}" },
        None,
    ));
    let mut cases = row().key("batch-cases").role("radiogroup").label("Case").gap(2.0).px(2.0).py(2.0).radius(6.0).fill(l.t.control_off);
    for c in crate::rename::CaseMode::ALL {
        let on = b.case == c;
        let pill = row()
            .key(format!("case-{}", c.label()))
            .role("radio")
            .focus_selected(on)
            .h(22.0)
            .px(8.0)
            .center()
            .radius(4.0)
            .cursor(Cursor::Pointer)
            .on_press_tx(move |s: &mut State, tx| {
                tx.instant();
                s.batch_set(|b| b.case = c);
            })
            .child(text(c.label()).font(SMALL).color(if on { l.t.text } else { l.t.text_secondary }));
        cases = cases.child(if on { pill.fill(l.t.card).border(1.0, l.t.card_border) } else { pill.hover(l.t.hover) });
    }
    let ext_on = b.include_ext;
    let check = el().w(14.0).h(14.0).radius(3.0).center().border(1.5, if ext_on { l.accent } else { l.t.text_secondary.alpha(0.6) });
    let check = if ext_on { check.fill(l.accent).child(text("✓").font(TextStyle::new(10.0, 700.0)).color(Color::WHITE)) } else { check };
    let include_ext = row()
        .key("batch-ext")
        .role("checkbox")
        .label("Include extension")
        .checked(ext_on)
        .gap(6.0)
        .center_y()
        .cursor(Cursor::Pointer)
        .help("Rename the extension too (⌥E); off, only the name before it changes")
        .on_press_tx(|s: &mut State, tx| {
            tx.instant();
            s.batch_set(|b| b.include_ext = !b.include_ext);
        })
        .child(check)
        .child(text("Include extension").font(SMALL).color(l.t.text));
    let number = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(label("Number"))
        .child(text("Start").font(SMALL).color(l.t.text_secondary).a11y_hidden())
        .child(field(Field::Start, &b.start, "batch-start", "Number start", "1", Some(64.0)))
        .child(text("Step").font(SMALL).color(l.t.text_secondary).a11y_hidden())
        .child(field(Field::Step, &b.step, "batch-step", "Number step", "1", Some(52.0)))
        .child(el().w(10.0))
        .child(text("Case").font(SMALL).color(l.t.text_secondary).a11y_hidden())
        .child(cases)
        .child(el().grow(1.0))
        .child(include_ext);
    let fields = col().key("batch-fields").w_full().px(16.0).py(12.0).gap(8.0).child(find).child(replace).child(number);
    // The preview: only the rows in view are built.
    let count = b.items.len();
    let rows = col()
        .key("batch-rows")
        .role("list")
        .label("New names")
        .w_full()
        .growth(GrowthSpec { spring: rune::GROWTH, edge: rune::edge::NO_MASK })
        .virtual_rows(
            count,
            ROW_H,
            |_: &State, i| Cow::Owned(format!("b{i}")),
            move |s: &State, i| match &s.batch {
                Some(b) => batch_row(b, i, l),
                None => el(),
            },
        )
        // Keys are row numbers, the same whatever the rows hold.
        .virtual_rows_version(0u8);
    let head = row()
        .key("batch-head")
        .w_full()
        .h(24.0)
        .px(16.0)
        .gap(12.0)
        .center_y()
        .child(
            text(format!("{} item{}", crate::search::count(count as u64), if count == 1 { "" } else { "s" }))
                .font(SMALL)
                .color(l.t.text_secondary)
                .grow(1.0)
                .min_w(0.0),
        )
        .child(el().w(14.0))
        .child(text("New name").font(SMALL).color(l.t.text_secondary).grow(1.0).min_w(0.0));
    let h = (count as f32).min(BATCH_ROWS) * ROW_H;
    let list = col().key("batch-list").w_full().h(h).scroll(ScrollSpec { stick_bottom: false }).child(rows);
    // The summary, and Rename.
    let blocked = b.blocked().or_else(|| s.job.is_some().then(|| crate::app::BUSY.to_string()));
    let (summary, color) = match &b.error {
        Some(e) => (e.clone(), red()),
        None if b.plan.problems > 0 => (b.plan.summary(), red()),
        None if b.plan.changes > 0 => (b.plan.summary(), l.accent),
        None => (b.plan.summary(), l.t.text_secondary),
    };
    let summary = if b.stale() || !b.known() { format!("{summary} …") } else { summary };
    let button = |label: String, key: &'static str, yes: bool, on: bool| {
        let r = row()
            .key(key)
            .role("button")
            .disabled(!on)
            .h(28.0)
            .px(14.0)
            .center_y()
            .radius(6.0)
            .cursor(if on { Cursor::Pointer } else { Cursor::Default })
            .on_press_tx(move |s: &mut State, tx| {
                tx.instant();
                if yes { s.batch_apply(tx) } else { s.close_batch(tx) }
            })
            .child(text(label).font(TextStyle::new(13.0, 500.0)).color(if yes && on {
                Color::WHITE
            } else if on {
                l.t.text
            } else {
                l.t.text_secondary.alpha(0.6)
            }));
        match (yes, on) {
            (true, true) => r.fill(l.accent),
            (_, _) => r.fill(l.t.control_off).hover(l.t.hover),
        }
    };
    let key = |spec: &str| crate::commands::parse_chord(spec).map(|c| c.label()).unwrap_or_default();
    let rename_label = if b.plan.changes > 0 && blocked.is_none() {
        format!("Rename {}", crate::search::count(b.plan.changes as u64))
    } else {
        "Rename".into()
    };
    let foot = row()
        .key("batch-foot")
        .w_full()
        .px(16.0)
        .py(10.0)
        .gap(10.0)
        .center_y()
        .child(text(summary).key("batch-summary").role("status").font(NUM).color(color).truncate().grow(1.0).min_w(0.0))
        .child(
            text(match &blocked {
                Some(why) if b.error.is_none() && b.plan.problems == 0 && b.plan.changes > 0 => why.clone(),
                _ => format!("{} Rename   {} Cancel", key("enter"), key("escape")),
            })
            .font(SMALL)
            .color(l.t.text_secondary),
        )
        .child(button("Cancel".into(), "batch-cancel", false, true))
        .child(button(rename_label, "batch-apply", true, blocked.is_none()));
    let title = row()
        .w_full()
        .px(16.0)
        .pt(12.0)
        .child(text("Batch Rename").role("heading").level(1).font(TextStyle::new(14.0, 600.0)).color(l.t.text));
    let panel = overlay_card("batch", "Batch Rename", 820.0, l, |s: &mut State| {
        if let Some(b) = s.batch.take() {
            s.batch_rule = b.rule();
        }
    })
    .child(title)
    .child(fields)
    .child(hr(l))
    .child(head)
    .child(hr(l))
    .child(list)
    .child(hr(l))
    .child(foot);
    overlay_layer("batch-layer", panel)
}

/// One preview row: the old name, and the new one. Unchanged rows are dim; a problem shows in
/// red with its reason.
fn batch_row(b: &crate::batch::BatchBox, i: usize, l: Look) -> El {
    use crate::rename::Status;
    let Some(item) = b.items.get(i) else { return el() };
    let new = b.plan.new.get(i).map_or(item.name.as_str(), String::as_str);
    let status = b.plan.status.get(i).cloned().unwrap_or(Status::Same);
    let dim = l.t.text_secondary.alpha(0.55);
    let (old_c, new_c) = match status {
        Status::Same | Status::Kept(_) => (dim, dim),
        Status::Change => (l.t.text_secondary, l.t.text),
        Status::Problem(_) => (l.t.text_secondary, red()),
    };
    let mut new_col = row()
        .grow(1.0)
        .min_w(0.0)
        .gap(8.0)
        .center_y()
        .child(text(new.to_string()).font(BODY).color(new_c).truncate_middle().shrink().min_w(0.0));
    match &status {
        Status::Problem(why) => new_col = new_col.child(text(why.clone()).font(SMALL).color(red()).truncate().grow(1.0).min_w(0.0)),
        Status::Kept(why) => new_col = new_col.child(text(why.to_string()).font(SMALL).color(dim).truncate().grow(1.0).min_w(0.0)),
        _ => {}
    }
    let r = row()
        .role("listitem")
        .w_full()
        .h(ROW_H)
        .px(16.0)
        .gap(12.0)
        .center_y()
        .help(fs::pretty(&b.paths[i]))
        .child(text(item.name.clone()).font(BODY).color(old_c).truncate_middle().grow(1.0).min_w(0.0))
        .child(text("→").label("becomes").font(BODY).color(dim).w(14.0))
        .child(new_col);
    if matches!(status, Status::Problem(_)) {
        r.fill(red().alpha(0.10))
    } else if i % 2 == 1 {
        r.fill(l.stripe)
    } else {
        r
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A long path is cut to whole trailing folders that fit, after "…/"; one that fits stays
    /// whole; at least the last folder is kept (Rune cuts that one further if it must).
    #[test]
    fn long_paths_are_cut_to_whole_trailing_folders() {
        let one = |_: char| 1.0;
        assert_eq!(fit_path_with("~/a/bb/ccc", 100.0, &one), (Cow::Borrowed("~/a/bb/ccc"), 0));
        // 9.28 × 0.97 = 9 of room, 7 after "…/": "bb/ccc" (6) fits, "a/bb/ccc" (8) doesn't.
        assert_eq!(fit_path_with("~/a/bb/ccc", 9.28, &one), (Cow::Owned("…/bb/ccc".into()), 4));
        assert_eq!(fit_path_with("~/a/bb/ccc", 4.0, &one), (Cow::Owned("…/ccc".into()), 7), "the last folder at least");
        assert_eq!(fit_path_with(r"C:\Users\me\Documents", 15.0, &one), (Cow::Owned(r"…\me\Documents".into()), 9));
        assert_eq!(fit_path_with("no-separators-at-all", 3.0, &one).1, 0, "left to Rune");
        assert_eq!(fit_path_with("~/é/ü/naïve", 10.0, &one), (Cow::Owned("…/ü/naïve".into()), 5), "bytes, not chars");
        // Bold marks follow the cut: those in the cut part go, one across it keeps its rest.
        let marks = [(0, 1), (2, 3), (4, 6), (7, 10), (2, 5)];
        assert_eq!(shift_marks(&marks, 4), [(4, 6), (7, 10), (4, 5)]);
        assert_eq!(shift_marks(&marks, 0), marks);
        let (shown, cut) = fit_path_with("~/a/bb/ccc", 9.28, &one);
        for (a, b) in shift_marks(&[(4, 6), (7, 10)], cut) {
            assert!(shown.is_char_boundary(a) && shown.is_char_boundary(b));
        }
        assert_eq!(&shown[4..6], "bb");
    }

    /// The real advances: a path cut for 300 points of BODY text measures within it.
    #[test]
    fn a_cut_path_fits_its_room() {
        let p = "~/Library/Mobile Documents/com~apple~CloudDocs/Projects/2026/clients/acme/reports/quarterly/final";
        let (shown, cut) = fit_path(p, 300.0);
        assert!(cut > 0 && shown.starts_with("…/") && shown.ends_with("/final"), "{shown}");
        let w = rune::text::measure(&shown, &[], BODY, None).0;
        assert!(w <= 300.0, "{shown}: {w}");
        // And not much shorter than it could be: the next folder up wouldn't fit.
        let longer = format!("…/{}", &p[p[..cut - 1].rfind('/').unwrap() + 1..]);
        assert!(rune::text::measure(&longer, &[], BODY, None).0 > 300.0 * 0.9, "{longer}");
    }

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
