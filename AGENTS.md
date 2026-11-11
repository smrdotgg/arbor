# Agent guidelines for Arbor

**Read [`NORTHSTAR.md`](NORTHSTAR.md) before any change.** It decides every trade-off. If a change
conflicts with it, don't make the change. Raise the conflict with the owner instead.

Before adding anything that caches, animates, polls, or runs in the background, check the hard
rules in `NORTHSTAR.md`. Breaking one needs an entry in its Exceptions log with the owner's
explicit, dated sign-off. Never add an exception yourself.

A change is done only when:

- it meets the speed budgets in `NORTHSTAR.md`
- it works on macOS, Windows and Linux
- nothing destructive happens without undo or an explicit "can't be undone" confirmation

**Use Rune as it is.** Never edit `vendor/rune` or patch Rune. If a feature has to be built in a
limited way because Rune lacks something, build the best workaround inside Arbor and add an entry
to [`RUNE-GAPS.md`](RUNE-GAPS.md).

**On the owner's Mac over ssh:** builds and headless tests only. Never run `osascript`, System
Events or any other GUI automation, and never trigger Accessibility, Automation or Screen
Recording permission prompts. GUI checks go through the project manager's Codex QA sessions. The owner says when they're
using the Mac; until they do, assume it's free for those sessions.

When the owner is away, don't block on questions: pick the recommended option, carry on, and
record it in [`DECISIONS.md`](DECISIONS.md) so it can be reviewed and undone.

See `README.md` for build commands and keys, and `ARCHITECTURE.md` for the source layout.
