//! Batch rename's rules, as pure functions on names (NORTHSTAR Core: "regex, numbering,
//! preview before apply"). Nothing here touches the disk: the dialog (`batch.rs`) gives it the
//! items and what's already in their folders, and gets back each new name and what's wrong
//! with it. Applying is one file operation (`ops::Op::Rename`).
//!
//! A rule:
//!
//! - **Find**: plain text (every occurrence), or a regular expression (`regex-lite`: Perl
//!   syntax, classes and case folding ASCII only). Capitals count only when asked. Empty
//!   Find makes **Replace with** the whole new name (or stem).
//! - **Replace with**: text and tokens. `{n}` is the counter (`{n:3}` pads it to 3 digits),
//!   `{name}` the original stem, `{ext}` the original extension (without the dot) and
//!   `{parent}` the item's folder's name. In regex mode `$1`, `${1}` and `${name}` are what a
//!   group matched (`$$` is a dollar sign). Anything else in braces stays as typed.
//! - **Numbering**: the counter starts at Start and goes up by Step for each item, in the
//!   order the items are listed.
//! - **Case**: keep, lower, UPPER or Title, applied last.
//! - **Include extension**: off by default, so only the stem changes and the extension stays.
//!   A folder has no extension (as inline rename treats it), and neither has a name whose only
//!   dot is its first character (`.bashrc`).

use std::collections::HashMap;
use std::collections::HashSet;

use regex_lite::{Regex, RegexBuilder};

/// What Case does to the new name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CaseMode {
    #[default]
    Keep,
    Lower,
    Upper,
    Title,
}

impl CaseMode {
    pub const ALL: [CaseMode; 4] = [CaseMode::Keep, CaseMode::Lower, CaseMode::Upper, CaseMode::Title];

    pub fn label(self) -> &'static str {
        match self {
            CaseMode::Keep => "Keep",
            CaseMode::Lower => "lower",
            CaseMode::Upper => "UPPER",
            CaseMode::Title => "Title",
        }
    }

    pub fn apply(self, s: &str) -> String {
        match self {
            CaseMode::Keep => s.to_string(),
            CaseMode::Lower => s.to_lowercase(),
            CaseMode::Upper => s.to_uppercase(),
            CaseMode::Title => title_case(s),
        }
    }
}

/// Each word's first letter capital and the rest small. A word starts after anything that
/// isn't a letter, digit or apostrophe ("don't" stays one word).
pub fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut start = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if start {
                out.extend(c.to_uppercase());
            } else {
                out.extend(c.to_lowercase());
            }
            start = false;
        } else {
            out.push(c);
            start = !matches!(c, '\'' | '’');
        }
    }
    out
}

/// A rule as the dialog's fields hold it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub find: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub replace: String,
    pub start: String,
    pub step: String,
    pub case: CaseMode,
    pub include_ext: bool,
}

impl Default for Rule {
    fn default() -> Rule {
        Rule {
            find: String::new(),
            regex: false,
            case_sensitive: false,
            replace: String::new(),
            start: "1".into(),
            step: "1".into(),
            case: CaseMode::Keep,
            include_ext: false,
        }
    }
}

/// A piece of Replace with.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    Text(String),
    /// `{n}`, padded to this many digits.
    Counter(usize),
    Name,
    Ext,
    Parent,
}

/// What Find looks for.
enum Find {
    Nothing,
    /// Plain text, capitals counting or (ASCII) not: no regex needed, so 10k names take a
    /// fraction of what the regex engine would.
    Text {
        needle: String,
        fold: bool,
    },
    Regex(Regex),
}

/// A rule ready to run on names.
pub struct Compiled {
    find: Find,
    pieces: Vec<Piece>,
    start: i64,
    step: i64,
    case: CaseMode,
    include_ext: bool,
}

/// The longest padding `{n:…}` takes (more is surely a typo).
const MAX_PAD: usize = 32;

impl Rule {
    /// Checks the rule and gets it ready, or says what's wrong with it.
    pub fn compile(&self) -> Result<Compiled, String> {
        let number = |s: &str, what: &str| -> Result<i64, String> {
            let t = s.trim();
            if t.is_empty() {
                return Ok(1);
            }
            t.parse::<i64>().ok().filter(|n| n.unsigned_abs() < 1 << 40).ok_or_else(|| format!("{what} must be a whole number"))
        };
        let start = number(&self.start, "Start")?;
        let step = number(&self.step, "Step")?;
        let find = if self.find.is_empty() {
            Find::Nothing
        } else if self.regex {
            Find::Regex(RegexBuilder::new(&self.find).case_insensitive(!self.case_sensitive).build().map_err(|e| regex_error(&e))?)
        } else {
            Find::Text { needle: self.find.clone(), fold: !self.case_sensitive }
        };
        Ok(Compiled { find, pieces: pieces(&self.replace), start, step, case: self.case, include_ext: self.include_ext })
    }
}

/// regex-lite's message, first line only (the rest draws the pattern with a caret).
fn regex_error(e: &regex_lite::Error) -> String {
    let s = e.to_string();
    let last = s.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("not a valid regular expression").trim().to_string();
    format!("Regex: {}", last.trim_start_matches("error: "))
}

/// Replace with, split into text and tokens.
fn pieces(s: &str) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    let mut text = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        text.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let token = after.find('}').and_then(|j| {
            let inner = &after[..j];
            let piece = match inner {
                "n" => Piece::Counter(0),
                "name" => Piece::Name,
                "ext" => Piece::Ext,
                "parent" => Piece::Parent,
                _ => {
                    let w = inner.strip_prefix("n:")?.parse::<usize>().ok().filter(|&w| w <= MAX_PAD)?;
                    Piece::Counter(w)
                }
            };
            Some((piece, j))
        });
        match token {
            Some((piece, j)) => {
                if !text.is_empty() {
                    out.push(Piece::Text(std::mem::take(&mut text)));
                }
                out.push(piece);
                rest = &after[j + 1..];
            }
            None => {
                text.push('{');
                rest = after;
            }
        }
    }
    text.push_str(rest);
    if !text.is_empty() {
        out.push(Piece::Text(text));
    }
    out
}

/// `hay` with every `needle` replaced by `with`, left to right without overlaps, and whether
/// there was one; with `fold`,
/// ASCII capitals don't count (as regex-lite's case-insensitive matching, which folds only
/// ASCII). Byte-wise on UTF-8 is safe: a non-ASCII byte only ever equals itself.
fn replace_text(hay: &str, needle: &str, with: &str, fold: bool) -> (String, bool) {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    let eq = |a: &[u8]| if fold { a.eq_ignore_ascii_case(n) } else { a == n };
    let mut out = String::with_capacity(hay.len() + with.len());
    let (mut i, mut from) = (0, 0);
    while i + n.len() <= h.len() {
        if eq(&h[i..i + n.len()]) {
            out.push_str(&hay[from..i]);
            out.push_str(with);
            i += n.len();
            from = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&hay[from..]);
    (out, from > 0)
}

/// A name's stem and extension (without its dot), as inline rename splits it: the last dot,
/// unless it's the first character; folders have no extension.
pub fn split_ext(name: &str, is_dir: bool) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && !is_dir => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

impl Compiled {
    /// The new name for an item, `index` being how many items before it Find matched (the
    /// counter counts only those), and whether Find matched this one.
    pub fn apply(&self, name: &str, is_dir: bool, parent: &str, index: usize) -> (String, bool) {
        let (stem, ext) = split_ext(name, is_dir);
        let part = if self.include_ext { name } else { stem };
        let n = self.start.saturating_add(self.step.saturating_mul(index as i64));
        let expand = |out: &mut String, escape_dollars: bool| {
            for p in &self.pieces {
                let mut put = |s: &str| {
                    if escape_dollars {
                        out.push_str(&s.replace('$', "$$"));
                    } else {
                        out.push_str(s);
                    }
                };
                match p {
                    // Typed text keeps its `$1` in regex mode: that's what it's for.
                    Piece::Text(t) => out.push_str(t),
                    Piece::Counter(0) => put(&n.to_string()),
                    Piece::Counter(w) => put(&format!("{n:0w$}", w = *w + usize::from(n < 0))),
                    Piece::Name => put(stem),
                    Piece::Ext => put(ext),
                    Piece::Parent => put(parent),
                }
            }
        };
        let (changed, matched) = match &self.find {
            Find::Nothing if self.pieces.is_empty() => (part.to_string(), true),
            Find::Nothing => {
                let mut s = String::new();
                expand(&mut s, false);
                (s, true)
            }
            Find::Text { needle, fold } => {
                let mut with = String::new();
                expand(&mut with, false);
                replace_text(part, needle, &with, *fold)
            }
            Find::Regex(re) => {
                let mut with = String::new();
                expand(&mut with, true);
                match re.replace_all(part, with.as_str()) {
                    std::borrow::Cow::Owned(s) => (s, true),
                    std::borrow::Cow::Borrowed(s) => (s.to_string(), false),
                }
            }
        };
        let mut out = if self.case == CaseMode::Keep { changed } else { self.case.apply(&changed) };
        if !self.include_ext && !ext.is_empty() {
            out.push('.');
            out.push_str(ext);
        }
        (out, matched)
    }
}

// ---- what's wrong with a name -------------------------------------------------------------------

/// Whose rules a name must follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Mac,
    Windows,
}

impl Platform {
    pub const fn current() -> Platform {
        if cfg!(windows) {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::Mac
        } else {
            Platform::Linux
        }
    }

    /// Whether its usual disks ignore case (APFS and NTFS by default). The dialog asks each
    /// folder's disk instead when it can (`batch.rs`).
    pub const fn ignores_case(self) -> bool {
        !matches!(self, Platform::Linux)
    }
}

/// The longest name: 255 bytes of UTF-8 on Linux and macOS (ext4, APFS), 255 UTF-16 units on
/// Windows (NTFS).
pub const MAX_NAME: usize = 255;

/// Names Windows reserves for devices, with or without an extension.
const RESERVED: [&str; 28] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT1",
    "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
];

/// Why `name` can't be a file name on `p`, or `None` when it can.
pub fn invalid(name: &str, p: Platform) -> Option<String> {
    if name.is_empty() {
        return Some("Empty name".into());
    }
    if name == "." || name == ".." {
        return Some(format!("“{name}” can't be a name"));
    }
    if name.contains('/') {
        return Some("Names can't contain “/”".into());
    }
    if name.contains('\0') {
        return Some("Names can't contain a NUL character".into());
    }
    if p == Platform::Windows {
        if let Some(c) = name.chars().find(|c| matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*')) {
            return Some(format!("Names can't contain “{c}” on Windows"));
        }
        if name.chars().any(|c| (c as u32) < 32) {
            return Some("Names can't contain control characters on Windows".into());
        }
        if name.ends_with('.') || name.ends_with(' ') {
            return Some("Names can't end with a dot or a space on Windows".into());
        }
        let device = name.split('.').next().unwrap_or(name).trim_end_matches(' ').to_uppercase();
        if RESERVED.contains(&device.as_str()) {
            return Some(format!("“{device}” is a reserved name on Windows"));
        }
    }
    let (len, unit) = match p {
        Platform::Windows => (name.encode_utf16().count(), "characters"),
        Platform::Linux | Platform::Mac => (name.len(), "bytes"),
    };
    if len > MAX_NAME {
        return Some(format!("Too long: {len} {unit} (the limit is {MAX_NAME})"));
    }
    None
}

// ---- the preview --------------------------------------------------------------------------------

/// One item to rename: its name, and which of the batch's folders it's in.
#[derive(Clone, Debug)]
pub struct Item {
    pub name: String,
    pub is_dir: bool,
    pub dir: u32,
    /// It keeps its name whatever the rule says, and why (a name that isn't valid Unicode
    /// can't be edited as text without changing it).
    pub keep: Option<&'static str>,
    /// It wasn't there when its folder was read for the preview.
    pub gone: bool,
}

/// One of the batch's folders: its name (for `{parent}`), what else is in it, and whether its
/// disk ignores case.
#[derive(Clone, Debug, Default)]
pub struct Dir {
    pub name: String,
    /// The names in it that aren't being renamed (lowercased when it ignores case).
    pub others: HashSet<String>,
    pub ignores_case: bool,
    /// What's in it is known (read from the disk).
    pub known: bool,
}

/// What happens to one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// The new name is the old one.
    Same,
    /// It keeps its name, and why.
    Kept(&'static str),
    Change,
    /// It can't be renamed as asked, and why.
    Problem(String),
}

/// The whole batch's new names, each one's status, and the counts the summary shows.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub new: Vec<String>,
    pub status: Vec<Status>,
    pub changes: usize,
    pub problems: usize,
}

impl Plan {
    /// "12 will change · 2 problems".
    pub fn summary(&self) -> String {
        let s = |n: usize| if n == 1 { "" } else { "s" };
        let change = match self.changes {
            0 => "Nothing will change".to_string(),
            n => format!("{} will change", crate::search::count(n as u64)),
        };
        match self.problems {
            0 => change,
            n => format!("{change} · {} problem{}", crate::search::count(n as u64), s(n)),
        }
    }
}

/// Every item's new name and status under rule `r`, checking each new name against `p`'s
/// rules, the other new names in its folder, and what else is in its folder.
pub fn plan(r: &Compiled, items: &[Item], dirs: &[Dir], p: Platform) -> Plan {
    let mut new = Vec::with_capacity(items.len());
    let mut status = Vec::with_capacity(items.len());
    // The counter: how many items Find has matched so far.
    let mut counted = 0;
    for it in items {
        if let Some(why) = it.keep {
            new.push(it.name.clone());
            status.push(Status::Kept(why));
            continue;
        }
        let dir = &dirs[it.dir as usize];
        let (name, matched) = r.apply(&it.name, it.is_dir, &dir.name, counted);
        counted += usize::from(matched);
        status.push(if it.gone {
            Status::Problem("It isn't there any more (moved or deleted)".into())
        } else if name == it.name {
            Status::Same
        } else {
            invalid(&name, p).map_or(Status::Change, Status::Problem)
        });
        new.push(name);
    }
    // Two items given one name in a folder: the ones that change are problems (one keeping
    // its name holds it). On a disk that ignores case, names that differ only in case are one
    // name.
    let keys: Vec<std::borrow::Cow<str>> = items
        .iter()
        .zip(&new)
        .map(|(it, n)| if dirs[it.dir as usize].ignores_case { n.to_lowercase().into() } else { n.as_str().into() })
        .collect();
    // Per name: the first item with it, the second, and how many.
    let mut groups: HashMap<(u32, &str), (usize, usize, usize)> = HashMap::with_capacity(items.len());
    for (i, k) in keys.iter().enumerate() {
        let g = groups.entry((items[i].dir, k.as_ref())).or_insert((i, usize::MAX, 0));
        if g.2 == 1 {
            g.1 = i;
        }
        g.2 += 1;
    }
    for (i, k) in keys.iter().enumerate() {
        let (first, second, n) = groups[&(items[i].dir, k.as_ref())];
        if n < 2 || status[i] != Status::Change {
            continue;
        }
        let other = if i == first { second } else { first };
        status[i] = Status::Problem(if n > 2 {
            format!("{} other items get this name too", n - 1)
        } else if new[i] == new[other] {
            format!("Same new name as “{}”", items[other].name)
        } else {
            format!("Same name as “{}” once case is ignored (this disk ignores case)", new[other])
        });
    }
    // A new name already taken in its folder by something that isn't being renamed.
    for (i, it) in items.iter().enumerate() {
        let d = &dirs[it.dir as usize];
        if status[i] != Status::Change || !d.known || !d.others.contains(keys[i].as_ref()) {
            continue;
        }
        status[i] = Status::Problem(if d.ignores_case && !d.others.contains(&new[i]) {
            format!("A name differing only in case is already here (this disk ignores case): “{}”", new[i])
        } else {
            format!("“{}” already exists here", new[i])
        });
    }
    let changes = status.iter().filter(|s| **s == Status::Change).count();
    let problems = status.iter().filter(|s| matches!(s, Status::Problem(_))).count();
    Plan { new, status, changes, problems }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(find: &str, replace: &str) -> Rule {
        Rule { find: find.into(), replace: replace.into(), ..Rule::default() }
    }

    fn one(r: &Rule, name: &str) -> String {
        r.compile().unwrap().apply(name, false, "Photos", 0).0
    }

    fn items(names: &[&str]) -> Vec<Item> {
        names.iter().map(|n| Item { name: n.to_string(), is_dir: false, dir: 0, keep: None, gone: false }).collect()
    }

    fn dir(others: &[&str], ignores_case: bool) -> Dir {
        let others = others.iter().map(|s| if ignores_case { s.to_lowercase() } else { s.to_string() }).collect();
        Dir { name: "Photos".into(), others, ignores_case, known: true }
    }

    fn run(r: &Rule, names: &[&str], d: Dir) -> Plan {
        plan(&r.compile().unwrap(), &items(names), &[d], Platform::Linux)
    }

    #[test]
    fn plain_text_replaces_every_occurrence_and_ignores_capitals_unless_asked() {
        assert_eq!(one(&rule("a", "o"), "banana.txt"), "bonono.txt");
        assert_eq!(one(&rule("IMG", "Photo"), "img_IMG.jpg"), "Photo_Photo.jpg");
        let r = Rule { case_sensitive: true, ..rule("IMG", "Photo") };
        assert_eq!(one(&r, "img_IMG.jpg"), "img_Photo.jpg");
        // Regex characters are plain text here, and so is `$1`.
        assert_eq!(one(&rule("(1)", "$1"), "copy (1).txt"), "copy $1.txt");
        assert_eq!(one(&rule("x", ""), "xax"), "a");
    }

    #[test]
    fn regex_captures() {
        let r = Rule { regex: true, ..rule(r"^IMG_(\d+)$", "Photo $1") };
        assert_eq!(one(&r, "IMG_0042.JPG"), "Photo 0042.JPG");
        let r = Rule { regex: true, ..rule(r"(?<y>\d{4})-(?<m>\d\d)", "${m}.${y}") };
        assert_eq!(one(&r, "report 2024-03.pdf"), "report 03.2024.pdf");
        let r = Rule { regex: true, ..rule(r"(\w+) (\w+)", "${2}_$1") };
        assert_eq!(one(&r, "hello world.md"), "world_hello.md");
        let r = Rule { regex: true, ..rule(r"\d", "$$") };
        assert_eq!(one(&r, "a1b2"), "a$b$");
        // Capitals: ignored unless asked, also in regex mode.
        let r = Rule { regex: true, ..rule("^img", "x") };
        assert_eq!(one(&r, "IMG_1.jpg"), "x_1.jpg");
        let r = Rule { regex: true, case_sensitive: true, ..rule("^img", "x") };
        assert_eq!(one(&r, "IMG_1.jpg"), "IMG_1.jpg");
        // A bad pattern says why, in a line.
        let e = Rule { regex: true, ..rule("(unclosed", "") }.compile().err().unwrap();
        assert!(e.starts_with("Regex: ") && !e.contains('\n'), "{e}");
    }

    /// The counter counts the items Find matches, so ones it leaves alone don't use a number.
    #[test]
    fn the_counter_counts_only_matches() {
        let r = Rule { regex: true, ..rule(r"^IMG_\d+", "Rome {n}") };
        let p = run(&r, &["IMG_trip", "IMG_01.jpg", "notes.txt", "IMG_02.jpg"], dir(&[], false));
        assert_eq!(p.new, ["IMG_trip", "Rome 1.jpg", "notes.txt", "Rome 2.jpg"]);
        let p = run(&rule("IMG", "{n}"), &["a", "IMG_1", "IMG_2"], dir(&[], false));
        assert_eq!(p.new, ["a", "1_1", "2_2"]);
        // With nothing to find, every item counts.
        let p = run(&rule("", "{n}"), &["a", "b"], dir(&[], false));
        assert_eq!(p.new, ["1", "2"]);
    }

    #[test]
    fn plain_text_is_byte_safe_and_folds_only_ascii() {
        assert_eq!(one(&rule("é", "e"), "café É.txt"), "cafe É.txt");
        assert_eq!(one(&rule("CAFÉ", "x"), "café.txt"), "café.txt", "É and é aren't folded, as regex-lite doesn't");
        assert_eq!(one(&rule("caf", "x"), "CAFé.txt"), "xé.txt");
        assert_eq!(one(&rule("aa", "b"), "aaa"), "ba", "left to right, no overlaps");
    }

    #[test]
    fn tokens_expand_and_pad() {
        let c = Rule { start: "1".into(), step: "1".into(), ..rule("", "Trip {n:3}") }.compile().unwrap();
        let names: Vec<String> = (0..3).map(|i| c.apply("IMG_9.jpg", false, "Rome", i).0).collect();
        assert_eq!(names, ["Trip 001.jpg", "Trip 002.jpg", "Trip 003.jpg"]);
        let c = Rule { start: "10".into(), step: "5".into(), ..rule("", "{parent} {n} {name} [{ext}]") }.compile().unwrap();
        assert_eq!(c.apply("IMG_9.jpg", false, "Rome", 2).0, "Rome 20 IMG_9 [jpg].jpg");
        let c = Rule { start: "-1".into(), ..rule("", "{n:3}") }.compile().unwrap();
        assert_eq!(c.apply("a", false, "", 0).0, "-001");
        assert_eq!(c.apply("a", false, "", 1).0, "000");
        // Not a token: stays as typed. Too much padding isn't a token either.
        assert_eq!(one(&rule("", "{x} {n:99} {n"), "a.txt"), "{x} {n:99} {n.txt");
        // Tokens in regex mode are text, not `$` references, even when they hold a `$`.
        let r = Rule { regex: true, ..rule("^(.*)$", "$1 {name}") };
        assert_eq!(r.compile().unwrap().apply("a$1.txt", false, "", 0).0, "a$1 a$1.txt");
        // A bad number says so.
        assert_eq!(Rule { start: "x".into(), ..rule("", "{n}") }.compile().err().unwrap(), "Start must be a whole number");
        assert_eq!(Rule { step: "1.5".into(), ..rule("", "{n}") }.compile().err().unwrap(), "Step must be a whole number");
    }

    #[test]
    fn case_modes() {
        let with = |case| Rule { case, ..Rule::default() };
        assert_eq!(one(&with(CaseMode::Lower), "My Photo.JPG"), "my photo.JPG", "the extension is kept as it was");
        assert_eq!(one(&with(CaseMode::Upper), "My Photo.jpg"), "MY PHOTO.jpg");
        assert_eq!(one(&with(CaseMode::Title), "my_PHOTO-of rome don't.jpg"), "My_Photo-Of Rome Don't.jpg");
        assert_eq!(one(&with(CaseMode::Keep), "MiXeD.txt"), "MiXeD.txt");
        let r = Rule { include_ext: true, ..with(CaseMode::Lower) };
        assert_eq!(one(&r, "My Photo.JPG"), "my photo.jpg");
        assert_eq!(title_case("élan vital"), "Élan Vital");
    }

    #[test]
    fn the_extension_is_kept_unless_included() {
        assert_eq!(one(&rule("jpg", "png"), "jpg.jpg"), "png.jpg");
        let r = Rule { include_ext: true, ..rule("jpg", "png") };
        assert_eq!(one(&r, "jpg.jpg"), "png.png");
        assert_eq!(one(&rule("", "x"), "a.tar.gz"), "x.gz", "only the last extension");
        assert_eq!(one(&rule("bash", "zsh"), ".bashrc"), ".zshrc", "a leading dot isn't an extension");
        let c = rule("", "new").compile().unwrap();
        assert_eq!(c.apply("folder.d", true, "", 0).0, "new", "folders have no extension");
        let r = Rule { include_ext: true, ..rule("", "{name}.{ext}.bak") };
        assert_eq!(one(&r, "a.txt"), "a.txt.bak");
        // Nothing to find or put: nothing changes.
        assert_eq!(one(&rule("", ""), "a.txt"), "a.txt");
    }

    #[test]
    fn invalid_names_on_every_platform() {
        use Platform::*;
        for p in [Linux, Mac, Windows] {
            assert_eq!(invalid("", p).as_deref(), Some("Empty name"), "{p:?}");
            assert!(invalid(".", p).is_some() && invalid("..", p).is_some());
            assert!(invalid("a/b", p).unwrap().contains('/'));
            assert!(invalid("a\0b", p).unwrap().contains("NUL"));
            assert_eq!(invalid("photo 001.jpg", p), None);
            assert_eq!(invalid("...a", p), None);
            assert_eq!(invalid(&"x".repeat(255), p), None);
            assert!(invalid(&"x".repeat(256), p).unwrap().starts_with("Too long: 256"));
        }
        for c in ['<', '>', ':', '"', '\\', '|', '?', '*'] {
            let name = format!("a{c}b");
            assert!(invalid(&name, Windows).unwrap().contains(c), "{name}");
            assert_eq!(invalid(&name, Linux), None, "{name}");
            assert_eq!(invalid(&name, Mac), None, "{name}");
        }
        assert!(invalid("a\u{1}b", Windows).unwrap().contains("control"));
        for name in ["trailing.", "trailing "] {
            assert!(invalid(name, Windows).unwrap().contains("end with"), "{name}");
            assert_eq!(invalid(name, Linux), None);
        }
        for name in ["CON", "con.txt", "Prn", "aux.tar.gz", "NUL", "COM1", "com9.log", "LPT1", "lpt9", "COM¹", "nul .txt"] {
            assert!(invalid(name, Windows).unwrap().contains("reserved"), "{name}");
            assert_eq!(invalid(name, Linux), None, "{name}");
            assert_eq!(invalid(name, Mac), None, "{name}");
        }
        for name in ["CONSOLE", "COM10", "LPT0", "xcon"] {
            assert_eq!(invalid(name, Windows), None, "{name}");
        }
        // Lengths: bytes on Linux and macOS, UTF-16 units on Windows.
        let e = "é".repeat(200); // 400 bytes, 200 units
        assert!(invalid(&e, Linux).is_some() && invalid(&e, Mac).is_some());
        assert_eq!(invalid(&e, Windows), None);
    }

    #[test]
    fn duplicates_within_the_batch_are_problems() {
        let p = run(&rule("", "same"), &["a.txt", "b.txt", "c.jpg"], dir(&[], false));
        assert_eq!(p.status[0], Status::Problem("Same new name as “b.txt”".into()));
        assert_eq!(p.status[1], Status::Problem("Same new name as “a.txt”".into()));
        assert_eq!(p.status[2], Status::Change, "another extension: another name");
        assert_eq!((p.changes, p.problems), (1, 2));
        assert_eq!(p.summary(), "1 will change · 2 problems");
        let p = run(&rule("", "same"), &["a.txt", "b.txt", "c.txt"], dir(&[], false));
        assert_eq!(p.status, vec![Status::Problem("2 other items get this name too".into()); 3]);
        // Renaming onto a batch item that keeps its own name.
        let p = run(&rule("b", "a"), &["a.txt", "b.txt"], dir(&[], false));
        assert_eq!(p.status[0], Status::Same);
        assert!(matches!(&p.status[1], Status::Problem(w) if w.contains("“a.txt”")), "{:?}", p.status);
        // In different folders, one name is fine.
        let c = rule("", "same").compile().unwrap();
        let mut its = items(&["a.txt", "b.txt"]);
        its[1].dir = 1;
        let p = plan(&c, &its, &[dir(&[], false), dir(&[], false)], Platform::Linux);
        assert_eq!(p.status, [Status::Change, Status::Change]);
    }

    #[test]
    fn names_already_in_the_folder_are_problems_unless_their_item_moves() {
        let p = run(&rule("a", "b"), &["a.txt"], dir(&["b.txt"], false));
        assert_eq!(p.status[0], Status::Problem("“b.txt” already exists here".into()));
        // Swaps and chains: each name a batch item leaves is free.
        let p = run(&Rule { regex: true, ..rule("^(a|b)$", "x") }, &["a", "b"], dir(&[], false));
        assert_eq!(p.problems, 2);
        let swap = Rule { regex: true, ..rule("^.*$", "{n}") };
        let c = Rule { start: "2".into(), ..swap }.compile().unwrap();
        let p = plan(&c, &items(&["1", "2", "3"]), &[dir(&[], false)], Platform::Linux);
        assert_eq!(p.new, ["2", "3", "4"]);
        assert_eq!(p.status, [Status::Change, Status::Change, Status::Change]);
        // Not known yet: no check (the dialog doesn't let Apply run until it is).
        let p = run(&rule("a", "b"), &["a.txt"], Dir { known: false, ..dir(&["b.txt"], false) });
        assert_eq!(p.status[0], Status::Change);
    }

    #[test]
    fn case_only_clashes_count_only_where_the_disk_ignores_case() {
        // Upper for one, lower for the other: a clash only when case is ignored.
        let mixed = Rule { regex: true, case_sensitive: true, ..rule("^a(.*)$", "Z$1") };
        let names = ["a.txt", "z.txt"];
        let p = plan(&mixed.compile().unwrap(), &items(&names), &[dir(&[], false)], Platform::Linux);
        assert_eq!(p.status, [Status::Change, Status::Same]);
        let p = plan(&mixed.compile().unwrap(), &items(&names), &[dir(&[], true)], Platform::Mac);
        assert!(matches!(&p.status[0], Status::Problem(w) if w.contains("case is ignored")), "{:?}", p.status);
        // And against a name already there.
        let p = run(&rule("a", "B"), &["a.txt"], dir(&["b.txt"], true));
        assert!(matches!(&p.status[0], Status::Problem(w) if w.contains("differing only in case")), "{:?}", p.status);
        let p = run(&rule("a", "B"), &["a.txt"], dir(&["b.txt"], false));
        assert_eq!(p.status[0], Status::Change);
        // A case-only rename of an item itself is fine.
        let p = run(&Rule { case: CaseMode::Upper, ..Rule::default() }, &["a.txt"], dir(&[], true));
        assert_eq!((p.new[0].as_str(), &p.status[0]), ("A.txt", &Status::Change));
    }

    #[test]
    fn kept_items_hold_their_names() {
        let mut its = items(&["a", "b"]);
        its[0].keep = Some("its name isn't valid Unicode");
        let p = plan(&rule("b", "a").compile().unwrap(), &its, &[dir(&[], false)], Platform::Linux);
        assert_eq!(p.status[0], Status::Kept("its name isn't valid Unicode"));
        assert!(matches!(p.status[1], Status::Problem(_)));
        assert_eq!(p.summary(), "Nothing will change · 1 problem");
    }

    /// Every key while the dialog is open runs the whole plan: `cargo test --release --
    /// --ignored --nocapture plan_speed`.
    #[test]
    #[ignore = "benchmark"]
    fn plan_speed() {
        let names: Vec<String> = (0..10_000).map(|i| format!("IMG_{i:05} holiday in Rome.JPG")).collect();
        let its: Vec<Item> = names.iter().map(|n| Item { name: n.clone(), is_dir: false, dir: 0, keep: None, gone: false }).collect();
        let others: Vec<String> = (0..10_000).map(|i| format!("other {i}.txt")).collect();
        let d = Dir { name: "Rome".into(), others: others.into_iter().collect(), ignores_case: false, known: true };
        let cases = [
            ("plain, nothing typed", rule("", "")),
            ("plain 'Rome' → 'Paris'", rule("Rome", "Paris")),
            ("regex captures + {n:4}", Rule { regex: true, ..rule(r"^IMG_(\d+) (.*)$", "$2 {n:4} ($1)") }),
            ("template {parent} {n:3}, Title", Rule { case: CaseMode::Title, ..rule("", "{parent} {n:3}") }),
        ];
        for (what, r) in cases {
            let c = r.compile().unwrap();
            for ignores_case in [false, true] {
                let d = Dir { ignores_case, ..d.clone() };
                let mut t: Vec<f64> = (0..15)
                    .map(|_| {
                        let t = std::time::Instant::now();
                        std::hint::black_box(plan(&c, &its, std::slice::from_ref(&d), Platform::Linux));
                        t.elapsed().as_secs_f64() * 1000.0
                    })
                    .collect();
                t.sort_by(f64::total_cmp);
                println!("10k items, {what}, ignores case {ignores_case}: median {:.2} ms, max {:.2} ms", t[7], t[14]);
            }
        }
    }
}
