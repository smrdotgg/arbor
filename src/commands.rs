//! Every user action, in one table: a stable id (`file.trash`), a title ("Move to Trash") and its
//! default keys. Key dispatch (`app/dispatch.rs`), the command palette (`palette.rs`), the context menu
//! (`actions.rs`), the settings template (`config.rs`) and the README's key table all read it.
//! What each command does, and when it's available, is in `State::run` and
//! `State::unavailable` (`app/dispatch.rs`), which match on [`Cmd`] exhaustively, so a new command can't
//! be left without either.
//!
//! Keys are written as in the settings file: `cmd` is ⌘ on macOS and Ctrl elsewhere; `ctrl`,
//! `alt`, `shift` and `super` name those keys (see [`parse_chord`]).

use rune::{Chord, KeyPlatform};

macro_rules! commands {
    ($($var:ident $id:literal $title:expr, [$($key:literal),*];)*) => {
        /// A user action. See the module docs.
        #[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
        pub enum Cmd { $($var),* }

        impl Cmd {
            /// Every command, in the order the palette and the settings template list them.
            pub const ALL: &'static [Cmd] = &[$(Cmd::$var),*];

            /// The stable id the settings file binds keys to.
            pub fn id(self) -> &'static str {
                match self { $(Cmd::$var => $id),* }
            }

            /// The title the palette and the context menu show.
            pub fn title(self) -> &'static str {
                match self { $(Cmd::$var => $title),* }
            }

            /// Its default keys as the table writes them: `mac:` before a key makes it
            /// macOS's only, `pc:` Windows' and Linux's only, `win:` Windows' only (see
            /// [`Cmd::default_keys_on`]).
            fn key_specs(self) -> &'static [&'static str] {
                match self { $(Cmd::$var => &[$($key),*]),* }
            }
        }
    };
}

impl Cmd {
    /// Commands that can't be undone. Running one asks first, in Satchel's one dialog
    /// (`confirm.rs`), and only its destructive button runs it.
    pub fn permanent(self) -> bool {
        matches!(self, Cmd::ClearJump | Cmd::Delete | Cmd::EmptyTrash)
    }

    /// Commands that ask first: the permanent ones, and those that change how the system
    /// behaves (undone by their counterpart, which the dialog names).
    pub fn asks(self) -> bool {
        self.permanent() || matches!(self, Cmd::MakeDefault | Cmd::RestoreDefault)
    }

    /// The keys it has on `platform` when the settings file doesn't bind it, in settings
    /// syntax. Most keys are the same everywhere; a few follow each platform's convention
    /// (Delete Permanently is ⌥⌘⌫ on macOS, as in Finder, and Shift+Delete elsewhere, as in
    /// Explorer).
    pub fn default_keys_on(self, platform: KeyPlatform) -> Vec<&'static str> {
        let mac = platform == KeyPlatform::Mac;
        self.key_specs()
            .iter()
            .filter_map(|k| match (k.strip_prefix("mac:"), k.strip_prefix("pc:"), k.strip_prefix("win:")) {
                (Some(k), ..) => mac.then_some(k),
                (_, Some(k), _) => (!mac).then_some(k),
                (.., Some(k)) => (platform == KeyPlatform::Windows).then_some(k),
                _ => Some(*k),
            })
            .collect()
    }

    /// [`Cmd::default_keys_on`] this platform.
    pub fn default_keys(self) -> Vec<&'static str> {
        self.default_keys_on(KeyPlatform::current())
    }
}

/// "Show in Finder" and its equivalents.
pub const REVEAL: &str = if cfg!(target_os = "macos") {
    "Show in Finder"
} else if cfg!(windows) {
    "Show in Explorer"
} else {
    "Show in File Manager"
};

/// The Permissions column's command: Windows has attributes rather than permissions.
pub const PERMISSIONS_COLUMN: &str =
    if cfg!(windows) { "Show or Hide the Attributes Column" } else { "Show or Hide the Permissions Column" };

/// "Empty Trash…" and "Go to Trash", by each platform's name for it.
pub const EMPTY_TRASH: &str = if cfg!(windows) { "Empty the Recycle Bin…" } else { "Empty the Trash…" };
pub const GO_TRASH: &str = if cfg!(windows) { "Go to the Recycle Bin" } else { "Go to the Trash" };

commands! {
    Open "nav.open" "Open", ["enter", "cmd+o", "cmd+down", "alt+down"];
    Into "nav.into" "Open Folder", ["right"];
    Parent "nav.parent" "Go to Parent Folder", ["left", "backspace", "cmd+up", "alt+up"];
    Back "nav.back" "Go Back", ["cmd+[", "alt+left"];
    Forward "nav.forward" "Go Forward", ["cmd+]", "alt+right"];
    GoTo "nav.goto" "Go to Path…", ["cmd+l"];
    Jump "nav.jump" "Jump to Folder…", ["cmd+j"];
    Search "nav.search" "Search in This Folder…", ["cmd+f"];
    ClearJump "nav.clear_jump_history" "Clear Jump History…", [];
    Reload "nav.reload" "Reload", ["cmd+r"];
    Reveal "nav.reveal" REVEAL, ["cmd+enter"];
    ShowOriginal "nav.show_original" "Show Original", [];
    GoTrash "nav.trash" GO_TRASH, [];
    OpenWith "file.open_with" "Open With…", [];
    Info "file.info" "Get Info", ["cmd+i"];
    Palette "app.palette" "Command Palette", ["cmd+k", "cmd+shift+p"];
    OpenSettings "app.settings" "Open Settings File", ["cmd+comma"];
    ReloadSettings "app.reload_settings" "Reload Settings", [];
    MakeDefault "app.make_default" "Make Satchel the Default for Folders…", [];
    RestoreDefault "app.restore_default" "Restore the System Default for Folders…", [];
    Clear "app.clear" "Clear Filter and Selection", ["escape"];

    Copy "file.copy" "Copy", ["cmd+c"];
    Cut "file.cut" "Cut", ["cmd+x"];
    Paste "file.paste" "Paste", ["cmd+v"];
    PasteMove "file.paste_move" "Move Here (Paste and Move)", ["cmd+alt+v"];
    CopyToOther "file.copy_to_other" "Copy to Other Pane", ["f5"];
    MoveToOther "file.move_to_other" "Move to Other Pane", ["f6"];
    CopyPath "file.copy_path" "Copy Path", ["cmd+shift+c", "cmd+alt+c"];
    Duplicate "file.duplicate" "Duplicate", ["cmd+d"];
    MakeLink "file.make_link" "Make Symbolic Link", [];
    Rename "file.rename" "Rename…", ["f2", "cmd+shift+r"];
    BatchRename "file.batch_rename" "Batch Rename…", [];
    NewFolder "file.new_folder" "New Folder", ["cmd+shift+n"];
    NewFile "file.new_file" "New File", ["cmd+alt+n"];
    Compress "file.compress" "Compress", [];
    Extract "file.extract" "Extract Here", [];
    Trash "file.trash" "Move to Trash", ["cmd+backspace", "delete"];
    Delete "file.delete" "Delete Permanently…", ["mac:cmd+alt+backspace", "pc:shift+delete"];
    PutBack "file.put_back" "Put Back", [];
    EmptyTrash "file.empty_trash" EMPTY_TRASH, [];
    Undo "file.undo" "Undo", ["cmd+z"];
    Redo "file.redo" "Redo", ["cmd+shift+z", "win:ctrl+y"];
    Cancel "file.cancel" "Cancel File Operation", [];

    Up "select.up" "Move Up", ["up"];
    Down "select.down" "Move Down", ["down"];
    PageUp "select.page_up" "Move Up a Page", ["pageup"];
    PageDown "select.page_down" "Move Down a Page", ["pagedown"];
    First "select.first" "Move to First", ["home"];
    Last "select.last" "Move to Last", ["end"];
    ExtendUp "select.extend_up" "Extend Selection Up", ["shift+up"];
    ExtendDown "select.extend_down" "Extend Selection Down", ["shift+down"];
    ExtendPageUp "select.extend_page_up" "Extend Selection Up a Page", ["shift+pageup"];
    ExtendPageDown "select.extend_page_down" "Extend Selection Down a Page", ["shift+pagedown"];
    ExtendFirst "select.extend_first" "Extend Selection to First", ["shift+home"];
    ExtendLast "select.extend_last" "Extend Selection to Last", ["shift+end"];
    SelectAll "select.all" "Select All", ["cmd+a"];
    SelectPattern "select.pattern" "Select by Pattern…", ["cmd+s"];
    InvertSelection "select.invert" "Invert Selection", ["cmd+shift+i"];
    SelectKind "select.kind" "Select All of This Kind", [];

    Hidden "view.hidden" "Show or Hide Hidden Files", ["cmd+.", "cmd+shift+."];
    SortName "view.sort_name" "Sort by Name", [];
    SortKind "view.sort_kind" "Sort by Kind", [];
    SortSize "view.sort_size" "Sort by Size", [];
    SortModified "view.sort_modified" "Sort by Date Modified", [];
    SortCreated "view.sort_created" "Sort by Date Created", [];
    SortReverse "view.sort_reverse" "Reverse Sort Order", [];
    FoldersFirst "view.folders_first" "Keep Folders First (Toggle)", [];
    TogglePreview "view.preview" "Quick Preview (Toggle)", ["space"];
    ClosePreview "view.close_preview" "Close Preview", [];
    AsList "view.as_list" "As List", ["cmd+alt+1"];
    AsIcons "view.as_icons" "As Icons", ["cmd+alt+2"];
    AsColumns "view.as_columns" "As Columns", ["cmd+alt+3"];
    AsTiles "view.as_tiles" "As Tiles", ["cmd+alt+4"];
    LargerIcons "view.larger_icons" "Larger Icons", ["cmd+plus", "cmd+equal"];
    SmallerIcons "view.smaller_icons" "Smaller Icons", ["cmd+minus"];
    ColumnKind "view.column_kind" "Show or Hide the Kind Column", [];
    ColumnExtension "view.column_extension" "Show or Hide the Extension Column", [];
    ColumnSize "view.column_size" "Show or Hide the Size Column", [];
    ColumnModified "view.column_modified" "Show or Hide the Date Modified Column", [];
    ColumnCreated "view.column_created" "Show or Hide the Date Created Column", [];
    ColumnPermissions "view.column_permissions" PERMISSIONS_COLUMN, [];
    ResetColumns "view.reset_columns" "Reset Columns", [];
    ColumnOptions "view.column_options" "Column Options…", [];

    NewWindow "window.new" "New Window", ["cmd+n"];
    OpenInNewWindow "window.open_folder" "Open in New Window", [];
    NewTab "tab.new" "New Tab", ["cmd+t"];
    CloseTab "tab.close" "Close Tab", ["cmd+w"];
    NextTab "tab.next" "Next Tab", ["ctrl+tab", "cmd+shift+]"];
    PrevTab "tab.previous" "Previous Tab", ["ctrl+shift+tab", "cmd+shift+["];
    ReopenTab "tab.reopen" "Reopen Closed Tab", ["cmd+shift+t"];
    MoveTabLeft "tab.move_left" "Move Tab Left", ["ctrl+shift+pageup"];
    MoveTabRight "tab.move_right" "Move Tab Right", ["ctrl+shift+pagedown"];
    MoveTabOther "tab.move_to_other_pane" "Move Tab to Other Pane", ["cmd+shift+backslash"];
    Split "pane.split" "Split View (Toggle)", ["cmd+backslash"];
    OtherPane "pane.switch" "Switch to Other Pane", ["tab"];

    SidebarAdd "sidebar.add" "Add to Sidebar", [];
    SidebarRemove "sidebar.remove" "Remove from Sidebar", [];
    Eject "volume.eject" "Eject", ["cmd+e"];

    Place1 "place.1" "Go to Place 1", ["cmd+1"];
    Place2 "place.2" "Go to Place 2", ["cmd+2"];
    Place3 "place.3" "Go to Place 3", ["cmd+3"];
    Place4 "place.4" "Go to Place 4", ["cmd+4"];
    Place5 "place.5" "Go to Place 5", ["cmd+5"];
    Place6 "place.6" "Go to Place 6", ["cmd+6"];
    Place7 "place.7" "Go to Place 7", ["cmd+7"];
    Place8 "place.8" "Go to Place 8", ["cmd+8"];
    Place9 "place.9" "Go to Place 9", ["cmd+9"];
}

impl Cmd {
    pub fn from_id(id: &str) -> Option<Cmd> {
        Cmd::ALL.iter().copied().find(|c| c.id() == id)
    }

    /// For `place.N`, the sidebar place's index (N − 1).
    pub fn place(self) -> Option<usize> {
        let i = [Cmd::Place1, Cmd::Place2, Cmd::Place3, Cmd::Place4, Cmd::Place5, Cmd::Place6, Cmd::Place7, Cmd::Place8, Cmd::Place9]
            .iter()
            .position(|&c| c == self)?;
        Some(i)
    }
}

/// Reads a chord in settings syntax: `cmd` (also `mod`, `command`, `primary`) is ⌘ on macOS and
/// Ctrl elsewhere; `ctrl`, `alt` (`option`), `shift` and `super` (`win`, `meta`) name those
/// keys. Keys are a character (`a`, `.`, `[`) or a name (`enter`, `backspace`, `delete`,
/// `up`, `pageup`, `f2`, `comma`, `space`; see Rune's `Chord`). Case and spaces don't matter.
///
/// Rune's own syntax reads `cmd` as the ⌘/logo key on every platform, which would make
/// `cmd+c` the Windows key on Windows, so `cmd` becomes Rune's `mod` here.
pub fn parse_chord(s: &str) -> Result<Chord, String> {
    let lower = s.trim().to_lowercase();
    let spelled: Vec<&str> = lower.split('+').map(str::trim).map(|p| if matches!(p, "cmd" | "command") { "mod" } else { p }).collect();
    let spelled = spelled.join("+");
    spelled.parse::<Chord>().map_err(|_| format!("'{s}' isn't a key chord (try \"cmd+shift+n\", \"delete\" or \"f2\")"))
}

/// A chord in settings syntax, as `parse_chord` reads it back.
pub fn chord_spec(c: &Chord) -> String {
    let s = c.to_string();
    let mut parts: Vec<&str> = s.split('+').collect();
    // Rune writes "mod" for the primary modifier; settings call it "cmd".
    for p in &mut parts {
        if *p == "mod" {
            *p = "cmd";
        }
    }
    parts.join("+")
}

/// Which keys run which command. Built from the defaults and the settings file
/// (`config::Config`); each chord belongs to at most one command.
#[derive(Clone, Debug)]
pub struct Keymap {
    binds: Vec<(Chord, Cmd)>,
}

impl Keymap {
    /// The default keys.
    pub fn defaults() -> Keymap {
        let mut binds = Vec::with_capacity(80);
        for &cmd in Cmd::ALL {
            for k in cmd.default_keys() {
                // A test parses every default.
                if let Ok(c) = parse_chord(k) {
                    binds.push((c, cmd));
                }
            }
        }
        Keymap { binds }
    }

    pub(crate) fn from_binds(binds: Vec<(Chord, Cmd)>) -> Keymap {
        Keymap { binds }
    }

    /// The command whose chord `matches` says the key is.
    pub fn find(&self, matches: impl Fn(&Chord) -> bool) -> Option<Cmd> {
        self.binds.iter().find(|(c, _)| matches(c)).map(|&(_, cmd)| cmd)
    }

    /// `cmd`'s chords, the main one first.
    pub fn chords(&self, cmd: Cmd) -> impl Iterator<Item = &Chord> {
        self.binds.iter().filter(move |b| b.1 == cmd).map(|b| &b.0)
    }

    /// `cmd`'s main chord as this platform draws it ("⇧⌘N", "Ctrl+Shift+N"), or "".
    pub fn label(&self, cmd: Cmd) -> String {
        self.chords(cmd).next().map(Chord::label).unwrap_or_default()
    }

    /// `cmd`'s main chord as a native menu's key equivalent.
    pub fn shortcut(&self, cmd: Cmd) -> Option<rune::Shortcut> {
        self.chords(cmd).next()?.to_string().parse().ok()
    }
}

// ---- palette matching ---------------------------------------------------------------------------

/// How well `query` matches `text`, higher is better; `None` when it doesn't. The query's
/// characters must appear in order (case-insensitive, spaces ignored), the first one starting
/// a word. A prefix of the whole
/// text ranks first, then the query starting a word ("folder" in "New Folder"), then a scatter
/// that lands on word starts ("nf" for New Folder) over one that doesn't, consecutive letters
/// over gaps, and shorter texts over longer ones.
pub fn score(query: &str, text: &str) -> Option<i32> {
    let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let (n, m) = (t.len(), q.len());
    if m > n {
        return None;
    }
    let start = |j: usize| j == 0 || !t[j - 1].is_alphanumeric();
    let bonus = |j: usize| 1 + if start(j) { 10 } else { 0 };
    // best[j]: the best score with the query so far matched and its last character at t[j].
    const NONE: i32 = i32::MIN / 4;
    // The first letter must start a word, so "ren" finds Rename but not "Move Here".
    let mut best: Vec<i32> = (0..n).map(|j| if t[j] == q[0] && start(j) { bonus(j) } else { NONE }).collect();
    let mut next = vec![NONE; n];
    for &qc in &q[1..] {
        let mut before = NONE; // max of best[..j - 1]: a match with a gap
        for j in 0..n {
            if j >= 2 {
                before = before.max(best[j - 2]);
            }
            next[j] = NONE;
            if t[j] != qc {
                continue;
            }
            let run = if j >= 1 { best[j - 1] + 6 } else { NONE };
            let from = run.max(before - 2);
            if from > NONE / 2 {
                next[j] = from + bonus(j);
            }
        }
        std::mem::swap(&mut best, &mut next);
    }
    let fuzzy = best.into_iter().max().filter(|&s| s > NONE / 2)?;
    let lower = text.to_lowercase();
    let whole = query.trim().to_lowercase();
    let tier = if lower.starts_with(&whole) {
        2000
    } else if lower.match_indices(&whole).any(|(i, _)| i == 0 || !lower[..i].ends_with(char::is_alphanumeric)) {
        1000
    } else {
        0
    };
    Some(tier + fuzzy * 4 - n as i32)
}

/// The command id closest to `id` by edit distance, for "did you mean" in settings errors.
pub fn closest_id(id: &str) -> Option<&'static str> {
    let d = |a: &str, b: &str| {
        let b: Vec<char> = b.chars().collect();
        let mut row: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.chars().enumerate() {
            let mut prev = row[0];
            row[0] = i + 1;
            for j in 0..b.len() {
                let cur = row[j + 1];
                row[j + 1] = (prev + usize::from(ca != b[j])).min(row[j] + 1).min(row[j + 1] + 1);
                prev = cur;
            }
        }
        row[b.len()]
    };
    Cmd::ALL.iter().map(|c| (d(id, c.id()), c.id())).filter(|&(n, _)| n <= 3).min_by_key(|&(n, _)| n).map(|(_, id)| id)
}

/// The command table as Markdown, for the README (kept in sync by a test).
#[cfg(test)]
pub fn markdown_table() -> String {
    let mut s = String::from("| Command | macOS | Windows, Linux | Id |\n| --- | --- | --- | --- |\n");
    let keys = |c: Cmd, p: KeyPlatform| -> String {
        c.default_keys_on(p).iter().filter_map(|k| parse_chord(k).ok()).map(|c| c.label_on(p)).collect::<Vec<_>>().join(", ")
    };
    for &cmd in Cmd::ALL {
        let row = match cmd.place() {
            Some(0) => ["Go to sidebar place 1…9".into(), "⌘1 … ⌘9".into(), "Ctrl+1 … Ctrl+9".into(), "`place.1` … `place.9`".into()],
            Some(_) => continue,
            None => {
                // Titles that name each platform's own thing, written for all of them.
                let title = match cmd {
                    Cmd::Reveal => "Show in Finder / Explorer / file manager",
                    Cmd::GoTrash => "Go to the Trash / Recycle Bin",
                    Cmd::EmptyTrash => "Empty the Trash / Recycle Bin…",
                    Cmd::ColumnPermissions => "Show or Hide the Permissions (Windows: Attributes) Column",
                    _ => cmd.title(),
                };
                [title.into(), keys(cmd, KeyPlatform::Mac), keys(cmd, KeyPlatform::Windows), format!("`{}`", cmd.id())]
            }
        };
        s.push_str(&format!("| {} |\n", row.join(" | ")));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Key, KeyEvent, Mods};

    const PLATFORMS: [KeyPlatform; 3] = [KeyPlatform::Mac, KeyPlatform::Windows, KeyPlatform::Linux];

    #[test]
    fn ids_are_unique_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for &cmd in Cmd::ALL {
            assert!(seen.insert(cmd.id()), "duplicate id {}", cmd.id());
            assert!(cmd.id().split_once('.').is_some_and(|(a, b)| !a.is_empty() && !b.is_empty()), "{}", cmd.id());
            assert!(cmd.id().chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_'), "{}", cmd.id());
            assert!(!cmd.title().is_empty());
            assert_eq!(Cmd::from_id(cmd.id()), Some(cmd));
        }
    }

    #[test]
    fn default_keys_parse_and_never_conflict() {
        for platform in PLATFORMS {
            let mut all: Vec<(Chord, Cmd)> = Vec::new();
            for &cmd in Cmd::ALL {
                for k in cmd.default_keys_on(platform) {
                    let c = parse_chord(k).unwrap_or_else(|e| panic!("{}: {e}", cmd.id()));
                    assert_eq!(parse_chord(&chord_spec(&c)), Ok(c), "{k} round trip");
                    if let Some((_, other)) = all.iter().find(|(o, _)| o.same_keys(&c, platform)) {
                        panic!("{k} is bound to both {} and {} on {platform:?}", other.id(), cmd.id());
                    }
                    all.push((c, cmd));
                }
            }
        }
    }

    #[test]
    fn cmd_means_the_primary_modifier() {
        let c = parse_chord("cmd+shift+n").unwrap();
        let ctrl = parse_chord("ctrl+shift+n").unwrap();
        let sup = parse_chord("super+shift+n").unwrap();
        assert!(c.same_keys(&ctrl, KeyPlatform::Linux) && c.same_keys(&ctrl, KeyPlatform::Windows));
        assert!(!c.same_keys(&ctrl, KeyPlatform::Mac));
        assert!(c.same_keys(&sup, KeyPlatform::Mac) && !c.same_keys(&sup, KeyPlatform::Linux));
        assert_eq!(c.label_on(KeyPlatform::Mac), "⇧⌘N");
        assert_eq!(c.label_on(KeyPlatform::Linux), "Ctrl+Shift+N");
        assert_eq!(parse_chord(" Cmd + Backspace ").unwrap().label_on(KeyPlatform::Mac), "⌘⌫");
        assert_eq!(parse_chord("delete").unwrap().label_on(KeyPlatform::Windows), "Delete");
        assert_ne!(parse_chord("delete").unwrap(), parse_chord("backspace").unwrap());
        assert_eq!(parse_chord("option+left").unwrap(), parse_chord("alt+left").unwrap());
        assert!(parse_chord("cmd+").is_err() && parse_chord("hyper+x").is_err() && parse_chord("").is_err());
        assert!(parse_chord("cmd+nope").unwrap_err().contains("'cmd+nope'"));
    }

    #[test]
    fn keymap_finds_keys() {
        let km = Keymap::defaults();
        let key = |key: Key, mods: Mods| {
            let ev = KeyEvent { key, mods };
            km.find(|c| c.matches_on(&ev, KeyPlatform::current()))
        };
        let cmd = Mods { cmd: true, ..Mods::default() };
        let shift = Mods { shift: true, ..Mods::default() };
        assert_eq!(key(Key::Down, Mods::default()), Some(Cmd::Down));
        assert_eq!(key(Key::Down, shift), Some(Cmd::ExtendDown));
        assert_eq!(key(Key::Down, cmd), Some(Cmd::Open));
        assert_eq!(key(Key::Delete, Mods::default()), Some(Cmd::Trash));
        assert_eq!(key(Key::Backspace, cmd), Some(Cmd::Trash));
        assert_eq!(key(Key::Backspace, Mods::default()), Some(Cmd::Parent));
        assert_eq!(key(Key::Command('k'), cmd), Some(Cmd::Palette));
        assert_eq!(key(Key::Command('p'), Mods { shift: true, ..cmd }), Some(Cmd::Palette));
        assert_eq!(key(Key::Command('3'), cmd), Some(Cmd::Place3));
        assert_eq!(key(Key::Function(2), Mods::default()), Some(Cmd::Rename));
        assert_eq!(key(Key::Text("j".into()), Mods::default()), None);
        assert_eq!(km.label(Cmd::Trash), parse_chord("cmd+backspace").unwrap().label());
        assert_eq!(km.label(Cmd::ReloadSettings), "");
    }

    /// Delete Permanently follows each platform: ⌥⌘⌫ on macOS (Finder), Shift+Delete on
    /// Windows and Linux (Explorer, Files, Dolphin), and nothing else on either.
    #[test]
    fn delete_permanently_has_each_platforms_key() {
        let label = |p: KeyPlatform| Cmd::Delete.default_keys_on(p).iter().map(|k| parse_chord(k).unwrap().label_on(p)).collect::<Vec<_>>();
        assert_eq!(label(KeyPlatform::Mac), ["⌥⌘⌫"]);
        assert_eq!(label(KeyPlatform::Windows), ["Shift+Delete"]);
        assert_eq!(label(KeyPlatform::Linux), ["Shift+Delete"]);
        assert_eq!(Cmd::Trash.default_keys_on(KeyPlatform::Mac), Cmd::Trash.default_keys_on(KeyPlatform::Linux));
        assert!(Cmd::Delete.permanent() && Cmd::Delete.title().ends_with('…'));
        let km = Keymap::defaults();
        let mods = |shift, alt, cmd| Mods { shift, alt, cmd, ..Mods::default() };
        let (key, mods) =
            if cfg!(target_os = "macos") { (Key::Backspace, mods(false, true, true)) } else { (Key::Delete, mods(true, false, false)) };
        let ev = KeyEvent { key, mods };
        assert_eq!(km.find(|c| c.matches_on(&ev, KeyPlatform::current())), Some(Cmd::Delete));
    }

    #[test]
    fn ranking_prefers_prefixes_and_word_starts() {
        let rank = |q: &str| {
            let mut v: Vec<(i32, &str)> = Cmd::ALL.iter().filter_map(|c| Some((score(q, c.title())?, c.title()))).collect();
            v.sort_by_key(|a| std::cmp::Reverse(a.0));
            v.into_iter().map(|(_, t)| t).collect::<Vec<_>>()
        };
        assert_eq!(rank("ren")[0], "Rename…");
        let mut top = rank("new")[..4].to_vec();
        top.sort();
        assert_eq!(top, ["New File", "New Folder", "New Tab", "New Window"]);
        assert_eq!(rank("folder")[0], "New Folder", "a word start beats a scatter");
        let mut top = rank("nf")[..2].to_vec();
        top.sort();
        assert_eq!(top, ["New File", "New Folder"], "initials");
        assert_eq!(rank("nfo")[0], "New Folder");
        assert_eq!(rank("link")[0], "Make Symbolic Link");
        // The everyday command first; "Go to the Trash" and "Empty the Trash…" follow.
        assert_eq!(rank("trash")[0], "Move to Trash");
        if !cfg!(windows) {
            assert_eq!(rank("trash")[..3], ["Move to Trash", "Go to the Trash", "Empty the Trash…"]);
        }
        let mut top = rank("settings")[..2].to_vec();
        top.sort();
        assert_eq!(top, ["Open Settings File", "Reload Settings"]);
        assert!(rank("zzz").is_empty());
        assert!(!rank("ren").contains(&"Move Here (Paste and Move)"), "the first letter starts a word");
        assert_eq!(score("", "Anything"), Some(0));
        assert!(score("Open", "Open").unwrap() > score("Open", "Open Folder").unwrap(), "shorter wins a tie");
        assert!(score("dup", "file.duplicate").is_some(), "ids match too");
    }

    /// What dispatch and the palette cost: `cargo test --release -- --ignored --nocapture dispatch_speed`.
    #[test]
    #[ignore = "benchmark"]
    fn dispatch_speed() {
        let km = Keymap::defaults();
        let n = 10_000;
        let time = |name: &str, f: &dyn Fn()| {
            let t = std::time::Instant::now();
            for _ in 0..n {
                f();
            }
            println!("{name}: {:.2} µs", t.elapsed().as_secs_f64() * 1e6 / n as f64);
        };
        let down = KeyEvent { key: Key::Down, mods: Mods::default() };
        let text = KeyEvent { key: Key::Text("q".into()), mods: Mods::default() };
        time("find ↓ (early in the table)", &|| assert!(km.find(|c| c.matches_on(&down, KeyPlatform::current())).is_some()));
        time("find an unbound letter (whole table)", &|| assert!(km.find(|c| c.matches_on(&text, KeyPlatform::current())).is_none()));
        time("9 sidebar labels", &|| {
            for &c in &Cmd::ALL[Cmd::ALL.len() - 9..] {
                std::hint::black_box(km.label(c));
            }
        });
        time("rank every command for \"new f\"", &|| {
            for &c in Cmd::ALL {
                std::hint::black_box(score("new f", c.title()));
            }
        });
        time("Keymap::defaults (startup)", &|| {
            std::hint::black_box(Keymap::defaults());
        });
    }

    #[test]
    fn closest_id_suggests_typos() {
        assert_eq!(closest_id("file.trahs"), Some("file.trash"));
        assert_eq!(closest_id("totally.wrong"), None);
    }

    /// The README's key table is generated from the registry. To refresh it after changing a
    /// command: `SATCHEL_WRITE_README=1 cargo test readme`.
    #[test]
    fn readme_key_table_matches_registry() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/README.md");
        let readme = std::fs::read_to_string(path).unwrap().replace("\r\n", "\n");
        let (start, end) = ("<!-- commands:start -->\n", "<!-- commands:end -->");
        let a = readme.find(start).expect("README has the commands:start marker") + start.len();
        let b = readme.find(end).expect("README has the commands:end marker");
        let table = markdown_table();
        if readme[a..b] != table {
            if std::env::var_os("SATCHEL_WRITE_README").is_some() {
                std::fs::write(path, format!("{}{table}{}", &readme[..a], &readme[b..])).unwrap();
            } else {
                panic!("README's key table is out of date: run `SATCHEL_WRITE_README=1 cargo test readme`");
            }
        }
    }
}
