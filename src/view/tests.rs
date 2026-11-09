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

/// A tab dropped on the other pane goes where the pointer is in its strip (before the tab
/// under its left half, after one under its right half), and at the end anywhere else in the
/// pane, or when it has no strip.
#[test]
fn a_tab_dropped_on_a_pane_lands_where_the_pointer_is() {
    let pane = |n: usize, tab: usize| Pane {
        f: Folder::new(9, PathBuf::from("/"), crate::fs::Sort { key: SortKey::Name, desc: false }, false, None),
        tabs: (0..n)
            .map(|j| {
                (j != tab)
                    .then(|| Folder::new(9, PathBuf::from("/"), crate::fs::Sort { key: SortKey::Name, desc: false }, false, None).memo())
            })
            .collect(),
        tab,
    };
    // 3 tabs in a 600 pt pane at x = 100: each 220 pt (the most), from x = 106.
    let rect = [100.0, 0.0, 600.0, 400.0];
    let y = BAR_H + 1.0 + TABS_H / 2.0;
    let p = pane(3, 0);
    assert_eq!(strip_slot(rect, &p, [110.0, y]), 0);
    assert_eq!(strip_slot(rect, &p, [106.0 + 150.0, y]), 1, "right half of the first tab: after it");
    assert_eq!(strip_slot(rect, &p, [106.0 + 224.0 + 50.0, y]), 1);
    assert_eq!(strip_slot(rect, &p, [690.0, y]), 3, "past the last: the end");
    assert_eq!(strip_slot(rect, &p, [300.0, 200.0]), 3, "over the list: the end");
    assert_eq!(strip_slot(rect, &pane(1, 0), [110.0, y]), 1, "no strip: after its one tab");
}
