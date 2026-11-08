//! What a key costs Arbor and Rune before the GPU: the handler, the view, layout and commit,
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
    let d = std::env::temp_dir().join(format!("arbor-frame-cost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..250 {
        std::fs::create_dir(d.join(format!("folder {i:04}"))).unwrap();
    }
    for i in 0..9750 {
        std::fs::write(d.join(format!("file {i:05}.txt")), "x").unwrap();
    }
    DIR.with_borrow_mut(|slot| *slot = Some(d.clone()));
    let spec = AppSpec::<State> {
        poll: Some(|s, _, tx| s.poll(tx)),
        on_key: Some(|s, ev, tx| s.on_key(ev, tx)),
        ..AppSpec::new("Arbor", (980.0, 620.0), init, crate::view::view)
    };
    let mut rt = App::new(spec).headless_runtime((980.0, 620.0), Env::new(false, 2.0));
    for _ in 0..3 {
        rt.frame(0.0);
    }
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
        "frame_cost: key → committed frame over 10,000 entries: min {:.3} ms, median {:.3} ms, p95 {:.3} ms",
        at(0.0),
        at(0.5),
        at(0.95)
    );
    let _ = std::fs::remove_dir_all(&d);
}
