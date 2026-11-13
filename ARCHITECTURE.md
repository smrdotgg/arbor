# Architecture

A map for finding your way. What Arbor does is in `README.md`; why, and what it must never do,
in `NORTHSTAR.md`. Each module's own `//!` comment says more about it.

## The shape

One UI thread; each window (Rune's `open_window`: real OS windows in one event loop) has its
own `State` (`src/app/`), and all of them share one `Shared` (`app/shared.rs`: the
file-operation slot, undo and redo, the clipboard's files, the Jump history). A window reaches
another only through that one's inbox (a message and a wake, run in its next `poll`). Rune
calls four functions on each window's `State`:
`view::view` (build the element tree from `State`, every frame), `State::on_key`,
`State::poll` (after a waker fires: land whatever background work finished) and `setup`.
Handlers change `State` and land the change with `tx.instant()` (nothing animates). Anything
that reads or changes the disk runs off the UI thread and lands through `poll`: listings, every
file operation (the inline rename, New Folder and New File too), Show Original's link read, Get Info's
details. What's left on the UI thread is small and named: starting other apps (`fs::open`,
reveal), creating the settings file when it's opened, and one `lstat` per item for a drop's
"same disk?" and for a pasted list of paths.

## Modules

A folder is a module whose parts got their own files; `mod.rs` holds its interface, and
`tests.rs` its tests.

| Area | Modules | What they own |
| --- | --- | --- |
| Entry | `main.rs` | Start-up order, the first window, exit drains |
| | `window.rs` | Every window's spec and callbacks, opening another (⌘N), the registry other threads route through (requests from other apps, volumes) |
| State | `app/` | `State` and navigation (`mod.rs`), keys and per-command `run`/`unavailable` (`dispatch.rs`), the startup read (`boot.rs`), what windows share and their inboxes (`shared.rs`) |
| | `folder.rs` | One folder on screen: listing, cursor, selection, filter, sort, history; `TabMemo` |
| | `panes.rs` | Tabs and split view |
| Commands | `commands.rs` | The command table (`Cmd`: id, title, default keys), `Keymap`, palette scoring |
| | `config.rs` | `config.toml`: settings and key bindings, the template |
| | `palette.rs`, `menu.rs`, `confirm.rs` | Palette, Linux's in-window context menu, the one dialog |
| File ops | `actions.rs` | UI side of file operations: clipboard, paste, rename, drag and drop, context menu |
| | `ops/` | The operation worker (`worker.rs`), undo and redo (`undo.rs`), batch rename (`batch.rs`), fingerprints (`print.rs`), Empty Trash and Put Back (`trashed.rs`), New File and Make Symbolic Link with their undo and Windows' shortcut fallback (`links.rs`), Compress and Extract Here (`archive/`) |
| | `leftovers.rs` | What an operation cut short by quitting left: the record, its writer, the next launch's note |
| | `archive/` | The archive formats Compress writes and Extract Here reads (`zip.rs`, `tar.rs`), what an archive's entries may become on disk (`mod.rs`: refusing paths and links out of the folder, renaming invalid names, the bounds against bombs), and writing them without following links (`out.rs`) |
| | `noclobber.rs` | Renames, copies and file ids that never replace anything |
| | `trash/` | Each platform's trash, and telling a disk with none (`win.rs`: the Recycle Bin); what's in the trashes, their count for Empty Trash's dialog, `.trashinfo` records (`contents.rs`) |
| | `delete.rs` | Permanent delete without following links |
| Features | `preview/` (+ `thumb.rs`) | Quick preview: dispatch (`mod.rs`), its thread (`worker.rs`), text, image decoders (`decode.rs`); OS thumbnails |
| | `search/` | Recursive search: the walk and its threads (`walk.rs`), results and rechecks (`mod.rs`) |
| | `jump/` | Jump to Folder: frecency and ranking, the history and its writer, the box |
| | `select.rs`, `pattern.rs` | Select by Pattern and other selection commands; globs |
| | `band.rs` | Drag-select: the selection rectangle, its hits from layout geometry, its edge scrolling |
| | `grid.rs`, `tiles.rs` | The grid views (icons, tiles): view modes, icon sizes and cell geometry; thumbnails for the cells on screen and their workers |
| | `columns.rs` | The column view: the columns beside the active folder (which is the tab's `Folder`), their reads, scroll and the root; making a column active from its listing |
| | `batch.rs`, `rename.rs` | Batch Rename's dialog; its rules (pure functions) |
| | `complete.rs` | Tab completion in go-to mode |
| | `info.rs`, `openwith/` | Get Info (and its streaming total); Open With (per platform, `xdg.rs`) |
| | `sidebar.rs`, `volumes.rs` | Favorites and their file; Locations and Eject |
| | `marked.rs` | The rows on screen with running sums of their sizes, and the selection as bits over them, with its totals |
| | `volinfo.rs` | The status bar's free space: at most four threads, one query in flight per volume |
| | `listcols.rs` | The list's columns after the name: which show, their widths, how they fit a pane, `columns.toml` and its writer, the header's drag |
| Platform glue | `handler/` | Making Arbor the default for folders, and restoring (`linux.rs`, `windows.rs`, `mac.rs`) |
| | `incoming/` | Folders other apps send (`mac.rs` Apple events, `linux.rs` D-Bus, `windows.rs` a pipe) |
| UI | `view/` | Every element: root and status bar (`mod.rs`), `sidebar`, `pane` (path bar, tabs, view switch), `list` (rows, item presses and drags, the selection rectangle), `grid` (the icon and tiles views, type icons), `columns` (the column view), `panels`, `overlays`, `batch` |
| Shared | `bounded.rs`, `persist.rs`, `mounts.rs`, `dirfd.rs`, `ffi.rs`, `fs.rs` | See [Patterns](#patterns-to-reuse); folder reads, sorting, formatting |
| | `gpu.rs` | The window's GPU made early: Rune's prewarm (macOS) and warm-up (Linux); nothing on Windows, where Rune draws the first frame on the CPU |
| Tooling | `timing.rs`, `coldstart.rs` | Opt-in speed logs (`ARBOR_TIMING`, `ARBOR_TIMING_FILE`, the bench); the cold-start probes' clock and line, shared with `examples/win32_hello.rs` and `examples/rune_hello.rs`; `examples/gpu_probe.rs` times Rune's GPU setup phase by phase |
| Tests only | `a11y_tests.rs`, `frame_bench.rs`, `packaging_tests.rs`, `prop.rs` | Accessibility tree, per-key cost, install scripts, seeded property tests (a PRNG and a runner) |

## Threads

Nothing runs in the background unless the user did something, and nothing polls.

- **Startup read** (`app::boot`): started by `main` before Rune's loop; reads
  `config.toml`, `sidebar.toml`, `columns.toml`, then the first folder. A later window reads nothing but its
  folder (`State::new_window`). The first window waits at most
  `FIRST_PAINT_WAIT` (8 ms) for it, then lands it in `poll`.
- **GPU warm-up** (`gpu.rs`): started first thing in `main`. On Linux Rune's `rune-gpu-warm`
  (`rune::warm_gpu`) makes the window's GPU device and renderer while Rune builds the event loop
  and window, which take them; macOS does the same through `rune::prewarm_gpu`. On Windows Arbor
  starts nothing: Rune draws the window's first frame on the CPU and shows it, then makes the
  GPU on its own below-normal thread (`rune-gpu-window`) and hands over to it. Headless runs
  start none of it.
- **`rune::Latest` workers**: Rune's shared pool (≤ 4 threads) runs every latest-wins request:
  folder listings (`Folder::loader`, and each column's of the column view), settings reload, typed-path checks, completion, Get
  Info's metadata, Open With's list, batch rename's reads and plans, search rechecks, volume
  lists, eject, launching apps. An older answer never lands over a newer one.
- **Preview worker** (`arbor-preview`, `preview::Worker`): one thread, started by the first
  preview; one job at a time, at most one waiting; a stop flag checked between reads and rows.
- **Thumbnail workers** (`arbor-thumb-0`, `-1`, `tiles::Pool`): two threads, started by the
  first icon view with an image (or, on macOS and Windows, a document) on screen; one decode
  each from a shared queue. Rune's `on_visible_rows` says which grid rows are on screen; cells
  that leave have their requests taken out of the queue or stopped (a flag per request, the
  workers' bells rung for an OS wait). Idle between grids.
- **Search pool** (`search::Pool`): up to four walker threads, started by the first search,
  below the window's priority, sharing one queue and a stop flag per walk.
- **Writers**: `arbor-history` (`jump::history`: appends, load, compaction, clear, under a
  lock file), `arbor-sidebar` (`sidebar::Writer`: edits merged into the file under its
  lock, then an atomic rewrite) and `arbor-columns` (`listcols::Writer`: the whole file, the
  last change queued, an atomic rewrite), each started by its first write. One of each for every window (`Shared`), waking the open window
  focused last (`Shared::live_waker`): nothing shared holds a window's own waker or handle
  (the operation's waker is moved to its new owner, `Shared::own_job`). Each registers with
  `persist::at_exit` and gets up to 100 ms when Arbor quits (`persist::drain_at_exit`).
- **Free space** (`arbor-volume`, `volinfo.rs`): the status bar's `statvfs`, never on Rune's
  pool (it can't be stopped, and a dead network volume never answers). At most four threads in
  the whole app, below the window's priority, ending when nothing waits; volumes by device, one
  query in flight per volume (its answer goes to every folder that asked about it); while all
  four are stuck nothing new starts. Asked when a listing lands and after a file operation.
- **Get Info's total** (`arbor-info`): one walk per request, waking the window at most every
  30 ms.
- **Listeners** (not in tests or the bench): `incoming::listen` (macOS open-documents events,
  Linux `arbor-dbus` serving `org.freedesktop.FileManager1`, Windows `arbor-pipe`) and
  `volumes::watch` (macOS mount notifications, Windows `arbor-devices` hidden window). Each
  blocks on its source; none polls. Both reach the window focused last (`window::route`). The
  volume watcher starts at the first window's first frame on screen (Rune's `on_ready`).
- **The operation worker** (`ops::start`): one thread per file operation; only one runs at a
  time in the whole app, held by `Shared` (`Shared::job`), shown and landed by its owner (the
  window that started it), which tells the others what changed. The worker wakes whichever
  window owns it (`Shared::op_wake`): a window closing mid-operation makes another the owner
  on the spot (`State::leave`), and a window whose operation's owner is gone lands it. The
  last window's close cancels it and hands it to `arbor-quit` (`leftovers.rs`), which waits
  ≤ 2 s for its cleanup and records what it left (`arbor-leftovers` writes the record); `main`'s
  `last` waits for it once no window is on screen (Rune's `fast_exit` runs it before ending the
  process, on every quit).
- **The Trash's count** (`arbor-trash-count`, `trash::contents::Counting`): while Empty
  Trash's dialog is open, one thread lists the items, then adds up their size, waking the
  window after each, and ends; closing the dialog stops it.
- **Favorite looks** (`sidebar::Lookers`): at most two threads sharing a queue, started from
  Rune's `on_ready` (the first frame on screen), ending when it's empty.
- **Termination signals** (`arbor-signals`, `signals.rs`, Unix, not in tests or headless runs):
  sleeps on a self-pipe the SIGTERM/SIGINT/SIGHUP handler writes to; the first signal quits as
  ⌘Q does (`window::quit_app`), and ends.
- Short-lived threads: reaping launched processes (`fs::reap`, `openwith`).

## Patterns to reuse

- **Bounded wait, then async** (`bounded.rs`). `read_waiting(loader, wait, work)` runs `work` on a
  `Latest` and waits up to `wait` (usually ≤ one frame) for it: in time, the answer comes back
  from the call; late, it lands in `poll` through the loader. Never both, never neither. Used
  by listings, tab switches, typed paths, completion, Get Info, Open With and batch rename.
  `bounded::start` and `Waiting::until` do the same for several requests sharing one deadline
  (the column view's columns).
  The startup read and Jump's load do the same over their own channels.
- **Request numbers.** Every listing request takes `folder::next_request()` (one process-wide
  counter); an answer lands only if it carries the number its folder is still waiting for.
  Navigations that need follow-up (a Jump, a sidebar click) remember the number. Preview,
  Get Info and Open With keep their own counters the same way.
- **Identity, re-checked right before acting.** Two questions, two types. *Is it still the
  item chosen?* is `fs::Ident`, the object's identity: device and inode on Unix (Windows: its
  creation time), its type, and its birth time where the file system keeps one (statx btime,
  macOS's birth time). Every listing reads it for each entry from the metadata it reads anyway
  (Windows: `fs::win_dir` enumerates with `FileIdBothDirectoryInfo`, which adds the volume and
  NTFS file id at no cost per entry); so does the search walk. Every action on chosen items
  carries it (`ops::Checks`, `Op::Checked`, `ops::Doomed`, a drop on the other pane, Empty
  Trash's items, which on Linux add the file handle; a drag reads nothing when it starts, and an
  item whose identity couldn't be read is never moved or removed) and the worker compares it immediately before each
  item is touched (`fs::ident_of`, which on Windows reads the file id through a handle). An item
  replaced since is left alone ("…was replaced since the folder was read; press ⌘R"), and one
  with no identity is never deleted. Size and times of change aren't in it, so a file edited
  after its folder was listed is still the item (Arbor watches nothing). Device and inode alone
  aren't enough: ext4 hands a new file the inode just freed; the birth time tells them apart, and
  where there's none, Linux's file handle (inode and generation) or else the ctime does (on such
  systems an edit then refuses too). Left: a replacement made in the original's clock tick (ext4),
  and on Windows one within NTFS's 15 s tunnelling where the identity came without a file id (the
  search walk, `std` metadata). *Is it unchanged?* is `ops::Print` (`ops/print.rs`: identity,
  type, size, mtime, and a tree for folders): undo, redo, batch rename (from its preview, and its
  rollback) and a cancelled copy's cleanup refuse if anything differs.
  A *folder's* identity, links followed (`noclobber::DirId`: a listing's `dir_id`, where a
  paste, drop, New File or New Folder goes), has its birth time or Linux's handle the same way,
  but never its ctime.
- **Mount boundaries** (`mounts.rs`). `Mounts` (Linux's mount table, read once per operation)
  and `volume` (macOS's volume per folder) decide what's "another disk"; `Fence` bundles them
  for a walk. Used by permanent delete, cleanup of cancelled copies, cross-disk
  moves, Get Info's total and Compress's walk. `local_points` lists the local disks (Empty Trash's per-disk
  trashes). Search uses device numbers alone (`search::devices_for`).
- **User data on disk** (`persist.rs`). `replace` rewrites a file atomically (`Durable` also
  flushes it first, for the handler backup); `locked` holds a file's lock file around a
  read-then-write so two Arbor processes don't interleave; a writer thread reports a run of
  failures once (`Failures`) and registers its flush for exit.
- **Walking without following links** (`dirfd.rs`, Unix): folders opened with `O_NOFOLLOW`,
  checked against the identity they were listed with, entries `lstat`ed through the
  descriptor. Permanent delete, Get Info's total, Empty Trash (each trash folder opened
  that way and pinned at the count, items and records removed with `unlinkat` relative to it),
  Compress's walk, and Extract Here's writes (`archive/out.rs`: files created `O_EXCL |
  O_NOFOLLOW` relative to their folder).
- **OS strings** (`ffi.rs`): `cstr` (Unix) and `wide` (Windows) for every path passed to the OS.
- **Stop flags.** Every background read has an `Arc<AtomicBool>` set when its result stops
  mattering (navigated away, tab hidden, dropped); work checks it between entries.

## How a command flows

1. A key arrives in `State::on_key` (`app/dispatch.rs`), which handles text fields and overlays first,
   then looks the chord up in the `Keymap` built from `commands.rs` and `config.toml`. The
   palette and the context menus resolve to the same `Cmd`, and dim or skip it when
   `State::unavailable(cmd)` gives a reason (each command's own code says it again).
2. `State::run(cmd)` asks first through `confirm.rs` when `Cmd::asks` (what can't be undone,
   or changes the system; the dialog's button calls `run_permanent`), then `run_cmd` matches
   on `Cmd` exhaustively, as `unavailable` does.
3. Navigation calls `State::go*` → `Folder::read` → a `Latest` listing, landed by
   `State::land`. File commands go to `actions.rs`, which builds an `ops::Op` (capturing
   identities) and calls `ops::start`; the outcome lands in `poll` (`State::poll_job`) and goes on
   the (shared) undo stack; every window reads the folders it changed
   (`State::changed_on_disk`, the others through their inboxes).

## Platform code

Per-platform code lives beside its portable logic, behind `#[cfg]`, in one module per concern:
`noclobber.rs` (no-replace primitives), `trash/`, `delete.rs` (`imp` per family), `mounts.rs`,
`thumb.rs`, `volumes.rs`, `openwith/`, `handler/` and `incoming/` (a file per platform),
`ffi.rs` and `fs.rs` (open, reveal, hidden files). Linux's handler code is plain
files and is tested on every platform; Windows' registry plan is tested in CI under a test key.
