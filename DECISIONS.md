# Decision log

Decisions made while the owner was away, so nothing blocked on a question. Each one can be
reversed. **Review** marks the ones the owner should look at first. Newest first.

Format: date · decision · why · how to undo.

---

## Waiting for owner review

- **2026-11-11 · Cold start on Windows and Linux: under 30 ms (owner).** The owner found Arbor
  takes about a second to open on a Windows gaming laptop and set the budget: "less than 30ms…
  the 100ms floor was macOS only." NORTHSTAR's row now says under 30 ms on Windows and Linux.

- **2026-11-11 · feature/columns-status merges without its real-Mac QA (owner's call).** The Mac
  may be off; the owner chose to merge on CI (macOS runner included) plus the GPT-6.1 Sol review
  and run the Codex Mac pass on the next round, together with updating /Applications/Arbor.app.

- **2026-11-11 · List columns, Folders First and the status bar (`feature/columns-status`;
  Review: the file, Folders First's default and toggle, what the status bar drops).**
  - **Columns live in `columns.toml` beside the settings (Review)**, not in `config.toml`:
    Arbor would have to rewrite a file people edit by hand, comments and all. It's written
    whole by a thread of its own (`arbor-columns`, started by the first change, the last of a
    run of changes only, an atomic rewrite, a failure said once, drained at exit), like
    `sidebar.toml`'s writer but without its read-and-merge: the file is one value, so two
    Arbor processes are last-change-wins. A resize is saved when the drag ends. A corrupt file
    gives the defaults and a note; unknown column names (a newer Arbor's) are skipped, widths
    clamped. Undo: `listcols::{load, Writer}`.
  - **One set of columns for every window, tab and folder**, Explorer's "apply to all folders"
    rather than Finder's per-folder view options (those would be per-folder user data to keep
    and look up on every navigation). Undo: `Shared::cols` per window instead.
  - **New columns: Extension, Date Created, Permissions (Windows: Attributes). Date Added:
    skipped**: macOS keeps it as a Spotlight attribute (`kMDItemDateAdded`), a call per item, and
    Windows and Linux have nothing like it. Date Created is the birth time each entry's
    identity already holds (statx's btime, macOS's birthtime, the Windows record's
    CreationTime), read from it, not stored again; Permissions is the `lstat`'s `st_mode` (a
    link's own, as `ls -l` shows it) or the record's attributes, a `u32` that sits in the
    padding beside the flags. No call more per entry and no bigger an entry, so the listing
    pays nothing for them, hidden or shown (a first version stored the birth time too: 16
    bytes more per entry; its cost on a 10k read was within this host's noise, but a test now
    holds `Entry` to its old size). **Extension stays though Kind is the
    extension today** (Kind says "Folder"/"File" too); asked for, off by default, free.
    Its header sorts as Kind does (same order); Permissions doesn't sort; Date Created adds
    "Sort by Date Created" and `sort = "created"`. Undo: `listcols::Field`.
  - **Resizing: the strip before a column's header drags that column's width; Name takes the
    rest** and never goes under 180 pt (what the list kept before); a column won't widen past
    what Name can give, 36–48 pt at least, 480 at most. A pane too narrow drops the least
    needed first (Permissions, Created, Extension, Kind, Modified; Size stays, as before). Not
    by keyboard (optional in the task): Reset Columns puts widths back. Undo: `listcols::{fit,
    dragged}`, `view::list::column_edge`.
  - **Folders First defaults on (Review)**: it's what Arbor always did, and Explorer's and
    most Linux file managers' default; Finder's is off. **The setting is `[general]
    folders_first`**, beside `sort` and `sort_descending` (the task's `sort.folders_first`
    would have been a new `[sort]` section for one key). **The palette's "Turn Off/On Folders
    First" lasts until the window closes**, as ⌘. does for hidden files; nothing is saved.
    Off, a Kind sort puts folders among the kinds as "folder" (Finder's way). Undo:
    `Sort::dirs_first`, `State::toggle_folders_first`.
  - **Status bar: "3 selected · 4.2 MB + 2 folders · 120 items · 84 GB free"**, the selection
    part only with two or more selected (one item's size is on its row). **The free space is
    left out while a note or progress shows (Review)**: the note keeps its room; the counts
    stay as they always did. Free space is one `statvfs`/`GetDiskFreeSpaceExW` on a `Latest`
    worker per folder, asked when the folder's read is asked (so it usually lands before the
    listing and shows in its frame), and again after any file operation, for every pane; never
    on a timer. Not shown in a search's results (they can cross disks). The selection's total
    is worked out once per selection and listing (`Folder::picked_size`), not every frame.
    Undo: `view::status`, `Folder::{ask_free, picked_size}`.
  - **A test fixed, not the app**: `a_drop_on_a_column_refuses_a_replaced_folder` dropped
    right after an operation and waited one frame; the free-space read after the operation can
    land in that frame, and Rune holds a file drop a frame when state changed since the last
    build (`hold_files`), so the drop came a frame later. It now waits for the drop to be taken.
- **2026-11-11 · Compress and Extract Here (`feature/archives`; Review: Open, the folder made,
  the bomb limits, the level).**
  - **No zip crate: Arbor's own ZIP reader and writer and tar reader** (`src/archive/`, about
    1,900 lines without tests: the two formats about 1,000, the safety rules and the writer the
    rest), over `flate2` and `crc32fast`, which the lock file already had through `png` with the
    same features: the lock file gains two edges and no crate, and nothing in the shared `vendor/`
    changed. The `zip` crate brings its own dependency tree (`indexmap`, `memchr`, `thiserror`,
    `displaydoc` even with deflate alone) and features Arbor refuses anyway (encryption, other
    methods); the checks that matter (paths, links, bombs, overlap) would have been Arbor's code
    either way. The subset: stored and deflate, ZIP64, UTF-8 and code page 437 names, data
    descriptors on read, Unix modes and links, extended and NTFS times; tar: ustar, pax, GNU long
    names, gzipped. Undo: swap `zip.rs` for the crate.
  - **Open (Enter, double-click) still hands an archive to the system's app (Review)**; Extract
    Here is its own command (palette, context menu on archives), so Enter means the same for
    every file, and macOS users keep Archive Utility one double-click away. Neither command has a
    default key (none is conventional). Compress's title names what it acts on ("Compress
    “Photos”", "Compress 3 Items"), as Finder's does. Undo: `Cmd::Compress`, `Cmd::Extract`.
  - **The folder made (Review): named after the archive, unless everything in it is inside one
    folder; then that folder is what's made** (Archive Utility's rule), so Arbor's own
    "Photos.zip" round-trips to "Photos", not "Photos/Photos". A lone file still gets a folder
    named after the archive. Undo: `single` in `archive::plan`.
  - **Bomb limits (changed after review): no ratio rule.** Refused before writing anything above
    1,000,000 entries, 256 folders deep, 1 TB, or when the contents wouldn't leave the disk 1 GB
    or 5% of its free space (`archive::room`); overlapping entries always (their names counted,
    after review). Each entry is cut off at its declared size and must reach it with its CRC, a
    `.tar.gz`'s gzip stream is read to its end so its CRC and length are checked, and Esc works
    inside a big entry (a plain tar seeks past data; a gzipped one asks every MB). The first
    version refused contents over 256 MB at more than 200× the archive, which refused real
    archives (logs at 294×, zeros at 1,029×, Arbor's own archive of a sparse image). Undo or
    tighten: `archive::limits`.
  - **An entry whose path leads out refuses the whole archive** (an absolute path, `..`, a drive
    letter, a UNC path, a NUL): a crafted archive isn't partly trusted. `\` separates parts
    everywhere (archives from Windows tools), so `a\..\..\x` is refused on every OS. Damage (CRC,
    truncation) and cancel remove everything made, identity-checked (`ops::unmake`). An empty
    archive says so and makes nothing. Undo: `archive::parts`.
  - **Links (Review; rewritten after review): judged by the target alone, and links that fail
    are left out with a note naming them (up to three), the rest extracted.** The first version
    compared the target's path with other entries' names, which a link renamed when made ("Y 2")
    or a disk's Unicode normalization slips past: the review's archive reached four folders above
    the destination. Now a target must be relative, its `..` parts only first and no more than
    the link's depth (`archive::link_target`), which keeps it inside by itself: its `..`s climb
    real folders Arbor made, and the names after go down, through other links only if those keep
    to the same rule. **Deviation from the review's suggestion:** a target that goes down through
    another link the archive made is allowed, not refused, because that is safe by the rule above
    and refusing it would break macOS frameworks in zipped apps (`Headers → Versions/Current/Headers`
    with `Versions/Current → A`). Making each link, its target's path is walked from the link's
    folder without following anything, and a link this extraction didn't make on the way (another
    app's, put there while it ran) refuses the extraction (`out::walk_target`). Undo:
    `unsafe_links`, `link_target`.
  - **Names without the UTF-8 flag stay code page 437** (the ZIP spec's), unless they're valid
    UTF-8 (which many tools write without the flag). Keeping raw bytes on Unix would give names
    no other app shows the same way, and Windows can't; a review nit, left as is. Undo:
    `archive::decode_cp437`.
  - **A ZIP of 1,000,000 entries is listed whole in memory** (about 200 bytes an entry, so
    ~200 MB at the cap): the plan needs every entry before writing anything (the one-folder rule,
    the duplicate-free names, the total). Streaming the directory is for later if it matters; a
    review nit, left as is. Undo: lower `limits::ENTRIES`.
  - **Deflate level 3** for Compress. A 1 GB folder of mixed files on tabot-dev (BENCH.md),
    in CPU time on the worker's thread (the box's load was 6–11, which made wall-clock times
    swing 2–3×): level 1 compresses at 171 MB/s to 70% of its size, level 3 at 84–100 MB/s to
    66%, level 6 at 72 MB/s to 65%. Level 6 saves 1% more for a quarter more time; level 1 is
    twice as fast and its deflated text a fifth bigger. Files already compressed are stored by
    extension, and a big file whose first 256 KB don't deflate below 97% is stored without
    deflating the rest. Compressing files on several threads would be the next step if it's
    wanted. Undo: `LEVEL` in `ops/archive/mod.rs`.
  - **Links: stored as links on macOS and Linux, left out on Windows both ways** (making one
    needs Developer Mode or an administrator), with a note. **Permissions: a file keeps its
    owner's read, write and run bits from the archive (read always), the rest from the umask;
    never setuid, setgid or sticky; folders get the default mode.** Undo: `out::file_mode`,
    `plan`'s Windows branch.
  - **Undo trashes the archive or folder only while it's unchanged, and redo takes it back out of
    the Trash** (the task said "recreates": taking it back is the same result, and is what an
    undone copy's redo does). Undo: `What::Compress`, `What::Extract` in `Undo::in_trash`.
  - **Compress stops on a file it can't read** (Finder does too; an archive quietly missing a
    file is worse) but leaves out, with a note, what it never archives: another disk mounted
    inside, devices, sockets, FIFOs, links on Windows. Its temporary file is
    `.<name>.arbor-<pid>-<n>.part` beside the items (shown on Windows while it runs). Undo:
    `ops/archive/mod.rs`.
  - **A `.tar.gz`'s gzip stream (after the re-review):** read member by member, each member's
    CRC and length checked; it ends with the member holding the tar's end (nothing after it is
    decompressed or looked at, so trailing zero padding, which gzip, tar and Python accept, and
    even a damaged extra member, don't refuse it); zero bytes after the last member while the
    tar still goes on end the stream, and data after zero padding is damage. A tar's running
    total is held to the disk's room while it's listed. Undo: `archive::tar::Gz`, `list_from`'s
    `free`.
  - **With multiple windows (main's aeeed9e):** Compress and Extract Here start through the
    app's one operation slot like every operation, and their outcome refreshes every window
    showing the folder, by identity. What they make is registered for the leftovers record the
    moment it's made (the extracted folder and each item in it, Compress's temporary file), so
    quitting mid-way names what cleanup couldn't remove; a finished one is cleared. Undo and redo
    say "Moved “Photos.zip” to the Trash", "Put back “Photos” from the Trash".
  - **Tar: read twice** (once for the list, so it's checked whole before anything is written,
    then to write it, each entry matched against the list; a gzipped tar is decompressed twice),
    hard links, devices and sparse files left out with a note, names that aren't UTF-8 shown with
    replacement characters (noted). **`__MACOSX/` is left out silently** (macOS's resource forks,
    which Archive Utility folds back into the files). Undo: `archive::tar`, the `__MACOSX` check
    in `plan`.

- **2026-11-11 · Some reviews go to an Opus reviewer instead of GPT-6.1 Sol (Review).** OpenAI's
  safety filter refused two re-reviews of the Make Link undo fix (checks that a symlink wasn't
  swapped just before removal), even reworded in plain terms. Rather than reword further to get
  past a safety filter, the PM gave that review to an independent Claude Opus 5.5 reviewer with
  the same scope. Sol stays the default reviewer. Undo: none needed; it's a process choice.
- **2026-11-11 · New File, Make Symbolic Link and Show Original (`feature/new-file-links`;
  Review: the link's name and target, the keys).**
  - **Keys (Review): New File ⌥⌘N (Ctrl+Alt+N off macOS)**, beside New Folder's ⇧⌘N; free on
    all three (the registry test checks), and ⌘N stays for New Window. **Make Symbolic Link: no
    default key.** Finder's Make Alias is ⌃⌘A, but `cmd+ctrl+a` in a settings file written on a
    Mac reads as Ctrl+A (Select All) on Windows and Linux, where ⌘ is Ctrl (macOS CI's template
    test caught it); Explorer and Linux file managers have no conventional key. Bind
    `file.make_link` in the settings file. **Show Original: no key** (Finder's ⌘R is Reload here).
    Undo: `NewFile`, `MakeLink`, `ShowOriginal` in `commands.rs`.
  - **A link points at the item's absolute path as listed (Review)**, not a relative one and not
    resolved: a link made beside its item is for moving elsewhere (Finder's aliases work that
    way); a relative target would survive moving the pair together instead, and break when the
    link moves alone. Undo: `Link::target` in `ops::links::Worker::make_link` (a relative target
    would be the item's name).
  - **The link's name puts " link" before a file's extension (Review): `photo link.jpg`, not
    `photo.jpg link`** as the task said: the link keeps its item's type for Arbor's icons and
    preview, Open With, and Windows, which opens by extension. Folders and names without an
    extension get "name link"; taken names count up ("photo link 2.jpg"). Windows' shortcut adds
    `.lnk`. Undo: `ops::links::link_name`.
  - **Undo of Make Link removes the links permanently, not to the Trash; redo makes them again.**
    A link holds no data, so removing one that's still exactly the link Arbor made loses
    nothing. "Exactly" (after review): its fingerprint, its object identity (`fs::Ident`, with
    the birth time or Linux's handle, recorded when it was made: a `cp -a` copy put back at the
    freed inode has the same inode, times and target) and its target, checked once with the
    others and again at the removal itself. On Unix every call looks the name up again (even
    `statx`, `readlinkat` and `unlinkat` relative to the folder), so the link is first moved
    aside with the no-replace rename to a fresh hidden name in its folder, checked there, and
    removed there; anything else found there goes back to its name (no-replace), or, when the
    name was taken meanwhile, stays under the hidden name and the status bar says which (after
    the second review: a swap at the instant of `unlinkat` deleted the newcomer). On Windows,
    through a handle on the link that shares no delete access, deleted through that handle.
    New File's undo checks the same identity when it checks and again right before the trash;
    the trash itself goes by path, and isn't moved aside first: the Trash would then record the
    hidden name, and Put Back or redo would restore the file under it. Removing also means redo works on Windows, where the Recycle Bin doesn't say where it
    put an item. New File's undo trashes the file, as New Folder's does (the task's rule). Undo:
    `Worker::undo_links`, `links::remove_if_still`.
  - **Windows: `std`'s symbolic-link calls** (they pass
    `SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE` to `CreateSymbolicLinkW`); on
    `ERROR_PRIVILEGE_NOT_HELD`, and on `ERROR_INVALID_FUNCTION`/`ERROR_NOT_SUPPORTED` (FAT and
    exFAT drives hold no links), a shell shortcut through `IShellLinkW` and `IPersistFile`, saved
    to a fresh `create_new` name beside it and then given its name with the no-replace rename
    (`IPersistFile::Save` replaces what's at its path). The status bar says "Made as shortcuts
    (.lnk): …". No new windows-sys features. Undo: `ops::links::shortcut_fallback`.
  - **A folder's identity (`noclobber::DirId`) has its birth time, or on Linux without one the
    file system's handle** (after review): device and inode alone took a folder removed and
    made again at the freed inode (ext4) for the one listed, so New File, New Folder, a paste
    and a drop went into it. Never the ctime, which a folder's every new entry changes. Every
    "same folder?" check uses it. Undo: `noclobber::dir_id`.
  - **Undo and redo say what they did, per kind** (real-Mac QA: undoing New File or Make Link
    said "Restored 1 item"): "Removed 1 link" / "Made 1 link again"; New File, New Folder and
    copies "Moved “untitled.txt” to the Trash" / "Made “untitled.txt” again" (not the QA's
    suggested "Removed “untitled.txt”": the file is in the Trash, not gone, and saying so tells
    the user where to find it); moves and renames "Moved/Renamed … back" / "… again"; a trash
    "Put back … from the Trash" / "Moved … to the Trash again". One item is named, more are
    counted. Undo: `ops::undone_note`.
  - **In the column view, New File and New Folder go into the column holding the selection**
    (the active column: the parent of a selected folder, whose contents are open in the next
    column), as Finder does; into that folder only once its column is made active (→, or a
    click in it). The real-Mac QA noted it; it's the column view's rule that every action acts
    on the active column. Undo: `State::here` would take the selected folder instead.
  - **New File and New Folder clear a filter, and rename only what they made** (after review):
    a filter hiding "untitled.txt" left another row selected, and the rename opened on it. The
    active pane's filter (or go-to text) is cleared when the item is made, as Finder shows a new
    item whatever the search, and the rename starts only when the selected row is the item made,
    by name and identity; otherwise none starts. Undo: `State::rename_made`.
  - **New Folder now checks the folder's identity too**, as New File does: one put at the
    folder's path since it was listed makes nothing ("Nothing was made: … is another folder
    now"). A folder whose identity isn't known yet (still being read) is used as before. Undo:
    `State::here` in `actions.rs`.
  - **Show Original selects the original in the same pane** (in place when it's in the same
    folder, else navigating there, ⌘[ back), not in a new tab: it's Reveal for links. The link is
    read on the go-to worker with the usual one-frame wait; a target is used as written unless it
    has `.` or `..`, which are resolved through the real folders on Unix (as the kernel follows
    them) and by spelling on Windows. Symbolic links only,
    not `.lnk` shortcuts. It's in an item's context menu only on a link.
    Nothing on screen may hide the original (after review): the pane's filter is cleared, and a
    hidden original turns hidden files on (for every pane, as ⌘. does), saying so. Undo:
    `State::show_original`, `app::original`.

- **2026-11-11 · Multiple windows (`feature/new-window`): the choices (Review: the shared undo
  stack, ⌘W, where a new window opens).**
  - **Real OS windows in one process**, through Rune's `open_window` (each window its own
    `State`, retained tree and focus; one event loop). No RUNE-GAPS workaround was needed for the
    windows themselves. Undo: `window.rs`, `app/shared.rs`.
  - **⌘N (Ctrl+N) opens the new window on the active pane's folder**, not Home: what Explorer,
    Nautilus and Dolphin do, and what makes a second window useful at once (drag between them).
    Finder opens its "New Finder windows show" folder (Recents by default). Undo: the `NewWindow`
    arm in `app/dispatch.rs` (`fs::home()` instead of `cwd`). ⌘N/Ctrl+N was free on every
    platform (New Folder is ⇧⌘N, as in Finder); Ctrl+N in a text field isn't Arbor's (the filter
    takes only typed text).
  - **"Open in New Window" opens one selected folder**, with no default key (Finder's ⌥⌘O-style
    keys differ by version; the palette and context menus reach it). With several folders
    selected it says "Select one folder first" rather than opening a window per folder (a 500-item
    selection would open 500). It's also on a favorite's and a volume's menu. Undo:
    `State::folder_to_open`.
  - **One file-operation slot and one undo/redo stack for the app (Review)**, as Finder has:
    ⌘Z in window B undoes what A did. Per-window stacks would let B's ⌘Z skip A's newer change,
    or leave A's change undoable only from A after A's folder is gone. The window that started an
    operation shows its progress and is the one where Esc cancels it; the others say "Another file
    operation is running in another window" (and Cancel there says to cancel it in that window).
    The operation is held by `Shared`, not a window: a window closed mid-operation makes the
    window focused last its owner at once, which lands it, its note and its undo, and a window
    whose operation's owner is gone lands it whoever polls first; the very last window closing
    drops it as quitting did before. Undo: `Shared::{job, undo, redo}`, `State::{busy,
    poll_job, leave}`.
  - **An undone operation's note goes from every window still saying it** (real-Mac QA): each
    outcome has a number, its undo record carries it, and an undo or redo of it tells the
    other windows; a window whose note is still that outcome's (not one said since) clears it,
    rather than rewording it (the window that undid it says what the undo did). Undo:
    `State::outcome_undone`, `Shared::outcomes`.
  - **Closing the last window (or quitting) while an operation runs (PM decision,
    2026-11-11):** the close never waits for it. The operation is cancelled and the window
    closes at once; its usual cancel cleanup (a cancelled copy removing what it made,
    identity-checked) goes on off the UI thread for at most 2 s (`leftovers::QUIT_WAIT`), which
    `main` waits out after Rune's loop ends, when no window is on screen. Nothing it couldn't
    finish is lost: a record of what was left where and why (`leftovers.txt` beside the
    settings, user data) is written provisionally at the close (the copy being made) and made
    exact when the cleanup ends or its time runs out; the next launch says it once in the
    status bar ("Arbor quit during a file operation and left “Photos” partly done in
    ~/Downloads: …", only items still there), with a Show button; only once that frame has
    been presented (or the user typed or started an operation) is what was read removed from the
    record, so a crash before then keeps it for the launch after. The
    record names only what the copy confirmed it made, registered the moment each item is
    created (a file by the handle it's written through, before any byte; a folder or link right
    after it's made), each with its creation mark (kind, file id, birth time where the file
    system keeps one; not its size, which a copy cut short leaves anywhere), and the next launch
    offers only what still has that mark: never a planned destination, never something put in its place since. Past
    the 2 s deadline the record is written from what's in memory (no look at the disk), and
    `main` waits for that write at most 250 ms more (`leftovers::WRITE_MARGIN`): Arbor exits
    within 2.25 s of the close even on a disk that doesn't answer, losing the record then.
    Keeping Arbor running with no window until the operation ends was the alternative: a
    process the user closed doing file work they can't see or cancel, with no undo afterwards.
    The close callback does no record I/O: the quit thread writes the record (provisional,
    then exact). On macOS, Quit (⌘Q) ends the process right after the windows' close callbacks
    (RUNE-GAPS.md), so there even the provisional record may be lost (accepted, PM
    2026-11-11). The callback's existing bounded drain of the Jump history's and the sidebar's
    writers stays as it was before multiple windows. An operation nobody holds is cancelled (`Drop
    for ops::Running`). Undo: `leftovers.rs`, `State::leave`'s last-window arm,
    `window::closing`.
  - **After an operation, every window showing a changed folder reads it again** (by identity,
    `State::refresh`, as panes do); the starting window selects what was made, the others keep
    their selection. A window on another folder reads nothing.
  - **Settings, keys, favorites and volumes are each window's copy, kept in step by messages**
    (Reload Settings in one window applies in all and is said only there; a favorite added in one
    shows in all; one window reads the volumes and tells the others). The clipboard's files and
    the Jump history are one object. The favorites have one writer for every window and a
    revision number: each edit takes the next one, and the writer's answer or a window's message
    about an older revision than the newest is dropped, so a slow answer never puts back a list
    without a newer favorite. A window dragging a favorite holds a newer list until the drag
    ends, then makes the drag's move on it as an operation (moved last means to the end, as the
    writer merges it); a message always carries the revision of the list it holds. Shared writers wake the open window focused last
    (`Shared::live_waker`), never one that has closed. Undo: `State::tell_favorites`,
    `Shared::{side_writer, side_rev}`, the settings arm in `State::poll`, `poll_volumes`.
  - **⌘W on a window's last tab closes the window while another window is open**; on the very
    last window it still does nothing (closing it would quit Arbor). Undo: the `CloseTab` arms in
    `app/dispatch.rs`.
  - **Requests from other apps go to the window focused last** (the first window before any was
    focused). "A new window if there's none" can't arise: Arbor quits with its last window, so a
    request with no window open is dropped with the quitting app. Undo: `window::route`.
  - **A new window copies what was already known**: the opener's settings, keys, favorites and
    what their looks found, volumes, the hidden-files toggle and the icon size; its view starts
    as the settings say (as a new tab's does). It reads only its folder, fresh (nothing of the
    opener's listing is reused: no caching), and ⌘N doesn't wait for it, unlike showing a tab:
    the native window takes longer than a frame to make anyway, so the read overlaps it and
    lands in the new window's `setup` or first `poll`, before its first frame in every bench run
    (⌘N's handler went from 8.2 ms, the bounded wait a 10k folder missed, to 0.2 ms). It opens 28 pt down and right of its opener at the same size (Wayland: where the
    compositor puts it). Its title comes from its first `poll` (Rune's window title starts as a
    `&'static str`). Undo: `State::new_window`, `window::open`.
  - **Views, thumbnails and columns are each window's**; another window's operation re-reads
    every column showing a changed folder (`refresh` → `Columns::reread`, by identity).
  - **Drops between windows**: Rune's in-window drag is window-local, so Arbor's own row and tab
    drags can't cross windows. Rows dragged out to the system (macOS) land in another window as a
    drop from outside, through `State::drop_into`, so its destination-identity check applies;
    the items themselves carry no listing identity then, as from any other app. Elsewhere: ⌘X, ⌘V.
  - **With New File and Make Symbolic Link (from main, e39f626):** they run through the shared
    operation slot and undo stack like every operation; every window on the folder re-reads it
    and an undo clears the note in all. The inline rename of what New File or New Folder made
    starts only in the window that asked (not another window on the folder, nor one that landed
    it because the asking window closed: `Job::asked`). The rename's Esc and Enter are each
    window's. New File and symbolic links are made whole in one call, so a quit leaves nothing
    partial; a Windows shortcut's staging file is registered while it exists, so a quit
    mid-write names it in the leftovers record.
  - **Windows aren't remembered across launches** (later: a window list in the user's data).
  - **Timing and the bench**: only the first window's keys are timed; `ARBOR_BENCH_WINDOW` and
    `bench --window N` time ⌘N to the new window's first frame showing a 10k folder; `bench
    --idle` adds a run idle with two windows. Undo: `timing.rs`, `examples/bench.rs`.

- **2026-11-11 · The idle list view's endless frames (since 7cd20cb): the rows hear their
  layout only while a selection rectangle is held, and never say `Tx::live_only` there.**
  Rune doesn't clear what a `live_only` event leaves behind in a window with no live values
  (RUNE-GAPS.md), so a handler that said it on every idle scroll, resize or view switch kept the
  list view drawing every frame. Outside a rectangle drag nothing listens; during one, a report
  that changed nothing commits a cheap view. `bench` now ends with an idle check (`--idle 1` by
  default, about 15 s; `--idle 0` skips it) that fails above 0.05 s of CPU in 5 s. Undo:
  `list::follow_band` and `Ctx::banding` (`src/view/`), `--idle`'s default in
  `examples/bench.rs`.
- **2026-11-11 · More views: As Columns and As Tiles (Review: the root, the keys, the
  preview).**
  - **Keys (Review): ⌥⌘3 As Columns, ⌥⌘4 As Tiles** (Ctrl+Alt+3/4 off macOS), next to ⌥⌘1/⌥⌘2;
    Finder's ⌘3/⌘4 stay the sidebar's places, as ⌘1/⌘2 did. No default-key conflict (the
    registry test checks). `view.default` takes "columns" and "tiles". Undo: `AsColumns`,
    `AsTiles` in `commands.rs`.
  - **The columns' root (Review): the deepest sidebar place the folder is in (a favorite or a
    volume), else its volume's root** (`/`, `C:\`), and it **stays while the folder is under it**
    (a folder outside picks a new one). From the volume root, a folder in ~/Projects/arbor/src
    would be six columns; from the place, Home's three. ← at the root column goes to the parent
    anyway (the root moves up), where Finder stops. Undo: `columns::root_for`.
  - **The active column is the tab's `Folder`; the other columns are plain listings.** So every
    action resolves against the active column's folder with no code of its own, and a press,
    right-click or drag in another column first makes it active from the listing on screen, in
    the same frame and without a read (`State::adopt_column`), then acts. ←, →, Enter, ⌘↑/⌘↓,
    Back and a crumb all do that when the folder is a column on screen. Each move is a
    navigation (⌘[ goes back through them), and → selects the new column's first item, as
    Finder does. Undo: the `adopt_column` call in `State::go_full`.
  - **The folder just left becomes a column in the same frame from its own listing** (on the
    right after ←, on the left after →), never kept past that sync (`Columns::spare`); I read
    it as "on screen, moved", not a cache. A press in a column whose listing isn't in makes it
    the active folder at once, empty and reading, rather than leaving the folder before (and its
    selection) as what actions act on until it lands (after review). A file operation that changes a column's folder, and
    ⌘R, read it again, its rows staying until the new listing lands. Undo: the `spare` push in
    `State::land`.
  - **Columns scroll by whole columns, the deepest after every move or resize; the wheel
    scrolls back only as far as the active column stays on screen** (RUNE-GAPS: Rune's
    horizontal scroll can't say which children are on screen, which "no entries off screen"
    needs). One width for every column, 220 pt, its dividers drag it (140 to 520, this
    session). Undo: `Columns::bounds`, `COL_W`.
  - **A column waits at most 4 ms for its read (`columns::WAIT`), shared by every column one
    change needs**, not a full frame: ↑↓ through big folders keeps the key within a frame; a
    slower read lands through `poll`, the column empty until then (no "Loading…" flash).
    Undo: `WAIT`.
  - **The preview (Review): the selected file's preview is the last column, opened and closed
    with the selection** (Finder's column view), the quick preview's own panel and worker and
    rules (on demand: the user chose this view). Space turns it off and on for the session; Esc
    doesn't close it. A docked preview open when the tab switches moves into the columns. In a
    split window only the active pane's columns have it. Undo: `State::sync_column_preview`.
  - **Rows in a column show the name alone** (with the ▸ for a folder), the active column too;
    a search in the column view shows its results as a list. Undo: `Ctx::narrow`.
  - **A column move that only moves listings already on screen is a key: 8 ms; one that reads
    a folder is a listing: 50 ms** (corrected after review: the first version held every move
    to 50 ms, and ← and → over 10k took 8.4 and 8.9 ms of CPU). Each column is one element
    keyed by its number, active or not, and a listing moves between a column and the folder
    with its rows and their version (`folder::Adopted`, `Column::ready`), so Rune keys nothing
    and the rows aren't sorted, filtered or copied again: ← and → over 10k now take 1.8 and
    2.3 ms of CPU (`column_cost`). The bench tells the two kinds apart by whether the key started
    a read (`reads=` on a `key` line) and waits for a column's read to land before the next key.
    Undo: the budgets in `examples/bench.rs`.
  - **Idle, all four views: zero frames.** The redraw found here on Xvfb after As Icons, As List
    or → (main too) was the rows' `live_only` layout handler, fixed on main (9e4f168, above).
    Its idle test and `bench --idle` now cover the tiles and column views too, a column move
    included: 0.00 s of CPU in 5 s for each view on tabot-dev.
  - **Every drop is checked against the folder the user saw (second review).** One path
    (`State::drop_into`, an `ops::Dest`): a folder row carries the identity its listing read
    (`fs::Ident`, not following a link), a background (list, grid, column) or the other pane the
    folder's own (`FileId`), and the worker refuses either if what's at the path differs, as a
    paste does. A destination whose identity isn't known (a folder not read yet, a row whose
    identity couldn't be read) takes nothing ("Nothing was dropped: … isn't read yet"), rather
    than going ahead unchecked. The sidebar's favorites take no file drops (dropping folders on
    the sidebar adds them as favorites), so there's nothing to check there. Undo: `drop_into`'s
    `None` arm.
  - **The column view drags rows in the window, never the system's way (second review)**, as
    split view does: a file dragged onto a folder row or another column moves there; on macOS a
    row can't then be dragged out to Finder from the column view (README, RUNE-GAPS). Undo: the
    `None if c.narrow` arm in `view/list.rs::item_drags`, `column_drags`.
  - **Tiles: one size** (48 pt icon in 248 × 72 pt tiles, widened to fill the row), the kind in
    words then the extension ("Image · PNG"), then size and date; ⌘+/⌘- stay the icon view's.
    It's the icon view's machinery (virtual rows, thumbnails at 48 pt, 2-D arrows, the
    rectangle) with another cell. Undo: `Geom::tiles`, `tile_kind` in `view/grid.rs`.

- **2026-11-10 · Milestone 16 review fixes: three judgment calls.**
  - **An emptied Go To field stays open** (real-Mac QA: Backspace held to clear ⌘L's path went
    on to the parent folder, up to `/`). Once Backspace empties it, the status bar still shows
    "Go to" with nothing typed, and Backspace and Enter do nothing until Esc closes it or typing
    carries on (a letter makes it a filter). Enter is swallowed too: in an empty go-to field it
    used to open the selected item. Undo: `Folder::goto`, its check in `app/dispatch.rs::key`.
  - **Thumbnails follow "this grid is on screen", checked after every input and landing**
    (`State::sync_tiles`, from `sync_preview` and `poll`), not at a list of transitions: any way
    a grid leaves the screen (another tab, the list view, an empty filter, the preview covering
    its pane, another folder) drops them, including ways added later. A grid whose listing or
    icon size changed is asked again from the rows Rune last reported. Undo: `sync_tiles`.
  - **A rectangle alone sets the old selection aside rather than copying it**: Esc puts it back,
    and a finished rectangle frees it on release, one pass over it (as a plain click that
    deselects 500,000 items does), never per move. Undo: `Band::cleared`.

- **2026-11-10 · A trash folder whose listing fails partway is left out of Empty Trash whole**
  (review: the dialog said "what's in it stays" of a trash it had partly listed, and the
  partial list was emptied). Of the two ways to make it true (word it as "items Arbor couldn't
  list in X stay", or leave X out), leaving it out is the safer: nothing from a folder Arbor
  couldn't read to the end is deleted, so a disk failing midway never loses part of its trash
  on a half-read list. The count, size and what's emptied are only the trashes read whole;
  the dialog names the one left out, as for a trash it couldn't open. Undo: `whole` in
  `trash::contents::list` (both versions).

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
- **2026-11-11 · A row's system drag left behind by its release is given the release again**
  (Mac QA: after a quick single-pane drag nothing took, keys did nothing until Esc). The
  real-Mac `ARBOR_DRAG_LOG` runs showed the cause: the release came before AppKit's session took
  over the mouse, so it went to the window, and the session waited for another (RUNE-GAPS.md has
  the timings). Folder rows had nothing to do with it. When Rune reports the session started,
  Arbor asks the window server whether the left button is still down, and if not posts a
  left-mouse-up at the pointer to its own event queue (in-process, no permission); the session
  ends Cancelled and the status bar says how to move files. 7ae015b's check (refusing a drag
  offered while the event being handled was a release) is reverted: Rune already refuses that,
  and the event at offer time can't see a release that comes later. Nothing is checked at offer
  time: Rune has no hook between the offer and the session, and the offer is where synthetic
  input read "no button down" mid-drag. Judgment call: with synthetic input that never sets the
  button state, a drag still held when the session starts is ended at once (Cancelled, the same
  hint); a real mouse reports it. Undo: `State::drag_out_started` and the `on_drag_start` in
  `view/list.rs::item_drags`.
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
