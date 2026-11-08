# Decision log

Decisions made while the owner was away, so nothing blocked on a question. Each one can be
reversed. **Review** marks the ones the owner should look at first. Newest first.

Format: date · decision · why · how to undo.

---

## Waiting for owner review

- **2026-11-09 · Windows trash: residual race accepted (Review).** A file that grows past the
  Recycle Bin's capacity, or a bin setting changed, between the pre-check and the shell's delete
  could still be deleted permanently. The pre-check plus the sink's `PostDeleteItem` check cover
  the realistic cases; full protection would need staging every trashed item. The reviewer
  flagged it; the agent accepted it. Undo: stage items before recycling (`src/trash.rs`).

- **2026-11-09 · Packages are unsigned (Review).** `Arbor.app` is signed ad hoc and not
  notarized, and `arbor.exe` isn't signed: notarizing needs an Apple Developer account and
  signing on Windows a code-signing certificate, both yours to get. The README tells users how
  to open them the first time (right-click → Open; SmartScreen's More info → Run anyway). To
  change: add the identity to `scripts/package.sh` (`codesign --sign "Developer ID…"`, then
  `notarytool`) and a signing step for Windows in CI.
- **2026-11-09 · Releases wait for your first tag (Review).** CI uploads the three packages on
  every push (artifacts `arbor-macOS`, `arbor-Linux`, `arbor-Windows`); a pushed `v*` tag also
  publishes them as a GitHub Release (the `release` job, with `gh release create`). No tag or
  release was made. To release: `git tag v0.1.0 && git push origin v0.1.0`.
- **2026-11-09 · Windows: the default-folder registry plan is untested on a real machine
  (Review).** CI writes and restores it under a test-only key (`windows_round_trip_in_the_real_registry_under_a_test_key`), and
  the README has a hand check. Folders (`Directory`), drives (`Drive`) and Win+E (This PC's
  `opennewwindow`) only, as a new "Open in Arbor" verb made the default, not the `Folder` class
  (Control Panel and other virtual folders would stop opening) and not Explorer's own `open`
  verb (so Restore only has to take Arbor's verb away). Undo: `handler::windows::plan`.

- **2026-11-09 · Windows full thumbnail generation stays OFF until you decide (Review).**
  Windows previews of PDF and video only show thumbnails the shell already has cached (1–8 ms).
  Generating a new one (`IShellItemImageFactory`) can't be cancelled once started: 541 ms for a
  1080p MP4 and 174 ms for a 4K MP4 on the CI runner, and the next preview waits behind it.
  Turning it on (`UNCANCELLABLE_WINDOWS_THUMBNAILS` in `src/thumb.rs:43`) breaks NORTHSTAR's
  view-triggered-work rule, so it needs an Exceptions-log entry with your sign-off; AGENTS.md
  forbids the agent adding one. Recommendation: approve it like WebP (similar cost, UI never
  affected). To approve: flip the flag and add the exception entry.

- **2026-11-08 · Windows previews show only thumbnails the shell already cached (Review).** Full
  generation (`IShellItemImageFactory::GetImage` without `SIIGBF_INCACHEONLY`) can't be
  cancelled once it starts, so it's behind `UNCANCELLABLE_WINDOWS_THUMBNAILS` in `src/thumb.rs`,
  `false`. The timings for the owner's call are in BENCH.md (milestone 9). Undo: set it to `true`,
  after an Exceptions-log entry in NORTHSTAR.md.

- **2026-11-08 · NORTHSTAR cold-start budget changed (Review).** The speed-budget row now reads
  "under 100 ms on Windows and Linux; on macOS, within 25 ms of a bare native window (macOS's own
  launch is about 110 ms)". The owner asked for this to be drafted, then said to proceed with
  recommendations rather than wait. Why: on an M4 Pro, Arbor's median is ~134 ms, of which ~15 ms
  is exec and 65–80 ms is AppKit plus Rune start-up before Arbor's code runs. Arbor's own share is
  about 3 ms. Undo: revert the NORTHSTAR.md line.

- **2026-11-09 · Codex QA on the Mac runs with `danger-full-access` (Review).** Codex's
  `workspace-write` sandbox (macOS seatbelt) blocks the window server, so it couldn't launch
  Arbor. It now runs from Terminal.app (started with `open -a Terminal`) in the GUI session with
  full access, confined only by its instructions: operate only the "Arbor QA" app and one Finder
  window, stay in `~/arbor-qa/play`, never empty the Trash. Arbor is wrapped in a throwaway,
  ad-hoc-signed bundle `~/arbor-qa/Arbor-QA.app` (`gg.smr.arbor.qa`) because computer use
  addresses apps by bundle id; `RUNE_DIRS_ROOT` keeps its settings and Jump history in
  `~/arbor-qa/dirs`. Undo: delete `~/arbor-qa`; revoke computer-use access to "Arbor QA".
- **2026-11-09 · Windows trash pre-checks the Recycle Bin's registry settings.** NukeOnDelete,
  MaxCapacity versus item size, or unknown → Arbor's permanent-delete confirm instead of the shell.
  Why: `TSF_DELETE_RECYCLE_IF_POSSIBLE` only means "if possible"; oversized items can be deleted
  permanently with no UI. Undo: drop the pre-check in `src/trash.rs` (not recommended).

- **2026-11-09 · Incident: the QA bundle left `launchservicesd` spinning (Review).** The
  throwaway `~/arbor-qa/Arbor-QA.app` used a zsh script as its bundle executable (it `exec`ed the
  real binary). After Codex quit it, launchservicesd kept polling the dead pid at ~95% of a core
  from 00:14, adding ~15–25 ms to every app launch on the Mac. The bundle is unregistered and
  deleted; the daemon needs a logout or `sudo killall launchservicesd` (the owner was told via
  Telegram). Future QA bundles must use the real binary as the bundle executable and pass
  arguments and environment via `open --args` / `LSEnvironment`, as the shipping `Arbor.app`
  does. The 166 ms cold-start bench at 55134bd ran during this and isn't valid; alternating
  passes showed no Arbor regression (+9 ms over a bare AppKit window, within budget).

## Made with the owner's answer

- **2026-11-08 · Windows trash checks the Recycle Bin's registry settings first** (owner's
  decision). Before the shell is asked: the volume's `BitBucket\Volume\{GUID}` key
  (`NukeOnDelete`, `MaxCapacity` MB), the parent key's `NukeOnDelete` when the volume has none,
  and the `NoRecycleFiles` / `RecycleBinSize` policies; the item's size is walked on the worker
  (cancellable, not following reparse points). Turned off, too big, or anything unknown means
  Arbor's permanent-delete question. Removable drives are allowed when these say they have a bin.
  Agent's calls within it: (1) a volume key with no `NukeOnDelete` anywhere counts as unknown;
  (2) only `MaxCapacity` is compared, not what's free in the bin, since a full bin drops its oldest
  items to make room (a size over the limit is what makes Windows delete for good); (3) the
  existing progress-sink check stays as a second line. Undo: `trash::bin_verdict`.

- **2026-11-08 · OS thumbnail caches are acceptable** for PDF and video preview (macOS QuickLook,
  Windows shell thumbnails). Arbor stores nothing; the OS cache is the one Finder and Explorer
  already fill. Linux stays text and images only.
- **2026-11-08 · WebP preview exception, cap 4 MP.** Logged in NORTHSTAR's Exceptions log with
  sign-off. It was first approved at 12 MP on a wrong estimate (~100 ms); the measured 1.0 s at
  12 MP led to 4 MP.

## Made by the agent (owner said "I trust your judgment")

- **2026-11-09 · Windows: still no single instance (Review).** See the milestone 11 entry
  below if it changes; otherwise each folder opened from Explorer starts a new Arbor.
- **2026-11-09 · File-list rows are named from their cells, with no separate description.**
  A reader hears "notes.txt, txt, 5 KB, Today 10:30" as the row's name. A description holding
  kind, size and date apart would cost a string per visible row every frame for everyone, since
  Rune can't build labels lazily (RUNE-GAPS.md). Undo: `.label(name).description(…)` in
  `file_row` (`src/view.rs`).
- **2026-11-09 · The file list can hold keyboard focus and hands every key to the window's
  handler; no focus ring on it** (its selection shows focus, as in AppKit's tables). Both lists
  of a split window name the active pane's cursor row as their active descendant, since Rune
  can't move focus when Tab switches panes. Undo: `list` in `src/view.rs`.
- **2026-11-09 · Announced at once (alerts): an operation's errors or refusal and a folder that
  can't be read; every other note politely.** Undo: `State::alert`.
- **2026-11-09 · Redo: undoing records the undo's reverse; any new operation (one that starts,
  or a rename or new folder) clears redo.** ⇧⌘Z everywhere, Ctrl+Y on Windows only (Linux apps
  mostly use Ctrl+Shift+Z; Ctrl+Y would be a new key nobody expects there). It's "Redid N items"
  in the status bar. Undo: `Cmd::Redo` and `Outcome::inverse`.
- **2026-11-09 · Go-to completion follows the platform's case rule, not
  `[select] case_sensitive`,** lists files as well as folders (Enter opens a file), shows
  hidden items while they're shown or once a dot is typed, and its card sits over the status
  bar (nothing is under the field). Undo: `src/complete.rs`.
- **2026-11-09 · The text preview's contents are a Tab stop that arrows scroll** (three lines a
  press), so keyboard users can read a long file. Undo: `preview_scroll_key` in `src/view.rs`.

- **2026-11-09 · A reload puts back exactly what was selected; a cursor the user deselected
  (⌘-click) isn't reselected.** The first selected item takes the cursor instead, so what an
  action acts on doesn't change (from the review). Undo: `Folder::picked`.
- **2026-11-09 · Windows' `RecycleBinSize` policy is a percentage of the drive** (it was read as
  megabytes); the drive's size comes from `GetDiskFreeSpaceExW`, and an unknown size means the
  permanent-delete question. Undo: `bin_verdict`.
- **2026-11-09 · macOS deletes stop at volume boundaries, not only devices** (`statfs`'s mount
  point per folder: the review found a walk entering `/System/Volumes/Data` and the `/Users`
  firmlink, which share `/`'s device number). For permanent delete, cancelled-copy cleanup,
  cross-disk move cleanup and the count of what's left. Undo: `delete::volume`.

- **2026-11-09 · Linux: the folder default is written into `~/.config/mimeapps.list` by Arbor,
  not with `xdg-mime`.** It's the freedesktop.org file `xdg-mime` and `gio` write; editing it
  directly lets Restore put back the user's exact line (or remove Arbor's), and works without
  xdg-utils (not installed on tabot-dev). Undo: `handler::linux`.
- **2026-11-09 · Linux: the D-Bus activation file is written by "Make Arbor the Default for
  Folders", not by `install.sh`.** Installing shouldn't change where "Show in folder" goes (the
  task asked for the install script to install it; opting in is NORTHSTAR's rule). Arbor serves
  `org.freedesktop.FileManager1` only while that file is its own, and asks the session bus to
  reload after writing or removing it (a bus started before `~/.local/share/dbus-1/services`
  existed doesn't watch it; found under Xvfb, where the system's Thunar kept being started).
  Undo: `install.sh` could copy the file, and `incoming::linux::spawn` drop its check.
- **2026-11-09 · macOS: only a viewer the user chose is backed up; otherwise Restore sets
  Finder.** LaunchServices' answer without a choice is a guess among apps declaring folders
  (`com.apple.archiveutility` on the M4 Pro), and restoring it would hand folders to Archive
  Utility. The choice is read from `LSHandlers` in `com.apple.launchservices.secure.plist`.
  `Arbor.app` declares folders at rank Alternate so installing it never makes it the default.
  Undo: `handler::mac`.
- **2026-11-09 · Restore never overwrites what changed since.** Each setting goes back only if
  it still holds what Arbor wrote; the rest are named in the status bar. Making Arbor the default
  again keeps the first backup. A damaged backup file is an error, not a guess. Undo:
  `handler::{linux, windows, mac}::restore`.
- **2026-11-09 · Single instance: macOS and Linux get it from the platform, Windows later.**
  macOS sends folders to the running app (open-documents events) and Linux's FileManager1
  requests reach the running Arbor; each opens a tab. On Windows each folder opened from Explorer
  starts a new Arbor: a named-pipe listener would do it without polling, but nothing can be run
  on Windows here to check it. `xdg-open` on a folder also starts a new Arbor (it runs the
  desktop file's `Exec`). Undo: n/a (later: a pipe in `incoming.rs`).
- **2026-11-09 · The first folder another app sends replaces the window's first tab while it's
  untouched** (one tab, nothing navigated, picked, filtered or searched): launching Arbor for a
  folder shows just that folder, not Home plus a tab. Its history starts there (`Nav::Fresh`).
  Undo: `State::untouched` returning false.
- **2026-11-09 · No new crates.** `build.rs` writes the Windows `.res` (icon group, PNG icons,
  `VS_VERSIONINFO`) itself and hands it to MSVC's linker, instead of `winresource` or
  `embed-resource` (which need `rc.exe`); Linux's D-Bus uses the zbus 6 and Tokio Rune's `dbus`
  feature already builds. Undo: `build.rs`.
- **2026-11-09 · Release builds on Windows are windowed programs** (`windows_subsystem =
  "windows"`), so no console opens with Arbor from Explorer; debug builds keep the console.
  Timing logs still reach a pipe (the bench). Undo: the attribute in `src/main.rs`.
- **2026-11-09 · The icon** is an original tree glyph (trunk, two branches, three round crowns)
  on a green rounded square, drawn by `scripts/icon.py` with signed distances (no PIL), in
  Apple's icon grid for macOS. The Mac package is Apple silicon only (CI's runner); a universal
  build is later. Undo: edit `STROKES`/`CROWNS` in the script and rerun it (and
  `scripts/package.sh --icns` on a Mac).
- **2026-11-09 · The dialog's action button is the accent colour, not red, for the folder-default
  commands**: red stays for what can't be undone. Undo: `confirm_view` in `src/view.rs`.
- **2026-11-09 · `arbor --restore-folder-default`** restores without a window, for uninstall
  scripts (`uninstall.sh` runs it first and stops if it fails). Undo: the check in `main`.

- **2026-11-09 · Text preview: horizontal scrolling, not soft wrap** (it already scrolled
  sideways; checked under X11 with a horizontal wheel and with Shift+wheel). Code and logs keep
  their columns, and only the lines in view are laid out either way. The Mac QA's "no change" is
  most likely its tool's synthetic Shift+scroll, which macOS doesn't turn into a horizontal one
  as it does for a real mouse (Rune relies on macOS for that); a trackpad swipe is horizontal.
  Undo: `.wrap()` on the preview's lines in `src/view.rs`.
- **2026-11-09 · 8-bit text that isn't UTF-8 is Windows-1252**, said in the panel ("Shown as
  Windows-1252"). UTF-8 with a few invalid bytes stays UTF-8 (shown with �) only if it has real
  multi-byte characters too; text full of invalid UTF-8 and no control bytes counts as text now
  (it was binary, and since milestone 9 went to Quick Look, which drew Latin-1 as Mac Roman).
  Undo: `sniff_text` in `src/preview.rs`.
- **2026-11-09 · A reload keeps a selection the user made after it was asked for** (a click, an
  arrow, ⌘A, Select by Pattern bump `Folder::picks`), instead of the operation's own pick (the new
  folder, the copy), and starts no rename then. This was the QA's "can't copy a folder into
  itself". Undo: the `picks` check in `State::land`.
- **2026-11-09 · Any drag out that nothing took says how to move files** ("Nothing took the drop.
  … cut and paste them (⌘X, ⌘V) or use split view (⌘\\)"), also after an Escape or a refused drop
  elsewhere, since Rune doesn't say where a drag ended. Undo: `State::drag_out_ended`.

- **2026-11-09 · Long paths are cut at whole folders, by estimate.** Jump rows and Search's
  folder column show "…/" and the trailing folders that fit, estimated from per-character
  advances measured once per thread (3% to spare; a character outside ASCII counts as the widest
  ASCII one), with Rune's head truncation as the fallback. Search's folder column now has a fixed
  share of the row (0.6 to the name's 1, as its grow factor was) so the cut knows its room.
  Undo: `fit_path` in `src/view.rs`.
- **2026-11-09 · The Jump bench's history is synthetic paths under the home folder** (2–9 levels,
  realistic names, not created on disk) and it types a different pair of letters each round.
  Its numbers aren't comparable with earlier Jump rows. Undo: `make_jump_history` and
  `JUMP_QUERIES`.
- **2026-11-09 · `--pace` sleeps on a thread and wakes the window** (bench only): no timer and no
  polling in the app. Undo: `paced` in `src/timing.rs`.

- **2026-11-08 · Permanent delete stops at a mount point** (it doesn't skip it and carry on with
  the rest of the folder): the owner said "stop and report". Deleting a mount point itself is
  refused the same way, since its own folder can't be removed while mounted and the confirm said
  "and everything in it". A btrfs subvolume has its own device number, so it's treated as a mount
  too (safe; its removal needs btrfs's own tools anyway). Undo: `src/delete.rs`, `Halt::Mounted`.
- **2026-11-08 · A cross-disk move leaves the originals of a disk mounted inside the moved
  folder** (they were copied; the move is reported as split, as for any original it couldn't
  remove). Undo: the `Fence` check in `Worker::remove_sources`.

- **2026-11-08 · Quick Look is asked for a thumbnail, never an icon** (representation types
  Thumbnail and LowQualityThumbnail). A generic file-type icon says less than the card. Undo: add
  `Types::Icon` in `src/thumb.rs`.
- **2026-11-08 · When the OS has no thumbnail, the card says "Can't preview this file."**
  (changed 2026-11-09 after the Mac QA found a blank panel for a broken PDF; it first showed no
  note). A request that hasn't answered in 10 s (`thumb::OS_LIMIT`) is cancelled and gets the
  same card. Binary files of other types keep "Binary file: there's no preview for its
  contents." Undo: the `Thumb::Nothing` arm in `preview::load_with`.
- **2026-11-08 · Every binary file Arbor can't show goes to the OS's thumbnailer**, not just a
  list of extensions, so fonts, `.usdz`, `.sketch`, Keynote and so on preview where the OS can.
  Only extensions that are never text go straight to it (PDF, video, audio, documents, fonts,
  HEIC…); `.key` and `.ts` (also keys and TypeScript) are left to their bytes. Undo:
  `preview::file`'s binary branch back to the card.

- **⌘W on the last tab of the last pane does nothing.** Passing it on would make macOS close the
  window and quit, losing every tab's history.
- **⌘1–9 stay as sidebar places**; tabs use ⌃Tab, ⇧⌘[ / ⇧⌘], ⌘T, ⌘W, ⇧⌘T.
- **⌘S is Select by Pattern** (Nautilus uses Ctrl+S for this; "save" means nothing in a file
  manager), ⇧⌘I Invert Selection, ⌘J Jump to Folder, ⌘F Search, ⌘K / ⇧⌘P palette, ⌘, Settings.
- **Delete Permanently is ⌥⌘⌫ on macOS and Shift+Delete on Windows/Linux** (each platform's
  convention), so its default key differs per OS.
- **Mixed trash selections:** what can go to the trash goes first (undoable), then one confirm
  for the rest.
- **Cancelled-copy cleanup deletes permanently** (not to Trash): these are Arbor's own unfinished
  copies, identified by the handle that created them and re-checked just before removal.
- **Select by Pattern is glob only**; regex-lite was 20–40× slower than a glob over 50k names.
- **Recursive search restarts on every keystroke** (no debounce, which would need a timer); up to
  4 walker threads; while walking, only the first 1,000 results are listed (Rune row-key cost, in
  RUNE-GAPS.md).
- **Unavailable palette commands are dimmed, not hidden**, and say why when chosen.
- **Config: the later key binding wins**; duplicates within the file are reported.
- **Undo: a refused or failed undo stays on the stack**; a partial undo keeps only what didn't go
  back.
- **Known limits left documented rather than fixed:** a same-size edit within a filesystem's
  timestamp resolution looks unchanged to undo; a crash mid-move on filesystems without a
  no-replace rename can leave a duplicate hard link (no data loss); on Windows, search-result
  identity uses creation time (15 s tunnelling caveat).
- **"dl" does not find Downloads in Jump** (zoxide behaves the same); left as is.
