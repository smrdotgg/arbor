//! Reading tar archives (`.tar`, and gzipped: `.tar.gz`, `.tgz`): POSIX ustar, pax extended
//! headers (path, link path, size, modification time) and GNU long names. Files, folders and
//! symbolic links are extracted; hard links, devices, FIFOs and sparse files are left out with a
//! note. Every header's checksum is checked.
//!
//! A tar is a stream: Extract reads it twice, once for the list (headers only, the data skipped
//! or, gzipped, decompressed and dropped) so the whole archive is checked before anything is
//! written, then again to write it, each entry matched against the list.

use std::io::{self, Read};

use super::{Entry, Error, Kind, limits};

const BLOCK: usize = 512;

/// An archive being read: the next header comes after the data of the one before.
pub struct Reader<R: Read> {
    r: R,
    /// Data of the current entry not read yet, and the padding after it.
    left: u64,
    pad: u64,
    done: bool,
}

/// The pax and GNU headers that apply to the next entry.
#[derive(Default)]
struct Pending {
    path: Option<Vec<u8>>,
    link: Option<Vec<u8>>,
    size: Option<u64>,
    mtime: Option<i64>,
}

fn corrupt(s: &str) -> Error {
    Error::Corrupt(s.into())
}

/// A number field: octal digits (space or NUL ends it), or base-256 when the top bit is set.
fn number(f: &[u8]) -> Result<u64, Error> {
    if f.first().is_some_and(|b| b & 0x80 != 0) {
        let mut v: u64 = u64::from(f[0] & 0x7F);
        for &b in &f[1..] {
            v = v.checked_mul(256).and_then(|v| v.checked_add(u64::from(b))).ok_or_else(|| corrupt("a size is too large"))?;
        }
        return Ok(v);
    }
    let s = f.iter().skip_while(|b| **b == b' ').take_while(|b| **b != 0 && **b != b' ');
    let mut v: u64 = 0;
    for &b in s {
        if !(b'0'..=b'7').contains(&b) {
            return Err(corrupt("a header has a bad number"));
        }
        v = v.checked_mul(8).and_then(|v| v.checked_add(u64::from(b - b'0'))).ok_or_else(|| corrupt("a number is too large"))?;
    }
    Ok(v)
}

/// A NUL-terminated text field.
fn text(f: &[u8]) -> &[u8] {
    &f[..f.iter().position(|&b| b == 0).unwrap_or(f.len())]
}

impl<R: Read> Reader<R> {
    pub fn new(r: R) -> Reader<R> {
        Reader { r, left: 0, pad: 0, done: false }
    }

    /// Skips what's left of the current entry (its data and padding).
    fn skip(&mut self) -> Result<(), Error> {
        let n = self.left + self.pad;
        if n > 0 {
            let got = io::copy(&mut (&mut self.r).take(n), &mut io::sink())?;
            if got < n {
                return Err(corrupt("it ends in the middle of an entry"));
            }
        }
        self.left = 0;
        self.pad = 0;
        Ok(())
    }

    /// Reads a whole small entry (a pax header, a GNU long name).
    fn small(&mut self, size: u64) -> Result<Vec<u8>, Error> {
        if size > limits::META as u64 {
            return Err(Error::Refused("an extended header is too large to be real".into()));
        }
        let mut v = vec![0; size as usize];
        self.r.read_exact(&mut v)?;
        self.left = 0;
        Ok(v)
    }

    /// The next entry; `None` at the end. Skips the rest of the one before.
    pub fn next(&mut self) -> Result<Option<Entry>, Error> {
        if self.done {
            return Ok(None);
        }
        self.skip()?;
        let mut pending = Pending::default();
        for _ in 0..16 {
            let mut h = [0u8; BLOCK];
            let got = read_full(&mut self.r, &mut h)?;
            if got == 0 {
                // No end blocks: tolerated, as GNU tar does.
                self.done = true;
                return Ok(None);
            }
            if got < BLOCK {
                return Err(corrupt("it ends in the middle of a header"));
            }
            if h.iter().all(|&b| b == 0) {
                self.done = true;
                return Ok(None);
            }
            let want = number(&h[148..156])?;
            let sum: u64 = h.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { u64::from(b) }).sum();
            let signed: i64 = h.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { i64::from(b as i8) }).sum();
            if want != sum && want as i64 != signed {
                return Err(corrupt("a header's checksum doesn't match"));
            }
            let flag = h[156];
            let size = pending.size.take().map_or_else(|| number(&h[124..136]), Ok)?;
            self.left = size;
            self.pad = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
            match flag {
                b'x' => {
                    let d = self.small(size)?;
                    pax(&d, &mut pending)?;
                    self.skip()?;
                    continue;
                }
                // Global pax headers set defaults for every entry; nothing Arbor uses.
                b'g' => {
                    self.skip()?;
                    continue;
                }
                b'L' => {
                    let d = self.small(size)?;
                    pending.path = Some(text(&d).to_vec());
                    self.skip()?;
                    continue;
                }
                b'K' => {
                    let d = self.small(size)?;
                    pending.link = Some(text(&d).to_vec());
                    self.skip()?;
                    continue;
                }
                _ => {}
            }
            let ustar = &h[257..262] == b"ustar";
            let name = match pending.path.take() {
                Some(p) => p,
                None => {
                    let n = text(&h[0..100]);
                    let prefix = if ustar { text(&h[345..500]) } else { &[] };
                    if prefix.is_empty() { n.to_vec() } else { [prefix, b"/", n].concat() }
                }
            };
            let link = pending.link.take().unwrap_or_else(|| text(&h[157..257]).to_vec());
            let mode = number(&h[100..108]).ok().map(|m| (m & 0o7777) as u32);
            let mtime = pending.mtime.take().or_else(|| number(&h[136..148]).ok().map(|t| t as i64));
            let (name, lossy) = match String::from_utf8(name) {
                Ok(s) => (s, false),
                Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
            };
            let kind = match flag {
                b'0' | 0 | b'7' if name.ends_with('/') => Kind::Dir,
                b'0' | 0 | b'7' => Kind::File { size },
                b'5' => Kind::Dir,
                b'2' => Kind::Link(String::from_utf8_lossy(&link).into_owned()),
                b'1' => Kind::Skip("hard links aren't extracted"),
                b'S' => Kind::Skip("sparse files aren't extracted"),
                b'3' | b'4' | b'6' => Kind::Skip("devices and FIFOs aren't extracted"),
                _ => Kind::Skip("it's a kind of entry Arbor doesn't know"),
            };
            return Ok(Some(Entry { name, kind, mode, mtime: mtime.and_then(super::from_unix), lossy }));
        }
        Err(corrupt("too many extended headers in a row"))
    }

    /// Reads the current entry's data.
    pub fn read_data(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        if self.left == 0 {
            return Ok(0);
        }
        let cap = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.r.read(&mut buf[..cap])?;
        if n == 0 {
            return Err(corrupt("it ends in the middle of an entry"));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// Fills `buf` as far as the stream goes; how much it read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// The records of a pax extended header ("<length> <key>=<value>\n") Arbor uses.
fn pax(d: &[u8], p: &mut Pending) -> Result<(), Error> {
    let mut rest = d;
    while !rest.is_empty() {
        let sp = rest.iter().position(|&b| b == b' ').ok_or_else(|| corrupt("an extended header is damaged"))?;
        let len: usize =
            std::str::from_utf8(&rest[..sp]).ok().and_then(|s| s.parse().ok()).ok_or_else(|| corrupt("an extended header is damaged"))?;
        if len <= sp + 1 || len > rest.len() {
            return Err(corrupt("an extended header is damaged"));
        }
        let rec = &rest[sp + 1..len];
        let rec = rec.strip_suffix(b"\n").unwrap_or(rec);
        if let Some(eq) = rec.iter().position(|&b| b == b'=') {
            let (k, v) = (&rec[..eq], &rec[eq + 1..]);
            match k {
                b"path" => p.path = Some(v.to_vec()),
                b"linkpath" => p.link = Some(v.to_vec()),
                b"size" => p.size = std::str::from_utf8(v).ok().and_then(|s| s.parse().ok()),
                b"mtime" => {
                    p.mtime = std::str::from_utf8(v).ok().and_then(|s| s.split('.').next()).and_then(|s| s.parse().ok());
                }
                _ => {}
            }
        }
        rest = &rest[len..];
    }
    Ok(())
}

/// Counts what passes through: the compressed bytes read, for progress.
pub struct Counted<R> {
    pub r: R,
    pub n: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let k = self.r.read(buf)?;
        self.n.fetch_add(k as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(k)
    }
}

/// Reads every entry of the tar in `r`, checking the running total against the limits as it
/// goes; `stop` is asked between entries.
pub fn list(r: impl Read, stop: &dyn Fn() -> bool) -> Result<Vec<Entry>, Error> {
    let mut rd = Reader::new(r);
    let mut v = Vec::new();
    let mut total = 0u64;
    while let Some(e) = rd.next()? {
        if stop() {
            return Err(Error::Cancelled);
        }
        if let Kind::File { size } = e.kind {
            total = total.saturating_add(size);
            super::check_size(total)?;
        }
        v.push(e);
        if v.len() > limits::ENTRIES {
            return Err(Error::Refused(format!("it has more than {} entries", super::group(limits::ENTRIES as u64))));
        }
    }
    Ok(v)
}
