# Rune gaps

Arbor uses Rune exactly as it is (pinned to the commit in `RUNE_REV`). We don't patch it. When a feature ends
up worse than it should be because Rune is missing something, it gets an entry here. The owner
reviews this file with the Rune team at Anomaly.

Each entry:

- **Feature:** the Arbor feature that hit the gap
- **What Rune is missing:** the API or behaviour we needed
- **What we did instead:** the workaround, with file references
- **What users lose:** how much better the experience would be with it (be concrete)
- **Suggested Rune change:** what we'd ask for, as small as possible

Newest first.

---

## Drag-select: no pointer events while the pointer holds still outside the list

- **Feature:** drag-select (milestone 16): a selection rectangle dragged past the list's top or
  bottom edge scrolls the list, as Finder's and Explorer's do, for as long as the pointer stays
  out there.
- **What Rune is missing:** a way to keep scrolling while a drag holds still past a viewport's
  edge without a timer: an auto-scroll on drags (as AppKit's `autoscroll:` and Windows'
  drag-scroll do, driven by the system's own repeating drag events), or `Phase::Move` repeated
  while a captured press is held outside its element. Rune sends `on_drag` only when the pointer
  moves, and Arbor runs no timers (NORTHSTAR: no polling).
- **What we did instead:** each pointer move past the edge scrolls one step, further the further
  out the pointer is (half the distance, 2 to 48 pt; `Band::edge_step` in `src/band.rs`), with
  `Tx::scroll_to_instant`, and the rectangle and its hits follow from geometry.
- **What users lose:** holding the pointer still below the list stops the scrolling; the user
  wiggles it (or uses the wheel, which scrolls the list under the rectangle) to go on.
- **Suggested Rune change:** an opt-in edge auto-scroll for `on_drag` inside a scroll viewport
  (`El::drag_autoscroll(true)`), driven by the platform's drag events where it repeats them
  (macOS sends periodic `mouseDragged` events during `autoscroll:`), or a repeating
  `Phase::Move` at the display's rate only while a captured press is outside its viewport.

## Wheel: Shift + wheel isn't turned on macOS when the system didn't turn it

- **Feature:** scrolling the text preview's long lines sideways (Quick preview, NORTHSTAR Core).
- **What Rune is missing:** Shift + a mouse wheel turned into a horizontal scroll on macOS too.
  Rune turns it only off macOS (`SHIFT_TURNS_WHEEL` in `runtime/scroll/x.rs`), because macOS turns
  a real mouse's wheel before the app sees it. It doesn't turn a wheel event that something else
  posted (a mouse utility re-posting the wheel, Screen Sharing, an automation tool such as the
  QA session's): that arrives as a vertical delta with Shift held, and Rune scrolls down. Rune
  does deliver horizontal deltas (a tilt wheel, a trackpad swipe, with pixel deltas and phases)
  to `scroll_xy` viewports correctly; Arbor's tests feed both.
- **What we did instead:** the preview's text has an `on_wheel` that takes an unturned
  Shift + wheel delta (not precise, no horizontal part) and scrolls sideways itself with
  `Tx::scroll_to_instant`, adding up several notches in one frame (`preview_shift_wheel` in
  `src/view/panels.rs`). Off macOS Rune has already turned it, so the handler leaves it alone.
- **What users lose:** within one wheel gesture (0.1 s without phases), letting go of Shift while
  still turning drops those notches instead of scrolling down; and every other `scroll_x`/
  `scroll_xy` viewport would need the same handler (the preview is Arbor's only one).
- **Suggested Rune change:** on macOS, turn a non-precise delta with Shift held and no horizontal
  part as other platforms do (a delta the system turned already has one, so nothing turns twice).

## Drag out: a release past the threshold starts a system drag that never ends

- **Feature:** dragging rows out of the list (macOS, one pane): to Finder and other apps, and the
  guidance when nothing takes the drop.
- **What Rune is missing:** a guarantee that `drag_out` never starts a session once the button is
  up. Rune means to (`Runtime::release`: "A release never starts a drag-out"), but winit's
  `mouseUp:` sends the release's position as a `CursorMoved` before the `MouseInput` release
  (`view.rs`: `mouse_motion` then `mouse_click`), and that move goes through `pointer_move`, which
  starts the drag-out when it crosses the threshold (`window/events.rs` then calls
  `drag_out::begin` with the mouse-up as the current event). AppKit's session then waits for a
  release that already happened: it eats every key (⌘J, F2, anything) until Esc cancels it, and
  `on_drag_end` doesn't run until then. It happens when the press moved less than the threshold
  in its last drag event and the rest arrived with the release: a quick flick, Screen Sharing, or
  an automation tool's drag (the QA session's "keys blocked after a single-pane drag until Esc").
- **What we did instead:** the row's `drag_out` closure asks AppKit for the event being handled
  (`NSApp.currentEvent`) and offers nothing when it's a release or a plain move
  (`os_drag_can_start` in `src/actions.rs`), marking `State::drag_refused`; the next frame's
  `poll` (or the next key) says how to move files, as a drag nothing took does. The press then
  ends as a press that moved off its row: nothing happens.
- **What users lose:** such a flick starts no drag at all, where Finder would still drag. Arbor
  calls into AppKit from a view closure, which every Rune app with `drag_out` would need.
- **Suggested Rune change:** don't start a drag-out from a move that comes with a release: in the
  winit shell, skip `take_drag_out` (and drop the request as `Cancelled`) when
  `NSApp.currentEvent` isn't a mouse-dragged event, or have `pointer_move` check that the button
  is still down (`NSEvent.pressedMouseButtons`). Better still, also pass where a session ended to
  `on_drag_end` (see "Drag between panes" below).

## Volumes: no mount or device notifications

- **Feature:** the sidebar's Locations (milestone 12), which should show a USB stick when it's
  plugged in and drop it when it's pulled, as Finder's and Explorer's do, without polling.
- **What Rune is missing:** a way to hear that volumes changed: macOS's NSWorkspace
  `didMount`/`didUnmount` notifications, Windows' `WM_DEVICECHANGE` (sent to top-level windows;
  Rune's window procedure doesn't pass it on), and on Linux udisks' D-Bus signals.
- **What we did instead:** on macOS Arbor registers its own observer with NSWorkspace's
  notification center (`volumes::mac_watch`); on Windows it makes a hidden top-level window of
  its own on a thread that sleeps in `GetMessage` (`volumes::win_watch`). Each refreshes the list
  on a worker. Linux refreshes by hand (a sidebar right-click, ⌘R).
- **What users lose:** on Linux, a stick plugged in shows only after a right-click or ⌘R. On
  Windows, a thread and a hidden window for something the app's own window already receives.
- **Suggested Rune change:** `App::on_volumes_changed(|s, tx|)`, from NSWorkspace on macOS,
  `WM_DEVICECHANGE` on Windows, and udisks' `InterfacesAdded`/`InterfacesRemoved` (or
  `/proc/self/mounts` becoming readable with `POLLPRI`) on Linux.

## Native menus: a submenu can't be filled when it opens

- **Feature:** Open With's submenu in the context menu (milestone 12): the apps that open the
  file, which take Launch Services a few milliseconds to list (NORTHSTAR: pay only for what
  you use).
- **What Rune is missing:** a callback when a submenu is about to open (AppKit's
  `menuNeedsUpdate:` / `menu:updateItem:…`, Windows' `WM_INITMENUPOPUP`), or a way to change an
  open menu. `PopupMenu` is fixed when `Tx::popup_menu` is called.
- **What we did instead:** the right-click on a file asks a worker for the apps and waits at
  most a frame (`State::apps_for_menu` in `src/openwith.rs`); if they're in, the menu has an
  Open With submenu, otherwise an "Open With…" item that opens Arbor's own list, which fills in
  when the worker answers.
- **What users lose:** every right-click on a file lists the apps whether or not the submenu is
  opened (a few ms on a worker), and on a slow Launch Services the submenu becomes a separate
  list.
- **Suggested Rune change:** `PopupMenu::lazy_submenu(title, id)` with
  `Tx::popup_menu`'s callback getting a "fill `id`" request (answered with a `PopupMenu`), or an
  `App::on_menu_will_open`.

## Accessibility: no way to put keyboard focus on an element from a handler

- **Feature:** screen-reader use of the file list (milestone 11; NORTHSTAR "zero learning to
  start", for everyone). A reader follows the selection only while the list holds focus (its
  active descendant is the cursor's row).
- **What Rune is missing:** a `Tx::focus(key)` for any focusable element. Text fields take focus
  through their `TextEdit::focused`, and focus returns to a keyed element when an overlay or a
  `focus_return` element goes away, but nothing else moves it: not at launch, not when Arbor
  switches panes.
- **What we did instead:** the list is focusable and hands every key to the window's own
  handler (`list` in `src/view.rs`, `timing::key`), so keys behave the same focused or not; Tab
  puts focus there when the window has one pane (Arbor passes Tab on), a click does in any
  case, and overlays give it back when they close. In a split window both lists name the
  active pane's cursor row as their active descendant (`active_row`), so whichever holds focus,
  a reader hears the row the keys move.
- **What users lose:** at launch, focus is on the window: a VoiceOver, Narrator or Orca user
  hears nothing as ↑↓ move the selection until a first Tab (one pane) or a click puts focus in
  the list. In a split window opened before that, Tab switches panes instead, so it takes a
  click.
- **Suggested Rune change:** `Tx::focus(key)` (and `AppSpec::initial_focus` or
  `El::autofocus`) for any focusable element, applied after the handler's commit as
  `TextEdit::focused` is.

## Accessibility: no announcements on demand, and no "several can be selected"

- **Feature:** the status bar announcing operation results and errors; the file list's
  multi-selection.
- **What Rune is missing:** a `Tx::announce(text, politeness)` (AccessKit's adapters announce
  only when a live region's name changes), and `El::multiselectable` (AccessKit has
  `Node::set_multiselectable`; `A11yMore` has no field for it).
- **What we did instead:** the note is a live region (`role("status")`, or `"alert"` for a
  failure: `State::alert`), so each new note is announced. Rows report selected or not.
- **What users lose:** the same note twice in a row ("Restored 1 item" after two undos) is
  announced once. Readers don't say the list takes several selected items (⇧ and ⌘ with a
  click or an arrow), so a blind user has to know.
- **Suggested Rune change:** `Tx::announce(text, LiveRegion)`, routed to each adapter's
  announcement (NSAccessibility's announcement request, UIA's notification event, AT-SPI's
  `Announcement`), and `El::multiselectable(bool)`.

## Accessibility: per-frame labels can't be lazy, and the app can't tell a reader is there

- **Feature:** what Arbor names for assistive technology must cost nothing when no reader is
  connected (NORTHSTAR: pay only for what you use).
- **What Rune is missing:** a way to build labels only when the tree is built (a label closure,
  or `Cx::accessibility_active()`). `El::label` and `El::description` take a `String`, made
  every time the view runs; Rune itself builds nothing until a client asks
  (`AccessibilityLink`).
- **What we did instead:** nothing per row: rows are named by Rune from the text they already
  draw (`roles::named_from_content` for options), a folder's "▸" is `a11y_hidden` (or labelled
  "Folder" only while the Kind column is hidden), and shortcuts and decoration are
  `a11y_hidden`. Labels are kept to the window's chrome (a few dozen short strings a frame:
  the list's "Files in …", the path bar's buttons, headers) and to overlays while open.
- **What users lose:** nothing measurable (BENCH.md, milestone 11), but a richer description per
  row (kind, size and date as a description separate from the name, as Finder gives them)
  would cost an allocation per visible row per frame for everyone.
- **Suggested Rune change:** `Cx::accessibility_active() -> bool` (true once a client
  activated the window's tree; the view re-runs when it changes), or `El::label_with(|| ..)`
  called only when the node is built.

## Accessibility: increased contrast and larger text aren't reported

- **Feature:** respecting each OS's accessibility settings (milestone 11).
- **What Rune is missing:** the system's increased-contrast setting (macOS "Increase contrast",
  `accessibilityDisplayShouldIncreaseContrast`; Windows high-contrast themes,
  `SPI_GETHIGHCONTRAST`; GNOME's `high-contrast`) and its text-size preference (Windows' "Text
  size", GNOME's `text-scaling-factor`; macOS has no global one). `Cx` reports dark mode,
  accent, reduced motion, scroll-bar style and Full Keyboard Access only.
- **What we did instead:** nothing: Arbor draws its normal theme and sizes. Reduced motion is
  moot (Arbor never animates).
- **What users lose:** someone who turned on increased contrast or larger text gets Arbor's
  default greys (secondary text, dimmed rows, the grey selection of the inactive pane) and
  13 pt text, unlike native apps.
- **Suggested Rune change:** `Cx::increased_contrast()` and `Cx::text_scale()` (re-running the
  view on change), and a contrast variant in `tok::theme`.

## Opening folders from Finder: no "open documents" hook

- **Feature:** Arbor as the place folders open (NORTHSTAR scope: "the default handler for
  opening and revealing folders"): "Open With → Arbor" in Finder, `open -a Arbor dir`, and
  Arbor as the default for folders.
- **What Rune is missing:** a hook for the files and folders macOS opens with the app. macOS
  sends them as an "open documents" Apple event (`kAEOpenDocuments`), to the running app or the
  one it launched for them (the path isn't in `argv`). Rune handles only the "open URL" event
  (`App::on_open_url`, `open_url/incoming.rs`), and winit 0.30 has no `application:openURLs:`.
- **What we did instead:** Arbor installs its own handler for the event (`src/incoming.rs`,
  `mac`): an Objective-C object registered with `NSAppleEventManager` when
  `NSApplicationWillFinishLaunchingNotification` arrives (AppKit installs its own handler while
  it launches, so earlier would be overwritten; the launch's event comes after). It reads the
  event's file URLs, sorts folders from files on a thread, and hands them to the window through
  `AppHandle::update` (and `AppHandle::open`). Checked on the M4 Pro: the folder Arbor is
  launched for, and one sent to it while running.
- **What users lose:** nothing today. The cost is that Arbor carries Objective-C runtime code
  for something every document-opening Rune app needs, and depends on AppKit's launch order.
- **Suggested Rune change:** `App::on_open_files(|s, paths: &[PathBuf], tx|)` on macOS, fed
  from the same Apple-event inbox as `on_open_url` (with the launch's files kept until a window
  listens). Windows and Linux could pass the files from `argv` to the same hook, so apps handle
  one path everywhere.

## Wakes: one raised while Rune drains wakes is handled in the same run-loop turn

- **Feature:** the speed bench (`ARBOR_BENCH`, `src/timing.rs`), which drives Arbor one step per
  frame from `App::on_frame_encoded`, and anything else that wakes the window from a frame hook
  or from `poll`.
- **What Rune is missing:** wakes raised during the drain going to the next turn. On macOS a
  wake raised while Rune is draining them (winit's `control_flow_end_handler` →
  `map_user_event`, which loops on `try_recv`) is picked up by the same loop. A covered or
  not-yet-visible window's frame (`covered_frame` → `poll_hidden` → the frame report) that wakes
  again therefore never returns to AppKit, so the occlusion change that would end covered mode is
  never delivered. Sampled on the M4 Pro: 100% of the main thread in that observer callout,
  46–100% CPU, until the bench's 60 s timeout; 13 of 63 Jump and tab runs with the display
  awake (the window opens inactive and lacks `.visible` for its first ~100 ms), every run with
  it asleep.
- **What we did instead:** the bench never wakes from a frame that wasn't drawn, and starts its
  next step only once the last step's frame was drawn (`shown` and `STEP_SHOWN` in
  `src/timing.rs`); Rune draws again by itself when the window shows. With that, 0 of 12 runs
  stuck. On a timeout the bench prints the run's last lines and, on macOS, `sample`s it first,
  and it warns when the display is asleep (`examples/bench.rs`).
- **What users lose:** nothing directly (outside the bench nothing wakes from a frame hook). The
  risk is for any future code that wakes from `poll` or a frame report while the window is
  covered: it would spin a core and starve AppKit.
- **Suggested Rune change:** hand wakes that arrive while a drain runs to the next run-loop
  turn (drain only what was queued when the drain began), so AppKit's notifications always get
  a turn in between.

## Truncated text: each width tried shapes the whole string again

- **Feature:** Jump to Folder's rows and Search's folder column, which show long paths cut at the
  start ("…/projects/arbor"), so the folder's own name stays in view (NORTHSTAR: every key within
  a frame).
- **What Rune is missing:** a cheap head (or middle, or end) truncation. `truncated()`
  (`runtime/commit/text_layout.rs:198–255`) binary-searches the cut, and each probe builds the
  cut string and lays it out again in full (`layout_blocks_inner`): about 8 shapes for every row
  that doesn't fit. Its elision cache only helps when exactly the same row shows again. On the
  M4 Pro, with a fresh Jump query each round over ~85-character paths, it was 45% of a frame:
  keys took 6.3–8.6 ms of `update_ms` hot and 14–19 ms once the CPU cooled, against 1.1–1.7 ms
  for the same history with short paths (FINDINGS of the 2026-11-08 Mac investigation).
- **What we did instead:** Arbor cuts long paths itself to whole trailing folders that fit,
  "…/" first, before Rune sees them (`fit_path` in `src/view.rs`), estimating widths from
  per-character advances of the font measured once per thread (`char_advance`, through
  `rune::text::advance`), with 3% to spare. The bold match ranges move with the cut
  (`shift_marks`). `truncate_head()` stays as the fallback, which then almost never runs. The
  search folder column got a width of its own (the same share of the row) so the cut knows its
  room.
- **What users lose:** a little room: the estimate leaves up to a few percent unused, and a path
  is cut at a folder boundary, so "…/projects/arbor" may show where "…ojects/arbor/src" would
  have fit (which also reads better). A row with a character outside ASCII is estimated with
  the widest ASCII advance, so it's cut a little shorter.
- **Suggested Rune change:** shape the string once and find the cut from the clusters' advances
  (plus the ellipsis's width), then shape only the final cut string: one or two shapes instead
  of eight. Optionally a "cut at these byte offsets only" hint (path separators), so apps get
  whole-segment cuts for free.

## Context menu: no native pop-up menu on Linux

- **Feature:** right-click on a row or the list opens a context menu (NORTHSTAR: "Out of the box
  it works like Finder or Explorer: click, double-click, drag, right-click, menus").
- **What Rune is missing:** a native pop-up menu off macOS and Windows. `Tx::popup_menu` answers
  `None` at once on Linux (`popup_menu/mod.rs`: `popup_menu_supported()` is false; the host
  there shows nothing), so the native menu Arbor opens on macOS and Windows never appears.
- **What we did instead:** where `rune::popup_menu_supported()` is false, the same items, from the
  same table (`State::menu_items` in `src/actions.rs`), open in Arbor's own menu in the window
  (`src/menu.rs`, `menu_view` in `src/view.rs`): a small card at the pointer, flipped at the
  window's edges, with each command's keys; ↑↓, Home, End and a letter move, Enter or Space
  runs, Esc or a click outside closes. Checked under X11 (Xvfb, XTest).
- **What users lose:** it's drawn inside the window, so it can't extend past the window's edges
  (a long menu in a small window is shifted to fit), and it doesn't look like the desktop's own
  menus (GTK or Qt): no theme, no accessibility roles of a native menu (it's a modal card), no
  submenus. Under Wayland it can't appear over other windows, which a native popup surface could.
- **Suggested Rune change:** a Linux host for `popup_menu`: an `xdg_popup` with a positioner on
  Wayland and an override-redirect window on X11, drawn by Rune with the platform's menu metrics,
  so `Tx::popup_menu` works the same on all three.

## Search: a virtual list that grows costs a read of every key

- **Feature:** Search in This Folder (NORTHSTAR Core: "live walk, no index, streaming"). Results
  arrive in batches, at most one per frame, for as long as the walk runs, and are appended to
  the list.
- **What Rune is missing:** a cheap append to `virtual_rows`. When the row count changes,
  layout reads every row's key again, copies each into a `String` and rebuilds its key sets
  (`runtime/commit/measure.rs`: `same` is false whenever `count != keys.len()`, and the
  `virtual_rows_version` shortcut only applies to an unchanged count). So each frame that adds
  rows costs time in proportion to the whole list. Measured under Xvfb on tabot-dev: a frame's
  update grew from 5 ms at 6,000 results to 54 ms at 60,000, and a key pressed while results
  streamed took 29.5 ms median, 56 ms p95.
- **What we did instead:** while a walk is still going, the list takes in at most 1,000 results
  (`LIVE_ROWS` in `src/search.rs`); the rest are counted in the status bar ("Searching… 5,234
  found in 40,000 items · showing the first 1,000 until it's done") and join in one step when the
  walk ends or Esc stops it, which costs what a listing of that size costs (7–8 ms for 61,000
  rows here).
- **What users lose:** during a long search over a big tree they can scroll through the first
  1,000 results only, until it finishes or they press Esc. The step at the end is one frame of
  Rune's per-key cost for the whole list.
- **Suggested Rune change:** with an unchanged `virtual_rows_version` and a larger count, treat
  the last commit's keys as a prefix and read only the new rows' keys (an append), or an explicit
  `virtual_rows_appended(old_count)` hint. Then a streaming list costs per frame only what it
  adds.

## Quick preview: no hook that runs after every input

- **Feature:** quick preview (NORTHSTAR Core). While it's open it follows the active pane's
  selection, however the selection moved: a key, a click on a row, a click in the other pane, a
  listing landing, a tab shown.
- **What Rune is missing:** a callback that's guaranteed to run after a frame's input handlers.
  `AppSpec::poll` is documented as "called at the start of every frame", and in a frame with
  input it does run after the handlers, except when an earlier wake's `poll` already ran for that
  frame (`timer_wake.rs`, `frame_polled`; `frame.rs` skips `poll` when it's set). Seen under
  Xvfb: a ↓ that arrived right after a preview landed moved the list, and the preview stayed on
  the old file until the next event.
- **What we did instead:** `State::sync_preview` (`src/preview.rs`) is called wherever the
  selection can move: after every key (`State::on_key`) and command (`State::run`), when a
  listing lands (`State::land`), when a pane is made active, closed or shows a tab
  (`src/panes.rs`), and after a click on a row (`file_row` in `src/view.rs`). `poll` calls it too,
  as a catch-all. Closed, each call is one `None` check.
- **What users lose:** nothing today. The cost is fragility: a new way to move the selection
  that forgets the call leaves the preview showing the old item until the next key, click or wake.
- **Suggested Rune change:** run `poll` (or a new `AppSpec::after_input`) after the input
  handlers of every frame that had input, even when a wake already polled for it; or document
  that `poll` may not follow input, so apps know not to rely on it.

## Text preview: `Family::Mono` isn't monospaced in native builds

- **Feature:** quick preview of text files, shown in a monospaced font so columns and
  indentation line up.
- **What Rune is missing:** a monospaced font behind `TextStyle::mono()` on native platforms.
  The `Mono` slot defaults to "IBM Plex Mono" (`text.rs`), which only web builds bundle
  (`embedded-fonts`); natively, unless the user installed it, text falls back to Inter.
- **What we did instead:** the preview uses `Family::SystemMono` (SF Mono or Menlo on macOS,
  Cascadia Mono or Consolas on Windows, fontconfig's monospace on Linux), loaded the first time
  a text preview shows (`CODE` in `src/view.rs`). The status bar's filter field still uses
  `mono()`, and so isn't actually monospaced.
- **What users lose:** nothing in the preview. Opening the first text preview of a session
  spends about 8 ms more building that frame (Linux: Rune runs `fc-match` and loads the font).
- **Suggested Rune change:** point the native `Mono` slot at the system monospaced families
  (as `SystemMono` does) when no mono font was supplied, or bundle a mono font natively too.

## Breadcrumbs: no way to say which children shrink first

- **Feature:** the path bar's breadcrumbs, in split view and narrow windows (NORTHSTAR: the
  same app at every size; you always see where you are).
- **What Rune is missing:** flex-shrink weights (or a shrink order). An element either shrinks
  (`shrink()`, `truncate()`: CSS `flex-shrink: 1`) or doesn't, so when a row of crumbs is too
  wide every crumb is cut in proportion to its width, the current folder included ("ho…",
  "s…", "m3de…", "Pho…").
- **What we did instead:** the view measures each label with `rune::text::measure` against the
  pane's laid-out width (`shown_crumbs` in `src/view.rs`) and folds the leading crumbs into one
  "…" crumb (which opens the deepest hidden folder) until the rest fit whole. The current
  folder truncates only when it alone doesn't fit. A cheap length bound skips measuring when
  the pane is clearly wide enough.
- **What users lose:** little. Hidden ancestors are a click on "…" away (one level at a time;
  there's no menu of them). The fit is computed from constants that mirror the path bar's
  layout (padding, button sizes), so a layout change there has to update `PATHBAR_CHROME` too.
- **Suggested Rune change:** `El::shrink_weight(f32)` (CSS `flex-shrink: n`), or a
  `shrink_order(i32)` that cuts lower-order children to their minimum before touching higher
  ones. With either, the crumbs could stay one plain row.

## Drag between panes: a system drag can't land in the window it came from

- **Feature:** split view (NORTHSTAR Core): dragging rows from one pane onto the other copies or
  moves them, as a drop from Finder or Explorer does.
- **What Rune is missing:** a drag that can end inside its own window. On macOS a `drag_out`
  session offers nothing to the app's own windows (`drag_out/macos.rs`: `mask_in` answers
  `None` for `WithinApplication`, and `mute` unregisters the app's windows' drag types for the
  session), so the other pane's `on_drop_files` never hears it. An element with `drag_out`
  never runs its `on_drag` on macOS, and which of the two a press becomes is fixed per element,
  so one gesture can't be "inside the window, then out to Finder". Off macOS, `drag_out` starts
  no system drag at all, so there's nothing to drop either.
- **What we did instead:** while the window is split, rows drop `drag_out` and use `on_drag`,
  Arbor's own drag inside the window: the pane under the pointer comes from the panes' laid-out
  x (`Panes::at` and `file_row` in `src/view.rs`), the target pane gets a drop highlight, and the
  release goes through the same code as a file drop (`State::drop_on_pane` in `src/panes.rs`:
  same disk moves, another copies, ⌥ copies). Unsplit, rows keep `drag_out` as before. Checked
  under X11 (Xvfb, XTest pointer).
- **Unsplit, too:** a row dropped on a folder row of the same window does nothing on macOS (the
  QA session's "folder-row drop does nothing"): the OS drag offers nothing to its own window.
  Arbor can't even tell that's where it was let go: AppKit passes the end point to
  `draggingSession:endedAtPoint:operation:` (`drag_out/macos.rs`), but `on_drag_end` gets only
  the outcome. So any drag nothing took now says how to move files instead ("Nothing took the
  drop. To move files into a folder in this window, cut and paste them (⌘X, ⌘V) or use split
  view (⌘\\)", `State::drag_out_ended` in `src/actions.rs`), also after an Escape or a refused
  drop elsewhere. Starting every row drag as Arbor's own and handing it to the OS only when the
  pointer leaves the window isn't possible either: which of the two a press becomes is fixed per
  element when it starts (above).
- **The sidebar, too (milestone 12):** rows dragged onto the sidebar add their folders through
  the same in-window drag, which now runs unsplit as well; on macOS, unsplit, the row's drag is
  the system's, so it can't land on the sidebar of its own window (folders from Finder can).
- **Tabs, too (milestone 14):** a tab dragged along its strip or onto the other pane is the same
  in-window drag. A handler can't ask which element is under the pointer, so where it lands in
  the other pane's strip is worked out from the strip's layout constants and the pane's rect
  (`strip_slot` in `src/view/pane.rs`), as Rune draws it; a layout change there has to update it.
  No drag image follows the pointer, only the landing place's highlight. Dragging a tab out
  into a window of its own isn't offered.
- **What users lose:** on macOS, while split, a row can't be dragged out to Finder or another
  app; closing the split (⌘\\) brings that back. The in-window drag shows no drag image under the
  pointer, only the target pane's highlight, and can't drop on a folder row in the other pane
  (only into its folder).
- **Suggested Rune change:** at the least, pass where a `drag_out` ended to `on_drag_end` (window
  points, or whether it was over one of the app's own windows). Better, either an opt-in for a
  `drag_out` session to drop in its own app
  (`DragItem::allow_in_app()`, routed to `on_drop_files` and `on_drop_hover` like any drop), or
  let an element have both: run `on_drag` while the pointer stays in the window and start the
  system session only when it leaves.

## Command availability: no cheap "what's on the clipboard?" check

- **Feature:** the command palette and context menu dim commands that can't run right now
  (NORTHSTAR: every action reachable from the palette, which shows what's possible).
- **What Rune is missing:** a synchronous, cheap query of what the clipboard holds (files, text,
  nothing) without reading it. Rune only offers `Tx::read_clipboard` and `Tx::request_paste`,
  which read the whole content off the event thread and answer an update later. AppKit has
  `NSPasteboard.types` / `changeCount`, Windows `IsClipboardFormatAvailable(CF_HDROP)`, and
  Wayland/X11 the offered MIME types, all without transferring data.
- **What we did instead:** Paste and "Move Here" always show as available
  (`State::unavailable` in `src/app.rs`). Pasting with nothing usable says "Nothing to paste:
  copy some files first" (`State::paste` in `src/actions.rs`). Reading the clipboard each time the
  palette or menu opens was rejected: it transfers the content (possibly a large image) for a
  yes/no answer, and the answer would arrive a frame after the palette is drawn.
- **What users lose:** Paste looks runnable in the palette and context menu when it would do
  nothing, unlike Finder and Explorer, which grey it out. A small trust cost on a common action.
- **Suggested Rune change:** `Tx::clipboard_kinds() -> ClipboardKinds { files, text, image, .. }`
  (or `Cx::clipboard_kinds`), answered from the platform's type list without reading data, and
  `None` where it can't be known cheaply (the web).

## Cold start: most of the 100 ms budget goes before the app's code runs

- **Feature:** cold start to interactive window, budget 100 ms (NORTHSTAR speed budgets).
- **What Rune is missing:** a launch fast enough to leave the app room, and a way to see where
  launch time goes in a release build. On an M4 Pro (macOS 27.2), `main` → first frame
  submitted is about 123 ms (median) whether the folder has 1 entry or 10,000. `AppSpec::init`
  starts 65–80 ms after `main`, and window, Metal surface and first frame take about 40 ms more.
  Arbor's own share is about 3 ms (`init` 0.2 ms, first view build 2–4 ms). Rune's launch
  breakdown (`perf_warn::launch`) exists only in debug builds and only warns past 100 ms of app
  time, so the 65–80 ms before `init` can't be split further from outside Rune.
- **What we did instead:** call `rune::prewarm_gpu()` first thing in `main` and read the first
  folder on a thread started there, so both overlap AppKit's launch (`src/main.rs`,
  `app::read_first_folder` in `src/app.rs`). That took the 10k-folder case from ~140 ms to
  ~123 ms `main` → frame. `ARBOR_TIMING=1` logs `init_at_ms` and `init_ms` to show the split
  (`src/timing.rs`). Numbers are in `BENCH.md`.
- **What users lose:** every launch takes about 140 ms from click to window instead of under
  100 ms. Opening a folder from another app (Arbor as the default folder handler) feels like
  launching an app instead of switching to one.
- **Suggested Rune change:** an opt-in release-build launch breakdown, for example phases added
  to the `RUNE_READY_DIR` JSON (event loop built, did-finish-launching, window created, surface
  ready, first frame submitted). Then cut the time before the window is created, for example by
  creating the window and drawing the first frame as soon as AppKit allows.

## Frame timing: no "presented at" for a frame

- **Feature:** measuring "keypress to updated pixels under 8 ms" (`ARBOR_TIMING`, `scripts/bench.sh`).
- **What Rune is missing:** when a frame actually reached the screen. `FrameEncoded::at()` is
  when the commands were submitted, and `presents_at()` is the display link's target, known only
  for display-paced macOS frames. It's `None` for the first frame after a key press, which is
  exactly the one that matters. Rune keeps Metal's `presentedTime` timestamps for timing only
  while tracing (`--trace`, `frame/present_time.rs`; untraced, it reads them only to keep a
  transparent window's shadow in step, `window/create.rs`) and doesn't pass them to apps. Other
  platforms have no source at all.
- **What we did instead:** time to `FrameEncoded::at()` (submission) with `Tx::input_time()` as
  the start (`src/timing.rs`), labelled a "submission proxy" in `BENCH.md` and the bench output,
  which claim no pass against the pixel budgets. Presentation runs two to three frames behind
  submission (`frame/present_time.rs`).
- **At launch:** the first two drawables are never presented even in a bare AppKit + Metal app
  (`presentedTime` 0 for frames 1–2 on the M4 Pro), so pixels reach the screen ~55 ms after the
  first submission the cold-start numbers end at.
- **What users lose:** nothing directly, but the budget can't be verified. Measured numbers
  undercount by up to one refresh (8.3 ms at 120 Hz, 16.7 ms at 60 Hz) plus compositor latency,
  so a key press that measures 1 ms could still miss the 8 ms budget on screen.
- **Suggested Rune change:** an `App::on_frame_presented(|seq, Option<Instant>|)` hook, active
  only when registered. Sources: `presentedTime` / `addPresentedHandler` on macOS,
  `IDXGISwapChain::GetFrameStatistics` on Windows, and `wp_presentation` or
  `VK_KHR_present_wait` on Linux where available.
