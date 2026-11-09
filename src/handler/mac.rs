use super::{Backup, Change, remove_backup};
use std::path::Path;

pub const BUNDLE_ID: &str = "gg.smr.arbor";
pub const FINDER: &str = "com.apple.finder";
pub const FOLDER_TYPE: &str = "public.folder";
const BEFORE: &str = "public.folder viewer";

/// The default viewer for `public.folder`, as far as this needs it.
pub trait Handlers {
    /// What LaunchServices answers now. Without a choice of the user's, that's a guess
    /// among the apps that declare folders (Archive Utility, an editor), never Finder,
    /// which handles folders without declaring them.
    fn current(&self) -> Option<String>;
    /// The viewer the user (or an app) chose explicitly, if any: the one worth putting back.
    fn chosen(&self) -> Option<String>;
    fn set(&mut self, bundle_id: &str) -> Result<(), String>;
}

/// `bundle` is the running app's bundle id, if it runs from a bundle.
pub fn run(change: Change, ls: &mut dyn Handlers, bundle: Option<&str>, backup: &Path) -> Result<String, String> {
    match change {
        Change::Make => {
            if bundle != Some(BUNDLE_ID) {
                return Err("Only Arbor.app can be the default for folders: macOS asks the app bundle (this Arbor runs as a bare program). Open Arbor.app and run it there".into());
            }
            // The backup is on the disk before LaunchServices is changed.
            if Backup::require(Backup::read(backup)?, &[BEFORE.into()], backup)?.is_none() {
                let mut b = Backup::default();
                b.put(BEFORE, ls.chosen().filter(|c| !c.eq_ignore_ascii_case(BUNDLE_ID)).as_deref().map(str::as_bytes));
                b.write(backup)?;
            }
            ls.set(BUNDLE_ID)?;
            Ok("Arbor is now the default for folders that apps open. Finder still opens the desktop, the Dock's folders and \"Show in Finder\" from other apps. Restore the System Default for Folders undoes it.".into())
        }
        Change::Restore => {
            let b = Backup::require(Backup::read(backup)?, &[BEFORE.into()], backup)?.unwrap_or_default();
            let to = b.get(BEFORE).flatten().map_or_else(|| FINDER.to_string(), |v| String::from_utf8_lossy(v).into_owned());
            let now = ls.current();
            if now.as_deref().is_some_and(|c| !c.eq_ignore_ascii_case(BUNDLE_ID)) {
                remove_backup(backup)?;
                return Ok(format!(
                    "Arbor isn't the default for folders ({} is, set since), so nothing was changed",
                    now.unwrap_or_default()
                ));
            }
            ls.set(&to)?;
            remove_backup(backup)?;
            let name = if to == FINDER { "Finder".to_string() } else { to };
            Ok(format!("Folders open in {name} again"))
        }
    }
}

/// The running app's bundle id, when it runs from an `.app` bundle.
#[cfg(target_os = "macos")]
pub fn bundle_id() -> Option<String> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSString;
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: class messages returning autoreleased objects (or nil), released with the pool.
        unsafe {
            let bundle: *mut AnyObject = msg_send![objc2::class!(NSBundle), mainBundle];
            let bundle = bundle.as_ref()?;
            let path: *mut NSString = msg_send![bundle, bundlePath];
            if !path.as_ref()?.to_string().ends_with(".app") {
                return None;
            }
            let id: *mut NSString = msg_send![bundle, bundleIdentifier];
            Some(id.as_ref()?.to_string())
        }
    })
}

#[cfg(target_os = "macos")]
pub struct LaunchServices;

#[cfg(target_os = "macos")]
mod ffi {
    use std::ffi::c_void;
    /// `kLSRolesViewer`.
    pub const VIEWER: u32 = 2;
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        pub fn LSCopyDefaultRoleHandlerForContentType(content_type: *const c_void, role: u32) -> *const c_void;
        pub fn LSSetDefaultRoleHandlerForContentType(content_type: *const c_void, role: u32, handler: *const c_void) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFRelease(cf: *const c_void);
    }
}

/// The `public.folder` viewer in LaunchServices' own record of explicit choices
/// (`LSHandlers` in `com.apple.launchservices.secure.plist`). Read only.
#[cfg(target_os = "macos")]
fn chosen_viewer() -> Option<String> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSString;
    let home = std::env::var("HOME").ok()?;
    let file = format!("{home}/Library/Preferences/com.apple.LaunchServices/com.apple.launchservices.secure.plist");
    objc2::rc::autoreleasepool(|_| {
        let string = |o: *mut AnyObject| -> Option<String> {
            // SAFETY: `o` is nil or an object; it's read only if it's an NSString.
            unsafe {
                let o = o.as_ref()?;
                let is: bool = msg_send![o, isKindOfClass: objc2::class!(NSString)];
                is.then(|| (*(o as *const AnyObject).cast::<NSString>()).to_string())
            }
        };
        let key = |k: &str| NSString::from_str(k);
        // SAFETY: Foundation reads of a property list; every object is checked before use.
        unsafe {
            let dict: *mut AnyObject = msg_send![objc2::class!(NSDictionary), dictionaryWithContentsOfFile: &*key(&file)];
            let dict = dict.as_ref()?;
            let handlers: *mut AnyObject = msg_send![dict, objectForKey: &*key("LSHandlers")];
            let handlers = handlers.as_ref()?;
            let is_array: bool = msg_send![handlers, isKindOfClass: objc2::class!(NSArray)];
            if !is_array {
                return None;
            }
            let n: usize = msg_send![handlers, count];
            for i in 0..n {
                let h: *mut AnyObject = msg_send![handlers, objectAtIndex: i];
                let Some(h) = h.as_ref() else { continue };
                let is_dict: bool = msg_send![h, isKindOfClass: objc2::class!(NSDictionary)];
                if !is_dict || string(msg_send![h, objectForKey: &*key("LSHandlerContentType")]).as_deref() != Some(FOLDER_TYPE) {
                    continue;
                }
                return string(msg_send![h, objectForKey: &*key("LSHandlerRoleViewer")])
                    .or_else(|| string(msg_send![h, objectForKey: &*key("LSHandlerRoleAll")]))
                    .filter(|v| v != "-");
            }
            None
        }
    })
}

#[cfg(target_os = "macos")]
impl Handlers for LaunchServices {
    fn chosen(&self) -> Option<String> {
        chosen_viewer()
    }

    fn current(&self) -> Option<String> {
        use objc2_foundation::NSString;
        let ty = NSString::from_str(FOLDER_TYPE);
        // SAFETY: an NSString is a CFString (toll-free bridged); the copied result is
        // released after it's read.
        unsafe {
            let got = ffi::LSCopyDefaultRoleHandlerForContentType(objc2::rc::Retained::as_ptr(&ty).cast(), ffi::VIEWER);
            if got.is_null() {
                return None;
            }
            let s = (*got.cast::<NSString>()).to_string();
            ffi::CFRelease(got);
            Some(s)
        }
    }

    fn set(&mut self, bundle_id: &str) -> Result<(), String> {
        use objc2_foundation::NSString;
        let (ty, id) = (NSString::from_str(FOLDER_TYPE), NSString::from_str(bundle_id));
        // SAFETY: two CFStrings (bridged NSStrings) alive for the call.
        let r = unsafe {
            ffi::LSSetDefaultRoleHandlerForContentType(
                objc2::rc::Retained::as_ptr(&ty).cast(),
                ffi::VIEWER,
                objc2::rc::Retained::as_ptr(&id).cast(),
            )
        };
        if r == 0 { Ok(()) } else { Err(format!("macOS refused to change the default for folders (error {r})")) }
    }
}
