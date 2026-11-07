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

The core is about 1,100 lines in `src/`: `fs.rs` (disk and OS), `app.rs` (state and keys) and
`view.rs` (UI).

## Run

```sh
cargo run --release                 # opens the current dir (or ~ when launched from /)
cargo run --release -- ~/Downloads  # open a folder
cargo build --profile dist          # stripped, LTO build: target/dist/arbor
```

Builds are fully offline. Dependencies are vendored in `vendor/` (see `.cargo/config.toml`).
`vendor/` is gitignored because it's about 550 MB, so a fresh clone runs `scripts/vendor.sh` first.
`scripts/vendor.sh` refreshes them through a fast remote machine (default `smr@tabot-dev`) and
syncs them back.

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
| ⌘R | Reload |
| ⌘C | Copy the selected path |
| ⌘O | Open |
| ⌘Enter | Reveal in Finder / Explorer / the default file manager |
| ⌘1…⌘9 | Jump to a sidebar place |

Off macOS, ⌘ is Ctrl. Mouse: click to select, double-click to open, click a breadcrumb or
place to jump there, click a column header to sort (click again to reverse).

## Platform notes

- **macOS** (main target): content sits under a transparent titlebar, with the traffic lights in
  the sidebar. Sidebar places include `/Volumes`.
- **Linux**: `cargo check` passes. Building needs `libxkbcommon-dev libwayland-dev`. Opens files
  with `xdg-open`. Sidebar places include `/mnt` and `/media`.
- **Windows**: written for it (drive letters, hidden attribute, `explorer /select,`), but not
  compiled yet. Cross-checking from Linux needs a Windows C toolchain for `blake3`.

## Not in v1

Copy, move, rename, delete/trash and new folder. Also no file watching (use ⌘R), no
Quick Look-style preview (Space is reserved for it), no tabs, and no Tab-completion in go-to
mode.
