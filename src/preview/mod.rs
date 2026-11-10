//! Quick preview (NORTHSTAR Core: "on demand, no cache"). Space opens a panel showing the
//! active pane's selected item: the head of a text file, an image shrunk to the panel, the OS's
//! own thumbnail of anything else it can show (PDF, video, office documents, fonts: Quick Look
//! on macOS, the shell's thumbnail cache on Windows; see `thumb.rs`), or a card of what Arbor
//! knows about it (a folder's item count).
//!
//! It's view-triggered background work, so it follows every NORTHSTAR rule for that:
//!
//! - **Closed costs nothing.** `State::preview` is `None`: no reads, no decoding, nothing per
//!   key. `State::sync_preview` is one `None` check in `poll`.
//! - **First paint never waits.** The panel's header (name, kind, size, date, all from the
//!   listing) shows in the key's own frame; the body lands from a worker when it's ready.
//! - **Scoped to what's on screen.** A selection change or closing the panel sets the read's
//!   stop flag, which every read call and every decoded row checks, and an older answer
//!   never lands (each request has its own number). A request to the OS's thumbnailer is
//!   waited on with a [`Bell`] the UI rings with the stop, so it's cancelled at once.
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
//!   its header before its pixels are allocated. Formats decoded whole (BMP, interlaced PNG,
//!   progressive JPEG, interlaced GIF) are refused over [`MAX_PIXELS_WHOLE`]. GIF decodes a
//!   chunk of rows at a time and looks at the stop flag between them. WebP can't be stopped
//!   once it starts (image-webp has no incremental decoding), so it's held to
//!   [`MAX_PIXELS_WEBP`]: an exception to the view-triggered work rule, signed off by the owner
//!   (NORTHSTAR.md, Exceptions log).

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
use crate::thumb::{self, Bell, Thumb};

mod decode;
#[cfg(test)]
mod tests;
mod text;
mod worker;

pub use decode::*;
pub use text::*;
pub use worker::Worker;

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
/// The most pixels for formats decoded whole before shrinking (BMP, interlaced PNG and GIF,
/// progressive and 16-bit JPEG). Their decoders run to the end once started (the stop flag is
/// only seen between reads, and a small file of a big image is read in one go), so this bounds
/// how long and how big a decode that can't be cancelled is: 24 MP (6000 × 4000, a full-frame
/// camera's size) is 96 MB of RGBA, plus as much again for a GIF's canvas, and about 200 ms to
/// decode a GIF on this class of machine (a 49 MP one took 423 ms). Serialized, one at a time,
/// that's at most about 200 MB for a moment on an 8 GB machine, however fast Space is tapped.
pub const MAX_PIXELS_WHOLE: u64 = 24_000_000;
/// WebP decodes whole, and image-webp has no way to stop partway (no incremental decoding: a
/// lossy image's data is read up front, and a well-compressed one is in the first read), so a
/// decode that has started runs to the end after it's told to stop, and its result is dropped.
/// That breaks the view-triggered work rule's "cancels it immediately", by an exception the
/// owner signed off (NORTHSTAR.md, Exceptions log): lossy, lossless and animated WebP up to
/// 4 MP (2309 × 1732 at 4:3), "Too large to preview" above (the owner lowered it from 12 MP).
/// Measured on tabot-dev (Xeon 8375C, `webp_decode_speed`), the slowest 4 MP images (random
/// pixels) are in BENCH.md. The UI
/// thread is never involved: one decode at a time on the preview's worker, and a newer request
/// replaces any waiting one.
pub const MAX_PIXELS_WEBP: u64 = 4_000_000;
/// The widest or tallest image decoded: JPEG's own limit. A pixel limit alone lets a
/// 99,999,999 × 1 image through, whose rows and column map would take gigabytes; with each side
/// at most this, a row is at most 512 KB (16-bit RGBA) and the column map 256 KB.
pub const MAX_SIDE: u64 = 65_535;
/// The most metadata (APPn and comment segments) a JPEG may carry. The decoder keeps every ICC
/// profile segment it meets, so thousands of them would pile up in memory before any pixel
/// limit is checked; real files carry a few hundred KB at most (an ICC profile, EXIF with a
/// thumbnail, XMP).
pub const MAX_JPEG_META: u64 = 16 << 20;
/// What every "too big" refusal starts with: the panel shows it as a card, not an error.
pub const TOO_LARGE: &str = "Too large to preview";

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
            // It takes Get Info's place.
            self.info = None;
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
        // Get Info's panel follows the selection the same way.
        let info = self.sync_info();
        let Some(p) = &mut self.preview else { return info };
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

/// [`load_with`], for tests that don't stop an OS request midway.
#[cfg(test)]
pub(crate) fn load(job: &Job, stop: &AtomicBool) -> Option<Loaded> {
    load_with(job, stop, &Arc::default())
}

/// What one preview reads: `None` when stopped (the selection moved or the panel closed).
/// `bell` is rung with every stop, for a wait on the OS's thumbnailer.
pub(crate) fn load_with(job: &Job, stop: &AtomicBool, bell: &Arc<Bell>) -> Option<Loaded> {
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
        // Straight to the OS: these are never text or an image Arbor decodes (and a PDF's first
        // bytes can pass for text).
        match os_preview(job, stop, bell) {
            Some(Thumb::Image(px)) => thumbnail(px),
            Some(Thumb::Stopped) => return None,
            // The OS has nothing to show (a broken file, no thumbnailer for it, out of time): the
            // card, saying so, never a blank panel.
            Some(Thumb::Nothing) => Loaded::body(Body::Card(Card { items: None, note: Some(CANT.into()) })),
            None => Loaded::body(Body::Card(Card { items: None, note: Some(no_preview_here(what)) })),
        }
    } else {
        file(job, stop, bell)?
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

/// Formats that go straight to the OS's thumbnailer, by extension, and what to call them.
/// Anything else that isn't text or an image Arbor decodes (found by its bytes) goes to it too,
/// after Arbor's own look at its first bytes; this list is for what that look could take for
/// text (PDF, RTF) and for the card's note where the OS has no thumbnailer. Extensions that are
/// also common text files (`.key` for Keynote and for keys, `.ts` for video and TypeScript)
/// aren't here: their bytes decide.
pub(crate) fn platform_kind(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "pdf" => "PDF",
        "mp4" | "m4v" | "mov" | "avi" | "mkv" | "webm" | "wmv" | "flv" | "mpg" | "mpeg" | "3gp" => "Video",
        "mp3" | "m4a" | "aac" | "wav" | "flac" | "ogg" | "opus" | "aif" | "aiff" | "wma" => "Audio",
        "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp" | "pages" | "numbers" | "rtf" | "epub" => "A document",
        "ttf" | "otf" | "ttc" | "woff" | "woff2" => "A font",
        "heic" | "heif" | "avif" | "tif" | "tiff" | "psd" | "dng" | "cr2" | "cr3" | "nef" | "arw" | "raf" | "orf" | "ico" | "icns" => {
            "This image format"
        }
        _ => return None,
    })
}

/// The card's note when the OS's thumbnailer has nothing for a file it should know.
const CANT: &str = "Can't preview this file.";

/// The card's note where the OS has no thumbnailer Arbor uses (Linux).
fn no_preview_here(what: &str) -> String {
    format!("{what}: no preview on this system. Its thumbnailers keep a thumbnail cache, and Arbor keeps no caches.")
}

/// The OS's thumbnail for the job's file: `None` where the OS has no thumbnailer Arbor uses.
fn os_preview(job: &Job, stop: &AtomicBool, bell: &Arc<Bell>) -> Option<Thumb> {
    #[cfg(test)]
    {
        hook::at(&job.path, "os");
        if let Some(t) = thumb::fake::answer(&job.path, stop, bell) {
            return Some(t);
        }
    }
    thumb::thumbnail(&job.path, job.fit, stop, bell)
}

/// An OS thumbnail, shown as an image at its own size (the OS fitted it to the panel).
fn thumbnail((w, h, px): thumb::Rgba) -> Loaded {
    Loaded {
        body: Body::Image(Picture { width: w, height: h, format: Format::Thumbnail, shown: [w, h] }),
        link: None,
        rgba: Some((w, h, px)),
    }
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

/// Opens `p` only if it's a regular file, without ever blocking on the open: on Unix with
/// `O_NONBLOCK` (a FIFO swapped in after the check would otherwise wait for a writer), then
/// the opened handle's own type is checked, and blocking reads are turned back on. `None`
/// when it isn't a regular file.
fn open_regular(p: &Path) -> std::io::Result<Option<std::fs::File>> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NONBLOCK);
    let f = o.open(p)?;
    if !f.metadata()?.is_file() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = f.as_raw_fd();
        // SAFETY: `fd` is open for the life of `f`; F_GETFL and F_SETFL only change its flags.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
        }
    }
    Ok(Some(f))
}

/// A file: an image by its first bytes, else text if it looks like text, else the OS's
/// thumbnail, else a card.
fn file(job: &Job, stop: &AtomicBool, bell: &Arc<Bell>) -> Option<Loaded> {
    // A pipe would block the read until something writes to it; a device may never end.
    let not_regular = || {
        Loaded::body(Body::Card(Card { items: None, note: Some("Not a regular file (a pipe, socket or device): it isn't opened.".into()) }))
    };
    match std::fs::metadata(&job.path) {
        Err(e) => return Some(Loaded::body(couldnt(e))),
        // Not even opened (opening some devices does something).
        Ok(m) if !m.is_file() => return Some(not_regular()),
        Ok(_) => {}
    }
    #[cfg(test)]
    hook::at(&job.path, "before-open");
    // It can be swapped for something else since: the open checks again.
    let f = match open_regular(&job.path) {
        Ok(Some(f)) => f,
        Ok(None) => return Some(not_regular()),
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
            Err(e) if e.starts_with(TOO_LARGE) => Some(Loaded::body(Body::Card(Card { items: None, note: Some(e) }))),
            Err(e) => Some(Loaded::body(Body::Error(e))),
        };
    }
    if job.size == 0 && head.is_empty() {
        return Some(Loaded::body(Body::Card(Card { items: None, note: Some("Empty file".into()) })));
    }
    // The head was cut at its limit if it filled it (the file may go on).
    if sniff_text(&head, head.len() == HEAD).is_none() {
        drop(r);
        return match os_preview(job, stop, bell) {
            Some(Thumb::Image(px)) => Some(thumbnail(px)),
            Some(Thumb::Stopped) => None,
            Some(Thumb::Nothing) | None => {
                Some(Loaded::body(Body::Card(Card { items: None, note: Some("Binary file: there's no preview for its contents.".into()) })))
            }
        };
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

/// An image file shrunk to `fit` (device pixels) for the icon view's thumbnail: the preview's
/// own decoders, with the same caps (a too-large image, and WebP over its 4 MP, gives `None`, as
/// does anything that isn't an image Arbor decodes, or a stop). Only regular files are opened,
/// without blocking, and each read checks `stop`.
pub(crate) fn image_thumbnail(path: &Path, fit: [u32; 2], stop: &AtomicBool) -> Option<thumb::Rgba> {
    let f = open_regular(path).ok()??;
    let mut r = Stop { r: f, stop };
    let head = read_up_to(&mut r, HEAD).ok()?;
    let format = sniff_image(&head)?;
    r.seek(SeekFrom::Start(0)).ok()?;
    match image(format, BufReader::with_capacity(64 * 1024, r), fit, stop) {
        Ok(Some((_, rgba))) if !stop.load(Relaxed) => Some(rgba),
        _ => None,
    }
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

    /// Paths whose read panics at its start, as a decoder with a bug would.
    pub(crate) static PANICS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    pub(crate) fn at(p: &Path, stage: &'static str) {
        if stage == "open" && PANICS.lock().unwrap().iter().any(|q| q == p) {
            panic!("a decoder bug, for the test");
        }
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
