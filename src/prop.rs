//! Seeded randomized ("property") tests: a tiny PRNG and a runner, without a crate.
//!
//! Each property runs a fixed number of cases from a fixed seed, so CI is deterministic. A
//! failing case names its seed; `SATCHEL_PROP_SEED=<seed>` reruns from it, and
//! `SATCHEL_PROP_CASES=<n>` runs more cases (to hunt locally). A bug a property finds is kept as a
//! plain regression test with the input it found.

use std::time::Duration;

/// SplitMix64: tiny, fast, and good enough to make test inputs.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n` (`n` > 0).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// True one time in `n`.
    pub fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }

    pub fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.below(from.len())]
    }

    /// A length, mostly short: up to `max`, with small ones far likelier.
    pub fn len(&mut self, max: usize) -> usize {
        match self.below(4) {
            0 => self.below(4.min(max + 1)),
            1 | 2 => self.below(16.min(max + 1)),
            _ => self.below(max + 1),
        }
    }

    /// Random bytes, biased towards the ones parsers care about.
    pub fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = self.len(max);
        (0..n).map(|_| self.byte()).collect()
    }

    pub fn byte(&mut self) -> u8 {
        const SPECIAL: &[u8] = b"\0\t\n\r %\\/\"'=[]#;:.~-*?{},$\x7f\x80\xbf\xc3\xe2\xf0\xff";
        match self.below(3) {
            0 => *self.pick(SPECIAL),
            1 => b"abcxyz019AZ"[self.below(11)],
            _ => self.next() as u8,
        }
    }

    /// A character, biased towards what breaks text handling: ASCII punctuation, controls,
    /// combining marks, zero-width joiners, right-to-left text, emoji, and characters whose
    /// case changes their length.
    pub fn char(&mut self) -> char {
        const TRICKY: &[char] = &[
            '\0',
            '\t',
            '\n',
            '\r',
            ' ',
            '/',
            '\\',
            '"',
            '\'',
            '%',
            '~',
            '.',
            '-',
            '*',
            '?',
            '[',
            ']',
            '{',
            '}',
            ',',
            '$',
            '=',
            '#',
            ':',
            ';',
            '\u{7f}',
            '\u{85}',
            '\u{a0}',
            'é',
            'ß',
            'İ',
            'ı',
            'ﬀ',
            'Ω',
            '\u{301}',
            '\u{200d}',
            '\u{200b}',
            '\u{200e}',
            '\u{202e}',
            '\u{feff}',
            'ש',
            'ا',
            '中',
            '😀',
            '👩',
            '\u{1f3fd}',
            '\u{fe0f}',
            '\u{fffd}',
            '\u{10ffff}',
            'Σ',
            'ς',
            'ǅ',
            '…',
            '›',
            '\u{2028}',
        ];
        match self.below(4) {
            0 | 1 => *self.pick(TRICKY),
            2 => (b'a' + self.below(26) as u8) as char,
            _ => loop {
                let max = if self.one_in(4) { 0x11_0000 } else { 0x3000 };
                if let Some(c) = char::from_u32(self.below(max) as u32) {
                    break c;
                }
            },
        }
    }

    pub fn string(&mut self, max: usize) -> String {
        let n = self.len(max);
        (0..n).map(|_| self.char()).collect()
    }

    /// A string made of pieces from `alphabet` (tokens, keywords, separators), joined.
    pub fn from(&mut self, alphabet: &[&str], max: usize) -> String {
        let n = self.len(max);
        (0..n).map(|_| *self.pick(alphabet)).collect()
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

/// Runs `case` `n` times, each with an `Rng` seeded from `name` and the case number (or from
/// `SATCHEL_PROP_SEED`). A failure says which seed, so it can be rerun alone.
pub fn check(name: &str, n: usize, mut case: impl FnMut(&mut Rng)) {
    let base = name.bytes().fold(0xA5A5_5A5A_DEAD_BEEFu64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01B3));
    let (seeds, n): (Vec<u64>, usize) = match env_u64("SATCHEL_PROP_SEED") {
        Some(s) => (vec![s], 1),
        None => {
            let n = env_u64("SATCHEL_PROP_CASES").map_or(n, |m| m as usize);
            ((0..n as u64).map(|i| base.wrapping_add(i.wrapping_mul(0x9E37_79B9_7F4A_7C15))).collect(), n)
        }
    };
    for &seed in seeds.iter().take(n) {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| case(&mut Rng::new(seed))));
        if let Err(e) = r {
            eprintln!("property {name} failed with SATCHEL_PROP_SEED={seed}");
            std::panic::resume_unwind(e);
        }
    }
}

/// Runs `f` on its own thread and fails if it takes longer than `limit` (a hang, or a blowup
/// on a pathological input). Its answer comes back.
pub fn within<T: Send + 'static>(limit: Duration, what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::time::Instant::now();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(limit) {
        Ok(v) => v,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("{what}: still running after {limit:?}"),
        Err(_) => panic!("{what}: panicked after {:?}", t.elapsed()),
    }
}

#[test]
fn the_generator_is_deterministic_and_spreads() {
    let (mut a, mut b) = (Rng::new(7), Rng::new(7));
    let xs: Vec<u64> = (0..100).map(|_| a.next()).collect();
    assert!(xs.iter().all(|&x| x == b.next()));
    let mut r = Rng::new(1);
    let mut seen = [0usize; 10];
    for _ in 0..10_000 {
        seen[r.below(10)] += 1;
    }
    assert!(seen.iter().all(|&n| (800..1200).contains(&n)), "{seen:?}");
}
