use super::*;

#[cfg(all(unix, not(target_os = "macos")))]
fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("satchel-trash-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[cfg(all(unix, not(target_os = "macos")))]
fn uid() -> u32 {
    // SAFETY: getuid can't fail.
    unsafe { libc::getuid() }
}

/// Permission tricks don't stop root.
#[cfg(all(unix, not(target_os = "macos")))]
fn root() -> bool {
    // SAFETY: geteuid can't fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn chmod(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn freedesktop_trash_and_restore_leave_no_record() {
    let d = tmp("restore");
    let f = d.join("a b.txt");
    std::fs::write(&f, "x").unwrap();
    let at = super::trash_in(&d.join("Trash"), &f, &f).ok().unwrap();
    assert_eq!(at, d.join("Trash/files/a b.txt"));
    let info = std::fs::read_to_string(d.join("Trash/info/a b.txt.trashinfo")).unwrap();
    assert!(info.contains("a%20b.txt"), "{info}");
    std::fs::rename(&at, &f).unwrap();
    super::forget(&at);
    assert!(!d.join("Trash/info/a b.txt.trashinfo").exists());
    let _ = std::fs::remove_dir_all(&d);
}

/// A file in `Trash/files` with no `.trashinfo` (left by another app) isn't replaced.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn freedesktop_trash_never_overwrites_a_stray_file() {
    let d = tmp("stray");
    std::fs::create_dir_all(d.join("Trash/files")).unwrap();
    std::fs::write(d.join("Trash/files/a"), "stray").unwrap();
    std::fs::write(d.join("a"), "mine").unwrap();
    let at = super::trash_in(&d.join("Trash"), &d.join("a"), &d.join("a")).ok().unwrap();
    assert_eq!(at, d.join("Trash/files/a.2"));
    assert_eq!(std::fs::read_to_string(d.join("Trash/files/a")).unwrap(), "stray");
    assert_eq!(std::fs::read_to_string(&at).unwrap(), "mine");
    assert!(!d.join("Trash/info/a.trashinfo").exists() && d.join("Trash/info/a.2.trashinfo").exists());
    let _ = std::fs::remove_dir_all(&d);
}

/// The home trash takes items on its disk.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn the_home_trash_comes_first_on_its_disk() {
    let d = tmp("home");
    std::fs::write(d.join("a"), "x").unwrap();
    let at = trash_with(&d.join("a"), Some(&d.join("home/Trash")), &d, uid()).unwrap().unwrap();
    assert_eq!(at, d.join("home/Trash/files/a"));
    assert!(!d.join(format!(".Trash-{}", uid())).exists(), "no disk trash made");
    let _ = std::fs::remove_dir_all(&d);
}

/// On another disk: `$topdir/.Trash/$uid` when `.Trash` is sticky, with the path written
/// relative to the disk's top folder.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn a_disk_with_a_sticky_shared_trash_uses_it() {
    let d = tmp("sticky");
    std::fs::create_dir_all(d.join("sub")).unwrap();
    std::fs::create_dir(d.join(".Trash")).unwrap();
    chmod(&d.join(".Trash"), 0o1777);
    std::fs::write(d.join("sub/a"), "x").unwrap();
    let top = std::fs::canonicalize(&d).unwrap();
    let at = trash_with(&d.join("sub/a"), None, &top, uid()).unwrap().unwrap();
    assert_eq!(at, top.join(format!(".Trash/{}/files/a", uid())));
    let info = std::fs::read_to_string(top.join(format!(".Trash/{}/info/a.trashinfo", uid()))).unwrap();
    assert!(info.contains("\nPath=sub/a\n"), "{info}");
    let _ = std::fs::remove_dir_all(&d);
}

/// A shared `.Trash` that isn't sticky, or is a link, is never used: `.Trash-$uid` is.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn an_unsafe_shared_trash_is_skipped_for_the_users_own() {
    for link in [false, true] {
        let d = tmp(if link { "link" } else { "unsticky" });
        std::fs::write(d.join("a"), "x").unwrap();
        if link {
            std::fs::create_dir(d.join("elsewhere")).unwrap();
            chmod(&d.join("elsewhere"), 0o1777);
            std::os::unix::fs::symlink(d.join("elsewhere"), d.join(".Trash")).unwrap();
        } else {
            std::fs::create_dir(d.join(".Trash")).unwrap();
            chmod(&d.join(".Trash"), 0o777);
        }
        let at = trash_with(&d.join("a"), None, &d, uid()).unwrap().unwrap();
        assert_eq!(at, d.join(format!(".Trash-{}/files/a", uid())), "link {link}");
        assert!(std::fs::read_dir(d.join(".Trash")).unwrap().next().is_none(), "nothing put in the shared one");
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// A disk where no trash can be made (its top folder can't be written, as on a disk mounted
/// read-only at the top, or owned by someone else) leaves the item and offers a permanent
/// delete, as long as the item's own folder can be changed; otherwise it's an error, since
/// a delete couldn't remove it either. A `.Trash-$uid` that's a link is never followed.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn no_usable_trash_offers_a_permanent_delete_and_never_deletes() {
    if root() {
        eprintln!("skipped: permissions don't stop root");
        return;
    }
    let d = tmp("none");
    std::fs::create_dir_all(d.join("sub")).unwrap();
    std::fs::write(d.join("sub/a"), "x").unwrap();
    chmod(&d, 0o555);
    let r = trash_with(&d.join("sub/a"), None, &d, uid());
    assert!(matches!(&r, Err(TrashError::NoTrash(w)) if w.contains("has no trash Satchel can use")), "{r:?}");
    assert!(d.join("sub/a").exists(), "left where it was");
    // Its folder can't be changed either: an error, no offer.
    chmod(&d.join("sub"), 0o555);
    let r = trash_with(&d.join("sub/a"), None, &d, uid());
    assert!(matches!(&r, Err(TrashError::Failed(w)) if w.contains("can't be changed")), "{r:?}");
    chmod(&d.join("sub"), 0o755);
    chmod(&d, 0o755);
    // A link where .Trash-$uid goes: refused, not followed.
    std::fs::create_dir(d.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink(d.join("elsewhere"), d.join(format!(".Trash-{}", uid()))).unwrap();
    let r = trash_with(&d.join("sub/a"), None, &d, uid());
    assert!(matches!(&r, Err(TrashError::NoTrash(w)) if w.contains("isn't a folder")), "{r:?}");
    assert!(d.join("sub/a").exists() && std::fs::read_dir(d.join("elsewhere")).unwrap().next().is_none());
    let _ = std::fs::remove_dir_all(&d);
}

/// A real second disk (tmpfs at /dev/shm, when the temp folder isn't on it): its top is
/// found by device, and the home trash isn't offered for it.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn the_top_of_another_disk_is_found_by_device() {
    use std::os::unix::fs::MetadataExt;
    let shm = Path::new("/dev/shm");
    let (Ok(s), Ok(t)) = (shm.metadata(), std::env::temp_dir().metadata()) else { return };
    if s.dev() == t.dev() {
        eprintln!("skipped: /dev/shm is on the temp folder's disk");
        return;
    }
    let d = shm.join(format!("satchel-top-{}", std::process::id()));
    std::fs::create_dir_all(d.join("x/y")).unwrap();
    assert_eq!(mount_top(&d.join("x/y"), s.dev()), shm);
    assert_eq!(mount_top(&std::env::temp_dir(), t.dev()).metadata().unwrap().dev(), t.dev());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn mac_failures_offer_a_permanent_delete_only_for_a_disk_with_no_trash() {
    let msg = || "nope".to_string();
    assert_eq!(classify_mac("NSCocoaErrorDomain", 3328, true, msg(), None), TrashError::NoTrash("the disk it's on has no Trash".into()));
    // A write error isn't proof the disk has no Trash: macOS's own reason, and still the
    // question.
    let why = Some("You don't have permission to access it.".to_string());
    assert_eq!(
        classify_mac("NSCocoaErrorDomain", 513, true, msg(), why),
        TrashError::NoTrash("macOS couldn't move it there (You don't have permission to access it)".into())
    );
    assert_eq!(
        classify_mac("NSCocoaErrorDomain", 512, true, msg(), None),
        TrashError::NoTrash("macOS couldn't move it there (nope)".into())
    );
    // Its folder can't be changed: a delete would fail too.
    assert_eq!(classify_mac("NSCocoaErrorDomain", 3328, false, msg(), None), TrashError::Failed(msg()));
    // Not there any more, or another domain: just the error.
    assert_eq!(classify_mac("NSCocoaErrorDomain", 4, true, msg(), None), TrashError::Failed(msg()));
    assert_eq!(classify_mac("NSPOSIXErrorDomain", 3328, true, msg(), None), TrashError::Failed(msg()));
}

/// Only a local disk goes on; network locations, discs and RAM disks are refused up front.
#[test]
fn windows_asks_the_shell_only_about_fixed_local_disks() {
    assert_eq!(precheck(r"C:\", drive::FIXED), None);
    assert_eq!(precheck(r"C:\mnt\data\", drive::FIXED), None);
    assert_eq!(precheck(r"\\?\Volume{1234}\", drive::FIXED), None);
    let no = |root: &str, kind: u32, says: &str| {
        let why = precheck(root, kind).unwrap_or_default();
        assert!(why.contains(says), "{root} {kind}: {why}");
    };
    // A removable drive goes on to the registry's settings (`bin_verdict`).
    assert_eq!(precheck(r"E:\", drive::REMOVABLE), None);
    no(r"Z:\", drive::REMOTE, "drive Z: is a network location");
    no(r"\\nas\photos\", drive::REMOTE, "“\\\\nas\\photos” is a network location");
    no(r"\\nas\photos\", drive::FIXED, "network location");
    no(r"\\?\UNC\nas\photos\", drive::FIXED, "network location");
    no(r"D:\", drive::CDROM, "disc");
    no(r"R:\", drive::RAMDISK, "RAM disk");
    no(r"Q:\", 0, "can't tell");
    no(r"Q:\", 1, "can't tell");
}

/// The shell's answers: recycled only when PostDeleteItem says the item landed in the bin.
#[test]
fn windows_verdicts_never_call_a_delete_a_recycle() {
    let at = "drive C:";
    assert_eq!(verdict(false, 0, Some((0, true)), at), Ok(()));
    // Refused before deleting: offer the confirm.
    assert!(matches!(verdict(true, 0x80004004_u32 as i32, None, at), Err(TrashError::NoTrash(w)) if w.contains("drive C:")));
    assert!(matches!(verdict(true, 0, Some((0, true)), at), Err(TrashError::NoTrash(_))));
    // Deleted, but not into the bin: said plainly.
    assert_eq!(verdict(false, 0, Some((0, false)), at), Err(TrashError::DeletedForGood));
    // Failures, and silence.
    assert!(matches!(verdict(false, 0, Some((0x80070005_u32 as i32, false)), at), Err(TrashError::Failed(w)) if w.contains("0x80070005")));
    assert!(matches!(verdict(false, 0x80070002_u32 as i32, None, at), Err(TrashError::Failed(w)) if w.contains("0x80070002")));
    assert!(matches!(verdict(false, 0, None, at), Err(TrashError::Failed(_))));
}

/// Windows: an item goes to the shell only when the registry says the volume's Recycle Bin
/// keeps files and is big enough for it; anything turned off, too big or unknown is left for
/// Satchel's own question.
#[test]
fn windows_recycles_only_what_the_bin_would_keep() {
    let ok = BinSettings { volume_key: true, nuke: Some(0), max_mb: Some(100), ..Default::default() };
    let mb = 1 << 20;
    assert_eq!(bin_verdict(&ok, Some(100 * mb), "drive C:", false), Ok(()));
    let why = |s: &BinSettings, size: Option<u64>| bin_verdict(s, size, "drive C:", false).unwrap_err();
    assert!(why(&ok, Some(200 * mb)).contains(&format!("bigger (210 MB) than the {BIN} on drive C: can hold (105 MB)")));
    assert!(why(&ok, Some(100 * mb + 1)).contains("bigger"));
    assert!(why(&ok, None).contains("couldn't read all of it"));
    assert!(why(&BinSettings { nuke: Some(1), ..ok.clone() }, Some(1)).contains("remove files immediately"));
    // The volume's own setting wins over the parent key's; without one, the parent's counts.
    assert_eq!(bin_verdict(&BinSettings { global_nuke: Some(1), ..ok.clone() }, Some(1), "drive C:", false), Ok(()));
    assert!(why(&BinSettings { nuke: None, global_nuke: Some(1), ..ok.clone() }, Some(1)).contains("immediately"));
    assert_eq!(bin_verdict(&BinSettings { nuke: None, global_nuke: Some(0), ..ok.clone() }, Some(1), "drive C:", false), Ok(()));
    // Unknown: no key, no say on nuking, no size.
    assert!(why(&BinSettings { nuke: None, ..ok.clone() }, Some(1)).contains("doesn't say whether"));
    assert!(why(&BinSettings { max_mb: None, ..ok.clone() }, Some(1)).contains("doesn't say how big"));
    assert!(why(&BinSettings { volume_key: false, ..ok.clone() }, Some(1)).contains(&format!("no {BIN} settings for drive C:")));
    let removable = bin_verdict(&BinSettings::default(), Some(1), "drive E:", true).unwrap_err();
    assert!(removable.contains("drive E: is a removable drive"), "{removable}");
    assert_eq!(bin_verdict(&ok, Some(1), "drive E:", true), Ok(()), "a removable drive with a bin");
    // Policies: off, or a smaller limit.
    assert!(why(&BinSettings { policy_off: true, ..ok.clone() }, Some(1)).contains("policy"));
    // RecycleBinSize is a percentage of the drive (the review: 10 was read as 10 MB).
    let gb = 1u64 << 30;
    let ten_percent = BinSettings { max_mb: None, policy_percent: Some(10), volume_bytes: Some(500 * gb), ..ok.clone() };
    assert_eq!(bin_verdict(&ten_percent, Some(20 * mb), "drive C:", false), Ok(()), "20 MB fits in 10% of 500 GB");
    assert_eq!(bin_verdict(&ten_percent, Some(50 * gb), "drive C:", false), Ok(()));
    assert!(why(&ten_percent, Some(50 * gb + 1)).contains("bigger"));
    // The smaller of the two limits counts.
    assert!(why(&BinSettings { max_mb: Some(100), ..ten_percent.clone() }, Some(200 * mb)).contains("can hold (105 MB)"));
    assert!(why(&BinSettings { policy_percent: Some(1), volume_bytes: Some(gb), ..ok.clone() }, Some(20 * mb)).contains("bigger"));
    assert!(why(&BinSettings { policy_percent: Some(0), volume_bytes: Some(gb), ..ok.clone() }, Some(1)).contains("bigger"), "0%: no bin");
    // A percentage of a drive whose size is unknown: unknown, so it asks.
    assert!(why(&BinSettings { volume_bytes: None, ..ten_percent }, Some(1)).contains("share of it"));
    assert_eq!(place_of(r"C:\"), "drive C:");
    assert_eq!(place_of(r"\\nas\photos\"), "“\\\\nas\\photos”");
}

/// The size walk adds up files, doesn't follow links, and stops when told.
#[test]
fn the_size_walk_adds_up_files_and_stops_when_told() {
    let d = std::env::temp_dir().join(format!("satchel-trash-size-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("t/sub")).unwrap();
    std::fs::write(d.join("t/a"), [0u8; 1000]).unwrap();
    std::fs::write(d.join("t/sub/b"), [0u8; 24]).unwrap();
    std::fs::create_dir_all(d.join("big")).unwrap();
    std::fs::write(d.join("big/huge"), [0u8; 5000]).unwrap();
    #[cfg(unix)]
    let _ = std::os::unix::fs::symlink(d.join("big"), d.join("t/link"));
    #[cfg(windows)]
    let _ = std::os::windows::fs::symlink_dir(d.join("big"), d.join("t/link"));
    assert_eq!(tree_size(&d.join("t"), &|| false), Size::Known(1024));
    assert_eq!(tree_size(&d.join("t"), &|| true), Size::Stopped);
    assert_eq!(tree_size(&d.join("gone"), &|| false), Size::Unreadable);
    let _ = std::fs::remove_dir_all(&d);
}

/// On CI's Windows runner: with the Recycle Bin for C: limited to 1 MB in HKCU, a 2 MB file
/// comes back as needing Satchel's question and is still there; a small one still goes. The
/// registry is put back afterwards.
#[cfg(windows)]
#[test]
fn windows_a_file_too_big_for_the_bin_is_left_for_the_question() {
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, REG_DWORD, RegCloseKey, RegCreateKeyW, RegDeleteValueW, RegSetValueExW,
    };
    if std::env::var_os("CI").is_none() {
        eprintln!("skipped: only on CI (it changes the Recycle Bin's settings and uses it)");
        return;
    }
    let _one = CI_BIN.lock().unwrap_or_else(|e| e.into_inner());
    let d = std::env::temp_dir().join(format!("satchel-recycle-big-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let abs = std::path::absolute(&d).unwrap();
    let (root, _) = win::volume(&abs);
    let before = win::bin_settings(&root);
    eprintln!("Recycle Bin settings for {root}: {before:?}");
    let key = win::volume_key(&root).expect("C:'s volume name");
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let mut h: HKEY = std::ptr::null_mut();
    // SAFETY: NUL-terminated names and out-handles; values are DWORDs; the key is closed.
    let set = |h: HKEY, name: &str, v: u32| unsafe {
        RegSetValueExW(h, wide(name).as_ptr(), 0, REG_DWORD, (&v as *const u32).cast(), 4);
    };
    // SAFETY: a NUL-terminated name and an out-handle, closed below.
    assert_eq!(unsafe { RegCreateKeyW(HKEY_CURRENT_USER, wide(&key).as_ptr(), &mut h) }, 0);
    set(h, "MaxCapacity", 1);
    set(h, "NukeOnDelete", 0);
    let big = d.join("big.bin");
    std::fs::write(&big, vec![7u8; 2 << 20]).unwrap();
    let r = trash(&big, &|| false);
    let small = d.join("small.txt");
    std::fs::write(&small, "x").unwrap();
    let r_small = trash(&small, &|| false);
    // Put the settings back as they were.
    let restore = |name: &str, v: Option<u64>| match v {
        Some(v) => set(h, name, v as u32),
        // SAFETY: an open key and a NUL-terminated name.
        None => unsafe {
            RegDeleteValueW(h, wide(name).as_ptr());
        },
    };
    restore("MaxCapacity", before.max_mb);
    restore("NukeOnDelete", before.nuke.map(u64::from));
    // SAFETY: opened above.
    unsafe { RegCloseKey(h) };
    assert!(matches!(&r, Err(TrashError::NoTrash(w)) if w.contains("bigger")), "{r:?}");
    assert!(big.exists(), "never deleted");
    assert_eq!(r_small, Ok(None), "a small file still goes to the bin");
    assert!(!small.exists());
    let _ = std::fs::remove_dir_all(&d);
}

/// On CI's Windows runner (a throwaway machine, so its Recycle Bin can take a scratch file):
/// a file on the fixed system drive goes to the bin, and the same kind of file reached
/// through the admin share (`\\localhost\C$\…`, a network path with no bin) is refused and
/// left alone, both by the precheck and by the shell's own answer with the precheck skipped.
#[cfg(windows)]
#[test]
fn windows_recycles_locally_and_refuses_a_network_path() {
    if std::env::var_os("CI").is_none() {
        eprintln!("skipped: only on CI (it uses the real Recycle Bin)");
        return;
    }
    let _one = CI_BIN.lock().unwrap_or_else(|e| e.into_inner());
    let d = std::env::temp_dir().join(format!("satchel-recycle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let local = d.join("local.txt");
    std::fs::write(&local, "x").unwrap();
    assert_eq!(trash(&local, &|| false), Ok(None));
    assert!(!local.exists());
    let f = d.join("remote.txt");
    std::fs::write(&f, "x").unwrap();
    let abs = std::fs::canonicalize(&f).unwrap();
    let s = abs.to_string_lossy();
    let s = s.trim_start_matches(r"\\?\");
    let unc = PathBuf::from(format!(r"\\localhost\{}${}", &s[..1], &s[2..]));
    // CI's runner shares C$; the test must reach it, or it proves nothing about the shell.
    assert!(unc.symlink_metadata().is_ok(), "{} isn't reachable", unc.display());
    let r = trash(&unc, &|| false);
    assert!(matches!(&r, Err(TrashError::NoTrash(w)) if w.contains("network")), "{r:?}");
    // With the precheck skipped, the shell itself is asked, and the sink stops it.
    let root = r"\\localhost\C$\";
    let r = recycle(&unc, root);
    assert!(matches!(&r, Err(TrashError::NoTrash(_))), "the shell's own refusal: {r:?}");
    assert!(f.exists(), "never deleted");
    let _ = std::fs::remove_dir_all(&d);
}

/// The CI tests that use the real Recycle Bin (one changes its settings), one at a time.
#[cfg(windows)]
static CI_BIN: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
#[ignore = "moves a scratch file to the real Trash"]
fn trashes_a_file() {
    let p = std::env::temp_dir().join(format!("satchel-trash-test-{}.txt", std::process::id()));
    std::fs::write(&p, "x").unwrap();
    let t = std::time::Instant::now();
    let r = super::trash(&p, &|| false);
    eprintln!("trash took {:?}: {r:?}", t.elapsed());
    assert!(r.is_ok());
    assert!(!p.exists());
}

/// Any names (bytes, not just UTF-8; names that collide; names that look like records): each
/// trashed item lands in `files/` under a name of its own, never over another, with a record
/// beside it that Put Back reads as exactly where it came from.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn any_name_is_trashed_with_a_record_that_reads_back() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let d = tmp("names");
    let home = d.join("home/Trash");
    crate::prop::check("trash_names", 40, |r| {
        let src = d.join("src");
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(&src).unwrap();
        let mut made = Vec::new();
        for _ in 0..1 + r.below(4) {
            let name: Vec<u8> = match r.below(4) {
                0 => b"same".to_vec(),
                1 => b"x.trashinfo".to_vec(),
                _ => r.bytes(20).into_iter().filter(|&c| c != 0 && c != b'/').collect(),
            };
            if name.is_empty() || name == b"." || name == b".." {
                continue;
            }
            let p = src.join(std::ffi::OsString::from_vec(name));
            if p.exists() {
                continue;
            }
            std::fs::write(&p, p.as_os_str().as_bytes()).unwrap();
            made.push(p);
        }
        for p in &made {
            let at = super::trash_in(&home, p, p).ok().expect("trashed");
            assert!(!p.exists() && at.parent() == Some(home.join("files").as_path()), "{p:?} → {at:?}");
            assert_eq!(std::fs::read(&at).unwrap(), p.as_os_str().as_bytes(), "{at:?} is the item, not another");
            let mut rec = at.file_name().unwrap().to_os_string();
            rec.push(".trashinfo");
            let text = std::fs::read_to_string(home.join("info").join(rec)).unwrap();
            assert_eq!(crate::trash::contents::parse_info(&text, None).as_ref(), Ok(p), "{text:?}");
        }
    });
    let _ = std::fs::remove_dir_all(&d);
}
