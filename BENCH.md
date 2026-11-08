# Benchmarks

Arbor against the speed budgets in [`NORTHSTAR.md`](NORTHSTAR.md). Newest first. To reproduce:

```sh
scripts/bench.sh 11 10                         # 11 cold starts, 10 rounds each (opens windows: leave the machine alone)
xvfb-run -a scripts/bench.sh 11 10             # Linux without a display
cargo test --release -- --ignored --nocapture list_10k   # read + sort only, no window
```

Each run cold-starts the release build on a fresh 10,000-entry folder (250 folders, 9,750 files),
then 10 times leaves it, comes back and presses ↓, then quits (`ARBOR_BENCH`, see
`src/timing.rs`). The first run is reported on its own (below). All times end when the frame is
submitted to the GPU. The pixels reach the screen at the next display refresh, which Rune doesn't
report (see [`RUNE-GAPS.md`](RUNE-GAPS.md)), so true latency is up to one refresh more.

## 2026-11-08

### MacBook Pro, Apple M4 Pro, 24 GB, macOS 27.2 (the real target)

Window on a 60 Hz display. Release build (`dist` gave the same numbers).

| Measure | Median | p95 | Budget | Result |
| --- | --- | --- | --- | --- |
| Cold start (spawn → first frame) | 139 ms | 161 ms | 100 ms | **fail** |
| … of which `main` → first frame | 123 ms | 138 ms | | |
| Listing visible (10k entries, request → frame) | 36.9 ms | 37.4 ms | 50 ms | pass |
| Key → frame (synthetic ↓) | 0.7 ms | 0.8 ms | 8 ms | pass |
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
| Listing visible (10k entries) | 40.0 ms | 45.1 ms | 50 ms | pass |
| Key → frame (synthetic ↓) | 6.6 ms | 7.1 ms | 8 ms | pass |
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

- **Listing (37 ms on the Mac)** is about 18 ms reading (`read_dir` plus one `lstat` per entry on
  the worker), about 4 ms building the frame, and the rest is waking the UI thread and waiting
  for a 60 Hz refresh. That leaves 13 ms of headroom on a fast SSD. A slow disk or old laptop
  (NORTHSTAR milestone 3) will need a faster read. On macOS, `getattrlistbulk` returns names,
  types, sizes and dates for a whole folder in a few calls instead of 10,000 `lstat`s.
- **Key → frame** is synthetic: it starts when the handler is called, so it leaves out the OS
  input queue. With `ARBOR_TIMING=1`, real key presses are timed from the OS event's own
  timestamp (macOS) for anyone who wants to check by hand.
