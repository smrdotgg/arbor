# Rune gaps

Arbor uses Rune exactly as it is (pinned to the commit in `RUNE_REV`). We don't patch it. When a feature ends
up worse than it should be because Rune is missing something, it gets an entry here. The owner
reviews this file with the Rune team at Anomaly.

Each entry:

- **Feature:** the Arbor feature that hit the gap
- **What Rune is missing:** the API or behaviour we needed
- **What we did instead:** the workaround, with file references
- **What users lose:** how much better the experience would be with it (be concrete)
- **Suggested Rune change:** what we'd ask for, as small as possible

Newest first.

---

## Cold start: most of the 100 ms budget goes before the app's code runs

- **Feature:** cold start to interactive window, budget 100 ms (NORTHSTAR speed budgets).
- **What Rune is missing:** a launch fast enough to leave the app room, and a way to see where
  launch time goes in a release build. On an M4 Pro (macOS 27.2), `main` → first frame
  submitted is about 123 ms (median) whether the folder has 1 entry or 10,000. `AppSpec::init`
  starts 65–80 ms after `main`, and window, Metal surface and first frame take about 40 ms more.
  Arbor's own share is about 3 ms (`init` 0.2 ms, first view build 2–4 ms). Rune's launch
  breakdown (`perf_warn::launch`) exists only in debug builds and only warns past 100 ms of app
  time, so the 65–80 ms before `init` can't be split further from outside Rune.
- **What we did instead:** call `rune::prewarm_gpu()` first thing in `main` and read the first
  folder on a thread started there, so both overlap AppKit's launch (`src/main.rs`,
  `app::read_first_folder` in `src/app.rs`). That took the 10k-folder case from ~140 ms to
  ~123 ms `main` → frame. `ARBOR_TIMING=1` logs `init_at_ms` and `init_ms` to show the split
  (`src/timing.rs`). Numbers are in `BENCH.md`.
- **What users lose:** every launch takes about 140 ms from click to window instead of under
  100 ms. Opening a folder from another app (Arbor as the default folder handler) feels like
  launching an app instead of switching to one.
- **Suggested Rune change:** an opt-in release-build launch breakdown, for example phases added
  to the `RUNE_READY_DIR` JSON (event loop built, did-finish-launching, window created, surface
  ready, first frame submitted). Then cut the time before the window is created, for example by
  creating the window and drawing the first frame as soon as AppKit allows.

## Frame timing: no "presented at" for a frame

- **Feature:** measuring "keypress to updated pixels under 8 ms" (`ARBOR_TIMING`, `scripts/bench.sh`).
- **What Rune is missing:** when a frame actually reached the screen. `FrameEncoded::at()` is
  when the commands were submitted, and `presents_at()` is the display link's target, known only
  for display-paced macOS frames. It's `None` for the first frame after a key press, which is
  exactly the one that matters. Rune reads Metal's `presentedTime` only under `--trace`
  (`frame/present_time.rs`) and doesn't pass it to apps. Other platforms have no source at all.
- **What we did instead:** time to `FrameEncoded::at()` (submission) with `Tx::input_time()` as
  the start (`src/timing.rs`). The docs and `BENCH.md` say the pixels land at the next refresh.
- **What users lose:** nothing directly, but the budget can't be verified. Measured numbers
  undercount by up to one refresh (8.3 ms at 120 Hz, 16.7 ms at 60 Hz) plus compositor latency,
  so a key press that measures 1 ms could still miss the 8 ms budget on screen.
- **Suggested Rune change:** an `App::on_frame_presented(|seq, Option<Instant>|)` hook, active
  only when registered. Sources: `presentedTime` / `addPresentedHandler` on macOS,
  `IDXGISwapChain::GetFrameStatistics` on Windows, and `wp_presentation` or
  `VK_KHR_present_wait` on Linux where available.
