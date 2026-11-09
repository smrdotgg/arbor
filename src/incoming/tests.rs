use super::*;
use crate::app::tests::{settle_all, state_in};

#[test]
fn file_uris_become_paths() {
    assert_eq!(file_uri_path("file:///home/me/a%20b/c%C3%A9"), Some(PathBuf::from("/home/me/a b/cé")));
    assert_eq!(file_uri_path("file://localhost/tmp/x"), Some(PathBuf::from("/tmp/x")));
    assert_eq!(file_uri_path("file://server/share/x"), None);
    assert_eq!(file_uri_path("https://example.com/x"), None);
    assert_eq!(file_uri_path("file:///100%"), Some(PathBuf::from("/100%")), "a stray % stays");
    assert_eq!(file_uri_path("file:///"), Some(PathBuf::from("/")));
}

/// Percent-encodes a path's bytes as a file manager's caller would (unreserved and `/` as they
/// are).
#[cfg(unix)]
fn encode(b: &[u8]) -> String {
    b.iter()
        .map(|&c| if c.is_ascii_alphanumeric() || b"/-._~".contains(&c) { (c as char).to_string() } else { format!("%{c:02X}") })
        .collect()
}

/// Any path (any bytes but NUL) sent as a `file://` URI comes back exactly.
#[cfg(unix)]
#[test]
fn any_path_round_trips_through_a_file_uri() {
    use std::os::unix::ffi::OsStrExt;
    crate::prop::check("file_uri_round_trip", 3000, |r| {
        let mut b = vec![b'/'];
        b.extend(r.bytes(30).into_iter().filter(|&c| c != 0));
        let host = r.pick(&["", "localhost"]);
        let uri = format!("file://{host}{}", encode(&b));
        assert_eq!(file_uri_path(&uri).as_deref().map(|p| p.as_os_str().as_bytes()), Some(&b[..]), "{uri}");
    });
}

/// What a `file://` URI names, the plain way: a local host, no fragment, each `%` and two hex
/// digits a byte (never a NUL or a `/`, which no name holds), anything else as it is.
fn reference(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.strip_prefix("file://")?;
    let at = rest.find('/')?;
    let (host, path) = rest.split_at(at);
    if !(host.is_empty() || host == "localhost") || path.contains('#') {
        return None;
    }
    let (b, mut out, mut i) = (path.as_bytes(), Vec::new(), 0);
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            let v = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).ok()?, 16).ok()?;
            if v == 0 || v == b'/' {
                return None;
            }
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Any text: never a panic, and what's read is what the URI names, the plain way.
#[test]
fn any_uri_gives_what_it_names_or_nothing() {
    const BITS: &[&str] = &[
        "file://",
        "file:",
        "localhost",
        "server",
        "/",
        "%",
        "%2F",
        "%2f",
        "%00",
        "%+f",
        "%-1",
        "%41",
        "%C3%A9",
        "%zz",
        "#",
        "?",
        "a",
        "é",
        " ",
        "%\u{e9}",
    ];
    crate::prop::check("file_uri_any", 5000, |r| {
        let uri = r.from(BITS, 10);
        let got = file_uri_path(&uri);
        #[cfg(unix)]
        assert_eq!(got.as_deref().map(|p| std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str()).to_vec()), reference(&uri), "{uri:?}");
        #[cfg(not(unix))]
        assert_eq!(got.is_some(), reference(&uri).is_some(), "{uri:?}");
    });
}

/// Found by `any_uri_gives_what_it_names_or_nothing`: "%+f" was read as the byte 0x0f
/// (`from_str_radix` takes a sign), "%2F" became a separator inside a name, "%00" a NUL, and a
/// fragment ("#…") part of the path; a "%" before a character that isn't ASCII threw the whole
/// URI away. GLib refuses the first four too; the last is a literal "%" now.
#[test]
fn a_file_uri_never_hides_a_separator_a_nul_or_a_fragment() {
    for uri in ["file:///a%2Fb", "file:///a%2fb", "file:///a%00b", "file:///a#frag", "file:///a%2F..%2Fetc"] {
        assert_eq!(file_uri_path(uri), None, "{uri}");
    }
    assert_eq!(file_uri_path("file:///a%+fb"), Some(PathBuf::from("/a%+fb")));
    assert_eq!(file_uri_path("file:///a%-1b"), Some(PathBuf::from("/a%-1b")));
    assert_eq!(file_uri_path("file:///caf%\u{e9}"), Some(PathBuf::from("/caf%\u{e9}")));
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-incoming-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("sub")).unwrap();
    std::fs::write(d.join("a.txt"), "a").unwrap();
    std::fs::write(d.join("sub/b.txt"), "b").unwrap();
    d
}

#[test]
fn folders_open_and_items_are_selected_in_theirs() {
    let d = tmp("classify");
    let shows = classify(vec![d.join("sub"), d.join("a.txt"), d.join("sub/b.txt"), d.join("missing")], false);
    assert_eq!(
        shows,
        [
            Show::Folder(d.join("sub")),
            Show::Items(d.clone(), vec!["a.txt".into(), "missing".into()]),
            Show::Items(d.join("sub"), vec!["b.txt".into()]),
        ]
    );
    // ShowItems: a folder is shown selected in its parent.
    assert_eq!(classify(vec![d.join("sub")], true), [Show::Items(d.clone(), vec!["sub".into()])]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_first_request_replaces_the_untouched_tab_and_later_ones_open_tabs() {
    let d = tmp("tabs");
    let mut s = state_in(&d);
    settle_all(&mut s);
    let mut tx = Tx::default();
    s.show_external(vec![Show::Folder(d.join("sub"))], &mut tx);
    settle_all(&mut s);
    assert_eq!((s.panes[0].tabs.len(), s.cur().cwd.clone()), (1, d.join("sub")), "launched for a folder: just that folder");
    assert!(s.cur().back.is_empty(), "its history starts there");
    s.show_external(vec![Show::Items(d.clone(), vec!["a.txt".into()])], &mut tx);
    settle_all(&mut s);
    assert_eq!(s.panes[0].tabs.len(), 2, "a new tab once the window was used");
    assert_eq!(s.cur().cwd, d);
    assert_eq!(s.cur().selected().map(|e| e.name.clone()).as_deref(), Some("a.txt"));
    assert!(s.cur().back.is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

/// The review's case: several folders in one request (a macOS open-documents event, a
/// burst of D-Bus calls) arrive before the first one's read lands. Each ends up in its own
/// tab, selection included; the first used to be lost (its tab was hidden still showing
/// the folder it was leaving).
#[test]
fn several_requests_at_once_each_get_their_tab() {
    let d = tmp("burst");
    std::fs::create_dir(d.join("second")).unwrap();
    let mut s = state_in(&d);
    settle_all(&mut s);
    s.show_external(
        vec![Show::Folder(d.join("sub")), Show::Items(d.join("second"), Vec::new()), Show::Items(d.clone(), vec!["a.txt".into()])],
        &mut Tx::default(),
    );
    settle_all(&mut s);
    let p = &s.panes[0];
    assert_eq!(p.tabs.len(), 3);
    assert_eq!((0..3).map(|j| p.tab_path(j).to_path_buf()).collect::<Vec<_>>(), [d.join("sub"), d.join("second"), d.clone()]);
    assert_eq!(s.cur().selected().map(|e| e.name.clone()).as_deref(), Some("a.txt"));
    // The hidden ones show their folders, history fresh, when chosen.
    s.show_tab(0, 0, &mut Tx::default());
    settle_all(&mut s);
    assert_eq!(s.cur().cwd, d.join("sub"));
    assert!(s.cur().back.is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

/// A folder opened (not yet landed) and then ⌘T: the tab left behind keeps the folder it
/// was opening, with the folder it left in its Back history.
#[test]
fn a_tab_hidden_on_its_way_keeps_where_it_was_going() {
    let d = tmp("on-its-way");
    let mut s = state_in(&d);
    settle_all(&mut s);
    s.go(d.join("sub"), Nav::New, None);
    s.new_tab(&mut Tx::default());
    settle_all(&mut s);
    let m = s.panes[0].tabs[0].as_ref().expect("the hidden tab's memo");
    assert_eq!((m.path.clone(), m.back.clone()), (d.join("sub"), vec![d.clone()]));
    assert_eq!(s.cur().cwd, d.join("sub"), "and the new tab shows it too");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_window_in_use_keeps_its_tab() {
    let d = tmp("in-use");
    let mut s = state_in(&d);
    settle_all(&mut s);
    s.go(d.join("sub"), Nav::New, None);
    settle_all(&mut s);
    s.show_external(vec![Show::Folder(d.clone())], &mut Tx::default());
    settle_all(&mut s);
    assert_eq!(s.panes[0].tabs.len(), 2);
    let _ = std::fs::remove_dir_all(&d);
}

/// The review's case: Make, in the running Arbor, starts serving the name at once (it used
/// to wait for the next launch, so the next request started a second Arbor), and Restore
/// lets it go (the second Arbor used to keep it, taking requests after Restore). On a
/// private bus; skipped where there's no `dbus-daemon`.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn the_name_follows_make_and_restore_in_the_running_arbor() {
    use std::io::BufRead;
    use std::sync::atomic::{AtomicBool, Ordering};
    let Ok(mut daemon) = std::process::Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(std::process::Stdio::piped())
        .spawn()
    else {
        eprintln!("no dbus-daemon: skipped");
        return;
    };
    let mut addr = String::new();
    std::io::BufReader::new(daemon.stdout.take().unwrap()).read_line(&mut addr).unwrap();
    let addr: &'static str = Box::leak(addr.trim().to_string().into_boxed_str());
    let (shows, got) = std::sync::mpsc::channel();
    let shows = std::sync::Mutex::new(shows);
    let sink: Sink = std::sync::Arc::new(move |s, _| shows.lock().unwrap().send(s).unwrap());
    static OURS: AtomicBool = AtomicBool::new(false);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(linux::follow(move || zbus::connection::Builder::address(addr), sink, || OURS.load(Ordering::SeqCst), rx));
        let client = zbus::connection::Builder::address(addr).build().await.unwrap();
        let owned = || {
            let client = client.clone();
            async move {
                let r = client
                    .call_method(
                        Some("org.freedesktop.DBus"),
                        "/org/freedesktop/DBus",
                        Some("org.freedesktop.DBus"),
                        "NameHasOwner",
                        &(crate::handler::linux::BUS_NAME,),
                    )
                    .await
                    .unwrap();
                r.body().deserialize::<bool>().unwrap()
            }
        };
        // Each call is a round trip to the bus, which lets the serving task run.
        let wait_for = |want: bool| async move {
            for _ in 0..500 {
                if owned().await == want {
                    return true;
                }
                tokio::task::yield_now().await;
            }
            false
        };
        assert!(wait_for(false).await, "not Arbor's: not served");
        OURS.store(true, Ordering::SeqCst); // Make wrote the activation file
        tx.send(()).unwrap();
        assert!(wait_for(true).await, "served at once after Make");
        client
            .call_method(
                Some(crate::handler::linux::BUS_NAME),
                linux::OBJECT_PATH,
                Some("org.freedesktop.FileManager1"),
                "ShowFolders",
                &(vec!["file:///tmp"], ""),
            )
            .await
            .unwrap();
        assert_eq!(got.try_recv().unwrap(), vec![Show::Folder(PathBuf::from("/tmp"))]);
        OURS.store(false, Ordering::SeqCst); // Restore removed it
        tx.send(()).unwrap();
        assert!(wait_for(false).await, "let go after Restore");
    });
    let _ = daemon.kill();
    let _ = daemon.wait();
}

/// The FileManager1 interface on a private session bus: ShowFolders and ShowItems reach
/// the sink as tabs to open. Skipped where there's no `dbus-daemon`.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn file_manager1_on_a_private_bus() {
    use std::io::BufRead;
    use std::path::Path;
    let Ok(mut daemon) = std::process::Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(std::process::Stdio::piped())
        .spawn()
    else {
        eprintln!("no dbus-daemon: skipped");
        return;
    };
    let mut addr = String::new();
    std::io::BufReader::new(daemon.stdout.take().unwrap()).read_line(&mut addr).unwrap();
    let addr = addr.trim().to_string();
    let d = tmp("dbus");
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let sink: Sink = std::sync::Arc::new(move |shows, token| {
        let _ = tx.lock().unwrap().send((shows, token));
    });
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let uri = |p: &Path| format!("file://{}", p.display().to_string().replace(' ', "%20"));
    rt.block_on(async {
        let _server = linux::connect(zbus::connection::Builder::address(addr.as_str()), sink, true).await.unwrap();
        let client = zbus::connection::Builder::address(addr.as_str()).build().await.unwrap();
        let call = |method: &'static str, uris: Vec<String>| {
            let client = client.clone();
            async move {
                client
                    .call_method(
                        Some(crate::handler::linux::BUS_NAME),
                        linux::OBJECT_PATH,
                        Some("org.freedesktop.FileManager1"),
                        method,
                        &(uris, "token-1"),
                    )
                    .await
                    .unwrap();
            }
        };
        call("ShowFolders", vec![uri(&d.join("sub"))]).await;
        call("ShowItems", vec![uri(&d.join("a.txt")), uri(&d.join("sub/b.txt"))]).await;
        call("ShowItemProperties", vec![uri(&d.join("sub"))]).await;
    });
    let got: Vec<_> = rx.try_iter().collect();
    assert_eq!(
        got,
        [
            (vec![Show::Folder(d.join("sub"))], Some("token-1".to_string())),
            (
                vec![Show::Items(d.clone(), vec!["a.txt".into()]), Show::Items(d.join("sub"), vec!["b.txt".into()])],
                Some("token-1".to_string())
            ),
            (vec![Show::Items(d.clone(), vec!["sub".into()])], Some("token-1".to_string())),
        ]
    );
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&d);
}
