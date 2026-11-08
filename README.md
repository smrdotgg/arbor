# Arbor

A small, fast file manager for macOS, Linux and Windows, built on [Rune](https://github.com/anomalyco/rune)
(native GPU UI in Rust).

![Arbor](shots/dark.png)

## Design rules

- **No caching.** Every navigation reads the folder fresh. There's no index, thumbnail store or
  prefetch. It's fast because it does very little work.
- **One read per navigation.** It runs `read_dir` plus one `lstat` per entry on a worker
  (`rune::Latest`), so a slow or network disk never freezes the window. The newest request wins,
  and the old listing stays on screen until the new one arrives, so there's no blank flash.
- **Build only what's visible.** Rows are `virtual_rows`: a 50k-file folder lays out about 30 rows.
- **No animation.** Every change lands with `tx.instant()`.
- **Idle means idle.** No timers and no polling. Nothing redraws until you do something.

- **File operations never block and never overwrite.** Copies and moves run on a worker with
  progress in the status bar (Esc cancels). A name that's taken gets a free one instead:
  "name copy" in the same folder, "name 2" elsewhere. On APFS, copies are clones (instant).

About 2,300 lines in `src/`: `fs.rs` (reading folders, opening files), `ops.rs` (copy, move,
trash and undo on a worker), `trash.rs` (each platform's trash), `app.rs` (state, selection,
keys), `actions.rs` (clipboard, rename, drag and drop, context menu) and `view.rs` (UI).

## Run

```sh
cargo run --release                 # opens the current dir (or ~ when launched from /)
cargo run --release -- ~/Downloads  # open a folder
cargo build --profile dist          # stripped, LTO build: target/dist/arbor
```

Builds are fully offline. Dependencies are vendored in `vendor/` (see `.cargo/config.toml`).
`vendor/` is gitignored because it's about 550 MB, so a fresh clone runs `scripts/vendor.sh` first.
`scripts/vendor.sh` refreshes them through a fast remote machine (default `smr@tabot-dev`) and
syncs them back. When it runs on that machine itself (or with `HOST=local`) it skips ssh. Rune's
commit is pinned in `RUNE_REV`; `REV=origin/dev scripts/vendor.sh` moves the pin.

CI (`.github/workflows/ci.yml`) runs on every push and pull request on macOS, Linux and Windows:
`cargo build --release --locked`, `cargo test --release --locked` and `cargo clippy -D warnings`,
plus `cargo fmt --check` on Linux. It clones Rune at `RUNE_REV` and gets crates from crates.io
instead of `vendor/`.

## Keys

| Key | Action |
| --- | --- |
| ↑ ↓, PgUp PgDn, Home End | Move selection |
| Enter, → (folder), ⌘↓ | Open (folders navigate; files open in their default app) |
| ←, ⌫ (empty filter), ⌘↑ | Parent folder (re-selects where you came from) |
| ⌘[ ⌘], ⌥← ⌥→ | Back / forward |
| *type anything* | Filter the folder (smart: the first prefix match gets selected) |
| Esc | Clear filter |
| ⌘L, or type `/` or `~` | Go-to-path mode. Enter navigates (or opens a file) |
| ⌘. (or ⌘⇧.) | Show/hide hidden files (dotfiles, macOS `UF_HIDDEN`, Windows hidden attr) |
| ⇧↑ ⇧↓, ⇧-click, ⌘-click, ⌘A | Select several items |
| ⌘C / ⌘X | Copy / cut the selected files (Finder and Explorer can paste them) |
| ⌘V / ⌥⌘V | Paste into this folder: copies, or moves if you cut them; ⌥⌘V always moves |
| ⌘D | Duplicate |
| F2 or ⇧⌘R | Rename inline (the name before the extension is selected). Enter saves, Esc cancels |
| ⇧⌘N | New folder (starts renaming it) |
| ⌘⌫ or Delete | Move to Trash |
| ⌘Z | Undo the last rename, move, copy, trash or new folder |
| ⌘R | Reload |
| ⇧⌘C | Copy the selected path as text |
| ⌘O | Open |
| ⌘Enter | Reveal in Finder / Explorer / the default file manager |
| ⌘1…⌘9 | Jump to a sidebar place |

Off macOS, ⌘ is Ctrl. Mouse: click to select, double-click to open, click a breadcrumb or
place to jump there, click a column header to sort (click again to reverse). Right-click opens
a native context menu (macOS and Windows). Drag rows out to Finder or other apps (macOS).
Drop files on a folder row or on the list: same disk moves, another disk copies, and ⌥ forces
a copy.

## Platform notes

All three build, pass tests and pass clippy in CI. How much has been run for real differs:

- **macOS** (main target, used daily): content sits under a transparent titlebar, with the
  traffic lights in the sidebar. Sidebar places include `/Volumes`.
- **Linux**: building needs `libxkbcommon-dev libwayland-dev`. Running under X11 also needs
  `libxkbcommon-x11-0` (desktops have it; minimal installs may not). Opens files with `xdg-open`.
  Sidebar places include `/mnt` and `/media`. Checked at runtime only under Xvfb with Mesa's
  software Vulkan (Ubuntu 24.04): the window opens and lists a folder. Not yet checked: Wayland,
  a real GPU, keys, drag and drop, clipboard, trash and undo, `xdg-open`.
- **Windows**: drive letters, hidden attribute, Recycle Bin, `explorer /select,`. Compiles and
  passes tests in CI but **has never been run**: everything at runtime is unchecked.

Trash uses NSFileManager on macOS, the freedesktop.org spec on Linux, and the Recycle Bin on
Windows. Undoing a trash puts the item back on macOS and Linux. Windows doesn't report where
the item went, so restore it from the Recycle Bin there.

## Not yet

Permanent delete (on purpose, for now), file watching (use ⌘R), Quick Look-style preview
(Space is reserved for it), tabs, Tab-completion in go-to mode, redo, more than one operation
at a time, and a context menu on Linux.
