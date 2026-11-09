//! The Linux package's `install.sh` and `uninstall.sh`, run for real against a throwaway home
//! (with a stand-in for the `arbor` binary), as a user would run them from the tarball.

use std::path::{Path, PathBuf};
use std::process::Command;

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
