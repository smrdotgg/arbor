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
