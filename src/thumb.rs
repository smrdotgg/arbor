//! Rich previews from the OS's own thumbnailer, for what Arbor's own decoders don't read: PDF,
//! video (a still frame), audio (its artwork), office and iWork documents, fonts, HEIC, RAW…
//!
//! - **macOS:** Quick Look's `QLThumbnailGenerator`, asked for the panel's size in device pixels
//!   and its best thumbnail (never a generic icon). Its answer arrives on one of Quick Look's
//!   threads; the preview's worker waits for it or for a stop, whichever comes first, on a
//!   [`Bell`], and a stop calls `cancelRequest:` (true cancellation) and drops any late answer.
//! - **Windows:** the shell's `IShellItemImageFactory::GetImage`, on the preview's worker (COM
//!   initialised once on that thread). It can't be cancelled once it starts, so only the shell's
//!   own thumbnail cache is asked (`SIIGBF_INCACHEONLY`: what Explorer already made; instant or
//!   nothing). Full generation is behind [`UNCANCELLABLE_WINDOWS_THUMBNAILS`], off until the owner
//!   decides on a NORTHSTAR exception.
//! - **Linux:** nothing. Its thumbnailers write their results to a cache, which Arbor doesn't
//!   keep (NORTHSTAR: no caching).
//!
//! Arbor stores nothing. The OS may keep the thumbnails it makes in its own system cache (the one
//! Finder and Explorer fill); the owner approved that as the OS's business (DECISIONS.md).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// Shrunk pixels: width, height and straight-alpha RGBA.
pub type Rgba = (u32, u32, Vec<u8>);

/// What the OS's thumbnailer gave.
#[derive(Debug, PartialEq)]
#[cfg_attr(not(any(target_os = "macos", windows, test)), allow(dead_code))]
pub enum Thumb {
    Image(Rgba),
    /// It has no thumbnail for this file (or failed): the panel shows the metadata card.
    Nothing,
    /// The selection moved or the panel closed first: the request was cancelled.
    Stopped,
}

/// Full shell thumbnail generation on Windows (`GetImage` without `SIIGBF_INCACHEONLY`). It runs
/// to the end once started (a video's frame, a PDF's page, in the thumbnail handler's own
/// process), which breaks NORTHSTAR's "cancels it immediately" and holds up the next preview
/// behind it, so it stays off until the owner signs off an exception. Off, Windows shows only
/// thumbnails the shell already has cached.
#[cfg_attr(not(windows), allow(dead_code))]
pub const UNCANCELLABLE_WINDOWS_THUMBNAILS: bool = false;

/// The OS's thumbnail for `path`, at most `fit` (device pixels). `None` when this OS has no
/// thumbnailer Arbor uses (Linux). `stop` ends the wait as soon as it's set and `bell` rung.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn thumbnail(path: &Path, fit: [u32; 2], stop: &AtomicBool, bell: &Arc<Bell>) -> Option<Thumb> {
    if stop.load(Relaxed) {
        return Some(Thumb::Stopped);
    }
    #[cfg(target_os = "macos")]
    return Some(mac::thumbnail(path, fit, stop, bell));
    #[cfg(windows)]
    return Some(win::thumbnail(path, fit, stop, UNCANCELLABLE_WINDOWS_THUMBNAILS));
    #[cfg(not(any(target_os = "macos", windows)))]
    None
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Wakes a thread waiting for an OS answer or a stop, without polling. Whoever changes what the
/// waiter looks at (an answer, a stop flag) does so first, then rings.
#[derive(Default)]
pub struct Bell {
    m: Mutex<()>,
    cv: Condvar,
}

impl Bell {
    pub fn ring(&self) {
        let _g = lock(&self.m);
        self.cv.notify_all();
    }

    /// Returns once `done()` is true. It's checked with the bell's lock held, and a ringer takes
    /// that lock, so a ring after the check can't be missed.
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    pub fn wait_until(&self, done: impl Fn() -> bool) {
        let mut g = lock(&self.m);
        while !done() {
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// An OS request in flight.
#[cfg(any(target_os = "macos", test))]
pub(crate) trait Pending {
    fn cancel(&self);
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq)]
pub(crate) enum Waited<T> {
    Answer(T),
    Stopped,
    /// The request couldn't be made.
    Failed,
}

/// The answer handed to an OS completion handler (any thread, at most once).
#[cfg(any(target_os = "macos", test))]
pub(crate) type Done<T> = Box<dyn FnOnce(T) + Send>;

/// Starts an asynchronous OS request with `start` (which gets the function its completion
/// handler calls) and waits for its answer or a stop. Stopped first: the request is cancelled,
/// and an answer that comes later is dropped.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn wait<T: Send + 'static>(
    stop: &AtomicBool,
    bell: &Arc<Bell>,
    start: impl FnOnce(Done<T>) -> Option<Box<dyn Pending>>,
) -> Waited<T> {
    let slot: Arc<Mutex<Option<T>>> = Arc::default();
    let (answer, ring) = (slot.clone(), bell.clone());
    let Some(pending) = start(Box::new(move |t| {
        *lock(&answer) = Some(t);
        ring.ring();
    })) else {
        return Waited::Failed;
    };
    bell.wait_until(|| stop.load(Relaxed) || lock(&slot).is_some());
    if stop.load(Relaxed) {
        pending.cancel();
        return Waited::Stopped;
    }
    lock(&slot).take().map_or(Waited::Failed, Waited::Answer)
}

/// Premultiplied RGBA (or BGRA) to straight alpha, in place.
#[cfg_attr(not(any(target_os = "macos", windows, test)), allow(dead_code))]
fn unpremultiply(px: &mut [u8]) {
    for p in px.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

// ---- macOS: Quick Look --------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use block2::RcBlock;
    use objc2::AllocAnyThread;
    use objc2::rc::{Retained, autoreleasepool};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{
        CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo, CGInterpolationQuality,
        kCGColorSpaceSRGB,
    };
    use objc2_foundation::{NSError, NSURL};
    use objc2_quick_look_thumbnailing::{
        QLThumbnailGenerationRequest, QLThumbnailGenerationRequestRepresentationTypes as Types, QLThumbnailGenerator,
        QLThumbnailRepresentation,
    };

    struct Request {
        generator: Retained<QLThumbnailGenerator>,
        request: Retained<QLThumbnailGenerationRequest>,
    }

    impl Pending for Request {
        fn cancel(&self) {
            // SAFETY: a request this generator was asked to run; cancelling a finished one is a
            // no-op.
            unsafe { self.generator.cancelRequest(&self.request) };
            #[cfg(test)]
            CANCELS.fetch_add(1, Relaxed);
        }
    }

    /// Requests cancelled, and the error code of the last completion (5 is "cancelled"): what
    /// the tests check.
    #[cfg(test)]
    pub(crate) static CANCELS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    #[cfg(test)]
    pub(crate) static LAST_CODE: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(-1);

    /// The completion handler's image (if any), retained to be turned into pixels on the worker.
    type Answer = Option<Retained<CGImage>>;

    pub(super) fn thumbnail(path: &Path, fit: [u32; 2], stop: &AtomicBool, bell: &Arc<Bell>) -> Thumb {
        // The worker isn't a Cocoa thread: anything Quick Look autoreleases here goes when this
        // request is done, not when the thread ends.
        autoreleasepool(|_| {
            let Some(url) = NSURL::from_file_path(path) else { return Thumb::Nothing };
            let waited = wait(stop, bell, |done: Done<Answer>| {
                let size = CGSize::new(fit[0].max(1) as f64, fit[1].max(1) as f64);
                // The thumbnail, or a quick low-quality one if that's all there is; never the
                // file type's icon (the card says more than a generic icon).
                let types = Types::Thumbnail | Types::LowQualityThumbnail;
                // SAFETY: a valid file URL, size and scale (1: `fit` is in device pixels).
                let request = unsafe {
                    QLThumbnailGenerationRequest::initWithFileAtURL_size_scale_representationTypes(
                        QLThumbnailGenerationRequest::alloc(),
                        &url,
                        size,
                        1.0,
                        types,
                    )
                };
                // SAFETY: the shared generator may be used from any thread.
                let generator = unsafe { QLThumbnailGenerator::sharedGenerator() };
                let done = Mutex::new(Some(done));
                let block = RcBlock::new(move |rep: *mut QLThumbnailRepresentation, err: *mut NSError| {
                    // SAFETY: Quick Look passes a valid representation or nil, and an error or nil.
                    let image = unsafe { rep.as_ref() }.map(|r| unsafe { r.CGImage() });
                    #[cfg(test)]
                    LAST_CODE.store(unsafe { err.as_ref() }.map_or(0, |e| e.code()), Relaxed);
                    #[cfg(not(test))]
                    let _ = err;
                    if let Some(done) = lock(&done).take() {
                        done(image);
                    }
                });
                // SAFETY: the block is copied by Quick Look and called once, on its own thread.
                unsafe { generator.generateBestRepresentationForRequest_completionHandler(&request, &block) };
                Some(Box::new(Request { generator, request }))
            });
            match waited {
                Waited::Answer(Some(image)) => rgba(&image, fit, stop),
                Waited::Stopped => Thumb::Stopped,
                Waited::Answer(None) | Waited::Failed => Thumb::Nothing,
            }
        })
    }

    /// Draws `image` into an sRGB bitmap of its size fitted to `fit`, and reads it back as
    /// straight-alpha RGBA.
    fn rgba(image: &CGImage, fit: [u32; 2], stop: &AtomicBool) -> Thumb {
        let (w, h) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
        if w == 0 || h == 0 || w as u64 > crate::preview::MAX_SIDE || h as u64 > crate::preview::MAX_SIDE {
            return Thumb::Nothing;
        }
        let (dw, dh) = crate::preview::fit_size(w as u32, h as u32, fit);
        let (dw, dh) = (dw as usize, dh as usize);
        let mut px = vec![0u8; dw * dh * 4];
        // SAFETY: a constant CoreGraphics exports.
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }));
        let info = CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;
        // SAFETY: `px` holds `dh` rows of `dw * 4` bytes and outlives the context.
        let Some(ctx) = (unsafe { CGBitmapContextCreate(px.as_mut_ptr().cast(), dw, dh, 8, dw * 4, space.as_deref(), info) }) else {
            return Thumb::Nothing;
        };
        CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
        CGContext::draw_image(Some(&ctx), CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(dw as f64, dh as f64)), Some(image));
        drop(ctx);
        if stop.load(Relaxed) {
            return Thumb::Stopped;
        }
        unpremultiply(&mut px);
        Thumb::Image((dw as u32, dh as u32, px))
    }
}

// ---- Windows: the shell's thumbnails ------------------------------------------------------------

#[cfg(windows)]
pub(crate) mod win {
    //! `IShellItemImageFactory` by hand (windows-sys has no COM interfaces). Its vtable follows
    //! `shobjidl_core.h`: IUnknown's three methods, then `GetImage`.

    use super::*;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::SIZE;
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, GetObjectW,
        HBITMAP,
    };
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize};
    use windows_sys::Win32::UI::Shell::{SHCreateItemFromParsingName, SIIGBF, SIIGBF_INCACHEONLY, SIIGBF_THUMBNAILONLY};
    use windows_sys::core::{GUID, HRESULT};

    const IID_ISHELLITEMIMAGEFACTORY: GUID = GUID::from_u128(0xbcc18b79_ba16_442f_80c4_8a59c30c463b);

    // Read by COM through pointers, not by name.
    #[allow(dead_code)]
    #[repr(C)]
    struct FactoryVtbl {
        query_interface: usize,
        add_ref: usize,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        get_image: unsafe extern "system" fn(*mut c_void, SIZE, SIIGBF, *mut HBITMAP) -> HRESULT,
    }

    /// COM, initialised once on the preview's thread (a single-threaded apartment, as the shell
    /// wants) and uninitialised when the thread ends.
    struct Com(bool);

    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: balances this thread's successful CoInitializeEx.
                unsafe { CoUninitialize() };
            }
        }
    }

    thread_local! {
        // SAFETY: CoInitializeEx on the current thread; S_OK and S_FALSE are balanced by Drop.
        static COM: Com = Com(unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) } >= 0);
    }

    /// Which of the shell's two ways to ask.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub(crate) enum Ask {
        /// Only what the thumbnail cache already holds: never runs a thumbnail handler.
        CacheOnly,
        /// Runs the file type's thumbnail handler if the cache has nothing: can't be cancelled.
        Generate,
    }

    pub(super) fn thumbnail(path: &Path, fit: [u32; 2], stop: &AtomicBool, generate: bool) -> Thumb {
        if let Some(px) = get(path, fit, Ask::CacheOnly) {
            return Thumb::Image(px);
        }
        if stop.load(Relaxed) {
            return Thumb::Stopped;
        }
        if !generate {
            return Thumb::Nothing;
        }
        let px = get(path, fit, Ask::Generate);
        if stop.load(Relaxed) {
            return Thumb::Stopped;
        }
        px.map_or(Thumb::Nothing, Thumb::Image)
    }

    /// One `GetImage` call: the thumbnail fitted to `fit`, or `None` when the shell has none.
    pub(crate) fn get(path: &Path, fit: [u32; 2], ask: Ask) -> Option<Rgba> {
        COM.with(|_| ());
        // The shell's parser takes only backslashes, and a path typed or joined with `/` has some.
        let wide: Vec<u16> = path.as_os_str().encode_wide().map(|c| if c == '/' as u16 { '\\' as u16 } else { c }).chain([0]).collect();
        let mut factory: *mut c_void = std::ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated; a valid IID and out-pointer.
        let hr = unsafe { SHCreateItemFromParsingName(wide.as_ptr(), std::ptr::null_mut(), &IID_ISHELLITEMIMAGEFACTORY, &mut factory) };
        if hr < 0 || factory.is_null() {
            return None;
        }
        let flags = SIIGBF_THUMBNAILONLY | if ask == Ask::CacheOnly { SIIGBF_INCACHEONLY } else { 0 };
        let size = SIZE { cx: fit[0].clamp(1, i32::MAX as u32) as i32, cy: fit[1].clamp(1, i32::MAX as u32) as i32 };
        let mut bitmap: HBITMAP = std::ptr::null_mut();
        // SAFETY: `factory` is a live IShellItemImageFactory, released once here.
        let hr = unsafe {
            let vt = &**(factory as *mut *const FactoryVtbl);
            let hr = (vt.get_image)(factory, size, flags, &mut bitmap);
            (vt.release)(factory);
            hr
        };
        if hr < 0 || bitmap.is_null() {
            return None;
        }
        // SAFETY: a bitmap the shell handed over, deleted once here.
        let px = unsafe { read(bitmap, fit) };
        // SAFETY: as above.
        unsafe { DeleteObject(bitmap) };
        px
    }

    /// The bitmap's pixels as straight-alpha RGBA, top row first, shrunk to `fit` if the shell
    /// gave more.
    ///
    /// SAFETY: `bitmap` is a valid HBITMAP.
    unsafe fn read(bitmap: HBITMAP, fit: [u32; 2]) -> Option<Rgba> {
        // SAFETY: as the function's contract says; every buffer is as big as we tell GDI.
        unsafe {
            let mut bm: BITMAP = std::mem::zeroed();
            if GetObjectW(bitmap, std::mem::size_of::<BITMAP>() as i32, (&mut bm as *mut BITMAP).cast()) == 0 {
                return None;
            }
            let (w, h) = (bm.bmWidth.unsigned_abs(), bm.bmHeight.unsigned_abs());
            if w == 0 || h == 0 || w as u64 > crate::preview::MAX_SIDE || h as u64 > crate::preview::MAX_SIDE {
                return None;
            }
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            let mut px = vec![0u8; w as usize * h as usize * 4];
            let dc = CreateCompatibleDC(std::ptr::null_mut());
            let rows = GetDIBits(dc, bitmap, 0, h, px.as_mut_ptr().cast(), &mut info, DIB_RGB_COLORS);
            DeleteDC(dc);
            if rows != h as i32 {
                return None;
            }
            // BGRA, premultiplied when it has alpha at all; a bitmap with no alpha says 0.
            let opaque = px.as_chunks::<4>().0.iter().all(|p| p[3] == 0);
            for p in px.as_chunks_mut::<4>().0 {
                p.swap(0, 2);
                if opaque {
                    p[3] = 255;
                }
            }
            if !opaque {
                unpremultiply(&mut px);
            }
            let (dw, dh) = crate::preview::fit_size(w, h, fit);
            if (dw, dh) == (w, h) {
                return Some((w, h, px));
            }
            let mut s = crate::preview::Shrink::new(w, h, dw, dh);
            for (y, row) in px.chunks_exact(w as usize * 4).enumerate() {
                s.row(y as u32, row, 4);
            }
            Some((dw, dh, s.finish()))
        }
    }
}

// ---- tests --------------------------------------------------------------------------------------

/// Test-only: a stand-in for the OS's thumbnailer, per path, so the preview's routing, fallback
/// and cancellation are tested on every platform.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone)]
    pub(crate) enum Fake {
        /// A `w` × `h` thumbnail, every pixel `[9, 99, 199, 255]`.
        Image(u32, u32),
        Nothing,
        /// An asynchronous request that answers only when the test says.
        Hang(Arc<Os>),
    }

    /// One pretend asynchronous OS request.
    #[derive(Default)]
    pub(crate) struct Os {
        pub started: AtomicUsize,
        pub cancelled: AtomicUsize,
        pub done: Mutex<Option<Done<Option<Rgba>>>>,
    }

    struct Request(Arc<Os>);

    impl Pending for Request {
        fn cancel(&self) {
            self.0.cancelled.fetch_add(1, Relaxed);
        }
    }

    pub(crate) static FAKES: Mutex<Vec<(PathBuf, Fake)>> = Mutex::new(Vec::new());

    pub(crate) fn set(path: &Path, f: Fake) {
        lock(&FAKES).push((path.to_path_buf(), f));
    }

    /// The fake's answer for `path`, if one is set.
    pub(crate) fn answer(path: &Path, stop: &AtomicBool, bell: &Arc<Bell>) -> Option<Thumb> {
        let f = lock(&FAKES).iter().find(|(p, _)| p == path).map(|(_, f)| f.clone())?;
        Some(match f {
            Fake::Image(w, h) => Thumb::Image((w, h, [9, 99, 199, 255].repeat((w * h) as usize))),
            Fake::Nothing => Thumb::Nothing,
            Fake::Hang(os) => match wait(stop, bell, |done| {
                *lock(&os.done) = Some(done);
                os.started.fetch_add(1, Relaxed);
                Some(Box::new(Request(os.clone())))
            }) {
                Waited::Answer(Some(px)) => Thumb::Image(px),
                Waited::Stopped => Thumb::Stopped,
                Waited::Answer(None) | Waited::Failed => Thumb::Nothing,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Waits (briefly) until `f()` holds.
    fn until(f: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < end, "timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// An answer that comes before any stop is what the wait returns.
    #[test]
    fn a_wait_returns_the_answer() {
        let (stop, bell) = (AtomicBool::new(false), Arc::new(Bell::default()));
        let r = wait(&stop, &bell, |done: Done<u32>| {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                done(7);
            });
            struct P;
            impl Pending for P {
                fn cancel(&self) {
                    panic!("not stopped");
                }
            }
            Some(Box::new(P))
        });
        assert_eq!(r, Waited::Answer(7));
    }

    /// A stop rung mid-flight ends the wait at once and cancels the request; the answer that
    /// comes afterwards is dropped.
    #[test]
    fn a_stop_mid_flight_cancels_and_drops_the_late_answer() {
        let os = Arc::new(fake::Os::default());
        let (stop, bell) = (Arc::new(AtomicBool::new(false)), Arc::new(Bell::default()));
        let (o, s, b) = (os.clone(), stop.clone(), bell.clone());
        let waiter = std::thread::spawn(move || {
            wait(&s, &b, |done: Done<Option<Rgba>>| {
                *lock(&o.done) = Some(done);
                o.started.fetch_add(1, Relaxed);
                struct P(Arc<fake::Os>);
                impl Pending for P {
                    fn cancel(&self) {
                        self.0.cancelled.fetch_add(1, Relaxed);
                    }
                }
                Some(Box::new(P(o.clone())))
            })
        });
        until(|| os.started.load(Relaxed) == 1);
        std::thread::sleep(Duration::from_millis(20));
        assert!(!waiter.is_finished(), "it waits for the answer");
        let told = Instant::now();
        stop.store(true, Relaxed);
        bell.ring();
        assert_eq!(waiter.join().unwrap(), Waited::Stopped);
        assert!(told.elapsed() < Duration::from_millis(500), "the stop ended the wait at once");
        assert_eq!(os.cancelled.load(Relaxed), 1, "the OS request was cancelled");
        // The OS answers anyway, late: nobody takes it, and nothing breaks.
        let done = lock(&os.done).take().unwrap();
        done(Some((1, 1, vec![0; 4])));
    }

    #[test]
    fn unpremultiplying() {
        let mut px = [100, 50, 0, 128, 10, 20, 30, 255, 5, 5, 5, 0];
        unpremultiply(&mut px);
        assert_eq!(px, [199, 100, 0, 128, 10, 20, 30, 255, 5, 5, 5, 0]);
    }

    /// A one-page PDF: a blue band and a word.
    #[cfg(any(target_os = "macos", windows))]
    pub(crate) fn pdf() -> Vec<u8> {
        let content = b"0 0 1 rg 72 480 468 240 re f 0 g BT /F1 64 Tf 72 360 Td (Arbor) Tj ET";
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".into(),
            format!("<< /Length {} >>\nstream\n{}\nendstream", content.len(), std::str::from_utf8(content).unwrap()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        ];
        let mut out = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut at = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            at.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for a in at {
            out.extend_from_slice(format!("{a:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        out
    }

    #[cfg(any(target_os = "macos", windows))]
    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-thumb-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Quick Look thumbnails a generated PDF: non-empty RGBA, fitted, with the page's blue band.
    #[cfg(target_os = "macos")]
    #[test]
    fn quick_look_thumbnails_a_pdf() {
        let d = scratch("ql");
        let p = d.join("page.pdf");
        std::fs::write(&p, pdf()).unwrap();
        let bell = Arc::new(Bell::default());
        let t = Instant::now();
        let r = thumbnail(&p, [400, 400], &AtomicBool::new(false), &bell).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let Thumb::Image((w, h, px)) = r else { panic!("no thumbnail: {r:?}") };
        eprintln!("Quick Look: {w} × {h} thumbnail of a one-page PDF in {ms:.1} ms");
        assert!(w > 0 && h > 0 && w <= 400 && h <= 400, "{w} × {h}");
        assert_eq!(px.len(), (w * h * 4) as usize);
        assert!(px.as_chunks::<4>().0.iter().any(|p| p[2] > 200 && p[0] < 80), "the blue band is there");
        // A file Quick Look has no thumbnail for: nothing, not an icon.
        let junk = d.join("junk.arborjunk");
        std::fs::write(&junk, [0u8, 1, 2, 3, 255, 254].repeat(100)).unwrap();
        assert_eq!(thumbnail(&junk, [400, 400], &AtomicBool::new(false), &bell), Some(Thumb::Nothing));
    }

    /// A Quick Look request stopped while it runs: the wait ends at once, `cancelRequest:` is
    /// called, and the request finishes as cancelled (or had already finished).
    #[cfg(target_os = "macos")]
    #[test]
    fn quick_look_requests_are_cancelled() {
        let d = scratch("ql-cancel");
        let p = d.join("page.pdf");
        std::fs::write(&p, pdf()).unwrap();
        let (stop, bell) = (Arc::new(AtomicBool::new(false)), Arc::new(Bell::default()));
        let before = mac::CANCELS.load(Relaxed);
        mac::LAST_CODE.store(-1, Relaxed);
        let (s, b, q) = (stop.clone(), bell.clone(), p.clone());
        let t = std::thread::spawn(move || thumbnail(&q, [2000, 2000], &s, &b));
        std::thread::sleep(Duration::from_millis(1));
        let told = Instant::now();
        stop.store(true, Relaxed);
        bell.ring();
        let r = t.join().unwrap();
        let after_ms = told.elapsed().as_secs_f64() * 1000.0;
        if r == Some(Thumb::Stopped) {
            assert_eq!(mac::CANCELS.load(Relaxed), before + 1, "cancelRequest: was called");
            until(|| mac::LAST_CODE.load(Relaxed) != -1);
            eprintln!("Quick Look: stopped {after_ms:.2} ms after the stop; completion error code {}", mac::LAST_CODE.load(Relaxed));
        } else {
            eprintln!("Quick Look answered before the stop: {r:?}");
        }
    }

    /// Windows: the cache-only and full paths on a generated PNG and PDF. The full path makes a
    /// PNG thumbnail (Windows has a handler for it); the cache-only path never runs one.
    #[cfg(windows)]
    #[test]
    fn shell_thumbnails_cache_only_and_full() {
        use image::ImageEncoder;
        let d = scratch("shell");
        let png = d.join("pic.png");
        let (w, h) = (640u32, 480u32);
        let px: Vec<u8> = (0..w * h).flat_map(|i| [(i % w * 255 / w) as u8, (i / w * 255 / h) as u8, 128, 255]).collect();
        let mut b = Vec::new();
        image::codecs::png::PngEncoder::new(&mut b).write_image(&px, w, h, image::ExtendedColorType::Rgba8).unwrap();
        std::fs::write(&png, b).unwrap();
        std::fs::write(d.join("page.pdf"), pdf()).unwrap();
        let cold = win::get(&png, [256, 256], win::Ask::CacheOnly);
        eprintln!("cache only, new file: {:?}", cold.as_ref().map(|t| (t.0, t.1)));
        let full = win::get(&png, [256, 256], win::Ask::Generate);
        let Some((tw, th, tpx)) = full else { panic!("the shell made no thumbnail of a PNG") };
        assert!(tw <= 256 && th <= 256 && tw > 0 && th > 0, "{tw} × {th}");
        assert_eq!(tpx.len(), (tw * th * 4) as usize);
        assert!(tpx[3] == 255, "opaque");
        // A path with forward slashes (typed in go-to, or joined) works as well.
        let slashed = std::path::PathBuf::from(png.to_string_lossy().replace('\\', "/"));
        assert!(win::get(&slashed, [256, 256], win::Ask::Generate).is_some(), "{}", slashed.display());
        let warm = win::get(&png, [256, 256], win::Ask::CacheOnly);
        eprintln!("cache only after generating: {:?}", warm.as_ref().map(|t| (t.0, t.1)));
        // Off by default: the preview's own path asks the cache only.
        const { assert!(!UNCANCELLABLE_WINDOWS_THUMBNAILS) };
        let _ = win::get(&d.join("page.pdf"), [256, 256], win::Ask::CacheOnly);
    }

    /// How long the OS takes, cache-only and full, for a generated PDF and PNG and every file in
    /// `ARBOR_THUMB_DIR` (videos, real PDFs). Full generation can't be cancelled on Windows: this
    /// is the work that would go on after a cancel there.
    /// `cargo test --release -- --ignored --nocapture os_thumbnail_speed`
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    #[ignore = "benchmark"]
    fn os_thumbnail_speed() {
        let d = scratch("speed");
        let mut files = vec![d.join("page.pdf")];
        std::fs::write(&files[0], pdf()).unwrap();
        if let Some(dir) = std::env::var_os("ARBOR_THUMB_DIR") {
            let mut more: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
            more.sort();
            files.extend(more);
        }
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
        for p in files {
            let size = std::fs::metadata(&p).map_or(0, |m| m.len());
            #[cfg(target_os = "macos")]
            {
                let bell = Arc::new(Bell::default());
                for round in ["first", "again"] {
                    let t = Instant::now();
                    let r = thumbnail(&p, [1024, 1024], &AtomicBool::new(false), &bell).unwrap();
                    let got = if let Thumb::Image((w, h, _)) = r { format!("{w} × {h}") } else { format!("{r:?}") };
                    eprintln!("{} ({size} B), {round}: {:.1} ms, {got}", p.display(), ms(t));
                }
            }
            #[cfg(windows)]
            for ask in [win::Ask::CacheOnly, win::Ask::Generate, win::Ask::CacheOnly, win::Ask::Generate] {
                let t = Instant::now();
                let r = win::get(&p, [1024, 1024], ask);
                eprintln!("{} ({size} B), {ask:?}: {:.1} ms, {:?}", p.display(), ms(t), r.map(|t| (t.0, t.1)));
            }
        }
    }
}
