# Architecture

A map for finding your way. What Arbor does is in `README.md`; why, and what it must never do,
in `NORTHSTAR.md`. Each module's own `//!` comment says more about it.

## The shape

One window, one `State` (`src/app/`), one UI thread. Rune calls four functions on it:
`view::view` (build the element tree from `State`, every frame), `State::on_key`,
`State::poll` (after a waker fires: land whatever background work finished) and `setup`.
Handlers change `State` and land the change with `tx.instant()` (nothing animates). Anything
that reads or changes the disk runs off the UI thread and lands through `poll`: listings, every
file operation (the inline rename and New Folder too, since milestone 13's review), Get Info's
details. What's left on the UI thread is small and named: starting other apps (`fs::open`,
reveal), creating the settings file when it's opened, and one `lstat` per item for a drop's
"same disk?" and for a pasted list of paths.

## Modules

A folder is a module whose parts got their own files; `mod.rs` holds its interface, and
`tests.rs` its tests.

| Area | Modules | What they own |
| --- | --- | --- |
| Entry | `main.rs` | Start-up order, Rune's callbacks, exit drains |
| State | `app/` | `State` and navigation (`mod.rs`), keys and per-command `run`/`unavailable` (`dispatch.rs`), the startup read (`boot.rs`) |
| | `folder.rs` | One folder on screen: listing, cursor, selection, filter, sort, history; `TabMemo` |
| | `panes.rs` | Tabs and split view |
| Commands | `commands.rs` | The command table (`Cmd`: id, title, default keys), `Keymap`, palette scoring |
| | `config.rs` | `config.toml`: settings and key bindings, the template |
| | `palette.rs`, `menu.rs`, `confirm.rs` | Palette, Linux's in-window context menu, the one dialog |
| File ops | `actions.rs` | UI side of file operations: clipboard, paste, rename, drag and drop, context menu |
| | `ops/` | The operation worker (`worker.rs`), undo and redo (`undo.rs`), batch rename (`batch.rs`), fingerprints (`print.rs`), Empty Trash and Put Back (`trashed.rs`) |
| | `noclobber.rs` | Renames, copies and file ids that never replace anything |
| | `trash/` | Each platform's trash, and telling a disk with none (`win.rs`: the Recycle Bin); what's in the trashes, their count for Empty Trash's dialog, `.trashinfo` records (`contents.rs`) |
| | `delete.rs` | Permanent delete without following links |
| Features | `preview/` (+ `thumb.rs`) | Quick preview: dispatch (`mod.rs`), its thread (`worker.rs`), text, image decoders (`decode.rs`); OS thumbnails |
| | `search/` | Recursive search: the walk and its threads (`walk.rs`), results and rechecks (`mod.rs`) |
| | `jump/` | Jump to Folder: frecency and ranking, the history and its writer, the box |
| | `select.rs`, `pattern.rs` | Select by Pattern and other selection commands; globs |
| | `batch.rs`, `rename.rs` | Batch Rename's dialog; its rules (pure functions) |
| | `complete.rs` | Tab completion in go-to mode |
| | `info.rs`, `openwith/` | Get Info (and its streaming total); Open With (per platform, `xdg.rs`) |
| | `sidebar.rs`, `volumes.rs` | Favorites and their file; Locations and Eject |
| Platform glue | `handler/` | Making Arbor the default for folders, and restoring (`linux.rs`, `windows.rs`, `mac.rs`) |
| | `incoming/` | Folders other apps send (`mac.rs` Apple events, `linux.rs` D-Bus, `windows.rs` a pipe) |
| UI | `view/` | Every element: root and status bar (`mod.rs`), `sidebar`, `pane` (path bar, tabs), `list`, `panels`, `overlays`, `batch` |
| Shared | `bounded.rs`, `persist.rs`, `mounts.rs`, `dirfd.rs`, `ffi.rs`, `fs.rs` | See [Patterns](#patterns-to-reuse); folder reads, sorting, formatting |
| Tooling | `timing.rs` | Opt-in speed logs (`ARBOR_TIMING`, the bench) |
| Tests only | `a11y_tests.rs`, `frame_bench.rs`, `packaging_tests.rs`, `prop.rs` | Accessibility tree, per-key cost, install scripts, seeded property tests (a PRNG and a runner) |

## Threads

Nothing runs in the background unless the user did something, and nothing polls.

- **Startup read** (`app::boot`): started by `main` before Rune's loop; reads
  `config.toml`, `sidebar.toml`, then the first folder. The first window waits at most
  `FIRST_PAINT_WAIT` (8 ms) for it, then lands it in `poll`.
- **`rune::Latest` workers**: Rune's shared pool (≤ 4 threads) runs every latest-wins request:
  folder listings (`Folder::loader`), settings reload, typed-path checks, completion, Get
  Info's metadata, Open With's list, batch rename's reads and plans, search rechecks, volume
  lists, eject, launching apps. An older answer never lands over a newer one.
- **Preview worker** (`arbor-preview`, `preview::Worker`): one thread, started by the first
  preview; one job at a time, at most one waiting; a stop flag checked between reads and rows.
- **Search pool** (`search::Pool`): up to four walker threads, started by the first search,
  below the window's priority, sharing one queue and a stop flag per walk.
- **Writers**: `arbor-history` (`jump::history`: appends, load, compaction, clear, under a
  lock file) and `arbor-sidebar` (`sidebar::Writer`: edits merged into the file under its
  lock, then an atomic rewrite). Each registers with
  `persist::at_exit` and gets up to 100 ms when Arbor quits (`persist::drain_at_exit`).
- **Get Info's total** (`arbor-info`): one walk per request, waking the window at most every
  30 ms.
- **Listeners** (not in tests or the bench): `incoming::listen` (macOS open-documents events,
  Linux `arbor-dbus` serving `org.freedesktop.FileManager1`, Windows `arbor-pipe`) and
  `volumes::watch` (macOS mount notifications, Windows `arbor-devices` hidden window). Each
  blocks on its source; none polls.
- **The operation worker** (`ops::start`): one thread per file operation; only one runs at a
  time (`State::job`).
- **The Trash's count** (`arbor-trash-count`, `trash::contents::Counting`): while Empty
  Trash's dialog is open, one thread lists the items, then adds up their size, waking the
  window after each, and ends; closing the dialog stops it.
- **Favorite looks** (`sidebar::Lookers`): at most two threads sharing a queue, started from
  Rune's `on_ready` (the first frame on screen), ending when it's empty.
- Short-lived threads: reaping launched processes (`fs::reap`, `openwith`).

## Patterns to reuse

- **Bounded wait, then async** (`bounded.rs`). `read_waiting(loader, wait, work)` runs `work` on a
  `Latest` and waits up to `wait` (usually ≤ one frame) for it: in time, the answer comes back
  from the call; late, it lands in `poll` through the loader. Never both, never neither. Used
  by listings, tab switches, typed paths, completion, Get Info, Open With and batch rename.
  The startup read and Jump's load do the same over their own channels.
- **Request numbers.** Every listing request takes `folder::next_request()` (one process-wide
  counter); an answer lands only if it carries the number its folder is still waiting for.
  Navigations that need follow-up (a Jump, a sidebar click) remember the number. Preview,
  Get Info and Open With keep their own counters the same way.
- **Identity, re-checked right before acting.** A source's identity (`fs::Ident`, from the
  metadata the listing already has: device and inode on Unix, or Windows' creation time, plus
  its type, a file's size, its ctime to the nanosecond (Windows: last write) and its birth time
  where the file system keeps one) is captured when an operation is asked for (from the
  listing, or the search walk) and checked on the worker immediately before each item is
  touched (`fs::ident_at`; `Op::Checked`, `ops::Doomed`, `ops::Seen` for batch rename). Device
  and inode alone aren't enough: ext4 hands a new file the inode just freed. An item replaced
  or changed since is left alone, and one with no identity is never deleted. Walks that only
  check a folder wasn't swapped (Get Info, search rechecks) compare `Ident::node`. Windows
  can't see a replacement within its 15 s creation-time tunnelling that has the same size and
  write time (its file index needs the file opened). Undo keeps a fuller fingerprint
  (`ops/print.rs`: identity, type, size, mtime, and a tree for folders) and refuses if
  anything differs.
- **Mount boundaries** (`mounts.rs`). `Mounts` (Linux's mount table, read once per operation)
  and `volume` (macOS's volume per folder) decide what's "another disk"; `Fence` bundles them
  for a walk. Used by permanent delete, cleanup of cancelled copies, cross-disk
  moves and Get Info's total. `local_points` lists the local disks (Empty Trash's per-disk
  trashes). Search uses device numbers alone (`search::devices_for`).
- **User data on disk** (`persist.rs`). `replace` rewrites a file atomically (`Durable` also
  flushes it first, for the handler backup); `locked` holds a file's lock file around a
  read-then-write so two Arbor processes don't interleave; a writer thread reports a run of
  failures once (`Failures`) and registers its flush for exit.
- **Walking without following links** (`dirfd.rs`, Unix): folders opened with `O_NOFOLLOW`,
  checked against the identity they were listed with, entries `lstat`ed through the
  descriptor. Permanent delete, Get Info's total, and Empty Trash (each trash folder opened
  that way and pinned at the count, items and records removed with `unlinkat` relative to it).
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
   identities) and calls `ops::start`; the outcome lands in `poll` (`State::job`) and goes on
   the undo stack.

## Platform code

Per-platform code lives beside its portable logic, behind `#[cfg]`, in one module per concern:
`noclobber.rs` (no-replace primitives), `trash/`, `delete.rs` (`imp` per family), `mounts.rs`,
`thumb.rs`, `volumes.rs`, `openwith/`, `handler/` and `incoming/` (a file per platform),
`ffi.rs` and `fs.rs` (open, reveal, hidden files). Linux's handler code is plain
files and is tested on every platform; Windows' registry plan is tested in CI under a test key.
