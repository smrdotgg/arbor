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
  "name copy" in the same folder, "name 2" elsewhere. Every move and rename is an atomic
  "rename unless the name is taken" (`renameat2(RENAME_NOREPLACE)` on Linux,
  `renamex_np(RENAME_EXCL)` on macOS, `MoveFileExW` without replace on Windows), and copies
  create each file and folder exclusively, so a name another app takes at the same instant is
  never replaced (`src/noclobber.rs`). On APFS, copies are clones (instant). A move to another
  disk copies, then removes only the originals that haven't changed since they were copied.
- **Undo refuses rather than guesses.** When an operation finishes, each result gets a
  fingerprint: identity (device and inode; volume and file id on Windows), type, size and
  modified time. For copies and new folders, which undo sends to the Trash, it also covers
  everything inside. Before undoing, the worker re-checks every item. If anything was moved,
  deleted, replaced or edited, or its spot is taken, nothing is undone and the status bar says
  which item changed and how. Each item is checked again right before it goes back, and one
  whose spot is taken in between stays where it is. A refused or failed undo stays on the
  stack. A partly failed one keeps only what didn't go back; an undo across disks that copied
  an item back but couldn't remove all of the original says where both halves are, and undoing
  again finishes the job if neither has changed.
- **Known limit:** contents aren't hashed (too slow for big files), so an edit that keeps a
  file's size and leaves its modified time unchanged isn't seen. That includes an edit within
  the disk's timestamp granularity (2 s on FAT, 1 s on HFS+ and ext3; ext4 and NTFS stamp from
  a clock that ticks every few milliseconds).

About 5,500 lines in `src/` (over a third of it tests): `fs.rs` (reading folders, opening files),
`ops.rs` (copy, move, trash and undo on a worker), `noclobber.rs` (moves, copies and file ids
that never replace anything), `trash.rs` (each platform's trash), `app.rs`
(state, selection, key dispatch, what each command does), `commands.rs` (the command table and
keymap), `palette.rs` (the command palette), `config.rs` (the settings file), `actions.rs`
(clipboard, rename, drag and drop, context menu), `view.rs` (UI) and `timing.rs` (opt-in speed
measurement).

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

## Measuring speed

`ARBOR_TIMING=1 arbor` logs to stderr how long the first frame took from `main`, each key press
to its frame, and each navigation to the frame showing the new listing (`src/timing.rs`). Off,
it costs nothing: `main` reads the variable once and Rune gets the plain callbacks and no frame
hook. `scripts/bench.sh` cold-starts Arbor on a 10,000-entry folder several times and reports
median and p95 against each budget. It opens and closes windows, so leave the machine alone
while it runs. `cargo test --release -- --ignored --nocapture list_10k` times reading and
sorting alone. Results are in [`BENCH.md`](BENCH.md).

## Keys and the command palette

Every action is a command with an id, a title and default keys, in one table
(`src/commands.rs`). Keys, the palette, the context menu and the settings file all read it.

**⌘K** (or **⇧⌘P**) opens the command palette: type part of a command's name, see its keys,
press Enter (or click) to run it. ↑↓ and Page Up/Down move, Esc or a click outside closes it.
Matches that start the name, then ones that start a word, come first; initials work too ("nf"
for New Folder), and so do ids once you type a `.` ("file.tr"). Commands that can't run right
now (Rename with nothing selected) are dimmed, and choosing one says why.

Typing, outside the palette:

- **Type anything** to filter the folder. The first name starting with what you typed gets
  selected. While the filter has text, typing, ⌫ and ⌥⌫ edit it.
- **Type `/` or `~`** (or ⌘L) for go-to-path mode. Enter navigates, or opens a file.
- Space with nothing typed does nothing (reserved for preview).

Off macOS, ⌘ is Ctrl. Mouse: click to select, ⇧-click and ⌘-click to select several,
double-click to open, click a breadcrumb or place to jump there, click a column header to sort
(click again to reverse). Right-click opens a native context menu (macOS and Windows). Drag rows
out to Finder or other apps (macOS). Drop files on a folder row or on the list: same disk moves,
another disk copies, and ⌥ forces a copy.

Every command, with its default keys (generated from `src/commands.rs`; a test keeps it in sync,
`ARBOR_WRITE_README=1 cargo test readme` rewrites it):

<!-- commands:start -->
| Command | macOS | Windows, Linux | Id |
| --- | --- | --- | --- |
| Open | ↩, ⌘O, ⌘↓, ⌥↓ | Enter, Ctrl+O, Ctrl+↓, Alt+↓ | `nav.open` |
| Open Folder | → | → | `nav.into` |
| Go to Parent Folder | ←, ⌫, ⌘↑, ⌥↑ | ←, Backspace, Ctrl+↑, Alt+↑ | `nav.parent` |
| Go Back | ⌘[, ⌥← | Ctrl+[, Alt+← | `nav.back` |
| Go Forward | ⌘], ⌥→ | Ctrl+], Alt+→ | `nav.forward` |
| Go to Path… | ⌘L | Ctrl+L | `nav.goto` |
| Reload | ⌘R | Ctrl+R | `nav.reload` |
| Show in Finder / Explorer / file manager | ⌘↩ | Ctrl+Enter | `nav.reveal` |
| Command Palette | ⌘K, ⇧⌘P | Ctrl+K, Ctrl+Shift+P | `app.palette` |
| Open Settings File | ⌘, | Ctrl+, | `app.settings` |
| Reload Settings |  |  | `app.reload_settings` |
| Clear Filter and Selection | ⎋ | Esc | `app.clear` |
| Copy | ⌘C | Ctrl+C | `file.copy` |
| Cut | ⌘X | Ctrl+X | `file.cut` |
| Paste | ⌘V | Ctrl+V | `file.paste` |
| Move Here (Paste and Move) | ⌥⌘V | Ctrl+Alt+V | `file.paste_move` |
| Copy Path | ⇧⌘C, ⌥⌘C | Ctrl+Shift+C, Ctrl+Alt+C | `file.copy_path` |
| Duplicate | ⌘D | Ctrl+D | `file.duplicate` |
| Rename… | F2, ⇧⌘R | F2, Ctrl+Shift+R | `file.rename` |
| New Folder | ⇧⌘N | Ctrl+Shift+N | `file.new_folder` |
| Move to Trash | ⌘⌫, ⌦ | Ctrl+Backspace, Delete | `file.trash` |
| Undo | ⌘Z | Ctrl+Z | `file.undo` |
| Cancel File Operation |  |  | `file.cancel` |
| Move Up | ↑ | ↑ | `select.up` |
| Move Down | ↓ | ↓ | `select.down` |
| Move Up a Page | ⇞ | Page Up | `select.page_up` |
| Move Down a Page | ⇟ | Page Down | `select.page_down` |
| Move to First | ↖ | Home | `select.first` |
| Move to Last | ↘ | End | `select.last` |
| Extend Selection Up | ⇧↑ | Shift+↑ | `select.extend_up` |
| Extend Selection Down | ⇧↓ | Shift+↓ | `select.extend_down` |
| Extend Selection Up a Page | ⇧⇞ | Shift+Page Up | `select.extend_page_up` |
| Extend Selection Down a Page | ⇧⇟ | Shift+Page Down | `select.extend_page_down` |
| Extend Selection to First | ⇧↖ | Shift+Home | `select.extend_first` |
| Extend Selection to Last | ⇧↘ | Shift+End | `select.extend_last` |
| Select All | ⌘A | Ctrl+A | `select.all` |
| Show or Hide Hidden Files | ⌘., ⇧⌘. | Ctrl+., Ctrl+Shift+. | `view.hidden` |
| Sort by Name |  |  | `view.sort_name` |
| Sort by Kind |  |  | `view.sort_kind` |
| Sort by Size |  |  | `view.sort_size` |
| Sort by Date Modified |  |  | `view.sort_modified` |
| Reverse Sort Order |  |  | `view.sort_reverse` |
| Go to sidebar place 1…9 | ⌘1 … ⌘9 | Ctrl+1 … Ctrl+9 | `place.1` … `place.9` |
<!-- commands:end -->

Notes: paste copies, or moves what you cut; ⌥⌘V always moves. Undo refuses if any item changed
since. Rename selects the name before the extension; Enter saves, Esc cancels. Esc also cancels
a running copy or move.

## Settings

One file holds settings and keybindings. "Open Settings File" (⌘, or from the palette) creates it
with every setting and every command listed, all commented out, and opens it. Nothing watches the
file: run "Reload Settings" after saving (or restart).

| Platform | File |
| --- | --- |
| macOS | `~/Library/Application Support/Arbor/config.toml` |
| Linux | `$XDG_CONFIG_HOME/arbor/config.toml` (default `~/.config/arbor/config.toml`) |
| Windows | `%APPDATA%\Arbor\config.toml` |

```toml
[general]
show_hidden = false        # show hidden files in new windows (⌘. toggles)
sort = "name"              # "name", "kind", "size" or "modified"
sort_descending = false    # default: size and modified sort largest and newest first
start_folder = "~"         # when none is given on the command line
places = ["~", "~/Projects", "/"]   # sidebar places, replacing the automatic list

[keys]
"file.trash" = ["cmd+backspace", "delete"]   # replaces the command's default keys
"file.duplicate" = []                        # unbinds it
"app.palette" = ["cmd+p"]
```

In a key, `cmd` is ⌘ on macOS and Ctrl elsewhere; `ctrl`, `alt` (`option`), `shift` and
`super` name those keys. A binding takes its key from any command that had it by default, and if
two lines bind the same key the later one gets it (and the status bar says so). Off macOS
`cmd+g` and `ctrl+g` are the same key. Only `[]` unbinds: a list whose keys are all typos keeps
the defaults. Mistakes never stop Arbor: the status bar shows the first one ("Settings: line 12:
unknown command 'file.trahs' (did you mean 'file.trash'?)"), everything valid still applies, and
the rest keeps its default. A line that isn't valid TOML is skipped. The file is read at
startup on the thread that reads the first folder, and by Reload Settings on a worker, so it
never holds up the window. Only a regular file up to 1 MB is read; anything else (a folder, a
pipe, a huge file) is reported and ignored.


## Platform notes

All three build, pass tests and pass clippy in CI. How much has been run for real differs:

- **macOS** (main target, used daily): content sits under a transparent titlebar, with the
  traffic lights in the sidebar. Sidebar places include `/Volumes`.
- **Linux**: building needs `libxkbcommon-dev libwayland-dev`. Running under X11 also needs
  `libxkbcommon-x11-0` (desktops have it; minimal installs may not). Opens files with `xdg-open`.
  Sidebar places include `/mnt` and `/media`. Checked at runtime only under Xvfb with Mesa's
  software Vulkan (Ubuntu 24.04): the window opens and lists a folder, and synthetic X key
  presses (XTest) filter, move the selection and drive the command palette and settings reload.
  Not yet checked: Wayland, a real GPU, a real keyboard and input methods, drag and drop,
  clipboard, trash and undo, `xdg-open`.
- **Windows**: drive letters, hidden attribute, Recycle Bin, `explorer /select,`. Compiles and
  passes tests in CI but **has never been run**: everything at runtime is unchecked.

Trash uses NSFileManager on macOS, the freedesktop.org spec on Linux, and the Recycle Bin on
Windows. Undoing a trash puts the item back on macOS and Linux (on Linux its `.trashinfo` is
removed too). Windows doesn't report where
the item went, so restore it from the Recycle Bin there.

## Not yet

Permanent delete (on purpose, for now), file watching (use ⌘R), Quick Look-style preview
(Space is reserved for it), tabs, Tab-completion in go-to mode, redo, more than one operation
at a time, and a context menu on Linux.
