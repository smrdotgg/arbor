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
