# Decision log

Decisions made while the owner was away, so nothing blocked on a question. Each one can be
reversed. **Review** marks the ones the owner should look at first. Newest first.

Format: date · decision · why · how to undo.

---

## Waiting for owner review

- **2026-11-10 · Milestone 16: drag-select and the icon view (Review: the start rule and the
  keys).**
  - **Where a rectangle starts (Review).** A press on an item acts on the item: a list row's
    name text (not the room after it), any part of a row that's already selected, or a grid
    cell's icon and name. Anywhere else in the list it starts a rectangle once it moves 6 pt: an
    unselected row's ▸, the room after its name and its Kind, Size and Date cells, the space
    below the rows, a grid's gutters. Finder's list view drags from the name only; Explorer's
    details view drags a selected row from anywhere; this takes both, so a selection is still
    dragged from wherever it's grabbed. A click (no move) on a row's empty part still selects
    the row; a click below the rows clears a multiple selection (a folder always keeps a cursor
    item, so "nothing selected" is the cursor alone). The rectangle selects every row it
    touches vertically (rows span the width) or every icon cell it touches (the cell less its
    6 pt inset). ⇧ adds to the selection the press started with, ⌘ toggles against it (⌘ wins
    with both, as with clicks); Esc or losing the pointer restores the selection. Undo:
    `band.rs`, `whole` and the name's handlers in `view/list.rs::file_row`.
  - **Keys (Review): ⌥⌘1 As List, ⌥⌘2 As Icons, ⌘+ (also ⌘=) larger, ⌘- smaller icons.** ⌘1–⌘9
    stay the sidebar's places (Finder's ⌘1/⌘2 would have meant moving them). No default-key
    conflict on any platform (the registry test checks). Off macOS they're Ctrl+Alt+1/2, which
    on layouts that type characters with AltGr (German AltGr+2 is ², Polish has none on the
    digits) switches the view instead of typing that character into the filter; rebind in
    `[keys]` if that matters. Undo: `AsList`, `AsIcons`, `LargerIcons`, `SmallerIcons` in
    `commands.rs`.
  - **The icon size is one for the window** (⌘+/⌘- change every grid), starting from
    `view.icon_size`; the view is per tab (its memo), new tabs and a split's second pane take
    `view.default`. No per-folder memory (it would be user data, but nothing asked for it yet).
    Undo: `State::icon_size`, `settings.view_default` in `panes.rs`.
  - **The icon view hides the column headers** (nothing to click in a grid); the Sort commands
    and the palette still sort. The path bar gets the two view buttons at its right. Undo:
    `view/pane.rs`.
  - **Thumbnails: 2 workers, images by Arbor's decoders everywhere, documents and video through
    the OS on macOS and Windows (cache only there), nothing on Linux beyond images.** An image
    the preview would refuse (over its caps, WebP over 4 MP) keeps its type icon. An item that
    decodes to nothing isn't asked again while it stays on screen. Undo: `tiles.rs`.
  - **The type icons** are drawn from plain shapes in each kind's colour (folder tab and body,
    a page with a kind band and its extension from medium size up, an app tile); no icon font.
    Undo: `view/grid.rs::type_icon`.

- **2026-11-10 · Empty Trash with nothing to empty only informs** (Mac QA: without Full Disk
  Access the dialog explained it and still showed a red "Empty Trash"; pressing it re-listed the
  Trash on the worker and put the same Full Disk Access text in the status bar, deleting nothing).
  When the count finds nothing Arbor can empty, the same dialog has no destructive button: Close,
  plus **Open Privacy Settings** (`open x-apple.systempreferences:…Privacy_AllFiles`) when macOS
  refused. That includes an already-empty Trash ("Nothing to Empty"), which used to keep the red
  button too. NORTHSTAR calls the confirmation "the only kind of dialog Arbor shows"; this is that
  dialog in the state where there's nothing to confirm, not a new kind. When only some trashes
  can be read, it keeps the button, counts what can be emptied, names the unreadable trash and
  says it can't be undone. Undo: `contents::ask_words`, `Ask::action`/`settings`.
- **2026-11-10 · A row's OS drag never starts from a release** (Mac QA: after a single-pane drag
  nothing took, keys did nothing until Esc). Cause: winit sends a release's position as a move
  before the release, and Rune starts a drag-out from that move when it crosses the threshold;
  AppKit's session then waits for a release that already happened, eating keys. Arbor's
  `drag_out` asks AppKit for the current event and offers nothing on a release or plain move; the
  status bar then says how to move files. Such a flick starts no drag (Finder would drag).
  Off macOS, an unsplit in-window row drag let go on nothing now says the same (it said nothing).
  RUNE-GAPS has the entry. Undo: `actions::os_drag_can_start`, the `drag_out` closure in
  `view/list.rs`, the `far && split.is_none()` branch there.
- **2026-11-10 · Shift + wheel turns sideways in the text preview on macOS too** (Mac QA: the
  tool's horizontal scroll didn't move the preview). Rune delivers horizontal deltas fine (tests
  feed a tilt wheel and a trackpad swipe); what it leaves to macOS is turning Shift + wheel, which
  macOS does only for a real mouse. The preview turns an unturned one itself. Undo:
  `view::preview_shift_wheel` and its `on_wheel`.

- **2026-11-10 · Linux opens with `xdg-open`, then `gio open`** (Linux QA: with no
  `xdg-open`, Open and Settings said "No such file or directory" though `gio` was there). Open,
  Show in folder and the settings file use the first one found on PATH, as an executable file
  in an absolute PATH folder (a relative one, `.`, would run whatever is where Arbor started),
  and fall through to the next if it won't start; with neither: "neither xdg-open nor gio is
  installed (xdg-utils or GLib provides one)". The path is one argument (`gio open -- PATH`),
  never through a shell. Only a failure to start falls through: an opener that starts and
  then fails isn't seen (it's reaped on a thread, never waited for). Undo: `fs::OPENERS`.
- **2026-11-10 · Get Info counts what's inside, never the selection itself** (Linux QA: a
  folder of 2,000 files said "2,001 items", an empty one "1 item"). One folder, or the folder
  on screen: "2,000 items · 8.9 KB", or "Empty" (not when a folder couldn't be read or a disk
  inside was left out: then "0 items · 0 B" and the caveat). Several items: "3 selected ·
  2,014 items inside · 9.1 MB", the selected items apart from the contents of the selected
  folders (the old count added both into one number), and "3 selected · 62 B" when none is a
  folder. Undo: `Total::line` and the seed in `info::retarget_info`; the walk counting each
  root again (`walk_with`).
- **2026-11-09 · Milestone 15: what malformed input gets.** Each refusal below is "damage, not
  a guess"; each has a regression test with the input a property test found.
  - Go-to tries the typed path exactly first, then without trailing spaces (a name can end in
    one; Tab completion writes it whole). Undo: `app::typed_targets`.
  - `~\` is the home folder only on Windows. Elsewhere a backslash is part of a name. Undo:
    `fs::expand`.
  - Tab completion leaves out names that aren't UTF-8 (they can't be typed). Undo:
    `complete::candidates`.
  - Globs over 4096 characters are refused. Undo: `pattern::MAX_LEN`.
  - Batch rename's regexes are capped at 64 KB compiled, and a regex rule is always planned on
    the worker. Undo: `rename::REGEX_SIZE`, `c.is_regex()` in `batch_replan_within`.
  - A `history.log` line over 128 KB is skipped. Undo: `jump::MAX_LINE`.
  - These are refused rather than read:
    - a `.trashinfo` with two `Path=` lines, `%00` or `%2F`;
    - a `file://` URI with `%00`, `%2F` or a fragment;
    - a desktop id with a separator, `..` or a leading dot;
    - a `mimeapps.list` that isn't UTF-8;
    - a backed-up folder default that isn't one line of UTF-8 text, a macOS viewer that isn't
      a bundle id, or a Windows value shorter than its type.
  - A mount whose device name isn't UTF-8 gets no device, so Eject falls back to `gio` with
    the exact mount point.
  - Property tests have a fixed seed per property, so CI is deterministic.
    `ARBOR_PROP_SEED` and `ARBOR_PROP_CASES` hunt locally. There's no crate (`src/prop.rs`).
    Undo: per item.

- **2026-11-09 · Empty Trash refuses a whole trash whose folders aren't safe** rather than
  emptying part of it: a trash folder, its `files/` or `info/` that's a link or someone else's,
  or a shared `.Trash` that isn't sticky, is said in the dialog and nothing in it is read or
  deleted; one moved or replaced between the count and the button is refused the same way. On
  Linux each item's file handle (inode plus generation, `name_to_handle_at`) is checked too,
  where the file system gives one. Undo: `trash::contents::open`, `Worker::empty_bins`.
- **2026-11-09 · Put Back from a disk's trash refuses any link on the way back, even one that
  stays on the disk**, and an absolute record outside the disk. Telling a link that stays on
  the disk from one that leaves it would mean resolving it; refusing is the smaller surprise
  (the item stays in the Trash, and the message names the link). The home trash keeps
  restoring to any absolute path through the user's own links. Left: the move itself is still
  by path after the check, so a folder swapped for a link in that instant isn't caught. Undo:
  `trash::contents::original`.
- **2026-11-09 · A pending inline rename is a label: Open, Copy, Cut, Rename, Open With, Get
  Info, Reveal, Copy Path and Add to Sidebar on that row say "Wait for the rename to finish"**
  until it lands (Trash, Delete and the rest already waited for the operation). Undo:
  `Entry::pending`, `renaming` in `State::unavailable`.
- **2026-11-09 · The Intel slice's Rosetta run stays best effort; the README says so** (rather
  than failing CI on a runner that can't install Rosetta, a download that has failed before).
  Undo: drop the `|| true` and the `else` branch in `ci.yml`'s macOS package check.

- **2026-11-09 · Empty the Trash: which trashes, and what's deleted (Review: never run on a
  real Trash; Windows never run at all).** Linux: the home trash plus `.Trash/$uid` and
  `.Trash-$uid` on local disks only (no network shares, no FUSE mounts but `fuseblk`: a dead
  one could hang the count). macOS: `~/.Trash` plus local `/Volumes/*/.Trashes/$uid`, with the
  shared no-follow delete, not Finder through AppleScript (that would show Finder's own
  confirmation and needs Automation permission; listing `~/.Trash` needs Full Disk Access, which
  Arbor explains on `EPERM`). Only the items the dialog's count found are deleted, each only
  while it's still that item; anything trashed after the count stays, and `.trashinfo` files
  without an item are left (Arbor's own trash writes the record a moment before the item, so
  "orphans" can be in flight). Windows: `SHEmptyRecycleBinW` with `SHERB_NOCONFIRMATION`,
  `SHERB_NOPROGRESSUI` (Arbor's status bar shows it; the shell's window would be a second
  dialog, at the cost of the shell's own Cancel) and `SHERB_NOSOUND` (Arbor makes no sounds);
  it can't be stopped once started, and the bin is counted again to report what's left. Undo:
  `trash::contents::bins` and `ops/trashed.rs`.
- **2026-11-09 · "Empty the Trash…" and "Go to the Trash", not Finder's "Empty Trash…".** With the
  shorter titles the palette ranked them above "Move to Trash" for "trash" (shorter wins a tie),
  so typing "trash" and Enter would no longer trash the selection. Undo: `EMPTY_TRASH` and
  `GO_TRASH` in `src/commands.rs`, and the ranking test.
- **2026-11-09 · The sidebar's Trash sits under the favorites, not as one** (no ⌘ number, no
  drag, its own menu: Open, Empty the Trash…). None on Windows. Undo: `trash_row` in
  `src/view/sidebar.rs`.
- **2026-11-09 · Put Back is Linux only, refuses when the item's folder is gone, and is
  undoable.** macOS has no public API for Finder's Put Back (the original location is kept in
  Finder's private metadata), so the command is dimmed there and says to use Finder; Windows
  can't list the Recycle Bin. A taken name gets a free one beside it ("name 2", said in the
  status bar); a missing folder isn't made again (Nautilus recreates it; refusing is the
  smaller surprise). Undo trashes the items again. Undo: `Worker::restore_from_trash`.
- **2026-11-09 · Tabs: keys and drops.** Move Tab Left/Right are ⌃⇧Page Up/Down on every platform
  (browsers' keys off macOS; macOS has no standard), Move Tab to Other Pane is ⇧⌘\\ (beside
  ⌘\\ for split) and is dimmed unsplit rather than splitting. A tab let go anywhere over the
  other pane moves there (where the pointer is over its strip, else at the end); moving a
  pane's only tab closes that pane without adding it to Reopen Closed Tab (the tab lives on).
  Undo: `State::move_tab_to_pane`, the three commands in `src/commands.rs`.
- **2026-11-09 · Universal Mac package, macOS 11 for both slices (Review: the Intel slice runs
  only on CI's runner, under Rosetta).** One `LSMinimumSystemVersion` (11.0) and
  `MACOSX_DEPLOYMENT_TARGET=11.0` for both slices, so the binary's own minimum matches the
  Info.plist (Intel's default would be 10.12, which nothing here tests; Big Sur is the first
  macOS on Apple silicon). The M4 Pro has no Rosetta and a nearly full disk, so neither Rosetta
  nor the x86_64 target was installed there. Undo: `ARCH=native` in `scripts/package.sh`.

- **2026-11-09 · Windows: one Arbor per session through a named pipe (Review: never run on
  a real Windows machine; CI runs a hand-off between two threads).** A second `arbor.exe
  <folder>` writes its folders to `\\.\pipe\gg.smr.arbor.<session>.<user>` and exits; the first
  waits on the pipe in a blocking thread (no polling) and opens each in a tab, and lets itself
  come to the front (`AllowSetForegroundWindow`). `arbor.exe` without a folder (Win+E) still
  opens its own window: a new tab of Home in the running one is the other choice. The pipe takes
  local clients only, and its default security lets only this user and administrators write.
  Not handled: another user's process taking the name first would receive the folder paths (it
  can't make Arbor do anything). Undo: `incoming::windows`, and its call in `main`.

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

- **2026-11-09 · A drag starts with the rows as listed; a drop out to another app is that app's
  to check** (review: starting a drag stat'ed every selected item, and opened it on Windows, on
  the window's thread). Nothing is read when a drag starts. A drop on the other pane goes to
  the worker as an `Op::Checked` with each row's listing identity, so a replaced item is left
  alone there; a drop in another app (Finder, Explorer, a file manager, an editor) happens after
  Arbor's part ends, so Arbor can't check it, and a row replaced since the listing is offered as
  its path. Undo: `State::drag_item`.
- **2026-11-09 · A row listed without an identity is never moved or removed** until its folder
  is read again ("Couldn't verify “x” when the folder was read, so it was left alone; press ⌘R
  to read it again"; for search results, "search again"): trash, move, paste of a cut, F6, a
  drop, inline and batch rename, Put Back and Delete. A copy (F5, paste, Duplicate) still goes
  ahead: it leaves the item where it is. Undo: `ops::Checks` (`Option` identities), `run_checked`.
- **2026-11-09 · Where identity is told by ctime alone, Arbor's own changes carry over**
  (review: two hard links trashed together had the second refused; an item Arbor just renamed
  was refused until its folder was read again). After each rename, move, trash, delete or Put
  Back, the worker reads what its change made of the item, where it went or through another name
  for it the same operation still has to act on (a name still linking the inode means it can't
  have been reused), and the rows still showing the old identity take the new one. Left: a hard
  link Arbor wasn't asked to act on, in a folder it doesn't read again, after the other name was
  deleted for good (nothing left to read the new ctime through), is refused until reloaded.
  Another app's change is still refused. Undo: `Worker::acted`, `Folder::reidentify`.
- **2026-11-09 · Actions on listed items check the listing's identity (the PM's call), and
  Windows lists with file ids** (review of milestone 15's fixes). Trash, move, copy, rename,
  F5/F6, paste, drag and Duplicate on a folder's rows now carry each row's `fs::Ident` from
  the listing (`ops::Checks`), as search results did; a replaced item is left alone with "“x”
  was replaced since the folder was read, so it was left alone; press ⌘R to read it again"
  (Ctrl+R off macOS; the default key, not a rebinding). Edits still pass. Windows' listing
  enumerates with `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` (no 8.3 short names,
  which `FileIdBothDirectoryInfo` made NTFS look up: 23.8 ms against std's 13.9 ms on CI), so
  each row's identity has the volume and NTFS file id, and checks read the file id through a
  handle: a replacement within NTFS's 15 s tunnelling is refused (CI: tunnelled, refused). FAT
  and file systems that can't answer it are read `std`'s way, by creation time as before; a
  ReFS id that needs 128 bits isn't used; search results stay creation time (the walk can't open
  every file). Listing 10k entries, read and sort, medians: Linux 14.9–19.5 ms before, 15.2–18.1
  ms after (tabot-dev, load 3–4); macOS 11.4–12.0 before, 12.0–12.3 after (MacBook); Windows
  16.5 ms with file ids against 15.2 ms for std's read, taking turns (CI runner). Undo:
  `Folder::target_checks` (searching only), `fs::read` (`read_std` everywhere).
- **2026-11-09 · Without a birth time, identity adds the Linux file handle, else the ctime**
  (review: with statx returning `ENOSYS`, Delete removed 50 of 50 replacements that got the
  freed inode). Where `statx` gives no btime, a listing reads each entry's handle
  (`name_to_handle_at`: inode and generation on ext4), as do the checks; two handles are
  compared instead of the ctime, so edits still pass. Without a handle (or where only one side
  has it: search results' identities carry none) the ctime is compared, so on those systems an
  item edited since it was chosen is refused too. Cost, only there: 10k entries 25.5–26.0 ms
  against 16 ms with birth times (tabot-dev). Undo: `fs::Ident`'s `changed` and `handle`.
- **2026-11-09 · Get Info compares a selected folder's whole identity** when it opens it
  (`dirfd::ident_of_fd`), not device and inode. Undo: `info::walk_tree`.
- **2026-11-09 · A newer batch rule stops the older plan before it's compiled**, so a rule that
  doesn't compile stops it too. Undo: `batch_replan_within`.
- **2026-11-09 · uninstall.sh's UTF-8 check is Unicode's**: overlong forms, surrogates and code
  points past U+10FFFF are damage, as Restore says; each range's edges are tested against
  Arbor's reader. Undo: `not_text` in `packaging/linux/uninstall.sh`.
- **2026-11-09 · Object identity and content fingerprint are two notions** (the PM's call;
  replaces the entry that made identity type, size, ctime and birth time, under which a file
  edited after its folder was listed was refused by Delete and by every action on a search
  result until the folder was read again: Arbor watches nothing, so that would block users all
  the time). `fs::Ident` is now the object: device and inode (Windows: creation time), type,
  and birth time where the file system keeps one (Linux statx btime, macOS `st_birthtime`).
  Every action on chosen items compares it: Delete, Empty Trash (plus Linux's file handle),
  trash, move, copy, rename, F5/F6, drag, Put Back, batch rename's search results, and the
  worker's "replaced while it ran". An edit doesn't change it; a replacement that got the freed
  inode back does (new birth time). `ops::Print` (identity, type, size, mtime) stays the
  content fingerprint for undo, redo, batch rename and a cancelled copy's cleanup, unchanged.
  Batch rename still refuses an item edited between its preview and Apply (its rollback shares
  that fingerprint). Limits: without a birth time (ext2/3, some FUSE or network file systems,
  no statx), or a replacement made in the original's clock tick, device, inode and type are all
  there is, so a reused inode reads as the same item; Windows' identity no longer includes a
  file's size and write time, so a replacement within NTFS's 15 s tunnelling isn't seen at all
  (the file index needs every file opened). Undo: `fs::Ident` (add the size and ctime back).
  (Amended the same day, above: the no-birth-time fallback, and Windows' file ids from the
  listing.)

- **2026-11-09 · The inline rename and New Folder are worker operations** (review of milestone
  13: they touched the disk on the window's thread). The list shows the new name at once and
  reads the folder again when the worker answers (which puts it right if the rename was
  refused); undo is as before. They're file operations like the rest, so they wait for one
  that's running: the rename field stays open and says so, New Folder is dimmed. A rename has
  nothing to say when it goes well. Undo: `Op::RenameOne`, `Op::NewFolder`.
- **2026-11-09 · Windows `fs::Ident` stays creation time and type** (review of milestone 13:
  ARCHITECTURE.md called it volume and file index). The file index needs each item opened (a
  call per listed item, on a disk that may be slow), so ARCHITECTURE.md now says what it is,
  and the tunnelling limit, instead.
- **2026-11-09 · Get Info's details are fully asynchronous** (review of milestone 13; replaces
  the milestone 12 call of a 2 ms wait, which macOS CI stretched to 9.5 ms). The key shows
  what the listing knows (name, kind, size, modified) and "…" in the rows still to come;
  the rest lands from a worker in the same rows, so nothing moves. Undo: `retarget_info` in
  `src/info.rs` (a `bounded::read_waiting` again).
- **2026-11-09 · Eject waits for any running file operation** (review of milestone 13). The
  old check compared paths, so an operation reaching the volume through a link, or copying a
  folder that holds it, was missed. Telling it surely would mean resolving every source,
  destination and undo step to its device when the operation is planned (on the window's
  thread, or another worker round trip); one operation runs at a time and most are short, so
  Eject now says "Wait for the file operation to finish (or cancel it with Esc), then eject" and
  is dimmed meanwhile. Undo: `State::eject` in `src/volumes.rs`.
- **2026-11-09 · Review of milestone 12, fixed** (all ten findings; each with a test that
  fails on the old code, except Windows-only paths, which CI compiles but can't exercise).
  Calls made: (1) **Eject identity** is the device number plus the mount source (Linux: the
  mount table's; macOS: `f_mntfromname`), or the volume GUID on Windows, read with the list
  only for volumes that can be ejected (a dead network mount's `stat` could hang the list); a
  volume whose identity can't be read isn't ejected ("changed since; refresh and try again"),
  and a volume row's other menu items (Open, New Tab, Add to Favorites) also act on the volume
  the menu opened on. (2) **Eject during an operation** is refused (not queued) when the
  operation's sources, destination or undo steps are on the volume; searches whose root holds
  the volume stop, as panes on it go home. (3) **Get Info waits 2 ms** for an item's details
  (`info::META_WAIT`), not 0: a local `lstat` lands within it, so moving the selection with the
  panel open doesn't flash a placeholder; a slow disk costs a quarter of the frame at most.
  (4) **Get Info on Windows** checks each folder's type and identity right before reading it
  (no descriptor-relative reads there), which narrows the swap race to that instant.
  (5) **Favorite looks** start from Rune's `on_ready` (the first frame on screen) on at most two
  threads; headless runs count as shown from the start. (6) **Sidebar merging** applies each
  window's operations to the file under `sidebar.lock`; a window shows the merged list (the
  other window's changes) after its own next change is written, not live (that would need
  watching the file). (7) **A backup naming a setting twice is damaged** for Arbor's own
  Restore too, not only `uninstall.sh`, so both restorers agree. Undo: the commits from
  bb1284b to 97bad06.

- **2026-11-09 · Milestone 13: consolidation, with no behaviour change.** Big files became
  folders whose `mod.rs` is the interface (`ops/`, `preview/`, `view/`, `app/`, `jump/`,
  `search/`, `handler/`, `incoming/`, `trash/`, `openwith/`), tests moved out of inline
  modules into `tests.rs` beside them (same test names), and four shared modules took over
  duplicated code: `persist` (atomic rewrites, "say a failure once", exit drains),
  `bounded` (the wait-at-most-a-frame hand-off), `mounts` (mount boundaries), `ffi` (C and
  UTF-16 paths); `fs::ident_at` for the re-check idiom. Moved items got `pub(super)` rather
  than a hand-tuned visibility each. `ARCHITECTURE.md` maps it; README and AGENTS.md point
  there (AGENTS.md: one line). Only visible change: the folder-handler backup's temporary
  file is now named like the others (`<name>.<pid>-<n>.tmp`, was `<name>.arbor-<pid>`).
  Left alone on purpose: Search keeps its device-only boundary (it walks bind mounts and
  stops repeats by identity; `mounts::Fence` would change what it finds); the startup read
  and Jump's load keep their own channels (they're not `Latest` requests); BENCH.md's dated
  sections stay as history; `ops/tests.rs` stays one file. Undo: revert the commits from
  fd1279d to 7ed423d (each stands alone).
- **2026-11-09 · `startup_does_no_history_io_and_a_visit_appends` was a test race, not a bug.**
  The history writer counts an operation as it starts it, so on the M4 the test read the log
  before the append landed. The test now flushes the writer after the count; the code is
  unchanged. Undo: 3785041.
- **2026-11-09 · Windows and macOS are clippy-checked on tabot-dev too**, with
  `cargo clippy --target x86_64-pc-windows-msvc` / `aarch64-apple-darwin` (no linking), under
  `CARGO_FEATURE_PURE=1` so blake3's build script needs no C cross-compiler. CI still builds
  and tests all three. Undo: nothing to undo (a habit, not code).

- **2026-11-09 · Review of milestone 11, fixed** (each with a test that fails on the old code,
  except the Windows pipe's, which runs only on CI's Windows runner): Make again failing halfway
  on Windows puts back the registry as it was just before that attempt and the first backup
  file as it was; the Linux uninstall without any Arbor puts back the saved activation file
  and folder default, refuses a damaged backup before changing anything, and keeps it until all
  is back; redo of an undone folder copy checks everything inside; completion reads the folder
  again on each Tab; the README says what accessibility costs. Calls made: (1) the Windows
  hand-off now waits for a one-byte answer from the running Arbor, at most 1 s once connected
  (the pipe is duplex, and a request ends with an extra NUL; an older Arbor's request, ended by
  closing, still reads). On time out the blocked call is cancelled and the new Arbor opens its
  own window; if the running one was only slow, both may show the folder, which beats a launch
  that hangs. The running one now answers before it sorts folders from files, on another
  thread. (2) A damaged backup makes `uninstall.sh` stop with nothing removed, rather than take
  back only Arbor's own values. The Windows trash race stays accepted (above). Undo: the
  commits after 3e06dd7.

- **2026-11-09 · Scope: Milestone 12 (everyday parity: sidebar favorites, volumes and eject,
  Open With, Get Info)** chosen by the agent after all NORTHSTAR Core features were built, to
  serve milestone 1 (a month without Finder/Explorer). NORTHSTAR's 'Later' list (operation
  queue, folder sizes column, archives, terminal, compare) was not started.
- **2026-11-09 · Sidebar favorites live in `sidebar.toml`, beside `config.toml`.** The settings'
  `[general] places` becomes the default list: it applies until the first change made in Arbor,
  and again once `sidebar.toml` is deleted (a settings reload leaves the user's own list alone).
  A file that can't be read gives the defaults and a note; the next change replaces it (no
  backup of the bad file). Home is written as `~`. ⌘1–9 count favorites first, then Locations.
  Undo: `sidebar::load` in `Booting::start`.
- **2026-11-09 · Add to Sidebar has no default key.** Finder's ⌃⌘T becomes Ctrl+T (New Tab) off
  macOS, so a settings file shared between a Mac and a PC would clash (the template test caught
  it). It's in the palette, the context menu, drag and drop. Undo: give `SidebarAdd` a key in
  `src/commands.rs`.
- **2026-11-09 · A missing favorite is found by one `stat` per favorite per launch,** each on a
  thread of its own, from the first `poll` after the first listing (not by the startup read: the
  window never waits for it). The sidebar has no virtual rows, so "when shown" is "once the
  window is up". A favorite that vanishes later is marked when a click's read finds it missing.
  Undo: `State::look_at_favorites`.
- **2026-11-09 · Locations: which volumes, and when they're read.** Linux: one level under
  `/media/$USER`, `/run/media/$USER`, `/media` and `/mnt`, not kernel file systems; Eject only
  for block devices under `/media` and `/run/media` (udisks' mounts; `/mnt` needs root). macOS:
  `/Volumes` minus links to `/`, leftover folders (same device as `/Volumes`), dot names and
  `com.apple.*`. Windows: drive letters with labels, except network drives (a disconnected one
  can take seconds), on a thread with critical-error dialogs off. Read on a worker from the first
  frame, then on: the sidebar's menu, ⌘R, after an eject, macOS mount notifications, Windows
  `WM_DEVICECHANGE`. Linux refreshes by hand only, as the task said; udisks' D-Bus signals or
  `poll(2)` on `/proc/self/mounts` would do it without polling, later. The volumes left the
  favorites (they used to be appended to the places). Undo: `volumes::list`.
- **2026-11-09 · Windows Eject locks, dismounts and ejects the volume with `DeviceIoControl`,
  not the shell's Eject verb (Review: never run on Windows).** A lock that's refused is exactly
  "in use by another app", which the verb doesn't report. Undo: `volumes::win_eject`.
- **2026-11-09 · Open With: Windows shows its own dialog (`SHOpenWithDialog`); macOS gets a
  native submenu only when the apps are read within a frame of the right-click,** "Open With…"
  (Arbor's list) otherwise, since a native menu can't be filled after it opens (RUNE-GAPS.md);
  Linux's in-window menu has no submenus, so "Open With…" opens the list. Computing the list at
  right-click is the closest to "when the submenu opens". Undo: `State::apps_for_menu`.
- **2026-11-09 · No "Other…" in Open With on macOS and Linux (Review).** Rune's native file
  dialog (`Tx::open_file`, which would be an NSOpenPanel on `/Applications`) is behind its
  `native-file-dialog` feature, which needs the `rfd` crate, not vendored (no new crates). Linux
  has no standard app picker. To add: enable the feature (and vendor `rfd`), then an "Other…"
  row opening `/Applications` filtered to `.app`. Windows' dialog has "Choose another app".
- **2026-11-09 · Linux Open With: any `text/*` file also lists the plain-text apps; MIME
  subclasses and aliases aren't followed otherwise; `Terminal=true` apps are started directly;
  an `Exec` line without a file code gets the files at the end.** Undo: `openwith::linux::list`,
  `xdg::exec_argv`.
- **2026-11-09 · Get Info shares the preview's place** (one or the other), follows the selection
  the same way, and counts apparent sizes (each hard link counts, as Finder's does). Changing
  attributes (hidden, read-only) isn't offered: it couldn't go on the undo stack. Rename is the
  list's own. Undo: `info.rs`, `side_panel` in `src/view.rs`.
- **2026-11-09 · Rows dragged onto the sidebar add their folders in a split window and on Linux
  and Windows; not on macOS unsplit,** where a row drag is a system drag that can't land in its
  own window (RUNE-GAPS.md). Folders dropped from Finder or other apps work everywhere. Undo:
  the `inside` drag in `file_row`.

- **2026-11-09 · Review of milestone 10, fixed** (each with a test that fails on the old code):
  the backup is saved and flushed before any change on all three platforms, Windows writes
  commands before the default selectors and puts a half-made change back from the snapshot; a
  damaged backup (empty, no header, an entry missing) is refused before anything changes;
  several folders requested at once each get a tab (a tab hidden on its way to a folder keeps
  that folder, which also fixes ⌘T right after opening a folder); Linux's install and uninstall
  scripts. Calls made: (1) the Linux bus thread now stays for the session, parked on a channel
  when Arbor isn't the default (it used to end at once), so Make and Restore take effect in
  the running Arbor; (2) uninstall without any Arbor binary edits `mimeapps.list` itself
  (only the `[Default Applications]` line Arbor wrote), decoding the backup with `awk`, and
  removes the activation file only if it's Arbor's. The Windows trash race stays accepted
  (above). Undo: the commits after 9376bf8.

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
- **2026-11-09 · Single instance: macOS and Linux get it from the platform, Windows later**
  (Windows since milestone 11: a named pipe, above).
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
