//! ZIP, the subset Satchel reads and writes: stored and deflate entries, ZIP64 (over 4 GB or
//! 65,535 entries), UTF-8 names, Unix modes and links, and the extended timestamp. Reading
//! goes by the central directory, so entries with data descriptors (sizes after the data) read
//! too; encrypted entries and other compression methods are refused by name. Writing streams
//! each file and then writes its sizes into the header it left room for (the archive is a
//! temporary file of Satchel's own), so no data descriptors are written.
//!
//! Against bombs: entries may not overlap (one compressed stream counted many times is how the
//! big non-recursive bombs work), each entry's output is cut off at the size the directory
//! declares (and must reach it, with a matching CRC), and [`super::plan`] bounds the total.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::time::SystemTime;

use super::{Entry, Error, Kind, limits};

const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const END: u32 = 0x0605_4b50;
const END64: u32 = 0x0606_4b50;
const LOCATOR64: u32 = 0x0706_4b50;
/// A 16- or 32-bit field that says "see the ZIP64 extra field".
const FF16: u16 = 0xFFFF;
const FF32: u32 = 0xFFFF_FFFF;

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;

/// Where one entry's data is, and how to check it.
#[derive(Debug, Clone)]
pub struct Item {
    /// Offset of its local header.
    offset: u64,
    method: u16,
    crc: u32,
    csize: u64,
    pub size: u64,
    /// The least its local header and data take: the fixed part, its name (the local header
    /// repeats the directory's) and its compressed size.
    span: u64,
}

/// An archive's list: the entries, and where each one's data is (same order).
pub struct Listing {
    pub entries: Vec<Entry>,
    pub items: Vec<Item>,
    /// Where the central directory starts: no entry's data goes past it.
    end: u64,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().expect("4 bytes"))
}
fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().expect("8 bytes"))
}

fn corrupt(s: &str) -> Error {
    Error::Corrupt(s.into())
}

fn method_name(m: u16) -> String {
    match m {
        1..=6 => "an old compression method (shrink, reduce or implode)".into(),
        9 => "Deflate64 compression".into(),
        12 => "bzip2 compression".into(),
        14 => "LZMA compression".into(),
        93 => "Zstandard compression".into(),
        95 => "XZ compression".into(),
        98 => "PPMd compression".into(),
        99 => "AES encryption".into(),
        m => format!("compression method {m}"),
    }
}

/// Reads the list of the ZIP archive in `f` (`len` bytes long), checking it as it goes. The
/// targets of links are read too (they're small, and [`super::plan`] needs them).
pub fn list(f: &File, len: u64, stop: &dyn Fn() -> bool) -> Result<Listing, Error> {
    let mut r = BufReader::with_capacity(64 * 1024, f);
    // The end record: in the last 22 bytes, or before a comment of up to 64 KB.
    let tail = len.min(22 + 65_535);
    if tail < 22 {
        return Err(corrupt("it's too short to be a ZIP archive"));
    }
    r.seek(SeekFrom::Start(len - tail))?;
    let mut buf = vec![0; tail as usize];
    r.read_exact(&mut buf)?;
    let at = (0..=buf.len() - 22)
        .rev()
        .filter(|&i| u32_at(&buf, i) == END)
        .find(|&i| i + 22 + usize::from(u16_at(&buf, i + 20)) == buf.len())
        .or_else(|| (0..=buf.len() - 22).rev().find(|&i| u32_at(&buf, i) == END))
        .ok_or_else(|| corrupt("it isn't a ZIP archive, or it was cut short"))?;
    let end_pos = len - tail + at as u64;
    let e = &buf[at..];
    if u16_at(e, 4) != 0 || u16_at(e, 6) != 0 {
        return Err(Error::Unsupported("it's split across several files, which Satchel can't extract".into()));
    }
    let (mut count, mut cd_size, mut cd_start) = (u64::from(u16_at(e, 10)), u64::from(u32_at(e, 12)), u64::from(u32_at(e, 16)));
    let mut shift = 0u64;
    if count == u64::from(FF16) || cd_size == u64::from(FF32) || cd_start == u64::from(FF32) {
        // ZIP64: the locator just before the end record says where its own end record is.
        if end_pos < 20 {
            return Err(corrupt("its ZIP64 end record is missing"));
        }
        let mut loc = [0; 20];
        r.seek(SeekFrom::Start(end_pos - 20))?;
        r.read_exact(&mut loc)?;
        if u32_at(&loc, 0) != LOCATOR64 {
            return Err(corrupt("its ZIP64 end record is missing"));
        }
        let at64 = u64_at(&loc, 8);
        if at64.checked_add(56).is_none_or(|e| e > end_pos) {
            return Err(corrupt("its ZIP64 end record is out of place"));
        }
        let mut e64 = [0; 56];
        r.seek(SeekFrom::Start(at64))?;
        r.read_exact(&mut e64)?;
        if u32_at(&e64, 0) != END64 {
            return Err(corrupt("its ZIP64 end record is missing"));
        }
        count = u64_at(&e64, 32);
        cd_size = u64_at(&e64, 40);
        cd_start = u64_at(&e64, 48);
        if cd_start.checked_add(cd_size).is_none_or(|e| e > at64) {
            return Err(corrupt("its directory is out of place"));
        }
    } else {
        // Data in front of the archive (a self-extracting one): everything is that much later.
        if cd_start.checked_add(cd_size).is_none_or(|e| e > end_pos) {
            return Err(corrupt("its directory is out of place"));
        }
        shift = end_pos - (cd_start + cd_size);
        cd_start += shift;
    }
    if count > limits::ENTRIES as u64 {
        return Err(Error::Refused(format!("it has more than {} entries", super::group(limits::ENTRIES as u64))));
    }
    if count.saturating_mul(46) > cd_size {
        return Err(corrupt("its directory is shorter than its entries"));
    }
    r.seek(SeekFrom::Start(cd_start))?;
    let mut entries = Vec::with_capacity(count as usize);
    let mut items = Vec::with_capacity(count as usize);
    let mut links = Vec::new();
    let mut head = [0u8; 46];
    for n in 0..count {
        if n % 4096 == 0 && stop() {
            return Err(Error::Cancelled);
        }
        r.read_exact(&mut head)?;
        if u32_at(&head, 0) != CENTRAL {
            return Err(corrupt("its directory is damaged"));
        }
        let made_by = u16_at(&head, 4);
        let flags = u16_at(&head, 8);
        let method = u16_at(&head, 10);
        let (time, date) = (u16_at(&head, 12), u16_at(&head, 14));
        let crc = u32_at(&head, 16);
        let (mut csize, mut size) = (u64::from(u32_at(&head, 20)), u64::from(u32_at(&head, 24)));
        let (nlen, xlen, clen) = (usize::from(u16_at(&head, 28)), usize::from(u16_at(&head, 30)), usize::from(u16_at(&head, 32)));
        let ext = u32_at(&head, 38);
        let mut offset = u64::from(u32_at(&head, 42));
        let mut name = vec![0; nlen];
        r.read_exact(&mut name)?;
        let mut extra = vec![0; xlen];
        r.read_exact(&mut extra)?;
        r.seek_relative(clen as i64)?;
        let mut mtime = None;
        let mut unicode = None;
        // Extra fields: ZIP64 sizes and offset, times, a UTF-8 name.
        let mut i = 0;
        while i + 4 <= extra.len() {
            let (id, sz) = (u16_at(&extra, i), usize::from(u16_at(&extra, i + 2)));
            let Some(d) = extra.get(i + 4..i + 4 + sz) else { break };
            match id {
                0x0001 => {
                    let mut j = 0;
                    let wide = [size, csize, offset].map(|v| v == u64::from(FF32));
                    for (field, wide) in [&mut size, &mut csize, &mut offset].into_iter().zip(wide) {
                        if wide && j + 8 <= d.len() {
                            *field = u64_at(d, j);
                            j += 8;
                        }
                    }
                }
                0x5455 if !d.is_empty() && d[0] & 1 != 0 && d.len() >= 5 => {
                    mtime = super::from_unix(i64::from(u32_at(d, 1) as i32));
                }
                0x000a if mtime.is_none() && d.len() >= 32 && u16_at(d, 4) == 1 && u16_at(d, 6) >= 24 => {
                    let ft = u64_at(d, 8);
                    mtime = super::from_unix((ft / 10_000_000) as i64 - 11_644_473_600);
                }
                0x7075 if d.len() > 5 && d[0] == 1 && u32_at(d, 1) == crc32fast::hash(&name) => {
                    unicode = std::str::from_utf8(&d[5..]).ok().map(str::to_string);
                }
                _ => {}
            }
            i += 4 + sz;
        }
        let name = match unicode {
            Some(u) => u,
            None if flags & 0x800 != 0 => String::from_utf8(name).map_err(|_| corrupt("an entry's UTF-8 name isn't UTF-8"))?,
            None => super::decode_cp437(&name),
        };
        let shown = || name.chars().take(80).collect::<String>();
        if flags & 1 != 0 {
            return Err(Error::Unsupported(format!("“{}” is encrypted, and Satchel can't extract encrypted archives", shown())));
        }
        if method != 0 && method != 8 {
            return Err(Error::Unsupported(format!("“{}” uses {}, which Satchel can't extract", shown(), method_name(method))));
        }
        if method == 0 && csize != size {
            return Err(corrupt("a stored entry's sizes don't match"));
        }
        offset = offset.checked_add(shift).ok_or_else(|| corrupt("an entry is out of place"))?;
        // Unix hosts (3, and 19 for old macOS) keep the mode in the high half.
        let host = made_by >> 8;
        let mode = (matches!(host, 3 | 19) && ext >> 16 != 0).then_some(ext >> 16);
        let kind = match mode.map(|m| m & S_IFMT) {
            Some(S_IFLNK) => {
                links.push(entries.len());
                Kind::Link(String::new())
            }
            Some(S_IFDIR) => Kind::Dir,
            _ if name.ends_with(['/', '\\']) => Kind::Dir,
            Some(0) | Some(S_IFREG) | None => Kind::File { size },
            Some(_) => Kind::Skip("devices, sockets and FIFOs aren't extracted"),
        };
        let mtime = mtime.or_else(|| super::from_dos(date, time));
        entries.push(Entry { name, kind, mode: mode.map(|m| m & 0o7777), mtime, lossy: false });
        let span = 30u64.saturating_add(nlen as u64).saturating_add(csize);
        items.push(Item { offset, method, crc, csize, size, span });
    }
    // No two entries' data may overlap, and none may run into the directory.
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_unstable_by_key(|&i| items[i].offset);
    for w in order.windows(2) {
        let (a, b) = (&items[w[0]], &items[w[1]]);
        if a.offset.saturating_add(a.span) > b.offset {
            return Err(Error::Refused("its entries overlap, as in a decompression bomb".into()));
        }
    }
    if let Some(&last) = order.last()
        && items[last].offset.saturating_add(items[last].span) > cd_start
    {
        return Err(corrupt("an entry runs into the directory"));
    }
    let listing = Listing { entries, items, end: cd_start };
    let mut listing = listing;
    for i in links {
        let it = listing.items[i].clone();
        if it.size > limits::META as u64 || it.csize > limits::META as u64 {
            return Err(Error::Refused(format!("the link “{}” has a target too long to be real", listing.entries[i].name)));
        }
        let mut target = Vec::new();
        let mut rd = listing.open(f, i)?;
        rd.read_to_end(&mut target)?;
        rd.finish()?;
        listing.entries[i].kind = Kind::Link(String::from_utf8_lossy(&target).into_owned());
    }
    Ok(listing)
}

impl Listing {
    /// A reader of entry `i`'s data, decompressed, which [`Data::finish`] checks.
    pub fn open<'a>(&self, f: &'a File, i: usize) -> Result<Data<'a>, Error> {
        let it = &self.items[i];
        let cap = usize::try_from(it.csize).unwrap_or(usize::MAX).saturating_add(4096).clamp(8 * 1024, 256 * 1024);
        let mut r = BufReader::with_capacity(cap, f);
        r.seek(SeekFrom::Start(it.offset))?;
        let mut h = [0; 30];
        r.read_exact(&mut h)?;
        if u32_at(&h, 0) != LOCAL {
            return Err(corrupt("an entry's header is missing"));
        }
        let skip = u64::from(u16_at(&h, 26)) + u64::from(u16_at(&h, 28));
        let start = it.offset + 30 + skip;
        if start.checked_add(it.csize).is_none_or(|e| e > self.end) {
            return Err(corrupt("an entry runs into the directory"));
        }
        r.seek_relative(skip as i64)?;
        let raw = r.take(it.csize);
        let inner: Box<dyn Read + 'a> = if it.method == 8 { Box::new(flate2::bufread::DeflateDecoder::new(raw)) } else { Box::new(raw) };
        Ok(Data { inner, left: it.size, crc: crc32fast::Hasher::new(), want: it.crc })
    }
}

/// One entry's data: never more than the size it declared (more is an error), and checked
/// against its size and CRC once read ([`Data::finish`]).
pub struct Data<'a> {
    inner: Box<dyn Read + 'a>,
    left: u64,
    crc: crc32fast::Hasher,
    want: u32,
}

impl Read for Data<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let cap = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..cap])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "an entry ends before its size"));
        }
        self.left -= n as u64;
        self.crc.update(&buf[..n]);
        Ok(n)
    }
}

impl Data<'_> {
    /// Checks that the entry ended where it said and its CRC matches.
    pub fn finish(mut self) -> Result<(), Error> {
        if self.left > 0 {
            return Err(corrupt("an entry ends before its size"));
        }
        let mut one = [0];
        if self.inner.read(&mut one).map_err(Error::from)? != 0 {
            return Err(Error::Refused("an entry is bigger than it says, as in a decompression bomb".into()));
        }
        if self.crc.clone().finalize() != self.want {
            return Err(corrupt("an entry's CRC doesn't match its contents"));
        }
        Ok(())
    }
}

// ---- writing ---------------------------------------------------------------------------------

/// Files from this size up get ZIP64 room in their header (their compressed size may grow past
/// 4 GB, or the file may grow while it's read).
const ZIP64_FROM: u64 = 0xF000_0000;
/// Files up to this size are read whole and compressed in memory, then stored if that's smaller.
const SMALL: u64 = 1 << 20;
/// How much of a bigger file is deflated first to see whether it compresses at all.
const SAMPLE: u64 = 256 << 10;

/// What a file entry's data is made of.
struct Central {
    name: Vec<u8>,
    utf8: bool,
    method: u16,
    dos: (u16, u16),
    mtime: Option<i64>,
    crc: u32,
    csize: u64,
    size: u64,
    offset: u64,
    mode: u32,
    /// Its local header has the ZIP64 field.
    zip64: bool,
}

/// Writes a ZIP archive into a new file, one entry at a time.
pub struct Writer {
    out: BufWriter<File>,
    pos: u64,
    cd: Vec<Central>,
    level: u32,
}

/// Data written, counted.
struct Counting<'a> {
    w: &'a mut BufWriter<File>,
    n: u64,
}

impl Write for Counting<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = self.w.write(b)?;
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

/// The extended timestamp field's 32-bit time, when the time fits.
fn ut_time(t: Option<SystemTime>) -> Option<i64> {
    t.map(super::to_unix).filter(|&s| i32::try_from(s).is_ok())
}

impl Writer {
    /// Writes into `f` (a new, empty file) with deflate at `level` (1 to 9).
    pub fn new(f: File, level: u32) -> Writer {
        Writer { out: BufWriter::with_capacity(256 * 1024, f), pos: 0, cd: Vec::new(), level }
    }

    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.out.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }

    fn local(&mut self, c: &Central) -> io::Result<()> {
        let mut h = Vec::with_capacity(30 + c.name.len() + 29);
        h.extend(LOCAL.to_le_bytes());
        h.extend(if c.zip64 { 45u16 } else { 20 }.to_le_bytes());
        h.extend(if c.utf8 { 0x800u16 } else { 0 }.to_le_bytes());
        h.extend(c.method.to_le_bytes());
        h.extend(c.dos.1.to_le_bytes());
        h.extend(c.dos.0.to_le_bytes());
        h.extend(c.crc.to_le_bytes());
        let (cs, us) = if c.zip64 { (FF32, FF32) } else { (c.csize as u32, c.size as u32) };
        h.extend(cs.to_le_bytes());
        h.extend(us.to_le_bytes());
        h.extend((c.name.len() as u16).to_le_bytes());
        let xlen: u16 = if c.mtime.is_some() { 9 } else { 0 } + if c.zip64 { 20 } else { 0 };
        h.extend(xlen.to_le_bytes());
        h.extend(&c.name);
        if let Some(t) = c.mtime {
            h.extend(0x5455u16.to_le_bytes());
            h.extend(5u16.to_le_bytes());
            h.push(1);
            h.extend((t as i32).to_le_bytes());
        }
        if c.zip64 {
            h.extend(1u16.to_le_bytes());
            h.extend(16u16.to_le_bytes());
            h.extend(c.size.to_le_bytes());
            h.extend(c.csize.to_le_bytes());
        }
        self.put(&h)
    }

    fn entry(name: &[u8], utf8: bool, mtime: Option<SystemTime>, mode: u32, offset: u64) -> Central {
        Central {
            name: name.to_vec(),
            utf8,
            method: 0,
            dos: mtime.map_or((0x21, 0), super::to_dos),
            mtime: ut_time(mtime),
            crc: 0,
            csize: 0,
            size: 0,
            offset,
            mode,
            zip64: false,
        }
    }

    /// A folder (`name` ends with `/`).
    pub fn dir(&mut self, name: &[u8], utf8: bool, mtime: Option<SystemTime>, perm: u32) -> io::Result<()> {
        let c = Writer::entry(name, utf8, mtime, S_IFDIR | (perm & 0o777), self.pos);
        self.local(&c)?;
        self.cd.push(c);
        Ok(())
    }

    /// A symbolic link to `target` (Compress stores links on Unix; Windows leaves them out).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn link(&mut self, name: &[u8], utf8: bool, target: &[u8], mtime: Option<SystemTime>) -> io::Result<()> {
        let mut c = Writer::entry(name, utf8, mtime, S_IFLNK | 0o777, self.pos);
        c.crc = crc32fast::hash(target);
        (c.csize, c.size) = (target.len() as u64, target.len() as u64);
        self.local(&c)?;
        self.put(target)?;
        self.cd.push(c);
        Ok(())
    }

    /// A file, read from `src` (opened at its start; `size` is what it was when listed), with
    /// `perm` bits. `deflate`: compress it (else store it as it is, for formats that are already
    /// compressed). `go` hears each run of bytes read; `false` stops (a cancel).
    #[allow(clippy::too_many_arguments)]
    pub fn file(
        &mut self,
        name: &[u8],
        utf8: bool,
        src: &mut File,
        size: u64,
        mtime: Option<SystemTime>,
        perm: u32,
        deflate: bool,
        go: &mut dyn FnMut(u64) -> bool,
    ) -> Result<(), Error> {
        let mut c = Writer::entry(name, utf8, mtime, S_IFREG | (perm & 0o777), self.pos);
        if size <= SMALL {
            let mut data = Vec::with_capacity(size as usize);
            (&mut *src).take(SMALL + 1).read_to_end(&mut data)?;
            if data.len() as u64 <= SMALL {
                c.crc = crc32fast::hash(&data);
                c.size = data.len() as u64;
                let packed = if deflate && !data.is_empty() {
                    let mut e =
                        flate2::write::DeflateEncoder::new(Vec::with_capacity(data.len() / 2), flate2::Compression::new(self.level));
                    e.write_all(&data)?;
                    Some(e.finish()?).filter(|p| p.len() < data.len())
                } else {
                    None
                };
                let body = match &packed {
                    Some(p) => {
                        c.method = 8;
                        p
                    }
                    None => &data,
                };
                c.csize = body.len() as u64;
                self.local(&c)?;
                self.put(body)?;
                self.cd.push(c);
                if !go(data.len() as u64) {
                    return Err(Error::Cancelled);
                }
                return Ok(());
            }
            // It grew past the small size since it was listed: stream it.
            src.seek(SeekFrom::Start(0))?;
        }
        c.zip64 = size >= ZIP64_FROM;
        // A sample first: a big file that doesn't compress (encrypted, already compressed under
        // a name Satchel doesn't know) is stored without deflating it all to find out.
        let deflate = deflate && {
            let mut sample = Vec::with_capacity(SAMPLE as usize);
            (&mut *src).take(SAMPLE).read_to_end(&mut sample)?;
            src.seek(SeekFrom::Start(0))?;
            let mut e = flate2::write::DeflateEncoder::new(Vec::with_capacity(sample.len()), flate2::Compression::fast());
            e.write_all(&sample)?;
            e.finish()?.len() * 100 < sample.len() * 97
        };
        c.method = if deflate { 8 } else { 0 };
        self.local(&c)?;
        let start = self.pos;
        let (mut crc, mut n, mut packed) = self.stream(src, deflate, go)?;
        if deflate && packed >= n && n > 0 {
            // Deflate made it bigger: store it instead.
            self.out.flush()?;
            self.out.get_ref().set_len(start)?;
            self.out.seek(SeekFrom::Start(start))?;
            self.pos = start;
            src.seek(SeekFrom::Start(0))?;
            (crc, n, packed) = self.stream(src, false, go)?;
            c.method = 0;
        }
        if !c.zip64 && (n >= u64::from(FF32) || packed >= u64::from(FF32)) {
            return Err(Error::Io(io::Error::other("it grew past 4 GB while it was being compressed")));
        }
        (c.crc, c.size, c.csize) = (crc, n, packed);
        // Back to its header for the sizes and CRC, then on.
        let end = self.pos;
        self.out.seek(SeekFrom::Start(c.offset + 8))?;
        self.out.write_all(&c.method.to_le_bytes())?;
        self.out.seek(SeekFrom::Start(c.offset + 14))?;
        self.out.write_all(&crc.to_le_bytes())?;
        if c.zip64 {
            let at = c.offset + 30 + c.name.len() as u64 + if c.mtime.is_some() { 9 } else { 0 } + 4;
            self.out.seek(SeekFrom::Start(at))?;
            self.out.write_all(&n.to_le_bytes())?;
            self.out.write_all(&packed.to_le_bytes())?;
        } else {
            self.out.write_all(&(packed as u32).to_le_bytes())?;
            self.out.write_all(&(n as u32).to_le_bytes())?;
        }
        self.out.seek(SeekFrom::Start(end))?;
        self.cd.push(c);
        Ok(())
    }

    /// Copies `src` to the archive, deflated or not: its CRC, its size and what it took.
    fn stream(&mut self, src: &mut File, deflate: bool, go: &mut dyn FnMut(u64) -> bool) -> Result<(u32, u64, u64), Error> {
        let mut crc = crc32fast::Hasher::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut n = 0u64;
        let level = flate2::Compression::new(self.level);
        enum Sink<'a> {
            Plain(Counting<'a>),
            Deflate(flate2::write::DeflateEncoder<Counting<'a>>),
        }
        let w = Counting { w: &mut self.out, n: 0 };
        let mut sink = if deflate { Sink::Deflate(flate2::write::DeflateEncoder::new(w, level)) } else { Sink::Plain(w) };
        loop {
            let k = match src.read(&mut buf) {
                Ok(0) => break,
                Ok(k) => k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Error::Io(e)),
            };
            crc.update(&buf[..k]);
            n += k as u64;
            match &mut sink {
                Sink::Deflate(e) => e.write_all(&buf[..k])?,
                Sink::Plain(w) => w.write_all(&buf[..k])?,
            }
            if !go(k as u64) {
                return Err(Error::Cancelled);
            }
        }
        let packed = match sink {
            Sink::Deflate(e) => e.finish()?.n,
            Sink::Plain(w) => w.n,
        };
        self.pos += packed;
        Ok((crc.finalize(), n, packed))
    }

    /// Writes the central directory and the end records; hands back the file, flushed.
    pub fn finish(mut self) -> io::Result<File> {
        let start = self.pos;
        let cd = std::mem::take(&mut self.cd);
        for c in &cd {
            let wide_off = c.offset >= u64::from(FF32);
            let mut x = Vec::new();
            if let Some(t) = c.mtime {
                x.extend(0x5455u16.to_le_bytes());
                x.extend(5u16.to_le_bytes());
                x.push(1);
                x.extend((t as i32).to_le_bytes());
            }
            if c.zip64 || wide_off {
                let mut d = Vec::new();
                if c.zip64 {
                    d.extend(c.size.to_le_bytes());
                    d.extend(c.csize.to_le_bytes());
                }
                if wide_off {
                    d.extend(c.offset.to_le_bytes());
                }
                x.extend(1u16.to_le_bytes());
                x.extend((d.len() as u16).to_le_bytes());
                x.extend(d);
            }
            let mut h = Vec::with_capacity(46 + c.name.len() + x.len());
            h.extend(CENTRAL.to_le_bytes());
            h.extend((3u16 << 8 | 63).to_le_bytes());
            h.extend(if c.zip64 || wide_off { 45u16 } else { 20 }.to_le_bytes());
            h.extend(if c.utf8 { 0x800u16 } else { 0 }.to_le_bytes());
            h.extend(c.method.to_le_bytes());
            h.extend(c.dos.1.to_le_bytes());
            h.extend(c.dos.0.to_le_bytes());
            h.extend(c.crc.to_le_bytes());
            let (cs, us) = if c.zip64 { (FF32, FF32) } else { (c.csize as u32, c.size as u32) };
            h.extend(cs.to_le_bytes());
            h.extend(us.to_le_bytes());
            h.extend((c.name.len() as u16).to_le_bytes());
            h.extend((x.len() as u16).to_le_bytes());
            h.extend(0u16.to_le_bytes()); // comment
            h.extend(0u16.to_le_bytes()); // disk
            h.extend(0u16.to_le_bytes()); // internal attributes
            let dos_dir = if c.mode & S_IFMT == S_IFDIR { 0x10 } else { 0 };
            h.extend((c.mode << 16 | dos_dir).to_le_bytes());
            h.extend(if wide_off { FF32 } else { c.offset as u32 }.to_le_bytes());
            h.extend(&c.name);
            h.extend(x);
            self.put(&h)?;
        }
        let size = self.pos - start;
        let n = cd.len() as u64;
        let wide = n >= u64::from(FF16) || start >= u64::from(FF32) || size >= u64::from(FF32);
        if wide {
            let at = self.pos;
            let mut e = Vec::with_capacity(76);
            e.extend(END64.to_le_bytes());
            e.extend(44u64.to_le_bytes());
            e.extend((3u16 << 8 | 63).to_le_bytes());
            e.extend(45u16.to_le_bytes());
            e.extend(0u32.to_le_bytes());
            e.extend(0u32.to_le_bytes());
            e.extend(n.to_le_bytes());
            e.extend(n.to_le_bytes());
            e.extend(size.to_le_bytes());
            e.extend(start.to_le_bytes());
            e.extend(LOCATOR64.to_le_bytes());
            e.extend(0u32.to_le_bytes());
            e.extend(at.to_le_bytes());
            e.extend(1u32.to_le_bytes());
            self.put(&e)?;
        }
        let mut e = Vec::with_capacity(22);
        e.extend(END.to_le_bytes());
        e.extend(0u16.to_le_bytes());
        e.extend(0u16.to_le_bytes());
        let n16 = if wide { FF16 } else { n as u16 };
        e.extend(n16.to_le_bytes());
        e.extend(n16.to_le_bytes());
        e.extend(if wide { FF32 } else { size as u32 }.to_le_bytes());
        e.extend(if wide { FF32 } else { start as u32 }.to_le_bytes());
        e.extend(0u16.to_le_bytes());
        self.put(&e)?;
        self.out.flush()?;
        self.out.into_inner().map_err(|e| e.into_error())
    }
}
