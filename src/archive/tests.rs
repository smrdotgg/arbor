use super::*;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

use crate::rename::Platform::{Linux, Mac, Windows};

pub(crate) fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-archive-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn file(name: &str, size: u64) -> Entry {
    Entry { name: name.into(), kind: Kind::File { size }, mode: Some(0o644), mtime: None, lossy: false }
}
fn dir(name: &str) -> Entry {
    Entry { name: name.into(), kind: Kind::Dir, mode: Some(0o755), mtime: None, lossy: false }
}
fn link(name: &str, to: &str) -> Entry {
    Entry { name: name.into(), kind: Kind::Link(to.into()), mode: None, mtime: None, lossy: false }
}

fn refused(r: Result<Plan, Error>) -> String {
    match r {
        Err(Error::Refused(s)) => s,
        Err(e) => panic!("refused for another reason: {e}"),
        Ok(p) => panic!("not refused: {p:?}"),
    }
}

#[test]
fn every_way_out_of_the_folder_is_refused() {
    for name in [
        "/etc/passwd",
        "\\etc\\passwd",
        "\\\\server\\share\\x",
        "//server/share/x",
        "C:\\Windows\\x",
        "c:x",
        "..",
        "../x",
        "a/../../x",
        "a/b/../../../x",
        "a\\..\\..\\x",
        "ok/..",
        "a\0b",
    ] {
        assert!(parts(name).is_err(), "{name:?} should be refused");
        // And the whole archive with it, whatever else is there.
        let why = refused(plan("a.zip", 1000, &[file("fine.txt", 1), file(name, 1)], Linux));
        assert!(!why.is_empty());
    }
    assert_eq!(parts("a/./b//c/").unwrap(), ["a", "b", "c"]);
    assert_eq!(parts("./").unwrap(), Vec::<&str>::new());
    assert_eq!(parts("a\\b").unwrap(), ["a", "b"], "a backslash separates, as Windows tools write it");
    assert_eq!(parts("..a/b..").unwrap(), ["..a", "b.."], "dots inside names are names");
}

#[test]
fn names_invalid_here_are_renamed_validly() {
    assert_eq!(fix("a:b", Windows).as_deref(), Some("a_b"));
    assert_eq!(fix("what?.txt", Windows).as_deref(), Some("what_.txt"));
    assert_eq!(fix("CON.txt", Windows).as_deref(), Some("_CON.txt"));
    assert_eq!(fix("nul", Windows).as_deref(), Some("_nul"));
    assert_eq!(fix("trailing. ", Windows).as_deref(), Some("trailing_"));
    assert_eq!(fix("tab\there", Windows).as_deref(), Some("tab_here"));
    assert_eq!(fix("a:b", Linux), None, "valid on Linux");
    assert_eq!(fix("CON.txt", Mac), None);
    let long = format!("{}.txt", "é".repeat(200));
    let fixed = fix(&long, Linux).unwrap();
    assert!(fixed.len() <= 255 && fixed.ends_with(".txt"), "{fixed}");
    assert!(crate::rename::invalid(&fixed, Linux).is_none());
    let fixed = fix(&"x".repeat(300), Windows).unwrap();
    assert_eq!(fixed.len(), 255);
    // In a plan: the name changes, and a note says so.
    let p = plan("a.zip", 100, &[file("d/a:b.txt", 1), file("d/c", 1)], Windows).unwrap();
    assert_eq!(p.items[0].rel, ["a_b.txt"]);
    assert!(p.notes.iter().any(|n| n.contains("“a:b.txt” was renamed “a_b.txt”")), "{:?}", p.notes);
}

#[test]
fn one_folder_holding_everything_is_the_folder_made() {
    let p = plan("Photos.zip", 100, &[dir("Photos/"), file("Photos/a.jpg", 1), dir("Photos/sub/"), file("Photos/sub/b.jpg", 1)], Linux)
        .unwrap();
    assert_eq!(p.root, "Photos");
    let rels: Vec<String> = p.items.iter().map(|i| i.rel.join("/")).collect();
    assert_eq!(rels, ["a.jpg", "sub", "sub/b.jpg"]);
    // Without its own entry too.
    let p = plan("x.zip", 100, &[file("Other/a", 1)], Linux).unwrap();
    assert_eq!(p.root, "Other");
    // Several items, or one file: a folder named after the archive.
    let p = plan("Stuff.tar.gz", 100, &[file("a", 1), file("b/c", 1)], Linux).unwrap();
    assert_eq!((p.root.as_str(), p.items.len()), ("Stuff", 2));
    let p = plan("one.zip", 100, &[file("notes.txt", 1)], Linux).unwrap();
    assert_eq!(p.root, "one");
    assert_eq!(Format::stem("A.B.TGZ"), "A.B");
    assert_eq!(Format::stem(".zip"), "Archive");
}

#[test]
fn links_that_leave_the_folder_are_refused() {
    for (at, to) in [
        ("l", "../outside"),
        ("l", "/etc/passwd"),
        ("l", "C:\\Windows"),
        ("d/l", "../../x"),
        ("d/e/l", "../../../x"),
        ("l", "a/../../x"),
        ("l", "\\\\server\\share"),
    ] {
        let why = refused(plan("a.zip", 100, &[file("f", 1), link(at, to)], Linux));
        assert!(why.contains("link"), "{at} → {to}: {why}");
    }
    // The top folder is the folder made: "../x" from inside it is outside.
    let why = refused(plan("Photos.zip", 100, &[dir("Photos/"), link("Photos/l", "../x")], Linux));
    assert!(why.contains("outside"), "{why}");
    // Down and back up: never followed, whatever the names on the way are (they may be links,
    // or renamed when made).
    for to in ["x/y/l/../..", "x/y/L/../..", "a/./../b", "a/b/../../.."] {
        let why = refused(plan("a.zip", 100, &[dir("x/y/"), link("x/y/l", ".."), link("m", to), file("f", 1)], Linux));
        assert!(why.contains("goes down and back up"), "{to}: {why}");
    }
    // Down through another link of the archive: that link keeps to the same rule, so this stays
    // inside.
    assert!(plan("a.zip", 100, &[dir("x/y/"), link("x/y/l", ".."), link("m", "x/y/l/z"), file("f", 1)], Linux).is_ok());
    // Links that stay inside are fine.
    let p = plan(
        "a.zip",
        100,
        &[
            file("lib/libfoo.so.1", 1),
            link("lib/libfoo.so", "libfoo.so.1"),
            link("bin/foo", "../lib/libfoo.so"),
            link("here", "."),
            file("z", 1),
        ],
        Linux,
    )
    .unwrap();
    assert_eq!(p.items.len(), 5);
    // Windows makes none, and says so.
    let p = plan("a.zip", 100, &[file("f", 1), link("l", "f")], Windows).unwrap();
    assert_eq!(p.items.len(), 1);
    assert!(p.notes.iter().any(|n| n.contains("links aren't extracted on Windows")), "{:?}", p.notes);
}

/// The review's case: a folder `a/b/c/Y`, a link `a/b/c/Y → ../../..` (which gets the free name
/// "Y 2" when it's made, the folder having taken "Y"), and a link through the renamed one that
/// climbs four folders above the destination. Checked by names, it passed (the names didn't
/// match); by the target alone, it goes down and back up.
#[test]
fn a_link_through_a_renamed_link_is_refused() {
    let entries = [dir("a/b/c/Y/"), link("a/b/c/Y", "../../.."), link("X", "a/b/c/Y 2/../../../../in1"), file("f", 1)];
    let why = refused(plan("evil.zip", 100, &entries, Linux));
    assert!(why.contains("“X”") && why.contains("down and back up"), "{why}");
    assert!(link_target(3, "../../..").is_ok());
    assert!(link_target(2, "../../..").is_err());
    assert!(link_target(0, "./.").is_ok());
}

#[test]
fn bombs_are_refused_by_size() {
    // 300 MB from a 1 MB archive: over 200 times its size, past the floor.
    let why = refused(plan("b.zip", 1 << 20, &[file("a", 300 << 20)], Linux));
    assert!(why.contains("too large to extract") && why.contains("times"), "{why}");
    // The same from a 10 MB archive is fine; so is 200 MB from a tiny one (under the floor).
    assert!(plan("b.zip", 10 << 20, &[file("a", 300 << 20)], Linux).is_ok());
    assert!(plan("b.zip", 1000, &[file("a", 200 << 20)], Linux).is_ok());
    // Past the absolute cap, whatever the archive's size; sizes that would overflow add up to it.
    let why = refused(plan("b.zip", u64::MAX / 2, &[file("a", limits::TOTAL + 1)], Linux));
    assert!(why.contains("more than Arbor extracts"), "{why}");
    assert!(plan("b.zip", u64::MAX / 2, &[file("a", u64::MAX), file("b", u64::MAX)], Linux).is_err());
    // Too deep.
    let deep = "d/".repeat(limits::DEPTH + 1) + "f";
    assert!(refused(plan("b.zip", 100, &[file(&deep, 1)], Linux)).contains("deep"));
}

#[test]
fn macos_resource_forks_are_left_out() {
    let p = plan("a.zip", 100, &[dir("a/"), file("a/x", 1), dir("__MACOSX/"), file("__MACOSX/a/._x", 1)], Linux).unwrap();
    assert_eq!(p.root, "a", "the one folder, once __MACOSX is left out");
    assert_eq!(p.items.len(), 1);
    assert!(p.notes.is_empty());
}

#[test]
fn skipped_entries_are_noted() {
    let e = Entry { name: "hard".into(), kind: Kind::Skip("hard links aren't extracted"), mode: None, mtime: None, lossy: false };
    let p = plan("t.tar", 100, &[file("f", 1), e.clone(), Entry { name: "hard2".into(), ..e }], Linux).unwrap();
    assert_eq!(p.items.len(), 1);
    assert!(p.notes.iter().any(|n| n == "“hard” was left out: hard links aren't extracted (and 1 more)"), "{:?}", p.notes);
}

#[test]
fn dos_times_and_code_page_437() {
    let t = from_unix(1_700_000_000).unwrap();
    let (d, tm) = to_dos(t);
    let back = from_dos(d, tm).unwrap();
    let diff = to_unix(back) - 1_700_000_000;
    assert!((-2..=0).contains(&diff), "{diff}");
    assert_eq!(to_dos(from_unix(0).unwrap()), (0x21, 0), "before 1980: its first day");
    assert_eq!(from_dos(0, 0), None);
    assert_eq!(decode_cp437(b"caf\x82 \x9c"), "café £");
    assert_eq!(decode_cp437("café".as_bytes()), "café", "UTF-8 without the flag stays UTF-8");
    assert_eq!(group(1234567), "1,234,567");
}

// ---- ZIP ----------------------------------------------------------------------------------

/// Writes a ZIP at `path` with `add`, and reads its list back.
fn zip_with(path: &Path, add: impl FnOnce(&mut zip::Writer)) -> zip::Listing {
    let f = fs::File::create_new(path).unwrap();
    let mut w = zip::Writer::new(f, 6);
    add(&mut w);
    w.finish().unwrap();
    let f = fs::File::open(path).unwrap();
    zip::list(&f, f.metadata().unwrap().len(), &|| false).unwrap()
}

fn src_file(d: &Path, name: &str, data: &[u8]) -> fs::File {
    let p = d.join(name);
    fs::write(&p, data).unwrap();
    fs::File::open(p).unwrap()
}

fn read_entry(path: &Path, l: &zip::Listing, i: usize) -> Result<Vec<u8>, Error> {
    let f = fs::File::open(path).unwrap();
    let mut d = l.open(&f, i)?;
    let mut v = Vec::new();
    d.read_to_end(&mut v)?;
    d.finish()?;
    Ok(v)
}

/// Bytes nothing compresses (a fixed xorshift stream).
pub(crate) fn noise(n: usize) -> Vec<u8> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[test]
fn zip_round_trip() {
    let d = tmp("zip-rt");
    let text = "hello ".repeat(10_000).into_bytes();
    let big = noise(3 << 20); // past the in-memory size, and incompressible: stored after all
    let compressible = "abc".repeat(1 << 20).into_bytes(); // streamed and deflated
    let t = from_unix(1_600_000_000).unwrap();
    let z = d.join("a.zip");
    let l = zip_with(&z, |w| {
        w.dir("dir/".as_bytes(), true, Some(t), 0o755).unwrap();
        w.dir("dir/empty/".as_bytes(), true, None, 0o700).unwrap();
        let mut f = src_file(&d, "t", &text);
        w.file("dir/ünïcödé ✓.txt".as_bytes(), true, &mut f, text.len() as u64, Some(t), 0o640, true, &mut |_| true).unwrap();
        let mut f = src_file(&d, "e", b"");
        w.file(b"dir/empty.txt", true, &mut f, 0, None, 0o600, true, &mut |_| true).unwrap();
        let mut f = src_file(&d, "b", &big);
        w.file(b"dir/big.bin", true, &mut f, big.len() as u64, None, 0o755, true, &mut |_| true).unwrap();
        let mut f = src_file(&d, "c", &compressible);
        w.file(b"dir/c.txt", true, &mut f, compressible.len() as u64, None, 0o644, true, &mut |_| true).unwrap();
        w.link(b"dir/l", true, b"c.txt", None).unwrap();
    });
    let names: Vec<&str> = l.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["dir/", "dir/empty/", "dir/ünïcödé ✓.txt", "dir/empty.txt", "dir/big.bin", "dir/c.txt", "dir/l"]);
    assert_eq!(l.entries[0].kind, Kind::Dir);
    assert_eq!(l.entries[0].mtime, Some(t));
    assert_eq!(l.entries[2].mode, Some(0o640));
    assert_eq!(l.entries[4].mode, Some(0o755));
    assert_eq!(l.entries[6].kind, Kind::Link("c.txt".into()));
    assert_eq!(read_entry(&z, &l, 2).unwrap(), text);
    assert_eq!(read_entry(&z, &l, 3).unwrap(), b"");
    assert_eq!(read_entry(&z, &l, 4).unwrap(), big);
    assert_eq!(read_entry(&z, &l, 5).unwrap(), compressible);
    let size = fs::metadata(&z).unwrap().len();
    assert!(size < (big.len() + 200_000) as u64, "the noise was stored, the text deflated: {size}");
}

#[test]
fn zip64_for_many_entries() {
    let d = tmp("zip64");
    let z = d.join("many.zip");
    let l = zip_with(&z, |w| {
        for i in 0..70_000 {
            w.dir(format!("d{i}/").as_bytes(), true, None, 0o755).unwrap();
        }
    });
    assert_eq!(l.entries.len(), 70_000);
    assert_eq!(l.entries[69_999].name, "d69999/");
}

/// A ZIP written byte by byte, for archives Arbor's writer would never make.
pub(crate) struct Raw {
    data: Vec<u8>,
    cd: Vec<u8>,
    n: u16,
}

impl Raw {
    pub(crate) fn new() -> Raw {
        Raw { data: Vec::new(), cd: Vec::new(), n: 0 }
    }

    /// An entry: its name, flags, method, the CRC and sizes it declares, and its data as stored.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn entry(&mut self, name: &str, flags: u16, method: u16, crc: u32, csize: u32, size: u32, body: &[u8], offset: Option<u32>) {
        let off = offset.unwrap_or(self.data.len() as u32);
        if offset.is_none() {
            let mut h = Vec::new();
            h.extend(0x0403_4b50u32.to_le_bytes());
            h.extend(20u16.to_le_bytes());
            h.extend(flags.to_le_bytes());
            h.extend(method.to_le_bytes());
            h.extend([0, 0, 0x21, 0]);
            h.extend(crc.to_le_bytes());
            h.extend(csize.to_le_bytes());
            h.extend(size.to_le_bytes());
            h.extend((name.len() as u16).to_le_bytes());
            h.extend(0u16.to_le_bytes());
            h.extend(name.as_bytes());
            self.data.extend(h);
            self.data.extend(body);
        }
        let mut c = Vec::new();
        c.extend(0x0201_4b50u32.to_le_bytes());
        c.extend((3u16 << 8 | 20).to_le_bytes());
        c.extend(20u16.to_le_bytes());
        c.extend(flags.to_le_bytes());
        c.extend(method.to_le_bytes());
        c.extend([0, 0, 0x21, 0]);
        c.extend(crc.to_le_bytes());
        c.extend(csize.to_le_bytes());
        c.extend(size.to_le_bytes());
        c.extend((name.len() as u16).to_le_bytes());
        c.extend([0; 8]);
        c.extend((0o100_644u32 << 16).to_le_bytes());
        c.extend(off.to_le_bytes());
        c.extend(name.as_bytes());
        self.cd.extend(c);
        self.n += 1;
    }

    pub(crate) fn stored(&mut self, name: &str, body: &[u8]) {
        self.entry(name, 0, 0, crc32fast::hash(body), body.len() as u32, body.len() as u32, body, None);
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        let mut v = self.data.clone();
        let start = v.len() as u32;
        v.extend(&self.cd);
        v.extend(0x0605_4b50u32.to_le_bytes());
        v.extend([0; 4]);
        v.extend(self.n.to_le_bytes());
        v.extend(self.n.to_le_bytes());
        v.extend((self.cd.len() as u32).to_le_bytes());
        v.extend(start.to_le_bytes());
        v.extend([0; 2]);
        v
    }

    fn write(&self, p: &Path) -> Result<zip::Listing, Error> {
        fs::write(p, self.bytes()).unwrap();
        let f = fs::File::open(p).unwrap();
        zip::list(&f, f.metadata().unwrap().len(), &|| false)
    }
}

fn deflated(b: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

#[test]
fn zip_damage_is_an_error() {
    let d = tmp("zip-bad");
    let mut r = Raw::new();
    r.stored("a.txt", b"first");
    r.stored("b.txt", b"second");
    let bytes = r.bytes();
    // Cut short: no directory to read.
    fs::write(d.join("cut.zip"), &bytes[..bytes.len() / 2]).unwrap();
    let f = fs::File::open(d.join("cut.zip")).unwrap();
    assert!(matches!(zip::list(&f, f.metadata().unwrap().len(), &|| false), Err(Error::Corrupt(_))));
    // Not a ZIP at all.
    fs::write(d.join("text.zip"), "just text, nothing else in here at all").unwrap();
    let f = fs::File::open(d.join("text.zip")).unwrap();
    assert!(matches!(zip::list(&f, f.metadata().unwrap().len(), &|| false), Err(Error::Corrupt(_))));
    // A flipped byte: the CRC doesn't match.
    let mut bad = bytes.clone();
    let at = bad.windows(6).position(|w| w == b"second").unwrap();
    bad[at] = b'S';
    fs::write(d.join("crc.zip"), &bad).unwrap();
    let f = fs::File::open(d.join("crc.zip")).unwrap();
    let l = zip::list(&f, f.metadata().unwrap().len(), &|| false).unwrap();
    assert_eq!(read_entry(&d.join("crc.zip"), &l, 0).unwrap(), b"first");
    let e = read_entry(&d.join("crc.zip"), &l, 1).unwrap_err();
    assert!(e.to_string().contains("CRC"), "{e}");
    // A broken deflate stream.
    let mut r = Raw::new();
    r.entry("x", 0, 8, 0, 4, 100, b"\xff\xff\xff\xff", None);
    let l = r.write(&d.join("inflate.zip")).unwrap();
    assert!(matches!(read_entry(&d.join("inflate.zip"), &l, 0), Err(Error::Corrupt(_))));
}

#[test]
fn zip_features_arbor_cant_read_are_named() {
    let d = tmp("zip-unsup");
    let mut r = Raw::new();
    r.entry("secret.txt", 1, 0, 0, 3, 3, b"abc", None);
    let e = r.write(&d.join("enc.zip")).err().unwrap();
    assert!(e.to_string().contains("encrypted"), "{e}");
    let mut r = Raw::new();
    r.entry("x.bz", 0, 12, 0, 3, 3, b"abc", None);
    let e = r.write(&d.join("bz.zip")).err().unwrap();
    assert!(e.to_string().contains("bzip2"), "{e}");
}

#[test]
fn zip_bombs_are_refused() {
    let d = tmp("zip-bomb");
    // Overlapping entries: two names for one stream.
    let body = deflated(&vec![0; 100_000]);
    let mut r = Raw::new();
    let crc = crc32fast::hash(&vec![0; 100_000]);
    r.entry("a", 0, 8, crc, body.len() as u32, 100_000, &body, None);
    r.entry("b", 0, 8, crc, body.len() as u32, 100_000, &[], Some(0));
    let e = r.write(&d.join("overlap.zip")).err().unwrap();
    assert!(matches!(e, Error::Refused(ref s) if s.contains("overlap")), "{e}");
    // An entry that inflates past the size it declares: cut off there, and refused.
    let big = vec![7u8; 1 << 20];
    let body = deflated(&big);
    let mut r = Raw::new();
    r.entry("liar", 0, 8, crc32fast::hash(&big[..10]), body.len() as u32, 10, &body, None);
    let l = r.write(&d.join("liar.zip")).unwrap();
    let e = read_entry(&d.join("liar.zip"), &l, 0).unwrap_err();
    assert!(matches!(e, Error::Refused(ref s) if s.contains("bigger than it says")), "{e}");
    // Or ends before it.
    let mut r = Raw::new();
    let small = deflated(b"short");
    r.entry("short", 0, 8, crc32fast::hash(b"short"), small.len() as u32, 1000, &small, None);
    let l = r.write(&d.join("short.zip")).unwrap();
    assert!(matches!(read_entry(&d.join("short.zip"), &l, 0), Err(Error::Corrupt(_))));
    // Declared sizes far past the archive's: refused by the plan before anything is written.
    let mut r = Raw::new();
    for i in 0..4 {
        r.entry(&format!("z{i}"), 0, 8, 0, 2, u32::MAX - 1, &[3, 0], None);
    }
    let l = r.write(&d.join("ratio.zip")).unwrap();
    let why = refused(plan("ratio.zip", fs::metadata(d.join("ratio.zip")).unwrap().len(), &l.entries, Linux));
    assert!(why.contains("too large"), "{why}");
}

// ---- tar ------------------------------------------------------------------------------------

/// A ustar header block.
pub(crate) fn tar_header(name: &str, kind: u8, size: u64, link: &str) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..108].copy_from_slice(b"0000644\0");
    h[108..116].copy_from_slice(b"0001750\0");
    h[116..124].copy_from_slice(b"0001750\0");
    h[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    h[136..148].copy_from_slice(b"14500000000\0");
    h[148..156].copy_from_slice(b"        ");
    h[156] = kind;
    h[157..157 + link.len()].copy_from_slice(link.as_bytes());
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

/// A tar entry with its data, padded.
pub(crate) fn tar_entry(out: &mut Vec<u8>, name: &str, kind: u8, data: &[u8], link: &str) {
    out.extend(tar_header(name, kind, data.len() as u64, link));
    out.extend(data);
    out.resize(out.len().div_ceil(512) * 512, 0);
}

/// One pax record: "<length> <key>=<value>\n", the length counting itself.
fn pax_record(k: &str, v: &str) -> String {
    let rest = format!(" {k}={v}\n");
    let mut n = rest.len();
    loop {
        let s = format!("{n}{rest}");
        if s.len() == n {
            return s;
        }
        n = s.len();
    }
}

#[test]
fn tar_reads_ustar_pax_and_gnu() {
    let long = format!("{}/file.txt", "deep".repeat(40));
    let mut t = Vec::new();
    tar_entry(&mut t, "top/", b'5', b"", "");
    tar_entry(&mut t, "top/a.txt", b'0', b"hello", "");
    tar_entry(&mut t, "top/l", b'2', b"", "a.txt");
    tar_entry(&mut t, "top/hard", b'1', b"", "top/a.txt");
    tar_entry(&mut t, "top/fifo", b'6', b"", "");
    let pax = pax_record("path", &format!("top/{long}"));
    tar_entry(&mut t, "././@PaxHeader", b'x', pax.as_bytes(), "");
    tar_entry(&mut t, "top/short-pax", b'0', b"pax", "");
    tar_entry(&mut t, "././@LongLink", b'L', format!("top/gnu-{long}\0").as_bytes(), "");
    tar_entry(&mut t, "top/short-gnu", b'0', b"gnu", "");
    t.extend([0; 1024]);
    let entries = tar::list(&t[..], t.len() as u64, &|| false).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["top/", "top/a.txt", "top/l", "top/hard", "top/fifo", &format!("top/{long}"), &format!("top/gnu-{long}")]);
    assert_eq!(entries[1].kind, Kind::File { size: 5 });
    assert_eq!(entries[2].kind, Kind::Link("a.txt".into()));
    assert!(matches!(entries[3].kind, Kind::Skip(_)) && matches!(entries[4].kind, Kind::Skip(_)));
    assert_eq!(entries[1].mtime, from_unix(0o14500000000));
    // Reading the data back, entry by entry.
    let mut rd = tar::Reader::new(&t[..]);
    let mut got = Vec::new();
    while let Some(e) = rd.next().unwrap() {
        let mut buf = [0u8; 64];
        let n = rd.read_data(&mut buf).unwrap();
        got.push((e.name, buf[..n].to_vec()));
    }
    assert_eq!(got[1].1, b"hello");
    assert_eq!(got[6].1, b"gnu");
}

#[test]
fn tar_damage_is_an_error() {
    let mut t = Vec::new();
    tar_entry(&mut t, "a.txt", b'0', b"hello world", "");
    tar_entry(&mut t, "b.txt", b'0', b"more", "");
    t.extend([0; 1024]);
    // A changed header byte: its checksum doesn't match.
    let mut bad = t.clone();
    bad[512 + 512] ^= 1; // b.txt's header, first byte of its name
    assert!(matches!(tar::list(&bad[..], 0, &|| false), Err(Error::Corrupt(ref s)) if s.contains("checksum")));
    // Cut in the middle of an entry, or of a header.
    assert!(matches!(tar::list(&t[..600], 0, &|| false), Err(Error::Corrupt(_))));
    assert!(matches!(tar::list(&t[..1024 + 100], 0, &|| false), Err(Error::Corrupt(_))));
    // No end blocks: tolerated.
    assert_eq!(tar::list(&t[..2048], 0, &|| false).unwrap().len(), 2);
    // A declared size past the limits.
    let mut huge = Vec::new();
    huge.extend(tar_header("big", b'0', 0o77777777777, ""));
    assert!(matches!(tar::list(&huge[..], 1000, &|| false), Err(Error::Refused(_))));
}

#[test]
fn formats_by_name() {
    assert_eq!(Format::of("a.ZIP"), Some(Format::Zip));
    assert_eq!(Format::of("a.tar.gz"), Some(Format::TarGz));
    assert_eq!(Format::of("a.tgz"), Some(Format::TarGz));
    assert_eq!(Format::of("a.tar"), Some(Format::Tar));
    assert_eq!(Format::of("a.gz"), None);
    assert_eq!(Format::of("zip"), None);
}
