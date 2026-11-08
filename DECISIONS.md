# Decision log

Decisions made while the owner was away, so nothing blocked on a question. Each one can be
reversed. **Review** marks the ones the owner should look at first. Newest first.

Format: date · decision · why · how to undo.

---

## Waiting for owner review

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
