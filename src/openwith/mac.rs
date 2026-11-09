use super::Handler;
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send, sel};
use objc2_foundation::NSString;
use std::path::Path;

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn LSCopyApplicationURLsForURL(url: *const AnyObject, roles: u32) -> *mut AnyObject;
    fn CFRelease(cf: *const std::ffi::c_void);
}

fn path_of(url: &AnyObject) -> Option<String> {
    // SAFETY: an NSURL; `path` is nil-safe and autoreleased.
    unsafe {
        let p: *mut NSString = msg_send![url, path];
        p.as_ref().map(|p| p.to_string())
    }
}

pub fn list(file: &Path) -> Result<Vec<Handler>, String> {
    autoreleasepool(|_| {
        // SAFETY: NSWorkspace may be used from any thread; every object is checked for nil,
        // the array Launch Services copies is released, and the rest are autoreleased.
        unsafe {
            let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
            let ws = ws.as_ref().ok_or("NSWorkspace isn't available")?;
            let p = NSString::from_str(&file.to_string_lossy());
            let url: *mut AnyObject = msg_send![class!(NSURL), fileURLWithPath: &*p];
            let url = url.as_ref().ok_or("not a file URL")?;
            let default: *mut AnyObject = msg_send![ws, URLForApplicationToOpenURL: url];
            let default = default.as_ref().and_then(path_of);
            let modern: bool = msg_send![ws, respondsToSelector: sel!(URLsForApplicationsToOpenURL:)];
            let (arr, owned): (*mut AnyObject, bool) = if modern {
                (msg_send![ws, URLsForApplicationsToOpenURL: url], false)
            } else {
                (LSCopyApplicationURLsForURL(url, 0xFFFF_FFFF), true)
            };
            let mut paths: Vec<String> = Vec::new();
            if let Some(a) = arr.as_ref() {
                let n: usize = msg_send![a, count];
                for i in 0..n {
                    let u: *mut AnyObject = msg_send![a, objectAtIndex: i];
                    if let Some(p) = u.as_ref().and_then(path_of)
                        && !paths.contains(&p)
                    {
                        paths.push(p);
                    }
                }
                if owned {
                    CFRelease((a as *const AnyObject).cast());
                }
            }
            if let Some(d) = &default {
                paths.retain(|p| p != d);
                paths.insert(0, d.clone());
            }
            let fm: *mut AnyObject = msg_send![class!(NSFileManager), defaultManager];
            let out: Vec<Handler> = paths
                .into_iter()
                .map(|p| {
                    let ns = NSString::from_str(&p);
                    let shown: *mut NSString = match fm.as_ref() {
                        Some(fm) => msg_send![fm, displayNameAtPath: &*ns],
                        None => std::ptr::null_mut(),
                    };
                    let name = shown.as_ref().map(|s| s.to_string()).unwrap_or_else(|| p.clone());
                    let name = name.strip_suffix(".app").unwrap_or(&name).to_string();
                    Handler { default: Some(&p) == default.as_ref(), name, id: p, exec: None }
                })
                .collect();
            if out.is_empty() { Err("No apps can open this file".into()) } else { Ok(out) }
        }
    })
}
