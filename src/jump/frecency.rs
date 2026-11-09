//! The aggregate: each folder's frecency (visits weighted by how recent they are), and ranking
//! folders against what's typed (every word in order, the last one in the folder's own name).

use super::*;

/// One folder in the history.
#[derive(Clone, Debug)]
pub struct Dir {
    /// Shared with the rows showing it, so ranking copies no strings.
    pub path: Arc<Path>,
    pub rank: f64,
    /// When it was last visited (Unix seconds).
    pub last: u64,
    /// As shown: the home folder as `~`.
    pub shown: Arc<str>,
    /// `shown` folded for matching: each character lowercased to one character, and `\` as
    /// `/` on Windows, so an index here is a character index in `shown`.
    pub(super) low: Arc<[char]>,
    /// Where the last folder name starts in `low`.
    pub(super) tail: usize,
    /// The whole path folded the same way, for go-to's "folders under what's typed".
    pub(super) full: Box<[char]>,
}

impl Dir {
    pub(super) fn new(path: Arc<Path>, rank: f64, last: u64) -> Dir {
        let shown = HOME.with(|h| pretty_under(h, &path));
        let low = fold(&shown);
        let tail = low.iter().rposition(|&c| c == '/').map_or(0, |i| i + 1).min(low.len());
        // A root ("/", "C:\") is all tail.
        let tail = if tail == low.len() { 0 } else { tail };
        let full = fold(&path.to_string_lossy()).into_boxed_slice();
        Dir { path, rank, last, shown: shown.into(), low: low.into(), tail, full }
    }

    /// Rank weighted by how recently it was visited (zoxide's buckets).
    pub fn frecency(&self, now: u64) -> f64 {
        let age = now.saturating_sub(self.last);
        self.rank
            * if age < HOUR {
                4.0
            } else if age < DAY {
                2.0
            } else if age < WEEK {
                0.5
            } else {
                0.25
            }
    }
}

pub(super) fn fold(s: &str) -> Vec<char> {
    let sep = |c: char| if cfg!(windows) && c == '\\' { '/' } else { c };
    if s.is_ascii() {
        return s.bytes().map(|b| sep(b.to_ascii_lowercase() as char)).collect();
    }
    s.chars().map(|c| sep(c.to_lowercase().next().unwrap_or(c))).collect()
}

thread_local! {
    /// The home folder, read once per thread rather than once per folder of a 5k history.
    pub(super) static HOME: PathBuf = fs::home();
}

/// `fs::pretty` with the home folder given.
pub(super) fn pretty_under(home: &Path, p: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// Every folder visited, with its rank and last visit.
#[derive(Default, Debug)]
pub struct Frecency {
    pub(super) dirs: Vec<Dir>,
    pub(super) index: HashMap<Arc<Path>, usize>,
    /// The ranks added up, for aging.
    pub(super) total: f64,
    /// Changes whenever `dirs` does, so a box narrowing its last matches knows they still hold.
    pub(super) generation: u64,
}

/// One thing the log records.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Visit { path: PathBuf, at: u64, rank: f64 },
    Drop { path: PathBuf },
}

impl Event {
    /// The line the log stores it as; `None` for a path that can't be one line of UTF-8.
    pub(super) fn line(&self, at: u64) -> Option<String> {
        let (path, mid) = match self {
            Event::Visit { path, rank, .. } => (path, if *rank == 1.0 { "1".to_string() } else { format!("{rank:.3}") }),
            Event::Drop { path } => (path, "-".to_string()),
        };
        // No NUL either: `parse` refuses a line with one (it marks a torn line).
        let p = path.to_str().filter(|p| !p.is_empty() && !p.contains(['\n', '\r', '\0']))?;
        Some(format!("{at}\t{mid}\t{p}\n"))
    }

    pub(super) fn parse(line: &str) -> Option<Event> {
        // A NUL marks a line cut short by a crash (see `append`): never a record.
        if line.contains('\0') {
            return None;
        }
        let mut it = line.splitn(3, '\t');
        let at: u64 = it.next()?.trim().parse().ok()?;
        let mid = it.next()?;
        let path = it.next().filter(|p| !p.is_empty())?;
        let path = PathBuf::from(path);
        if !path.is_absolute() && !path.has_root() {
            return None;
        }
        if mid == "-" {
            return Some(Event::Drop { path });
        }
        let rank: f64 = mid.trim().parse().ok().filter(|r: &f64| r.is_finite() && *r > 0.0)?;
        Some(Event::Visit { path, at, rank })
    }
}

impl Frecency {
    pub fn len(&self) -> usize {
        self.dirs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }

    #[cfg(test)]
    pub fn get(&self, path: &Path) -> Option<&Dir> {
        self.index.get(path).map(|&i| &self.dirs[i])
    }

    pub fn apply(&mut self, e: &Event) {
        self.generation += 1;
        match e {
            Event::Visit { path, at, rank } => self.visit(path, *at, *rank),
            Event::Drop { path } => {
                self.remove(path);
            }
        }
    }

    pub(super) fn visit(&mut self, path: &Path, at: u64, rank: f64) {
        match self.index.get(path) {
            Some(&i) => {
                let d = &mut self.dirs[i];
                d.rank += rank;
                d.last = d.last.max(at);
            }
            None => {
                let path: Arc<Path> = path.into();
                self.index.insert(path.clone(), self.dirs.len());
                self.dirs.push(Dir::new(path, rank, at));
            }
        }
        self.total += rank;
        if self.total > MAX_AGE {
            let k = 0.9 * MAX_AGE / self.total;
            for d in &mut self.dirs {
                d.rank *= k;
            }
            self.dirs.retain(|d| d.rank >= 1.0);
            self.total = self.dirs.iter().map(|d| d.rank).sum();
            self.reindex();
        }
    }

    /// Takes `path` out; true when it was there.
    pub fn remove(&mut self, path: &Path) -> bool {
        self.generation += 1;
        let Some(i) = self.index.remove(path) else { return false };
        self.total -= self.dirs.swap_remove(i).rank;
        if let Some(moved) = self.dirs.get(i) {
            self.index.insert(moved.path.clone(), i);
        }
        true
    }

    pub(super) fn reindex(&mut self) {
        self.index = self.dirs.iter().enumerate().map(|(i, d)| (d.path.clone(), i)).collect();
    }

    /// The aggregate of a log's text, and how many lines it had. A last line without its
    /// newline was cut short (a crash mid-write), so it's left out; so is any line that
    /// doesn't parse, or isn't UTF-8 (Arbor writes only UTF-8: anything else is damage, and
    /// reading it as U+FFFD would make up a folder that was never visited).
    pub fn from_log(text: &str) -> (Frecency, usize) {
        Frecency::from_reader(text.as_bytes()).unwrap_or_default()
    }

    /// [`from_log`](Self::from_log), reading a line at a time, so a log of any size is read
    /// whole without holding it in memory.
    pub fn from_reader(mut r: impl std::io::BufRead) -> std::io::Result<(Frecency, usize)> {
        let mut f = Frecency::default();
        let (mut lines, mut buf) = (0, Vec::new());
        loop {
            buf.clear();
            if r.read_until(b'\n', &mut buf)? == 0 || buf.last() != Some(&b'\n') {
                return Ok((f, lines));
            }
            lines += 1;
            let Ok(line) = std::str::from_utf8(&buf) else { continue };
            if let Some(e) = Event::parse(line.trim_end_matches(['\n', '\r'])) {
                f.apply(&e);
            }
        }
    }

    /// The log rewritten: one line per folder for the top [`KEEP`] by frecency.
    pub fn compacted(&self, now: u64) -> String {
        let mut v: Vec<&Dir> = self.dirs.iter().collect();
        v.sort_by(|a, b| b.frecency(now).total_cmp(&a.frecency(now)));
        v.truncate(KEEP);
        v.iter()
            .filter_map(|d| Event::Visit { path: d.path.to_path_buf(), at: d.last, rank: d.rank }.line(d.last))
            .collect::<Vec<_>>()
            .concat()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The folders matching `query`, best first, at most `limit`. Every word of the query must
    /// appear in the path in order (case doesn't matter); the last word should be in the last
    /// folder name, as zoxide has it ("proj arb" finds `~/projects/arbor`). A path whose last
    /// word only matches a parent still shows, far down. Score: frecency × how well it matched
    /// (the last word starting or being the folder's name counts most).
    #[cfg(test)]
    pub fn rank(&self, query: &str, now: u64, limit: usize) -> Vec<Hit> {
        self.rank_in(query, now, limit, None).0
    }

    /// [`rank`](Self::rank) among `among` (indices of folders that matched a query this one
    /// narrows, from this generation), or all. Also every folder that matched, for the next
    /// key to narrow. Only the top `limit` are sorted, and a hit shares its folder's strings.
    pub fn rank_in(&self, query: &str, now: u64, limit: usize, among: Option<&[u32]>) -> (Vec<Hit>, Vec<u32>) {
        let words: Vec<Vec<char>> = query.split_whitespace().map(fold).collect();
        let score = |i: u32| {
            let d = &self.dirs[i as usize];
            Some((d.frecency(now) * quality(&words, &d.low, d.tail, None)?, i))
        };
        let mut v: Vec<(f64, u32)> = match among {
            Some(ix) => ix.iter().filter_map(|&i| score(i)).collect(),
            None => (0..self.dirs.len() as u32).filter_map(score).collect(),
        };
        let matched: Vec<u32> = v.iter().map(|&(_, i)| i).collect();
        let order = |a: &(f64, u32), b: &(f64, u32)| {
            let (da, db) = (&self.dirs[a.1 as usize], &self.dirs[b.1 as usize]);
            b.0.total_cmp(&a.0).then(da.low.len().cmp(&db.low.len())).then_with(|| da.shown.cmp(&db.shown))
        };
        if v.len() > limit {
            v.select_nth_unstable_by(limit, order);
            v.truncate(limit);
        }
        v.sort_by(order);
        let hits = v
            .into_iter()
            .map(|(score, i)| {
                let d = &self.dirs[i as usize];
                Hit { path: d.path.clone(), shown: d.shown.clone(), low: Some((d.low.clone(), d.tail)), score }
            })
            .collect();
        (hits, matched)
    }

    /// The folders under (or at) the path being typed, by frecency: what go-to in Jump offers.
    pub fn under(&self, typed: &Path, now: u64, limit: usize) -> Vec<Hit> {
        let want = fold(&typed.to_string_lossy());
        let mut v: Vec<&Dir> = self.dirs.iter().filter(|d| d.full.starts_with(&want)).collect();
        v.sort_by(|a, b| b.frecency(now).total_cmp(&a.frecency(now)));
        v.truncate(limit);
        v.into_iter().map(|d| Hit { path: d.path.clone(), shown: d.shown.clone(), low: None, score: d.frecency(now) }).collect()
    }
}

/// Whether every folder matching `new` also matches `old`, so `new` can rank among `old`'s
/// matches: the same words, the last one extended, or more words after. (Matching is words
/// in order as substrings, so a longer word or one more word only ever matches fewer.)
pub fn narrows(old: &str, new: &str) -> bool {
    let (old, new): (Vec<&str>, Vec<&str>) = (old.split_whitespace().collect(), new.split_whitespace().collect());
    let Some((last, firsts)) = old.split_last() else { return true };
    new.len() >= old.len() && new[..firsts.len()] == *firsts && new[firsts.len()].starts_with(last)
}

/// `query`'s first match in `low` from `from` on.
pub(super) fn find(low: &[char], word: &[char], from: usize) -> Option<usize> {
    if word.is_empty() {
        return Some(from.min(low.len()));
    }
    if word.len() > low.len() {
        return None;
    }
    (from..=low.len() - word.len()).find(|&i| low[i..i + word.len()] == *word)
}

/// How well `words` match a folder's folded path (see [`Frecency::rank`]), `None` when they
/// don't. Writes the matched character ranges to `marks` when given.
pub(super) fn quality(words: &[Vec<char>], low: &[char], tail: usize, mut marks: Option<&mut Vec<(usize, usize)>>) -> Option<f64> {
    let mut q = 1.0;
    let mut pos = 0;
    for (k, w) in words.iter().enumerate() {
        let last = k + 1 == words.len();
        let (at, in_tail) = if last {
            match find(low, w, pos.max(tail)) {
                Some(a) => (a, true),
                None => (find(low, w, pos)?, false),
            }
        } else {
            (find(low, w, pos)?, false)
        };
        let starts_word = at == 0 || !low[at - 1].is_alphanumeric();
        if !last {
            if starts_word {
                q *= 1.1;
            }
        } else if !in_tail {
            q *= 0.2;
        } else if at == tail && at + w.len() == low.len() {
            q *= 2.0;
        } else if at == tail {
            q *= 1.5;
        } else if starts_word {
            q *= 1.2;
        }
        if let Some(m) = marks.as_deref_mut() {
            m.push((at, at + w.len()));
        }
        pos = at + w.len();
    }
    Some(q)
}

/// Character ranges in `s` as byte ranges.
pub(super) fn byte_ranges(s: &str, chars: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let offs: Vec<usize> = s.char_indices().map(|(b, _)| b).chain([s.len()]).collect();
    chars.iter().filter(|(a, b)| a < b && *b < offs.len()).map(|&(a, b)| (offs[a], offs[b])).collect()
}

/// A row in the box. Its strings are its folder's own, shared.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: Arc<Path>,
    pub shown: Arc<str>,
    /// `shown` folded and where its last name starts, to find what matched when the row is
    /// built (`marks`); `None` for the typed path in go-to mode.
    pub(super) low: Option<(Arc<[char]>, usize)>,
    pub score: f64,
}

impl Hit {
    /// Byte ranges of `shown` that `words` (folded) matched, to emphasise. Worked out only for
    /// the rows being built, not for every row ranked.
    pub fn marks(&self, words: &[Vec<char>]) -> Vec<(usize, usize)> {
        let Some((low, tail)) = &self.low else { return Vec::new() };
        let mut marks = Vec::new();
        quality(words, low, *tail, Some(&mut marks));
        byte_ranges(&self.shown, &marks)
    }
}

/// A query's words folded for matching (and for [`Hit::marks`]).
pub fn words(query: &str) -> Vec<Vec<char>> {
    query.split_whitespace().map(fold).collect()
}
