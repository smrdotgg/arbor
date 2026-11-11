# Benchmarks

Arbor against the speed budgets in [`NORTHSTAR.md`](NORTHSTAR.md). Newest first. To reproduce:

```sh
scripts/bench.sh 11 10                         # 11 cold starts, 10 rounds each (opens windows: leave the machine alone)
scripts/bench.sh 11 10 --tabs 5                # plus 5 cold starts timing a tab switch to the 10k folder
scripts/bench.sh 11 10 --jump 5                # plus 5 cold starts opening Jump with a 5k-folder history
scripts/bench.sh 11 10 --search 5              # plus 5 cold starts searching a 120k-file tree
scripts/bench.sh 11 10 --rename 5              # plus 5 cold starts typing in Batch Rename over 10k items
scripts/bench.sh 1 10 --grid 4                 # plus 4 + 4 cold starts paging the icon view (10k, and 1,000 photos)
scripts/bench.sh 1 10 --columns 5 --tiles 4    # plus 5 cold starts moving in and out of the 10k folder as columns, 4 paging tiles
cargo test --release -- --ignored --nocapture column_cost   # the column view's ← and → over 10k, CPU only, beside the list's own landing
scripts/bench.sh 1 12 --jump 5 --pace 120 --gap 1000   # the same, paced as a person types (report both)
scripts/bench.sh 1 10 --window 4               # plus 4 cold starts opening a new window on the 10k folder 10 times each
scripts/bench.sh 1 2 --idle 3                  # 3 starts left idle in each view, and 3 with two windows: CPU over 5 s, fails above 0.05 s
cargo test --release -- --ignored --nocapture search_speed   # walk speed by thread count (vendor/, or ARBOR_SEARCH_TREE)
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

## 2026-11-11, cold start on Windows and Linux (`perf/fast-start`)

The budget is now under 30 ms on Windows and Linux; the owner's v0.1.0-alpha.4 takes about a
second on a Windows gaming laptop. **Not met on either platform yet**; most of what's left is in
Rune (RUNE-GAPS.md, "Cold start on Windows and Linux").

New measurement: `ARBOR_TIMING_FILE=<path>` (a release build on Windows has no console) with
`ARBOR_EXIT_AFTER_FIRST_FRAME=1`, a total from the process's creation (`GetProcessTimes`, so the
loader counts) and finer splits; two probes to compare against, `examples/win32_hello.rs` (bare
Win32 + GDI, the floor) and `examples/rune_hello.rs` (one Rune text view, Arbor's features and
`dist` profile). The pre-release `coldstart-probe-1` has all three, MSVC `dist` builds of
a620e33 (v0.1.0-alpha.4's startup); `scripts/coldstart-probe.ps1` runs them (for the owner's
laptop: `irm https://raw.githubusercontent.com/smrdotgg/arbor/coldstart-probe-1/scripts/coldstart-probe.ps1 | iex`),
and `scripts/coldstart-ab.ps1` compares builds. The "Cold start (Windows)" workflow runs both on
every push to a `perf/` branch.

**GitHub's Windows runner** (Hyper-V VM, no GPU: DX12 through WARP, software, so GPU work is
grossly inflated; CPU-side costs are real), process creation → first frame submitted, median of
the launches after the first (the first of a new exe is Windows Defender's scan, +200 to +1000 ms):

| Probe (a620e33) | Run 1 (10 launches) | Run 2 (5) |
| --- | --- | --- |
| win32-hello | 32.5 ms | 32.7 ms |
| rune-hello | 889 ms | 886 ms |
| arbor | 678 ms | 683 ms |

Where Arbor's time goes there (a620e33, medians): process creation → `main` 11.7 ms (loading the
24 MB exe and its 32 imported DLLs; win32-hello imports 12); `main` → Rune runs `init` 3.8 ms
(event loop); `init` 0.4 ms; → Rune runs `setup` 29 ms (Rune's font discovery, about 25 ms);
→ native window created ~610 ms (window, GPU adapter probing, device, renderer, surface: WARP);
first frame `update` 11 ms and `encode` 12 ms (software raster). Rune picked "Microsoft Basic
Render Driver" (lowest power).

A/B in one job (a slower VM: win32-hello 46 ms), 15 alternating launches, process → frame:

| Build | Total | Before `main` | `setup` at | Window at |
| --- | --- | --- | --- | --- |
| a620e33 (alpha.4's startup) | 840.7 ms | 11.7 | 46.3 | 800.6 |
| `rune::warm_gpu` first in `main` | **785.5 ms** | 12.1 | 58.0 | 746.7 |
| the same, 13 system DLLs `/DELAYLOAD`ed | 785.7 ms | 8.3 | 60.6 | 747.5 |

`warm_gpu` makes the GPU device and renderer on a thread from the top of `main`, so they overlap
the event loop, fonts and window (macOS's `prewarm_gpu` does nothing elsewhere). Delay-loading
moves 3.8 ms from before `main` into the startup that needs the same DLLs anyway: not kept.

**tabot-dev, Linux** (Xeon 8375C, Xvfb with lavapipe: software rendering again; release builds,
21 alternating launches in a 2-entry folder, spawn → first frame):

| Build | Total | `init` at | `setup` at | Window at | First `update` |
| --- | --- | --- | --- | --- | --- |
| before | 127 ms | 25.5 | 34.2 | 121.4 | 3.1 ms |
| `warm_gpu` | **101 ms** | 39.1 | 48.1 | 95.0 | 3.3 ms |
| rune-hello (no warm-up) | 129 ms | | | | |

On Linux the warm-up thread slows Rune's event loop by ~14 ms (the Vulkan loader `dlopen`s every
ICD while winit `dlopen`s Xlib and XKB: the dynamic loader's lock, most likely), and still wins
26 ms. Before the window: X11 event loop 25 ms, Rune's font discovery (directory walk, `fc-list`,
mapping fallback fonts) ~9 ms. A 10,000-entry folder adds ~7 ms to the first `update` (10.5 vs
3.1 ms), Rune copying every row key (RUNE-GAPS.md).

What a real GPU would see: lavapipe and WARP make the device, pipelines and frames slow; on real
hardware the device takes tens of ms, now hidden behind the 30–60 ms of CPU-side startup above,
so Linux would land around 45–60 ms and Windows with one GPU around 50–70 ms (estimates, not
measured). A hybrid laptop pays Rune probing both GPUs with a D3D12 device (twice) before the
window: hundreds of ms, the likely cause of the owner's second. Waiting on the owner's
`coldstart-probe.ps1` numbers.

## 2026-11-11, `feature/archives` (Compress and Extract Here)

Compress and Extract Here of a 1 GB folder of mixed files (21,620 files: 300 MB of Rust source
in 1,500 files, 200 MB of it in 20,000 small files, 300 MB of photos stored as they are, 200 MB
of incompressible binaries under unknown names; `ARBOR_BENCH_TREE=… cargo test --release
archive_throughput -- --ignored --nocapture`), release build on tabot-dev (Xeon 8375C, 4 vCPU,
NVMe), folder and archive in the page cache. The load average was 6–11 from other work, so
wall-clock times swung 2–3× between identical runs; CPU time on the worker's thread is the
steadier figure.

| Deflate level | Archive | Compress, CPU | Compress, wall | Extract, CPU | Extract, wall |
| --- | --- | --- | --- | --- | --- |
| 1 | 699 MB (70%) | 5.9 s (171 MB/s) | 5.6–16.8 s | 4.2 s (239 MB/s) | 3.9–9.3 s |
| **3 (chosen)** | 656 MB (66%) | 10.0–11.9 s (84–100 MB/s) | 7.5–26.5 s | 3.7–3.9 s (254–274 MB/s) | 2.7–12.6 s |
| 6 | 651 MB (65%) | 13.9 s (72 MB/s) | 17.9–31.4 s | 2.5 s (395 MB/s) | 2.5–7.5 s |

A file over 4 GB (ZIP64; 4 GB with 40 MB of noise in it) went through Compress and Extract in
47 s (`a_file_over_4_gb_goes_through_zip64`, ignored by default).

No regression elsewhere: the binary is 330 KB bigger (40.25 → 40.59 MB, release, unstripped).
The bench under Xvfb (software rendering, not representative; 5 cold starts × 10 rounds,
`--idle 0`, the same harness with `--bin`), main (ffb96d0) and this branch run alternately:

| Run | Cold start, median | Listing (10k), median | Key → frame, median |
| --- | --- | --- | --- |
| main | 434.9 ms | 139.8 ms | 11.1 ms |
| branch | 466.2 ms | 143.7 ms | 10.9 ms |
| main | 152.9 ms | 53.2 ms | 8.4 ms |
| branch | 145.6 ms | 50.5 ms | 8.8 ms |

The first pair ran while the box was busiest. Nothing on these paths changed: the context menu
gains a row, and the palette's check for Extract Here reads the selected names' extensions
without allocating.
## 2026-11-11, multiple windows (`feature/new-window`)

tabot-dev (Xeon 8375C, 4 vCPU, Xvfb with lavapipe: software rendering, so frame and window
costs are inflated; load average 1.7–3.8), release builds, main at 9e4f168 ("base") against the
branch ("new"), `bench 11 10 --idle 0`, alternating:

| Measure | base | new | new | base |
| --- | --- | --- | --- | --- |
| cold start, median / p95 | 141.4 / 266.0 ms | 140.6 / 144.6 ms | 141.7 / 152.8 ms | 140.1 / 182.2 ms |
| 10k listing, nav → frame | 46.6 / 85.3 ms | 43.1 / 48.3 ms | 43.8 / 49.4 ms | 44.2 / 48.3 ms |
| key → frame | 6.9 / 11.2 ms | 6.8 / 7.9 ms | 6.8 / 7.8 ms | 6.8 / 7.4 ms |

A second pair, busier (load 3.4–3.8): base 149.6 ms cold, 53.3 ms listing, 8.6 ms key; new
144.8, 51.0, 8.4. Cold start and keys are unchanged: the first window does nothing new but
register its handle.

`bench 2 10 --window 4 --idle 1` (⌘N on the 10k folder to the new window's first frame showing
it, 40 windows): **90.7 ms median, 111.9 ms p95: over the 50 ms listing budget here**, with the
listing in the window's first frame 40 of 40 times. The time is the native window, not Arbor:
`ARBOR_TIMING`'s `window-up` line splits it, and over 10 windows each ⌘N's handler took
0.08–0.40 ms (`made_ms`; it was 8.2 ms while it waited a frame for the read, which a 10k folder
misses, so it no longer waits), Rune ran the window's `setup` 13–22 ms after ⌘N, and the first
frame (with the list built, `update_ms` 10–19) came at 78–156 ms. A window on a folder of two
files takes 59–83 ms the same way: about 60 ms is creating an X11 window and a lavapipe surface
in Rune, and the first window's own creation is about 115 ms of its 140 ms cold start. It
couldn't be measured on the Mac or Windows (no GUI over ssh, AGENTS.md); `bench --window` there
is the check.

Idle (`--idle 1`), CPU over 5 s: list 0.00 s, icons 0.00 s, **list with a second window open
0.00 s**. Live under Xvfb after opening, navigating, a new folder and an undo across two windows:
0 ticks of CPU over 5 s.

## 2026-11-11, idle CPU (the list view drew every frame while idle)

Since 7cd20cb, an idle list view drew a frame every refresh until the next key (RUNE-GAPS.md,
"Idle: a `Tx::live_only` that no commit follows…"). Launched on a folder of 3 JPEGs and 3 PNGs
with `view.default` set, left alone 15 s, then CPU time over 10 s (`ps -o time=`), release
builds on the M4 (macOS, 60 Hz display):

| Commit | list | icons |
| --- | --- | --- |
| 0ca5968 (before milestone 16) | 0.00 s | 0.00 s |
| e393af8 (milestone 16) | 0.00 s | 0.00 s |
| 7e356e3, 8f4152c, 1d7bae4, e1ac88f | 0.00 s | 0.00 s |
| **7cd20cb** (drag-select follows the wheel) | 0.89 s | |
| 9466d00 | 0.85 s | |
| 2beefb5 | 0.93 s | 0.00 s |
| 7390515 (v0.1.0-alpha.2's code) | 0.74–0.92 s | 0.00 s |
| this | 0.00 s, 0.00 s | 0.00 s, 0.00 s |

`bench 1 2 --idle N` (after As Icons, and As List for list; CPU over 5 s, at most 0.05 s):

| Machine | before (this commit without its view change) | this |
| --- | --- | --- |
| M4 (macOS), `--idle 3` | not run | list 0.00, 0.00, 0.00 s; icons 0.01, 0.00, 0.00 s |
| tabot-dev (Xvfb, lavapipe), `--idle 1` | list 8.88 s, icons 0.00 s | list 0.00 s, icons 0.00 s |

The icon view was spared while thumbnails were on screen: they're live sources, and latching
them clears the flag that kept the list view drawing.
## 2026-11-11, `feature/views` (As Columns and As Tiles)

What could cost the list view time: a check per key and per `poll` for panes showing columns
(`State::sync_columns`, one comparison when none does), one flag per row (`Ctx::narrow`), and
the `preview` sync's one more call. The column and tiles views cost nothing until used.
Measured against c3433d3 (main, the commit before the branch), on tabot-dev under load from
other builds (load average 4–7), so every number below is noisier than usual.

### tabot-dev (Xvfb, lavapipe; not representative)

`bench 8 10 --bin …`, the two release binaries alternating, two passes each:

| Measure (median per pass) | c3433d3 | this |
| --- | --- | --- |
| Cold start (spawn → first frame) | 153.7, 154.1 ms | 159.7, 151.6 ms |
| Listing visible (10k, nav → frame) | 50.7, 52.4 ms | 53.1, 48.9 ms |
| Key → frame (synthetic ↓) | 8.4, 8.5 ms | 8.7, 8.6 ms |

`frame_cost` (Arbor's own work per key, no GPU), the two test binaries alternating: medians
1.944, 1.968, 1.948 ms before and 1.980, 1.956, 1.949 ms after. No difference beyond noise.
The release binary: 40,091,344 bytes before, 40,190,136 after (+0.25%).

The new views, `bench 1 10 --columns 5 --tiles 4`: five cold starts in a folder of 10,000
files as columns, each round ← to its parent (the 10k folder becomes the column on the right),
↓ onto the next folder there and ↑ back onto the 10k folder (each read for the column on the
right), then → back into it; and four paging the tiles view of the 10k folder with Page Down
and ↓. A key that started a folder read (`reads=1`) is held to the listing budget; one that only
moved listings on screen, to the key's. After review (the numbers before it were 15.0 ms ← and
15.7 ms →, all held to 50 ms):

| Measure | n | median | p95 | budget |
| --- | --- | --- | --- | --- |
| Tiles, 10k entries: key → frame (Page Down, ↓) | 80 | 10.3 ms | 14.1 ms | 8 ms (over) |
| Columns: ← and → over 10k, no read (key → frame) | 100 ×3 | 8.8, 7.3, 7.2 ms | 12.8, 11.9, 12.3 ms | 8 ms (p95 over) |
| Columns: ↓ and ↑ onto a folder, read (key → frame) | 100 ×3 | 8.0, 8.0, 8.3 ms | 12.9, 12.3, 12.8 ms | 50 ms (under) |

Three passes after rebasing on 9e4f168 (load 2–3). The list's ↓ was 8.5, 8.3 and 8.9 ms in the
same passes: lavapipe's encode is 4–6 ms of every frame here, and Arbor's own update for ← and
→ is 2.2 and 4.1 ms (`ARBOR_TIMING`). `--idle`: list, icons, tiles and columns each 0.00 s of
CPU in 5 s. Tiles paging
is in line with the icon view's (milestone 16: 6.2 ms on a quiet machine, when the list's ↓ was
6.2 too).

`column_cost` (CPU only, headless, the 10k folder of `frame_cost`):

| Measure | 7b3a373 | this |
| --- | --- | --- |
| Columns: ← out of 10k (key → committed frame) | 8.40 ms | 1.29 ms |
| Columns: → into 10k | 8.91 ms | 2.32 ms |
| Columns: End onto a folder (a read) | — | 2.30 ms |
| List view: the frame landing the same 10k listing (its read left out) | 10.39 ms | 7.67 ms |

After the second review, on the Mac (MacBook Pro, M4 Pro; headless over ssh, CPU only, no
GPU: `cargo test --release -- --ignored --nocapture --test-threads=1 column_cost frame_cost`),
which keeps Arbor's own cost apart from tabot-dev's lavapipe and its load (16 at times):

| Measure (median, p95) | 1k entries | 10k | 30k |
| --- | --- | --- | --- |
| Columns: ← out of the folder | 0.45, 0.48 ms | 0.45, 0.47 ms | 0.45, 0.48 ms |
| Columns: → into it | 0.81, 0.86 ms | 0.82, 0.85 ms | 0.86, 0.90 ms |
| Columns: End onto a folder (a read) | 0.76, 0.79 ms | 0.77, 0.81 ms | 0.83, 0.87 ms |
| List view: the frame landing the folder's listing | 1.50, 1.58 ms | 2.65, 2.73 ms | 5.37, 5.45 ms |

A column move costs the same whatever the folder's size (nothing in it is per entry any more);
the list's ↓ (`frame_cost`, 10k) is 0.68 ms there. The p95 over 8 ms on tabot-dev is that host:
its minimums are as flat (← about 2 ms, → about 3.5 ms at every size, at load 16).

What changed: each column is one element, keyed by its number, whether it's the active column
or another, and a listing moves between a column and the folder with its rows and their
version, so Rune doesn't build a new list or key its 10k rows (it did both on every move, 0.5 ms
per 1,000 entries), and Arbor neither copies, sorts nor filters them again. The bench also lets
a column's read land, in a frame of its own, before the next key: otherwise → paid for drawing
the 10k folder read by the ↑ before it for the first time (12.9 ms of update instead of 4.1).
The bench's folder is the columns' sidebar place: under the disk's root, `/tmp` (60,594 entries
on this machine) was a column too.

## 2026-11-10, milestone 16 (drag-select, the icon view and its thumbnails)

What could cost the list view time: each unselected row's name now carries the item's press
and drag handlers (the row keeps a drag for the selection rectangle), and the view reads the
list's laid-out rect once. A first version wrapped each name in one more row with a spacer: that
cost 0.2–0.3 ms per key and about 1 ms per 10k listing on tabot-dev, so the handlers moved onto
the name text itself (no element more than before). The icon view, its thumbnails and the
rectangle cost nothing until used. Measured against 0ca5968, the commit before it.

### tabot-dev (Xvfb, lavapipe; not representative)

`target/release/examples/bench 8 10 --bin …`, the two release binaries alternating, two passes
each (load average about 1.5):

| Measure (median per pass) | 0ca5968 | this |
| --- | --- | --- |
| Cold start (spawn → first frame) | 135.7, 129.9 ms | 136.0, 132.4 ms |
| Listing visible (10k, nav → frame) | 39.9, 40.3 ms | 40.0, 40.2 ms |
| Key → frame (synthetic ↓) | 1.4\*, 6.2 ms | 6.2, 6.2 ms |

\* lavapipe's key times are bimodal (about 1.4 or 6.2 ms); that pass's p95 was 6.5 ms, as the
others'. The release binary: 39,935,120 bytes before, 40,052,720 after (+0.3%).

The icon view, `bench 1 10 --grid 4` (four cold starts in the 10k folder, icons only, and four in
a folder of 1,000 photo-like 1600 × 1200 JPEGs; each switches to the icon view, then pages with
Page Down and ↓):

| Measure | n | median | p95 |
| --- | --- | --- | --- |
| Grid, 10k entries: key → frame (Page Down, ↓) | 80 | 6.2 ms | 9.0 ms |
| Grid, photos: key → frame while thumbnails decode | 68 | 13.3 ms | 17.3 ms |
| 1,000 photos: As Icons → first thumbnail | 4 | 52.7 ms | 56.3 ms |
| 1,000 photos: As Icons → the screen's 30 thumbnails | 4 | 364.9 ms | 394.7 ms |

Over budget here, and why it doesn't carry over: with `ARBOR_TIMING`, the keys while thumbnails
decode split into about 5 ms of update and 6 ms of encode. The encode is lavapipe rasterising the
frame and uploading the new thumbnails in software; the update is the view and layout competing
for this machine's 4 vCPUs with two decoding threads and lavapipe's own. The workers now run
below the window's priority (as the search's walkers), which took the average from 11.8 to
10.5 ms. On the Mac (below) the same keys take 1.5 ms.

### MacBook Pro, Apple M4 Pro, macOS 27.2 (over ssh, display on)

`target/release/examples/bench 1 10 --grid 3`:

| Measure | n | median | p95 |
| --- | --- | --- | --- |
| Grid, 10k entries: key → frame (Page Down, ↓) | 60 | 1.5 ms | 2.5 ms |
| Grid, photos: key → frame while thumbnails decode | 51 | 1.5 ms | 2.5 ms |
| 1,000 photos: As Icons → first thumbnail | 3 | 55.0 ms | 68.0 ms |
| 1,000 photos: As Icons → the screen's 30 thumbnails | 3 | 150.6 ms | 185.7 ms |

## 2026-11-09, milestone 14 (Empty Trash, Put Back, tab drags and moves, universal Mac build)

What could cost time: one more sidebar row (the Trash) built every frame, a drag handler on
each tab built, and three more commands in the table. Emptying, counting and Put Back cost
nothing until asked for. Measured against 0990d0f, the commit before it, on tabot-dev.

### tabot-dev (Xvfb, lavapipe; not representative)

`xvfb-run /tmp/bench 11 10 --tabs 3 --bin …`, the two release binaries alternating, three passes
each. The machine was very busy (load average 8.4–9.8 at the start, 4.1–8.0 later), so the
spread is wide:

| Measure (median per pass) | 0990d0f | this |
| --- | --- | --- |
| Cold start (spawn → first frame) | 234.0, 381.7, 300.0 ms | 213.4, 381.1, 319.2 ms |
| Listing visible (10k, nav → frame) | 100.1, 107.9, 107.2 ms | 66.0, 100.6, 104.4 ms |
| Key → frame (synthetic ↓) | 19.6, 17.4, 14.5 ms | 12.1, 16.9, 16.3 ms |
| Tab switch to 10k (key → frame) | 93.0, 99.0, 100.2 ms | 100.2, 99.3, 102.0 ms |

`frame_cost` (Arbor's own work per key, no GPU), the two test binaries alternating: medians
1.816, 1.830, 1.844, 1.901 ms before and 1.861, 1.833, 1.866, 1.867 ms after (one pass of
each ran at 3.0 ms while the load spiked, and is left out). No difference beyond noise.

The release binary: 39,704,328 bytes before, 39,876,880 after (+0.4%).

## 2026-11-09, milestone 13 (consolidation, no behaviour change)

Files split into folders, shared modules extracted (`persist`, `bounded`, `mounts`, `ffi`), no
logic changed. Measured against 39782cd, the commit before it, on tabot-dev.

### tabot-dev (Xvfb, lavapipe; not representative)

`xvfb-run /tmp/bench 11 10 --preview 3 --bin …`, the two release binaries alternating, three
passes each (load average 0.6–2.8):

| Measure (median per pass) | 39782cd | this |
| --- | --- | --- |
| Cold start (spawn → first frame) | 135.5, 134.3, 134.7 ms | 128.8, 135.5, 138.7 ms |
| Listing visible (10k, nav → frame) | 42.7, 41.6, 42.9 ms | 42.6, 41.1, 42.5 ms |
| Key → frame (synthetic ↓) | 6.3, 6.4, 6.4 ms | 6.4, 6.5, 6.4 ms |
| Preview 256 KB text (Space → frame) | 15.0, 15.2, 15.3 ms | 15.2, 14.9, 15.2 ms |
| Preview 12 MP JPEG (Space → frame) | 111.2, 113.3, 110.7 ms | 111.3, 110.3, 112.9 ms |
| Key → frame, preview open | 4.9, 5.1, 5.1 ms | 5.3, 5.1, 5.4 ms |

`frame_cost` (Arbor's own work per key, no GPU), the two test binaries alternating: medians
1.789, 1.833, 1.825 ms before and 1.817, 1.812, 1.813 ms after. No difference beyond noise.

The stripped `dist` binary: 26,915,384 bytes before, 26,914,488 after (−0.003%).

## 2026-11-09, milestone 12 (sidebar favorites, Locations and Eject, Open With, Get Info)

What could cost time: at startup, reading `sidebar.toml` (on the thread that reads the first
folder, never the window's), then after the first frame one worker reading the volumes and one
`stat` per favorite, each on its own thread; on every frame, the sidebar's section headers, its
scroll container and the Locations rows; on every key, `sync_info` (a `None` check while Get
Info is closed) and a drag handler on each row built. Open With and Get Info cost nothing until
opened.

### Arbor's own work per key, without the GPU (`frame_cost`, tabot-dev)

Both test binaries kept and alternated (`test-base` from 820f79c, `test-new` this commit,
`--ignored --nocapture frame_cost`), five passes each. The machine was busy (load average
4.5–5.5, someone else's `rustc`), so each build swung between two levels:

| Build | median (ms), in run order |
| --- | --- |
| 820f79c | 2.986, 1.874, 2.627, 3.004, 1.837 |
| this | 1.844, 1.844, 3.059, 2.081, 1.918 |

Best medians 1.837 ms and 1.844 ms: no difference beyond noise.

### tabot-dev (Xvfb, lavapipe; not representative)

`xvfb-run target/release/examples/bench 11 10 --bin …`, alternating, three passes each, load
average 4.1–4.8 throughout:

| Measure (median / p95) | 820f79c (3 passes) | this (3 passes) |
| --- | --- | --- |
| Cold start (spawn → first frame) | 140.4–213.9 / 216.7–259.7 ms | 162.6–221.4 / 219.6–241.8 ms |
| Listing visible (10k, nav → frame) | 44.3–63.3 / 65.2–96.3 ms | 56.1–71.9 / 80.8–118.9 ms |
| Key → frame (synthetic ↓) | 6.5–11.3 / 11.0–20.5 ms | 9.0–12.3 / 13.9–23.3 ms |

The spread between passes of one build (cold start 140–214 ms for 820f79c) is larger than any
difference between builds; this run can't show a change either way, and the headless numbers
above are the measurement. Nothing new runs on the window's thread before the first frame: the
sidebar file is read with the settings, and the volumes and favorites' looks start from the first
`poll`, on workers.

### Get Info's total

Get Info on `/usr` (118,628 items, 3.0 GB, one file system) under Xvfb: the count streamed in
("7,041 items · 414 MB so far…" 120 ms after ⌘I) and had finished by the second shot 8 s later on the busy machine,
matching `find /usr -xdev | wc -l` and `du -sb --apparent-size /usr` exactly.

## 2026-11-09, milestone 11 (accessibility, go-to completion, redo, one Arbor on Windows)

What could cost time on every key: the roles, states and names the view now gives assistive
technology (a role and a selected flag per row, `a11y_hidden` on decoration, a few dozen short
labels for the window's chrome), the file list being focusable with its own key handler, and
its active descendant (one string a frame). Rune builds the accessibility tree only when a
client asks (`nothing_is_built_until_a_client_asks`), so none of the tree is built here.
Completion and redo cost nothing until used.

### Arbor's own work per key, without the GPU (`frame_cost`, tabot-dev)

`cargo test --release -- --ignored --nocapture frame_cost`: a headless runtime on a
10,000-entry folder, 1,200 keys (↓, and ↑ now and then), each timed from the key to the
committed frame (handler, view, layout, commit; no rendering). Steadier than the windowed bench
on a busy machine. The previous commit (41fed24, with the test file copied in) and this one, three
runs each:

| Build | median (ms) | p95 (ms) |
| --- | --- | --- |
| 41fed24 | 1.736, 1.768, 1.739 | 1.880, 1.886, 1.908 |
| this | 1.784, 1.790, 1.780, 1.808, 1.814, 1.813, 1.787 | 1.881, 2.194, 1.950, 1.884, 1.897, 1.916, 1.881 |

About 0.04–0.05 ms more a key (2–3% of Arbor's own work, 0.6% of the 8 ms budget). Taking the
rows' accessibility attributes out recovered about 0.02 ms of it (1.773–1.798 ms), and the list's
focus and key handler nothing measurable; the rest is the chrome's labels and Rune's handling of
the extra properties. Rune can't build labels lazily (RUNE-GAPS.md), so this is what being
readable costs everyone; the rows themselves are named from text they already draw.

### tabot-dev (Xvfb, lavapipe; not representative)

`xvfb-run target/release/examples/bench 11 10 --bin …`, alternating, five passes each. The
machine was busy throughout (load average 2.1–3.4, other users' processes), and the spread
between passes of the same build (key median 2.1–6.4 ms for 41fed24) is larger than any
difference between builds:

| Measure (median / p95) | 41fed24 (5 passes) | this (5 passes) |
| --- | --- | --- |
| Cold start (spawn → first frame) | 128.7–155.3 / 141.2–241.9 ms | 134.3–166.3 / 141.9–220.0 ms |
| Listing visible (10k, nav → frame) | 42.2–53.6 / 46.1–72.8 ms | 42.2–52.6 / 51.9–76.8 ms |
| Key → frame (synthetic ↓) | 2.1–6.4 / 7.0–12.4 ms | 6.3–8.2 / 8.5–12.7 ms |

No change shows through the noise at the median; the headless numbers above are the
measurement. Cold start does no new work (Windows' single-instance check runs only with a
folder on the command line, and costs one failed pipe open when no Arbor is running).

## 2026-11-09, milestone 10 (packages, the folder default, requests from other apps)

What could cost start-up time: on Linux a thread started in `main` that reads one file (whether
Arbor's D-Bus activation file is there) and ends; on macOS an Apple-event handler installed when
AppKit finishes launching; `rune::set_app_id`. The previous commit (c04e467) and this one,
alternated, `xvfb-run target/release/examples/bench 11 10 --bin …`, two passes each:

### tabot-dev (Xvfb, lavapipe; not representative)

| Measure (median / p95) | c04e467, pass 1 | this, pass 1 | c04e467, pass 2 | this, pass 2 |
| --- | --- | --- | --- | --- |
| Cold start (spawn → first frame) | 136.1 / 143.1 ms | 134.9 / 140.0 ms | 133.4 / 139.8 ms | 131.9 / 141.0 ms |
| Listing visible (10k, nav → frame) | 40.0 / 45.7 ms | 39.8 / 44.5 ms | 39.7 / 46.0 ms | 40.8 / 45.2 ms |
| Key → frame (synthetic ↓) | 6.2 / 6.8 ms | 6.2 / 6.9 ms | 6.2 / 6.7 ms | 6.2 / 6.8 ms |

No change beyond noise. When Arbor is started by D-Bus activation, the bus thread takes the name
before the window opens, so the request lands with the first listings (the activating call
returned in 7 ms under Xvfb).

## 2026-11-09, the Mac's floor, Batch Rename on the Mac, Jump's long paths, the bench's hangs

From the owner's Mac investigation (MacBook Pro, M4 Pro, macOS 27.2, external 60 Hz display
awake, Brave and WindowServer busy throughout), plus the fixes it led to.

### Cold start: the macOS budget is met

Spawn → first frame submitted, back-to-back, 20 launches after a warm-up, display awake:

| App | Median / p95 |
| --- | --- |
| Bare AppKit + Metal (NSWindow, CAMetalLayer, one clear pass) | 125.5 / 126.8 ms |
| Bare Rune (one empty element; Arbor's prewarm, chrome and inactive open) | 126.4 / 131.8 ms |
| **Arbor** (10k folder, 110df34) | **132.6 / 147.5 ms** |

NORTHSTAR's macOS budget is "within 25 ms of a bare native window": Arbor is **+7 ms**, so it's
**met**. macOS itself takes ~87 ms before `applicationDidFinishLaunching` and ~32 ms more to make
and show a window, even bare. Rune adds ~1 ms; Arbor ~6 ms, nearly all its first view build
(`update_ms` 5.3). A second of idle between launches adds ~45 ms to every app alike (AppKit
175, Rune 168, Arbor 168 ms), so never compare a paced pass with a back-to-back one.

An earlier 145.5 ms run that evening was noise, not a regression: four commits alternated (bench
`11 1`, four passes each) gave pass medians spread 7 ms within each build and means of 138.3
(9eecac3), 134.0 (3a357b1), 135.2 (8a3d86d) and 138.1 ms (110df34), with the same splits in each
(`init_at` 70–76 ms, `init` 0.2 ms, first `update` 5.2–5.3 ms).

### Batch Rename on the Mac (10,000 items, 110df34, `--rename 5`, 2 passes)

| Measure (median / p95) | Back-to-back | CPU kept hot |
| --- | --- | --- |
| Key → preview, plain | 2.6 / 2.8 ms | 2.3 / 2.8 ms |
| Key → preview, regex | 17.4 / 21.8 ms (the CPU clocks down after the first rounds) | 4.1 / 4.5 ms |
| Key → frame, typing | 4.5 / 5.0 ms | 4.0 / 4.5 ms |

Plain previews land in the key's own frame; regex ones do for the first rounds, then go to the
worker once the CPU has clocked down (the key's frame at ~4.9 ms including the 3 ms bounded wait,
the preview ~17 ms later). At human typing pace the cooled numbers are the real ones.

### Jump: long paths cut before Rune lays them out (191cca7)

On the Mac, 45% of a Jump frame with a fresh query was Rune's head truncation, which shapes each
long path about 8 times (RUNE-GAPS.md): keys took 6.3–8.6 ms of `update_ms` hot and 14–19 ms
cooled, against 1.1–1.7 ms for short paths. Arbor now cuts paths to whole trailing folders that
fit before Rune sees them. On tabot-dev (Xvfb; CPU-bound `update_ms`, 12 rounds × 2 keys, fresh
query each round, 5,000 folders all ~100+ characters, alternating, 2 runs each):

| Jump key `update_ms` | Without the cut | With it |
| --- | --- | --- |
| Median | 9.4, 9.8 ms | 5.9, 6.3 ms |
| p90 | 13.8, 15.5 ms | 8.3, 10.0 ms |

With the bench's own realistic history (most paths fit whole; p90 length 90 characters) the gain
is smaller (median 7.1–7.6 → 6.0–6.7 ms). **The Mac needs measuring**, back-to-back and paced:
`scripts/bench.sh 1 12 --jump 5` and `scripts/bench.sh 1 12 --jump 5 --pace 120 --gap 1000`.

### The bench's 60 s hangs (26755b3)

13 of 63 Jump and tab runs (and every run with the display asleep) hung until the timeout: a wake
from a frame that wasn't drawn was handled in the same run-loop turn, so AppKit never reported
the window visible (RUNE-GAPS.md). The owner's one-line patch (wake once per run of undrawn
frames) took that to 0 of 12. The bench now never wakes from an undrawn frame and steps only
after the last step's frame was drawn; on tabot-dev every run since has been valid (24 Jump and
rename runs, back-to-back and paced). Not yet re-run on the Mac.

## 2026-11-08, rich previews through the OS's thumbnailer (milestone 9)

What changed on the measured path: nothing at startup or per key. The preview's worker now waits
on the OS for formats it doesn't decode, and every request, cancel or close rings one more
condition variable (`thumb::Bell`, uncontended: a lock and a notify). Closed, the preview still
costs one `None` check. Before is 110df34, after is 823c237, the same bench (`11 10`) alternating.

### tabot-dev (Xvfb, lavapipe; not representative)

Load average 3.2–5.3. Order: before, after, after, before, before, after.

| Measure (median / p95, submission proxy) | Before (3 passes) | After (3 passes) |
| --- | --- | --- |
| Cold start (spawn → first frame) | 134.5 / 192.5, 132.6 / 139.5, 134.2 / 194.0 ms | 132.3 / 141.8, 136.8 / 140.0, 127.3 / 143.3 ms |
| Listing visible (10k) | 44.2 / 58.6, 42.0 / 45.4, 39.4 / 45.2 ms | 40.1 / 45.4, 40.8 / 44.8, 42.1 / 45.1 ms |
| Key → frame (synthetic ↓) | 6.5 / 9.5, 6.2 / 6.8, 6.2 / 6.8 ms | 6.2 / 6.6, 6.2 / 6.6, 6.2 / 6.6 ms |

No change beyond noise.

### The OS's thumbnailer (`os_thumbnail_speed`, CI runners)

`cargo test --release -- --ignored --nocapture os_thumbnail_speed`, run by CI's "OS thumbnail
timings" step on GitHub's runners (run 42519157218), panel size 1024 × 1024. Videos made there with
ffmpeg's test pattern; the PDF is the tests' one-page PDF (619 bytes). One pass each.

**Windows** (windows-latest, Windows Server 2025), `IShellItemImageFactory::GetImage`. "Generate"
is the full path, which can't be cancelled once it starts (`UNCANCELLABLE_WINDOWS_THUMBNAILS`, off):

| File | Cache only, new file | Generate, new file | Cache only, after | Generate, after |
| --- | --- | --- | --- | --- |
| H.264 MP4, 1080p, 10 s, 7.7 MB | 6.0 ms, none | **541 ms**, 1024 × 576 | 8.1 ms, 1024 × 576 | 7.0 ms |
| H.264 MP4, 4K, 120 s, 906 MB | 1.1 ms, none | **174 ms**, 1024 × 576 | 7.5 ms, 1024 × 576 | 7.1 ms |
| VP9 WebM, 1080p, 10 s | 3.3 ms, none | 92 ms, none (no handler) | 1.1 ms, none | 19 ms, none |
| PDF, one page | 47 ms (first call: COM and the shell start), none | 24 ms, none (no PDF handler on the runner) | 1.8 ms | 3.0 ms |

The first video (541 ms) includes Media Foundation starting in the thumbnail handler's process;
the second, much bigger one took 174 ms. These are a fast runner SSD: the handler reads the file
itself, so a slow or network disk stretches the uncancellable part, and the preview's one worker
waits behind it (the next preview can't start until it returns). A machine with a PDF handler
(Adobe Reader, some Office installs) wasn't available to measure.

**macOS** (macos-latest runner), Quick Look: the one-page PDF in 31.9 ms the first time, 6.3 ms
again (792 × 1024). Quick Look requests are cancelled (`cancelRequest:`), so none of this goes on
after the selection moves.

## 2026-11-08, Batch Rename's frame cost (milestone 7 review)

The review measured 15–16 ms a key typing in Batch Rename over 10,000 items here: update 5.3 ms,
encode 10.9 ms. `encode_ms` is Rune's CPU time from the end of the update to submission
(uploads, prepare, draw); under lavapipe it includes rasterizing in software what the frame
changes, so it follows how much of the window a key changes, not Arbor's work. Taken apart with
the same driver (`ARBOR_BENCH=12 ARBOR_BENCH_RENAME=1`, 12 rounds, the three typing keys of
each), one change at a time, load 2–3:

| Build | Encode, typing keys (median) |
| --- | --- |
| As it was (card shadow: offset 10, blur 30) | 14.2 ms |
| No card shadow | 12.5 ms |
| No shadow and no preview rows | 11.2 ms |
| No shadow, no rows, not modal | 11.1 ms |
| For comparison: Select All in the folder, no dialog (every visible row changes) | 8.1 ms |

So the dialog itself costs a few milliseconds over a frame that redraws the list, and most of
that is its shadow and its rows; the rest is lavapipe drawing what changed. The cheap cut:
overlay cards (Batch Rename, the palette, Jump, Select by Pattern, the confirm dialog) get a
smaller shadow (offset 4, blur 10). Alternating before and after, two passes each: encode 14.1 /
14.3 ms before, 13.2 / 13.4 ms after; the key to its frame 20.2 / 20.1 ms before, 18.5 / 19.4 ms
after (medians). The rows stay (only the 12 in view are built). The 8 ms budget is still missed
here; this is software rendering, and **the Mac still needs measuring** (`scripts/bench.sh 11
10 --rename 5`).

## 2026-11-08, Delete Permanently, trash that never deletes, Linux context menu (milestone 8)

What changed on the measured path: nothing runs at startup or per key that didn't before, apart
from the keymap picking each command's default keys for this platform (`Keymap::defaults`, 16 µs
for the whole table, `dispatch_speed`). Each key makes one more `None` check (the in-window
menu); the view one more. Trash and Delete Permanently run on the file operation worker. Before
is 117ae24, after is 8a29b81, both driven by the new bench.

### tabot-dev (Xvfb, lavapipe; not representative)

Busy: load average 5.3–6.4 (other agents on its 4 cores). Order: before, after, after, before.

| Measure (median / p95, submission proxy) | Before (2 passes) | After (2 passes) |
| --- | --- | --- |
| Cold start (spawn → first frame) | 140.5 / 152.9, 141.8 / 155.2 ms | 147.4 / 151.7, 146.4 / 177.5 ms |
| Listing visible (10k) | 47.2 / 60.8, 48.0 / 66.0 ms | 48.2 / 59.0, 49.3 / 59.7 ms |
| Key → frame (synthetic ↓) | 7.7 / 8.8, 8.0 / 11.7 ms | 8.0 / 8.9, 8.1 / 10.8 ms |

Cold start read 4–7 ms slower after in both passes, so it was checked twice more. Six more
alternations (11 cold starts each, load 3.5–4.0): after 148.4, 149.0, 145.1 ms; before 140.7,
143.0, 145.1 ms (median). Then `ARBOR_TIMING`'s own breakdown, six alternating pairs on a 10,000-
file folder: `main` → first frame 146.2 ms after against 146.7 ms before (medians), with Arbor's
own share the same in both (`init_ms` 0.1–1.2 ms, first view build `update_ms` 8.9–9.5 ms; the
rest is Rune and the window before `init`). Nothing in this milestone runs in that window beyond
the keymap's 16 µs, so the bench's gap is taken as the machine's load, not a regression; to be
re-checked on a quiet machine and on the Mac.

## 2026-11-08, WebP cap lowered to 4 MP (NORTHSTAR exception)

The owner lowered the WebP preview cap from 12 MP to 4 MP, lossy and lossless, after the 12 MP
numbers below came in. The exception (NORTHSTAR.md, Exceptions log) stays, with these numbers.

Same method as the 12 MP table (`webp_decode_speed`, median of 5 whole decodes, which is the
work that goes on after a cancel), on 2309 × 1732 images (3.999 MP, 4:3, just under the cap)
made with Pillow 10.2. Three passes on tabot-dev with load average 1.5–2.7. Peak RSS: the whole
preview path for one file under `/usr/bin/time -v`, against 4.5 MB for the test binary with no file.

| 4 MP WebP | File | Decode (3 passes) | Peak RSS |
| --- | --- | --- | --- |
| Lossy, random pixels with alpha (worst) | 7.4 MB | 567 / 389 / 327 ms | 34 MB |
| Lossy, random pixels | 3.4 MB | 505 / 312 / 313 ms | 29 MB |
| Lossy, photo-like (gradients) | 37 KB | 71 / 53 / 26 ms | 23 MB |
| Lossless, random pixels with alpha | 16.0 MB | 153 / 161 / 89 ms | 24 MB |
| Lossless, photo-like | 24 KB | 61 / 52 / 41 ms | 33 MB |

## 2026-11-08, milestone 6 review fixes (search)

What changed on the measured path: results carry the identity from the `lstat` the walk
already did (no new call), a big folder hands its matches over 256 at a time, the walk takes a
lock per folder to remember its identity (bind mounts), the root's device is looked up on the
walk's thread instead of in the key, and a walk still going after a file operation rechecks
the results that arrive under a changed path. Before is 931177e, after is 4908968, both driven
by the same bench (`--search 4`: four cold starts, 3 rounds each, in the 120,000-file tree),
alternating.

### tabot-dev (Xvfb, lavapipe; not representative)

Load average 2.2–2.4. Order: before, after, after, before.

| Measure (median / p95, submission proxy) | Before (2 passes) | After (2 passes) |
| --- | --- | --- |
| Search 120k: key → first results | 15.0 / 33.2, 16.1 / 30.7 ms | 18.0 / 26.3, 16.1 / 26.3 ms |
| Search 120k: key → walk done | 106.2 / 115.3, 109.8 / 162.2 ms | 107.7 / 169.5, 108.2 / 124.0 ms |
| Key → frame, query (starts the walk) | 6.0 / 13.4, 1.3 / 8.8 ms | 1.4 / 8.9, 5.2 / 11.2 ms |
| Key → frame while results stream | 9.5 / 13.6, 9.2 / 14.2 ms | 11.4 / 16.1, 9.0 / 16.0 ms |

No change beyond noise. With a 250 ms delay injected into the root's `stat`, the reviewer saw
six stalls typing `report`; the walk's thread now takes that delay (pinned by the test
`starting_a_search_never_waits_for_the_disk`, which holds the lookup for 3 s and fails if the
key waits for it).

## 2026-11-08, Batch Rename (milestone 7)

What changed on the measured path: nothing runs until Batch Rename opens. Each key and frame
makes one more `None` check (`State::batch`), and the binary carries regex-lite and the dialog.
Before is 6cd7541, after is 931177e, both driven by the new bench, alternating. New rows
(`--rename 5`: five more cold starts in the 10k folder, 10 rounds each; each round selects all
10,000 entries, opens Batch Rename, waits for its folder read, types `e` (plain text: every name
loses its e's), turns on regex and types `$` (`e$`), each waiting for its preview, and closes it):

- **rename 10k: key → preview**: the key to the frame showing the whole preview for it, plain
  (`e`) and regex (⌥R, then `$`).
- **key → frame, typing in rename**: those three keys' own frames (the preview may land later).

### tabot-dev (Xvfb, lavapipe; not representative)

Busy: load average 2.3–4.3 (other agents compiling on its 4 cores), so every number is well
above the earlier tables, both builds alike. Order: before, after, after, before.

| Measure (median / p95, submission proxy) | Before (2 passes) | After (2 passes) |
| --- | --- | --- |
| Cold start (spawn → first frame) | 192.1 / 226.9, 191.9 / 231.4 ms | 190.1 / 218.3, 204.0 / 219.6 ms |
| Listing visible (10k) | 59.6 / 79.9, 57.1 / 81.4 ms | 57.3 / 82.0, 56.0 / 81.2 ms |
| Key → frame (synthetic ↓) | 10.7 / 12.8, 10.7 / 12.8 ms | 10.6 / 12.3, 8.8 / 12.6 ms |
| **Rename 10k: key → preview, plain** | — | **37.3 / 62.4, 25.6 / 62.9 ms** |
| **Rename 10k: key → preview, regex** | — | **44.8 / 60.8, 44.4 / 63.9 ms** |
| **Key → frame, typing in rename (10k)** | — | **8.1 / 29.1, 8.1 / 31.1 ms** |

Re-checked once the machine was quieter (load 1.8–3.1), as twelve alternating passes of 21 cold
starts: before (6cd7541) against the final code (4908968, with the search fixes below too), in
the order before, after, after, before, before, after, then after, before, before, after,
after, before.

| Measure (median per pass, submission proxy) | Before (6 passes) | After (6 passes) |
| --- | --- | --- |
| Cold start (spawn → first frame) | 136.4, 130.6, 129.3, 133.1, 133.1, 134.5 ms (mean 132.8) | 133.8, 137.0, 135.1, 133.9, 134.2, 134.2 ms (mean 134.7) |
| Key → frame (synthetic ↓) | 6.1, 6.1, 6.1, 6.2, 6.2, 6.2 ms | 6.2, 6.4, 6.2, 6.2, 6.2, 6.1 ms |

No regression beyond noise: the second six passes are 0.5 ms apart on average, and the 1.9 ms
between the twelve passes' means is well inside either build's own spread (7 ms before, 3 ms
after). Nothing of regex-lite or the dialog runs at startup.

The previews over 10,000 names land a frame or two after the key, here. `ARBOR_TIMING` splits
them: a key typed in the dialog takes 4–5 ms of update (the view with its visible preview rows;
the handler itself is under 0.1 ms plus the wait below), and lavapipe's encoding of the frame is
0.2 ms, or 12–24 ms on frames whose preview rows change (new glyphs, drawn on the CPU). Up to
500 items are previewed inside the key (well under a millisecond); more go to a worker, which
the key waits at most 3 ms for. On a quiet machine a plain-text change to 10,000 names makes
that (about 2.5 ms; the key's own frame shows it), but not under this load, nor a regex (about
20 ms on a quiet machine, twice that here), which shows the last preview until the new one
lands, a frame or two later. The Mac hasn't been measured (no GUI runs there this milestone).

Measured directly (`cargo test --release -- --ignored --nocapture plan_speed batch_speed
match_speed`):

| Work | Quiet (load 0.9) | Busy (load 3.5) | Where it runs |
| --- | --- | --- | --- |
| Plan 10,000 names: nothing typed / plain `Rome` → `Paris` | 2.4 / 3.5 ms | 4.0 / 5.3 ms | the key (≤ 500 items), else a worker |
| … a regex with two groups and `{n:4}` | 20.5 ms | 38–40 ms | a worker |
| … a template `{parent} {n:3}`, Title case | 3.9 ms | 7.5 ms | a worker |
| Open Batch Rename on 10,000 selected items | 2.3–3.3 ms (load 3.1) | 4.3–8.4 ms | the key; the folder read (one `read_dir`, 10,000 `lstat`s) lands 17–33 ms later from a worker |
| A regex over 50,000 names (regex-lite), against a glob | 20–39 ms against 0.8–1.9 ms | | not used: why Select by Pattern has no `re:` |

The plain-text Find doesn't use the regex engine (a byte search, ASCII case folding as
regex-lite's), which is 3–4 times faster over 10,000 names (12.3 → 3.5 ms).

**Regex crate:** regex-lite 0.1.9, one crate with no dependencies. `regex` would add
regex-automata, regex-syntax with its Unicode tables, aho-corasick and memchr (not measured
here). What Batch Rename needs is Perl syntax and captures over short names, which regex-lite
has; it lacks Unicode classes and Unicode case folding (`\w`, `\d` and case-insensitive
matching are ASCII only), which the README says.

Binary: release 37.92 → 38.23 MB unstripped, 28.66 → 28.93 MB stripped (+0.27 MB). Of that,
regex-lite's own symbols are 64 KB and `rename.rs` and `batch.rs` 47 KB; the rest is the dialog's
view, the operation and the generic code they instantiate.

## 2026-11-08, WebP previews up to 12 MP (NORTHSTAR exception)

Superseded: the cap is now 4 MP (above). Kept for the numbers that led to it.

The owner signed off an exception to the view-triggered work rule (NORTHSTAR.md, Exceptions log):
WebP previews go up to 12 MP, lossy and lossless, instead of 0.45 / 1.7 MP. image-webp can't
stop a decode partway, so a WebP you've moved on from still decodes to the end on the preview's
thread (one at a time; a newer request replaces any waiting one; the result is dropped). The
65,535-pixel side cap and the bitstream checks stay.

`ARBOR_WEBP_DIR=… cargo test --release -- --ignored --nocapture webp_decode_speed` on 4000 × 3000
images made with Pillow 10.2 (libwebp), median of 5 whole decodes, which is the work that goes on
after a cancel. Two passes on a busy tabot-dev (load average 4–5: other agents compiling on its 4
cores) and one on a quiet one (load 1.0). Peak RSS: the whole preview path (decode, then shrink to
the panel) for one file, `/usr/bin/time -v`, against 4.8 MB for the test binary with no file.

| 12 MP WebP | File | Decode, busy (2 passes) | Decode, quiet | Peak RSS |
| --- | --- | --- | --- | --- |
| Lossy, random pixels with alpha (worst) | 22.2 MB | 1,835 / 2,005 ms | 1,012 ms | 92 MB |
| Lossy, random pixels | 10.1 MB | 1,519 / 1,538 ms | 956 ms | 78 MB |
| Lossy, photo-like (gradients) | 57 KB | 305 / 480 ms | 200 ms | 60 MB |
| Lossless, random pixels with alpha | 48.0 MB | 349 / 579 ms | 352 ms | 55 MB |
| Lossless, photo-like | 4.5 KB | 84 / 121 ms | 82 ms | 88 MB |

## 2026-11-08, milestone 5 review fixes and Jump ranking

### MacBook Pro, Apple M4 Pro, macOS 27.2 (60 Hz display), at 6932cf2, run by the owner

`scripts/bench.sh 11 10 --tabs 5 --preview 5 --jump 5`, submission proxy. Measured before the
fixes below (they came after this commit).

| Measure (median / p95) | Result |
| --- | --- |
| Cold start (spawn → first frame) | 134.3 / 143.3 ms (`main` → first frame 125.6 ms) |
| Listing visible (10k) | 36.8 / 37.4 ms |
| Key → frame (synthetic ↓) | 0.7 / 0.8 ms |
| Tab switch to 10k | 36.3 / 37.3 ms |
| Preview 256 KB text (Space → frame) | 17.8 / 58.4 ms |
| Preview 12 MP JPEG | 83.8 / 83.9 ms |
| Key → frame, preview open | 0.2 / 0.5 ms (0.2 / 0.3 while the JPEG decodes) |
| Jump open, 5k history, cold | 10.1 / 10.9 ms |
| Jump open, 5k history, warm | 2.8 / **10.7 ms (p95 over 8 ms)** |
| Key → frame, typing in Jump (5k) | 2.2 / **8.7 ms (p95 over 8 ms)** |

Three of six preview runs were INVALID (80 keys, 10 photo previews, but 9 text previews). That
was the timing harness, not the app: `timing::frame` took what had landed before checking that
the frame was drawn, so a preview landing in a `Skipped` frame (no drawable in time) was never
logged, though the bench saw it land and moved on, and the next drawn frame showed it. Fixed in
eb14848: what lands waits for the next drawn frame. The skipped frame is most likely each
run's first text preview, whose frame loads the system's monospaced font; that same first
showing is the text preview's p95 (58 ms against 17.8), as on Linux.

Jump's p95: on the Mac, Jump frames' `update_ms` reached 7.9 ms with encoding under 0.4 ms.
Measured on tabot-dev, the ranking handler is only part of it (about 1 ms of a 3.3 ms update
on a 5k history); the rest is the view: Rune reading every row key again when the number of
rows changes, and the box's visible rows built again with their bold matches. 3d60593 cuts
both: a key that extends the query ranks only the last query's matches, rows share their
folder's strings instead of copying them, the bold ranges are worked out only for the rows
built, and the box ranks 200 rows instead of 500. Handlers (`rank_speed`, `open_speed`, 5k
history, tabot-dev under load 4.5):

| Work | Before | After |
| --- | --- | --- |
| Open (warm) | 0.50 ms | 0.17 ms |
| Rank, nothing typed | 0.67 ms | 0.08 ms |
| Type `a` / then `r` | 0.9 / 0.95 ms | 0.5 / 0.46 ms |

Under Xvfb, with the machine at load 5.5, typing's `update_ms` was lower in both alternating
pairs (median 3.1 and 6.2 ms against 3.7 and 6.7) and the warm open's too (3.9 and 6.1 against
6.1 and 6.6): too noisy to quote as a gain beyond "lower". **The Mac needs re-measuring**; it's
the number that decides the budget. If Jump is still over there, ranking can move to a worker
with the last rows shown until the new ones land.

### Preview: work after a cancel (tabot-dev)

`cargo test --release -- --ignored --nocapture gif_cancel_latency webp_decode_speed`:

| Work | Before | After |
| --- | --- | --- |
| 24 MP GIF told to stop 12 ms in: work after the stop | 191 ms (the review's measurement) | 0.17 ms median, 0.71 max (0.69 / 0.92 interlaced) |
| 24 MP GIF, whole decode | about 200 ms | 172 ms (269 interlaced) |
| WebP, slowest allowed (can't be stopped) | 24 MP allowed: 136 ms after a cancel in the review; 1,086 ms for a noisy lossy 24 MP here | 0.45 MP lossy with alpha: 30.4 ms; 1.7 MP lossless: 31.7 ms |

WebP decode cost per megapixel, measured (image-webp 0.2.4): lossy photo-like 44 ms, lossy
random pixels 63–67 ms (with alpha the slowest), lossless 8–11 ms photo-like, 17 ms random,
3 ms for a flat image. The caps are set from the slowest.

### Select by Pattern: the complex-glob benchmark (tabot-dev)

The earlier `match_speed` numbers (1.4 / 1.6 / 3.0 ms) fed uppercase names to a
case-insensitive pattern without lowercasing them, so `IMG_*1*.{png,jpg}` matched nothing and
looked fast. With lowercased names (as a listing's are) and each count checked, it matches
15,844 names and took 6.4 ms with the general matcher; anchored star patterns now match in one
pass (8522d0c). Best of 20, 50,000 names:

| Pattern | Matches | Time |
| --- | --- | --- |
| `*.jpg` | 16,667 | 0.41 ms |
| `IMG_*1*.{png,jpg}` | 15,844 | 1.02 ms (6.4 ms before) |
| `*a*b*c*d*` | 0 | 1.30 ms |

## 2026-11-08, recursive search (milestone 6)

What changed on the measured path: nothing runs until ⌘F. Each key and frame makes one more
`None` check per pane (`Folder::search`), the window's walking threads start with its first
search, and the binary carries the search code. Before is 9eecac3, after is dbef797, both
driven by the new bench, alternating. New rows (`--search 5`: five more cold starts, 10 rounds
each, in a tree of 120,000 small files in 2,000 folders two levels deep; each round opens
Search in This Folder, types `1`, which about half the names contain, presses ↓ once a frame
while results stream in, and leaves with Esc when the walk is done):

- **search 120k: key → first results**: the `1` to the frame showing the first batch.
- **search 120k: key → walk done**: the `1` to the frame after the walk finished (61,524 found
  of 122,040 items).
- **key → frame, query**: the `1` itself, which stops any walk, clears the list and starts one.
- **key → frame while results stream**: each ↓ pressed while batches were still landing.

### tabot-dev (Xvfb, lavapipe; not representative)

Load average 1.5–2.3. Order: before, after, after, before. Warm cache (no root to drop it).

| Measure (median / p95, submission proxy) | Before (2 passes) | After (2 passes) |
| --- | --- | --- |
| Cold start (spawn → first frame) | 132.2 / 140.1, 132.7 / 141.6 ms | 131.1 / 139.1, 131.8 / 136.7 ms |
| Listing visible (10k) | 39.5 / 45.0, 38.3 / 44.4 ms | 39.2 / 45.7, 41.1 / 44.8 ms |
| Key → frame (synthetic ↓) | 6.1 / 6.9, 6.0 / 6.5 ms | 6.1 / 7.2, 6.1 / 6.8 ms |
| **Search 120k: key → first results** | — | **18.8 / 31.0, 19.0 / 28.8 ms** |
| **Search 120k: key → walk done** | — | **107.6 / 135.1, 109.4 / 123.9 ms** |
| **Key → frame, query (starts the walk)** | — | **4.5 / 10.0, 0.7 / 7.8 ms** |
| **Key → frame while results stream** | — | **9.1 / 14.9, 9.3 / 14.2 ms** (330 and 336 keys) |

No regression in cold start, listing or keys. Keys while results stream are over the 8 ms
budget on lavapipe. Split with `ARBOR_TIMING`: the update (Arbor's work: taking the batch,
the view, layout) is 2–3 ms against about 1.6 ms for a ↓ without a search, and the rest is
lavapipe encoding the frame on the CPU (6–9 ms), on a 4-core machine whose cores the four
walking threads are also using. On a real GPU encoding is under a millisecond (0.7 ms for a
whole ↓ frame on the Mac), so the Mac should land near 2–3 ms; it hasn't been measured there
yet (no GUI runs on it this milestone), and that's the number that decides the budget.

Before the fix that's in this build, the same keys took 29.5 / 56.1 ms, growing with the
results (a frame's update 5 ms at 6,000 results, 54 ms at 60,000): Rune reads every row key of
a virtual list again whenever its row count changes (RUNE-GAPS.md), so a list that grew every
frame cost time in proportion to the results. Now the list takes in at most 1,000 results while
the walk goes on and the rest join in one step at the end (7–8 ms for 61,524 rows here). The
walking threads also run at a lower priority than the window. Three walking threads instead of
four (leaving a core for the window) made no measurable difference: 11.0 against 11.3–11.6 ms
median on a busier pass (load 3–5).

Walk speed by thread count, measured directly (`cargo test --release -- --ignored --nocapture
search_speed`, warm cache, median of 7, load 2.4–3.4):

| Tree | Query | 1 thread | 2 threads | 4 threads |
| --- | --- | --- | --- | --- |
| `vendor/` (567 MB, 26,857 items) | `mod.rs` (2,392 found) | 34.1 ms, first 5.2 ms | 25.3 ms, first 4.0 ms | **22.2 ms, first 2.7 ms** |
| `vendor/` | none found | 29.0 ms | 21.5 ms | **18.1 ms** |
| `~/.cargo/registry` (990 MB, 38,144 items) | `mod.rs` (3,558 found) | 82.6 ms, first 13.6 ms | 52.6 ms, first 8.4 ms | **48.6 ms, first 8.1 ms** |
| `~` (115,550 items) | `mod.rs` (4,895 found) | 203.4 ms, first 25.2 ms | 111.6 ms, first 14.2 ms | **79.2 ms, first 5.9 ms** |
| `~` | none found | 117.8 ms | 88.5 ms | **71.5 ms** |

Four threads finish 1.5–2.6× faster than one and show the first result sooner, so a window
walks on min(4, cores). A cold cache (a first search after boot, or a spinning disk) will be
slower and lean harder on the parallel reads; it couldn't be measured here without root.

Binary: release 37.67 → 37.81 MB unstripped, 28.46 → 28.58 MB stripped (+0.12 MB). No new crates.

## 2026-11-08, Jump to Folder and Select by Pattern (milestone 5)

What changed on the measured path: each navigation that lands now sends its folder to the Jump
history's writer thread (one channel send; the thread starts on the first one, and the window
does no disk I/O for it), and `State::new` builds an empty history (no read). Nothing else runs
until ⌘J or ⌘S. Before is be544e2, after is 1c22c98, both driven by the new bench (which now
gives every run its own empty settings folder through `RUNE_DIRS_ROOT`, so the navigations don't
land in a real history), alternating. New rows (`--jump 5`: five more cold starts, 10 rounds
each, with a history of 5,000 folders, each `<root>/home/<word>/<word>-<n>/<word>`, one line
each):

- **jump open, cold**: ⌘J to the frame showing the ranked folders, the first open of a run: the
  writer reads the 380 KB log and adds it up (about 10 ms on an idle core, more under load);
  the box waits 8 ms for it, then shows "Loading history…" and the rows land in a later frame.
- **jump open, warm**: later opens, which rank what's in memory.
- **key → frame, typing in Jump**: `a`, then `r`, each ranking all 5,000 folders again.

### tabot-dev (Xvfb, lavapipe; not representative)

Load average 3.5–4.2 (another job compiling). Order: before, after, after, before.

| Measure (median / p95, submission proxy) | Before (2 passes) | After (2 passes) |
| --- | --- | --- |
| Cold start (spawn → first frame) | 133.9 / 142.9, 132.0 / 159.0 ms | 134.6 / 142.7, 136.7 / 145.2 ms |
| Listing visible (10k) | 40.3 / 45.6, 40.9 / 47.1 ms | 40.9 / 45.7, 40.3 / 45.9 ms |
| Key → frame (synthetic ↓) | 6.4 / 7.0, 6.2 / 8.4 ms | 6.4 / 7.2, 6.2 / 7.0 ms |
| **Jump open, 5k history, cold** | — | **49.7 / 51.4, 49.8 / 51.3 ms** |
| **Jump open, 5k history, warm** | — | **11.4 / 12.5, 12.1 / 13.0 ms** |
| **Key → frame, typing in Jump (5k)** | — | **10.8 / 13.0, 11.4 / 13.4 ms** |

The cold-start medians differ by up to 4.7 ms between passes, so they were checked with three
more alternating pairs of 21 cold starts once the machine was quieter (load 1.6): before 135.2,
136.3 and 134.8 ms, after 136.3, 135.2 and 134.2 ms (p95 140.0–143.3 ms for both), key → frame
6.4–6.7 ms for both. No regression.

Jump on lavapipe is over the 8 ms key budget, and it's the drawing: with the machine idle,
`ARBOR_TIMING` splits a key typed in Jump into about 2.2 ms of update (the handler ranking 5,000
folders, about 1 ms, plus the view, layout and paint) and about 8 ms of lavapipe encoding the
frame, against 1.6 and 4.9 ms for a ↓ without the box. The extra encoding is the overlay card
(its large soft shadow, drawn on the CPU here); the command palette's card costs the same. On a
real GPU encoding is a fraction of a millisecond (0.7 ms for a whole ↓ frame on the Mac). The
Mac was offline, so Jump hasn't been measured there yet; that's the number that decides the
budget.

Cold open: 50 ms, of which the log's read and aggregation on the writer are about 10 ms when a
core is free; the rest is the box's first frames on lavapipe (the open frame showing "Loading
history…", then the frame with the rows). Once per session.

Measured directly (`cargo test --release -- --ignored --nocapture rank_speed open_speed match_speed`):

| Work | Time | Where it runs |
| --- | --- | --- |
| Add up a 5,000-line history log | 10–15 ms | the history's writer thread, first open only |
| Rank 5,000 folders for "a" / "proj arb" / nothing typed | 1.2 / 0.5 / 0.6 ms | each key while Jump is open |
| Count a 50,000-name folder's matches for `*.jpg` / `IMG_*1*.{png,jpg}` / `*a*b*c*d*` | 1.4 / 1.6 / 3.0 ms (the middle one matched nothing: see the review fixes above) | each key while Select by Pattern is open |

Binary: release 37.39 → 37.65 MB unstripped, 28.24 → 28.46 MB stripped (+0.21 MB). No new crates.

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

A later review (2026-11-08) held ↓ at 40 Hz for 5 s in a folder of 200 24 MP JPEGs with the
preview open, which keeps the worker decoding the whole time: key → frame median 21.4 ms, p95
35.4 ms under Xvfb and lavapipe on this loaded 4-core machine. That's software rendering under
heavy decode load, so it isn't representative, but it isn't a pass of the 8 ms budget either;
and none of these times are pixels on screen, which no measurement here has verified.

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
