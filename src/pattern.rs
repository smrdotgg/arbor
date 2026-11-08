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

/// `{a,b}` expanded into each alternative (recursively), at most [`MAX_ALTS`].
fn expand_braces(p: &str) -> Result<Vec<String>, String> {
    let chars: Vec<char> = p.chars().collect();
    let mut out = Vec::new();
    expand_into(&chars, &mut out)?;
    Ok(out)
}

fn expand_into(p: &[char], out: &mut Vec<String>) -> Result<(), String> {
    // The first top-level `{` with its matching `}` (skipping `[...]` classes).
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            '[' => i = class_end(p, i).map_or(i + 1, |e| e + 1),
            '{' => {
                if let Some((end, commas)) = brace_end(p, i) {
                    let mut parts = Vec::new();
                    let mut from = i + 1;
                    for c in commas.into_iter().chain([end]) {
                        parts.push(&p[from..c]);
                        from = c + 1;
                    }
                    for part in parts {
                        let mut next: Vec<char> = p[..i].to_vec();
                        next.extend_from_slice(part);
                        next.extend_from_slice(&p[end + 1..]);
                        expand_into(&next, out)?;
                        if out.len() > MAX_ALTS {
                            return Err(format!("More than {MAX_ALTS} alternatives in {{…}}"));
                        }
                    }
                    return Ok(());
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out.push(p.iter().collect());
    Ok(())
}

/// The `}` matching the `{` at `open`, and the top-level commas between them.
fn brace_end(p: &[char], open: usize) -> Option<(usize, Vec<usize>)> {
    let (mut depth, mut commas, mut i) = (0, Vec::new(), open + 1);
    while i < p.len() {
        match p[i] {
            '[' => {
                i = class_end(p, i).map_or(i + 1, |e| e + 1);
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

/// The `]` closing the class at `open`. A `]` right after `[` or `[!` is a member.
fn class_end(p: &[char], open: usize) -> Option<usize> {
    let mut i = open + 1;
    if matches!(p.get(i), Some('!' | '^')) {
        i += 1;
    }
    if p.get(i) == Some(&']') {
        i += 1;
    }
    (i..p.len()).find(|&j| p[j] == ']')
}

fn is_sep(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

fn tokens(p: &str, path: bool) -> Vec<Tok> {
    let chars: Vec<char> = p.chars().collect();
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
                if let Some(end) = class_end(&chars, i) {
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
    loop {
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
    loop {
        match (toks.get(ti), s.get(si)) {
            (Some(Tok::Star | Tok::Deep), _) => {
                ti += 1;
                // A literal next: skip straight to its next occurrence.
                if let Some(Tok::Lit(c)) = toks.get(ti) {
                    match s[si..].iter().position(|&b| b == *c as u8) {
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
                    match s[next..].iter().position(|&b| b == *c as u8) {
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

/// A path against tokens, where `*` can't cross a separator and `**` can: memoized, since the
/// greedy match can't go back past a star that stopped at a separator. Paths are short and
/// patterns few (checked once per folder visited).
fn match_path(toks: &[Tok], s: &[char], case: Case) -> bool {
    let w = s.len() + 1;
    let mut memo = vec![None; (toks.len() + 1) * w];
    fn go(toks: &[Tok], s: &[char], case: Case, t: usize, i: usize, w: usize, memo: &mut [Option<bool>]) -> bool {
        if let Some(v) = memo[t * w + i] {
            return v;
        }
        let v = match toks.get(t) {
            None => i == s.len(),
            Some(Tok::Star) => {
                go(toks, s, case, t + 1, i, w, memo) || (i < s.len() && !is_sep(s[i]) && go(toks, s, case, t, i + 1, w, memo))
            }
            Some(Tok::Deep) => go(toks, s, case, t + 1, i, w, memo) || (i < s.len() && go(toks, s, case, t, i + 1, w, memo)),
            Some(tok) => i < s.len() && one(tok, s[i], case, true) && go(toks, s, case, t + 1, i + 1, w, memo),
        };
        memo[t * w + i] = Some(v);
        v
    }
    go(toks, s, case, 0, 0, w, &mut memo)
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
}
