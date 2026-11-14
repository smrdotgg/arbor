//! What a key costs Satchel and Rune before the GPU: the handler, the view, layout and commit,
//! in a headless runtime over a 10,000-entry folder (no window, no rendering). Steadier than
//! the windowed bench on a busy machine, since it times CPU work alone; used to compare two
//! builds (BENCH.md, milestone 11).
//!
//! `cargo test --release -- --ignored --nocapture frame_cost`

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Instant;

use rune::{App, AppSpec, Env, Key, KeyEvent, Mods};

use crate::app::State;

thread_local! {
    static DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

fn init() -> State {
    let d = DIR.with_borrow(|d| d.clone()).expect("a folder");
    let mut s = crate::app::tests::state_in(&d);
    crate::app::tests::settle_all(&mut s);
    s
}

#[test]
#[ignore]
fn frame_cost() {
    let d = std::env::temp_dir().join(format!("satchel-frame-cost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..250 {
        std::fs::create_dir(d.join(format!("folder {i:04}"))).unwrap();
    }
    for i in 0..9750 {
        std::fs::write(d.join(format!("file {i:05}.txt")), "x").unwrap();
    }
    DIR.with_borrow_mut(|slot| *slot = Some(d.clone()));
    // As before (980 pt: the default columns); then 1,600 pt wide with the default columns,
    // and with every column shown (Extension, Date Created and Permissions too).
    for (w, all) in [(980.0, false), (1600.0, false), (1600.0, true)] {
        let spec = AppSpec::<State> {
            poll: Some(|s, _, tx| s.poll(tx)),
            on_key: Some(|s, ev, tx| s.on_key(ev, tx)),
            ..AppSpec::new("Satchel", (w, 620.0), init, crate::view::view)
        };
        let mut rt = App::new(spec).headless_runtime((w, 620.0), Env::new(false, 2.0));
        if all {
            rt.update(|s, _| {
                let mut c = s.cols();
                for f in crate::listcols::Field::ALL {
                    c.set_shown(f, true);
                }
                s.shared.cols.set(c);
            });
        }
        for _ in 0..3 {
            rt.frame(0.0);
        }
        let shown = rt.state.cols().fit(w - 168.0, false).iter().count();
        let mut times = Vec::new();
        for round in 0..1200 {
            // Down through the list, back up now and then, so rows scroll in as they would.
            let key = if round % 300 < 250 { Key::Down } else { Key::Up };
            let t = Instant::now();
            rt.key(KeyEvent { key, mods: Mods::default() });
            rt.frame(1.0 / 120.0);
            times.push(t.elapsed().as_secs_f64() * 1e3);
        }
        times.sort_by(f64::total_cmp);
        let at = |q: f64| times[((times.len() as f64 - 1.0) * q).round() as usize];
        println!(
            "frame_cost ({w} pt, {shown} columns after the name): key → committed frame over 10,000 entries: min {:.3} ms, median {:.3} ms, p95 {:.3} ms",
            at(0.0),
            at(0.5),
            at(0.95)
        );
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// The column view's per-key cost, as `frame_cost` (CPU only, no GPU), in folders of 1,000, 10,000
/// and 30,000 entries: ← out of the big
/// folder (it becomes the column on the right) and → back into it (its entries become the
/// active column), which only move listings on screen (the key budget, 8 ms), and End onto a
/// folder, whose contents are read for the column on the right (a read: the listing budget),
/// each from the key to the committed frame.
///
/// `cargo test --release -- --ignored --nocapture column_cost`
#[test]
#[ignore]
fn column_cost() {
    // At three sizes: what a move costs shouldn't grow with the folder (nothing per entry).
    for entries in [1_000, 10_000, 30_000] {
        column_cost_in(entries);
    }
}

/// [`column_cost`] in a folder of `entries` (250 folders, the rest files).
fn column_cost_in(entries: usize) {
    let root = std::env::temp_dir().join(format!("satchel-column-cost-{}", std::process::id()));
    let d = root.join("big");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..250 {
        std::fs::create_dir(d.join(format!("folder {i:04}"))).unwrap();
    }
    for i in 0..entries - 250 {
        std::fs::write(d.join(format!("file {i:05}.txt")), "x").unwrap();
    }
    DIR.with_borrow_mut(|slot| *slot = Some(d.clone()));
    let spec = AppSpec::<State> {
        poll: Some(|s, _, tx| s.poll(tx)),
        on_key: Some(|s, ev, tx| s.on_key(ev, tx)),
        ..AppSpec::new("Satchel", (980.0, 620.0), init, crate::view::view)
    };
    let mut rt = App::new(spec).headless_runtime((980.0, 620.0), Env::new(false, 2.0));
    // The columns start at the bench folder's parent, as a sidebar place.
    rt.update(|s, _| s.places = vec![("Root".into(), root.clone())]);
    rt.update(|s, tx| s.run(crate::commands::Cmd::AsColumns, tx));
    for _ in 0..3 {
        rt.frame(0.0);
    }
    let (mut out, mut into, mut read, mut back) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for round in 0..400 {
        let (key, times) = match round % 4 {
            0 => (Key::Left, &mut out),
            1 => (Key::Right, &mut into),
            2 => (Key::End, &mut read),
            _ => (Key::Home, &mut back),
        };
        let t = Instant::now();
        rt.key(KeyEvent { key, mods: Mods::default() });
        rt.frame(1.0 / 120.0);
        times.push(t.elapsed().as_secs_f64() * 1e3);
        // Whatever a column read late lands before the next key, as it would between keys.
        for _ in 0..3 {
            rt.frame(1.0 / 120.0);
        }
    }
    assert!(rt.state.cur().cwd == d, "ended in the big folder");
    drop(back);
    // The list view's own frame landing the same listing after ← or → (the read itself, on a
    // worker, left out): what a navigation into a 10k folder costs the window anyway.
    rt.update(|s, tx| s.run(crate::commands::Cmd::AsList, tx));
    for _ in 0..3 {
        rt.frame(0.0);
    }
    let mut list = Vec::new();
    for round in 0..40 {
        let key = if round % 2 == 0 { Key::Left } else { Key::Right };
        rt.key(KeyEvent { key, mods: Mods::default() });
        rt.frame(1.0 / 120.0);
        while rt.state.cur().is_loading() {
            std::thread::sleep(std::time::Duration::from_millis(1));
            let t = Instant::now();
            rt.frame(1.0 / 120.0);
            if !rt.state.cur().is_loading() && rt.state.cur().cwd == d {
                list.push(t.elapsed().as_secs_f64() * 1e3);
            }
        }
    }
    let k = entries / 1000;
    for (name, times) in [
        (format!("← out of {k}k"), &mut out),
        (format!("→ into {k}k"), &mut into),
        ("End onto a folder (a read)".to_string(), &mut read),
        (format!("list view: {k}k listing lands"), &mut list),
    ] {
        times.sort_by(f64::total_cmp);
        let at = |q: f64| times[((times.len() as f64 - 1.0) * q).round() as usize];
        println!("column_cost: {name}: min {:.3} ms, median {:.3} ms, p95 {:.3} ms", at(0.0), at(0.5), at(0.95));
    }
    let _ = std::fs::remove_dir_all(&root);
}
