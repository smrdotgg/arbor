//! Folders other apps ask Arbor to show, whether it's running or they launch it.
//!
//! - **Windows and Linux** pass a folder on the command line (`arbor.exe "%1"`, the
//!   `.desktop` file's `%F`), which the first window opens (`app::start_dir`).
//! - **Windows**: a second `arbor.exe <folder>` hands its folders to the running Arbor over a
//!   named pipe and exits before opening a window ([`windows`]); the running one opens each in
//!   a new tab. A thread waits on the pipe (blocking, never polling).
//! - **macOS** sends an "open documents" Apple event instead, to the running app or the one it
//!   launched for it ("Open With → Arbor", `open -a Arbor dir`, Arbor as the default for
//!   folders). Rune passes on only "open URL" events (RUNE-GAPS.md), so Arbor installs its own
//!   handler, when AppKit says it's about to finish launching ([`mac`]).
//! - **Linux** also serves `org.freedesktop.FileManager1` on the session bus while Arbor is the
//!   default for folders ([`linux`]), so "Show in folder" from browsers and other apps opens a
//!   tab in the running Arbor, or the bus starts one through the activation file
//!   `handler::linux` writes. It's served only while that file is Arbor's: opting out stops it.
//!
//! A request becomes a tab ([`State::show_external`]): a folder opens in a new tab of the
//! active pane, and items open in their folder, selected. The first one replaces the tab the
//! window opened with while nobody has used it yet, so launching Arbor for a folder shows just
//! that folder. No polling: the bus thread waits on its socket, and AppKit calls the handler.

use std::ffi::OsString;
use std::path::PathBuf;

use rune::Tx;

use crate::app::State;
use crate::folder::Nav;

/// What another app asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Show {
    Folder(PathBuf),
    /// These names, selected in this folder.
    Items(PathBuf, Vec<OsString>),
}

/// Folders as themselves, anything else (or everything, with `items`) selected in its folder,
/// items of one folder together, in the order asked. Looks at the disk: off the UI thread.
pub fn classify(paths: Vec<PathBuf>, items: bool) -> Vec<Show> {
    let mut out: Vec<Show> = Vec::new();
    for p in paths {
        let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else {
            out.push(Show::Folder(p));
            continue;
        };
        if !items && p.is_dir() {
            out.push(Show::Folder(p));
            continue;
        }
        match out.iter_mut().find(|s| matches!(s, Show::Items(dir, _) if dir == parent)) {
            Some(Show::Items(_, names)) => names.push(name.to_os_string()),
            _ => out.push(Show::Items(parent.to_path_buf(), vec![name.to_os_string()])),
        }
    }
    out
}

#[cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]
/// The local path a `file://` URI names (`file:///a%20b` → `/a b`), or `None` for another
/// scheme or host.
pub fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let (host, path) = rest.split_at(slash);
    if !host.is_empty() && host != "localhost" {
        return None;
    }
    let mut bytes = Vec::with_capacity(path.len());
    let b = path.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).ok()?, 16)
        {
            bytes.push(v);
            i += 3;
            continue;
        }
        bytes.push(b[i]);
        i += 1;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        Some(PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
    }
}

/// Where requests go: the window, through Rune's handle (any thread), with the activation
/// token the asking app passed, if any.
pub type Sink = std::sync::Arc<dyn Fn(Vec<Show>, Option<String>) + Send + Sync>;

/// Starts listening for requests to the running app (see the module docs). On Windows a thread
/// blocked on the pipe, a handler on macOS, and on Linux a thread that checks one file and ends
/// unless Arbor is the default for folders.
pub fn listen(handle: rune::AppHandle<State>) {
    let sink: Sink = std::sync::Arc::new(move |shows: Vec<Show>, token: Option<String>| {
        if shows.is_empty() {
            return;
        }
        handle.update(move |s, tx| {
            tx.instant();
            s.show_external(shows, tx);
        });
        match token.filter(|t| !t.is_empty()) {
            Some(t) => handle.activate(t),
            None => handle.open(),
        }
    });
    #[cfg(target_os = "macos")]
    mac::install(sink);
    #[cfg(all(unix, not(target_os = "macos")))]
    linux::spawn(sink);
    #[cfg(windows)]
    windows::serve(windows::pipe_name(), sink);
}

impl State {
    /// Shows what another app asked for: each folder in a new tab of the active pane, items
    /// selected in theirs. The first replaces the window's first tab while nobody has used it.
    pub fn show_external(&mut self, shows: Vec<Show>, tx: &mut Tx) {
        for show in shows {
            let (dir, select) = match show {
                Show::Folder(p) => (p, Vec::new()),
                Show::Items(p, names) => (p, names),
            };
            if !self.untouched() {
                self.new_tab(tx);
            }
            self.go_full(dir, Nav::Fresh, select, false);
            self.external_shows += 1;
        }
        self.sync_preview();
    }

    /// Whether the window is still as it opened: one tab, nothing navigated, selected,
    /// filtered or searched, and nothing shown for another app yet.
    fn untouched(&self) -> bool {
        let [pane] = self.panes.as_slice() else { return false };
        let f = &pane.f;
        self.external_shows == 0
            && pane.tabs.len() == 1
            && self.lands <= 1
            && f.back.is_empty()
            && f.fwd.is_empty()
            && f.picks == 0
            && f.filter.is_empty()
            && !f.searching()
    }
}

// ---- macOS ----------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod mac {
    use super::{Sink, classify};
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, NSObject, Sel};
    use objc2::{ClassType, class, msg_send, sel};
    use objc2_foundation::NSString;
    use std::path::PathBuf;
    use std::sync::OnceLock;

    /// `kCoreEventClass` and `kAEOpenDocuments`.
    const CORE: u32 = u32::from_be_bytes(*b"aevt");
    const OPEN_DOCUMENTS: u32 = u32::from_be_bytes(*b"odoc");
    /// `keyDirectObject`: the list of files.
    const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");

    static SINK: OnceLock<Sink> = OnceLock::new();

    /// The file paths in an open-documents event.
    fn paths_of(event: &AnyObject) -> Vec<PathBuf> {
        autoreleasepool(|_| {
            let mut out = Vec::new();
            // SAFETY: `event` is an NSAppleEventDescriptor; every message is nil-safe and
            // returns an autoreleased object (or nil) or an integer.
            unsafe {
                let list: *mut AnyObject = msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT];
                let Some(list) = list.as_ref() else { return out };
                let n: isize = msg_send![list, numberOfItems];
                let items: Vec<*mut AnyObject> = if n == 0 {
                    vec![list as *const AnyObject as *mut AnyObject]
                } else {
                    (1..=n).map(|i| msg_send![list, descriptorAtIndex: i]).collect()
                };
                for item in items {
                    let Some(item) = item.as_ref() else { continue };
                    let url: *mut AnyObject = msg_send![item, fileURLValue];
                    let Some(url) = url.as_ref() else { continue };
                    let path: *mut NSString = msg_send![url, path];
                    if let Some(p) = path.as_ref() {
                        out.push(PathBuf::from(p.to_string()));
                    }
                }
            }
            out
        })
    }

    extern "C" fn open_documents(_: &AnyObject, _: Sel, event: *mut AnyObject, _reply: *mut AnyObject) {
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: AppKit passes the event, or nil.
            let Some(event) = (unsafe { event.as_ref() }) else { return };
            let paths = paths_of(event);
            let Some(sink) = SINK.get().cloned() else { return };
            // Which are folders is a disk question: not on the main thread.
            std::thread::spawn(move || sink(classify(paths, false), None));
        }));
        if caught.is_err() {
            eprintln!("arbor: open-documents handler panicked");
        }
    }

    /// AppKit installs its own open-documents handler while it launches, so Arbor's goes in
    /// after it, when AppKit is about to finish launching and before it hands over the event
    /// that launched the app.
    extern "C" fn will_finish_launching(this: &AnyObject, _: Sel, _note: *mut AnyObject) {
        // SAFETY: Foundation calls on the main thread, with `this` alive for the process.
        autoreleasepool(|_| unsafe {
            let manager: *mut AnyObject = msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
            if let Some(manager) = manager.as_ref() {
                let _: () = msg_send![manager, setEventHandler: this, andSelector: sel!(handleOpenDocuments:withReplyEvent:),
                    forEventClass: CORE, andEventID: OPEN_DOCUMENTS];
            }
        });
    }

    /// Arbor's handler object, made once and kept for the process.
    fn handler() -> Option<*mut AnyObject> {
        const NAME: &std::ffi::CStr = c"ArborOpenDocuments";
        let class = AnyClass::get(NAME).or_else(|| {
            let mut b = ClassBuilder::new(NAME, NSObject::class())?;
            // SAFETY: each function matches its selector (objects in, void out).
            unsafe {
                b.add_method(sel!(handleOpenDocuments:withReplyEvent:), open_documents as extern "C" fn(_, _, _, _));
                b.add_method(sel!(willFinishLaunching:), will_finish_launching as extern "C" fn(_, _, _));
            }
            Some(b.register())
        })?;
        // SAFETY: `new` on an NSObject subclass; the +1 reference is kept for the process.
        let made: *mut AnyObject = unsafe { msg_send![class, new] };
        (!made.is_null()).then_some(made)
    }

    pub(super) fn install(sink: Sink) {
        if SINK.set(sink).is_err() {
            return;
        }
        let Some(h) = handler() else { return };
        // SAFETY: Foundation calls on the main thread (before Rune's event loop starts); the
        // observer is kept for the process.
        autoreleasepool(|_| unsafe {
            let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
            let name = NSString::from_str("NSApplicationWillFinishLaunchingNotification");
            if let Some(center) = center.as_ref() {
                let nil: *mut AnyObject = std::ptr::null_mut();
                let _: () = msg_send![center, addObserver: h, selector: sel!(willFinishLaunching:), name: &*name, object: nil];
            }
        });
    }
}

// ---- Linux ----------------------------------------------------------------------------------

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) mod linux {
    use super::{Sink, classify, file_uri_path};
    use crate::handler::linux::{BUS_NAME, data_home, is_ours, service_file};

    pub const OBJECT_PATH: &str = "/org/freedesktop/FileManager1";

    /// `org.freedesktop.FileManager1`, as Nautilus, Dolphin and Nemo serve it.
    pub struct FileManager {
        pub sink: Sink,
    }

    impl FileManager {
        fn deliver(&self, uris: &[String], startup_id: &str, items: bool) {
            let paths = uris.iter().filter_map(|u| file_uri_path(u)).collect();
            (self.sink)(classify(paths, items), Some(startup_id.to_string()));
        }
    }

    #[zbus::interface(name = "org.freedesktop.FileManager1")]
    impl FileManager {
        /// Opens each folder.
        fn show_folders(&self, uris: Vec<String>, startup_id: String) {
            self.deliver(&uris, &startup_id, false);
        }

        /// Opens each item's folder with it selected.
        fn show_items(&self, uris: Vec<String>, startup_id: String) {
            self.deliver(&uris, &startup_id, true);
        }

        /// Arbor has no properties window: the items are shown selected in their folder.
        fn show_item_properties(&self, uris: Vec<String>, startup_id: String) {
            self.deliver(&uris, &startup_id, true);
        }
    }

    /// Connects through `builder`, serves the interface and (with `claim`) takes the name,
    /// replacing an owner that allows it. Inside a Tokio runtime.
    pub async fn connect(builder: zbus::connection::Builder<'static>, sink: Sink, claim: bool) -> zbus::Result<zbus::Connection> {
        let mut b = builder.serve_at(OBJECT_PATH, FileManager { sink });
        if claim {
            b = b.name(BUS_NAME).replace_existing_names(true);
        }
        b.build().await
    }

    /// Tells the bus thread to look at the activation file again.
    static RECHECK: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<()>> = std::sync::OnceLock::new();

    /// Whether the activation file is Arbor's, so Arbor should serve the name.
    fn ours() -> bool {
        data_home().map(|d| service_file(&d)).is_some_and(|f| std::fs::read_to_string(f).is_ok_and(|t| is_ours(&t)))
    }

    /// On its own thread: serves the name while the activation file is Arbor's (from startup,
    /// or once Make writes it) and lets it go when Restore removes it, waiting on the bus's
    /// socket and on [`recheck`] (no polling).
    pub(super) fn spawn(sink: Sink) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        if RECHECK.set(tx).is_err() {
            return;
        }
        let _ = std::thread::Builder::new().name("arbor-dbus".into()).spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            rt.block_on(follow(zbus::connection::Builder::session, sink, ours, rx));
        });
    }

    /// "Make Arbor the Default for Folders" or its restore finished: serve the name, or let it
    /// go, as the activation file now says. Does nothing before [`spawn`] (tests, headless).
    pub fn recheck() {
        if let Some(tx) = RECHECK.get() {
            let _ = tx.send(());
        }
    }

    /// Serves the name on a connection from `builder` whenever `ours()` says so, checked at
    /// the start and on each message from `rx`; releases it when it stops saying so.
    pub async fn follow(
        builder: impl Fn() -> zbus::connection::Builder<'static>,
        sink: Sink,
        ours: impl Fn() -> bool,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    ) {
        let mut conn: Option<zbus::Connection> = None;
        loop {
            match (ours(), conn.take()) {
                // The name may be someone else's (a file manager that doesn't let go of it):
                // then this waits for the next change.
                (true, None) => conn = connect(builder(), sink.clone(), true).await.ok(),
                (true, Some(c)) => conn = Some(c),
                (false, Some(c)) => {
                    let _ = c.release_name(BUS_NAME).await;
                }
                (false, None) => {}
            }
            if rx.recv().await.is_none() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
}

// ---- Windows --------------------------------------------------------------------------------

/// One Arbor per user session on Windows: the first serves a named pipe; a later
/// `arbor.exe <folder>` writes its folders to it and exits, and the first opens each in a tab.
/// Both ends block (the server on `ConnectNamedPipe`, which wakes when a client connects):
/// nothing polls. The pipe takes only local clients, and its default security lets only this
/// user (and administrators) write to it. Two Arbors started at the same instant may both
/// open windows (the second can't create the pipe, so it just runs on its own).
#[cfg(windows)]
pub(crate) mod windows {
    use super::{Sink, classify};
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
    };
    use windows_sys::Win32::System::IO::CancelSynchronousIo;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain([0]).collect()
    }

    /// The pipe for this user's login session (a fast user switch or a remote session gets its
    /// own Arbor).
    pub fn pipe_name() -> String {
        let mut session = 0u32;
        // SAFETY: a valid out pointer; on failure `session` stays 0.
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
        let user: String =
            std::env::var("USERNAME").unwrap_or_default().chars().filter(|c| c.is_alphanumeric() || "-_.".contains(*c)).collect();
        format!(r"\\.\pipe\gg.smr.arbor.{session}.{user}")
    }

    /// How long a second Arbor waits, once connected, for the running one to take its paths
    /// and say so; then it opens its own window instead.
    pub const DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

    /// What the running Arbor answers once it has read a request.
    const ACK: u8 = 1;

    /// The paths as the pipe carries them: UTF-16 code units, each path ended by a NUL, and one
    /// more NUL to end the request (so the running Arbor knows it's all there and can answer
    /// on the same pipe). An older client just closes its end.
    pub fn encode(paths: &[PathBuf]) -> Vec<u8> {
        paths
            .iter()
            .filter(|p| !p.as_os_str().is_empty())
            .flat_map(|p| p.as_os_str().encode_wide().chain([0]))
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    /// Whether `bytes` holds a whole request (it ends with the extra NUL).
    fn complete(bytes: &[u8]) -> bool {
        bytes.len() >= 4 && bytes.len().is_multiple_of(2) && bytes[bytes.len() - 4..] == [0, 0, 0, 0]
    }

    pub fn decode(bytes: &[u8]) -> Vec<PathBuf> {
        let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|&b| u16::from_le_bytes(b)).collect();
        units.split(|&u| u == 0).filter(|p| !p.is_empty()).map(|p| PathBuf::from(OsString::from_wide(p))).collect()
    }

    /// Hands `paths` to the Arbor serving `name`: true when it took them (this process then
    /// exits). False when none is running (or it couldn't be reached): this one opens its own
    /// window. Lets the running one bring its window to the front, which Windows allows only
    /// with the say-so of the process the user just started.
    /// At startup: the paths on the command line, if any, to a running Arbor. Touches no disk
    /// (the running one sorts folders from files on its pipe's thread); without arguments it
    /// does nothing, so a plain launch costs nothing and opens a window as before.
    pub fn hand_off_args() -> bool {
        let paths: Vec<PathBuf> =
            std::env::args_os().skip(1).filter(|a| !a.to_string_lossy().starts_with("--")).map(PathBuf::from).collect();
        if paths.is_empty() {
            return false;
        }
        let paths: Vec<PathBuf> = paths.into_iter().map(|p| std::path::absolute(&p).unwrap_or(p)).collect();
        hand_off(&pipe_name(), &paths)
    }

    pub fn hand_off(name: &str, paths: &[PathBuf]) -> bool {
        let name = wide(name);
        for _ in 0..3 {
            // SAFETY: `name` is NUL-terminated; no security attributes or template.
            let h: HANDLE = unsafe {
                CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
            };
            if h == INVALID_HANDLE_VALUE {
                // SAFETY: plain calls; `name` is NUL-terminated.
                if unsafe { GetLastError() } == ERROR_PIPE_BUSY && unsafe { WaitNamedPipeW(name.as_ptr(), 2000) } != 0 {
                    continue;
                }
                return false;
            }
            let mut pid = 0u32;
            // SAFETY: `h` is the open pipe; `pid` a valid out pointer.
            if unsafe { GetNamedPipeServerProcessId(h, &mut pid) } != 0 {
                unsafe { AllowSetForegroundWindow(pid) };
            }
            return exchange(h, encode(paths), DEADLINE);
        }
        false
    }

    /// Writes the request on the connected pipe `h` and waits for the running Arbor's answer,
    /// on a thread of its own, for at most `deadline`: a running Arbor that's hung (or stuck on
    /// an earlier request) never holds this launch up. On time out the thread's blocked call is
    /// cancelled and false comes back (this one opens its own window). Closes `h`.
    fn exchange(h: HANDLE, bytes: Vec<u8>, deadline: std::time::Duration) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        let pipe = h as usize;
        let worker = std::thread::spawn(move || {
            let h = pipe as HANDLE;
            let mut sent = 0usize;
            let mut ok = true;
            while sent < bytes.len() {
                let mut n = 0u32;
                let chunk = (bytes.len() - sent).min(u32::MAX as usize) as u32;
                // SAFETY: the buffer is valid for `chunk` bytes from `sent`.
                if unsafe { WriteFile(h, bytes[sent..].as_ptr(), chunk, &mut n, std::ptr::null_mut()) } == 0 || n == 0 {
                    ok = false;
                    break;
                }
                sent += n as usize;
            }
            let mut ack = [0u8];
            let mut n = 0u32;
            // SAFETY: `h` is the open pipe; `ack` is valid for one byte.
            ok = ok
                && unsafe { FlushFileBuffers(h) } != 0
                && unsafe { ReadFile(h, ack.as_mut_ptr(), 1, &mut n, std::ptr::null_mut()) } != 0
                && n == 1
                && ack[0] == ACK;
            // SAFETY: `h` is ours to close.
            unsafe { CloseHandle(h) };
            let _ = tx.send(ok);
        });
        match rx.recv_timeout(deadline) {
            Ok(ok) => ok,
            Err(_) => {
                use std::os::windows::io::AsRawHandle;
                // SAFETY: the worker's own thread handle, alive while `worker` is; it then
                // closes the pipe and ends.
                unsafe { CancelSynchronousIo(worker.as_raw_handle() as HANDLE) };
                false
            }
        }
    }

    /// Serves `name` on a thread of its own, sending what each client writes to `sink`. Does
    /// nothing if another process already serves it.
    pub fn serve(name: String, sink: Sink) {
        let wname = wide(&name);
        let first = make(&wname, true);
        if first == INVALID_HANDLE_VALUE {
            return;
        }
        // Handles are plain values: the thread owns this one from here.
        let first = first as usize;
        std::thread::Builder::new()
            .name("arbor-pipe".into())
            .spawn(move || {
                let mut h = first as HANDLE;
                loop {
                    // SAFETY: `h` is a pipe instance this thread made; blocks until a client
                    // connects (or one connected before this call: ERROR_PIPE_CONNECTED).
                    let connected =
                        unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0 || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
                    // The next instance first, so there's always one waiting: a client between
                    // this one closing and a new one opening would find no pipe and open a
                    // window of its own.
                    let next = make(&wname, false);
                    if connected {
                        let paths = decode(&read_all(h));
                        // Taken: the client can go (it waits for this, a second at most).
                        let mut n = 0u32;
                        // SAFETY: `h` is the connected pipe; one byte from a valid buffer.
                        unsafe { WriteFile(h, [ACK].as_ptr(), 1, &mut n, std::ptr::null_mut()) };
                        unsafe { FlushFileBuffers(h) };
                        // Which are folders is a disk question, which can be slow: not on this
                        // thread, which goes straight back to serving the next client.
                        if !paths.is_empty() {
                            let sink = sink.clone();
                            std::thread::spawn(move || sink(classify(paths, false), None));
                        }
                    }
                    // SAFETY: done with this client and this instance.
                    unsafe {
                        DisconnectNamedPipe(h);
                        CloseHandle(h);
                    }
                    if next == INVALID_HANDLE_VALUE {
                        return;
                    }
                    h = next;
                }
            })
            .ok();
    }

    /// One instance of the pipe `name` (NUL-terminated); `first` fails if any exists already.
    fn make(name: &[u16], first: bool) -> HANDLE {
        let mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        // SAFETY: `name` is NUL-terminated; default security (this user and administrators may
        // write, others only read, which gives them at most the one-byte answer).
        unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64,
                64 * 1024,
                0,
                std::ptr::null(),
            )
        }
    }

    /// Everything the client writes, until its request ends (or it closes its end; at most
    /// 1 MB).
    fn read_all(h: HANDLE) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while out.len() < 1 << 20 && !complete(&out) {
            let mut n = 0u32;
            // SAFETY: `buf` is valid for its length.
            if unsafe { ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) } == 0 || n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn paths_survive_the_pipe_encoding() {
            let paths = vec![PathBuf::from(r"C:\Users\me\Documents"), PathBuf::from(r"D:\été\naïve 2")];
            let bytes = encode(&paths);
            assert!(complete(&bytes) && !complete(&bytes[..bytes.len() - 2]), "the extra NUL ends it");
            assert_eq!(decode(&bytes), paths);
            assert!(decode(&[]).is_empty());
            assert_eq!(
                decode(&encode(&[PathBuf::new(), PathBuf::from(r"C:\")])),
                [PathBuf::from(r"C:\")],
                "an empty path can't end it early"
            );
        }

        /// The review's case: a running Arbor that accepts the connection but never reads (hung,
        /// or stuck on an earlier request) used to hold a later launch forever. Now it gives
        /// up after the deadline and opens its own window.
        #[test]
        fn a_stalled_first_arbor_doesnt_hold_a_launch() {
            let name = format!(r"\\.\pipe\gg.smr.arbor.stalled.{}", std::process::id());
            let wname = wide(&name);
            let h = make(&wname, true) as usize;
            let (connected, hold) = std::sync::mpsc::channel::<()>();
            std::thread::spawn(move || {
                // SAFETY: the instance made above; it's never read from.
                unsafe { ConnectNamedPipe(h as HANDLE, std::ptr::null_mut()) };
                let _ = connected.send(());
                std::thread::sleep(std::time::Duration::from_secs(30));
            });
            let t = std::time::Instant::now();
            assert!(!hand_off(&name, &[PathBuf::from(r"C:\")]), "not taken");
            assert!(t.elapsed() < DEADLINE * 3, "gave up in {:?}", t.elapsed());
            drop(hold);
        }

        /// A second Arbor's hand-off reaches the first: each folder as a request, and the
        /// server keeps serving after a client.
        #[test]
        fn a_second_arbor_hands_its_folders_to_the_first() {
            let name = format!(r"\\.\pipe\gg.smr.arbor.test.{}", std::process::id());
            let (tx, rx) = std::sync::mpsc::channel();
            let tx = std::sync::Mutex::new(tx);
            serve(name.clone(), std::sync::Arc::new(move |shows, _| tx.lock().unwrap().send(shows).unwrap()));
            let a = std::env::temp_dir();
            let b = std::env::current_dir().unwrap();
            assert!(hand_off(&name, std::slice::from_ref(&a)));
            let got = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            assert_eq!(got, vec![super::super::Show::Folder(a)]);
            assert!(hand_off(&name, std::slice::from_ref(&b)), "still serving, right after the first");
            let got = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            assert_eq!(got, vec![super::super::Show::Folder(b.clone())]);
            // Several at once, back to back: none finds the pipe missing.
            for _ in 0..5 {
                assert!(hand_off(&name, std::slice::from_ref(&b)));
            }
            for _ in 0..5 {
                rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            }
            // A second server for the same name doesn't start; nobody serving: no hand-off.
            serve(name.clone(), std::sync::Arc::new(|_, _| panic!("a second server")));
            assert!(!hand_off(&format!("{name}.nobody"), &[PathBuf::from(r"C:\")]));
        }
    }
}
