//! The settings file: preferences and keybindings in one TOML file (NORTHSTAR: "Settings and
//! keybindings in one config file"). It's user data, so storing it is allowed.
//!
//! - macOS `~/Library/Application Support/Arbor/config.toml`, Windows `%APPDATA%\Arbor\config.toml`,
//!   elsewhere `$XDG_CONFIG_HOME/arbor/config.toml` (default `~/.config/arbor/config.toml`).
//!   `RUNE_DIRS_ROOT` moves it under `<root>/config/` for tests (Rune's `dirs`).
//! - Read once at startup, on the thread that reads the first folder (`app::read_first_folder`),
//!   so it overlaps the platform's launch. Nothing watches it: "Reload Settings" reads it again.
//! - Errors never stop Arbor: each is reported with its line, everything valid applies, and the
//!   rest keeps its default. A line that doesn't parse as TOML is skipped and the rest re-read.
//! - Keybindings: `[keys]` maps a command id to its chords, which replace the defaults (`[]`
//!   unbinds). A chord bound to two commands goes to the one bound later: a binding in the file
//!   takes the chord from another command's defaults, and a later line takes it from an earlier
//!   line, which is reported. Later wins everywhere (as in VS Code and Vim), so adding a line at
//!   the bottom always does what it says.

use std::path::{Path, PathBuf};

use rune::{Chord, KeyPlatform};
use toml_edit::{Document, Item, TableLike};

use crate::commands::{self, Cmd, Keymap};
use crate::fs::{self, Sort, SortKey};

/// What the settings file sets, beyond keys. Defaults are what Arbor does without one.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Show hidden files when a window opens.
    pub show_hidden: bool,
    pub sort: Sort,
    /// The folder to open when none is named on the command line.
    pub start_folder: Option<PathBuf>,
    /// Sidebar places, replacing the automatic list.
    pub places: Option<Vec<PathBuf>>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { show_hidden: false, sort: Sort { key: SortKey::Name, desc: false }, start_folder: None, places: None }
    }
}

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

const FOLDER: &str = if cfg!(any(target_os = "macos", windows)) { "Arbor" } else { "arbor" };

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

    pub fn read(path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text, KeyPlatform::current()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => Config { errors: vec![format!("can't read {}: {e}", fs::pretty(path))], ..Config::default() },
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
                ("keys", Some(t)) => keys = key_table(t, &mut errors, &line),
                ("general" | "keys", None) => errors.push(format!("line {at}: '{name}' should be a section: [{name}]")),
                (_, _) if GENERAL.contains(&name) => {
                    errors.push(format!("line {at}: '{name}' belongs under [general]"));
                }
                _ => errors.push(format!("line {at}: unknown section '{name}' (sections are [general] and [keys])")),
            }
        }
        let keymap = build_keymap(&keys, platform, &mut errors);
        // Syntax errors are found first; list everything in file order.
        errors.sort_by_key(|e| e.strip_prefix("line ").and_then(|r| r.split(':').next()).and_then(|n| n.parse::<usize>().ok()));
        Config { settings, keymap, errors }
    }
}

const GENERAL: [&str; 5] = ["show_hidden", "sort", "sort_descending", "start_folder", "places"];

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
            "sort" => match item.as_str().and_then(sort_key) {
                Some(k) => s.sort.key = k,
                None => bad("\"name\", \"kind\", \"size\" or \"modified\""),
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
    s.sort.desc = desc.unwrap_or(matches!(s.sort.key, SortKey::Size | SortKey::Modified));
}

fn sort_key(s: &str) -> Option<SortKey> {
    Some(match s {
        "name" => SortKey::Name,
        "kind" => SortKey::Kind,
        "size" => SortKey::Size,
        "modified" | "date" => SortKey::Modified,
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
        for spec in specs {
            match commands::parse_chord(spec) {
                Ok(c) if !chords.contains(&c) => chords.push(c),
                Ok(_) => {}
                Err(e) => errors.push(format!("line {at}: {e}")),
            }
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
            if let Some(i) = binds.iter().position(|b| b.1 != *cmd && b.0.same_keys(c, platform)) {
                let (_, other, was) = binds.remove(i);
                if was > 0 {
                    errors.push(format!(
                        "line {at}: {} is also bound to {} on line {was}; {} gets it",
                        commands::chord_spec(c),
                        other.id(),
                        cmd.id()
                    ));
                }
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
    let mut s = String::from(
        "# Arbor settings. Lines starting with # are comments: remove the # to use a line.\n\
         # Changes apply when you run \"Reload Settings\" from the command palette, or at the next start.\n\
         \n\
         [general]\n\
         # Show hidden files (dotfiles, and files the system marks hidden) in new windows.\n\
         # show_hidden = false\n\
         \n\
         # How folders are sorted: \"name\", \"kind\", \"size\" or \"modified\".\n\
         # sort = \"name\"\n\
         # Reverse it. Without this line, size and modified sort largest and newest first.\n\
         # sort_descending = false\n\
         \n\
         # The folder to open when none is given on the command line. Empty: the current folder,\n\
         # or your home folder when Arbor is started from the desktop.\n\
         # start_folder = \"~\"\n\
         \n\
         # Sidebar places, replacing the automatic list. Empty: the automatic list.\n\
         # places = [\"~\", \"~/Downloads\", \"~/Projects\"]\n\
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
        assert_eq!(c.settings.sort, Sort { key: SortKey::Size, desc: true });
        assert_eq!(c.settings.start_folder, Some(fs::home().join("Projects")));
        assert_eq!(c.settings.places, Some(vec![fs::home(), PathBuf::from("/tmp")]));
        assert_eq!(chords(&c, Cmd::Trash), ["cmd+backspace", "shift+delete"]);
        assert!(chords(&c, Cmd::Duplicate).is_empty());
        assert_eq!(chords(&c, Cmd::Palette), ["ctrl+space"]);
        assert!(c.keymap.chords(Cmd::Palette).next().unwrap().same_keys(&commands::parse_chord("cmd+space").unwrap(), LINUX));
        assert_eq!(chords(&c, Cmd::Copy), ["cmd+c"], "unbound commands keep their defaults");
    }

    #[test]
    fn missing_or_empty_file_gives_defaults() {
        let dir = std::env::temp_dir().join(format!("arbor-cfg-{}", std::process::id()));
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
                "line 2: unknown setting 'show_hiden' (settings are show_hidden, sort, sort_descending, start_folder, places)",
                "line 3: sort should be \"name\", \"kind\", \"size\" or \"modified\"",
                "line 6: unknown command 'file.trahs' (did you mean 'file.trash'?)",
            ]
        );
        assert_eq!(c.summary().unwrap(), format!("Settings: {} (+2 more)", c.errors[0]));
        assert_eq!(chords(&c, Cmd::Trash), ["cmd+backspace", "delete"], "the bad line changes nothing");
        let c = Config::parse("show_hidden = true\n[colors]\nx = 1\n", LINUX);
        assert_eq!(
            c.errors,
            ["line 1: 'show_hidden' belongs under [general]", "line 2: unknown section 'colors' (sections are [general] and [keys])"]
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
                "line 5: unknown setting 'file.copy' (settings are show_hidden, sort, sort_descending, start_folder, places)",
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
        let _ = Config::read(Path::new("/nonexistent/arbor/config.toml"));
        println!("no file: {:.1} µs", t.elapsed().as_secs_f64() * 1e6);
    }

    #[test]
    fn path_follows_the_platform() {
        let p = path().unwrap();
        assert!(p.ends_with(if cfg!(any(target_os = "macos", windows)) { "Arbor/config.toml" } else { "arbor/config.toml" }));
    }
}
