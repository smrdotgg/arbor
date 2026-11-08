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

## Breadcrumbs: no way to say which children shrink first

- **Feature:** the path bar's breadcrumbs, in split view and narrow windows (NORTHSTAR: the
  same app at every size; you always see where you are).
- **What Rune is missing:** flex-shrink weights (or a shrink order). An element either shrinks
  (`shrink()`, `truncate()`: CSS `flex-shrink: 1`) or doesn't, so when a row of crumbs is too
  wide every crumb is cut in proportion to its width, the current folder included ("ho…",
  "s…", "m3de…", "Pho…").
- **What we did instead:** the view measures each label with `rune::text::measure` against the
  pane's laid-out width (`shown_crumbs` in `src/view.rs`) and folds the leading crumbs into one
  "…" crumb (which opens the deepest hidden folder) until the rest fit whole. The current
  folder truncates only when it alone doesn't fit. A cheap length bound skips measuring when
  the pane is clearly wide enough.
- **What users lose:** little. Hidden ancestors are a click on "…" away (one level at a time;
  there's no menu of them). The fit is computed from constants that mirror the path bar's
  layout (padding, button sizes), so a layout change there has to update `PATHBAR_CHROME` too.
- **Suggested Rune change:** `El::shrink_weight(f32)` (CSS `flex-shrink: n`), or a
  `shrink_order(i32)` that cuts lower-order children to their minimum before touching higher
  ones. With either, the crumbs could stay one plain row.

## Drag between panes: a system drag can't land in the window it came from

- **Feature:** split view (NORTHSTAR Core): dragging rows from one pane onto the other copies or
  moves them, as a drop from Finder or Explorer does.
- **What Rune is missing:** a drag that can end inside its own window. On macOS a `drag_out`
  session offers nothing to the app's own windows (`drag_out/macos.rs`: `mask_in` answers
  `None` for `WithinApplication`, and `mute` unregisters the app's windows' drag types for the
  session), so the other pane's `on_drop_files` never hears it. An element with `drag_out`
  never runs its `on_drag` on macOS, and which of the two a press becomes is fixed per element,
  so one gesture can't be "inside the window, then out to Finder". Off macOS, `drag_out` starts
  no system drag at all, so there's nothing to drop either.
- **What we did instead:** while the window is split, rows drop `drag_out` and use `on_drag`,
  Arbor's own drag inside the window: the pane under the pointer comes from the panes' laid-out
  x (`Panes::at` and `file_row` in `src/view.rs`), the target pane gets a drop highlight, and the
  release goes through the same code as a file drop (`State::drop_on_pane` in `src/panes.rs`:
  same disk moves, another copies, ⌥ copies). Unsplit, rows keep `drag_out` as before. Checked
  under X11 (Xvfb, XTest pointer).
- **What users lose:** on macOS, while split, a row can't be dragged out to Finder or another
  app; closing the split (⌘\\) brings that back. The in-window drag shows no drag image under the
  pointer, only the target pane's highlight, and can't drop on a folder row in the other pane
  (only into its folder).
- **Suggested Rune change:** either an opt-in for a `drag_out` session to drop in its own app
  (`DragItem::allow_in_app()`, routed to `on_drop_files` and `on_drop_hover` like any drop), or
  let an element have both: run `on_drag` while the pointer stays in the window and start the
  system session only when it leaves.

## Command availability: no cheap "what's on the clipboard?" check

- **Feature:** the command palette and context menu dim commands that can't run right now
  (NORTHSTAR: every action reachable from the palette, which shows what's possible).
- **What Rune is missing:** a synchronous, cheap query of what the clipboard holds (files, text,
  nothing) without reading it. Rune only offers `Tx::read_clipboard` and `Tx::request_paste`,
  which read the whole content off the event thread and answer an update later. AppKit has
  `NSPasteboard.types` / `changeCount`, Windows `IsClipboardFormatAvailable(CF_HDROP)`, and
  Wayland/X11 the offered MIME types, all without transferring data.
- **What we did instead:** Paste and "Move Here" always show as available
  (`State::unavailable` in `src/app.rs`). Pasting with nothing usable says "Nothing to paste:
  copy some files first" (`State::paste` in `src/actions.rs`). Reading the clipboard each time the
  palette or menu opens was rejected: it transfers the content (possibly a large image) for a
  yes/no answer, and the answer would arrive a frame after the palette is drawn.
- **What users lose:** Paste looks runnable in the palette and context menu when it would do
  nothing, unlike Finder and Explorer, which grey it out. A small trust cost on a common action.
- **Suggested Rune change:** `Tx::clipboard_kinds() -> ClipboardKinds { files, text, image, .. }`
  (or `Cx::clipboard_kinds`), answered from the platform's type list without reading data, and
  `None` where it can't be known cheaply (the web).

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
  exactly the one that matters. Rune keeps Metal's `presentedTime` timestamps for timing only
  while tracing (`--trace`, `frame/present_time.rs`; untraced, it reads them only to keep a
  transparent window's shadow in step, `window/create.rs`) and doesn't pass them to apps. Other
  platforms have no source at all.
- **What we did instead:** time to `FrameEncoded::at()` (submission) with `Tx::input_time()` as
  the start (`src/timing.rs`), labelled a "submission proxy" in `BENCH.md` and the bench output,
  which claim no pass against the pixel budgets. Presentation runs two to three frames behind
  submission (`frame/present_time.rs`).
- **What users lose:** nothing directly, but the budget can't be verified. Measured numbers
  undercount by up to one refresh (8.3 ms at 120 Hz, 16.7 ms at 60 Hz) plus compositor latency,
  so a key press that measures 1 ms could still miss the 8 ms budget on screen.
- **Suggested Rune change:** an `App::on_frame_presented(|seq, Option<Instant>|)` hook, active
  only when registered. Sources: `presentedTime` / `addPresentedHandler` on macOS,
  `IDXGISwapChain::GetFrameStatistics` on Windows, and `wp_presentation` or
  `VK_KHR_present_wait` on Linux where available.
