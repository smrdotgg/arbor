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
    let s = |v: Vec<Vec<String>>| v.into_iter().map(|r| r.join("|")).collect::<Vec<_>>();
    assert_eq!(s(exec_argv("gedit %U", &f(&["/a b.txt", "/c"]))), ["gedit|/a b.txt|/c"]);
    assert_eq!(s(exec_argv("viewer --one %f", &f(&["/a", "/b"]))), ["viewer|--one|/a", "viewer|--one|/b"]);
    assert_eq!(s(exec_argv("\"/opt/My App/run\" --file=%f %i %c", &f(&["/a"]))), ["/opt/My App/run|--file=/a"]);
    assert_eq!(s(exec_argv("sh -c \"echo \\\"hi\\\" 100%%\" %F", &f(&["/a"]))), ["sh|-c|echo \"hi\" 100%|/a"]);
    assert_eq!(s(exec_argv("plain", &f(&["/a"]))), ["plain|/a"], "no field code: files at the end");
    assert_eq!(unescape_value("a\\sb\\\\c"), "a b\\c");
}
