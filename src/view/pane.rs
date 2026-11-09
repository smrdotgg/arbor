//! A pane's chrome: the pane itself, its path bar (breadcrumbs that fold when narrow), tab bar and search field.

use super::*;

/// One pane. A click anywhere in it makes it the active one; every handler inside does that
/// first, so the command acts on the pane that was clicked.
pub(super) fn pane(s: &State, p: &Pane, c: Ctx) -> El {
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
pub(super) fn in_pane(s: &mut State, id: u64, tx: &mut rune::Tx) -> bool {
    tx.instant();
    s.activate_id(id, tx)
}

pub(super) fn pathbar(f: &Folder, l: Look, width: f32) -> El {
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
pub(super) const CRUMB_GAP: f32 = 2.0;
pub(super) const CRUMB_PAD: f32 = 4.0;
/// What the path bar spends besides the crumbs: its padding, three 24 pt buttons, a 6 pt
/// spacer, and the gaps between those five.
pub(super) const PATHBAR_CHROME: f32 = 2.0 * 8.0 + 3.0 * 24.0 + 6.0 + 4.0 * 2.0;

/// One breadcrumb: its label, the folder it opens, and whether a "›" goes before it (not
/// after the root or a Windows drive).
pub(super) struct Crumb {
    pub(super) label: String,
    pub(super) target: PathBuf,
    pub(super) sep: bool,
}

pub(super) fn crumbs_of(path: &std::path::Path) -> Vec<Crumb> {
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
pub(super) enum Part<'a> {
    /// A Windows drive (`C:`) or share (`\\server\share`).
    Prefix(&'a std::ffi::OsStr),
    Root,
    Normal(&'a std::ffi::OsStr),
}

/// The breadcrumbs of a path's parts, joined with `sep`. A drive or share and the root after it
/// are one crumb, which opens the drive's root (`C:\`), not the drive-relative `C:` (the
/// current folder on that drive). Pure path logic, so it's tested for Windows paths everywhere.
pub(super) fn crumbs_from(parts: &[Part], sep: &str) -> Vec<Crumb> {
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
pub(super) enum Shown {
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
pub(super) fn shown_crumbs(crumbs: &[Crumb], avail: f32, width: impl Fn(&str) -> f32) -> Shown {
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
pub(super) fn measure_crumb(label: &str) -> f32 {
    rune::text::measure(label, &[], CRUMB, None).0
}

/// The narrowest a tab gets before the strip shows fewer tabs, the gap between tabs, and the
/// width of a "‹ 3" / "5 ›" marker for the tabs that don't fit.
pub(super) const TAB_MIN: f32 = 64.0;
pub(super) const TAB_GAP: f32 = 4.0;
pub(super) const TAB_MORE: f32 = 34.0;
/// What the tab bar spends besides tabs: its padding, and "+" with the gap before it.
pub(super) const TABBAR_CHROME: f32 = 2.0 * 6.0 + 22.0 + TAB_GAP;

/// Which of `n` tabs the strip shows in `avail` points: all of them while each can be at least
/// [`TAB_MIN`] wide, else as many as fit beside the two markers, centred on the `active` one
/// (which is always shown).
pub(super) fn tab_window(n: usize, active: usize, avail: f32) -> std::ops::Range<usize> {
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
pub(super) fn tabbar(p: &Pane, l: Look, width: f32) -> El {
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
pub(super) fn search_bar(f: &Folder, s: &Search, c: Ctx) -> El {
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
