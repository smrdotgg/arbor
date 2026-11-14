//! The settings file: preferences and keybindings in one TOML file (NORTHSTAR: "Settings and
//! keybindings in one config file"). It's user data, so storing it is allowed.
//!
//! - macOS `~/Library/Application Support/Satchel/config.toml`, Windows `%APPDATA%\Satchel\config.toml`,
//!   elsewhere `$XDG_CONFIG_HOME/satchel/config.toml` (default `~/.config/satchel/config.toml`).
//!   `RUNE_DIRS_ROOT` moves it under `<root>/config/` for tests (Rune's `dirs`).
//! - Read once at startup, on the thread that reads the first folder (`app::read_first_folder`),
//!   so it overlaps the platform's launch; the window waits for it only briefly
//!   (`app::FIRST_PAINT_WAIT`) and applies it when it lands. Nothing watches it: "Reload
//!   Settings" reads it again, on a worker. Only a regular file up to [`MAX_BYTES`] is read,
//!   without ever blocking on the open (`read_text`).
//! - Errors never stop Satchel: each is reported with its line, everything valid applies, and the
//!   rest keeps its default. A line that doesn't parse as TOML is skipped and the rest re-read.
//! - Keybindings: `[keys]` maps a command id to its chords, which replace the defaults (`[]`
//!   unbinds; a list whose every chord is a typo keeps the defaults). A chord bound to two
//!   commands goes to the one bound later: a binding in the file takes the chord from another
//!   command's defaults, and a later line takes it from an earlier line, which is reported.
//!   Chords are compared as they'll be pressed here, so off macOS `cmd+g` and `ctrl+g` are one
//!   chord. Later wins everywhere (as in VS Code and Vim), so adding a line at the bottom always
//!   does what it says.

use std::path::{Path, PathBuf};

use rune::{Chord, KeyPlatform};
use toml_edit::{Document, Item, TableLike};

use crate::commands::{self, Cmd, Keymap};
use crate::fs::{self, Sort, SortKey};

/// What the settings file sets, beyond keys. Defaults are what Satchel does without one.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Show hidden files when a window opens.
    pub show_hidden: bool,
    pub sort: Sort,
    /// The folder to open when none is named on the command line.
    pub start_folder: Option<PathBuf>,
    /// Sidebar places, replacing the automatic list.
    pub places: Option<Vec<PathBuf>>,
    /// `[jump] enabled`: record visited folders for Jump to Folder.
    pub jump_enabled: bool,
    /// `[jump] exclude`: globs of folders never recorded.
    pub jump_exclude: Vec<String>,
    /// `[select] case_sensitive`: how Select by Pattern compares; `None` is the platform's way.
    pub case_sensitive: Option<bool>,
    /// `[search] cross_devices`: a search walks into other file systems mounted inside it.
    pub search_cross_devices: bool,
    /// `[view] default`: how new tabs show their folder.
    pub view_default: crate::grid::ViewMode,
    /// `[view] icon_size`: how big the icon view's icons start.
    pub icon_size: crate::grid::IconSize,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            show_hidden: false,
            sort: Sort::new(SortKey::Name, false),
            start_folder: None,
            places: None,
            jump_enabled: true,
            jump_exclude: Vec::new(),
            case_sensitive: None,
            search_cross_devices: false,
            view_default: crate::grid::ViewMode::List,
            icon_size: crate::grid::IconSize::Medium,
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub settings: Settings,
    pub keymap: Keymap,
    /// Problems found, each "line 12: …".
    pub errors: Vec<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config { settings: Settings::default(), keymap: Keymap::defaults(), errors: Vec::new() }
    }
}

const FOLDER: &str = if cfg!(any(target_os = "macos", windows)) { "Satchel" } else { "satchel" };

/// Where the settings file lives (it may not exist).
pub fn path() -> Option<PathBuf> {
    rune::dirs::AppDirs::new(FOLDER).map(|d| d.config().join("config.toml"))
}

impl Config {
    /// The settings file, or the defaults when there's none.
    pub fn load() -> Config {
        match path() {
            Some(p) => Config::read(&p),
            None => Config::default(),
        }
    }

    /// Reads and parses the file at `path`; anything wrong with it is an error in the result
    /// and the defaults apply. Never blocks on something that isn't a file (see [`read_text`]).
    pub fn read(path: &Path) -> Config {
        match read_text(path) {
            Ok(Some(text)) => Config::parse(&text, KeyPlatform::current()),
            Ok(None) => Config::default(),
            Err(e) => Config { errors: vec![e], ..Config::default() },
        }
    }

    /// One line for the status bar: "Settings: line 12: unknown command 'file.trahs' (+1 more)".
    pub fn summary(&self) -> Option<String> {
        let first = self.errors.first()?;
        let more = if self.errors.len() > 1 { format!(" (+{} more)", self.errors.len() - 1) } else { String::new() };
        Some(format!("Settings: {first}{more}"))
    }

    /// Reads settings from `text`. Chord conflicts are checked as on `platform`.
    pub fn parse(text: &str, platform: KeyPlatform) -> Config {
        let mut errors = Vec::new();
        let Some((doc, src)) = parse_lenient(text, &mut errors) else {
            return Config { errors, ..Config::default() };
        };
        let line = |span: Option<std::ops::Range<usize>>| span.map_or(0, |s| line_of(&src, s.start));
        let mut settings = Settings::default();
        let mut keys: Vec<(Cmd, Vec<Chord>, usize)> = Vec::new();
        let root = doc.as_table();
        for (name, item) in root.iter() {
            let at = line(root.key(name).and_then(|k| k.span()));
            match (name, item.as_table_like()) {
                ("general", Some(t)) => general(t, &mut settings, &mut errors, &line),
                ("jump", Some(t)) => jump(t, &mut settings, &mut errors, &line),
                ("select", Some(t)) => select(t, &mut settings, &mut errors, &line),
                ("search", Some(t)) => search(t, &mut settings, &mut errors, &line),
                ("view", Some(t)) => view(t, &mut settings, &mut errors, &line),
                ("keys", Some(t)) => keys = key_table(t, &mut errors, &line),
                ("general" | "jump" | "search" | "select" | "view" | "keys", None) => {
                    errors.push(format!("line {at}: '{name}' should be a section: [{name}]"))
                }
                (_, _) if GENERAL.contains(&name) => {
                    errors.push(format!("line {at}: '{name}' belongs under [general]"));
                }
                _ => errors.push(format!(
                    "line {at}: unknown section '{name}' (sections are [general], [jump], [search], [select], [view] and [keys])"
                )),
            }
        }
        let keymap = build_keymap(&keys, platform, &mut errors);
        // Syntax errors are found first; list everything in file order.
        errors.sort_by_key(|e| e.strip_prefix("line ").and_then(|r| r.split(':').next()).and_then(|n| n.parse::<usize>().ok()));
        Config { settings, keymap, errors }
    }
}

/// The largest settings file Satchel reads. The template is about 6 KB, and one binding every
/// command many times over is still far under this; a bigger file is a mistake, and reading
/// and parsing it would hold up startup or Reload.
pub const MAX_BYTES: u64 = 1 << 20;

/// The settings file's text, `None` when there's none. Only a regular file (or a link to one)
/// up to [`MAX_BYTES`] is read: a FIFO or device there could block forever and a huge file would
/// take long, so both are refused with a message. The open doesn't block either (`O_NONBLOCK`,
/// in case the file is swapped for a FIFO after the check), and what it opened is checked again.
pub(crate) fn read_text(path: &Path) -> Result<Option<String>, String> {
    use std::io::Read;
    let name = fs::pretty(path);
    let not_a_file = || format!("{name} isn't a regular file, so it was ignored");
    let too_big =
        |n: u64| format!("{name} is {} KB, more than the {} KB a settings file can be, so it was ignored", n / 1024, MAX_BYTES / 1024);
    let m = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("can't read {name}: {e}")),
    };
    if !m.is_file() {
        return Err(not_a_file());
    }
    if m.len() > MAX_BYTES {
        return Err(too_big(m.len()));
    }
    let mut open = std::fs::OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut open, libc::O_NONBLOCK);
    let f = open.open(path).map_err(|e| format!("can't read {name}: {e}"))?;
    if !f.metadata().is_ok_and(|m| m.is_file()) {
        return Err(not_a_file());
    }
    let mut bytes = Vec::new();
    f.take(MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|e| format!("can't read {name}: {e}"))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(too_big(bytes.len() as u64));
    }
    String::from_utf8(bytes).map(Some).map_err(|_| format!("{name} isn't text (UTF-8), so it was ignored"))
}

const GENERAL: [&str; 6] = ["show_hidden", "sort", "sort_descending", "folders_first", "start_folder", "places"];

fn general(t: &dyn TableLike, s: &mut Settings, errors: &mut Vec<String>, line: &dyn Fn(Option<std::ops::Range<usize>>) -> usize) {
    let mut desc = None;
    for (name, item) in t.iter() {
        let at = line(t.key(name).and_then(|k| k.span()));
        let mut bad = |want: &str| errors.push(format!("line {at}: {name} should be {want}"));
        match name {
            "show_hidden" => match item.as_bool() {
                Some(b) => s.show_hidden = b,
                None => bad("true or false"),
            },
            "sort_descending" => match item.as_bool() {
                Some(b) => desc = Some(b),
                None => bad("true or false"),
            },
            "folders_first" => match item.as_bool() {
                Some(b) => s.sort.dirs_first = b,
                None => bad("true or false"),
            },
            "sort" => match item.as_str().and_then(sort_key) {
                Some(k) => s.sort.key = k,
                None => bad("\"name\", \"kind\", \"size\", \"modified\" or \"created\""),
            },
            "start_folder" => match item.as_str() {
                Some("") => s.start_folder = None,
                Some(p) => s.start_folder = Some(fs::expand(p)),
                None => bad("a folder path in quotes, like \"~/Projects\""),
            },
            "places" => match item.as_array().map(|a| a.iter().map(|v| v.as_str()).collect::<Option<Vec<_>>>()) {
                Some(Some(v)) if v.is_empty() => s.places = None,
                Some(Some(v)) => s.places = Some(v.into_iter().map(fs::expand).collect()),
                _ => bad("a list of folder paths, like [\"~\", \"~/Projects\"]"),
            },
            _ => errors.push(format!("line {at}: unknown setting '{name}' (settings are {})", GENERAL.join(", "))),
        }
    }
    // Newest and largest first feel right; names and kinds read A→Z (as clicking a header).
    s.sort.desc = desc.unwrap_or(matches!(s.sort.key, SortKey::Size | SortKey::Modified | SortKey::Created));
}

fn jump(t: &dyn TableLike, s: &mut Settings, errors: &mut Vec<String>, line: &dyn Fn(Option<std::ops::Range<usize>>) -> usize) {
    for (name, item) in t.iter() {
        let at = line(t.key(name).and_then(|k| k.span()));
        let mut bad = |want: &str| errors.push(format!("line {at}: {name} should be {want}"));
        match name {
            "enabled" => match item.as_bool() {
                Some(b) => s.jump_enabled = b,
                None => bad("true or false"),
            },
            "exclude" => match item.as_array().map(|a| a.iter().map(|v| v.as_str()).collect::<Option<Vec<_>>>()) {
                Some(Some(v)) => {
                    s.jump_exclude.clear();
                    for p in v {
                        match crate::pattern::Glob::path(p, crate::pattern::Case::platform()) {
                            Ok(_) => s.jump_exclude.push(p.to_string()),
                            Err(e) => errors.push(format!("line {at}: exclude \"{p}\": {e}")),
                        }
                    }
                }
                _ => bad("a list of folder patterns, like [\"/tmp/**\", \"**/node_modules\"]"),
            },
            _ => errors.push(format!("line {at}: unknown setting '{name}' under [jump] (settings are enabled, exclude)")),
        }
    }
}

fn select(t: &dyn TableLike, s: &mut Settings, errors: &mut Vec<String>, line: &dyn Fn(Option<std::ops::Range<usize>>) -> usize) {
    for (name, item) in t.iter() {
        let at = line(t.key(name).and_then(|k| k.span()));
        match (name, item.as_bool()) {
            ("case_sensitive", Some(b)) => s.case_sensitive = Some(b),
            ("case_sensitive", None) => errors.push(format!("line {at}: case_sensitive should be true or false")),
            _ => errors.push(format!("line {at}: unknown setting '{name}' under [select] (settings are case_sensitive)")),
        }
    }
}

fn search(t: &dyn TableLike, s: &mut Settings, errors: &mut Vec<String>, line: &dyn Fn(Option<std::ops::Range<usize>>) -> usize) {
    for (name, item) in t.iter() {
        let at = line(t.key(name).and_then(|k| k.span()));
        match (name, item.as_bool()) {
            ("cross_devices", Some(b)) => s.search_cross_devices = b,
            ("cross_devices", None) => errors.push(format!("line {at}: cross_devices should be true or false")),
            _ => errors.push(format!("line {at}: unknown setting '{name}' under [search] (settings are cross_devices)")),
        }
    }
}

fn view(t: &dyn TableLike, s: &mut Settings, errors: &mut Vec<String>, line: &dyn Fn(Option<std::ops::Range<usize>>) -> usize) {
    for (name, item) in t.iter() {
        let at = line(t.key(name).and_then(|k| k.span()));
        match name {
            "default" => match item.as_str().and_then(crate::grid::ViewMode::named) {
                Some(v) => s.view_default = v,
                None => errors.push(format!("line {at}: default should be \"list\", \"icons\", \"columns\" or \"tiles\"")),
            },
            "icon_size" => match item.as_str().and_then(crate::grid::IconSize::named) {
                Some(v) => s.icon_size = v,
                None => errors.push(format!("line {at}: icon_size should be \"small\", \"medium\" or \"large\"")),
            },
            _ => errors.push(format!("line {at}: unknown setting '{name}' under [view] (settings are default, icon_size)")),
        }
    }
}

fn sort_key(s: &str) -> Option<SortKey> {
    Some(match s {
        "name" => SortKey::Name,
        "kind" => SortKey::Kind,
        "size" => SortKey::Size,
        "modified" | "date" => SortKey::Modified,
        "created" => SortKey::Created,
        _ => return None,
    })
}

fn key_table(
    t: &dyn TableLike,
    errors: &mut Vec<String>,
    line: &dyn Fn(Option<std::ops::Range<usize>>) -> usize,
) -> Vec<(Cmd, Vec<Chord>, usize)> {
    let mut out = Vec::new();
    for (id, item) in t.iter() {
        let at = line(t.key(id).and_then(|k| k.span()));
        let Some(cmd) = Cmd::from_id(id) else {
            let hint = commands::closest_id(id).map(|c| format!(" (did you mean '{c}'?)")).unwrap_or_default();
            errors.push(format!("line {at}: unknown command '{id}'{hint}"));
            continue;
        };
        let specs: Option<Vec<&str>> = match item {
            Item::Value(v) if v.as_str().is_some() => v.as_str().map(|s| vec![s]),
            _ => item.as_array().and_then(|a| a.iter().map(|v| v.as_str()).collect()),
        };
        let Some(specs) = specs else {
            errors.push(format!("line {at}: {id} should be a list of keys, like [\"cmd+k\"] ([] unbinds it)"));
            continue;
        };
        let mut chords: Vec<Chord> = Vec::new();
        for &spec in &specs {
            match commands::parse_chord(spec) {
                Ok(c) if !chords.contains(&c) => chords.push(c),
                Ok(_) => {}
                Err(e) => errors.push(format!("line {at}: {e}")),
            }
        }
        // Only `[]` unbinds: a list whose every key is a typo keeps the defaults (the typos are
        // reported above), so a mistake never silently takes a command's keys away.
        if chords.is_empty() && !specs.is_empty() {
            continue;
        }
        out.push((cmd, chords, at));
    }
    out
}

/// Defaults, then the file's bindings in order; each replaces its command's defaults, and a
/// chord bound again goes to the later binding (see the module docs).
fn build_keymap(user: &[(Cmd, Vec<Chord>, usize)], platform: KeyPlatform, errors: &mut Vec<String>) -> Keymap {
    let defaults = Keymap::defaults();
    // (chord, command, line it came from; 0 for a default)
    let mut binds: Vec<(Chord, Cmd, usize)> = Vec::new();
    for &cmd in Cmd::ALL {
        if !user.iter().any(|u| u.0 == cmd) {
            binds.extend(defaults.chords(cmd).map(|&c| (c, cmd, 0)));
        }
    }
    for (cmd, chords, at) in user {
        for c in chords {
            // The same keys on this platform ("cmd+g" and "ctrl+g" off macOS): bound once.
            if binds.iter().any(|b| b.1 == *cmd && b.0.same_keys(c, platform)) {
                continue;
            }
            // Every other command's binding of these keys goes, so the later line really wins.
            let mut told = Vec::new();
            binds.retain(|&(b, other, was)| {
                let clash = other != *cmd && b.same_keys(c, platform);
                if clash && was > 0 && !told.contains(&(other, was)) {
                    told.push((other, was));
                }
                !clash
            });
            for (other, was) in told {
                errors.push(format!(
                    "line {at}: {} is also bound to {} on line {was}; {} gets it",
                    commands::chord_spec(c),
                    other.id(),
                    cmd.id()
                ));
            }
            binds.push((*c, *cmd, *at));
        }
    }
    // Keep each command's chords in the order written, for "the main one first".
    let mut ordered = Vec::with_capacity(binds.len());
    for &cmd in Cmd::ALL {
        ordered.extend(binds.iter().filter(|b| b.1 == cmd).map(|b| (b.0, cmd)));
    }
    Keymap::from_binds(ordered)
}

/// Parses `text` as TOML; a line that doesn't parse is reported, commented out and the rest
/// parsed again (a few times at most). Returns the document and the text it was parsed from.
fn parse_lenient(text: &str, errors: &mut Vec<String>) -> Option<(Document<String>, String)> {
    let mut src = text.to_string();
    let mut last = None;
    for _ in 0..16 {
        match Document::parse(src.clone()) {
            Ok(doc) => return Some((doc, src)),
            Err(e) => {
                let at = e.span().map_or(0, |s| s.start).min(src.len());
                let line = line_of(&src, at);
                let msg = e.message().lines().next().unwrap_or("not valid TOML").trim().trim_end_matches('.').to_string();
                errors.push(format!("line {line}: {msg}"));
                let start = src[..at].rfind('\n').map_or(0, |i| i + 1);
                if last == Some(line) || src[start..].trim_start().starts_with('#') {
                    return None;
                }
                last = Some(line);
                src.insert(start, '#');
            }
        }
    }
    None
}

fn line_of(text: &str, byte: usize) -> usize {
    text.as_bytes()[..byte.min(text.len())].iter().filter(|&&b| b == b'\n').count() + 1
}

/// The file "Open Settings File" creates: every setting and every command with its default
/// keys, all commented out, so it changes nothing until a line is uncommented.
pub fn template() -> String {
    let s = String::from(
        "# Satchel settings. Lines starting with # are comments: remove the # to use a line.\n\
         # Changes apply when you run \"Reload Settings\" from the command palette, or at the next start.\n\
         \n\
         [general]\n\
         # Show hidden files (dotfiles, and files the system marks hidden) in new windows.\n\
         # show_hidden = false\n\
         \n\
         # How folders are sorted: \"name\", \"kind\", \"size\", \"modified\" or \"created\".\n\
         # sort = \"name\"\n\
         # Reverse it. Without this line, size and the dates sort largest and newest first.\n\
         # sort_descending = false\n\
         # Keep folders above files, whatever the sort. \"Folders First\" in the command palette\n\
         # turns it off or on in a window until Satchel quits.\n\
         # folders_first = true\n\
         \n\
         # The folder to open when none is given on the command line. Empty: the current folder,\n\
         # or your home folder when Satchel is started from the desktop.\n\
         # start_folder = \"~\"\n\
         \n\
         # Sidebar places, replacing the automatic list. Empty: the automatic list.\n\
         # places = [\"~\", \"~/Downloads\", \"~/Projects\"]\n\
         \n\
         [jump]\n\
         # Remember the folders you visit, for Jump to Folder (cmd+j). The history is kept in\n\
         # history.log beside this file; \"Clear Jump History\" forgets it.\n\
         # enabled = true\n\
         # Folders never remembered: * and ? stay within a folder name, ** crosses folders, and\n\
         # a pattern ending in /** is the folder and everything in it. Example: [\"/tmp/**\", \"**/node_modules\"]\n\
         # exclude = []\n\
         \n\
         [search]\n\
         # Search in This Folder (cmd+f) stays on the folder's own disk: a drive or network share\n\
         # mounted inside it isn't searched. true searches those too. (Links to folders are never\n\
         # followed, and on Windows neither are junctions or volumes mounted in a folder.)\n\
         # cross_devices = false\n\
         \n\
         [view]\n\
         # How new tabs show their folder: \"list\" (rows with columns), \"icons\" (a grid, with\n\
         # thumbnails of images), \"columns\" (a column per folder, the preview in the last),\n\
         # or \"tiles\" (larger icons with kind and size). As List, As Icons, As Columns and\n\
         # As Tiles switch a tab.\n\
         # default = \"list\"\n\
         # The icon view's icon size to start with: \"small\", \"medium\" or \"large\" (cmd+plus and\n\
         # cmd+minus change it).\n\
         # icon_size = \"medium\"\n\
         \n\
         [select]\n\
         # Whether Select by Pattern tells capitals apart. Without this line, it does on Linux and\n\
         # doesn't on macOS and Windows, as their usual file systems do.\n\
         # case_sensitive = CASE_DEFAULT\n\
         \n\
         [keys]\n\
         # Each command's keys. A list replaces the command's default keys; [] unbinds it.\n\
         # In a key, cmd is Command on macOS and Ctrl elsewhere; ctrl, alt, shift and super\n\
         # name those keys. Keys: a letter, digit or symbol (\"a\", \"1\", \".\", \"[\"), or enter, escape,\n\
         # backspace, delete, tab, space, up, down, left, right, home, end, pageup, pagedown, f1 to f24.\n\
         # A key you bind here is taken from any command that has it by default; if two lines bind\n\
         # the same key, the later one gets it.\n\
         #\n\
         # Typing letters filters the folder, and while the filter has text, typing, backspace and\n\
         # (in go-to-path mode) enter edit it rather than running commands.\n\
         #\n",
    );
    let s = s.replace("CASE_DEFAULT", if crate::pattern::Case::platform() == crate::pattern::Case::Sensitive { "true" } else { "false" });
    let mut s = s;
    let w = Cmd::ALL.iter().map(|c| c.id().len()).max().unwrap_or(0) + 2;
    for &cmd in Cmd::ALL {
        let keys = cmd.default_keys().iter().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(", ");
        let lhs = format!("\"{}\"", cmd.id());
        let rhs = format!("[{keys}]");
        s.push_str(&format!("# {lhs:<w$} = {rhs:<44} # {}\n", cmd.title()));
    }
    s
}

/// Creates the settings file from the template if it doesn't exist. Returns its path.
pub fn ensure_file() -> Result<PathBuf, String> {
    let p = path().ok_or("no home folder to keep settings in")?;
    if p.exists() {
        return Ok(p);
    }
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", fs::pretty(dir)))?;
    }
    // create_new: never overwrite a file that appeared in the meantime.
    use std::io::Write;
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
        Ok(mut f) => f.write_all(template().as_bytes()).map_err(|e| format!("can't write {}: {e}", fs::pretty(&p)))?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("can't create {}: {e}", fs::pretty(&p))),
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINUX: KeyPlatform = KeyPlatform::Linux;

    fn chords(c: &Config, cmd: Cmd) -> Vec<String> {
        c.keymap.chords(cmd).map(commands::chord_spec).collect()
    }

    /// `[view]`: the default view for new tabs and the icon size, and what a bad value says.
    #[test]
    fn view_settings() {
        let c = Config::parse("[view]\ndefault = \"icons\"\nicon_size = \"large\"\n", LINUX);
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!((c.settings.view_default, c.settings.icon_size), (crate::grid::ViewMode::Icons, crate::grid::IconSize::Large));
        let d = Config::parse("", LINUX);
        assert_eq!((d.settings.view_default, d.settings.icon_size), (crate::grid::ViewMode::List, crate::grid::IconSize::Medium));
        for (name, mode) in [("columns", crate::grid::ViewMode::Columns), ("tiles", crate::grid::ViewMode::Tiles)] {
            let c = Config::parse(&format!("[view]\ndefault = \"{name}\"\n"), LINUX);
            assert!(c.errors.is_empty(), "{:?}", c.errors);
            assert_eq!(c.settings.view_default, mode);
        }
        let bad = Config::parse("[view]\ndefault = \"gallery\"\nicon_size = 3\nzoom = 1\n", LINUX);
        assert_eq!(
            bad.errors,
            [
                "line 2: default should be \"list\", \"icons\", \"columns\" or \"tiles\"",
                "line 3: icon_size should be \"small\", \"medium\" or \"large\"",
                "line 4: unknown setting 'zoom' under [view] (settings are default, icon_size)",
            ]
        );
        assert_eq!(bad.settings.view_default, crate::grid::ViewMode::List, "a bad value changes nothing");
        // The template has both, commented out, and parses to the defaults.
        let t = template();
        assert!(t.contains("# default = \"list\"") && t.contains("# icon_size = \"medium\""));
        let parsed = Config::parse(&t, LINUX);
        assert!(parsed.errors.is_empty() && parsed.settings == Settings::default(), "{:?}", parsed.errors);
    }

    #[test]
    fn parses_settings_and_keys() {
        let c = Config::parse(
            r#"
            [general]
            show_hidden = true
            sort = "size"
            start_folder = "~/Projects"
            places = ["~", "/tmp"]

            [keys]
            "file.trash" = ["cmd+backspace", "Shift+Delete"]
            "file.duplicate" = []
            "app.palette" = "ctrl+space"
            "#,
            LINUX,
        );
        assert_eq!(c.errors, Vec::<String>::new());
        assert!(c.settings.show_hidden);
        assert_eq!(c.settings.sort, Sort::new(SortKey::Size, true));
        assert_eq!(c.settings.start_folder, Some(fs::home().join("Projects")));
        assert_eq!(c.settings.places, Some(vec![fs::home(), PathBuf::from("/tmp")]));
        assert_eq!(chords(&c, Cmd::Trash), ["cmd+backspace", "shift+delete"]);
        assert!(chords(&c, Cmd::Duplicate).is_empty());
        assert_eq!(chords(&c, Cmd::Palette), ["ctrl+space"]);
        assert!(c.keymap.chords(Cmd::Palette).next().unwrap().same_keys(&commands::parse_chord("cmd+space").unwrap(), LINUX));
        assert_eq!(chords(&c, Cmd::Copy), ["cmd+c"], "unbound commands keep their defaults");
    }

    /// `cmd` is Ctrl off macOS, so "cmd+g" and "ctrl+g" are one key there: a later line binding
    /// either takes it from every earlier binding, under both spellings.
    #[test]
    fn a_later_binding_takes_every_alias_of_its_keys() {
        let text = "[keys]\n\"nav.goto\" = [\"cmd+g\", \"ctrl+g\"]\n\"nav.reload\" = [\"cmd+g\"]\n";
        let (cmd_g, ctrl_g) = (commands::parse_chord("cmd+g").unwrap(), commands::parse_chord("ctrl+g").unwrap());
        for platform in [KeyPlatform::Linux, KeyPlatform::Windows] {
            let c = Config::parse(text, platform);
            assert_eq!(c.keymap.find(|b| b.same_keys(&ctrl_g, platform)), Some(Cmd::Reload), "{platform:?}");
            assert_eq!(c.keymap.find(|b| b.same_keys(&cmd_g, platform)), Some(Cmd::Reload), "{platform:?}");
            assert!(chords(&c, Cmd::GoTo).is_empty(), "{platform:?}");
            assert_eq!(c.errors, ["line 3: cmd+g is also bound to nav.goto on line 2; nav.reload gets it"], "{platform:?}");
            // And one command listing both spellings has the key once.
            let c = Config::parse("[keys]\n\"nav.goto\" = [\"cmd+g\", \"ctrl+g\"]\n", platform);
            assert_eq!(chords(&c, Cmd::GoTo), ["cmd+g"], "{platform:?}");
        }
        // On macOS ⌘G and ⌃G are different keys: Go to Path keeps ⌃G.
        let mac = KeyPlatform::Mac;
        let c = Config::parse(text, mac);
        assert_eq!(c.keymap.find(|b| b.same_keys(&cmd_g, mac)), Some(Cmd::Reload));
        assert_eq!(c.keymap.find(|b| b.same_keys(&ctrl_g, mac)), Some(Cmd::GoTo));
        assert_eq!(chords(&c, Cmd::GoTo), ["ctrl+g"]);
    }

    /// Only `[]` unbinds. A list whose every key is a typo is reported and keeps the defaults.
    #[test]
    fn a_list_of_only_typos_keeps_the_defaults() {
        let c = Config::parse("[keys]\n\"file.trash\" = [\"cmd+nope\"]\n\"file.copy\" = []\n\"file.cut\" = [\"cmd+nah\", \"f9\"]\n", LINUX);
        assert_eq!(chords(&c, Cmd::Trash), ["cmd+backspace", "delete"]);
        assert!(chords(&c, Cmd::Copy).is_empty());
        assert_eq!(chords(&c, Cmd::Cut), ["f9"]);
        assert_eq!(c.errors.len(), 2, "{:?}", c.errors);
        assert!(c.errors[0].starts_with("line 2: 'cmd+nope'") && c.errors[1].starts_with("line 4: 'cmd+nah'"), "{:?}", c.errors);
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("satchel-cfg-io-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Reads `path` on another thread; panics if that takes over 2 s (it would hang on a FIFO).
    fn read_soon(path: PathBuf) -> Config {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(Config::read(&path));
        });
        rx.recv_timeout(std::time::Duration::from_secs(2)).expect("reading the settings blocked")
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_at_the_settings_path_is_refused_without_blocking() {
        let p = scratch("fifo").join("config.toml");
        let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let cfg = read_soon(p);
        assert!(cfg.errors[0].ends_with("isn't a regular file, so it was ignored"), "{:?}", cfg.errors);
        assert!(!cfg.settings.show_hidden);
    }

    #[test]
    fn a_folder_or_huge_file_is_refused() {
        let d = scratch("big");
        let cfg = read_soon(d.clone());
        assert!(cfg.errors[0].contains("isn't a regular file"), "{:?}", cfg.errors);
        let p = d.join("config.toml");
        let mut text = String::from("[general]\nshow_hidden = true\n");
        text.push_str(&"#".repeat(MAX_BYTES as usize));
        std::fs::write(&p, &text).unwrap();
        let cfg = read_soon(p.clone());
        assert!(cfg.errors[0].contains("more than the 1024 KB a settings file can be"), "{:?}", cfg.errors);
        assert!(!cfg.settings.show_hidden, "nothing in it applies");
        // Up to the limit is read.
        text.truncate(MAX_BYTES as usize);
        std::fs::write(&p, &text).unwrap();
        let cfg = read_soon(p);
        assert!(cfg.errors.is_empty() && cfg.settings.show_hidden, "{:?}", cfg.errors);
    }

    #[test]
    fn a_binary_file_is_refused() {
        let p = scratch("bin").join("config.toml");
        std::fs::write(&p, b"\x80\xff").unwrap();
        let cfg = read_soon(p);
        assert!(cfg.errors[0].ends_with("isn't text (UTF-8), so it was ignored"), "{:?}", cfg.errors);
    }

    #[test]
    fn missing_or_empty_file_gives_defaults() {
        let dir = std::env::temp_dir().join(format!("satchel-cfg-{}", std::process::id()));
        let c = Config::read(&dir.join("nope.toml"));
        assert!(c.errors.is_empty());
        assert_eq!(c.settings, Settings::default());
        assert_eq!(chords(&c, Cmd::Trash), ["cmd+backspace", "delete"]);
        let c = Config::parse("", LINUX);
        assert!(c.errors.is_empty() && c.summary().is_none());
    }

    #[test]
    fn unknown_command_and_setting() {
        let c = Config::parse("[general]\nshow_hiden = true\nsort = \"colour\"\n\n[keys]\n\"file.trahs\" = [\"delete\"]\n", LINUX);
        assert_eq!(
            c.errors,
            [
                "line 2: unknown setting 'show_hiden' (settings are show_hidden, sort, sort_descending, folders_first, start_folder, places)",
                "line 3: sort should be \"name\", \"kind\", \"size\", \"modified\" or \"created\"",
                "line 6: unknown command 'file.trahs' (did you mean 'file.trash'?)",
            ]
        );
        assert_eq!(c.summary().unwrap(), format!("Settings: {} (+2 more)", c.errors[0]));
        assert_eq!(chords(&c, Cmd::Trash), ["cmd+backspace", "delete"], "the bad line changes nothing");
        let c = Config::parse("show_hidden = true\n[colors]\nx = 1\n", LINUX);
        assert_eq!(
            c.errors,
            [
                "line 1: 'show_hidden' belongs under [general]",
                "line 2: unknown section 'colors' (sections are [general], [jump], [search], [select], [view] and [keys])"
            ]
        );
    }

    #[test]
    fn bad_chord_keeps_the_good_ones() {
        let c = Config::parse("[keys]\n\"file.trash\" = [\"cmd+nope\", \"f8\"]\n\"nav.back\" = 3\n", LINUX);
        assert_eq!(c.errors.len(), 2, "{:?}", c.errors);
        assert!(c.errors[0].starts_with("line 2: 'cmd+nope' isn't a key chord"));
        assert!(c.errors[1].starts_with("line 3: nav.back should be a list of keys"));
        assert_eq!(chords(&c, Cmd::Trash), ["f8"]);
        assert_eq!(chords(&c, Cmd::Back), ["cmd+[", "alt+left"]);
    }

    #[test]
    fn conflicts_go_to_the_later_binding() {
        // Taking a default's key is silent; the default loses it.
        let c = Config::parse("[keys]\n\"nav.goto\" = [\"cmd+d\"]\n", LINUX);
        assert!(c.errors.is_empty());
        assert_eq!(chords(&c, Cmd::GoTo), ["cmd+d"]);
        assert!(chords(&c, Cmd::Duplicate).is_empty());
        // Two lines with the same key: reported, the later one wins.
        let c = Config::parse("[keys]\n\"nav.goto\" = [\"cmd+g\"]\n\"nav.reload\" = [\"ctrl+g\", \"f5\"]\n", LINUX);
        assert_eq!(c.errors, ["line 3: ctrl+g is also bound to nav.goto on line 2; nav.reload gets it"]);
        assert!(chords(&c, Cmd::GoTo).is_empty());
        assert_eq!(chords(&c, Cmd::Reload), ["ctrl+g", "f5"]);
        // On macOS ctrl and cmd are different keys, so there's no conflict.
        let c = Config::parse("[keys]\n\"nav.goto\" = [\"cmd+g\"]\n\"nav.reload\" = [\"ctrl+g\"]\n", KeyPlatform::Mac);
        assert!(c.errors.is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let c = Config::parse("[general]\nshow_hidden = yes please\nsort = \"kind\"\n[keys\n\"file.copy\" = [\"f9\"]\n", LINUX);
        assert_eq!(
            c.errors,
            [
                "line 2: string values must be quoted, expected literal string",
                "line 4: unclosed table, expected `]`",
                "line 5: unknown setting 'file.copy' (settings are show_hidden, sort, sort_descending, folders_first, start_folder, places)",
            ]
        );
        assert_eq!(c.settings.sort.key, SortKey::Kind, "the valid lines still apply");
        assert!(!c.settings.show_hidden);
        // Hopeless input gives up quietly with defaults.
        let c = Config::parse("\"\"\"never closed", LINUX);
        assert!(!c.errors.is_empty());
        assert_eq!(c.settings, Settings::default());
    }

    #[test]
    fn template_parses_and_uncommented_is_the_defaults() {
        let t = template();
        let c = Config::parse(&t, LINUX);
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        for &cmd in Cmd::ALL {
            assert!(t.contains(&format!("\"{}\"", cmd.id())), "template lists {}", cmd.id());
        }
        // Uncomment every setting and binding line: the result is valid and changes nothing.
        let on: String = t
            .lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(rest) if rest.starts_with('"') || rest.contains(" = ") => rest,
                _ => l,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for platform in [KeyPlatform::Mac, KeyPlatform::Windows, KeyPlatform::Linux] {
            let c = Config::parse(&on, platform);
            assert!(c.errors.is_empty(), "{platform:?}: {:?}", c.errors);
            for &cmd in Cmd::ALL {
                let want: Vec<Chord> = Keymap::defaults().chords(cmd).copied().collect();
                assert_eq!(c.keymap.chords(cmd).copied().collect::<Vec<_>>(), want, "{}", cmd.id());
            }
        }
    }

    /// Startup cost of the settings file: `cargo test --release -- --ignored --nocapture parse_speed`.
    #[test]
    #[ignore = "benchmark"]
    fn parse_speed() {
        let full: String =
            template().lines().map(|l| l.strip_prefix("# \"").map_or(l.to_string(), |r| format!("\"{r}"))).collect::<Vec<_>>().join("\n");
        for (name, text) in [("template (all comments)", template()), ("every command bound", full)] {
            let n = 1000;
            let t = std::time::Instant::now();
            for _ in 0..n {
                assert!(Config::parse(&text, LINUX).errors.is_empty());
            }
            println!("{name}, {} bytes: {:.1} µs", text.len(), t.elapsed().as_secs_f64() * 1e6 / n as f64);
        }
        let t = std::time::Instant::now();
        let _ = Config::read(Path::new("/nonexistent/satchel/config.toml"));
        println!("no file: {:.1} µs", t.elapsed().as_secs_f64() * 1e6);
    }

    const TOML_BITS: &[&str] = &[
        "[general]",
        "[keys]",
        "[jump]",
        "[select]",
        "[search]",
        "[[x]]",
        "[",
        "]",
        "\n",
        "\r\n",
        " ",
        "=",
        "\"",
        "'",
        "'''",
        "\"\"\"",
        "#",
        ",",
        "{",
        "}",
        ".",
        "\\",
        "\\u",
        "show_hidden",
        "sort",
        "true",
        "false",
        "1",
        "-1e9",
        "\"size\"",
        "places",
        "start_folder",
        "\"~\"",
        "\"/tmp\"",
        "exclude",
        "\"**/x\"",
        "\"{a,b}\"",
        "\"file.trash\"",
        "\"nav.goto\"",
        "\"cmd+g\"",
        "\"ctrl+shift+f2\"",
        "\"+\"",
        "\"cmd++\"",
        "\"é\"",
        "\0",
        "\u{feff}",
        "\u{301}",
        "😀",
        "case_sensitive",
        "cross_devices",
        "enabled",
    ];

    /// Any text: never a panic, every error names a line within the text (or none), and no
    /// chord ends up bound to two commands.
    #[test]
    fn any_settings_text_parses_without_panicking() {
        crate::prop::check("config_parse", 400, |r| {
            let text = if r.one_in(4) { r.string(200) } else { r.from(TOML_BITS, 80) };
            let lines = text.matches('\n').count() + 1;
            for platform in [KeyPlatform::Mac, KeyPlatform::Windows, KeyPlatform::Linux] {
                let c = Config::parse(&text, platform);
                for e in &c.errors {
                    if let Some(n) = e.strip_prefix("line ").and_then(|r| r.split(':').next()).and_then(|n| n.parse::<usize>().ok()) {
                        assert!((1..=lines).contains(&n), "{text:?}: {e}");
                    }
                }
                let all: Vec<(Chord, Cmd)> = Cmd::ALL.iter().flat_map(|&cmd| c.keymap.chords(cmd).map(move |&k| (k, cmd))).collect();
                for (i, a) in all.iter().enumerate() {
                    for b in &all[i + 1..] {
                        assert!(
                            !(a.1 != b.1 && a.0.same_keys(&b.0, platform)),
                            "{text:?}: {:?} on two commands",
                            commands::chord_spec(&a.0)
                        );
                    }
                }
            }
        });
    }

    /// A chord that parses is written back as text that parses to the same chord.
    #[test]
    fn chords_round_trip_through_their_text() {
        const BITS: &[&str] = &[
            "cmd",
            "ctrl",
            "alt",
            "shift",
            "super",
            "option",
            "win",
            "meta",
            "mod",
            "command",
            "primary",
            "+",
            " ",
            "a",
            "Z",
            "1",
            ".",
            ",",
            "[",
            "]",
            "/",
            "=",
            "-",
            "`",
            "'",
            ";",
            "enter",
            "escape",
            "backspace",
            "delete",
            "tab",
            "space",
            "up",
            "down",
            "left",
            "pageup",
            "home",
            "f1",
            "f12",
            "f24",
            "f25",
            "f0",
            "comma",
            "plus",
            "é",
            "\u{301}",
            "😀",
            "\0",
        ];
        crate::prop::check("chord_round_trip", 2000, |r| {
            let s = if r.one_in(5) { r.string(12) } else { r.from(BITS, 7) };
            if let Ok(c) = commands::parse_chord(&s) {
                let spec = commands::chord_spec(&c);
                assert_eq!(commands::parse_chord(&spec), Ok(c), "{s:?} → {spec:?}");
            }
        });
    }

    /// Pathological files up to the size limit parse (or are refused) quickly, without
    /// overflowing the stack: deep nesting, one huge line, thousands of bad lines.
    #[test]
    fn pathological_settings_are_bounded() {
        let n = MAX_BYTES as usize / 4;
        let cases: Vec<(&str, String)> = vec![
            ("deep arrays", format!("[general]\nplaces = {}", "[".repeat(n))),
            ("deep inline tables", format!("[general]\nx = {}", "{a=".repeat(n / 3))),
            ("one huge string", format!("[general]\nstart_folder = \"{}\"\n", "a".repeat(n))),
            ("many bad lines", "show_hidden = yes\n".repeat(n / 20)),
            ("many keys", (0..n / 40).map(|i| format!("\"file.trash\" = [\"f{}\"]\n", i % 30)).collect::<String>()),
            ("many sections", "[keys]\n".repeat(n / 8)),
        ];
        for (what, text) in cases {
            assert!(text.len() as u64 <= MAX_BYTES);
            let c = crate::prop::within(std::time::Duration::from_secs(5), what, move || Config::parse(&text, LINUX).errors.len());
            let _ = c;
        }
    }

    #[test]
    fn path_follows_the_platform() {
        let p = path().unwrap();
        assert!(p.ends_with(if cfg!(any(target_os = "macos", windows)) { "Satchel/config.toml" } else { "satchel/config.toml" }));
    }
}
