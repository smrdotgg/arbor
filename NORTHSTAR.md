# Satchel North Star

> **Satchel: every action, instantly. Everything you need, nothing you didn't ask for.**

Satchel is the fastest file manager on every computer, and the only one you need. It replaces Finder
and Explorer with one identical app on macOS, Windows and Linux. Anyone can use it on day one, and
it has no ceiling for power users.

When two good ideas compete, this document decides. If a change conflicts with it, the change
loses, or this document is changed first by the owner.

## Priorities

1. **Speed.** Every action lands within one frame. The UI never freezes.
2. **Power.** The features people can't go back to living without, and they cost nothing until
   used.
3. **Trust** is assumed, not traded. See [Safety](#safety).

When speed and power conflict, speed wins the default and power waits for the user to ask.

## Who it's for

Power users first, but never at the expense of everyone else: **zero learning to start, no
ceiling.**

- Out of the box it works like Finder or Explorer: click, double-click, drag, right-click, menus.
- Every action is reachable from a **command palette** that shows its shortcut. Normal people use
  it as a search box. Power users learn the keys and stop needing it.

## Hard rules

These define Satchel. Breaking one requires an entry in the [Exceptions log](#exceptions-log) with
the owner's explicit, dated sign-off. Assume no exception will be granted.

### No caching

A **cache** is any stored copy of something that can be re-derived from another source of truth
(the filesystem). Banned: thumbnail stores, stored folder sizes, search indexes, saved listings,
prefetch.

**User data is not a cache**, because the disk can't regenerate it: settings, keybindings,
bookmarks, history, frecency. These may be stored.

Thumbnails are not a good enough reason for an exception.

### No animation

Every change lands instantly.

### No polling

Idle means idle. No timers, watchers or redraws unless the user did something.

### View-triggered background work

Some work may start without an explicit request because the user opened a view (for example,
folder sizes streaming into a column). It must follow all of these:

1. **First paint never waits for it.** The folder opens instantly. Results stream in.
2. **Scoped to what's on screen.** Leaving the folder or scrolling cancels it immediately.
3. **Finishes and stops.** No watching, no re-checking.
4. **Nothing is stored.** Come back and it recomputes.
5. **A slow disk slows the results, never the UI.**

### Pay only for what you use

A feature may cost time only when it is turned on or invoked. The default path is never slower
because a feature exists.

## Speed budgets

Missing a budget is a bug, not a trade-off. A feature that can't meet them doesn't ship.

| Measure | Budget |
| --- | --- |
| Keypress to updated pixels | under one frame (8 ms at 120 Hz) |
| Cold start to interactive window | under 30 ms on Windows and Linux; on macOS, within 25 ms of a bare native window (macOS's own launch is about 110 ms) |
| Folder listing visible (10k entries, local SSD) | under 50 ms |
| UI frozen, on any disk, in any state | never |

## Safety

**Nothing destructive happens silently.** Every operation is either undoable or explicitly
confirmed as permanent before it runs.

1. **Undoable by default.** Rename, move, copy, new folder and trash go on the undo stack.
2. **Permanent means a confirmation.** Anything that can't be undone (permanent delete, emptying
   Trash, a volume with no trash) asks first and says plainly that it can't be undone. This is the
   only kind of dialog Satchel shows.
3. **Undo refuses rather than guesses.** If the disk changed since the operation (the file was
   moved, edited or deleted, or the drive was ejected), undo stops and says why. It never
   overwrites to force itself through. A wrong undo is worse than no undo.
4. **Never overwrite.** A name collision always gets a free name.
5. **File operations never block the UI.** They run on a worker with visible progress and can be
   cancelled.

## Scope

**In:** the place you manage files, and the default handler for opening and revealing folders.
Local files.

**Later:** network shares, SFTP, remote storage.

**Out, permanently:** desktop icons and desktop handling, replacing Open/Save dialogs in other
apps. The OS owns these, and fighting it would cost speed and break "same everywhere."

## Cross-platform

**One identical app** on macOS, Windows and Linux, with the same keys, layout and behaviour. Native
touches only where muscle memory demands it (⌘ vs Ctrl, each OS's trash, reveal in the system
file manager).

All three platforms are first-class: CI builds all three, and **a feature isn't done until it works
on all of them.**

## Configuration

Settings and keybindings in one config file. Nothing more for now: no custom commands, no plugins.
If plugins ever exist, they run out of process and can't touch the UI thread.

## Features

| Status | Features |
| --- | --- |
| **Core** (defines Satchel) | Batch rename (regex, numbering, preview before apply) · Tabs · Split / dual pane · Quick preview (on demand, no cache) · Fuzzy jump (frecency) · Recursive search (live walk, no index, streaming) · Select by pattern |
| **Later** | Operation queue · Folder sizes · Archive browsing · Open terminal here · Folder compare / sync |
| **Never, for now** | Tags (they don't exist the same way everywhere) · Plugins |

## How we know it's winning

Milestones, in order:

1. **The owner goes a month without opening Finder or Explorer.** This proves the power.
2. **A normal user switches and doesn't go back.** This proves "for everyone."
3. **Every speed budget holds on low-end hardware** (an old laptop, a spinning disk). This proves
   the speed isn't just a fast Mac.

## Exceptions log

Every exception to a hard rule is recorded here before it's built. Each entry needs:

- **Rule broken:** which hard rule
- **Feature:** what needs it
- **Why no compliant design works:** the alternatives tried
- **Measured cost without it:** numbers, not feelings
- **Owner sign-off:** name and date

### WebP preview decodes can't be cancelled midway

- **Rule broken:** View-triggered background work, "Scoped to what's on screen. Leaving the folder or scrolling cancels it immediately."
- **Feature:** Quick preview of WebP images.
- **Why no compliant design works:** the WebP decoder (`image-webp`) decodes a whole image in one call, with no row-by-row or chunked API, so a stop request can only take effect after it returns. PNG, JPEG and GIF are cancelled between rows or chunks. Each platform's own decoder (ImageIO, WIC) has the same limitation, and Linux has none to fall back on. Keeping the rule literally meant capping WebP at 0.45 MP lossy / 1.7 MP lossless (≤ 30 ms of work after cancel), so most WebP photos showed "Too large to preview". The cap is now 4 MP, lossy and lossless.
- **Measured cost without it:** with the cap at 4 MP, the work that goes on after a cancel on tabot-dev (Xeon 8375C, 4 vCPU, load average 1.5–2.7) is up to 0.57 s for a 4 MP lossy WebP of random pixels with alpha (the worst case; 0.33–0.39 s in the quieter passes) and 0.03–0.07 s for a photo-like one; up to 0.16 s for a 4 MP lossless WebP of random pixels (0.09 s in the quietest pass) and 0.04–0.06 s for a photo-like one. Peak RSS while one decodes: 34 MB for the process (4.5 MB before), about 30 MB for the decode. The UI thread is never involved: one decode runs at a time on the preview worker, a newer request supersedes it, and its result is discarded.
- **Owner sign-off:** smr, 2026-11-08 (approved in the agent session: "Raise cap, log exception").
  Cap lowered from 12 MP to 4 MP by the owner, 2026-11-08, after the 12 MP numbers (1.0 s lossy / 0.35 s lossless on tabot-dev) came in.

_Keep this list as short as possible._
