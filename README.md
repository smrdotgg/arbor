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
  never replaced (`src/noclobber.rs`). A case-only rename (`a.txt` to `A.txt` on a
  case-insensitive disk) that the no-replace rename refuses takes two such steps through a
  temporary name. On a disk without a no-replace rename, files move by hard link and folders
  can't be moved or renamed at all, since every way to do it there could replace an empty folder. On APFS, copies are clones (instant). A move to another
  disk copies, then removes only the originals that haven't changed since they were copied.
  A cancelled or failed copy removes what it made, each file known by the handle that wrote
  it, so a file another app put at the name meanwhile (a copy can stall on a slow source) stays
  and the status bar says so.
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

About 13,200 lines in `src/` (over a third of it tests): `fs.rs` (reading folders, opening files),
`ops.rs` (copy, move, trash and undo on a worker), `noclobber.rs` (moves, copies and file ids
that never replace anything), `trash.rs` (each platform's trash), `app.rs`
(state, startup, key dispatch, what each command does), `folder.rs` (one folder on screen: its
listing, selection, filter, sort and history, and what a hidden tab keeps), `panes.rs` (tabs and
split view), `commands.rs` (the command table and keymap), `palette.rs` (the command palette),
`config.rs` (the settings file), `actions.rs` (clipboard, rename, drag and drop, context menu),
`preview.rs` (quick preview: reading, sniffing and decoding on a worker), `jump.rs` (Jump to
Folder: the history's writer thread, frecency and ranking), `pattern.rs` (globs), `select.rs`
(Select by Pattern and the other selection commands), `view.rs` (UI) and `timing.rs` (opt-in
speed measurement).

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
to its frame, and each navigation to the frame showing the new listing (`src/timing.rs`). The
window waits at most 8 ms for the first folder; when it's slower, the listing's own frame is
logged as `first-listing`, and `ARBOR_SLOW_FIRST_READ_MS=N` (timing only) slows that read to try
it. All times end at frame submission, a proxy: pixels reach the screen at least a refresh later. Off,
it costs nothing: `main` reads the variable once and Rune gets the plain callbacks and no frame
hook. Showing a hidden tab is logged as `tab`, with `same_frame=1` when its fresh read landed
in time for the key's own frame. `scripts/bench.sh` cold-starts Arbor on a 10,000-entry folder
several times and reports median and p95 against each budget (`--tabs N` adds N runs timing a
tab switch to that folder, and `--preview N` adds N runs timing the quick preview of a 256 KB
text file and a 12 MP JPEG from Space to the frame showing it, and keys while it's open). It opens and closes windows, so leave the machine alone
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
- **Space** with nothing typed opens or closes the quick preview (below). While the filter has
  text, Space types a space, since names have spaces; a filter never starts with one.
- Tab switches to the other pane (Tab in go-to mode is reserved for path completion).

Off macOS, ⌘ is Ctrl. Mouse: click to select, ⇧-click and ⌘-click to select several,
double-click to open, click a breadcrumb or place to jump there (in a narrow pane the
leading folders fold into “…”, which opens the deepest hidden one), click a column header to sort
(click again to reverse). Right-click opens a native context menu (macOS and Windows). Drag rows
out to Finder or other apps (macOS). Drop files on a folder row or on the list: same disk moves,
another disk copies, and ⌥ forces a copy.

### Tabs and split view

**⌘\\** splits the window into two panes side by side, the new one on the same folder (and
active); ⌘\ again closes the other pane. Each pane has its own **tabs**: **⌘T** opens one on the
current folder, **⌘W** closes it, **⌃Tab** / **⌃⇧Tab** (or ⇧⌘] / ⇧⌘[) go to the next and previous,
**⇧⌘T** reopens the last closed one. Each tab has its own folder, back and forward history,
selection, scroll, filter and sort. The tab bar appears only when a pane has two or more tabs:
click a tab to show it, middle-click to close it, hover for its full path, "+" for a new one.
When they don't all fit, the bar shows the tabs around the active one, with "‹ 3" and "5 ›"
for how many are hidden on each side (click one to show the nearest).

One pane is **active**: it gets the keys, the palette's commands and the status bar, and in a
split window it has a thin accent line along its top and the accent-coloured selection (the
other pane's selection is grey). **Tab** or a click anywhere in a pane makes it active. The
sidebar drives the active pane. File operations stay global: one at a time, one undo stack, one
clipboard. **F5** copies and **F6** moves the selection into the other pane's folder (on a
worker, never over anything, undoable), and rows can be dragged from one pane onto the other
(same disk moves, another copies, ⌥ copies).

Closing a split pane's last tab closes that pane. On the last tab of the last pane, ⌘W does
nothing, on every platform: closing the window would quit Arbor and lose every tab's history,
which a habit of pressing ⌘W to tidy tabs shouldn't do. The window's close button still closes
it.

No caching: only the visible tab of each pane has a listing. A hidden tab keeps its path,
history, selected names, scroll offset, filter and sort, and does no work at all (no reads, no
watching). Showing it reads its folder fresh, waiting at most 8 ms so a local folder lands in
the same frame; a slower one shows the tab's path and an empty list until it arrives. The
selection comes back by name and the list scrolls to where it was. ⌘T and ⌘\ show the folder
already on screen, so they need no read. Closed tabs (up to 20, lightweight state only) are kept
in memory for ⇧⌘T, never on disk.

### Quick preview

**Space** opens a panel showing the active pane's selected item; Space again or **Esc** closes
it. With one pane it docks on the right and the list stays usable beside it: ↑↓ (or a click)
move the selection and the preview follows. In a split window it takes the other pane's place,
as a two-panel commander's quick view does (Tab swaps which pane shows; F5 and F6 still copy and
move into the covered pane's folder). Esc closes the palette first, then the preview, then
clears the filter or cancels a file operation.

- **Text** (found by its bytes, not its name: valid UTF-8 with few NULs and control characters,
  or UTF-16 with a byte order mark): the first 256 KB in the system's monospaced font, no
  wrapping (scroll sideways), lines over 1,000 characters cut. The panel says when it's showing
  only the start, and when the file isn't valid UTF-8 (invalid bytes show as �). Only the lines
  in view are laid out.
- **Images**: PNG, JPEG, GIF (its first frame), WebP and BMP, also found by their bytes. Decoded
  on a worker and shrunk to the panel's size in pixels before they're uploaded: a PNG shrinks row
  by row as it decodes, and a JPEG decodes at 1/2, 1/4 or 1/8 size straight from its DCT. JPEGs
  are turned as their EXIF orientation says. The header gives the size first, and anything over
  100 megapixels (24 for GIF, WebP, BMP, interlaced PNG and progressive JPEG, which decode whole
  first and can't stop partway, so one that can't be cancelled is short) is refused with a
  message, so a huge image never takes gigabytes.
- **Everything else**, and folders: name, kind, size and date, and for a folder how many items
  it has (one read of it). PDF, video, audio, office documents and other image formats (HEIC,
  TIFF, RAW) say they need the platform's own preview API, which Arbor doesn't use yet.
- A link previews what it points to and says where; a broken link, an unreadable file, or a
  pipe or device (never opened: reading a pipe would wait forever) says so.

It costs nothing while closed: no reads, no decoding, nothing per key. Open, it's background
work that follows the rules for it: the list moves at once and the header (from the listing)
changes in the same frame, then the contents land when they're read. Moving on or closing stops
the read in flight (every read call and decoded row checks) and its result is dropped. One read
per item, no watching, nothing kept: coming back to a file reads it again. All of it runs on one
thread of its own, started by the first preview and kept across opens, one read or decode at a
time: tapping Space quickly queues at most one more (each new request replaces the one
waiting), so decodes never pile up, and a slow or hung disk slows previews, never the list or
other folders.

### Jump to Folder

**⌘J** opens a box over the window. Type parts of a folder's path and it ranks the folders
you've been to, best first, with the matched letters in bold; ↑↓ move, Enter (or a click) goes
there in the active pane, Esc closes. Each word must appear in the path in order, case aside,
and the last word should be in the folder's own name: "proj arb" finds `~/projects/arbor`
before `~/projects/arbor/src`. Among matches, folders you go to often and lately come first
(frecency, as zoxide does: every visit adds 1, weighted ×4 within the hour, ×2 within the day,
×½ within the week, ×¼ after; when the total passes 10,000, all shrink to 90% and folders
under 1 are forgotten). Typing a path (`/…`, `~/…`, or `C:\…` on Windows) works as go-to does:
Enter goes there, and the folders below it you've visited are listed under it.

The history is user data, kept in `history.log` beside the settings file, and costs nothing
until Jump opens:

- Each navigation that lands (opening a folder, back, forward, a sidebar place, Jump itself;
  not reloads or tab switches) sends its folder to one writer thread, which appends a line.
  The window does no disk I/O for it, and nothing is read at startup.
- The first time Jump opens, the writer reads and adds up the log. The box waits at most 8 ms
  for it, then says "Loading history…" until it lands. It then stays in memory for the session.
  Past 10,000 lines the writer rewrites the log as one line for each of the top 5,000 folders.
- Nothing checks that the folders still exist. Going to one that's gone says so and forgets it.
- `[jump] enabled = false` stops recording and reading it (and Jump says it's off);
  `[jump] exclude` lists globs of folders never recorded. "Clear Jump History" forgets it all:
  it can't be undone, so the first run asks and running it again right after clears.

### Select by Pattern

**⌘S** opens a small box: type a glob and it counts the matches as you type ("14 of 27
match") without changing the selection. **Enter** selects the matches, **⇧Enter** adds them to
the selection, **⌥Enter** takes them out. `*` is any run of characters, `?` one, `[abc]`,
`[a-z]` and `[!abc]` one from (or not from) a set, and `{png,jpg}` either; a name has to match
whole (`IMG_*`, `*.jpg`, `*.{png,jpg}`). It matches the rows on screen: hidden files only while
they're shown, and only what the filter lets through. Capitals count on Linux and don't on macOS
and Windows, as their usual file systems do; `[select] case_sensitive` changes that (the box
shows "Aa" or "aA"). Glob only: regular expressions would need a new crate.
**⇧⌘I** inverts the selection, and "Select All of This Kind" (palette) selects every row with
the cursor row's extension (or every folder). The selection is a set of names, so a listing that
refreshes keeps it.

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
| Jump to Folder… | ⌘J | Ctrl+J | `nav.jump` |
| Clear Jump History |  |  | `nav.clear_jump_history` |
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
| Copy to Other Pane | F5 | F5 | `file.copy_to_other` |
| Move to Other Pane | F6 | F6 | `file.move_to_other` |
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
| Select by Pattern… | ⌘S | Ctrl+S | `select.pattern` |
| Invert Selection | ⇧⌘I | Ctrl+Shift+I | `select.invert` |
| Select All of This Kind |  |  | `select.kind` |
| Show or Hide Hidden Files | ⌘., ⇧⌘. | Ctrl+., Ctrl+Shift+. | `view.hidden` |
| Sort by Name |  |  | `view.sort_name` |
| Sort by Kind |  |  | `view.sort_kind` |
| Sort by Size |  |  | `view.sort_size` |
| Sort by Date Modified |  |  | `view.sort_modified` |
| Reverse Sort Order |  |  | `view.sort_reverse` |
| Quick Preview (Toggle) | Space | Space | `view.preview` |
| Close Preview |  |  | `view.close_preview` |
| New Tab | ⌘T | Ctrl+T | `tab.new` |
| Close Tab | ⌘W | Ctrl+W | `tab.close` |
| Next Tab | ⌃⇥, ⇧⌘] | Ctrl+Tab, Ctrl+Shift+] | `tab.next` |
| Previous Tab | ⌃⇧⇥, ⇧⌘[ | Ctrl+Shift+Tab, Ctrl+Shift+[ | `tab.previous` |
| Reopen Closed Tab | ⇧⌘T | Ctrl+Shift+T | `tab.reopen` |
| Split View (Toggle) | ⌘\ | Ctrl+\ | `pane.split` |
| Switch to Other Pane | ⇥ | Tab | `pane.switch` |
| Go to sidebar place 1…9 | ⌘1 … ⌘9 | Ctrl+1 … Ctrl+9 | `place.1` … `place.9` |
<!-- commands:end -->

Notes: paste copies, or moves what you cut; ⌥⌘V always moves. A paste goes into the folder it
was asked for in, each paste on its own (two asked before the clipboard answers keep their own
folders), and is refused if that folder was moved or replaced before the clipboard answered.
Two panes on one folder by different paths (a symlink to it) are known to be the same folder.
The palette names ⌘\ "Split View" or "Close Split View", and Space "Quick Preview" or "Close
Preview", whichever it will do. Undo refuses if any item changed since. Rename selects the name before the extension; Enter saves, Esc cancels. Esc also cancels
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

[jump]
enabled = true                              # remember visited folders for Jump (⌘J)
exclude = ["/tmp/**", "**/node_modules"]    # never remembered; "dir/**" is dir and all in it

[select]
case_sensitive = false     # Select by Pattern; default true on Linux, false on macOS and Windows

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

Permanent delete (on purpose, for now), file watching (use ⌘R), previews of PDF, video, audio
and office documents (they need each platform's preview API), syntax highlighting in the text
preview, regular expressions in Select by Pattern, Tab-completion in go-to mode, redo, more than one operation
at a time, a context menu on Linux, reordering tabs or dragging them between panes, and
dragging rows out to Finder while the window is split on macOS (see `RUNE-GAPS.md`).
