//! The Linux package's `install.sh` and `uninstall.sh`, run for real against a throwaway home
//! (with a stand-in for the `arbor` binary), as a user would run them from the tarball.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::handler::{BACKUP_FILE, Backup};

/// A package folder as the tarball unpacks it: the scripts, the desktop file, icons, and an
/// `arbor` that records `--restore-folder-default` (when `with_arbor`).
fn package(root: &Path, with_arbor: bool) -> PathBuf {
    let pkg = root.join("arbor-pkg");
    std::fs::create_dir_all(pkg.join("icons")).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/linux");
    for f in ["install.sh", "uninstall.sh", "arbor.desktop"] {
        std::fs::copy(src.join(f), pkg.join(f)).unwrap();
    }
    for n in [16, 24, 32, 48, 64, 128, 256, 512] {
        std::fs::write(pkg.join(format!("icons/arbor-{n}.png")), "png").unwrap();
    }
    std::fs::write(pkg.join("icons/arbor.svg"), "<svg/>").unwrap();
    if with_arbor {
        let stub = "#!/bin/sh\nif [ \"$1\" = --restore-folder-default ]; then echo restored-by \"$0\" >> \"$HOME/restore.log\"; fi\n";
        std::fs::write(pkg.join("arbor"), stub).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(pkg.join("arbor"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    pkg
}

/// Runs `script` from `pkg` with only `HOME` and a plain `PATH`, as CI does.
fn run(pkg: &Path, script: &str, home: &Path) -> (bool, String) {
    let out = Command::new("sh").arg(pkg.join(script)).env_clear().env("HOME", home).env("PATH", "/usr/bin:/bin").output().unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-pkg-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The review's case: a home with a space and `&` (and a `%`) used to come out as a desktop
/// file that can't launch (the path unquoted, `&` turned into the matched text by sed). Now
/// Exec is quoted and escaped as the Desktop Entry spec says, and validates.
#[test]
fn install_writes_any_path_into_the_desktop_file_correctly() {
    let root = tmp("special");
    let pkg = package(&root, true);
    for name in ["plain", "my home & stuff", "50% off", "back\\slash \"quoted\" $x"] {
        let home = root.join(name);
        std::fs::create_dir_all(&home).unwrap();
        let (ok, out) = run(&pkg, "install.sh", &home);
        assert!(ok, "{name}: {out}");
        let desktop = std::fs::read_to_string(home.join(".local/share/applications/arbor.desktop")).unwrap();
        let exe = home.join(".local/bin/arbor").display().to_string();
        let exec = desktop.lines().find(|l| l.starts_with("Exec=")).unwrap();
        let try_exec = desktop.lines().find(|l| l.starts_with("TryExec=")).unwrap();
        let want = match name {
            "plain" => format!("Exec={exe} %F"),
            "my home & stuff" => format!("Exec=\"{exe}\" %F"),
            "50% off" => format!("Exec=\"{}\" %F", exe.replace('%', "%%")),
            // In the quotes \ " $ are escaped, then every backslash is doubled for the string.
            _ => format!("Exec=\"{}\" %F", exe.replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$").replace('\\', "\\\\")),
        };
        assert_eq!(exec, want, "{name}");
        assert_eq!(try_exec, format!("TryExec={}", exe.replace('\\', "\\\\")), "{name}");
        assert!(desktop.contains("\nMimeType=inode/directory;\n"), "{name}: every other line as it was");
        if Command::new("desktop-file-validate").arg("--help").output().is_ok() {
            let v = Command::new("desktop-file-validate").arg(home.join(".local/share/applications/arbor.desktop")).output().unwrap();
            let said = String::from_utf8_lossy(&v.stdout).to_string() + &String::from_utf8_lossy(&v.stderr);
            assert!(v.status.success() && !said.contains("error"), "{name}: {said}");
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Arbor made the default (activation file, folder default, backup), as Make leaves them.
fn made_default(home: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let service = home.join(".local/share/dbus-1/services/org.freedesktop.FileManager1.service");
    std::fs::create_dir_all(service.parent().unwrap()).unwrap();
    std::fs::write(&service, crate::handler::linux::service_text(&home.join(".local/bin/arbor"))).unwrap();
    let mimeapps = home.join(".config/mimeapps.list");
    std::fs::create_dir_all(mimeapps.parent().unwrap()).unwrap();
    std::fs::write(&mimeapps, "[Added Associations]\ninode/directory=arbor.desktop;\n\n[Default Applications]\ninode/directory=arbor.desktop\ntext/plain=gedit.desktop\n").unwrap();
    let backup = home.join(".config/arbor").join(BACKUP_FILE);
    let mut b = Backup::default();
    b.put("mimeapps.list inode/directory", Some(b"old.desktop;fallback.desktop;"));
    b.put("dbus-1/services/org.freedesktop.FileManager1.service", None);
    b.write(&backup).unwrap();
    (service, mimeapps, backup)
}

/// The review's case: with the installed binary gone, uninstall used to skip Restore and leave
/// Arbor's activation file, so "Show in folder" failed to start anything. Now the Arbor beside
/// the script restores; with none at all, the script takes back what Arbor wrote itself.
#[test]
fn uninstall_never_strands_the_folder_default() {
    let root = tmp("uninstall");
    // The installed binary gone, the package's still there: it restores.
    let pkg = package(&root, true);
    let home = root.join("home1");
    std::fs::create_dir_all(&home).unwrap();
    assert!(run(&pkg, "install.sh", &home).0);
    std::fs::remove_file(home.join(".local/bin/arbor")).unwrap();
    let (ok, out) = run(&pkg, "uninstall.sh", &home);
    assert!(ok, "{out}");
    let log = std::fs::read_to_string(home.join("restore.log")).unwrap_or_default();
    assert!(log.contains(&*pkg.join("arbor").display().to_string()), "the package's Arbor restored: {log:?} {out}");
    // Neither: the script puts it back from the backup.
    let _ = std::fs::remove_dir_all(&root);
    let root = tmp("uninstall-bare");
    let pkg = package(&root, false);
    let home = root.join("home2");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::copy(Path::new("/bin/true"), pkg.join("arbor")).unwrap();
    assert!(run(&pkg, "install.sh", &home).0);
    std::fs::remove_file(pkg.join("arbor")).unwrap();
    std::fs::remove_file(home.join(".local/bin/arbor")).unwrap();
    let (service, mimeapps, backup) = made_default(&home);
    let (ok, out) = run(&pkg, "uninstall.sh", &home);
    assert!(ok, "{out}");
    assert!(!service.exists(), "Arbor's activation file is gone: {out}");
    assert_eq!(
        std::fs::read_to_string(&mimeapps).unwrap(),
        "[Added Associations]\ninode/directory=arbor.desktop;\n\n[Default Applications]\ninode/directory=old.desktop;fallback.desktop;\ntext/plain=gedit.desktop\n",
        "the old default is back, only in its section: {out}"
    );
    assert!(!backup.exists());
    assert!(
        out.contains("Removed Arbor's D-Bus activation file") && out.contains("Folders open in old.desktop;fallback.desktop; again"),
        "{out}"
    );
    // Someone else's activation file stays.
    let home = root.join("home3");
    std::fs::create_dir_all(&home).unwrap();
    let (service, _, _) = made_default(&home);
    std::fs::write(&service, "[D-BUS Service]\nName=org.freedesktop.FileManager1\nExec=/usr/bin/nautilus\n").unwrap();
    assert!(run(&pkg, "uninstall.sh", &home).0);
    assert!(service.exists(), "not Arbor's: left alone");
    let _ = std::fs::remove_dir_all(&root);
}

/// A package without any `arbor`, installed into a fresh home whose binary is gone too: only
/// the script can put things back.
fn bare(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let pkg = package(root, false);
    let home = root.join(name);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::copy(Path::new("/bin/true"), pkg.join("arbor")).unwrap();
    assert!(run(&pkg, "install.sh", &home).0);
    std::fs::remove_file(pkg.join("arbor")).unwrap();
    std::fs::remove_file(home.join(".local/bin/arbor")).unwrap();
    (pkg, home)
}

/// The review's case: with no Arbor to ask, a FileManager1 activation file another file
/// manager had (which the backup saved) used to be deleted instead of put back, and the backup
/// with it; an empty backup was taken for "nothing to restore" and discarded. Now both saved
/// values go back, and a damaged backup stops the script before anything changes, and stays.
#[test]
fn fallback_uninstall_restores_both_values_and_refuses_a_damaged_backup() {
    let root = tmp("uninstall-saved");
    let (pkg, home) = bare(&root, "home1");
    let (service, mimeapps, backup) = made_default(&home);
    let theirs = "[D-BUS Service]\nName=org.freedesktop.FileManager1\nExec=/usr/bin/previous-manager\n";
    let mut b = Backup::default();
    b.put("mimeapps.list inode/directory", Some(b"old.desktop;"));
    b.put("dbus-1/services/org.freedesktop.FileManager1.service", Some(theirs.as_bytes()));
    b.write(&backup).unwrap();
    let (ok, out) = run(&pkg, "uninstall.sh", &home);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read_to_string(&service).unwrap(), theirs, "their activation file is back: {out}");
    assert!(std::fs::read_to_string(&mimeapps).unwrap().contains("\n[Default Applications]\ninode/directory=old.desktop;\n"), "{out}");
    assert!(!backup.exists(), "everything went back, so the backup goes: {out}");
    // An empty activation file was there before (zero bytes, an empty hex field): a valid
    // backup, and it goes back empty (it used to be refused as damaged).
    let (pkg, home) = bare(&root, "home-empty");
    let (service, _, backup) = made_default(&home);
    let mut b = Backup::default();
    b.put("mimeapps.list inode/directory", None);
    b.put("dbus-1/services/org.freedesktop.FileManager1.service", Some(b""));
    b.write(&backup).unwrap();
    let (ok, out) = run(&pkg, "uninstall.sh", &home);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read(&service).unwrap(), b"", "the empty file is back: {out}");
    assert!(!backup.exists(), "{out}");
    // Damaged backups: empty, no header, an entry missing, or a record that isn't whole (hex
    // of half a byte, not hex, a field missing, a name twice).
    let header = std::fs::read_to_string({
        let mut b = Backup::default();
        b.put("x", None);
        let p = root.join("header.txt");
        b.write(&p).unwrap();
        p
    })
    .unwrap();
    let header = header.lines().next().unwrap().to_string();
    let (mime, svc) = ("mimeapps.list inode/directory", "dbus-1/services/org.freedesktop.FileManager1.service");
    let whole = |m: &str, s: &str| format!("{header}\n{m}\t{mime}\n{s}\t{svc}\n");
    for (i, damaged) in [
        "".to_string(),
        "\n".into(),
        format!("{header}\n"),
        format!("{header}\n{}\t{mime}\n", hex(b"old.desktop;")),
        whole("616", "-"),
        whole("zz", "-"),
        format!("{header}\n616263\n-\t{svc}\n-\t{mime}\n"),
        format!("{}-\t{mime}\n", whole("616263", "-")),
        // A folder default that isn't one line of text (it would add lines to mimeapps.list).
        whole(&hex(b"x.desktop\n[Default Applications]\ntext/html=evil.desktop"), "-"),
        whole(&hex(b"caf\xe9"), "-"),
    ]
    .iter()
    .enumerate()
    {
        let (pkg, home) = bare(&root, &format!("damaged{i}"));
        let (service, mimeapps, backup) = made_default(&home);
        std::fs::write(&backup, damaged).unwrap();
        let (svc, mime) = (std::fs::read_to_string(&service).unwrap(), std::fs::read_to_string(&mimeapps).unwrap());
        let (ok, out) = run(&pkg, "uninstall.sh", &home);
        assert!(!ok, "{damaged:?}: refused: {out}");
        assert!(out.contains("damaged") && out.contains("nothing was changed"), "{damaged:?}: {out}");
        assert_eq!(std::fs::read_to_string(&service).unwrap(), svc, "{damaged:?}: activation file as it was");
        assert_eq!(std::fs::read_to_string(&mimeapps).unwrap(), mime, "{damaged:?}: folder default as it was");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), *damaged, "{damaged:?}: the backup stays");
        assert!(home.join(".local/share/applications/arbor.desktop").exists(), "{damaged:?}: nothing removed");
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|c| format!("{c:02x}")).collect()
}
