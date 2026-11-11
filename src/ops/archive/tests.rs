use super::*;
use crate::archive::tests::{Raw, noise, tar_entry};
use std::fs;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-arc-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn run(op: Op) -> Outcome {
    run_with(op, crate::ops::tests::fake_trash)
}

fn hooked(op: Op, hooks: Hooks) -> Outcome {
    run_hooked(op, crate::ops::tests::fake_trash, hooks)
}

/// Names in `d`, sorted (the fake trash's `.trash` left out).
fn names(d: &Path) -> Vec<String> {
    let mut v: Vec<String> =
        fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n != ".trash").collect();
    v.sort();
    v
}

/// Everything under `root`: each path below it, and what it is (a folder, a file's bytes, a
/// link's target), sorted.
fn tree(root: &Path) -> Vec<(String, String)> {
    fn walk(root: &Path, p: &Path, out: &mut Vec<(String, String)>) {
        for e in fs::read_dir(p).unwrap() {
            let e = e.unwrap();
            let path = e.path();
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            let m = path.symlink_metadata().unwrap();
            if m.file_type().is_symlink() {
                out.push((rel, format!("link → {}", fs::read_link(&path).unwrap().display())));
            } else if m.is_dir() {
                out.push((rel, "folder".into()));
                walk(root, &path, out);
            } else {
                let b = fs::read(&path).unwrap();
                out.push((rel, format!("{} bytes, crc {:08x}", b.len(), crc32fast::hash(&b))));
            }
        }
    }
    let mut v = Vec::new();
    walk(root, root, &mut v);
    v.sort();
    v
}

fn ok(o: &Outcome) {
    assert!(o.refused.is_none() && o.errors.is_empty(), "{:?} {:?}", o.refused, o.errors);
}

/// A folder of mixed things: text, an incompressible file past the in-memory size, an empty
/// folder, a Unicode name, and (Unix) a link and an executable.
fn sample(d: &Path) -> PathBuf {
    let src = d.join("src");
    fs::create_dir_all(src.join("sub/deeper")).unwrap();
    fs::create_dir(src.join("empty")).unwrap();
    fs::write(src.join("a.txt"), "hello ".repeat(1000)).unwrap();
    fs::write(src.join("sub/b.bin"), noise(2 << 20)).unwrap();
    fs::write(src.join("sub/deeper/ünïcödé ✓.txt"), "unicode").unwrap();
    fs::write(src.join("photo.jpg"), noise(5000)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::os::unix::fs::symlink("a.txt", src.join("link")).unwrap();
        fs::write(src.join("run.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(src.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let t = std::time::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    fs::File::options().write(true).open(src.join("a.txt")).unwrap().set_modified(t).unwrap();
    src
}

fn compress(sources: &[PathBuf]) -> Outcome {
    let dir = sources[0].parent().unwrap().to_path_buf();
    run(Op::Compress { sources: sources.to_vec(), dir })
}

fn extract(archive: &Path) -> Outcome {
    run(Op::Extract { archives: vec![archive.to_path_buf()] })
}

#[test]
fn compress_then_extract_gives_back_the_same_tree() {
    let d = tmp("round");
    let src = sample(&d);
    let o = compress(std::slice::from_ref(&src));
    ok(&o);
    assert_eq!(o.results, [d.join("src.zip")]);
    assert_eq!(o.said.as_deref(), Some("Compressed “src” into “src.zip”"));
    assert!(o.undo.is_some());
    assert_eq!(names(&d), ["src", "src.zip"], "no temporary file left");
    // "src" is taken: the folder in it comes out as "src 2".
    let o = extract(&d.join("src.zip"));
    ok(&o);
    assert_eq!(o.results, [d.join("src 2")]);
    assert_eq!(o.said.as_deref(), Some("Extracted “src.zip” into “src 2”"));
    assert_eq!(tree(&src), tree(&d.join("src 2")));
    let t = |p: &Path| fs::symlink_metadata(p).unwrap().modified().unwrap();
    assert_eq!(t(&d.join("src 2/a.txt")), t(&src.join("a.txt")), "modification times kept");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(d.join("src 2/run.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o700, 0o700, "the owner may still run it");
    }
}

#[test]
fn several_items_make_archive_zip_and_names_are_never_reused() {
    let d = tmp("names");
    fs::write(d.join("a.txt"), "a").unwrap();
    fs::write(d.join("b.txt"), "b").unwrap();
    let both = [d.join("a.txt"), d.join("b.txt")];
    assert_eq!(compress(&both).results, [d.join("Archive.zip")]);
    let o = compress(&both);
    assert_eq!(o.results, [d.join("Archive 2.zip")]);
    assert_eq!(o.said.as_deref(), Some("Compressed 2 items into “Archive 2.zip”"));
    fs::write(d.join("a.txt.zip"), "theirs").unwrap();
    assert_eq!(compress(&[d.join("a.txt")]).results, [d.join("a.txt 2.zip")]);
    assert_eq!(fs::read_to_string(d.join("a.txt.zip")).unwrap(), "theirs", "never overwritten");
    // Two loose files extract into a folder named after the archive, beside any taken name.
    fs::create_dir(d.join("Archive")).unwrap();
    let o = extract(&d.join("Archive.zip"));
    ok(&o);
    assert_eq!(o.results, [d.join("Archive 2")]);
    assert_eq!(names(&d.join("Archive 2")), ["a.txt", "b.txt"]);
    assert!(names(&d.join("Archive")).is_empty(), "the taken folder is untouched");
}

#[test]
fn compress_is_undone_by_trashing_the_archive_only_while_unchanged() {
    let d = tmp("undo-c");
    fs::write(d.join("a.txt"), "a").unwrap();
    let o = compress(&[d.join("a.txt")]);
    let u = o.undo.unwrap();
    let back = run(Op::Undo(u));
    ok(&back);
    assert_eq!(names(&d), ["a.txt"]);
    let redo = back.inverse.expect("redo record");
    let again = run(Op::Undo(redo));
    ok(&again);
    assert_eq!(names(&d), ["a.txt", "a.txt.zip"]);
    // Changed since: refused, and nothing changes.
    let u = again.inverse.unwrap();
    let mut f = fs::OpenOptions::new().append(true).open(d.join("a.txt.zip")).unwrap();
    f.write_all(b"x").unwrap();
    let o = run(Op::Undo(u));
    assert!(o.refused.as_deref().is_some_and(|r| r.contains("Can't undo the compression") && r.contains("edited")), "{:?}", o.refused);
    assert_eq!(names(&d), ["a.txt", "a.txt.zip"]);
}

#[test]
fn extract_is_undone_by_trashing_the_folder_only_while_unchanged() {
    let d = tmp("undo-x");
    let src = sample(&d);
    compress(std::slice::from_ref(&src));
    fs::rename(&src, d.join("original")).unwrap();
    let o = extract(&d.join("src.zip"));
    ok(&o);
    let back = run(Op::Undo(o.undo.unwrap()));
    ok(&back);
    assert_eq!(names(&d), ["original", "src.zip"]);
    let again = run(Op::Undo(back.inverse.unwrap()));
    ok(&again);
    assert_eq!(tree(&d.join("src")), tree(&d.join("original")));
    fs::write(d.join("src/sub/new.txt"), "mine").unwrap();
    let o = run(Op::Undo(again.inverse.unwrap()));
    assert!(o.refused.as_deref().is_some_and(|r| r.contains("Can't undo the extraction")), "{:?}", o.refused);
    assert!(d.join("src/sub/new.txt").exists());
}

/// A ZIP at `path` made by Arbor's writer with whatever names it's given.
fn zip_at(path: &Path, add: impl FnOnce(&mut crate::archive::zip::Writer)) {
    let mut w = crate::archive::zip::Writer::new(fs::File::create_new(path).unwrap(), 6);
    add(&mut w);
    w.finish().unwrap();
}

fn add_file(w: &mut crate::archive::zip::Writer, scratch: &Path, name: &str, data: &[u8]) {
    let p = scratch.join(format!("src-{}", crc32fast::hash(name.as_bytes())));
    fs::write(&p, data).unwrap();
    let mut f = fs::File::open(&p).unwrap();
    w.file(name.as_bytes(), true, &mut f, data.len() as u64, None, 0o644, true, &mut |_| true).unwrap();
}

#[test]
fn zip_slip_is_refused_and_nothing_is_written_anywhere() {
    let d = tmp("slip");
    let scratch = d.join("scratch");
    fs::create_dir(&scratch).unwrap();
    let abs = std::env::temp_dir().join(format!("arbor-slip-abs-{}", std::process::id()));
    let abs_name = abs.to_string_lossy().replace('\\', "/");
    type Add<'a> = Box<dyn Fn(&mut crate::archive::zip::Writer) + 'a>;
    let cases: Vec<(&str, Add)> = vec![
        ("dotdot", Box::new(|w| add_file(w, &scratch, "../escaped.txt", b"x"))),
        ("deep", Box::new(|w| add_file(w, &scratch, "a/b/../../../escaped.txt", b"x"))),
        ("backslash", Box::new(|w| add_file(w, &scratch, "a\\..\\..\\escaped.txt", b"x"))),
        ("absolute", Box::new(|w| add_file(w, &scratch, &abs_name, b"x"))),
        ("drive", Box::new(|w| add_file(w, &scratch, "C:\\escaped.txt", b"x"))),
        ("unc", Box::new(|w| add_file(w, &scratch, "\\\\server\\share\\escaped.txt", b"x"))),
        ("nul", Box::new(|w| add_file(w, &scratch, "a\0b", b"x"))),
    ];
    for (name, add) in cases {
        // Not "nul" itself: a device on Windows.
        let case = d.join(format!("case-{name}"));
        fs::create_dir(&case).unwrap();
        let zip = case.join("evil.zip");
        zip_at(&zip, |w| {
            add_file(w, &scratch, "fine.txt", b"ok");
            add(w);
        });
        let o = extract(&zip);
        assert!(o.errors.first().is_some_and(|e| e.starts_with("Couldn't extract “evil.zip”")), "{name}: {:?}", o.errors);
        assert!(o.results.is_empty() && o.undo.is_none(), "{name}");
        assert_eq!(names(&case), ["evil.zip"], "{name}: nothing was made beside it");
    }
    assert!(!d.join("escaped.txt").exists() && !abs.exists() && !std::env::temp_dir().join("escaped.txt").exists());
}

/// Links that could lead out are left out and named; everything else is extracted.
#[cfg(unix)]
#[test]
fn links_leading_out_are_left_out_and_the_rest_extracted() {
    let d = tmp("bad-links");
    let zip = d.join("mixed.zip");
    zip_at(&zip, |w| {
        add_file(w, &d, "fine.txt", b"ok");
        w.link(b"out", true, b"../..", None).unwrap();
        w.link(b"abs", true, b"/etc", None).unwrap();
        w.link(b"x/y", true, b"..", None).unwrap();
        w.link(b"z", true, b"x/y/../..", None).unwrap();
        w.link(b"good", true, b"fine.txt", None).unwrap();
    });
    let o = extract(&zip);
    ok(&o);
    let r = d.join("mixed");
    assert_eq!(names(&r), ["fine.txt", "good", "x"]);
    assert_eq!(fs::read_link(r.join("x/y")).unwrap(), Path::new(".."));
    let note = o.notes.iter().find(|n| n.starts_with("Left out")).unwrap_or_else(|| panic!("{:?}", o.notes));
    assert!(note.contains("“out” → “../..”") && note.contains("“abs” → “/etc”") && note.contains("“z”"), "{note}");
}

/// A link the archive makes is made last, so no entry is ever written through it, even one
/// listed after it whose path goes through its name.
#[cfg(unix)]
#[test]
fn nothing_is_written_through_a_link_the_archive_makes() {
    let d = tmp("through");
    let zip = d.join("t.zip");
    zip_at(&zip, |w| {
        w.dir(b"real/", true, None, 0o755).unwrap();
        w.link(b"l", true, b"real", None).unwrap();
        add_file(w, &d, "l/x.txt", b"through?");
        add_file(w, &d, "f", b"f");
    });
    let o = extract(&zip);
    ok(&o);
    let root = d.join("t");
    assert!(names(&root.join("real")).is_empty(), "nothing went into the link's target");
    assert_eq!(fs::read(root.join("l/x.txt")).unwrap(), b"through?", "a real folder took the name");
    assert_eq!(fs::read_link(root.join("l 2")).unwrap(), Path::new("real"), "the link got a free name");
    assert!(o.notes.iter().any(|n| n.contains("free ones")), "{:?}", o.notes);
}

/// The review's escape, end to end: nothing is made, and nothing four folders up.
#[cfg(unix)]
#[test]
fn a_link_through_a_renamed_link_never_leads_outside() {
    let base = tmp("renamed-link");
    let d = base.join("1/2/3/4");
    fs::create_dir_all(&d).unwrap();
    let zip = d.join("evil.zip");
    zip_at(&zip, |w| {
        w.dir(b"a/b/c/Y/", true, None, 0o755).unwrap();
        w.link(b"a/b/c/Y", true, b"../../..", None).unwrap();
        w.link(b"X", true, b"a/b/c/Y 2/../../../../in1", None).unwrap();
        add_file(w, &base, "f", b"f");
    });
    let o = extract(&zip);
    ok(&o);
    let r = d.join("evil");
    assert!(r.join("a/b/c/Y 2").symlink_metadata().is_ok() && r.join("X").symlink_metadata().is_err(), "X left out");
    assert!(o.notes.iter().any(|n| n.contains("“X” → ")), "{:?}", o.notes);
    assert!(!base.join("in1").exists() && !base.join("1/in1").exists());
}

/// A link another app put in the destination while it was extracting is never gone through by
/// a link the archive makes.
#[cfg(unix)]
#[test]
fn a_link_through_a_foreign_link_is_refused() {
    let d = tmp("foreign-link");
    let zip = d.join("pack.zip");
    zip_at(&zip, |w| {
        add_file(w, &d, "f", b"f");
        w.link(b"l", true, b"planted/x", None).unwrap();
    });
    let hooks = Hooks {
        after_made: Some(Box::new(|p: &Path| {
            if p.ends_with("pack") {
                std::os::unix::fs::symlink("/", p.join("planted")).unwrap();
            }
        })),
        ..Hooks::default()
    };
    let o = hooked(Op::Extract { archives: vec![zip] }, hooks);
    assert!(o.errors.first().is_some_and(|e| e.contains("a link this archive didn't make")), "{:?}", o.errors);
    assert!(!d.join("pack/l").exists());
}

#[cfg(unix)]
#[test]
fn only_the_owners_permission_bits_are_kept() {
    use std::os::unix::fs::PermissionsExt;
    let d = tmp("perm");
    let zip = d.join("p.zip");
    let src = d.join("s");
    fs::write(&src, "x").unwrap();
    zip_at(&zip, |w| {
        let mut f = fs::File::open(&src).unwrap();
        w.file(b"suid", true, &mut f, 1, None, 0o6777, true, &mut |_| true).unwrap();
        let mut f = fs::File::open(&src).unwrap();
        w.file(b"ro", true, &mut f, 1, None, 0o444, true, &mut |_| true).unwrap();
        let mut f = fs::File::open(&src).unwrap();
        w.file(b"plain", true, &mut f, 1, None, 0o600, true, &mut |_| true).unwrap();
    });
    ok(&extract(&zip));
    let mode = |n: &str| fs::metadata(d.join("p").join(n)).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode("suid") & 0o7000, 0, "no setuid, setgid or sticky");
    assert_eq!(mode("suid") & 0o700, 0o700);
    assert_eq!(mode("ro") & 0o700, 0o400, "read-only stays read-only");
    assert_eq!(mode("plain") & 0o700, 0o600);
}

#[test]
fn bombs_are_refused_before_anything_is_written() {
    let d = tmp("bomb");
    let mut r = Raw::new();
    for i in 0..300 {
        r.entry(&format!("z{i}"), 0, 8, 0, 2, u32::MAX - 1, &[3, 0], None);
    }
    fs::write(d.join("ratio.zip"), r.bytes()).unwrap();
    let o = extract(&d.join("ratio.zip"));
    assert!(o.errors.first().is_some_and(|e| e.contains("too large to extract")), "{:?}", o.errors);
    let mut r = Raw::new();
    r.stored("a", &[0; 100]);
    r.entry("b", 0, 0, crc32fast::hash(&[0; 100]), 100, 100, &[], Some(0));
    fs::write(d.join("overlap.zip"), r.bytes()).unwrap();
    let o = extract(&d.join("overlap.zip"));
    assert!(o.errors.first().is_some_and(|e| e.contains("overlap")), "{:?}", o.errors);
    assert_eq!(names(&d), ["overlap.zip", "ratio.zip"]);
}

/// An archive of something very repetitive (here 260 MB of zeros, a thousand times its size)
/// extracts: there's no ratio rule, only the cap, the room on the disk, and each entry held to
/// its declared size.
#[test]
fn a_very_compressible_archive_extracts() {
    let d = tmp("zeros");
    let f = fs::File::create_new(d.join("zeros.img")).unwrap();
    f.set_len(260 << 20).unwrap();
    drop(f);
    ok(&compress(&[d.join("zeros.img")]));
    fs::remove_file(d.join("zeros.img")).unwrap();
    let ratio = (260u64 << 20) / fs::metadata(d.join("zeros.img.zip")).unwrap().len();
    assert!(ratio > 500, "{ratio}");
    let o = extract(&d.join("zeros.img.zip"));
    ok(&o);
    assert_eq!(fs::metadata(d.join("zeros.img/zeros.img")).unwrap().len(), 260 << 20);
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn a_damaged_archive_leaves_nothing_behind() {
    let d = tmp("damaged");
    let mut r = Raw::new();
    r.stored("one.txt", b"first file");
    r.stored("two.txt", b"second file");
    r.stored("three.txt", b"third file");
    let mut bytes = r.bytes();
    let at = bytes.windows(10).position(|w| w == b"third file").unwrap();
    bytes[at] = b'T';
    fs::write(d.join("crc.zip"), &bytes).unwrap();
    let o = extract(&d.join("crc.zip"));
    assert!(o.errors.first().is_some_and(|e| e.contains("damaged") && e.contains("CRC")), "{:?}", o.errors);
    assert_eq!(names(&d), ["crc.zip"], "the two good files went again");
    let good = r.bytes();
    fs::write(d.join("cut.zip"), &good[..good.len() - 30]).unwrap();
    let o = extract(&d.join("cut.zip"));
    assert!(o.errors.first().is_some_and(|e| e.contains("damaged")), "{:?}", o.errors);
    // A gzipped tar cut short: its list can't be read to the end.
    let mut t = Vec::new();
    tar_entry(&mut t, "a.txt", b'0', &noise(100_000), "");
    t.extend([0; 1024]);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
    gz.write_all(&t).unwrap();
    let gz = gz.finish().unwrap();
    fs::write(d.join("cut.tar.gz"), &gz[..gz.len() / 2]).unwrap();
    let o = extract(&d.join("cut.tar.gz"));
    assert!(!o.errors.is_empty(), "{:?}", o.said);
    assert_eq!(names(&d), ["crc.zip", "cut.tar.gz", "cut.zip"]);
}

/// A gzipped tar whose gzip CRC (in its last 8 bytes, after the tar's end) doesn't match is
/// damaged: nothing is extracted.
#[test]
fn a_tar_gz_with_a_bad_crc_is_damaged() {
    let d = tmp("gz-crc");
    let mut t = Vec::new();
    tar_entry(&mut t, "a.txt", b'0', b"hello", "");
    t.extend([0; 1024]);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
    gz.write_all(&t).unwrap();
    let mut gz = gz.finish().unwrap();
    let n = gz.len();
    gz[n - 8] ^= 0xFF;
    fs::write(d.join("bad.tgz"), &gz).unwrap();
    let o = extract(&d.join("bad.tgz"));
    assert!(o.errors.first().is_some_and(|e| e.contains("damaged")), "{:?} {:?}", o.errors, o.said);
    assert_eq!(names(&d), ["bad.tgz"]);
    // Its length field too.
    gz[n - 8] ^= 0xFF;
    gz[n - 1] ^= 0x01;
    fs::write(d.join("len.tgz"), &gz).unwrap();
    let o = extract(&d.join("len.tgz"));
    assert!(o.errors.first().is_some_and(|e| e.contains("damaged")), "{:?}", o.errors);
    assert_eq!(names(&d), ["bad.tgz", "len.tgz"]);
}

/// Gzipped tar bytes of `t`, in one member or split into two.
fn gz_of(t: &[u8], split: Option<usize>) -> Vec<u8> {
    let one = |b: &[u8]| {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
        gz.write_all(b).unwrap();
        gz.finish().unwrap()
    };
    match split {
        None => one(t),
        Some(at) => [one(&t[..at]), one(&t[at..])].concat(),
    }
}

/// Zero bytes after the gzip stream are padding (gzip -t, tar -tzf and Python accept them);
/// a tar split across gzip members reads as one; data after zero padding is damage.
#[test]
fn a_tar_gz_with_trailing_zeros_or_several_members_extracts() {
    let d = tmp("gz-zeros");
    let mut t = Vec::new();
    tar_entry(&mut t, "z/a.txt", b'0', b"hello", "");
    tar_entry(&mut t, "z/b.txt", b'0', &noise(50_000), "");
    t.extend([0; 1024]);
    let mut padded = gz_of(&t, None);
    padded.extend([0; 4096]);
    fs::write(d.join("padded.tgz"), &padded).unwrap();
    let o = extract(&d.join("padded.tgz"));
    ok(&o);
    assert_eq!(fs::read(d.join("z/a.txt")).unwrap(), b"hello");
    fs::write(d.join("two.tgz"), gz_of(&t, Some(700))).unwrap();
    let o = extract(&d.join("two.tgz"));
    ok(&o);
    assert_eq!(fs::read(d.join("z 2/b.txt")).unwrap(), noise(50_000));
    let mut junk = gz_of(&t, Some(700));
    let first = gz_of(&t[..700], None).len();
    junk.splice(first..first, [0u8; 16]);
    fs::write(d.join("junk.tgz"), &junk).unwrap();
    let o = extract(&d.join("junk.tgz"));
    assert!(o.errors.first().is_some_and(|e| e.contains("damaged")), "{:?}", o.errors);
    assert!(!d.join("z 3").exists());
}

/// What follows the gzip member holding the tar's end isn't decompressed: here a second member
/// that's damaged (it was refused as damaged when everything after was read), and a big one
/// (its bytes aren't read, past a buffer).
#[test]
fn nothing_after_the_tars_gzip_member_is_read() {
    use crate::archive::tar::{Counted, Gz, Reader, list_from};
    let d = tmp("gz-after");
    let mut t = Vec::new();
    tar_entry(&mut t, "after/a.txt", b'0', b"hello", "");
    t.extend([0; 1024]);
    let first = gz_of(&t, None);
    let mut bad = gz_of(b"anything after the tar", None);
    let n = bad.len();
    bad[n - 8] ^= 0xFF; // its CRC
    fs::write(d.join("extra.tgz"), [first.clone(), bad].concat()).unwrap();
    let o = extract(&d.join("extra.tgz"));
    ok(&o);
    assert_eq!(fs::read(d.join("after/a.txt")).unwrap(), b"hello");
    // The first member is still checked whole.
    let mut broken = first.clone();
    let m = broken.len();
    broken[m - 8] ^= 0xFF;
    fs::write(d.join("broken.tgz"), [broken, gz_of(b"x", None)].concat()).unwrap();
    assert!(extract(&d.join("broken.tgz")).errors.first().is_some_and(|e| e.contains("damaged")));
    // Bytes read: the first member and a buffer, not the 4 MB member after it.
    let all = [first.clone(), gz_of(&noise(4 << 20), None)].concat();
    let count = std::sync::Arc::new(AtomicU64::new(0));
    let gz = Gz::new(std::io::BufReader::with_capacity(8 * 1024, Counted { r: &all[..], n: count.clone() }));
    let last = gz.last();
    let mut rd = Reader::new(gz);
    assert_eq!(list_from(&mut rd, &|| false, None).unwrap().len(), 1);
    last.set(true);
    rd.drain(&|| false).unwrap();
    let read = count.load(Relaxed);
    assert!(read <= (first.len() + 16 * 1024) as u64, "read {read} of {}", all.len());
}

/// A gzipped tar too large for the disk is refused at the entry that makes it so, while it's
/// listed, not after the whole stream is decompressed: here the stream ends right after that
/// entry's header, which listing on would call damage.
#[test]
fn a_tar_gz_too_large_for_the_disk_is_refused_while_listing() {
    let d = tmp("gz-room");
    let mut t = Vec::new();
    tar_entry(&mut t, "big/a.txt", b'0', b"hello", "");
    t.extend(crate::archive::tests::tar_header("big/huge.img", b'0', 3 << 30, ""));
    t.extend(noise(4096));
    fs::write(d.join("big.tgz"), gz_of(&t, None)).unwrap();
    crate::archive::tests::PRETEND_FREE.with(|f| f.set(Some(2 << 30)));
    let o = extract(&d.join("big.tgz"));
    crate::archive::tests::PRETEND_FREE.with(|f| f.set(None));
    assert!(o.errors.first().is_some_and(|e| e.contains("too large to extract") && e.contains("free")), "{:?}", o.errors);
    assert_eq!(names(&d), ["big.tgz"]);
}

#[test]
fn cancelling_removes_what_was_made() {
    let d = tmp("cancel");
    let zip = d.join("c.zip");
    zip_at(&zip, |w| {
        for i in 0..5 {
            add_file(w, &d, &format!("dir/f{i}"), &noise(10_000));
        }
    });
    for f in fs::read_dir(&d).unwrap() {
        let p = f.unwrap().path();
        if p.file_name().unwrap().to_string_lossy().starts_with("src-") {
            fs::remove_file(p).unwrap();
        }
    }
    let hooks = Hooks { cancel_at: Some(Box::new(|p: &Path| p.ends_with("f2"))), ..Hooks::default() };
    let o = hooked(Op::Extract { archives: vec![zip.clone()] }, hooks);
    assert!(o.cancelled);
    assert_eq!(o.said.as_deref(), Some("Cancelled: nothing was extracted"));
    assert!(o.errors.is_empty() && o.undo.is_none(), "{:?}", o.errors);
    assert_eq!(names(&d), ["c.zip"]);
    // Compress too: no archive, no temporary file.
    let src = sample(&d);
    let hooks = Hooks { cancel_at: Some(Box::new(|p: &Path| p.ends_with("b.bin"))), ..Hooks::default() };
    let o = hooked(Op::Compress { sources: vec![src], dir: d.clone() }, hooks);
    assert!(o.cancelled);
    assert_eq!(o.said.as_deref(), Some("Cancelled: “src.zip” wasn't made"));
    assert_eq!(names(&d), ["c.zip", "src"]);
}

#[test]
fn a_folder_replaced_while_extracting_is_refused_and_left_alone() {
    let d = tmp("replaced");
    let zip = d.join("pack.zip");
    zip_at(&zip, |w| {
        add_file(w, &d, "a", b"a");
        add_file(w, &d, "b", b"b");
    });
    let dd = d.clone();
    let hooks = Hooks {
        after_made: Some(Box::new(move |p: &Path| {
            if p.ends_with("pack") {
                fs::rename(p, dd.join("pack-moved")).unwrap();
                fs::create_dir(p).unwrap();
                fs::write(p.join("theirs"), "theirs").unwrap();
            }
        })),
        ..Hooks::default()
    };
    let o = hooked(Op::Extract { archives: vec![zip] }, hooks);
    assert!(o.errors.first().is_some_and(|e| e.contains("replaced")), "{:?}", o.errors);
    assert_eq!(fs::read_to_string(d.join("pack/theirs")).unwrap(), "theirs", "the other folder is untouched");
    assert!(d.join("pack-moved").is_dir());
}

#[test]
fn an_archive_replaced_since_it_was_listed_is_left_alone() {
    let d = tmp("checked");
    let zip = d.join("a.zip");
    zip_at(&zip, |w| add_file(w, &d, "a", b"a"));
    let id = crate::fs::ident_at(&zip).unwrap();
    fs::remove_file(&zip).unwrap();
    zip_at(&zip, |w| add_file(w, &d, "b", b"b"));
    let o = run(Op::Checked {
        expect: Checks::new(Chosen::Listing, vec![(zip, Some(id))]),
        op: Box::new(Op::Extract { archives: vec![d.join("a.zip")] }),
    });
    assert!(o.refused.as_deref().is_some_and(|r| r.contains("replaced since the folder was read")), "{:?}", o.refused);
    assert!(!d.join("a").exists());
}

#[test]
fn tar_and_gzipped_tar_extract() {
    let d = tmp("tar");
    let mut t = Vec::new();
    tar_entry(&mut t, "proj/", b'5', b"", "");
    tar_entry(&mut t, "proj/src/main.rs", b'0', b"fn main() {}", "");
    tar_entry(&mut t, "proj/big", b'0', &noise(300_000), "");
    tar_entry(&mut t, "proj/hard", b'1', b"", "proj/big");
    tar_entry(&mut t, "proj/README", b'2', b"", "src/main.rs");
    t.extend([0; 1024]);
    fs::write(d.join("proj.tar"), &t).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
    gz.write_all(&t).unwrap();
    fs::write(d.join("proj.tgz"), gz.finish().unwrap()).unwrap();
    for (archive, root) in [("proj.tar", "proj"), ("proj.tgz", "proj 2")] {
        let o = extract(&d.join(archive));
        ok(&o);
        assert_eq!(o.results, [d.join(root)]);
        let r = d.join(root);
        assert_eq!(fs::read(r.join("src/main.rs")).unwrap(), b"fn main() {}");
        assert_eq!(fs::read(r.join("big")).unwrap(), noise(300_000));
        assert!(!r.join("hard").exists());
        assert!(o.notes.iter().any(|n| n.contains("hard links aren't extracted")), "{:?}", o.notes);
        #[cfg(unix)]
        assert_eq!(fs::read_link(r.join("README")).unwrap(), Path::new("src/main.rs"));
    }
}

/// An empty archive (a 0-byte tar, a ZIP with no entries) says so and makes nothing.
#[test]
fn an_empty_archive_makes_nothing() {
    let d = tmp("empty");
    fs::write(d.join("none.tar"), b"").unwrap();
    fs::write(d.join("none.zip"), Raw::new().bytes()).unwrap();
    for a in ["none.tar", "none.zip"] {
        let o = extract(&d.join(a));
        assert!(o.errors.first().is_some_and(|e| e.contains("it's empty")), "{a}: {:?}", o.errors);
    }
    assert_eq!(names(&d), ["none.tar", "none.zip"]);
}

#[test]
fn names_twice_in_an_archive_get_free_names() {
    let d = tmp("dups");
    let zip = d.join("d.zip");
    zip_at(&zip, |w| {
        add_file(w, &d, "x/a.txt", b"one");
        add_file(w, &d, "x/a.txt", b"two");
    });
    let o = extract(&zip);
    ok(&o);
    assert_eq!(fs::read(d.join("x/a.txt")).unwrap(), b"one");
    assert_eq!(fs::read(d.join("x/a 2.txt")).unwrap(), b"two");
}

/// Python's interpreter, isolated (`-I`), if there is one.
fn python() -> Option<&'static str> {
    ["python3", "python"]
        .into_iter()
        .find(|p| std::process::Command::new(p).args(["-I", "-c", "import zipfile, tarfile"]).output().is_ok_and(|o| o.status.success()))
}

#[test]
fn other_tools_read_arbors_zips() {
    let d = tmp("interop-out");
    let src = sample(&d);
    let o = compress(std::slice::from_ref(&src));
    ok(&o);
    let zip = d.join("src.zip");
    if let Some(py) = python() {
        // UTF-8 bytes, whatever the console's code page (Windows' is cp1252; `-I` ignores
        // PYTHONIOENCODING).
        let script = "import sys, zipfile\nz = zipfile.ZipFile(sys.argv[1])\nassert z.testzip() is None\nsys.stdout.buffer.write('\\n'.join(sorted(z.namelist())).encode())";
        let out = std::process::Command::new(py).arg("-I").arg("-c").arg(script).arg(&zip).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let listed = String::from_utf8(out.stdout).unwrap();
        assert!(listed.contains("src/sub/deeper/ünïcödé ✓.txt") && listed.contains("src/empty/"), "{listed}");
    } else {
        eprintln!("no Python: skipped reading Arbor's ZIP with zipfile");
    }
    match std::process::Command::new("unzip").arg("-tq").arg(&zip).output() {
        Ok(out) => {
            assert!(out.status.success(), "unzip -t: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
        }
        Err(_) => eprintln!("no unzip: skipped unzip -t"),
    }
}

#[test]
fn arbor_reads_other_tools_archives() {
    let Some(py) = python() else {
        eprintln!("no Python: skipped extracting zipfile's and tarfile's archives");
        return;
    };
    let d = tmp("interop-in");
    let script = r#"
import sys, os, zipfile, tarfile, io, time
d = sys.argv[1]
with zipfile.ZipFile(os.path.join(d, "py.zip"), "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("py/", "")
    z.writestr("py/text.txt", "hello " * 1000)
    z.writestr(zipfile.ZipInfo("py/stored.bin", date_time=(2020, 5, 17, 12, 30, 4)), bytes(range(256)) * 10, compress_type=zipfile.ZIP_STORED)
    z.writestr("py/ünïcödé ✓/inner.txt", "inner")
    with z.open("py/streamed.txt", "w") as f:
        f.write(b"data descriptor " * 100)
with tarfile.open(os.path.join(d, "py.tar.gz"), "w:gz", format=tarfile.PAX_FORMAT) as t:
    def add(name, data, mtime=1500000000):
        i = tarfile.TarInfo(name); i.size = len(data); i.mtime = mtime
        t.addfile(i, io.BytesIO(data))
    add("pt/a.txt", b"a")
    add("pt/" + "long" * 40 + "/name.txt", b"long")
    l = tarfile.TarInfo("pt/link"); l.type = tarfile.SYMTYPE; l.linkname = "a.txt"; t.addfile(l)
"#;
    let out = std::process::Command::new(py).arg("-I").arg("-c").arg(script).arg(&d).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let o = extract(&d.join("py.zip"));
    ok(&o);
    let r = d.join("py");
    assert_eq!(fs::read_to_string(r.join("text.txt")).unwrap(), "hello ".repeat(1000));
    assert_eq!(fs::read(r.join("stored.bin")).unwrap(), (0..=255u8).cycle().take(2560).collect::<Vec<_>>());
    assert_eq!(fs::read_to_string(r.join("ünïcödé ✓/inner.txt")).unwrap(), "inner");
    assert_eq!(fs::read_to_string(r.join("streamed.txt")).unwrap(), "data descriptor ".repeat(100));
    // The DOS time, read as local time, as zipfile wrote it.
    let local = crate::archive::from_dos((40 << 9) | (5 << 5) | 17, (12 << 11) | (30 << 5) | 2).unwrap();
    assert_eq!(fs::metadata(r.join("stored.bin")).unwrap().modified().unwrap(), local);
    // macOS's ditto, as Finder's Compress makes them, where there is one (a command, no window).
    if std::process::Command::new("ditto").arg("-h").output().is_ok() {
        let src = sample(&d.join("for-ditto"));
        let zip = d.join("ditto.zip");
        let out =
            std::process::Command::new("ditto").args(["-c", "-k", "--sequesterRsrc", "--keepParent"]).arg(&src).arg(&zip).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let o = extract(&zip);
        ok(&o);
        assert_eq!(tree(&d.join("src")), tree(&src), "ditto's archive extracts to the same tree, without __MACOSX");
    }
    let o = extract(&d.join("py.tar.gz"));
    ok(&o);
    let r = d.join("pt");
    assert_eq!(fs::read(r.join("a.txt")).unwrap(), b"a");
    assert_eq!(fs::read(r.join("long".repeat(40)).join("name.txt")).unwrap(), b"long");
    assert_eq!(fs::metadata(r.join("a.txt")).unwrap().modified().unwrap(), std::time::UNIX_EPOCH + Duration::from_secs(1_500_000_000));
    #[cfg(unix)]
    assert_eq!(fs::read_link(r.join("link")).unwrap(), Path::new("a.txt"));
}

/// A file over 4 GB (sparse where the disk allows) through Compress and Extract: ZIP64 sizes in
/// its headers. Slow and needs 5 GB free, so not in every run: `ARBOR_BIG_DIR=/some/dir cargo
/// test --release big_file -- --ignored`.
#[test]
#[ignore]
fn a_file_over_4_gb_goes_through_zip64() {
    let base = std::env::var_os("ARBOR_BIG_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let d = base.join(format!("arbor-arc-big-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    let big = d.join("big.img");
    let f = fs::File::create_new(&big).unwrap();
    f.set_len((4 << 30) + 12345).unwrap();
    // Noise in places, so it isn't a decompression bomb by ratio (4 GB of zeros is 1,000 times
    // its ZIP's size, which Extract refuses).
    {
        use std::io::{Seek, SeekFrom};
        let mut f = f;
        let chunk = noise(1 << 20);
        for i in 0..40u64 {
            f.seek(SeekFrom::Start(i * (100 << 20))).unwrap();
            f.write_all(&chunk).unwrap();
        }
    }
    let o = compress(std::slice::from_ref(&big));
    ok(&o);
    fs::rename(&big, d.join("original.img")).unwrap();
    let o = extract(&d.join("big.img.zip"));
    ok(&o);
    let out = d.join("big.img").join("big.img");
    assert_eq!(fs::metadata(&out).unwrap().len(), (4 << 30) + 12345);
    if let Some(py) = python() {
        let script = "import sys, zipfile\nz = zipfile.ZipFile(sys.argv[1])\ni = z.infolist()[0]\nprint(i.file_size, i.compress_size)";
        let out = std::process::Command::new(py).arg("-I").arg("-c").arg(script).arg(d.join("big.img.zip")).output().unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).starts_with(&format!("{} ", (4u64 << 30) + 12345)));
    }
    let _ = fs::remove_dir_all(&d);
}

/// Compress and Extract throughput on a folder of your choice (BENCH.md has this box's
/// numbers for a 1 GB folder of mixed files): `ARBOR_BENCH_TREE=/path/to/folder cargo test
/// --release archive_throughput -- --ignored --nocapture`. The archive and the extracted copy
/// go beside the folder and are removed after.
#[test]
#[ignore]
fn archive_throughput() {
    let Some(src) = std::env::var_os("ARBOR_BENCH_TREE").map(PathBuf::from) else {
        eprintln!("set ARBOR_BENCH_TREE");
        return;
    };
    let dir = src.parent().unwrap().to_path_buf();
    let bytes: u64 = {
        fn sum(p: &Path) -> u64 {
            fs::read_dir(p)
                .unwrap()
                .map(|e| e.unwrap().path())
                .map(|p| if p.is_dir() { sum(&p) } else { fs::metadata(p).unwrap().len() })
                .sum()
        }
        sum(&src)
    };
    let t = Instant::now();
    let cpu0 = thread_cpu();
    let o = run(Op::Compress { sources: vec![src.clone()], dir: dir.clone() });
    let (c, c_cpu) = (t.elapsed(), thread_cpu() - cpu0);
    ok(&o);
    let zip = o.results[0].clone();
    let zlen = fs::metadata(&zip).unwrap().len();
    let moved = dir.join(format!("{}-original", name_of(&src)));
    fs::rename(&src, &moved).unwrap();
    let t = Instant::now();
    let cpu0 = thread_cpu();
    let o = run(Op::Extract { archives: vec![zip.clone()] });
    let (x, x_cpu) = (t.elapsed(), thread_cpu() - cpu0);
    ok(&o);
    let mb = bytes as f64 / 1e6;
    eprintln!(
        "{mb:.0} MB → {:.0} MB zip ({:.0}%): compress {:.2} s ({:.0} MB/s; CPU {:.2} s, {:.0} MB/s), extract {:.2} s ({:.0} MB/s; CPU {:.2} s, {:.0} MB/s)",
        zlen as f64 / 1e6,
        zlen as f64 * 100.0 / bytes as f64,
        c.as_secs_f64(),
        mb / c.as_secs_f64(),
        c_cpu,
        mb / c_cpu,
        x.as_secs_f64(),
        mb / x.as_secs_f64(),
        x_cpu,
        mb / x_cpu
    );
    fs::remove_dir_all(&o.results[0]).unwrap();
    fs::rename(&moved, &src).unwrap();
    fs::remove_file(&zip).unwrap();
}

/// This thread's CPU time so far, in seconds (user and system), where the OS says; the bench
/// runs the operation on this thread, so it's what the operation cost whatever else the machine
/// is doing.
fn thread_cpu() -> f64 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: a zeroed rusage is a valid out-buffer.
        let mut u: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut u) } == 0 {
            let t = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 / 1e6;
            return t(u.ru_utime) + t(u.ru_stime);
        }
    }
    f64::NAN
}

/// What an extraction or a compress makes is registered for the leftovers record the moment
/// it's made (so quitting mid-way names it, `crate::leftovers`), and no longer once it's whole.
#[test]
fn what_they_make_is_registered_for_the_leftovers_record() {
    use std::sync::{Arc, Mutex};
    let d = tmp("leftovers");
    let zip = d.join("pack.zip");
    zip_at(&zip, |w| {
        add_file(w, &d, "dir/f0", b"0");
        add_file(w, &d, "dir/f1", b"1");
        add_file(w, &d, "g", b"g");
    });
    let p: &'static Progress = Box::leak(Box::new(Progress::default()));
    let seen: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    let s = seen.clone();
    let mut w = Worker::new(p, None, crate::ops::tests::fake_trash);
    w.hooks.cancel_at = Some(Box::new(move |at: &Path| {
        if at.ends_with("f1") || at == Path::new("a.txt") {
            *s.lock().unwrap() = p
                .created
                .lock()
                .unwrap()
                .iter()
                .map(|(path, mark)| {
                    assert!(!mark.is_empty());
                    path.clone()
                })
                .collect();
        }
        false
    }));
    let o = w.run(Op::Extract { archives: vec![zip] });
    ok(&o);
    let root = d.join("pack");
    assert_eq!(*seen.lock().unwrap(), [root.clone(), root.join("dir"), root.join("dir/f0"), root.join("dir/f1")]);
    assert!(p.created.lock().unwrap().is_empty(), "whole: not a leftover");
    // Compress: its temporary file, while it's written.
    fs::write(d.join("a.txt"), "a").unwrap();
    let o = w.run(Op::Compress { sources: vec![d.join("a.txt")], dir: d.clone() });
    ok(&o);
    let during = seen.lock().unwrap().clone();
    assert!(during.len() == 1 && during[0].to_string_lossy().ends_with(".part"), "{during:?}");
    assert!(p.created.lock().unwrap().is_empty());
}
