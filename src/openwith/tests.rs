use super::xdg::*;
use super::*;

const GLOBS: &str = "\
# This file was automatically generated
50:text/plain:*.txt
50:text/x-csrc:*.c
50:text/x-c++src:*.C:cs
50:image/png:*.png
60:application/x-compressed-tar:*.tar.gz
50:application/gzip:*.gz
55:text/x-makefile:makefile
10:text/x-readme:README*
";

#[test]
fn globs2_picks_the_weightiest_then_longest_match() {
    assert_eq!(mime_from_globs(GLOBS, "notes.TXT").map(|m| m.1).as_deref(), Some("text/plain"));
    assert_eq!(mime_from_globs(GLOBS, "a.tar.gz").map(|m| m.1).as_deref(), Some("application/x-compressed-tar"));
    assert_eq!(mime_from_globs(GLOBS, "a.gz").map(|m| m.1).as_deref(), Some("application/gzip"));
    assert_eq!(mime_from_globs(GLOBS, "x.C").map(|m| m.1).as_deref(), Some("text/x-c++src"), "case-sensitive line");
    assert_eq!(mime_from_globs(GLOBS, "x.c").map(|m| m.1).as_deref(), Some("text/x-csrc"));
    assert_eq!(mime_from_globs(GLOBS, "Makefile").map(|m| m.1).as_deref(), Some("text/x-makefile"));
    assert_eq!(mime_from_globs(GLOBS, "README.md").map(|m| m.1).as_deref(), Some("text/x-readme"));
    assert_eq!(mime_from_globs(GLOBS, "photo.jpeg"), None);
}

const USER_LIST: &str = "\
[Default Applications]
text/plain=gone.desktop;org.gnome.TextEditor.desktop;
image/png=org.gnome.Loupe.desktop

[Added Associations]
text/plain=vim.desktop;code.desktop;

[Removed Associations]
text/plain=nano.desktop;
";
const SYSTEM_LIST: &str = "[Default Applications]\ntext/plain=gedit.desktop\n";
const CACHE: &str =
    "[MIME Cache]\ntext/plain=gedit.desktop;nano.desktop;hidden.desktop;org.gnome.TextEditor.desktop;\nimage/png=gimp.desktop;\n";

fn desktop(id: &str) -> Option<Desktop> {
    let text = match id {
        "org.gnome.TextEditor.desktop" => {
            "[Desktop Entry]\nType=Application\nName=Text Editor\nName[de]=Texteditor\nExec=gnome-text-editor %U\nMimeType=text/plain;\n"
        }
        "vim.desktop" => "[Desktop Entry]\nName=Vim\nExec=vim %F\nTerminal=true\n",
        "code.desktop" => {
            "[Desktop Entry]\nName=Visual Studio Code\nExec=/usr/share/code/code --unity-launch %F\n[Desktop Action new]\nName=New Window\nExec=code --new-window\n"
        }
        "gedit.desktop" => "[Desktop Entry]\nName=gedit\nExec=gedit %U\n",
        "nano.desktop" => "[Desktop Entry]\nName=GNU nano\nExec=nano %F\n",
        "hidden.desktop" => "[Desktop Entry]\nName=Hidden\nExec=hidden %f\nHidden=true\n",
        "gimp.desktop" => "[Desktop Entry]\nName=GIMP\nExec=gimp-2.10 %U\n",
        _ => return None,
    };
    parse_desktop(text, "en_US.UTF-8")
}

/// The default from the user's list (skipping one whose file is gone), then the added
/// associations, then the cache, without removed or hidden apps or the same app twice.
#[test]
fn linux_handlers_follow_the_spec_order() {
    let lists = [parse_mimeapps(USER_LIST), parse_mimeapps(SYSTEM_LIST)];
    let caches = [parse_mimeinfo_cache(CACHE)];
    let got = resolve("text/plain", &lists, &caches, desktop);
    let names: Vec<(&str, bool)> = got.iter().map(|h| (h.name.as_str(), h.default)).collect();
    assert_eq!(names, [("Text Editor", true), ("Vim", false), ("Visual Studio Code", false), ("gedit", false)]);
    assert_eq!(got[2].exec.as_deref(), Some("/usr/share/code/code --unity-launch %F"), "the main entry's Exec, not an action's");
    let png = resolve("image/png", &lists, &caches, desktop);
    assert_eq!(png.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["GIMP"], "a default whose file is missing is skipped");
    assert!(resolve("video/mp4", &lists, &caches, desktop).is_empty());
}

#[test]
fn desktop_files_give_a_translated_name() {
    let text = "[Desktop Entry]\nName=Text Editor\nName[de]=Texteditor\nName[de_AT]=Texteditor (AT)\nExec=te %U\n";
    assert_eq!(parse_desktop(text, "de_DE.UTF-8").unwrap().name, "Texteditor");
    assert_eq!(parse_desktop(text, "de_AT.UTF-8").unwrap().name, "Texteditor (AT)");
    assert_eq!(parse_desktop(text, "C").unwrap().name, "Text Editor");
    assert!(parse_desktop("[Desktop Entry]\nName=Link\nType=Link\nExec=x\n", "C").is_none());
    assert!(parse_desktop("[Desktop Entry]\nName=No exec\n", "C").is_none());
}

#[test]
fn exec_lines_expand_their_field_codes() {
    let f = |v: &[&str]| v.iter().map(PathBuf::from).collect::<Vec<_>>();
    let s = |v: Vec<Vec<std::ffi::OsString>>| v.into_iter().map(|r| strings(r).join("|")).collect::<Vec<_>>();
    assert_eq!(s(exec_argv("gedit %U", &f(&["/a b.txt", "/c"]))), ["gedit|/a b.txt|/c"]);
    assert_eq!(s(exec_argv("viewer --one %f", &f(&["/a", "/b"]))), ["viewer|--one|/a", "viewer|--one|/b"]);
    assert_eq!(s(exec_argv("\"/opt/My App/run\" --file=%f %i %c", &f(&["/a"]))), ["/opt/My App/run|--file=/a"]);
    assert_eq!(s(exec_argv("sh -c \"echo \\\"hi\\\" 100%%\" %F", &f(&["/a"]))), ["sh|-c|echo \"hi\" 100%|/a"]);
    assert_eq!(s(exec_argv("plain", &f(&["/a"]))), ["plain|/a"], "no field code: files at the end");
    assert_eq!(unescape_value("a\\sb\\\\c"), "a b\\c");
}

// ---- robustness of desktop files and Exec lines ----

fn strings(r: Vec<std::ffi::OsString>) -> Vec<String> {
    r.into_iter().map(|a| a.into_string().expect("UTF-8")).collect()
}

/// [`exec_argv`] as text (every name here is UTF-8).
fn argv(exec: &str, files: &[PathBuf]) -> Vec<Vec<String>> {
    exec_argv(exec, files).into_iter().map(strings).collect()
}

/// A file name with anything in it: spaces, quotes, newlines, `%`, a leading `-`.
fn any_file(r: &mut crate::prop::Rng) -> String {
    let mut s = String::from("/");
    s.push_str(&r.from(&["a", " ", "\"", "'", "\n", "%", "%f", "%F", "%%", "-", "--x", "\\", "$", "`", "é", ";", "\t", "*"], 8));
    s
}

/// Whatever the file names, each lands as one argument, exactly as it is, as many times as its
/// field code says: `%F`/`%U` all of them in one run, `%f`/`%u` one per run, none at the end.
#[test]
fn every_file_is_one_argument_whatever_its_name() {
    crate::prop::check("exec_argv_files", 2000, |r| {
        let files: Vec<PathBuf> = (0..1 + r.below(4)).map(|_| PathBuf::from(any_file(r))).collect();
        let names: Vec<String> = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let cases: Vec<(&str, Vec<Vec<String>>)> = vec![
            ("app %F", vec![[s(&["app"]), names.clone()].concat()]),
            ("app %U", vec![[s(&["app"]), names.clone()].concat()]),
            ("app", vec![[s(&["app"]), names.clone()].concat()]),
            ("\"/opt/My App/run\" -n %F", vec![[s(&["/opt/My App/run", "-n"]), names.clone()].concat()]),
            ("app %f", names.iter().map(|n| s(&["app", n])).collect()),
            ("app --file=%u %i", names.iter().map(|n| s(&["app", &format!("--file={n}")])).collect()),
            ("app 100%% %F", vec![[s(&["app", "100%"]), names.clone()].concat()]),
            ("app %%f %F", vec![[s(&["app", "%f"]), names.clone()].concat()]),
            ("app --literal=%%f", vec![[s(&["app", "--literal=%f"]), names.clone()].concat()]),
        ];
        for (exec, want) in cases {
            assert_eq!(argv(exec, &files), want, "{exec} with {names:?}");
        }
    });
}

/// Any Exec line: never a panic, never an empty command, and every file passed appears as an
/// argument of its own (or inside one, for `--x=%f`), never split.
#[test]
fn any_exec_line_runs_something_sane() {
    const BITS: &[&str] =
        &["app", " ", "\"", "\\\"", "\\\\", "\\", "%f", "%F", "%u", "%U", "%%", "%", "%i", "%c", "%k", "--x=", "'", "\t", "é"];
    crate::prop::check("exec_argv_any", 3000, |r| {
        let exec = r.from(BITS, 12);
        let files: Vec<PathBuf> = (0..r.below(3)).map(|_| PathBuf::from(any_file(r))).collect();
        for run in argv(&exec, &files) {
            assert!(!run.is_empty() && run.iter().all(|a| !a.contains('\0')), "{exec:?}: {run:?}");
        }
    });
}

/// Found by `every_file_is_one_argument_whatever_its_name`: a literal "%%f" (the text "%f")
/// was taken for the field code, so the app ran once per file and was given none of them.
#[test]
fn a_literal_percent_f_isnt_a_field_code() {
    let f = vec![PathBuf::from("/a"), PathBuf::from("/b")];
    assert_eq!(argv("app --x=%%f", &f), [["app", "--x=%f", "/a", "/b"]]);
    assert_eq!(argv("app %f %F", &f), [["app", "/a", "/b"]], "a stray %f with %F is dropped, not an empty argument");
}

/// A file whose name isn't UTF-8 was passed with U+FFFD in its place: the app was asked to open
/// a file that doesn't exist (or another one). Now it's passed byte for byte.
#[cfg(unix)]
#[test]
fn a_name_that_isnt_utf8_is_passed_exactly() {
    use std::os::unix::ffi::OsStringExt;
    let f = PathBuf::from(std::ffi::OsString::from_vec(b"/caf\xe9.txt".to_vec()));
    for exec in ["app %f", "app %F", "app", "app --file=%u"] {
        let runs = exec_argv(exec, std::slice::from_ref(&f));
        let last = runs[0].last().unwrap().clone().into_vec();
        assert!(last.ends_with(b"/caf\xe9.txt"), "{exec}: {last:?}");
    }
}

/// Found reading `find_desktop`: an id from `mimeapps.list` was joined to the applications
/// folder as it was, so "../../x.desktop" or "/tmp/x.desktop" read a desktop file from anywhere
/// (and ran its Exec line). Now only a file name ending in `.desktop` is looked up.
#[test]
fn a_desktop_id_never_reaches_outside_the_applications_folders() {
    let apps = vec![PathBuf::from("/usr/share/applications")];
    for id in [
        "../../tmp/x.desktop",
        "/tmp/x.desktop",
        "a/../../b.desktop",
        "..desktop",
        ".hidden.desktop",
        "x\0.desktop",
        "",
        "x.sh",
        "kde-../x.desktop",
    ] {
        let got = desktop_paths(&apps, id);
        assert!(
            got.iter().all(|p| p.parent() == Some(apps[0].as_path()) || p.parent().and_then(Path::parent) == Some(apps[0].as_path())),
            "{id:?}: {got:?}"
        );
        assert!(got.iter().all(|p| !p.components().any(|c| matches!(c, std::path::Component::ParentDir))), "{id:?}: {got:?}");
    }
    assert!(desktop_paths(&apps, "../../tmp/x.desktop").is_empty());
    assert_eq!(desktop_paths(&apps, "org.gnome.gedit.desktop"), [apps[0].join("org.gnome.gedit.desktop")]);
    assert_eq!(desktop_paths(&apps, "kde-dolphin.desktop"), [apps[0].join("kde-dolphin.desktop"), apps[0].join("kde/dolphin.desktop")]);
    crate::prop::check("desktop_ids", 2000, |r| {
        let id = r.from(&["a", "-", ".", "..", "/", "\\", ".desktop", "é", "\0", " "], 8);
        for p in desktop_paths(&apps, &id) {
            assert!(p.starts_with(&apps[0]) && !p.components().any(|c| matches!(c, std::path::Component::ParentDir)), "{id:?}: {p:?}");
        }
    });
}

/// Any key file: parsing never panics (desktop files, mimeapps.list, mimeinfo.cache).
#[test]
fn any_key_file_parses_without_panicking() {
    const BITS: &[&str] = &[
        "[Desktop Entry]",
        "[Default Applications]",
        "[Added Associations]",
        "[Removed Associations]",
        "[MIME Cache]",
        "[",
        "]",
        "\n",
        "\r\n",
        "=",
        ";",
        "Name",
        "Name[de]",
        "Exec",
        "Type=Application",
        "MimeType",
        "Hidden=true",
        "text/plain",
        "a.desktop",
        " ",
        "#",
        "\\s",
        "\\",
        "é",
    ];
    crate::prop::check("key_files_any", 2000, |r| {
        let text = if r.one_in(4) { r.string(100) } else { r.from(BITS, 40) };
        let _ = parse_desktop(&text, "de_DE.UTF-8");
        let m = parse_mimeapps(&text);
        for (t, ids) in m.defaults.iter().chain(&m.added).chain(&m.removed) {
            assert!(!t.contains('\n') && ids.iter().all(|i| !i.is_empty() && !i.contains(';')), "{text:?}");
        }
        let _ = parse_mimeinfo_cache(&text);
        let _ = unescape_value(&text);
    });
}
