# Benchmarks

Arbor against the speed budgets in [`NORTHSTAR.md`](NORTHSTAR.md). Newest first. To reproduce:

```sh
scripts/bench.sh 11 10                         # 11 cold starts, 10 rounds each (opens windows: leave the machine alone)
scripts/bench.sh 11 10 --tabs 5                # plus 5 cold starts timing a tab switch to the 10k folder
xvfb-run -a scripts/bench.sh 11 10             # Linux without a display
cargo test --release -- --ignored --nocapture list_10k   # read + sort only, no window
```

Each run cold-starts the release build on a fresh 10,000-entry folder (250 folders, 9,750 files),
then 10 times leaves it, comes back and presses ↓, then quits (`ARBOR_BENCH`, see
`src/timing.rs`). The first run is reported on its own (below). A run counts only if Arbor exits
cleanly and logs its first frame and exactly 10 listings and 10 key presses; otherwise the bench
says INVALID and exits non-zero.

**Submission proxy.** Every time here ends when the frame is submitted to the GPU, not when its
pixels are on screen. They land at least one refresh later, and Rune's own presentation timing
runs two to three frames behind submission (`frame/present_time.rs`). Rune doesn't report when a
frame reached the screen (see [`RUNE-GAPS.md`](RUNE-GAPS.md)). So "under" below means the
submission proxy is under the budget. It is **not** a pass of the "keypress to updated pixels"
or "listing visible" budgets, which can't be verified yet.

## 2026-11-08, quick preview (milestone 4)

What changed on the measured path: nothing runs while the preview is closed (the default), but
every key, command and listing landing now makes one `None` check (`State::sync_preview`), and
the binary carries the new decoders. Before is a569066, after is 0a77e11, both driven by the new
bench, alternating. New rows (`--preview 5`: five more cold starts per pass, 10 rounds each, in a
folder of a 300 KB text file and a 4000 × 3000 JPEG of 4.3 MB):

- **preview 256 KB text**: Space on the text file to the frame showing it (the read, sniffing,
  laying out the lines on the worker, then the panel's first frame of visible lines).
- **preview 12 MP JPEG**: Space on the photo to the frame showing it (read, decode at 1/4 size
  through the IDCT, box-filter to the panel, upload).
- **key → frame, preview open**: ↑ and ↓ while it's open, each starting a new read; "while the
  JPEG decodes" is a ↑ pressed one frame after ↓ started the photo's decode, which it stops.

### tabot-dev (Xvfb, lavapipe; not representative)

Load average 0.2–1.4. Order: before, after, after, before.

| Measure (median / p95, submission proxy) | Before (2 passes) | After (2 passes) |
| --- | --- | --- |
| Cold start (spawn → first frame) | 130.9 / 144.6, 132.5 / 138.4 ms | 134.4 / 143.4, 136.2 / 140.6 ms |
| Listing visible (10k) | 40.1 / 45.3, 41.5 / 44.5 ms | 41.8 / 46.0, 42.3 / 45.7 ms |
| Key → frame (synthetic ↓, preview closed) | 6.3 / 6.8, 6.2 / 6.6 ms | 6.6 / 7.2, 6.3 / 6.9 ms |
| **Preview 256 KB text** (Space → frame) | — | **15.3 / 28.5, 14.9 / 28.1 ms** |
| **Preview 12 MP JPEG** (Space → frame) | — | **107.2 / 112.8, 107.1 / 112.7 ms** |
| **Key → frame, preview open** | — | **5.3 / 7.2, 5.2 / 7.0 ms** |
| … while the JPEG decodes | — | 5.1 / 6.2, 5.0 / 6.0 ms |

The cold-start medians came out 2–4 ms apart between passes, so they were checked twice more:
three more alternating pairs of 21 cold starts (before 129.6, 134.1, 131.8 ms; after 133.6,
134.4, 133.6 ms; p95 139.0–140.7 ms for both), then 12 single launches of each, interleaved one
by one, reading the first frame's own split: `main` → first frame 136.0 ms before and 136.2 ms
after, view build 8.96 and 9.11 ms, `State::new` 0.11 and 0.13 ms. No regression: the gap was
drift between passes. Listing and key times are within noise.

The text preview's p95 is its first showing in each run: that frame loads the system's
monospaced font (Rune runs `fc-match` on Linux), about 8 ms of view build once per session;
after it, 2 ms. The photo's time is mostly decoding: jpeg-decoder at 1/4 scale takes about 75 ms
of it on this machine (a full-size decode with zune-jpeg took 130–180 ms and 36 MB, against
2.3 MB). With the preview open the list still moves in about 5 ms a key (most of it lavapipe
encoding the frame; the view build is 0.5–1 ms), including while the photo decodes on its worker.

Binary: release 36.49 → 37.38 MB unstripped, 27.50 → 28.24 MB stripped (+0.74 MB: jpeg-decoder,
gif, image-webp, image's BMP codec and the preview). The new crates add no startup work: nothing
of theirs runs until a preview decodes.

## 2026-11-08, tabs and split view (milestone 3)

What changed on the measured path: the window's folder now lives in a pane (one, unless split),
every read carries a request number, and the view builds a pane column around the list. With
one pane and one tab nothing else is drawn or run: no tab bar, no second pane, no drag handler.
Before is 7b4b9b8, after is d7fe026. Both were driven by the new bench (bd3756c,
`--bin`), alternating, 11 cold starts × 10 rounds per pass. New row: **tab switch to 10k**,
`--tabs 5`: five more cold starts, each opening a second tab on the parent folder, then 10
times showing the 10k tab again (a fresh read, since a hidden tab keeps no listing), timed from
the key to the frame showing it, and going back.

### MacBook Pro, Apple M4 Pro, macOS 27.2 (60 Hz)

Load average 1.7–4.2 during the passes (not known to be idle). Order: before, before, after,
before, after (a first after pass was lost to a driver argument bug).

| Measure (median / p95, submission proxy) | Before (3 passes) | After (2 passes) |
| --- | --- | --- |
| Cold start (spawn → first frame) | 131.2 / 145.0, 129.8 / 146.1, 132.5 / 142.9 ms | 132.8 / 161.8, 130.6 / 139.4 ms |
| … of which `main` → first frame | 117.6, 117.3, 118.6 ms | 119.6, 118.8 ms |
| Listing visible (10k), request → frame | 36.8 / 38.6, 36.8 / 41.1, 36.8 / 43.5 ms | 36.9 / 38.4, 36.7 / 38.0 ms |
| Key → frame (synthetic ↓) | 0.7 / 0.8 ms | 0.7 / 0.8 ms |
| **Tab switch to 10k** (key → frame) | — | **36.2 / 50.2, 36.2 / 38.0 ms** (medians under the 50 ms listing budget; one pass's p95 over it, at 50.2 ms) |
| 10k listing in the first frame | 33 of 33 runs | 22 of 22 runs |

### tabot-dev (Xvfb, lavapipe; not representative)

The machine was busy (other jobs compiling; load average 2.3–4.4 on 4 cores), so every number
is well above the earlier tables, both builds alike. Order: before, after, after, before.

| Measure (median / p95, submission proxy) | Before (2 passes) | After (2 passes) |
| --- | --- | --- |
| Cold start (spawn → first frame) | 211.4 / 221.4, 207.6 / 231.1 ms | 207.5 / 219.0, 206.0 / 247.9 ms |
| Listing visible (10k) | 63.8 / 88.9, 61.8 / 87.9 ms | 58.0 / 78.9, 60.4 / 83.8 ms |
| Key → frame (synthetic ↓) | 11.6 / 12.8, 11.6 / 14.8 ms | 10.7 / 12.6, 10.6 / 12.5 ms |
| **Tab switch to 10k** (key → frame) | — | 48.5 / 56.0, 61.6 / 78.3 ms |

No regression beyond noise on either machine. A tab switch to the 10k folder costs what a
navigation to it costs: the read (about 18 ms on the Mac) is the work, and nothing is kept to
skip it. It never made the key's own frame (0 of 100 on each machine): showing a tab waits at
most 8 ms for its read, and a 10k read takes longer, so the key's frame shows the tab's path
and an empty list, and the listing follows in its own frame. Smaller folders do land in the
key's frame: a 300-entry tab under Xvfb logged `tab ms=26.9 same_frame=1` (most of it lavapipe
encoding). That wait holds the UI thread for up to 8 ms on a switch, which the key → frame
budget shares; it's the price of a local folder appearing without a blank frame.

## 2026-11-08, after the undo-safety and startup fixes (milestone 1 review)

What changed on the startup path: the window no longer waits for the first folder. `State::new`
waits at most 8 ms (one 120 Hz frame) for the settings and listing read on the thread `main`
starts, then opens regardless; a late listing lands like any navigation, with "Loading…" until
then. Also every move and copy now goes through no-replace primitives (no effect on these
measures). Before and after were built from 0402bba and the final commit, run alternately with
the same bench driver (11 cold starts × 10 rounds each).

### MacBook Pro, Apple M4 Pro, macOS 27.2 (60 Hz)

Owner away, nothing else running. "After" is the startup fix (a6496c8), four passes each in
both orders (before/after/before/after, then after/before/after/before) to cancel drift.

| Measure (median / p95, submission proxy) | Before (4 passes) | After (4 passes) |
| --- | --- | --- |
| Cold start (spawn → first frame) | 128.6–136.5 / 136–154 ms (mean of medians 133.1) | 126.7–137.3 / 138–159 ms (mean 131.9) |
| … of which `main` → first frame | 114.2–119.1 ms | 115.6–122.5 ms |
| Listing visible (10k), request → frame | 36.6–36.9 / 37.3–37.5 ms | 36.7–36.8 / 37.2–37.7 ms |
| Key → frame (synthetic ↓) | 0.7 / 0.8 ms | 0.7 / 0.8 ms |
| 10k listing in the first frame | 44 of 44 runs | 44 of 44 runs |

The final code (with the milestone 2 fixes too) against before, alternating, two passes each:
129.6 and 130.5 ms against 128.7 and 131.3 ms cold, listing 36.8 ms in all four, 44 of 44
first frames with the listing.
No change beyond noise: on the Mac `State::new` starts 65–80 ms after `main`, long after the
10k folder is read (about 18 ms), so the wait never runs and the listing always makes the first
frame. Cold start still misses its 100 ms budget, for the reasons below (Rune's launch path).

With a 2 s artificial first read (`ARBOR_TIMING=1 ARBOR_SLOW_FIRST_READ_MS=2000`, Linux): first
frame at 119.6 ms (`init_ms` 8.1, the bounded wait) and the listing at 2012 ms. Before, the
window waited the full 2 s. A FIFO as the settings file no longer holds up the window either
(120 ms first frame).

### tabot-dev (Xvfb, lavapipe; not representative)

| Measure (median / p95, submission proxy) | Before (3 passes) | After (2 of a6496c8, 2 final) |
| --- | --- | --- |
| Cold start (spawn → first frame) | 129.0–134.7 / 138–158 ms | 128.1–133.0 / 140–155 ms |
| Listing visible (10k) | 37.9–40.6 / 43.9–51.3 ms | 39.2–41.0 / 43.7–56.4 ms |
| Key → frame (synthetic ↓) | 6.2–6.3 / 6.7–9.7 ms | 6.1–6.3 / 6.6–9.7 ms |
| 10k listing in the first frame | 33 of 33 runs | 44 of 44 runs |

No change beyond noise (the last pair ran while the machine was busier, both builds alike). Here
`State::new` starts only about 16 ms after `main`, so the 8 ms wait matters: the 10k listing still
made the first frame in every run, but a slower disk would show the window first and the
listing a few milliseconds later.

An earlier Mac pass was thrown away because the owner was using the machine. One of its runs
came out INVALID (wrong sample count; the cause wasn't captured, and it didn't recur in 132 runs
since), which is what the new check is for.

## 2026-11-08, after the command palette and settings file (milestone 2)

tabot-dev only (Xvfb, lavapipe; same caveats as below). The Mac wasn't re-measured: no GUI runs
there this milestone. Before and after were built from adjacent commits and run back to back
(11 cold starts × 10 rounds each). The baseline ran twice to show the noise.

| Measure (median / p95, submission proxy) | Before | Before, again | After, no settings file | After, every command bound |
| --- | --- | --- | --- | --- |
| Cold start (spawn → first frame) | 132.5 / 138.6 ms | 133.0 / 138.4 ms | 132.8 / 137.8 ms | 133.5 / 143.8 ms |
| … of which `main` → first frame | 131.3 / 137.3 ms | 131.7 / 137.1 ms | 131.6 / 136.4 ms | 132.2 / 142.4 ms |
| Listing visible (10k entries) | 40.8 / 43.9 ms | 40.6 / 44.6 ms | 42.2 / 44.9 ms | 41.6 / 45.1 ms |
| Key → frame (synthetic ↓) | 6.2 / 6.7 ms | 6.2 / 6.8 ms | 6.4 / 7.2 ms | 6.4 / 6.9 ms |

No change beyond noise. What the new code costs, measured directly
(`cargo test --release -- --ignored --nocapture dispatch_speed parse_speed`):

| Work | Time | Where it runs |
| --- | --- | --- |
| Find a key's command (~70 chords) | 0.3–0.8 µs | every key |
| Sidebar's 9 shortcut labels from the keymap | 1.7 µs | every frame |
| Rank every command for a query | 24 µs | each palette frame, only while it's open |
| Read the settings file: none / the template / every command bound | 23 / 26 / 161 µs | once, on the startup thread, alongside the platform's launch |

Opening the palette in a real window (`ARBOR_TIMING=1`, XTest key under Xvfb): 9.7 ms to the
frame the first time (2.6 ms building the view, 7.0 ms encoding on the CPU renderer), 7.4 ms
after (1.2 ms view). Only the rows in view are built. On lavapipe, encoding dominates every
frame (a plain ↓ is 6.4 ms), so this says little about a real GPU; the Mac should be measured
next time GUI runs are allowed there. The release binary grew 0.47 MB (35.8 → 36.3 MB, unstripped).

## 2026-11-08

### MacBook Pro, Apple M4 Pro, 24 GB, macOS 27.2 (the real target)

Window on a 60 Hz display. Release build (`dist` gave the same numbers).

| Measure | Median | p95 | Budget | Result |
| --- | --- | --- | --- | --- |
| Cold start (spawn → first frame) | 139 ms | 161 ms | 100 ms | **fail** |
| … of which `main` → first frame | 123 ms | 138 ms | | |
| Listing visible (10k entries, request → frame) | 36.9 ms | 37.4 ms | 50 ms | under (submission proxy) |
| Key → frame (synthetic ↓) | 0.7 ms | 0.8 ms | 8 ms | under (submission proxy) |
| Read + sort 10k (microbenchmark) | 17.9 ms | 19.4 ms | | |

First run left out: 586 ms cold. The first exec of a newly built binary spends 400–800 ms before
`main` while macOS checks its signature. A copied binary shows the same, so users pay it once per
install or update.

### tabot-dev: AWS, Xeon Platinum 8375C (4 vCPU), Ubuntu 24.04, ext4

Xvfb with Mesa lavapipe (software Vulkan). **Not representative**: drawing runs on the CPU.

| Measure | Median | p95 | Budget | Result |
| --- | --- | --- | --- | --- |
| Cold start (spawn → first frame) | 134 ms | 141 ms | 100 ms | fail |
| … of which `main` → first frame | 133 ms | 140 ms | | |
| Listing visible (10k entries) | 40.0 ms | 45.1 ms | 50 ms | under (submission proxy) |
| Key → frame (synthetic ↓) | 6.6 ms | 7.1 ms | 8 ms | under (submission proxy) |
| Read + sort 10k (microbenchmark) | 18.7 ms | 19.2 ms | | |

### Why cold start fails

On the Mac, `ARBOR_TIMING=1` splits the 139 ms:

| Part | Time | Whose |
| --- | --- | --- |
| spawn → `main` (exec, dyld) | ~15 ms | OS |
| `main` → `State::new` starts | 65–80 ms | Rune event loop and AppKit launch (GPU prewarm runs alongside) |
| `State::new` | 0.2 ms | Arbor (the folder was already read on a thread started in `main`) |
| window, Metal surface, first frame | ~40 ms | Rune; Arbor's view build is 2–4 ms of it (`update_ms`) |

A one-entry folder takes as long as the 10k one, and Arbor's own work in the path is about 3 ms.
Before this milestone the 10k folder was read synchronously in `State::new` (20 ms, after AppKit's
launch). It's now read on a thread started at the top of `main`, alongside `rune::prewarm_gpu()`,
which saved about 20 ms on big folders. The rest is Rune's launch path and can't be fixed in
Arbor. Logged in RUNE-GAPS.md with a suggested Rune change. Launched as a bare binary over ssh;
a `.app` bundle launched from Finder may differ.

### Notes

- **Listing (37 ms on the Mac, to submission)** is about 18 ms reading (`read_dir` plus one `lstat` per entry on
  the worker), about 4 ms building the frame, and the rest is waking the UI thread and waiting
  for a 60 Hz refresh. That leaves 13 ms of headroom on a fast SSD. A slow disk or old laptop
  (NORTHSTAR milestone 3) will need a faster read. On macOS, `getattrlistbulk` returns names,
  types, sizes and dates for a whole folder in a few calls instead of 10,000 `lstat`s.
- **Key → frame** is synthetic: it starts when the handler is called, so it leaves out the OS
  input queue. With `ARBOR_TIMING=1`, real key presses are timed from the OS event's own
  timestamp (macOS) for anyone who wants to check by hand.
