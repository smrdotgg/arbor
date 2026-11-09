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
