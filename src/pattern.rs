//! Glob patterns, for Select by Pattern (names) and `jump.exclude` (paths). No regex: Arbor
//! adds no crate for it, and globs cover what selecting files needs.
//!
//! - `*` any run of characters, `?` one character, `[abc]`, `[a-z]` and `[!abc]` (or `[^abc]`)
//!   one character from (or not from) a set, `{png,jpg}` either alternative (nested braces
//!   work; at most [`MAX_ALTS`] expansions).
//! - A name has to match whole: `IMG_*` is a prefix, `*.jpg` a suffix.
//! - Paths (`Glob::path`): `*` and `?` stay within one folder name, `**` crosses folders, and a
//!   pattern ending in `/**` also matches the folder itself. On Windows `\` and `/` are one.
//! - No escapes (a backslash is a path separator on Windows): `[*]` matches a literal `*`.
//!   An unclosed `[` or `{` is literal.
//! - Case: [`Case::platform`] is insensitive on macOS and Windows and sensitive on Linux, as
//!   each platform's usual file system is.

/// The most alternatives one pattern's braces may expand to.
pub const MAX_ALTS: usize = 256;

/// The longest pattern, in characters: more than any name or path (Linux's `PATH_MAX` is 4096
/// bytes). Finding where braces and classes close takes time growing with the square of the
/// length when they never do, so an absurd pattern (a damaged `jump.exclude`, which the startup
/// read compiles) is refused rather than left to run for minutes.
pub const MAX_LEN: usize = 4096;

#[cfg(test)]
thread_local! {
    /// Test only: units of matching and parsing work done on this thread, so tests bound the
    /// work itself, which a busy machine can't stretch as it stretches time.
    static STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Work counted locally (a character looked at, a loop turned) and added to `STEPS` once, when
/// dropped: no cost per step, and none at all outside tests.
struct Work(usize);

impl Drop for Work {
    fn drop(&mut self) {
        #[cfg(test)]
        STEPS.with(|s| s.set(s.get() + self.0 as u64));
        #[cfg(not(test))]
        let _ = self.0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    Sensitive,
    Insensitive,
}

impl Case {
    /// What this platform's usual file system does: APFS and NTFS ignore case, ext4 doesn't.
    pub fn platform() -> Case {
        if cfg!(any(target_os = "macos", windows)) { Case::Insensitive } else { Case::Sensitive }
    }

    /// The setting (`Some(true)`: sensitive), or the platform's default.
    pub fn from_setting(sensitive: Option<bool>) -> Case {
        match sensitive {
            Some(true) => Case::Sensitive,
            Some(false) => Case::Insensitive,
            None => Case::platform(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Lit(char),
    One,
    Star,
    /// `**`: any run, separators included (paths only; in a name it's `*`).
    Deep,
    Class {
        neg: bool,
        items: Vec<(char, char)>,
    },
    /// A path separator (paths only).
    Sep,
}

/// A name pattern of only literals and stars, matched with plain string tests: most
/// patterns people type (`*.jpg`, `IMG_*`, `IMG_*.jpg`, `*draft*`).
#[derive(Clone, Debug, PartialEq)]
enum Fast {
    Exact(String),
    Prefix(String),
    Suffix(String),
    Contains(String),
    Affix(String, String),
    /// More stars, anchored by a prefix or suffix (`IMG_*1*.jpg`): pieces that must follow one
    /// another in order between them. Finding each piece at its first place after the last is
    /// always right when only stars come between, so this is linear, where the general matcher
    /// backtracks on every near miss (6.4 ms against 1 ms for 50,000 names, `match_speed`).
    /// Unanchored ones (`*a*b*c*`) stay with the general matcher, which measured faster there.
    Pieces(String, Vec<Piece>, String),
}

/// A literal between two stars: one character (found with a byte search, far cheaper per name
/// than setting up a substring search), or more.
#[derive(Clone, Debug, PartialEq)]
enum Piece {
    Char(char),
    Str(String),
}

impl Fast {
    fn of(toks: &[Tok]) -> Option<Fast> {
        let lit = |t: &[Tok]| t.iter().map(|t| if let Tok::Lit(c) = t { Some(*c) } else { None }).collect::<Option<String>>();
        let stars: Vec<usize> = toks.iter().enumerate().filter(|(_, t)| matches!(t, Tok::Star | Tok::Deep)).map(|(i, _)| i).collect();
        let n = toks.len();
        Some(match stars.as_slice() {
            [] => Fast::Exact(lit(toks)?),
            [0] => Fast::Suffix(lit(&toks[1..])?),
            [i] if *i == n - 1 => Fast::Prefix(lit(&toks[..n - 1])?),
            [i] => Fast::Affix(lit(&toks[..*i])?, lit(&toks[i + 1..])?),
            [0, j] if *j == n - 1 => Fast::Contains(lit(&toks[1..n - 1])?),
            _ if stars[0] > 0 || stars[stars.len() - 1] < n - 1 => {
                let (first, last) = (stars[0], stars[stars.len() - 1]);
                let middles = stars.windows(2).map(|w| lit(&toks[w[0] + 1..w[1]])).collect::<Option<Vec<String>>>()?;
                let piece = |m: String| {
                    let mut cs = m.chars();
                    match (cs.next(), cs.next()) {
                        (Some(c), None) => Piece::Char(c),
                        _ => Piece::Str(m),
                    }
                };
                let mids = middles.into_iter().filter(|m| !m.is_empty()).map(piece).collect();
                Fast::Pieces(lit(&toks[..first])?, mids, lit(&toks[last + 1..])?)
            }
            _ => return None,
        })
    }

    fn matches(&self, s: &str) -> bool {
        // The string tests are the standard library's, linear in `s`.
        let mut w = Work(s.len() + 1);
        match self {
            Fast::Exact(x) => s == x,
            Fast::Prefix(x) => s.starts_with(x.as_str()),
            Fast::Suffix(x) => s.ends_with(x.as_str()),
            Fast::Contains(x) => s.contains(x.as_str()),
            Fast::Affix(a, b) => s.len() >= a.len() + b.len() && s.starts_with(a.as_str()) && s.ends_with(b.as_str()),
            Fast::Pieces(a, mids, b) => {
                if s.len() < a.len() + b.len() || !s.starts_with(a.as_str()) || !s.ends_with(b.as_str()) {
                    return false;
                }
                let mut rest = &s[a.len()..s.len() - b.len()];
                for m in mids {
                    let found = match m {
                        Piece::Char(c) => rest.find(*c).map(|i| i + c.len_utf8()),
                        Piece::Str(x) => rest.find(x.as_str()).map(|i| i + x.len()),
                    };
                    // Each search starts where the last ended: what it passes over, it never sees again.
                    w.0 += found.unwrap_or(rest.len()) + 1;
                    match found {
                        Some(end) => rest = &rest[end..],
                        None => return false,
                    }
                }
                true
            }
        }
    }
}

#[derive(Clone, Debug)]
struct Alt {
    toks: Vec<Tok>,
    fast: Option<Fast>,
    /// Every token is ASCII, so an ASCII name can be matched byte by byte.
    ascii: bool,
}

#[derive(Clone, Debug)]
pub struct Glob {
    alts: Vec<Alt>,
    case: Case,
    path: bool,
}

impl Glob {
    /// A pattern for one name.
    pub fn name(pattern: &str, case: Case) -> Result<Glob, String> {
        Glob::build(pattern, case, false)
    }

    /// A pattern for a whole path (`jump.exclude`); `~` at the start is the home folder.
    pub fn path(pattern: &str, case: Case) -> Result<Glob, String> {
        let p = crate::fs::expand(pattern.trim());
        Glob::build(&p.to_string_lossy(), case, true)
    }

    fn build(pattern: &str, case: Case, path: bool) -> Result<Glob, String> {
        if pattern.chars().nth(MAX_LEN).is_some() {
            return Err(format!("The pattern is longer than {MAX_LEN} characters"));
        }
        let folded = match case {
            Case::Insensitive => pattern.to_lowercase(),
            Case::Sensitive => pattern.to_string(),
        };
        let mut toks = Vec::new();
        for p in expand_braces(&folded)? {
            toks.push(tokens(&p, path));
            // "dir/**" matches "dir" too.
            if path && let Some(dir) = p.strip_suffix("/**").or_else(|| p.strip_suffix("\\**")).filter(|d| !d.is_empty()) {
                toks.push(tokens(dir, path));
            }
        }
        let alts = toks
            .into_iter()
            .map(|toks| {
                let fast = if path { None } else { Fast::of(&toks) };
                let ascii = toks.iter().all(|t| match t {
                    Tok::Lit(c) => c.is_ascii(),
                    Tok::Class { items, .. } => items.iter().all(|(a, b)| a.is_ascii() && b.is_ascii()),
                    _ => true,
                });
                Alt { toks, fast, ascii }
            })
            .collect();
        Ok(Glob { alts, case, path })
    }

    pub fn case(&self) -> Case {
        self.case
    }

    /// Whether `s` (a name, or a path for a path pattern) matches.
    pub fn matches(&self, s: &str) -> bool {
        match self.case {
            Case::Insensitive => self.matches_folded(&s.to_lowercase()),
            Case::Sensitive => self.matches_folded(s),
        }
    }

    /// [`matches`](Self::matches) for a subject already lowercased (`str::to_lowercase`) when
    /// the pattern ignores case, as a listing's `Entry::lower` is: no allocation per name.
    pub fn matches_folded(&self, s: &str) -> bool {
        if self.path {
            let chars: Vec<char> = s.chars().collect();
            return self.alts.iter().any(|a| match_path(&a.toks, &chars, self.case));
        }
        let ascii = s.is_ascii();
        self.alts.iter().any(|a| match &a.fast {
            Some(f) => f.matches(s),
            None if ascii && a.ascii => match_name_ascii(&a.toks, s.as_bytes(), self.case),
            None => match_name(&a.toks, s, self.case),
        })
    }
}

/// `{a,b}` expanded into each alternative (nested ones too), at most [`MAX_ALTS`], in order.
/// A worklist rather than recursion: a pattern can hold a couple of thousand groups.
fn expand_braces(p: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    // Patterns still to expand, the next one last.
    let mut todo: Vec<Vec<char>> = vec![p.chars().collect()];
    while let Some(p) = todo.pop() {
        match first_group(&p) {
            None => {
                out.push(p.iter().collect());
                if out.len() > MAX_ALTS {
                    return Err(format!("More than {MAX_ALTS} alternatives in {{…}}"));
                }
            }
            Some((open, end, commas)) => {
                let mut from = open + 1;
                let mut parts = Vec::new();
                let mut w = Work(0);
                for c in commas.into_iter().chain([end]) {
                    let mut next: Vec<char> = p[..open].to_vec();
                    next.extend_from_slice(&p[from..c]);
                    next.extend_from_slice(&p[end + 1..]);
                    w.0 += next.len() + 1;
                    parts.push(next);
                    from = c + 1;
                }
                // Each one waiting can only add alternatives: past the limit, it's refused.
                if todo.len() + parts.len() > MAX_ALTS * 4 {
                    return Err(format!("More than {MAX_ALTS} alternatives in {{…}}"));
                }
                todo.extend(parts.into_iter().rev());
            }
        }
    }
    Ok(out)
}

/// The first top-level `{` that closes (skipping `[...]` classes): where it opens and closes,
/// and its top-level commas.
fn first_group(p: &[char]) -> Option<(usize, usize, Vec<usize>)> {
    let next = closes(p);
    let mut w = Work(0);
    let mut i = 0;
    while i < p.len() {
        w.0 += 1;
        match p[i] {
            '[' => i = class_end(p, &next, i).map_or(i + 1, |e| e + 1),
            '{' => {
                if let Some((end, commas)) = brace_end(p, &next, i) {
                    return Some((i, end, commas));
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// The `}` matching the `{` at `open`, and the top-level commas between them.
fn brace_end(p: &[char], next: &[usize], open: usize) -> Option<(usize, Vec<usize>)> {
    let (mut depth, mut commas, mut i) = (0, Vec::new(), open + 1);
    let mut w = Work(0);
    while i < p.len() {
        w.0 += 1;
        match p[i] {
            '[' => {
                i = class_end(p, next, i).map_or(i + 1, |e| e + 1);
                continue;
            }
            '{' => depth += 1,
            '}' if depth == 0 => return Some((i, commas)),
            '}' => depth -= 1,
            ',' if depth == 0 => commas.push(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// For each position in `p`, the first `]` at or after it (`usize::MAX`: none), so finding
/// where a class ends takes no scan: one that never closes would otherwise make every `[` and
/// `{` after it look to the end of the pattern again.
fn closes(p: &[char]) -> Vec<usize> {
    let _w = Work(p.len() + 1);
    let mut next = vec![usize::MAX; p.len() + 1];
    for j in (0..p.len()).rev() {
        next[j] = if p[j] == ']' { j } else { next[j + 1] };
    }
    next
}

/// The `]` closing the class at `open` (`next` from [`closes`]). A `]` right after `[` or `[!`
/// is a member.
fn class_end(p: &[char], next: &[usize], open: usize) -> Option<usize> {
    let mut i = open + 1;
    if matches!(p.get(i), Some('!' | '^')) {
        i += 1;
    }
    if p.get(i) == Some(&']') {
        i += 1;
    }
    Some(next[i.min(p.len())]).filter(|&j| j != usize::MAX)
}

fn is_sep(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

fn tokens(p: &str, path: bool) -> Vec<Tok> {
    let chars: Vec<char> = p.chars().collect();
    let next = closes(&chars);
    // Each turn of the loop consumes at least one character (a class, all of its own).
    let _w = Work(chars.len() + 1);
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                let mut n = 1;
                while chars.get(i + n) == Some(&'*') {
                    n += 1;
                }
                i += n;
                let t = if path && n >= 2 { Tok::Deep } else { Tok::Star };
                // Runs of stars are one.
                if !matches!(out.last(), Some(Tok::Star | Tok::Deep)) {
                    out.push(t);
                } else if t == Tok::Deep {
                    *out.last_mut().expect("just checked") = Tok::Deep;
                }
                continue;
            }
            '?' => out.push(Tok::One),
            '[' => {
                if let Some(end) = class_end(&chars, &next, i) {
                    let mut j = i + 1;
                    let neg = matches!(chars[j], '!' | '^');
                    if neg {
                        j += 1;
                    }
                    let mut items = Vec::new();
                    while j < end {
                        if j + 2 < end && chars[j + 1] == '-' {
                            items.push((chars[j], chars[j + 2]));
                            j += 3;
                        } else {
                            items.push((chars[j], chars[j]));
                            j += 1;
                        }
                    }
                    out.push(Tok::Class { neg, items });
                    i = end + 1;
                    continue;
                }
                out.push(Tok::Lit('['));
            }
            c if path && is_sep(c) => out.push(Tok::Sep),
            c => out.push(Tok::Lit(c)),
        }
        i += 1;
    }
    out
}

/// One character against a non-star token. In a path, `?` and classes never match a separator.
fn one(t: &Tok, c: char, case: Case, path: bool) -> bool {
    match t {
        Tok::Lit(l) => *l == c,
        Tok::Sep => is_sep(c),
        Tok::One => !(path && is_sep(c)),
        Tok::Class { neg, items } => {
            if path && is_sep(c) {
                return false;
            }
            let within = |c: char| items.iter().any(|&(a, b)| a <= c && c <= b);
            // The subject is lowercased; `[A-Z]` should still match it.
            let hit = within(c) || (case == Case::Insensitive && c.to_uppercase().any(within));
            hit != *neg
        }
        Tok::Star | Tok::Deep => true,
    }
}

/// A name against tokens: the classic greedy match, going back to the last star on a
/// mismatch. Linear for most patterns, with no allocation.
fn match_name(toks: &[Tok], s: &str, case: Case) -> bool {
    let (mut ti, mut si) = (0usize, 0usize);
    // (token after the last star, where in `s` that star's run ends now)
    let mut back: Option<(usize, usize)> = None;
    let mut w = Work(0);
    loop {
        w.0 += 1;
        let c = s[si..].chars().next();
        match (toks.get(ti), c) {
            (Some(Tok::Star | Tok::Deep), _) => {
                ti += 1;
                back = Some((ti, si));
                continue;
            }
            (Some(t), Some(c)) if one(t, c, case, false) => {
                ti += 1;
                si += c.len_utf8();
                continue;
            }
            (None, None) => return true,
            _ => {}
        }
        // Mismatch: let the last star take one more character.
        match back {
            Some((bt, bs)) if bs < s.len() => {
                let n = s[bs..].chars().next().map_or(1, char::len_utf8);
                back = Some((bt, bs + n));
                ti = bt;
                si = bs + n;
            }
            _ => return false,
        }
    }
}

/// [`match_name`] for an ASCII name and ASCII tokens: the same, a byte at a time.
fn match_name_ascii(toks: &[Tok], s: &[u8], case: Case) -> bool {
    let (mut ti, mut si) = (0usize, 0usize);
    let mut back: Option<(usize, usize)> = None;
    // Turns of the loop, and bytes the skips look at.
    let mut w = Work(0);
    loop {
        w.0 += 1;
        match (toks.get(ti), s.get(si)) {
            (Some(Tok::Star | Tok::Deep), _) => {
                ti += 1;
                // A literal next: skip straight to its next occurrence.
                if let Some(Tok::Lit(c)) = toks.get(ti) {
                    let k = s[si..].iter().position(|&b| b == *c as u8);
                    w.0 += k.unwrap_or(s.len() - si);
                    match k {
                        Some(k) => si += k,
                        None => return false,
                    }
                }
                back = Some((ti, si));
                continue;
            }
            (Some(t), Some(&b)) if one(t, b as char, case, false) => {
                ti += 1;
                si += 1;
                continue;
            }
            (None, None) => return true,
            _ => {}
        }
        match back {
            Some((bt, bs)) if bs < s.len() => {
                let mut next = bs + 1;
                if let Some(Tok::Lit(c)) = toks.get(bt) {
                    let k = s[next..].iter().position(|&b| b == *c as u8);
                    w.0 += k.unwrap_or(s.len() - next);
                    match k {
                        Some(k) => next += k,
                        None => return false,
                    }
                }
                back = Some((bt, next));
                ti = bt;
                si = next;
            }
            _ => return false,
        }
    }
}

/// A path against tokens, where `*` can't cross a separator and `**` can. The greedy match
/// can't go back past a star that stopped at a separator, so this goes token by token over every
/// position at once: `can[i]` says the tokens so far match the path's first `i` characters. No
/// recursion (a path can be 32,767 characters on Windows, whose main thread has 1 MB of stack),
/// two rows of memory, and time tokens × length, stopping early once nothing can match.
fn match_path(toks: &[Tok], s: &[char], case: Case) -> bool {
    let n = s.len();
    let mut w = Work(2 * (n + 1));
    let mut can = vec![false; n + 1];
    let mut next = vec![false; n + 1];
    can[0] = true;
    for tok in toks {
        // A pass over the row, and a look for anything still matching.
        w.0 += 2 * (n + 1);
        match tok {
            // Any run within a folder name: from each reachable point, on to the next separator.
            Tok::Star => {
                next[0] = can[0];
                for i in 1..=n {
                    next[i] = can[i] || (next[i - 1] && !is_sep(s[i - 1]));
                }
            }
            // Any run at all.
            Tok::Deep => {
                next[0] = can[0];
                for i in 1..=n {
                    next[i] = can[i] || next[i - 1];
                }
            }
            t => {
                next[0] = false;
                for i in 1..=n {
                    next[i] = can[i - 1] && one(t, s[i - 1], case, true);
                }
            }
        }
        std::mem::swap(&mut can, &mut next);
        if !can.contains(&true) {
            return false;
        }
    }
    can[n]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, s: &str) -> bool {
        Glob::name(p, Case::Sensitive).unwrap().matches(s)
    }

    fn mi(p: &str, s: &str) -> bool {
        Glob::name(p, Case::Insensitive).unwrap().matches(s)
    }

    #[test]
    fn stars_and_question_marks() {
        assert!(m("*.jpg", "a.jpg") && m("*.jpg", ".jpg") && !m("*.jpg", "a.jpeg") && !m("*.jpg", "a.jpg.txt"));
        assert!(m("IMG_*", "IMG_0001.HEIC") && !m("IMG_*", "xIMG_1"));
        assert!(m("*", "") && m("*", "anything") && m("**", "x"));
        assert!(m("a*b*c", "aXbYc") && m("a*b*c", "abbbc") && !m("a*b*c", "acb"));
        assert!(m("?", "é") && !m("?", "") && !m("?", "ab") && m("??.txt", "ab.txt"));
        assert!(m("*a", "aaaa") && m("*ab", "aab") && !m("*ab", "aba"));
        assert!(m("report", "report") && !m("report", "report.pdf"), "a name matches whole");
    }

    #[test]
    fn classes() {
        assert!(m("[abc].txt", "b.txt") && !m("[abc].txt", "d.txt"));
        assert!(m("file[0-9]", "file7") && !m("file[0-9]", "filex"));
        assert!(m("[!abc]", "d") && !m("[!abc]", "a") && m("[^0-9]x", "ax"));
        assert!(m("[]]", "]") && m("[*]", "*") && !m("[*]", "a"));
        assert!(m("[ab", "[ab"), "an unclosed class is literal");
    }

    /// Patterns of literals and stars take plain string tests; the rest the general matcher,
    /// a byte at a time for ASCII. All three agree.
    #[test]
    fn fast_paths_agree_with_the_general_matcher() {
        let names = ["", "a", "a.jpg", "IMG_1.jpg", "IMG_.jpg", "xIMG_1.jpgx", "draft-final", "ünï.jpg", "ab", "aXbYc"];
        for pat in ["*.jpg", "IMG_*", "IMG_*.jpg", "*draft*", "a", "*", "a*b*c", "*a?b*", "[a-z]*", "ü*", "*.j?g"] {
            let g = Glob::name(pat, Case::Sensitive).unwrap();
            for n in names {
                let general = g.alts.iter().any(|a| match_name(&a.toks, n, Case::Sensitive));
                assert_eq!(g.matches(n), general, "{pat} on {n}");
                if n.is_ascii() && g.alts.iter().all(|a| a.ascii) {
                    let ascii = g.alts.iter().any(|a| match_name_ascii(&a.toks, n.as_bytes(), Case::Sensitive));
                    assert_eq!(ascii, general, "{pat} on {n} (ascii)");
                }
            }
        }
        assert_eq!(Fast::of(&tokens("IMG_*.jpg", false)), Some(Fast::Affix("IMG_".into(), ".jpg".into())));
        assert_eq!(Fast::of(&tokens("*.jpg", false)), Some(Fast::Suffix(".jpg".into())));
        assert_eq!(Fast::of(&tokens("a?*", false)), None);
        assert_eq!(
            Fast::of(&tokens("IMG_*1**.jpg", false)),
            Some(Fast::Pieces("IMG_".into(), vec![Piece::Char('1')], ".jpg".into())),
            "an empty piece between stars is no piece"
        );
        // Pieces in order, not overlapping the prefix or suffix, and found anywhere between.
        let n = |p: &str, s: &str| Glob::name(p, Case::Sensitive).unwrap().matches(s);
        assert!(n("a*b*c", "abc") && n("a*b*c", "axxbyyc") && n("*é*ü*", "xéyüz"));
        assert!(!n("a*b*c", "acb") && !n("ab*b*ba", "aba") && !n("a*bb*c", "abc") && !n("*x*y*", "yx"));
        assert!(n("ab*b*ba", "abbba") && n("*aa*aa*", "aaaa") && !n("*aa*aa*", "aaa"));
        // "IMG_*IMG_" needs both ends to fit without overlapping.
        assert!(!m("IMG_*IMG_", "IMG_"));
    }

    #[test]
    fn braces() {
        let g = Glob::name("*.{png,jpg}", Case::Sensitive).unwrap();
        assert!(g.matches("a.png") && g.matches("b.jpg") && !g.matches("c.gif") && !g.matches("d.{png,jpg}"));
        assert!(m("{a,b{c,d}}x", "bdx") && m("{a,b{c,d}}x", "ax") && !m("{a,b{c,d}}x", "bx"));
        assert!(m("{,pre}fix", "fix") && m("{,pre}fix", "prefix"));
        assert!(m("{x", "{x"), "an unclosed brace is literal");
        assert!(m("[{]a,b}", "{a,b}"), "a brace in a class isn't a group");
        let many = "{a,b,c,d}{a,b,c,d}{a,b,c,d}{a,b,c,d}{a,b}";
        assert!(Glob::name(many, Case::Sensitive).is_err());
    }

    #[test]
    fn case_rules() {
        assert!(!m("*.JPG", "a.jpg") && !m("img_*", "IMG_1"));
        assert!(mi("*.JPG", "a.jpg") && mi("img_*", "IMG_1") && mi("[A-C]*", "beta") && mi("[a-c]*", "Beta"));
        assert!(!mi("[!A-C]*", "beta"));
        let g = Glob::name("*.JPG", Case::Insensitive).unwrap();
        assert!(g.matches_folded("photo.jpg"));
        assert_eq!(Case::platform() == Case::Insensitive, cfg!(any(target_os = "macos", windows)));
        assert_eq!(Case::from_setting(Some(true)), Case::Sensitive);
        assert_eq!(Case::from_setting(Some(false)), Case::Insensitive);
        assert_eq!(Case::from_setting(None), Case::platform());
    }

    #[cfg(unix)]
    #[test]
    fn paths() {
        let p = |pat: &str, s: &str| Glob::path(pat, Case::Sensitive).unwrap().matches(s);
        assert!(p("/tmp/*", "/tmp/x") && !p("/tmp/*", "/tmp/x/y") && !p("/tmp/*", "/tmp"));
        assert!(p("/tmp/**", "/tmp/x/y") && p("/tmp/**", "/tmp") && !p("/tmp/**", "/tmpx"));
        assert!(p("**/node_modules/**", "/a/b/node_modules/c") && p("**/node_modules", "/x/node_modules"));
        assert!(!p("/a/?", "/a//") && p("/a/?", "/a/b"));
        let home = crate::fs::home();
        assert!(p("~/Downloads/*", &format!("{}/Downloads/x", home.display())));
        assert!(Glob::path("/TMP/**", Case::Insensitive).unwrap().matches("/tmp/a"));
    }

    /// Matching every name of a 50k folder must fit well inside a frame. The names are
    /// lowercased first, as a listing's `Entry::lower` is (what `matches_folded` expects for a
    /// pattern that ignores case), and each count is checked, so a pattern that matches
    /// nothing can't pass for a fast one.
    #[test]
    #[ignore = "benchmark"]
    fn match_speed() {
        let names: Vec<String> =
            (0..50_000).map(|i| format!("IMG_{i:05} some longer name part.{}", ["jpg", "png", "txt"][i % 3]).to_lowercase()).collect();
        for (pat, want) in [("*.jpg", 16_667), ("IMG_*1*.{png,jpg}", 15_844), ("*a*b*c*d*", 0)] {
            let g = Glob::name(pat, Case::Insensitive).unwrap();
            // The best of 20, so a busy machine's noise doesn't decide.
            let (mut best, mut n) = (f64::MAX, 0);
            for _ in 0..20 {
                let t = std::time::Instant::now();
                n = names.iter().filter(|s| g.matches_folded(s)).count();
                best = best.min(t.elapsed().as_secs_f64() * 1e3);
            }
            println!("{pat}: {n} of 50k in {best:.2} ms (best of 20)");
            assert_eq!(n, want, "{pat}");
        }
        // A regex (regex-lite) for comparison: why Select by Pattern has no `re:` yet.
        for (pat, want) in [(r"\.jpg$", 16_667), (r"^img_\d*1", 26_281)] {
            let re = regex_lite::RegexBuilder::new(pat).case_insensitive(true).build().unwrap();
            let (mut best, mut n) = (f64::MAX, 0);
            for _ in 0..20 {
                let t = std::time::Instant::now();
                n = names.iter().filter(|s| re.is_match(s)).count();
                best = best.min(t.elapsed().as_secs_f64() * 1e3);
            }
            println!("re:{pat}: {n} of 50k in {best:.2} ms (best of 20)");
            assert_eq!(n, want, "{pat}");
        }
    }

    /// The plainest possible matcher, for checking the fast ones: tries every split.
    fn naive(toks: &[Tok], s: &[char], case: Case, path: bool) -> bool {
        match toks.first() {
            None => s.is_empty(),
            Some(Tok::Deep) if path => (0..=s.len()).any(|k| naive(&toks[1..], &s[k..], case, path)),
            Some(Tok::Star | Tok::Deep) => {
                (0..=s.len()).take_while(|&k| k == 0 || !(path && is_sep(s[k - 1]))).any(|k| naive(&toks[1..], &s[k..], case, path))
            }
            Some(t) => !s.is_empty() && one(t, s[0], case, path) && naive(&toks[1..], &s[1..], case, path),
        }
    }

    const PAT: &[&str] =
        &["*", "**", "?", "a", "b", "A", ".", "/", "[ab]", "[!a]", "[a-c]", "[", "]", "{", "}", ",", "{a,b}", "é", "ß", "İ", "-"];
    const SUBJ: &[&str] = &["a", "b", "A", "B", ".", "/", "c", "é", "É", "ß", "İ", "i\u{307}", "-", "[", "*"];

    /// Any pattern against any name: the fast paths, the byte-wise matcher and the general one
    /// all agree with the naive matcher, for both case rules, names and paths. Never a panic.
    #[test]
    fn any_glob_matches_as_the_naive_matcher_does() {
        crate::prop::check("glob_agree", 4000, |r| {
            let pat = r.from(PAT, 8);
            let subj = r.from(SUBJ, 10);
            for case in [Case::Sensitive, Case::Insensitive] {
                for path in [false, true] {
                    let Ok(g) = Glob::build(&pat, case, path) else { continue };
                    let folded = if case == Case::Insensitive { subj.to_lowercase() } else { subj.clone() };
                    let chars: Vec<char> = folded.chars().collect();
                    let want = g.alts.iter().any(|a| naive(&a.toks, &chars, case, path));
                    assert_eq!(g.matches(&subj), want, "{pat:?} {subj:?} {case:?} path {path}");
                }
            }
        });
    }

    /// Runs `f` and returns its answer and the work it did on this thread.
    fn counted<T>(f: impl FnOnce() -> T) -> (T, u64) {
        STEPS.with(|s| s.set(0));
        let r = f();
        (r, STEPS.with(|s| s.get()))
    }

    /// The most work matching `s` against `g` can take: linear in the subject for each
    /// alternative, never exponential or quadratic in it.
    ///
    /// - A name, `t` tokens against `n` bytes: the greedy matchers turn their loop at most
    ///   `(n + 1)(t + 1) + 1` times, as only the last star is ever gone back to, where it starts
    ///   (`bs`) never moves back, each going back moves it on, and between two the tokens only go
    ///   forward. The byte-wise one's skips look at each byte at most once (each starts at or
    ///   past `bs` and ends where the next `bs` is), and its last skip at most the rest: `2n`.
    ///   The plain string tests are linear. All within `2(n + 2)(t + 2)`.
    /// - A path: two passes over a row of `n + 1` per token, and two rows: `2(n + 1)(t + 1)`.
    fn match_bound(g: &Glob, s: &str) -> u64 {
        let n = if g.path { s.chars().count() } else { s.len() } as u64;
        g.alts
            .iter()
            .map(|a| {
                let t = a.toks.len() as u64;
                if g.path { 2 * (n + 1) * (t + 1) } else { 2 * (n + 2) * (t + 2) }
            })
            .sum()
    }

    /// Patterns that make a backtracking matcher take exponential time, and brace bombs: each
    /// answers within its bound of work, against names and paths far longer than real ones.
    /// Work is counted, not timed, so a loaded machine can't fail it; the generous time limits
    /// only catch a hang.
    #[test]
    fn pathological_globs_are_bounded() {
        use std::time::Duration;
        let check = |g: &Glob, s: &str, what: &str| {
            let (hit, steps) = counted(|| g.matches(s));
            assert!(!hit, "{what}");
            let bound = match_bound(g, s);
            assert!(steps <= bound, "{what}: {steps} steps, over the bound of {bound}");
        };
        let long_a = "a".repeat(10_000);
        let long_path = format!("/{}", vec!["aaaa"; 2_000].join("/"));
        for pat in ["*a*a*a*a*a*b", "*a*a*a*a*a*a*a*a*a*a*a*a*b*", "?*?*?*?*?*?*?*b", "*[ab]*[ab]*[ab]*[ab]*c", "IMG_*a*a*a*a*b.jpg"] {
            for case in [Case::Sensitive, Case::Insensitive] {
                let g = Glob::name(pat, case).unwrap();
                check(&g, &long_a, pat);
                check(&g, &format!("{long_a}é"), &format!("{pat}: the general matcher too"));
            }
        }
        for pat in ["/**a**a**a**a**b", "/*/*/*/*/*/*/b/**", "**/a*a*a*a*b"] {
            check(&Glob::path(pat, Case::Sensitive).unwrap(), &long_path, pat);
        }
        // A brace bomb stops at the limit. Each pass over the worklist costs at most `6(L + 1)`
        // for a pattern of `L` characters (finding where its first group closes, splitting it in
        // two, and the alternative's tokens). Each split adds one item waiting and each answer
        // takes one away, and the expansion is refused past `MAX_ALTS` answers or `4 MAX_ALTS`
        // waiting, so there are at most `6 MAX_ALTS + 2` passes, where all 2³⁰ would be exponential.
        let bomb = "{a,b}".repeat(30);
        let bound = (6 * MAX_ALTS as u64 + 2) * 6 * (bomb.len() as u64 + 1);
        let (refused, steps) =
            crate::prop::within(Duration::from_secs(10), "a brace bomb", move || counted(|| Glob::name(&bomb, Case::Sensitive).is_err()));
        assert!(refused);
        assert!(steps <= bound, "a brace bomb: {steps} steps, over the bound of {bound}");
        // Deep or unclosed braces and classes take at most `L²`, as `MAX_LEN` says: an unclosed
        // `{` looks to the end once (`L²/2` for all of them, `L²/4` between classes); each of
        // `L/2` nested groups is found, split and peeled in three passes over what's left
        // (`3L²/4`); where a class closes is looked up, not scanned for. Cubic would be `L³`.
        let n = MAX_LEN;
        for (what, pat) in [
            ("deep braces", format!("{}x{}", "{".repeat(n / 2 - 1), "}".repeat(n / 2 - 1))),
            ("unclosed braces", "{".repeat(n)),
            ("unclosed classes", "[".repeat(n)),
            ("classes and braces", "[{".repeat(n / 2)),
        ] {
            let len = pat.chars().count() as u64;
            let (_, steps) = crate::prop::within(Duration::from_secs(10), what, move || {
                counted(|| Glob::name(&pat, Case::Sensitive).map(|g| g.matches("x")))
            });
            assert!(steps <= len * len, "{what}: {steps} steps, over the bound of {}", len * len);
        }
    }

    /// A reviewer's run of the suite overflowed the test thread's stack in
    /// `pathological_globs_are_bounded`: a path pattern's matcher recursed once per character a
    /// star took, so its depth grew with the path. `jump.exclude` is matched on the UI thread
    /// against every folder visited, and Windows' main thread has 1 MB of stack and long paths
    /// up to 32,767 characters: a crash. Matching (and brace expansion) now use no recursion
    /// that grows with the input: these run in a thread with a quarter of that stack.
    #[test]
    fn matching_needs_no_stack_that_grows_with_the_input() {
        let r = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let deep = format!("/{}", vec!["abcd"; 6_500].join("/"));
                assert!(deep.len() > 32_000);
                for pat in ["**/node_modules/**", "/**a**a**a**b", "/*/**/x*", "**/abcd"] {
                    let g = Glob::path(pat, Case::Sensitive).unwrap();
                    let want = pat == "**/abcd";
                    assert_eq!(g.matches(&deep), want, "{pat}");
                }
                // Braces as deep and as many as a pattern can hold.
                let nested = format!("{}x{}", "{".repeat(MAX_LEN / 2 - 1), "}".repeat(MAX_LEN / 2 - 1));
                assert!(Glob::name(&nested, Case::Sensitive).unwrap().matches("x"));
                let groups = "{a}".repeat(MAX_LEN / 3);
                assert!(Glob::name(&groups, Case::Sensitive).unwrap().matches(&"a".repeat(MAX_LEN / 3)));
            })
            .unwrap()
            .join();
        assert!(r.is_ok());
    }

    /// Found by `pathological_globs_are_bounded`: an unclosed `{` or `[` made each one look for
    /// its end to the end of the pattern, so 80,000 of them took 16 s, and `config.toml` (up to
    /// 1 MB) could hold such a `jump.exclude` for the startup read to compile. A pattern over
    /// `MAX_LEN` characters is refused now, at once: before any of the parsing work.
    #[test]
    fn an_absurdly_long_pattern_is_refused_at_once() {
        let pat = "{".repeat(1 << 20);
        let (r, steps) = crate::prop::within(std::time::Duration::from_secs(10), "a 1 MB pattern", move || {
            counted(|| Glob::path(&pat, Case::Sensitive).err())
        });
        assert!(r.is_some_and(|e| e.contains("longer than 4096")));
        assert_eq!(steps, 0, "refused before parsing");
        let c = crate::config::Config::parse(&format!("[jump]\nexclude = [\"{}\"]\n", "[".repeat(100_000)), rune::KeyPlatform::Linux);
        assert!(c.errors.iter().any(|e| e.contains("longer than 4096")), "{:?}", c.errors);
    }
}
