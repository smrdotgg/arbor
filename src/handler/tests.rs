use super::*;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-handler-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn the_backup_reads_back_what_it_wrote() {
    let mut b = Backup::default();
    b.put("a key with spaces|Icon", Some(b"\x00\x01text\xff"));
    b.put("absent", None);
    b.put("empty", Some(b""));
    b.put("a key with spaces|Icon", Some(b"again"));
    let d = tmp("backup");
    let p = d.join(BACKUP_FILE);
    b.write(&p).unwrap();
    let back = Backup::read(&p).unwrap().unwrap();
    assert_eq!(back, b);
    assert_eq!(back.get("absent"), Some(None));
    assert_eq!(back.get("empty"), Some(Some(&b""[..])), "an empty value is a value, not damage");
    assert_eq!(back.get("missing"), None);
    assert_eq!(back.get("a key with spaces|Icon"), Some(Some(&b"again"[..])));
    assert!(Backup::read(&d.join("none")).unwrap().is_none());
    std::fs::write(&p, "zz\tkey\n").unwrap();
    assert!(Backup::read(&p).is_err(), "a damaged backup is said, not guessed at");
    std::fs::write(&p, format!("{BACKUP_HEADER}\n61\tkey\n-\tkey\n")).unwrap();
    assert!(Backup::read(&p).is_err_and(|e| e.contains("records key again")), "a name twice is damage too");
    let _ = std::fs::remove_dir_all(&d);
}

// ---- Linux ----

fn linux_dirs(d: &Path, installed: bool) -> linux::Dirs {
    let data_home = d.join("data");
    if installed {
        std::fs::create_dir_all(data_home.join("applications")).unwrap();
        std::fs::write(data_home.join("applications/arbor.desktop"), "[Desktop Entry]\n").unwrap();
    }
    linux::Dirs {
        config_home: d.join("config"),
        data_home,
        data_dirs: vec![d.join("system")],
        exe: PathBuf::from("/home/me/.local/bin/arbor"),
        desktops: vec!["gnome".into()],
        reload_bus: false,
    }
}

#[test]
fn mimeapps_edits_keep_every_other_line() {
    let text = "[Added Associations]\ninode/directory=x.desktop;\n\n[Default Applications]\ntext/plain=gedit.desktop\ninode/directory = org.gnome.Nautilus.desktop\n# note\n";
    assert_eq!(linux::default_app(text, "inode/directory").as_deref(), Some("org.gnome.Nautilus.desktop"));
    let set = linux::set_default_app(text, "inode/directory", Some("arbor.desktop"));
    assert_eq!(
        set,
        "[Added Associations]\ninode/directory=x.desktop;\n\n[Default Applications]\ninode/directory=arbor.desktop\ntext/plain=gedit.desktop\n# note\n"
    );
    let removed = linux::set_default_app(&set, "inode/directory", None);
    assert_eq!(removed, "[Added Associations]\ninode/directory=x.desktop;\n\n[Default Applications]\ntext/plain=gedit.desktop\n# note\n");
    assert_eq!(
        linux::set_default_app("", "inode/directory", Some("arbor.desktop")),
        "[Default Applications]\ninode/directory=arbor.desktop\n"
    );
    assert_eq!(
        linux::set_default_app("[Other]\na=b", "inode/directory", Some("arbor.desktop")),
        "[Other]\na=b\n\n[Default Applications]\ninode/directory=arbor.desktop\n"
    );
}

#[test]
fn linux_make_and_restore_put_back_exactly_what_was_there() {
    let d = tmp("linux-roundtrip");
    let dirs = linux_dirs(&d, true);
    let backup = d.join("arbor").join(BACKUP_FILE);
    let mimeapps = dirs.config_home.join("mimeapps.list");
    let service = linux::service_file(&dirs.data_home);
    let before = "[Default Applications]\ninode/directory=org.gnome.Nautilus.desktop\ntext/plain=gedit.desktop\n";
    std::fs::create_dir_all(&dirs.config_home).unwrap();
    std::fs::write(&mimeapps, before).unwrap();
    std::fs::create_dir_all(service.parent().unwrap()).unwrap();
    std::fs::write(&service, "[D-BUS Service]\nName=org.freedesktop.FileManager1\nExec=/usr/bin/other\n").unwrap();
    let msg = linux::run(Change::Make, &dirs, &backup).unwrap();
    assert!(msg.starts_with("Arbor is now the default for folders"), "{msg}");
    assert_eq!(linux::default_app(&std::fs::read_to_string(&mimeapps).unwrap(), linux::MIME).as_deref(), Some("arbor.desktop"));
    let s = std::fs::read_to_string(&service).unwrap();
    assert!(linux::is_ours(&s) && s.contains("Exec=/home/me/.local/bin/arbor\n") && s.contains("Name=org.freedesktop.FileManager1"), "{s}");
    // Again: the first backup stays (it isn't overwritten with Arbor's own settings).
    linux::run(Change::Make, &dirs, &backup).unwrap();
    let msg = linux::run(Change::Restore, &dirs, &backup).unwrap();
    assert_eq!(msg, "Folders open in org.gnome.Nautilus.desktop again");
    assert_eq!(std::fs::read_to_string(&mimeapps).unwrap(), before);
    assert_eq!(std::fs::read_to_string(&service).unwrap(), "[D-BUS Service]\nName=org.freedesktop.FileManager1\nExec=/usr/bin/other\n");
    assert!(!backup.exists(), "the backup goes once it's put back");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn linux_restore_with_nothing_before_removes_what_arbor_wrote() {
    let d = tmp("linux-fresh");
    let dirs = linux_dirs(&d, true);
    let backup = d.join("arbor").join(BACKUP_FILE);
    linux::run(Change::Make, &dirs, &backup).unwrap();
    let msg = linux::run(Change::Restore, &dirs, &backup).unwrap();
    assert_eq!(msg, "Folders open in the desktop's own file manager again");
    let text = std::fs::read_to_string(dirs.config_home.join("mimeapps.list")).unwrap();
    assert_eq!(linux::default_app(&text, linux::MIME), None, "{text}");
    assert!(!linux::service_file(&dirs.data_home).exists());
    // Without a backup at all, Restore still takes back what Arbor wrote.
    linux::run(Change::Make, &dirs, &backup).unwrap();
    std::fs::remove_file(&backup).unwrap();
    linux::run(Change::Restore, &dirs, &backup).unwrap();
    assert!(!linux::service_file(&dirs.data_home).exists());
    assert_eq!(linux::run(Change::Restore, &dirs, &backup).unwrap(), NOT_DEFAULT);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn linux_restore_leaves_settings_changed_since_and_make_needs_the_desktop_file() {
    let d = tmp("linux-changed");
    let dirs = linux_dirs(&d, false);
    let backup = d.join("arbor").join(BACKUP_FILE);
    let e = linux::run(Change::Make, &dirs, &backup).unwrap_err();
    assert!(e.starts_with("Install Arbor first"), "{e}");
    assert!(!backup.exists() && !dirs.config_home.join("mimeapps.list").exists(), "nothing changed");
    let dirs = linux_dirs(&d, true);
    linux::run(Change::Make, &dirs, &backup).unwrap();
    let mimeapps = dirs.config_home.join("mimeapps.list");
    std::fs::write(&mimeapps, "[Default Applications]\ninode/directory=thunar.desktop\n").unwrap();
    let msg = linux::run(Change::Restore, &dirs, &backup).unwrap();
    assert!(msg.contains("was changed since, so it was left as it is"), "{msg}");
    assert_eq!(std::fs::read_to_string(&mimeapps).unwrap(), "[Default Applications]\ninode/directory=thunar.desktop\n");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_desktops_own_list_that_wins_is_said() {
    let d = tmp("linux-desktop-list");
    let dirs = linux_dirs(&d, true);
    std::fs::create_dir_all(&dirs.config_home).unwrap();
    std::fs::write(dirs.config_home.join("gnome-mimeapps.list"), "[Default Applications]\ninode/directory=nautilus.desktop\n").unwrap();
    let msg = linux::run(Change::Make, &dirs, &d.join(BACKUP_FILE)).unwrap();
    assert!(msg.contains("gnome-mimeapps.list names nautilus.desktop for folders and wins over it"), "{msg}");
    let _ = std::fs::remove_dir_all(&d);
}

/// Linux, a write failing midway (the activation file's folder can't be written): the
/// folder default goes back as it was, and the backup made for it goes.
#[cfg(unix)]
#[test]
fn linux_make_puts_the_folder_default_back_when_a_write_fails() {
    use std::os::unix::fs::PermissionsExt;
    let d = tmp("linux-midway");
    let dirs = linux_dirs(&d, true);
    let mimeapps = dirs.config_home.join("mimeapps.list");
    std::fs::create_dir_all(&dirs.config_home).unwrap();
    let before = "[Default Applications]\ninode/directory=old.desktop\n# kept\n";
    std::fs::write(&mimeapps, before).unwrap();
    let services = dirs.data_home.join("dbus-1/services");
    std::fs::create_dir_all(&services).unwrap();
    std::fs::set_permissions(&services, std::fs::Permissions::from_mode(0o555)).unwrap();
    let backup = d.join(BACKUP_FILE);
    let e = linux::run(Change::Make, &dirs, &backup).unwrap_err();
    assert!(e.ends_with("so nothing was changed"), "{e}");
    assert_eq!(std::fs::read_to_string(&mimeapps).unwrap(), before);
    assert!(!backup.exists());
    // With no mimeapps.list before, none after.
    std::fs::remove_file(&mimeapps).unwrap();
    linux::run(Change::Make, &dirs, &backup).unwrap_err();
    assert!(!mimeapps.exists());
    std::fs::set_permissions(&services, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_service_files_exec_is_quoted_for_the_bus() {
    let s = linux::service_text(Path::new("/opt/my apps/arbor"));
    assert!(s.contains("Exec=\"/opt/my apps/arbor\"\n"), "{s}");
}

// ---- Windows ----

/// An in-memory registry.
#[derive(Default, Debug, Clone, PartialEq)]
struct Reg {
    keys: BTreeMap<String, BTreeMap<String, (u32, Vec<u8>)>>,
}

impl Reg {
    fn create(&mut self, key: &str) {
        let mut k = String::new();
        for part in key.split('\\') {
            if !k.is_empty() {
                k.push('\\');
            }
            k.push_str(part);
            self.keys.entry(k.to_lowercase()).or_default();
        }
    }
    fn text(&self, key: &str, name: &str) -> Option<String> {
        windows::Store::get(self, key, name).and_then(|(k, d)| windows::text(k, &d))
    }
}

impl windows::Store for Reg {
    fn get(&self, key: &str, name: &str) -> Option<(u32, Vec<u8>)> {
        self.keys.get(&key.to_lowercase())?.get(&name.to_lowercase()).cloned()
    }
    fn set(&mut self, key: &str, name: &str, kind: u32, data: &[u8]) -> Result<(), String> {
        self.create(key);
        self.keys.get_mut(&key.to_lowercase()).unwrap().insert(name.to_lowercase(), (kind, data.to_vec()));
        Ok(())
    }
    fn delete_value(&mut self, key: &str, name: &str) -> Result<(), String> {
        if let Some(k) = self.keys.get_mut(&key.to_lowercase()) {
            k.remove(&name.to_lowercase());
        }
        Ok(())
    }
    fn exists(&self, key: &str) -> bool {
        self.keys.contains_key(&key.to_lowercase())
    }
    fn delete_key_if_empty(&mut self, key: &str) -> Result<(), String> {
        let k = key.to_lowercase();
        let prefix = format!("{k}\\");
        if self.keys.get(&k).is_some_and(|v| v.is_empty()) && !self.keys.keys().any(|o| o.starts_with(&prefix)) {
            self.keys.remove(&k);
        }
        Ok(())
    }
}

const EXE: &str = r"C:\Users\me\AppData\Local\Arbor\arbor.exe";

/// A registry whose `n`th write (from 1) fails, as a full or locked hive would.
struct Failing {
    reg: Reg,
    writes: usize,
    fail_at: usize,
}

impl windows::Store for Failing {
    fn get(&self, key: &str, name: &str) -> Option<(u32, Vec<u8>)> {
        self.reg.get(key, name)
    }
    fn set(&mut self, key: &str, name: &str, kind: u32, data: &[u8]) -> Result<(), String> {
        self.writes += 1;
        if self.writes == self.fail_at {
            return Err(format!(r"Couldn't write HKCU\{key}: Access is denied."));
        }
        self.reg.set(key, name, kind, data)
    }
    fn delete_value(&mut self, key: &str, name: &str) -> Result<(), String> {
        self.reg.delete_value(key, name)
    }
    fn exists(&self, key: &str) -> bool {
        self.reg.exists(key)
    }
    fn delete_key_if_empty(&mut self, key: &str) -> Result<(), String> {
        self.reg.delete_key_if_empty(key)
    }
}

/// The review's blocker: a backup that can't be saved used to be found out only after the
/// registry had changed, leaving Arbor the default with nothing to restore from. Now the
/// whole snapshot is saved first, and a failure there changes nothing.
#[test]
fn windows_make_saves_the_backup_before_touching_the_registry() {
    use windows::Store;
    let root = windows::CLASSES;
    let mut reg = Reg::default();
    reg.set(r"Software\Classes\Directory\shell", "", windows::REG_SZ, &windows::utf16z("other")).unwrap();
    let start = reg.clone();
    let d = tmp("win-unwritable");
    std::fs::write(d.join("a file"), "x").unwrap();
    let backup = d.join("a file").join(BACKUP_FILE); // its folder can't be made
    let e = windows::run(Change::Make, &mut reg, root, EXE, &backup).unwrap_err();
    assert!(e.contains(BACKUP_FILE), "{e}");
    assert_eq!(reg, start, "nothing changed without a saved backup");
    let _ = std::fs::remove_dir_all(&d);
}

/// A registry write failing midway: everything written is put back from the snapshot, the
/// backup made for it goes, and it says nothing was changed. Every write position fails once.
#[test]
fn windows_make_puts_everything_back_when_a_write_fails_midway() {
    use windows::Store;
    let root = windows::CLASSES;
    let plan = windows::plan(root, EXE);
    for fail_at in 1..=plan.len() {
        let mut reg = Reg::default();
        reg.set(r"Software\Classes\Directory\shell", "", 2, &windows::utf16z("%SystemRoot%\\other")).unwrap();
        reg.create(r"Software\Classes\CLSID");
        let start = reg.clone();
        let d = tmp(&format!("win-midway-{fail_at}"));
        let backup = d.join(BACKUP_FILE);
        let mut failing = Failing { reg, writes: 0, fail_at };
        let e = windows::run(Change::Make, &mut failing, root, EXE, &backup).unwrap_err();
        assert!(e.ends_with("so nothing was changed"), "write {fail_at}: {e}");
        assert_eq!(failing.reg, start, "write {fail_at}: everything as it was");
        assert!(!backup.exists(), "write {fail_at}: the backup made for it is gone");
        let _ = std::fs::remove_dir_all(&d);
    }
    // Arbor already the default, and a second Make failing: the first backup stays.
    let d = tmp("win-midway-again");
    let backup = d.join(BACKUP_FILE);
    let mut reg = Reg::default();
    windows::run(Change::Make, &mut reg, root, EXE, &backup).unwrap();
    let first = std::fs::read_to_string(&backup).unwrap();
    let mut failing = Failing { reg, writes: 0, fail_at: 3 };
    windows::run(Change::Make, &mut failing, root, r"D:\Arbor\arbor.exe", &backup).unwrap_err();
    assert!(
        std::fs::read_to_string(&backup).unwrap().starts_with(&first[..first.find("wrote").unwrap()]),
        "the first backup's record stays"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// Make again (from another `arbor.exe`) failing halfway puts back the state just before
/// that attempt (the first Make's values, still working), not the state before Arbor; the
/// backup on disk is the first one again, and Restore still puts back what Arbor replaced.
#[test]
fn windows_a_failed_make_again_leaves_the_first_make_in_place() {
    use windows::Store;
    let root = windows::CLASSES;
    for fail_at in 1..=windows::plan(root, EXE).len() {
        let d = tmp(&format!("win-again-{fail_at}"));
        let backup = d.join(BACKUP_FILE);
        let mut reg = Reg::default();
        reg.set(r"Software\Classes\Directory\shell", "", 2, &windows::utf16z("%SystemRoot%\\other")).unwrap();
        let start = reg.clone();
        windows::run(Change::Make, &mut reg, root, EXE, &backup).unwrap();
        let (made, first) = (reg.clone(), std::fs::read_to_string(&backup).unwrap());
        let mut failing = Failing { reg, writes: 0, fail_at };
        let e = windows::run(Change::Make, &mut failing, root, r"D:\Arbor\arbor.exe", &backup).unwrap_err();
        assert!(e.ends_with("so nothing was changed"), "write {fail_at}: {e}");
        assert_eq!(failing.reg, made, "write {fail_at}: the first Make's values, as they were");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), first, "write {fail_at}: the first backup, as it was");
        let mut reg = failing.reg;
        windows::run(Change::Restore, &mut reg, root, EXE, &backup).unwrap();
        assert_eq!(reg, start, "write {fail_at}: Restore still puts back what Arbor replaced");
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// An empty or cut-short backup is damaged, not a record of "nothing was there": Restore
/// (and Make) refuse it and change nothing, on every platform.
#[test]
fn damaged_backups_are_refused_before_anything_changes() {
    let root = windows::CLASSES;
    for damaged in ["", "\n", &format!("{BACKUP_HEADER}\n"), &format!("{BACKUP_HEADER}\n-\tsomething else\n")] {
        let d = tmp("damaged");
        let backup = d.join(BACKUP_FILE);
        // Windows.
        let mut reg = Reg::default();
        windows::run(Change::Make, &mut reg, root, EXE, &backup).unwrap();
        let made = reg.clone();
        std::fs::write(&backup, damaged).unwrap();
        for change in [Change::Restore, Change::Make] {
            let e = windows::run(change, &mut reg, root, EXE, &backup).unwrap_err();
            assert!(e.contains("so nothing was changed"), "{damaged:?}: {e}");
            assert_eq!(reg, made, "{damaged:?}: the registry is as it was");
        }
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), damaged, "and the backup is kept");
        // Linux.
        let dirs = linux_dirs(&d, true);
        let mimeapps = dirs.config_home.join("mimeapps.list");
        std::fs::create_dir_all(&dirs.config_home).unwrap();
        std::fs::write(&mimeapps, "[Default Applications]\ninode/directory=old.desktop\n").unwrap();
        let _ = std::fs::remove_file(&backup);
        linux::run(Change::Make, &dirs, &backup).unwrap();
        let made = std::fs::read_to_string(&mimeapps).unwrap();
        std::fs::write(&backup, damaged).unwrap();
        let e = linux::run(Change::Restore, &dirs, &backup).unwrap_err();
        assert!(e.contains("so nothing was changed"), "{damaged:?}: {e}");
        assert_eq!(std::fs::read_to_string(&mimeapps).unwrap(), made);
        assert!(linux::service_file(&dirs.data_home).exists(), "the activation file stays");
        // macOS.
        let _ = std::fs::remove_file(&backup);
        let mut ls = FakeLs(Some("com.cocoatech.PathFinder".into()), true);
        mac::run(Change::Make, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap();
        std::fs::write(&backup, damaged).unwrap();
        let e = mac::run(Change::Restore, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap_err();
        assert!(e.contains("so nothing was changed"), "{damaged:?}: {e}");
        assert_eq!(ls.0.as_deref(), Some(mac::BUNDLE_ID));
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[test]
fn windows_plan_is_the_minimal_hkcu_set() {
    let plan = windows::plan(windows::CLASSES, EXE);
    let lines: Vec<String> = plan.iter().map(|s| format!(r"{}\[{}] = {}", s.key, s.name, s.data)).collect();
    assert_eq!(
        lines,
        [
            r"Software\Classes\Directory\shell\arbor\[] = Open in Arbor",
            r"Software\Classes\Directory\shell\arbor\[Icon] = C:\Users\me\AppData\Local\Arbor\arbor.exe,0",
            r#"Software\Classes\Directory\shell\arbor\command\[] = "C:\Users\me\AppData\Local\Arbor\arbor.exe" "%1""#,
            r"Software\Classes\Drive\shell\arbor\[] = Open in Arbor",
            r"Software\Classes\Drive\shell\arbor\[Icon] = C:\Users\me\AppData\Local\Arbor\arbor.exe,0",
            r#"Software\Classes\Drive\shell\arbor\command\[] = "C:\Users\me\AppData\Local\Arbor\arbor.exe" "%1""#,
            r#"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command\[] = "C:\Users\me\AppData\Local\Arbor\arbor.exe""#,
            r"Software\Classes\Directory\shell\[] = arbor",
            r"Software\Classes\Drive\shell\[] = arbor",
            r"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command\[DelegateExecute] = ",
        ],
        "the commands first, then what makes them the default"
    );
    assert!(plan.iter().all(|s| !s.key.to_lowercase().contains(r"classes\folder")), "never the Folder class");
    let keys = windows::keys(&plan, windows::CLASSES);
    assert_eq!(keys[0], r"Software\Classes\Directory");
    assert!(keys.contains(&r"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow".to_string()));
}

#[test]
fn windows_make_and_restore_put_back_exactly_what_was_there() {
    let root = windows::CLASSES;
    let mut reg = Reg::default();
    use windows::Store;
    // A user who had another verb as the default for folders, and a CLSID key already.
    reg.set(r"Software\Classes\Directory\shell", "", 2, &windows::utf16z("%SystemRoot%\\other")).unwrap();
    reg.set(r"Software\Classes\Directory\shell\find", "", windows::REG_SZ, &windows::utf16z("Find")).unwrap();
    reg.create(r"Software\Classes\CLSID");
    let start = reg.clone();
    let plan = windows::plan(root, EXE);
    let b = windows::make(&mut reg, &plan, root, None).unwrap();
    assert_eq!(reg.text(r"Software\Classes\Directory\shell", "").as_deref(), Some("arbor"));
    assert_eq!(reg.text(r"Software\Classes\Drive\shell\arbor\command", "").as_deref(), Some(&*format!("\"{EXE}\" \"%1\"")));
    // Made twice: the first backup is kept.
    let b = windows::make(&mut reg, &plan, root, Some(b)).unwrap();
    let (changed, kept) = windows::restore(&mut reg, &plan, root, Some(&b)).unwrap();
    assert!(kept.is_empty() && changed == plan.len(), "{kept:?}");
    assert_eq!(reg, start, "everything as it was, the expandable string's type too");
    assert_eq!(windows::restore(&mut reg, &plan, root, None).unwrap(), (0, Vec::new()), "nothing of Arbor's left");
}

#[test]
fn windows_restore_without_a_backup_and_after_changes() {
    let root = windows::CLASSES;
    let mut reg = Reg::default();
    use windows::Store;
    let plan = windows::plan(root, EXE);
    windows::make(&mut reg, &plan, root, None).unwrap();
    // Another app took over drives since.
    reg.set(r"Software\Classes\Drive\shell", "", windows::REG_SZ, &windows::utf16z("other")).unwrap();
    let b = windows::make(&mut Reg::default(), &plan, root, None).unwrap();
    let (_, kept) = windows::restore(&mut reg, &plan, root, Some(&b)).unwrap();
    assert_eq!(kept, [r"HKCU\Software\Classes\Drive\shell\(Default)"]);
    assert_eq!(reg.text(r"Software\Classes\Drive\shell", "").as_deref(), Some("other"));
    assert!(!reg.exists(r"Software\Classes\Directory"), "keys Arbor made are gone: {reg:?}");
    assert!(!reg.exists(r"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}"));
    assert!(!reg.exists(r"Software\Classes\Drive\shell\arbor"));
    // With no backup at all, what Arbor wrote goes.
    let mut reg = Reg::default();
    reg.create(root);
    let start = reg.clone();
    windows::make(&mut reg, &plan, root, None).unwrap();
    assert_eq!(windows::restore(&mut reg, &plan, root, None).unwrap(), (plan.len(), Vec::new()));
    assert_eq!(reg, start);
    // An Arbor that moved since is still recognised: what it wrote is in the backup.
    let mut reg = Reg::default();
    reg.create(root);
    let start = reg.clone();
    let b = windows::make(&mut reg, &plan, root, None).unwrap();
    let moved = windows::plan(root, r"D:\Arbor\arbor.exe");
    assert!(windows::restore(&mut reg, &moved, root, Some(&b)).unwrap().1.is_empty());
    assert_eq!(reg, start);
}

/// The real registry, under a test-only key (never the real classes): write, check, restore.
#[cfg(windows)]
#[test]
fn windows_round_trip_in_the_real_registry_under_a_test_key() {
    use windows::Store;
    let root = format!(r"Software\ArborTest-{}", std::process::id());
    let mut reg = windows::Hkcu;
    reg.set(&format!(r"{root}\Directory\shell"), "", windows::REG_SZ, &windows::utf16z("before")).unwrap();
    let plan = windows::plan(&root, EXE);
    let b = windows::make(&mut reg, &plan, &root, None).unwrap();
    let read = |k: &str, n: &str| reg.get(k, n).and_then(|(kind, d)| windows::text(kind, &d));
    assert_eq!(read(&format!(r"{root}\Directory\shell"), "").as_deref(), Some("arbor"));
    assert_eq!(read(&format!(r"{root}\Drive\shell\arbor\command"), "").as_deref(), Some(&*format!("\"{EXE}\" \"%1\"")));
    assert_eq!(read(&format!(r"{root}\CLSID\{}\shell\opennewwindow\command", windows::WIN_E), "DelegateExecute").as_deref(), Some(""));
    let (_, kept) = windows::restore(&mut reg, &plan, &root, Some(&b)).unwrap();
    assert!(kept.is_empty(), "{kept:?}");
    let read = |k: &str, n: &str| reg.get(k, n).and_then(|(kind, d)| windows::text(kind, &d));
    assert_eq!(read(&format!(r"{root}\Directory\shell"), "").as_deref(), Some("before"));
    assert!(!reg.exists(&format!(r"{root}\Drive")) && !reg.exists(&format!(r"{root}\CLSID")));
    assert!(!reg.exists(&format!(r"{root}\Directory\shell\arbor")));
    reg.delete_value(&format!(r"{root}\Directory\shell"), "").unwrap();
    for k in [format!(r"{root}\Directory\shell"), format!(r"{root}\Directory"), root.clone()] {
        reg.delete_key_if_empty(&k).unwrap();
    }
    assert!(!reg.exists(&root));
}

// ---- macOS ----

/// LaunchServices: what it answers now, and whether that was chosen (or is its guess).
struct FakeLs(Option<String>, bool);
impl mac::Handlers for FakeLs {
    fn current(&self) -> Option<String> {
        self.0.clone()
    }
    fn chosen(&self) -> Option<String> {
        self.0.clone().filter(|_| self.1)
    }
    fn set(&mut self, id: &str) -> Result<(), String> {
        self.0 = Some(id.into());
        self.1 = true;
        Ok(())
    }
}

#[test]
fn mac_make_needs_the_bundle_and_restore_puts_back_the_old_viewer() {
    let d = tmp("mac");
    let backup = d.join(BACKUP_FILE);
    let mut ls = FakeLs(Some("com.cocoatech.PathFinder".into()), true);
    let e = mac::run(Change::Make, &mut ls, None, &backup).unwrap_err();
    assert!(e.starts_with("Only Arbor.app can be the default for folders"), "{e}");
    assert_eq!(ls.0.as_deref(), Some("com.cocoatech.PathFinder"), "nothing changed");
    mac::run(Change::Make, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap();
    assert_eq!(ls.0.as_deref(), Some(mac::BUNDLE_ID));
    mac::run(Change::Make, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap();
    assert_eq!(
        mac::run(Change::Restore, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap(),
        "Folders open in com.cocoatech.PathFinder again"
    );
    assert_eq!(ls.0.as_deref(), Some("com.cocoatech.PathFinder"));
    // Only LaunchServices' guess (Archive Utility, as on the Mac here): Finder, never the guess.
    let mut ls = FakeLs(Some("com.apple.archiveutility".into()), false);
    mac::run(Change::Make, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap();
    assert_eq!(mac::run(Change::Restore, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap(), "Folders open in Finder again");
    assert_eq!(ls.0.as_deref(), Some(mac::FINDER));
    // Nothing at all: Finder.
    let mut ls = FakeLs(None, false);
    mac::run(Change::Make, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap();
    assert_eq!(mac::run(Change::Restore, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap(), "Folders open in Finder again");
    assert_eq!(ls.0.as_deref(), Some(mac::FINDER));
    // Changed since by something else: left alone.
    mac::run(Change::Make, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap();
    ls.0 = Some("com.other".into());
    let msg = mac::run(Change::Restore, &mut ls, Some(mac::BUNDLE_ID), &backup).unwrap();
    assert!(msg.contains("com.other is, set since"), "{msg}");
    assert_eq!(ls.0.as_deref(), Some("com.other"));
    let _ = std::fs::remove_dir_all(&d);
}

/// Read-only: what LaunchServices says the folder viewer is (never changes it).
#[cfg(target_os = "macos")]
#[test]
fn mac_reads_the_current_folder_viewer() {
    use mac::Handlers;
    eprintln!("public.folder viewer: {:?}, chosen: {:?}", mac::LaunchServices.current(), mac::LaunchServices.chosen());
    assert_eq!(mac::bundle_id(), None, "the test binary isn't an app bundle, so Make would explain instead");
}

// ---- robustness of the backup file ----

/// Any text after the header (or without it): decoding never panics, and anything it accepts
/// is what `encode` writes (each value whole bytes of hex, or `-`), so it reads the same again.
#[test]
fn any_backup_text_decodes_to_what_encode_writes_or_an_error() {
    const BITS: &[&str] =
        &["\n", "\r\n", "\t", "-", "#", " ", "0", "9", "a", "f", "F", "g", "+", "é", "\u{301}", "\0", "key", "61", "+a", "-1"];
    crate::prop::check("backup_decode", 2000, |r| {
        let body = if r.one_in(3) { r.string(40) } else { r.from(BITS, 30) };
        let text = if r.one_in(5) { body } else { format!("{BACKUP_HEADER}\n{body}") };
        if let Ok(b) = Backup::decode(&text) {
            for line in text.lines().skip(1).filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
                let hex = line.split_once('\t').unwrap().0;
                assert!(hex == "-" || (hex.len() % 2 == 0 && hex.bytes().all(|c| c.is_ascii_hexdigit())), "{text:?}: {hex:?} accepted");
            }
            assert_eq!(Backup::decode(&b.encode()), Ok(b), "{text:?}");
        }
    });
}

/// Every backup Arbor writes reads back the same.
#[test]
fn any_backup_round_trips() {
    crate::prop::check("backup_round_trip", 500, |r| {
        let mut b = Backup::default();
        for _ in 0..r.len(6) {
            let key = loop {
                let k = r.string(12).replace(['\n', '\r'], "");
                // A name is never blank or a comment (Arbor's are fixed words).
                if !k.trim().is_empty() && !k.starts_with('#') {
                    break k;
                }
            };
            let value = if r.one_in(4) { None } else { Some(r.bytes(30)) };
            b.put(&key, value.as_deref());
        }
        assert_eq!(Backup::decode(&b.encode()).as_ref(), Ok(&b));
    });
}

/// Found by `any_backup_text_decodes_to_what_encode_writes_or_an_error`: a value with a
/// multi-byte character in it panicked (slicing it between pairs of hex digits), and one with
/// "+a" was read as the byte 0x0a. Both are damage.
#[test]
fn a_value_that_isnt_hex_is_damage_not_a_panic_or_a_guess() {
    for hex in ["a\u{e9}b", "+a", "0x", " 1", "-1"] {
        assert!(Backup::decode(&format!("{BACKUP_HEADER}\n{hex}\tkey\n")).is_err(), "{hex:?}");
    }
    assert_eq!(Backup::decode(&format!("{BACKUP_HEADER}\n0aFf\tkey\n")).unwrap().get("key"), Some(Some(&b"\x0a\xff"[..])));
}

/// Any mimeapps.list text: setting the folder default reads back as what was set, removing it
/// leaves none, and every line not about it stays as it was (line breaks aside).
#[test]
fn any_mimeapps_text_takes_the_folder_default_and_keeps_the_rest() {
    const BITS: &[&str] = &[
        "[Default Applications]",
        "[Added Associations]",
        "[Other]",
        "\n",
        "\r\n",
        "inode/directory",
        "=",
        " = ",
        "x.desktop",
        ";",
        "#",
        " ",
        "text/plain=a.desktop",
        "[",
        "é",
    ];
    crate::prop::check("mimeapps_edit", 3000, |r| {
        let text = if r.one_in(4) { r.string(60) } else { r.from(BITS, 30) };
        let v = r.pick(&["arbor.desktop", "old.desktop;fallback.desktop;"]).to_string();
        let set = linux::set_default_app(&text, linux::MIME, Some(&v));
        assert_eq!(linux::default_app(&set, linux::MIME).as_deref(), Some(v.as_str()), "{text:?} → {set:?}");
        let gone = linux::set_default_app(&set, linux::MIME, None);
        assert_eq!(linux::default_app(&gone, linux::MIME), None, "{set:?} → {gone:?}");
        let about = |l: &str| l.trim().split_once('=').is_some_and(|(k, _)| k.trim() == linux::MIME);
        let others = |t: &str| {
            t.lines()
                .filter(|l| !about(l) && l.trim() != "[Default Applications]" && !l.trim().is_empty())
                .map(|l| l.trim_end_matches('\r').to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(others(&gone), others(&text), "{text:?}");
    });
}

/// Found by reading `linux::read`: mimeapps.list was read with U+FFFD for bytes that aren't
/// UTF-8 and written back, so Make changed the user's other lines (a comment in Latin-1, say).
/// Now such a file is refused, untouched; and another file manager's activation file is kept
/// and put back byte for byte, whatever it holds.
#[test]
fn linux_never_rewrites_a_file_it_cant_read_exactly() {
    let d = tmp("linux-bytes");
    let dirs = linux_dirs(&d, true);
    let backup = d.join("arbor").join(BACKUP_FILE);
    let mimeapps = dirs.config_home.join("mimeapps.list");
    std::fs::create_dir_all(&dirs.config_home).unwrap();
    let latin1 = b"# caf\xe9\n[Default Applications]\ninode/directory=old.desktop\n";
    std::fs::write(&mimeapps, latin1).unwrap();
    let e = linux::run(Change::Make, &dirs, &backup).unwrap_err();
    assert!(e.contains("isn't text (UTF-8)"), "{e}");
    assert_eq!(std::fs::read(&mimeapps).unwrap(), latin1, "untouched");
    assert!(!backup.exists());
    // Their activation file, with bytes that aren't UTF-8, round-trips exactly.
    std::fs::write(&mimeapps, "[Default Applications]\ninode/directory=old.desktop\n").unwrap();
    let service = linux::service_file(&dirs.data_home);
    std::fs::create_dir_all(service.parent().unwrap()).unwrap();
    let theirs = b"[D-BUS Service]\nName=org.freedesktop.FileManager1\nExec=/opt/caf\xe9/fm\n";
    std::fs::write(&service, theirs).unwrap();
    linux::run(Change::Make, &dirs, &backup).unwrap();
    linux::run(Change::Restore, &dirs, &backup).unwrap();
    assert_eq!(std::fs::read(&service).unwrap(), theirs);
    let _ = std::fs::remove_dir_all(&d);
}
