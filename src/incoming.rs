//! Folders other apps ask Arbor to show, whether it's running or they launch it.
//!
//! - **Windows and Linux** pass a folder on the command line (`arbor.exe "%1"`, the
//!   `.desktop` file's `%F`), which the first window opens (`app::start_dir`).
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

/// Starts listening for requests to the running app (see the module docs). Costs nothing on
/// Windows, a handler on macOS, and on Linux a thread that checks one file and ends unless Arbor
/// is the default for folders.
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
    drop(sink);
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

    /// On its own thread: does nothing more unless the activation file is Arbor's; then
    /// serves the name until the process ends (waiting on the bus's socket, no polling).
    pub(super) fn spawn(sink: Sink) {
        let _ = std::thread::Builder::new().name("arbor-dbus".into()).spawn(move || {
            let Some(file) = data_home().map(|d| service_file(&d)) else { return };
            if !std::fs::read_to_string(file).is_ok_and(|t| is_ours(&t)) {
                return;
            }
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            rt.block_on(async move {
                // The name may be someone else's (a file manager that doesn't let go of it).
                if let Ok(_conn) = connect(zbus::connection::Builder::session(), sink, true).await {
                    std::future::pending::<()>().await;
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{settle_all, state_in};
    use std::path::Path;

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

    /// The FileManager1 interface on a private session bus: ShowFolders and ShowItems reach
    /// the sink as tabs to open. Skipped where there's no `dbus-daemon`.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn file_manager1_on_a_private_bus() {
        use std::io::BufRead;
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
