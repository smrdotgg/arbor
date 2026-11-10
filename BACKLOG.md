# Backlog

Owner requests waiting their turn. Newest first. Each item still has to meet NORTHSTAR (speed
budgets, no caching, pay only for what you use) when it's built.

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
