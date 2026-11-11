# Backlog

Owner requests waiting their turn. Newest first. Each item still has to meet NORTHSTAR (speed
budgets, no caching, pay only for what you use) when it's built.

## Process (owner, 2026-11-11)

Each feature is built on its own branch (`feature/<name>`, in its own git worktree), goes through
build → review → fixes → test, and is then **squash-merged** into `main` as one commit, so any
feature can be reverted with a single `git revert`. Bug fixes may go straight to `main`.

## Table-stakes features, after the views (chosen by the agent, 2026-11-11)

Missing basics every file manager has, in build order:

1. `feature/new-window`: multiple windows (⌘N, "Open in New Window"), each independent.
2. `feature/new-file-links`: New File (empty text file, named inline) and Make Link (symlink;
   a shortcut where symlinks need privileges on Windows); undoable.
3. `feature/archives`: Compress to .zip and Extract (.zip, plus .tar/.tar.gz if cheap), on the
   worker with progress and cancel, never overwriting, safe against path traversal, undoable.
4. `feature/columns-status`: resizable and hideable list columns, a folders-first option, and
   a status bar showing the selection's size and the volume's free space (read on demand).

Left for later, per NORTHSTAR's "Later" list: open terminal here, folder sizes column,
browsing inside archives, folder compare, operation queue. File watching stays out (no watchers).

## After the current test-and-fix round (owner, 2026-11-10)

The owner tried `v0.1.0-alpha.1` on the Mac: "very fast… I'm very happy with the performance."
These come after the Mac QA round's real bugs (and any bad performance spots) are fixed.

- **Drag-select (rubber band):** drag in empty space in the list to select a range of rows, with
  ⌘/⇧ to add to the selection, like Finder and Explorer. Not built today (only click, ⇧-click,
  ⌘-click, keyboard).
- **More views**, switchable per tab (and remembered per folder only if that's user data, not a
  cache):
  - **List** (today's view).
  - **Thumbnails / icons:** a grid with image and document thumbnails, generated on demand for
    the visible cells only, cancelled on scroll, nothing stored by Arbor (NORTHSTAR's
    view-triggered work rules; reuses the preview decoders and the OS thumbnailers already
    approved in DECISIONS.md).
  - **Tiles:** larger icon plus name and a line of details.
  - **Columns (Finder's column view):** selecting a folder opens its contents in a new column to
    the right; arrows move across columns; the preview shows in the last column for a file. Each
    column is a fresh listing read with the usual bounded wait; nothing cached.
