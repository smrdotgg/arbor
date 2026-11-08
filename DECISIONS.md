# Decision log

Decisions made while the owner was away, so nothing blocked on a question. Each one can be
reversed. **Review** marks the ones the owner should look at first. Newest first.

Format: date · decision · why · how to undo.

---

## Waiting for owner review

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
- **2026-11-08 · When the OS has no thumbnail, the card shows with no note**, for formats it
  should know (PDF, video…): the header already says what it is, and the task asked for no error
  noise. Binary files of other types keep "Binary file: there's no preview for its contents."
  Undo: give the `Thumb::Nothing` arm in `preview::load_with` a note.
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
