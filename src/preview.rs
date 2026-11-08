//! Quick preview (NORTHSTAR Core: "on demand, no cache"). Space opens a panel showing the
//! active pane's selected item: the head of a text file, an image shrunk to the panel, or a
//! card of what Arbor knows about anything else (a folder's item count, which formats need
//! the platform's own preview API).
//!
//! It's view-triggered background work, so it follows every NORTHSTAR rule for that:
//!
//! - **Closed costs nothing.** `State::preview` is `None`: no reads, no decoding, nothing per
//!   key. `State::sync_preview` is one `None` check in `poll`.
//! - **First paint never waits.** The panel's header (name, kind, size, date, all from the
//!   listing) shows in the key's own frame; the body lands from a worker when it's ready.
//! - **Scoped to what's on screen.** A selection change or closing the panel sets the read's
//!   stop flag, which every read call and every decoded row checks, and an older answer
//!   never lands (each request has its own number).
//! - **Finishes and stops.** One read of one file (or one listing of a folder, to count it),
//!   then nothing: no watching.
//! - **Nothing stored.** Moving away and back reads the file again. The decoded pixels go
//!   when the selection moves.
//! - **A slow disk slows the preview, never the UI.** All reading and decoding runs on one
//!   thread of its own ([`Worker`]), started by the first preview and kept for the session
//!   across opens, so at most one read or decode runs at a time: a new request replaces one
//!   still waiting (which then never runs) and never runs beside the one in progress. Folder
//!   reads keep Rune's shared workers, even when a preview read hangs on a dead disk (later
//!   previews then wait behind it). Only regular files are opened: a pipe would block its
//!   reader forever.
//! - **Memory stays bounded.** One decode at a time, and every image's size is checked from
//!   its header before its pixels are allocated. Formats decoded whole, which can't stop
//!   between rows (GIF, WebP, BMP, interlaced PNG, progressive JPEG), are refused over
//!   [`MAX_PIXELS_WHOLE`], so one that can't be cancelled is short.

use std::cell::Cell;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};

use rune::Pixels;

use crate::app::State;
use crate::fs::{self, Entry};

/// How much of a text file is shown at most: 256 KB (decimal, as sizes show everywhere).
pub const TEXT_MAX: usize = 256_000;
/// How much is read first to tell an image or text from anything else.
const HEAD: usize = 8 * 1024;
/// Text lines longer than this many characters are cut (a minified file is one huge line,
/// and laying it out would cost more than the whole rest of the panel).
pub const MAX_COLS: usize = 1000;
/// Columns a tab advances to the next multiple of.
const TAB: usize = 4;
/// The most pixels an image may have when its decoder streams it (PNG rows) or shrinks it
/// while decoding (JPEG's IDCT scaling): the result is panel-sized whatever the source.
pub const MAX_PIXELS: u64 = 100_000_000;
/// The most pixels for formats decoded whole before shrinking (GIF, WebP, BMP, interlaced PNG,
/// progressive and 16-bit JPEG). Their decoders run to the end once started (the stop flag is
/// only seen between reads, and a small file of a big image is read in one go), so this bounds
/// how long and how big a decode that can't be cancelled is: 24 MP (6000 × 4000, a full-frame
/// camera's size) is 96 MB of RGBA, plus as much again for a GIF's canvas, and about 200 ms to
/// decode a GIF on this class of machine (a 49 MP one took 423 ms). Serialized, one at a time,
/// that's at most about 200 MB for a moment on an 8 GB machine, however fast Space is tapped.
pub const MAX_PIXELS_WHOLE: u64 = 24_000_000;

/// The panel's state while it's open.
pub struct Preview {
    /// What the panel is for: the active pane's selected item, from the listing.
    pub target: Option<Target>,
    pub body: Body,
    /// "Link to …", for a symbolic link (read on the worker).
    pub link: Option<String>,
    /// The image the body shows, replaced (not reused) for each image.
    pub pixels: Pixels,
    /// The request on screen (the worker numbers them across opens), and the stop flag of the
    /// read for it.
    req: u64,
    stop: Arc<AtomicBool>,
}

/// The preview's one worker thread: started by the first preview, kept for the session (it
/// sleeps on a condition variable while there's nothing to do), and ended when the state is
/// dropped. One request runs at a time; a newer one replaces the one waiting.
pub struct Worker {
    shared: Arc<Shared>,
}

struct Shared {
    slots: Mutex<Slots>,
    wake: Condvar,
}

#[derive(Default)]
struct Slots {
    /// The next request: replaced by a newer one before it starts, so it never runs.
    next: Option<(u64, Job, Arc<AtomicBool>)>,
    /// The last answer, until the UI takes it.
    done: Option<Answer>,
    /// The last request number given out.
    req: u64,
    waker: Option<rune::Waker>,
    /// The state is gone: the thread ends after what it's doing.
    closed: bool,
}

impl Worker {
    fn spawn(waker: Option<rune::Waker>) -> Worker {
        let shared = Arc::new(Shared { slots: Mutex::new(Slots { waker, ..Slots::default() }), wake: Condvar::new() });
        let s = shared.clone();
        std::thread::Builder::new().name("arbor-preview".into()).spawn(move || s.run()).expect("can't start the preview thread");
        Worker { shared }
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.shared.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues `job` as the next request, replacing one that hasn't started. Its number.
    fn request(&self, job: Job, stop: Arc<AtomicBool>) -> u64 {
        let mut s = self.slots();
        s.req += 1;
        let req = s.req;
        s.next = Some((req, job, stop));
        s.done = None;
        self.shared.wake.notify_one();
        req
    }

    /// Drops the waiting request and any answer not taken yet (the one running stops by its
    /// flag).
    fn cancel(&self) {
        let mut s = self.slots();
        s.next = None;
        s.done = None;
    }

    fn take(&self) -> Option<Answer> {
        self.slots().done.take()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let mut s = self.slots();
        s.closed = true;
        s.next = None;
        self.shared.wake.notify_one();
    }
}

impl Shared {
    fn run(&self) {
        loop {
            let (req, job, stop) = {
                let mut s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if s.closed {
                        return;
                    }
                    if let Some(next) = s.next.take() {
                        break next;
                    }
                    s = self.wake.wait(s).unwrap_or_else(|e| e.into_inner());
                }
            };
            let loaded = load(&job, &stop);
            let mut s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            // Only the newest request's answer is kept (a stopped read gives none).
            if let Some(loaded) = loaded
                && req == s.req
            {
                s.done = Some(Answer { req, loaded });
                if let Some(w) = s.waker.clone() {
                    drop(s);
                    w();
                }
            }
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

/// The item a preview is for, as the listing has it.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub pane: u64,
    pub dir: PathBuf,
    pub os: OsString,
    pub name: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub size: u64,
    pub mtime_ms: i64,
}

impl Target {
    fn of(pane: u64, dir: &Path, e: &Entry) -> Target {
        Target {
            pane,
            dir: dir.to_path_buf(),
            os: e.os.clone(),
            name: e.name.clone(),
            is_dir: e.is_dir,
            is_link: e.is_link,
            size: e.size,
            mtime_ms: e.mtime_ms,
        }
    }

    /// Whether this is still `e` in pane `pane` showing `dir` (no allocation: it runs every
    /// frame while the panel is open). A changed size or date (a reload after an edit) is a new
    /// target, read again.
    fn is(&self, pane: u64, dir: &Path, e: &Entry) -> bool {
        self.pane == pane && self.os == e.os && self.size == e.size && self.mtime_ms == e.mtime_ms && self.dir == dir
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(&self.os)
    }

    /// The lowercased extension.
    fn ext(&self) -> String {
        fs::ext(&self.name.to_lowercase()).to_string()
    }
}

/// What the panel's body shows.
#[derive(Debug, PartialEq)]
pub enum Body {
    /// Nothing selected.
    Empty,
    /// The worker hasn't answered yet.
    Loading,
    Text(Text),
    Image(Picture),
    Card(Card),
    /// It couldn't be read.
    Error(String),
}

/// The head of a text file, ready to show line by line.
#[derive(Debug, PartialEq)]
pub struct Text {
    /// Tabs expanded and control characters replaced.
    pub text: String,
    /// Each line's byte range in `text`, without its line break.
    pub lines: Vec<Range<u32>>,
    /// The longest line, in characters, for the panel's scroll width.
    pub cols: usize,
    /// What the panel should say about it: truncated, not UTF-8, lines cut.
    pub note: Option<String>,
}

impl Text {
    pub fn line(&self, i: usize) -> &str {
        self.lines.get(i).map_or("", |r| &self.text[r.start as usize..r.end as usize])
    }
}

/// A decoded image.
#[derive(Debug, PartialEq)]
pub struct Picture {
    /// The image's own size, as it shows (after EXIF rotation).
    pub width: u32,
    pub height: u32,
    pub format: Format,
    /// The decoded pixels' size (shrunk to the panel).
    pub shown: [u32; 2],
}

/// Everything else: what the listing says, plus a folder's item count and a note.
#[derive(Debug, PartialEq, Default)]
pub struct Card {
    /// A folder's entries (hidden ones too).
    pub items: Option<usize>,
    pub note: Option<String>,
}

/// A finished preview, for request `req`.
pub(crate) struct Answer {
    req: u64,
    loaded: Loaded,
}

pub(crate) struct Loaded {
    pub body: Body,
    pub link: Option<String>,
    /// For an image: the shrunk pixels, straight-alpha RGBA.
    pub rgba: Option<(u32, u32, Vec<u8>)>,
}

impl Loaded {
    fn body(body: Body) -> Loaded {
        Loaded { body, link: None, rgba: None }
    }
}

/// What the worker needs to know.
#[derive(Clone)]
pub(crate) struct Job {
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_link: bool,
    pub ext: String,
    pub size: u64,
    /// The box (device pixels) an image shrinks to fit.
    pub fit: [u32; 2],
}

impl Preview {
    fn new() -> Preview {
        Preview { target: None, body: Body::Empty, link: None, pixels: Pixels::new(), req: 0, stop: Arc::default() }
    }

    /// Stops the read in flight and arms a fresh flag for the next one.
    fn stop_read(&mut self) -> Arc<AtomicBool> {
        self.stop.store(true, Relaxed);
        self.stop = Arc::default();
        self.stop.clone()
    }

    /// Whether the body is still on its way.
    pub fn is_loading(&self) -> bool {
        self.body == Body::Loading
    }

    /// Points the panel at `target` (or nothing) and starts reading it on `worker`.
    fn retarget(&mut self, worker: &Worker, target: Option<Target>, fit: [u32; 2]) {
        let stop = self.stop_read();
        self.link = None;
        self.pixels = Pixels::new();
        self.target = target;
        let Some(t) = &self.target else {
            worker.cancel();
            self.req = 0;
            self.body = Body::Empty;
            return;
        };
        self.body = Body::Loading;
        let job = Job { path: t.path(), is_dir: t.is_dir, is_link: t.is_link, ext: t.ext(), size: t.size, fit };
        self.req = worker.request(job, stop);
    }

    /// Lands the worker's answer if it's for the request on screen. True when it did.
    fn land(&mut self, worker: &Worker) -> bool {
        let Some(a) = worker.take() else { return false };
        if a.req != self.req {
            return false;
        }
        let Loaded { body, link, rgba } = a.loaded;
        if let Some((w, h, px)) = rgba {
            self.pixels.set(w, h, px);
        }
        self.body = body;
        self.link = link;
        true
    }
}

impl State {
    /// Space: opens the preview on the active pane's selection, or closes it.
    pub fn toggle_preview(&mut self) {
        if self.preview.is_some() {
            self.close_preview();
        } else {
            if self.preview_worker.is_none() {
                self.preview_worker = Some(Worker::spawn(self.waker.clone()));
            }
            self.preview = Some(Preview::new());
            self.sync_preview();
        }
    }

    /// Closes the panel: the read in flight stops, one waiting never starts, and no answer
    /// lands.
    pub fn close_preview(&mut self) {
        self.preview = None;
        if let Some(w) = &self.preview_worker {
            w.cancel();
        }
    }

    /// Keeps the panel on the active pane's selected item, starting a read when it moved.
    /// Called wherever the selection can move: after every key (`on_key`) and command (`run`),
    /// when a listing lands (`land`), when another pane is made active, after a click on a row,
    /// and from `poll` as a catch-all. Not from `poll` alone: Rune skips `poll` in a frame an
    /// earlier wake's poll already asked for, so a key handled in that frame would go
    /// unfollowed. Closed, it's one `None` check. True when the panel changed.
    pub(crate) fn sync_preview(&mut self) -> bool {
        let Some(p) = &mut self.preview else { return false };
        let f = &self.panes[self.active].f;
        let target = match (f.selected(), &p.target) {
            (Some(e), Some(t)) if t.is(f.id, &f.cwd, e) => return false,
            (None, None) if p.body == Body::Empty => return false,
            (e, _) => e.map(|e| Target::of(f.id, &f.cwd, e)),
        };
        if target.is_some() {
            self.preview_reads += 1;
        }
        let fit = self.preview_fit.get();
        let Some(w) = &self.preview_worker else { return false };
        p.retarget(w, target, fit);
        true
    }

    /// The preview's answer, from `poll`. True when one landed.
    pub(crate) fn land_preview(&mut self) -> bool {
        let (Some(p), Some(w)) = (&mut self.preview, &self.preview_worker) else { return false };
        let landed = p.land(w);
        if landed {
            self.preview_lands += 1;
        }
        landed
    }
}

/// The panel's size in device pixels before it has been laid out once: a 2× display's
/// panel of about 500 points.
pub const DEFAULT_FIT: [u32; 2] = [1024, 1024];

/// Where the view writes the panel's laid-out size (device pixels), so the next image decodes
/// to fit it. Geometry, not file data: kept across opens.
pub type Fit = Rc<Cell<[u32; 2]>>;

// ---- the worker ---------------------------------------------------------------------------

/// What one preview reads: `None` when stopped (the selection moved or the panel closed).
pub(crate) fn load(job: &Job, stop: &AtomicBool) -> Option<Loaded> {
    #[cfg(test)]
    let _running = hook::Running::enter(&job.path);
    #[cfg(test)]
    hook::at(&job.path, "open");
    let mut link = None;
    if job.is_link {
        link = Some(match std::fs::read_link(&job.path) {
            Ok(t) => format!("Link to {}", fs::pretty(&t)),
            Err(_) => "Link".into(),
        });
        if std::fs::metadata(&job.path).is_err() {
            return Some(Loaded { body: Body::Error("The link's target is missing".into()), link, rgba: None });
        }
    }
    let mut out = if job.is_dir {
        Loaded::body(count(&job.path, stop)?)
    } else if let Some(what) = platform_kind(&job.ext) {
        Loaded::body(Body::Card(Card { items: None, note: Some(needs_platform(what)) }))
    } else {
        file(job, stop)?
    };
    if stop.load(Relaxed) {
        return None;
    }
    out.link = link;
    #[cfg(test)]
    hook::at(&job.path, "done");
    Some(out)
}

/// A folder's item count: one listing, stopped as soon as the selection moves.
fn count(path: &Path, stop: &AtomicBool) -> Option<Body> {
    let rd = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => return Some(Body::Error(format!("Couldn't read it: {e}"))),
    };
    let mut n = 0;
    for _ in rd {
        if stop.load(Relaxed) {
            return None;
        }
        n += 1;
    }
    Some(Body::Card(Card { items: Some(n), note: None }))
}

/// Formats only the platform's own preview API can show (Quick Look, Windows' preview
/// handlers), by extension, and what to call them.
fn platform_kind(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "pdf" => "PDF",
        "mp4" | "m4v" | "mov" | "avi" | "mkv" | "webm" | "wmv" | "flv" | "mpg" | "mpeg" | "3gp" => "Video",
        "mp3" | "m4a" | "aac" | "wav" | "flac" | "ogg" | "opus" | "aif" | "aiff" | "wma" => "Audio",
        "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp" | "pages" | "numbers" | "key" | "rtf" => {
            "An office document"
        }
        "heic" | "heif" | "avif" | "tif" | "tiff" | "psd" | "dng" | "cr2" | "cr3" | "nef" | "arw" | "raf" | "orf" | "ico" | "icns" => {
            "This image format"
        }
        _ => return None,
    })
}

fn needs_platform(what: &str) -> String {
    format!(
        "{what}: a full preview needs the platform's own preview API (Quick Look, Windows preview handlers), which Arbor doesn't use yet."
    )
}

/// A reader that fails as soon as the stop flag is set, so every decoder reading through it
/// stops at its next read.
struct Stop<'a, R> {
    r: R,
    stop: &'a AtomicBool,
}

const STOPPED: &str = "stopped";

impl<R: Read> Read for Stop<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.stop.load(Relaxed) {
            return Err(std::io::Error::other(STOPPED));
        }
        self.r.read(buf)
    }
}

impl<R: Seek> Seek for Stop<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.r.seek(pos)
    }
}

/// Reads up to `n` bytes (fewer at the end of the file).
fn read_up_to(r: &mut impl Read, n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(n.min(1 << 20));
    r.take(n as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

fn couldnt(e: impl std::fmt::Display) -> Body {
    Body::Error(format!("Couldn't read it: {e}"))
}

/// A file: an image by its first bytes, else text if it looks like text, else a card.
fn file(job: &Job, stop: &AtomicBool) -> Option<Loaded> {
    match std::fs::metadata(&job.path) {
        Err(e) => return Some(Loaded::body(couldnt(e))),
        // A pipe would block the read until something writes to it; a device may never end.
        Ok(m) if !m.is_file() => {
            return Some(Loaded::body(Body::Card(Card {
                items: None,
                note: Some("Not a regular file (a pipe, socket or device): it isn't opened.".into()),
            })));
        }
        Ok(_) => {}
    }
    let f = match std::fs::File::open(&job.path) {
        Ok(f) => f,
        Err(e) => return Some(Loaded::body(couldnt(e))),
    };
    let mut r = Stop { r: f, stop };
    let head = match read_up_to(&mut r, HEAD) {
        Ok(h) => h,
        Err(_) if stop.load(Relaxed) => return None,
        Err(e) => return Some(Loaded::body(couldnt(e))),
    };
    if let Some(format) = sniff_image(&head) {
        if r.seek(SeekFrom::Start(0)).is_err() {
            return Some(Loaded::body(Body::Error("Couldn't read it again from the start".into())));
        }
        #[cfg(test)]
        hook::at(&job.path, "header");
        return match image(format, BufReader::with_capacity(64 * 1024, r), job.fit, stop) {
            Ok(Some((picture, rgba))) => Some(Loaded { body: Body::Image(picture), link: None, rgba: Some(rgba) }),
            Ok(None) => None,
            Err(_) if stop.load(Relaxed) => None,
            Err(e) => Some(Loaded::body(Body::Error(e))),
        };
    }
    if job.size == 0 && head.is_empty() {
        return Some(Loaded::body(Body::Card(Card { items: None, note: Some("Empty file".into()) })));
    }
    if sniff_text(&head).is_none() {
        return Some(Loaded::body(Body::Card(Card {
            items: None,
            note: Some("Binary file: there's no preview for its contents.".into()),
        })));
    }
    let mut bytes = head;
    if bytes.len() == HEAD {
        match read_up_to(&mut r, TEXT_MAX + 1 - HEAD) {
            Ok(more) => bytes.extend_from_slice(&more),
            Err(_) if stop.load(Relaxed) => return None,
            Err(e) => return Some(Loaded::body(couldnt(e))),
        }
    }
    let truncated = bytes.len() > TEXT_MAX;
    bytes.truncate(TEXT_MAX);
    let total = job.size.max(bytes.len() as u64);
    Some(Loaded::body(match text(&bytes, truncated, total) {
        Some(t) => Body::Text(t),
        None => Body::Card(Card { items: None, note: Some("Binary file: there's no preview for its contents.".into()) }),
    }))
}

// ---- text ---------------------------------------------------------------------------------

/// How a file's bytes read as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    /// After a UTF-8 byte order mark.
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    /// Mostly UTF-8 with some invalid bytes (Latin-1, a broken file): shown with U+FFFD.
    Lossy,
}

/// Whether `b` (a file's first bytes, possibly cut mid-character) is text, and how to read
/// it. Not text: NUL bytes (beyond one in a thousand), control characters (over 2%), or
/// invalid UTF-8 (over 10%: accented Latin-1 text stays under it). A UTF-16 byte order mark reads as UTF-16 (it's full of NULs).
pub fn sniff_text(b: &[u8]) -> Option<Encoding> {
    if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Some(Encoding::Utf8Bom);
    }
    if b.starts_with(&[0xFF, 0xFE]) {
        return Some(Encoding::Utf16Le);
    }
    if b.starts_with(&[0xFE, 0xFF]) {
        return Some(Encoding::Utf16Be);
    }
    let nul = b.iter().filter(|&&c| c == 0).count();
    if nul * 1000 > b.len() {
        return None;
    }
    // Control characters other than tab, line breaks, form feed and escape.
    let ctrl = b.iter().filter(|&&c| (c < 0x20 && !matches!(c, b'\t' | b'\n' | b'\r' | 0x0C | 0x1B)) || c == 0x7F).count();
    if ctrl * 50 > b.len() {
        return None;
    }
    let mut bad = 0;
    for chunk in b.utf8_chunks() {
        // An incomplete sequence at the very end is a character cut by the bounded read.
        let cut = chunk.invalid().len() < 4 && chunk.invalid().as_ptr_range().end == b.as_ptr_range().end;
        if !chunk.invalid().is_empty() && !cut {
            bad += chunk.invalid().len();
        }
    }
    if bad == 0 {
        Some(Encoding::Utf8)
    } else if bad * 10 <= b.len() {
        Some(Encoding::Lossy)
    } else {
        None
    }
}

/// Lays out the head of a text file for the panel: decoded, tabs expanded, control
/// characters shown as U+FFFD, lines over [`MAX_COLS`] cut. `total` is the file's size, for the
/// note when `truncated`.
pub fn text(bytes: &[u8], truncated: bool, total: u64) -> Option<Text> {
    let enc = sniff_text(bytes)?;
    let decoded: std::borrow::Cow<str> = match enc {
        Encoding::Utf8 => String::from_utf8_lossy(bytes),
        Encoding::Utf8Bom => String::from_utf8_lossy(&bytes[3..]),
        Encoding::Lossy => String::from_utf8_lossy(bytes),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let units = bytes[2..].as_chunks::<2>().0.iter().map(|c| {
                let pair = [c[0], c[1]];
                if enc == Encoding::Utf16Le { u16::from_le_bytes(pair) } else { u16::from_be_bytes(pair) }
            });
            char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect::<String>().into()
        }
    };
    // A character cut by the bounded read shows as one U+FFFD at the end: drop it.
    let decoded = if truncated && enc == Encoding::Utf8 { decoded.trim_end_matches(char::REPLACEMENT_CHARACTER) } else { &decoded };
    let mut out = String::with_capacity(decoded.len() + decoded.len() / 16);
    let mut lines = Vec::new();
    let (mut cols, mut cut) = (0, false);
    let body = decoded.strip_suffix('\n').unwrap_or(decoded);
    if !body.is_empty() || decoded.is_empty() {
        for line in body.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let start = out.len() as u32;
            let mut col = 0;
            for c in line.chars() {
                if col >= MAX_COLS {
                    out.push('…');
                    cut = true;
                    col += 1;
                    break;
                }
                match c {
                    '\t' => {
                        let to = (col / TAB + 1) * TAB;
                        out.extend(std::iter::repeat_n(' ', to - col));
                        col = to;
                    }
                    c if c.is_control() => {
                        out.push(char::REPLACEMENT_CHARACTER);
                        col += 1;
                    }
                    c => {
                        out.push(c);
                        col += 1;
                    }
                }
            }
            cols = cols.max(col);
            lines.push(start..out.len() as u32);
        }
    }
    if decoded.is_empty() {
        lines.clear();
    }
    let mut notes = Vec::new();
    if truncated {
        notes.push(format!("Showing the first {} of {}", fs::fmt_size(TEXT_MAX as u64), fs::fmt_size(total)));
    }
    match enc {
        Encoding::Lossy => notes.push("Not valid UTF-8: invalid bytes show as �".into()),
        Encoding::Utf16Le | Encoding::Utf16Be => notes.push("UTF-16".into()),
        _ => {}
    }
    if cut {
        notes.push(format!("Lines over {MAX_COLS} characters are cut"));
    }
    Some(Text { text: out, lines, cols, note: (!notes.is_empty()).then(|| notes.join(" · ")) })
}

// ---- images -------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Png => "PNG",
            Format::Jpeg => "JPEG",
            Format::Gif => "GIF",
            Format::Webp => "WebP",
            Format::Bmp => "BMP",
        }
    }
}

/// An image format Arbor decodes, by its first bytes (not its name).
pub fn sniff_image(b: &[u8]) -> Option<Format> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Format::Png)
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(Format::Jpeg)
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some(Format::Gif)
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some(Format::Webp)
    } else if b.len() >= 26 && b.starts_with(b"BM") {
        Some(Format::Bmp)
    } else {
        None
    }
}

/// Why an image of `w` × `h` isn't decoded, when it's too big (checked from its header, before
/// anything is allocated for its pixels). `whole`: the decoder holds the whole image at once.
pub fn too_big(w: u64, h: u64, whole: bool) -> Option<String> {
    let limit = if whole { MAX_PIXELS_WHOLE } else { MAX_PIXELS };
    (w.saturating_mul(h) > limit).then(|| {
        format!(
            "{w} × {h} is {:.0} megapixels: too big to preview (the limit is {} for this format)",
            (w * h) as f64 / 1e6,
            limit / 1_000_000
        )
    })
}

/// The largest size at most `w` × `h` that fits in `fit` keeping the aspect ratio. Never
/// larger than the image itself.
pub fn fit_size(w: u32, h: u32, fit: [u32; 2]) -> (u32, u32) {
    let (bw, bh) = (fit[0].max(1), fit[1].max(1));
    if w <= bw && h <= bh {
        return (w.max(1), h.max(1));
    }
    let k = (bw as f64 / w as f64).min(bh as f64 / h as f64);
    (((w as f64 * k).round() as u32).clamp(1, bw), ((h as f64 * k).round() as u32).clamp(1, bh))
}

type Decoded = (Picture, (u32, u32, Vec<u8>));

/// Decodes an image shrunk to fit `fit`. `Ok(None)` when stopped; `Err` says why it can't show.
fn image<R: BufRead + Seek>(format: Format, r: R, fit: [u32; 2], stop: &AtomicBool) -> Result<Option<Decoded>, String> {
    let bad = |e: &dyn std::fmt::Display| format!("Couldn't decode this {}: {e}", format.name());
    match format {
        Format::Png => png(r, fit, stop).map_err(|e| bad(&e)),
        Format::Jpeg => jpeg(r, fit, stop),
        Format::Gif => gif(r, fit, stop).map_err(|e| bad(&e)),
        Format::Webp => webp(r, fit, stop).map_err(|e| bad(&e)),
        Format::Bmp => bmp(r, fit, stop).map_err(|e| bad(&e)),
    }
}

/// A source image's rows, shrunk to `dw` × `dh` as they arrive by averaging each output
/// pixel's box of source pixels (weighted by alpha, so transparent pixels don't darken edges).
/// A big image never needs to be in memory whole when its decoder gives rows.
pub struct Shrink {
    sh: u32,
    dw: u32,
    dh: u32,
    /// The output column of each source column.
    xmap: Vec<u32>,
    acc: Vec<u64>,
    n: Vec<u32>,
    /// The output row being gathered.
    oy: u32,
    out: Vec<u8>,
    rgba: Vec<u8>,
}

impl Shrink {
    pub fn new(sw: u32, sh: u32, dw: u32, dh: u32) -> Shrink {
        let xmap = (0..sw).map(|x| (x as u64 * dw as u64 / sw as u64) as u32).collect();
        Shrink {
            sh,
            dw,
            dh,
            xmap,
            acc: vec![0; dw as usize * 4],
            n: vec![0; dw as usize],
            oy: 0,
            out: vec![0; dw as usize * dh as usize * 4],
            rgba: Vec::new(),
        }
    }

    /// Source row `y` (rows in order), `channels` bytes a pixel: gray, gray and alpha, RGB or RGBA.
    pub fn row(&mut self, y: u32, px: &[u8], channels: usize) {
        let oy = ((y as u64 * self.dh as u64) / self.sh as u64) as u32;
        if oy != self.oy {
            self.flush();
            self.oy = oy;
        }
        let mut rgba = std::mem::take(&mut self.rgba);
        to_rgba(px, channels, &mut rgba);
        for (p, &o) in rgba.as_chunks::<4>().0.iter().zip(&self.xmap) {
            let (o, a) = (o as usize, p[3] as u64);
            let acc = &mut self.acc[o * 4..o * 4 + 4];
            acc[0] += p[0] as u64 * a;
            acc[1] += p[1] as u64 * a;
            acc[2] += p[2] as u64 * a;
            acc[3] += a;
            self.n[o] += 1;
        }
        self.rgba = rgba;
    }

    fn flush(&mut self) {
        let row = self.oy as usize * self.dw as usize * 4;
        if self.oy >= self.dh {
            return;
        }
        for x in 0..self.dw as usize {
            let (acc, n) = (&mut self.acc[x * 4..x * 4 + 4], self.n[x] as u64);
            if let Some(a) = acc[3].checked_div(n) {
                let out = &mut self.out[row + x * 4..row + x * 4 + 4];
                for c in 0..3 {
                    out[c] = acc[c].checked_div(acc[3]).unwrap_or(0).min(255) as u8;
                }
                out[3] = a as u8;
            }
            acc.fill(0);
            self.n[x] = 0;
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.flush();
        self.out
    }
}

/// One row of `channels`-byte pixels as RGBA.
fn to_rgba(px: &[u8], channels: usize, out: &mut Vec<u8>) {
    out.clear();
    match channels {
        1 => out.extend(px.iter().flat_map(|&l| [l, l, l, 255])),
        2 => out.extend(px.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]])),
        3 => out.extend(px.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255])),
        _ => out.extend_from_slice(px),
    }
}

/// Shrinks a whole decoded image (`channels` bytes a pixel) to fit.
fn shrink_whole(w: u32, h: u32, px: &[u8], channels: usize, fit: [u32; 2], stop: &AtomicBool) -> Option<(u32, u32, Vec<u8>)> {
    let (dw, dh) = fit_size(w, h, fit);
    let mut s = Shrink::new(w, h, dw, dh);
    let stride = w as usize * channels;
    for (y, row) in px.chunks_exact(stride).take(h as usize).enumerate() {
        if y % 64 == 0 && stop.load(Relaxed) {
            return None;
        }
        s.row(y as u32, row, channels);
    }
    Some((dw, dh, s.finish()))
}

fn picture(format: Format, w: u32, h: u32, rgba: (u32, u32, Vec<u8>)) -> Decoded {
    (Picture { width: w, height: h, format, shown: [rgba.0, rgba.1] }, rgba)
}

fn png<R: BufRead + Seek>(r: R, fit: [u32; 2], stop: &AtomicBool) -> Result<Option<Decoded>, String> {
    let mut d = png::Decoder::new_with_limits(r, png::Limits { bytes: MAX_PIXELS_WHOLE as usize * 4 });
    d.set_transformations(png::Transformations::normalize_to_color8());
    // The header only: nothing is allocated for pixels until the size is known to be fine.
    let mut reader = d.read_info().map_err(|e| e.to_string())?;
    let (w, h, interlaced) = {
        let i = reader.info();
        (i.width, i.height, i.interlaced)
    };
    if let Some(why) = too_big(w as u64, h as u64, interlaced) {
        return Err(why);
    }
    let channels = reader.output_color_type().0.samples();
    if interlaced {
        // Adam7 passes don't arrive in row order: decode whole, then shrink.
        let mut buf = vec![0; reader.output_buffer_size().ok_or("too big")?];
        reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        return Ok(shrink_whole(w, h, &buf, channels, fit, stop).map(|px| picture(Format::Png, w, h, px)));
    }
    let (dw, dh) = fit_size(w, h, fit);
    let mut s = Shrink::new(w, h, dw, dh);
    let mut y = 0;
    #[cfg(test)]
    let path = hook::current();
    while let Some(row) = reader.next_row().map_err(|e| e.to_string())? {
        if stop.load(Relaxed) {
            #[cfg(test)]
            hook::rows(&path, y);
            return Ok(None);
        }
        s.row(y, row.data(), channels);
        y += 1;
    }
    #[cfg(test)]
    hook::rows(&path, y);
    Ok(Some(picture(Format::Png, w, h, (dw, dh, s.finish()))))
}

fn jpeg<R: BufRead + Seek>(r: R, fit: [u32; 2], stop: &AtomicBool) -> Result<Option<Decoded>, String> {
    let bad = |e: jpeg_decoder::Error| format!("Couldn't decode this JPEG: {e}");
    let mut d = jpeg_decoder::Decoder::new(r);
    d.read_info().map_err(bad)?;
    let info = d.info().ok_or("Couldn't decode this JPEG: no header")?;
    let (w, h) = (info.width as u32, info.height as u32);
    let progressive = info.coding_process != jpeg_decoder::CodingProcess::DctSequential;
    // Progressive JPEGs keep every coefficient of the whole image while decoding.
    if let Some(why) = too_big(w as u64, h as u64, progressive || info.pixel_format == jpeg_decoder::PixelFormat::L16) {
        return Err(why);
    }
    let orientation = d.exif_data().and_then(image::metadata::Orientation::from_exif_chunk);
    let turned = matches!(
        orientation,
        Some(
            image::metadata::Orientation::Rotate90
                | image::metadata::Orientation::Rotate270
                | image::metadata::Orientation::Rotate90FlipH
                | image::metadata::Orientation::Rotate270FlipH
        )
    );
    // Shown turned, so it's the turned image that fits the panel.
    let want = if turned { [fit[1], fit[0]] } else { fit };
    let (want_w, want_h) = fit_size(w, h, want);
    // The IDCT does most of the shrinking (to the smallest of 1/8, 1/4, 1/2 or 1 that's still
    // at least the size wanted), the box filter the rest.
    let (sw, sh) = d.scale(want_w.min(u16::MAX as u32) as u16, want_h.min(u16::MAX as u32) as u16).map_err(bad)?;
    if stop.load(Relaxed) {
        return Ok(None);
    }
    let px = match d.decode() {
        Ok(px) => px,
        Err(_) if stop.load(Relaxed) => return Ok(None),
        Err(e) => return Err(bad(e)),
    };
    let fmt = d.info().map_or(info.pixel_format, |i| i.pixel_format);
    let (sw, sh) = (sw as u32, sh as u32);
    let (px, channels) = match fmt {
        jpeg_decoder::PixelFormat::L8 => (px, 1),
        jpeg_decoder::PixelFormat::RGB24 => (px, 3),
        // Big-endian 16-bit gray: its high bytes.
        jpeg_decoder::PixelFormat::L16 => (px.as_chunks::<2>().0.iter().map(|p| p[0]).collect(), 1),
        jpeg_decoder::PixelFormat::CMYK32 => (cmyk_to_rgb(&px), 3),
    };
    let Some((dw, dh, rgba)) = shrink_whole(sw, sh, &px, channels, want, stop) else { return Ok(None) };
    let (dw, dh, rgba) = match orientation {
        Some(o) if o != image::metadata::Orientation::NoTransforms => orient(dw, dh, rgba, o),
        _ => (dw, dh, rgba),
    };
    let (w, h) = if turned { (h, w) } else { (w, h) };
    Ok(Some(picture(Format::Jpeg, w, h, (dw, dh, rgba))))
}

fn cmyk_to_rgb(px: &[u8]) -> Vec<u8> {
    px.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| {
            let k = p[3] as u32;
            [p[0], p[1], p[2]].map(|c| (c as u32 * k / 255) as u8)
        })
        .collect()
}

/// Applies an EXIF orientation to RGBA pixels.
fn orient(w: u32, h: u32, rgba: Vec<u8>, o: image::metadata::Orientation) -> (u32, u32, Vec<u8>) {
    let Some(img) = image::RgbaImage::from_raw(w, h, rgba) else { return (w, h, Vec::new()) };
    let mut img = image::DynamicImage::ImageRgba8(img);
    img.apply_orientation(o);
    let img = img.into_rgba8();
    (img.width(), img.height(), img.into_raw())
}

fn gif<R: BufRead + Seek>(r: R, fit: [u32; 2], stop: &AtomicBool) -> Result<Option<Decoded>, String> {
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    if let Some(limit) = std::num::NonZeroU64::new(MAX_PIXELS_WHOLE * 4) {
        opts.set_memory_limit(gif::MemoryLimit::Bytes(limit));
    }
    let mut d = opts.read_info(r).map_err(|e| e.to_string())?;
    let (w, h) = (d.width() as u32, d.height() as u32);
    if let Some(why) = too_big(w as u64, h as u64, true) {
        return Err(why);
    }
    let Some(frame) = d.read_next_frame().map_err(|e| e.to_string())? else { return Err("it has no frames".into()) };
    // The first frame, on the image's canvas (a frame can be smaller and offset).
    let mut canvas = vec![0u8; w as usize * h as usize * 4];
    let (fw, fl, ft) = (frame.width as usize, frame.left as usize, frame.top as usize);
    for (y, row) in frame.buffer.chunks_exact(fw * 4).enumerate() {
        let cy = ft + y;
        if cy >= h as usize || fl >= w as usize {
            continue;
        }
        let n = fw.min(w as usize - fl) * 4;
        let at = (cy * w as usize + fl) * 4;
        canvas[at..at + n].copy_from_slice(&row[..n]);
    }
    Ok(shrink_whole(w, h, &canvas, 4, fit, stop).map(|px| picture(Format::Gif, w, h, px)))
}

fn webp<R: BufRead + Seek>(r: R, fit: [u32; 2], stop: &AtomicBool) -> Result<Option<Decoded>, String> {
    let mut d = image_webp::WebPDecoder::new(r).map_err(|e| e.to_string())?;
    let (w, h) = d.dimensions();
    if let Some(why) = too_big(w as u64, h as u64, true) {
        return Err(why);
    }
    let mut buf = vec![0; d.output_buffer_size().ok_or("too big")?];
    d.read_image(&mut buf).map_err(|e| e.to_string())?;
    let channels = if d.has_alpha() { 4 } else { 3 };
    Ok(shrink_whole(w, h, &buf, channels, fit, stop).map(|px| picture(Format::Webp, w, h, px)))
}

fn bmp<R: BufRead + Seek>(r: R, fit: [u32; 2], stop: &AtomicBool) -> Result<Option<Decoded>, String> {
    use image::ImageDecoder;
    let d = image::codecs::bmp::BmpDecoder::new(r).map_err(|e| e.to_string())?;
    let (w, h) = d.dimensions();
    if let Some(why) = too_big(w as u64, h as u64, true) {
        return Err(why);
    }
    let channels = d.color_type().channel_count() as usize;
    if d.color_type().bytes_per_pixel() as usize != channels {
        return Err("16-bit channels aren't supported".into());
    }
    let mut buf = vec![0; usize::try_from(d.total_bytes()).map_err(|e| e.to_string())?];
    d.read_image(&mut buf).map_err(|e| e.to_string())?;
    Ok(shrink_whole(w, h, &buf, channels, fit, stop).map(|px| picture(Format::Bmp, w, h, px)))
}

/// Test-only: holds a preview's worker at a stage for one path, and records what it did.
#[cfg(test)]
pub(crate) mod hook {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender};

    pub(crate) struct Gate {
        pub path: PathBuf,
        pub stage: &'static str,
        /// Told when the worker reaches the stage; it then waits for `go`.
        pub reached: Sender<()>,
        pub go: Option<Receiver<()>>,
    }

    pub(crate) static GATES: Mutex<Vec<Gate>> = Mutex::new(Vec::new());
    /// Every stage each path reached, in order, and the PNG rows decoded.
    pub(crate) static SEEN: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

    thread_local! {
        static CURRENT: std::cell::RefCell<PathBuf> = const { std::cell::RefCell::new(PathBuf::new()) };
    }

    pub(crate) fn at(p: &Path, stage: &'static str) {
        CURRENT.with(|c| *c.borrow_mut() = p.to_path_buf());
        SEEN.lock().unwrap().push((p.to_path_buf(), stage.to_string()));
        let go = {
            let mut g = GATES.lock().unwrap();
            let Some(i) = g.iter().position(|g| g.path == p && g.stage == stage) else { return };
            let gate = g.remove(i);
            let _ = gate.reached.send(());
            gate.go
        };
        if let Some(go) = go {
            let _ = go.recv();
        }
    }

    pub(crate) fn current() -> PathBuf {
        CURRENT.with(|c| c.borrow().clone())
    }

    pub(crate) fn rows(p: &Path, n: u32) {
        SEEN.lock().unwrap().push((p.to_path_buf(), format!("rows {n}")));
    }

    /// The stages `p` reached.
    pub(crate) fn seen(p: &Path) -> Vec<String> {
        SEEN.lock().unwrap().iter().filter(|(q, _)| q == p).map(|(_, s)| s.clone()).collect()
    }

    /// Reads running now and at most, per folder.
    pub(crate) static RUNNING: Mutex<Vec<(PathBuf, usize, usize)>> = Mutex::new(Vec::new());

    /// One read running, in its file's folder, until dropped.
    pub(crate) struct Running(PathBuf);

    impl Running {
        pub(crate) fn enter(p: &Path) -> Running {
            let dir = p.parent().unwrap_or(p).to_path_buf();
            let mut r = RUNNING.lock().unwrap();
            match r.iter_mut().find(|e| e.0 == dir) {
                Some(e) => {
                    e.1 += 1;
                    e.2 = e.2.max(e.1);
                }
                None => r.push((dir.clone(), 1, 1)),
            }
            Running(dir)
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(e) = RUNNING.lock().unwrap().iter_mut().find(|e| e.0 == self.0) {
                e.1 -= 1;
            }
        }
    }

    /// The most reads that ran at once for files in `dir`.
    pub(crate) fn most_running(dir: &Path) -> usize {
        RUNNING.lock().unwrap().iter().find(|e| e.0 == dir).map_or(0, |e| e.2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::state_in;
    use crate::commands::Cmd;
    use rune::{Key, KeyEvent, Mods, Tx};
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    fn folder(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-preview-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    // ---- sniffing and text ----------------------------------------------------------------

    #[test]
    fn text_is_told_from_binary_by_its_bytes() {
        assert_eq!(sniff_text(b"fn main() {}\n\tok\r\n"), Some(Encoding::Utf8));
        assert_eq!(sniff_text("naïve café ✓".as_bytes()), Some(Encoding::Utf8));
        assert_eq!(sniff_text(b"\xEF\xBB\xBFhello"), Some(Encoding::Utf8Bom));
        assert_eq!(sniff_text(b"\xFF\xFEh\0i\0"), Some(Encoding::Utf16Le));
        assert_eq!(sniff_text(b"\xFE\xFF\0h\0i"), Some(Encoding::Utf16Be));
        // Latin-1: a few invalid bytes in plain text.
        assert_eq!(sniff_text(b"Caf\xe9 cr\xe8me br\xfbl\xe9e and a lot more plain text here"), Some(Encoding::Lossy));
        // A character cut by the bounded read is still UTF-8.
        let cut = "a long line ending in é".as_bytes();
        assert_eq!(sniff_text(&cut[..cut.len() - 1]), Some(Encoding::Utf8));
        // NULs, control characters, or mostly invalid bytes: binary.
        assert_eq!(sniff_text(b"ELF\0\0\0\x01\x02 rest"), None);
        assert_eq!(sniff_text(&(0u8..=255).cycle().take(4096).collect::<Vec<_>>()), None);
        assert_eq!(sniff_text(&[0xC3u8, 0x28, 0xA0, 0xA1, 0xE2, 0x28, 0xA1, 0x41].repeat(100)), None);
        assert_eq!(sniff_text(b""), Some(Encoding::Utf8));
    }

    #[test]
    fn text_lays_out_lines_and_says_what_it_left_out() {
        let t = text(b"one\r\n\ttwo\nthree\n", false, 17).unwrap();
        assert_eq!((0..t.lines.len()).map(|i| t.line(i)).collect::<Vec<_>>(), ["one", "    two", "three"]);
        assert_eq!((t.cols, t.note.clone()), (7, None));
        // A control character shows as U+FFFD; a long line is cut.
        let long = format!("a\x07b\n{}", "x".repeat(MAX_COLS + 50));
        let t = text(long.as_bytes(), false, long.len() as u64).unwrap();
        assert_eq!(t.line(0), "a\u{FFFD}b");
        assert_eq!(t.line(1).chars().count(), MAX_COLS + 1);
        assert!(t.line(1).ends_with('…'));
        assert_eq!(t.note.as_deref(), Some("Lines over 1000 characters are cut"));
        // Truncated, cut mid-character: the note, and no stray U+FFFD at the end.
        let mut bytes = "é".repeat(10).into_bytes();
        bytes.pop();
        let t = text(&bytes, true, 3_400_000).unwrap();
        assert_eq!(t.line(0), "é".repeat(9));
        assert_eq!(t.note.as_deref(), Some("Showing the first 256 KB of 3.4 MB"));
        let t = text(b"Caf\xe9 and a lot more plain text to make it count", false, 40).unwrap();
        assert!(t.line(0).starts_with("Caf\u{FFFD} and"));
        assert_eq!(t.note.as_deref(), Some("Not valid UTF-8: invalid bytes show as �"));
        let t = text(b"\xFF\xFEh\0i\0\n\0", false, 8).unwrap();
        assert_eq!((t.line(0), t.note.as_deref()), ("hi", Some("UTF-16")));
        assert!(text(b"", false, 0).unwrap().lines.is_empty());
        assert!(text(b"\0\0\0\0binary", false, 10).is_none());
    }

    /// A 1 MB text file: only the bounded head is read, and the panel says so.
    #[test]
    fn a_big_text_file_reads_only_its_head() {
        let d = folder("bigtext");
        let p = d.join("big.txt");
        std::fs::write(&p, "0123456789abcde\n".repeat(65_536)).unwrap();
        let job = Job { path: p, is_dir: false, is_link: false, ext: "txt".into(), size: 1_048_576, fit: DEFAULT_FIT };
        let Body::Text(t) = load(&job, &never()).unwrap().body else { panic!("text") };
        assert_eq!(t.lines.len(), TEXT_MAX / 16);
        assert_eq!(t.note.as_deref(), Some("Showing the first 256 KB of 1.0 MB"));
    }

    // ---- images ---------------------------------------------------------------------------

    #[test]
    fn images_are_told_by_their_first_bytes() {
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\n...."), Some(Format::Png));
        assert_eq!(sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(Format::Jpeg));
        assert_eq!(sniff_image(b"GIF89a.."), Some(Format::Gif));
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8 "), Some(Format::Webp));
        assert_eq!(sniff_image(&[b"BM".as_slice(), &[0; 30]].concat()), Some(Format::Bmp));
        assert_eq!(sniff_image(b"BM"), None, "too short for a BMP header");
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WAVEfmt "), None);
        assert_eq!(sniff_image(b"plain text"), None);
    }

    #[test]
    fn size_limits_and_fitting() {
        assert_eq!(too_big(4000, 3000, false), None);
        assert_eq!(too_big(10_000, 10_000, false), None, "100 MP exactly is allowed");
        assert!(too_big(10_001, 10_000, false).unwrap().contains("100 megapixels"));
        assert!(too_big(8000, 7000, true).is_some(), "56 MP decoded whole is over 24");
        assert!(too_big(5000, 5000, true).is_some() && too_big(6000, 4000, true).is_none(), "24 MP decoded whole is the limit");
        assert!(too_big(u32::MAX as u64, u32::MAX as u64, false).is_some(), "no overflow");
        assert_eq!(fit_size(4000, 3000, [1000, 1000]), (1000, 750));
        assert_eq!(fit_size(3000, 4000, [1000, 500]), (375, 500));
        assert_eq!(fit_size(16, 16, [1000, 1000]), (16, 16), "never larger");
        assert_eq!(fit_size(100_000, 1, [100, 100]), (100, 1));
    }

    #[test]
    fn shrinking_averages_boxes_weighted_by_alpha() {
        // 4 × 2 → 2 × 1: each output pixel averages a 2 × 2 box.
        let mut s = Shrink::new(4, 2, 2, 1);
        s.row(0, &[0, 0, 0, 255, 100, 100, 100, 255, 10, 20, 30, 255, 255, 0, 0, 0], 4);
        s.row(1, &[0, 0, 0, 255, 100, 100, 100, 255, 10, 20, 30, 255, 255, 0, 0, 0], 4);
        let out = s.finish();
        assert_eq!(&out[..4], &[50, 50, 50, 255]);
        // Transparent pixels don't darken the colour: they only thin the alpha.
        assert_eq!(&out[4..], &[10, 20, 30, 127]);
        // Gray rows widen to RGBA.
        let mut s = Shrink::new(2, 1, 2, 1);
        s.row(0, &[7, 9], 1);
        assert_eq!(s.finish(), [7, 7, 7, 255, 9, 9, 9, 255]);
    }

    /// A PNG claiming 20,000 × 20,000 (400 MP) is refused from its header: nothing allocated.
    #[test]
    fn a_huge_image_is_refused_from_its_header() {
        let mut bytes = Vec::new();
        {
            let mut e = png::Encoder::new(&mut bytes, 20_000, 20_000);
            e.set_color(png::ColorType::Rgba);
            let mut w = e.write_header().unwrap();
            // The header is read up to the first data chunk.
            w.write_chunk(png::chunk::IDAT, &[0; 8]).unwrap();
            drop(w);
        }
        let err = image(Format::Png, std::io::Cursor::new(bytes), DEFAULT_FIT, &never()).unwrap_err();
        assert!(err.contains("400 megapixels") && err.contains("too big"), "{err}");
    }

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        (0..w * h).flat_map(|i| [(i % w * 255 / w) as u8, (i / w * 255 / h) as u8, 128, 255]).collect()
    }

    /// Writes one image in each format, `w` × `h`, into `d`.
    pub(crate) fn write_images(d: &Path, w: u32, h: u32) -> Vec<(PathBuf, Format)> {
        use image::ImageEncoder;
        let px = gradient(w, h);
        let rgb: Vec<u8> = px.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        let mut out = Vec::new();
        let mut file = |name: &str, f: Format, bytes: Vec<u8>| {
            std::fs::write(d.join(name), bytes).unwrap();
            out.push((d.join(name), f));
        };
        let mut b = Vec::new();
        image::codecs::png::PngEncoder::new(&mut b).write_image(&px, w, h, image::ExtendedColorType::Rgba8).unwrap();
        file("a.png", Format::Png, b);
        let mut b = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut b, 90).write_image(&rgb, w, h, image::ExtendedColorType::Rgb8).unwrap();
        file("b.jpg", Format::Jpeg, b);
        let mut b = Vec::new();
        image::codecs::bmp::BmpEncoder::new(&mut b).write_image(&rgb, w, h, image::ExtendedColorType::Rgb8).unwrap();
        file("c.bmp", Format::Bmp, b);
        let mut b = Vec::new();
        image_webp::WebPEncoder::new(&mut b).encode(&px, w, h, image_webp::ColorType::Rgba8).unwrap();
        file("d.webp", Format::Webp, b);
        let mut b = Vec::new();
        {
            let palette: Vec<u8> = (0..=255u8).flat_map(|i| [i, i, i]).collect();
            let mut e = gif::Encoder::new(&mut b, w as u16, h as u16, &palette).unwrap();
            let idx: Vec<u8> = px.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
            e.write_frame(&gif::Frame::from_indexed_pixels(w as u16, h as u16, idx, None)).unwrap();
        }
        file("e.gif", Format::Gif, b);
        out
    }

    #[test]
    fn every_format_decodes_shrunk_to_the_panel() {
        let d = folder("formats");
        for (path, format) in write_images(&d, 640, 480) {
            let job = Job { path: path.clone(), is_dir: false, is_link: false, ext: String::new(), size: 1, fit: [200, 200] };
            let l = load(&job, &never()).unwrap();
            let Body::Image(p) = l.body else { panic!("{}: {:?}", path.display(), l.body) };
            assert_eq!((p.format, p.width, p.height, p.shown), (format, 640, 480, [200, 150]), "{}", path.display());
            let (w, h, px) = l.rgba.unwrap();
            assert_eq!(((w, h), px.len()), ((200, 150), 200 * 150 * 4));
            // The gradient survives: dark on the left, bright on the right.
            assert!(px[(75 * 200) * 4] < 20 && px[(75 * 200 + 199) * 4] > 230, "{}: {:?}", path.display(), &px[..8]);
        }
        // A small image keeps its size.
        let small = folder("formats-small");
        let (path, _) = write_images(&small, 30, 20).remove(1);
        let job = Job { path, is_dir: false, is_link: false, ext: String::new(), size: 1, fit: [200, 200] };
        let Body::Image(p) = load(&job, &never()).unwrap().body else { panic!() };
        assert_eq!(p.shown, [30, 20]);
    }

    /// A JPEG whose EXIF says "rotate 90°" shows turned: 40 × 20 stored, 20 × 40 shown.
    #[test]
    fn jpeg_exif_orientation_is_respected() {
        use image::ImageEncoder;
        let (w, h) = (40, 20);
        let rgb: Vec<u8> = gradient(w, h).as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        let mut jpg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpg).write_image(&rgb, w, h, image::ExtendedColorType::Rgb8).unwrap();
        // APP1: "Exif\0\0", a little-endian TIFF header, one IFD entry: Orientation (0x0112) = 6.
        let tiff = [
            b"II*\0".as_slice(),
            &8u32.to_le_bytes(),
            &1u16.to_le_bytes(),
            &[0x12, 0x01, 3, 0],
            &1u32.to_le_bytes(),
            &[6, 0, 0, 0],
            &0u32.to_le_bytes(),
        ]
        .concat();
        let body = [b"Exif\0\0".as_slice(), &tiff].concat();
        let app1 = [&[0xFF, 0xE1][..], &((body.len() + 2) as u16).to_be_bytes(), &body].concat();
        let bytes = [&jpg[..2], &app1, &jpg[2..]].concat();
        let (p, (dw, dh, _)) = image(Format::Jpeg, std::io::Cursor::new(bytes), [100, 100], &never()).unwrap().unwrap();
        assert_eq!((p.width, p.height, dw, dh), (20, 40, 20, 40));
    }

    #[test]
    fn a_corrupt_image_says_so() {
        let d = folder("corrupt");
        let p = d.join("broken.png");
        std::fs::write(&p, b"\x89PNG\r\n\x1a\nthis is not a png at all").unwrap();
        let job = Job { path: p, is_dir: false, is_link: false, ext: "png".into(), size: 30, fit: DEFAULT_FIT };
        let Body::Error(why) = load(&job, &never()).unwrap().body else { panic!() };
        assert!(why.starts_with("Couldn't decode this PNG"), "{why}");
    }

    // ---- the panel in the app ---------------------------------------------------------------

    fn key(s: &mut State, k: Key) -> bool {
        s.on_key(&KeyEvent { key: k, mods: Mods::default() }, &mut Tx::default())
    }

    fn space(s: &mut State) -> bool {
        key(s, Key::Text(" ".into()))
    }

    /// Polls as frames would until the preview's body lands (or 10 s pass).
    fn settle(s: &mut State) {
        let until = Instant::now() + Duration::from_secs(10);
        while s.preview.as_ref().is_some_and(Preview::is_loading) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll(&mut Tx::default());
        }
        assert!(!s.preview.as_ref().is_some_and(Preview::is_loading), "never landed");
    }

    fn shown(s: &State) -> Option<&str> {
        s.preview.as_ref()?.target.as_ref().map(|t| t.name.as_str())
    }

    fn seen_open(p: &Path) -> usize {
        hook::seen(p).iter().filter(|s| *s == "open").count()
    }

    /// Closed, the preview does nothing at all: moving through files reads none of them.
    #[test]
    fn a_closed_preview_does_no_work() {
        let d = folder("closed");
        for n in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(d.join(n), "hello").unwrap();
        }
        let mut s = state_in(&d);
        for k in [Key::Down, Key::Down, Key::Up, Key::End, Key::Home] {
            key(&mut s, k);
            s.poll(&mut Tx::default());
        }
        s.run(Cmd::Reload, &mut Tx::default());
        crate::app::tests::settle_all(&mut s);
        assert!(s.preview.is_none());
        assert_eq!((s.preview_reads, s.preview_lands), (0, 0));
        for n in ["a.txt", "b.txt", "c.txt"] {
            assert!(hook::seen(&d.join(n)).is_empty(), "{n} was read");
        }
    }

    /// Space opens it on the selection, and it follows the selection; moving back reads again.
    #[test]
    fn space_opens_it_and_it_follows_the_selection() {
        let d = folder("follow");
        std::fs::write(d.join("a.txt"), "alpha\nbeta\n").unwrap();
        std::fs::create_dir(d.join("dir")).unwrap();
        for i in 0..3 {
            std::fs::write(d.join("dir").join(format!("f{i}")), "").unwrap();
        }
        let mut s = state_in(&d);
        assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("dir"));
        assert!(space(&mut s));
        assert_eq!(shown(&s), Some("dir"), "the header shows at once");
        settle(&mut s);
        assert_eq!(s.preview.as_ref().unwrap().body, Body::Card(Card { items: Some(3), note: None }));
        key(&mut s, Key::Down);
        assert_eq!(shown(&s), Some("a.txt"));
        assert!(s.preview.as_ref().unwrap().is_loading(), "the list moved; the body follows");
        settle(&mut s);
        let Body::Text(t) = &s.preview.as_ref().unwrap().body else { panic!() };
        assert_eq!((t.line(0), t.line(1)), ("alpha", "beta"));
        // Back and forth: nothing is kept, so each visit reads again.
        key(&mut s, Key::Up);
        settle(&mut s);
        key(&mut s, Key::Down);
        settle(&mut s);
        assert_eq!(seen_open(&d.join("a.txt")), 2);
        assert!(space(&mut s));
        assert!(s.preview.is_none(), "Space again closes it");
    }

    /// While a filter has text, Space types a space (names have spaces); with nothing typed it
    /// toggles the preview (a filter never starts with a space).
    #[test]
    fn space_types_into_a_filter() {
        let d = folder("filter-space");
        std::fs::write(d.join("my notes.txt"), "x").unwrap();
        let mut s = state_in(&d);
        key(&mut s, Key::Text("m".into()));
        key(&mut s, Key::Text("y".into()));
        assert!(space(&mut s));
        assert_eq!(s.cur().filter, "my ");
        assert!(s.preview.is_none());
        key(&mut s, Key::Text("n".into()));
        assert_eq!(s.cur().visible.len(), 1);
    }

    /// Esc: the palette first, then the preview, then the filter.
    #[test]
    fn escape_closes_the_palette_then_the_preview_then_clears_the_filter() {
        let d = folder("escape");
        std::fs::write(d.join("x1"), "x").unwrap();
        std::fs::write(d.join("x2"), "x").unwrap();
        let mut s = state_in(&d);
        key(&mut s, Key::Text("x".into()));
        s.run(Cmd::TogglePreview, &mut Tx::default());
        s.run(Cmd::Palette, &mut Tx::default());
        assert!(key(&mut s, Key::Escape));
        assert!(s.palette.is_none() && s.preview.is_some(), "the palette closes first");
        assert!(key(&mut s, Key::Escape));
        assert!(s.preview.is_none(), "then the preview");
        assert_eq!(s.cur().filter, "x", "the filter is still there");
        assert!(key(&mut s, Key::Escape));
        assert_eq!(s.cur().filter, "");
        // Toggle Preview needs a selection; Close Preview needs it open.
        assert_eq!(s.unavailable(Cmd::ClosePreview), Some("The preview isn't open"));
        let empty = folder("escape-empty");
        let s = state_in(&empty);
        assert_eq!(s.unavailable(Cmd::TogglePreview), Some("Select an item first"));
    }

    /// The selection moves while an image is being decoded: the decode stops (no rows decoded)
    /// and its result never lands; the new selection's does.
    #[test]
    fn moving_on_during_a_decode_stops_it_and_drops_its_result() {
        let d = folder("cancel");
        let imgs = write_images(&d, 400, 300);
        let png = imgs[0].0.clone(); // a.png, then b.jpg
        let mut s = state_in(&d);
        assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("a.png"));
        let ((reached, wait), (go, gate)) = (channel(), channel());
        hook::GATES.lock().unwrap().push(hook::Gate { path: png.clone(), stage: "header", reached, go: Some(gate) });
        space(&mut s);
        wait.recv_timeout(Duration::from_secs(5)).expect("the decode started");
        key(&mut s, Key::Down);
        assert_eq!(shown(&s), Some("b.jpg"));
        go.send(()).unwrap();
        settle(&mut s);
        let p = s.preview.as_ref().unwrap();
        assert!(matches!(&p.body, Body::Image(i) if i.format == Format::Jpeg), "{:?}", p.body);
        assert_eq!(s.preview_lands, 1, "only the JPEG landed");
        // Stopped at its next read, before decoding a single row.
        let seen = hook::seen(&png);
        assert!(seen.contains(&"header".to_string()) && !seen.contains(&"done".to_string()), "{seen:?}");
        assert!(!seen.iter().any(|s| s.starts_with("rows ") && s != "rows 0"), "{seen:?}");
    }

    /// Space tapped fast while a decode is held: reopening and moving on queue new requests,
    /// each replacing the last, and none runs beside the held one; only the newest runs after
    /// it. The decoder threads of earlier opens never pile up.
    #[test]
    fn reopening_never_runs_two_decodes_at_once() {
        let d = folder("reopen");
        let imgs = write_images(&d, 400, 300);
        let png = imgs[0].0.clone(); // a.png, then b.jpg, c.bmp, d.webp, e.gif
        let mut s = state_in(&d);
        let ((reached, wait), (go, gate)) = (channel(), channel());
        hook::GATES.lock().unwrap().push(hook::Gate { path: png.clone(), stage: "header", reached, go: Some(gate) });
        space(&mut s);
        wait.recv_timeout(Duration::from_secs(5)).expect("the decode started");
        for _ in 0..4 {
            space(&mut s); // close
            space(&mut s); // open again, on a.png
            key(&mut s, Key::Down); // b.jpg
            key(&mut s, Key::Up); // a.png
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(hook::most_running(&d), 1, "a second read ran beside the held one");
        go.send(()).unwrap();
        settle(&mut s);
        let body = &s.preview.as_ref().unwrap().body;
        assert!(matches!(body, Body::Image(i) if i.format == Format::Png), "{body:?}");
        assert_eq!(hook::most_running(&d), 1);
        // Only the newest waiting request ran: the JPEG's were always replaced before they
        // started, and the PNG ran twice (the held read, then the newest request).
        assert_eq!(seen_open(&imgs[1].0), 0, "a replaced request ran");
        assert_eq!(seen_open(&png), 2);
    }

    /// Closing during a read: it stops, and nothing lands afterwards.
    #[test]
    fn closing_during_a_read_drops_it() {
        let d = folder("close-read");
        std::fs::write(d.join("a.txt"), "hello").unwrap();
        let p = d.join("a.txt");
        let mut s = state_in(&d);
        let ((reached, wait), (go, gate)) = (channel(), channel());
        hook::GATES.lock().unwrap().push(hook::Gate { path: p.clone(), stage: "open", reached, go: Some(gate) });
        space(&mut s);
        wait.recv_timeout(Duration::from_secs(5)).expect("the read started");
        key(&mut s, Key::Escape);
        assert!(s.preview.is_none());
        go.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!s.poll(&mut Tx::default()) && s.preview.is_none());
        assert_eq!(s.preview_lands, 0);
        assert!(!hook::seen(&p).contains(&"done".to_string()), "stopped before it finished");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_previews_its_target_and_says_it_is_a_link() {
        let d = folder("link");
        std::fs::write(d.join("target.txt"), "through the link").unwrap();
        std::os::unix::fs::symlink(d.join("target.txt"), d.join("a-link")).unwrap();
        std::os::unix::fs::symlink(d.join("gone"), d.join("b-broken")).unwrap();
        let mut s = state_in(&d);
        space(&mut s);
        settle(&mut s);
        let p = s.preview.as_ref().unwrap();
        assert!(matches!(&p.body, Body::Text(t) if t.line(0) == "through the link"));
        assert!(p.link.as_deref().is_some_and(|l| l.starts_with("Link to ") && l.ends_with("target.txt")), "{:?}", p.link);
        key(&mut s, Key::Down);
        settle(&mut s);
        assert_eq!(s.preview.as_ref().unwrap().body, Body::Error("The link's target is missing".into()));
    }

    /// A FIFO would block its reader until something writes to it: it's never opened.
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_not_opened() {
        let d = folder("fifo");
        let p = d.join("pipe");
        let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let job = Job { path: p, is_dir: false, is_link: false, ext: String::new(), size: 0, fit: DEFAULT_FIT };
        let Body::Card(c) = load(&job, &never()).unwrap().body else { panic!() };
        assert!(c.note.unwrap().starts_with("Not a regular file"));
    }

    #[test]
    fn unreadable_and_platform_only_files_get_a_card() {
        let d = folder("cards");
        std::fs::write(d.join("a.pdf"), "%PDF-1.4").unwrap();
        std::fs::write(d.join("b.bin"), [0u8, 1, 2, 3, 0, 0, 0, 9]).unwrap();
        std::fs::write(d.join("c.txt"), "soon gone").unwrap();
        let mut s = state_in(&d);
        space(&mut s);
        settle(&mut s);
        let Body::Card(c) = &s.preview.as_ref().unwrap().body else { panic!() };
        assert!(c.note.as_deref().unwrap().starts_with("PDF: a full preview needs the platform's own preview API"));
        assert!(hook::seen(&d.join("a.pdf")).contains(&"open".to_string()));
        key(&mut s, Key::Down);
        settle(&mut s);
        assert!(matches!(&s.preview.as_ref().unwrap().body, Body::Card(c) if c.note.as_deref().unwrap().starts_with("Binary file")));
        std::fs::remove_file(d.join("c.txt")).unwrap();
        key(&mut s, Key::Down);
        settle(&mut s);
        assert!(matches!(&s.preview.as_ref().unwrap().body, Body::Error(e) if e.starts_with("Couldn't read it")));
    }

    /// The panel follows the active pane: switching panes previews the other pane's selection.
    #[test]
    fn it_follows_the_active_pane() {
        let (a, b) = (folder("pane-a"), folder("pane-b"));
        std::fs::write(a.join("in-a.txt"), "a").unwrap();
        std::fs::write(b.join("in-b.txt"), "b").unwrap();
        let mut s = state_in(&a);
        s.run(Cmd::Split, &mut Tx::default());
        s.go_to(b.clone());
        crate::app::tests::settle_all(&mut s);
        space(&mut s);
        assert_eq!(shown(&s), Some("in-b.txt"));
        s.run(Cmd::OtherPane, &mut Tx::default());
        assert_eq!(shown(&s), Some("in-a.txt"));
        settle(&mut s);
    }
}
