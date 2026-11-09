//! The COM plumbing for [`super::recycle`]: `IFileOperation` and a progress sink, by hand
//! (windows-sys has no COM interfaces). Vtable layouts follow `shobjidl_core.h`, checked
//! against the `windows` crate's `IFileOperation_Vtbl` and `IFileOperationProgressSink_Vtbl`.

use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering::SeqCst};

use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
use windows_sys::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NO_UI, FOFX_RECYCLEONDELETE, FileOperation, SHCreateItemFromParsingName, TSF_DELETE_RECYCLE_IF_POSSIBLE,
};
use windows_sys::core::{GUID, HRESULT};

use super::TrashError;

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_IFILEOPERATION: GUID = GUID::from_u128(0x947aab5f_0a5c_4c13_b4d6_4bf7836fc9f8);
const IID_ISINK: GUID = GUID::from_u128(0x04b0f1a7_9490_44bc_96e1_4296a31252e2);
const IID_ISHELLITEM: GUID = GUID::from_u128(0x43826d1e_e718_42ee_bc55_a1e261c37bfe);
const S_OK: HRESULT = 0;
const E_NOINTERFACE: HRESULT = 0x80004002_u32 as i32;
const E_ABORT: HRESULT = 0x80004004_u32 as i32;

use crate::ffi::wide;

/// The volume `abs` is on: its root (`C:\`, `\\server\share\`) and drive type.
pub fn volume(abs: &Path) -> (String, u32) {
    let path = wide(abs.as_os_str());
    let mut buf = [0u16; 1024];
    // SAFETY: `path` is NUL-terminated and `buf` is as long as we say.
    let ok = unsafe { GetVolumePathNameW(path.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } != 0;
    if !ok {
        return (abs.display().to_string(), 0);
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    // SAFETY: `buf` holds a NUL-terminated root path.
    let kind = unsafe { GetDriveTypeW(buf.as_ptr()) };
    (String::from_utf16_lossy(&buf[..n]), kind)
}

/// The registry's Recycle Bin settings for the volume at `root` (see [`super::BinSettings`]).
pub fn bin_settings(root: &str) -> super::BinSettings {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    const POLICIES: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    const BITBUCKET: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket";
    let policy = |name: &str| dword(HKEY_CURRENT_USER, POLICIES, name).or_else(|| dword(HKEY_LOCAL_MACHINE, POLICIES, name));
    let mut s = super::BinSettings {
        policy_off: policy("NoRecycleFiles").is_some_and(|v| v != 0),
        policy_percent: policy("RecycleBinSize").map(u64::from),
        global_nuke: dword(HKEY_CURRENT_USER, BITBUCKET, "NukeOnDelete"),
        ..Default::default()
    };
    // `\\?\Volume{…}\`: its GUID names the volume's key.
    let mut name = [0u16; 64];
    let path = wide(std::ffi::OsStr::new(root));
    // SAFETY: `path` is NUL-terminated and `name` is as long as we say.
    if unsafe { GetVolumeNameForVolumeMountPointW(path.as_ptr(), name.as_mut_ptr(), name.len() as u32) } == 0 {
        return s;
    }
    let name = String::from_utf16_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())]);
    let (Some(a), Some(b)) = (name.find('{'), name.find('}')) else { return s };
    let key = format!(r"{BITBUCKET}\Volume\{}", &name[a..=b]);
    s.volume_key = key_exists(HKEY_CURRENT_USER, &key);
    s.nuke = dword(HKEY_CURRENT_USER, &key, "NukeOnDelete");
    s.max_mb = dword(HKEY_CURRENT_USER, &key, "MaxCapacity").map(u64::from);
    if s.policy_percent.is_some() {
        s.volume_bytes = volume_bytes(root);
    }
    s
}

/// The size of the volume at `root`, for the percentage policy.
fn volume_bytes(root: &str) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let path = wide(std::ffi::OsStr::new(root));
    let mut total = 0u64;
    // SAFETY: a NUL-terminated root path; only the total is asked for.
    let ok = unsafe { GetDiskFreeSpaceExW(path.as_ptr(), std::ptr::null_mut(), &mut total, std::ptr::null_mut()) } != 0;
    (ok && total > 0).then_some(total)
}

fn dword(root: windows_sys::Win32::System::Registry::HKEY, key: &str, value: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{RRF_RT_REG_DWORD, RegGetValueW};
    let (k, v) = (wide(std::ffi::OsStr::new(key)), wide(std::ffi::OsStr::new(value)));
    let mut data = 0u32;
    let mut len = 4u32;
    // SAFETY: NUL-terminated names; `data` holds the DWORD asked for.
    let r = unsafe {
        RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut data as *mut u32).cast(), &mut len)
    };
    (r == 0).then_some(data)
}

fn key_exists(root: windows_sys::Win32::System::Registry::HKEY, key: &str) -> bool {
    use windows_sys::Win32::System::Registry::{HKEY, KEY_READ, RegCloseKey, RegOpenKeyExW};
    let k = wide(std::ffi::OsStr::new(key));
    let mut h: HKEY = std::ptr::null_mut();
    // SAFETY: a NUL-terminated name and an out-handle, closed once here.
    unsafe {
        if RegOpenKeyExW(root, k.as_ptr(), 0, KEY_READ, &mut h) != 0 {
            return false;
        }
        RegCloseKey(h);
    }
    true
}

/// The volume key's name for `root` (tests set its values).
#[cfg(test)]
pub fn volume_key(root: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;
    let mut name = [0u16; 64];
    let path = wide(std::ffi::OsStr::new(root));
    // SAFETY: as in `bin_settings`.
    if unsafe { GetVolumeNameForVolumeMountPointW(path.as_ptr(), name.as_mut_ptr(), name.len() as u32) } == 0 {
        return None;
    }
    let name = String::from_utf16_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())]);
    let (a, b) = (name.find('{')?, name.find('}')?);
    Some(format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{}", &name[a..=b]))
}

/// What the shell said about one delete.
pub struct Recycled {
    pub refused: bool,
    pub performed: HRESULT,
    pub deleted: Option<(HRESULT, bool)>,
}

// Read by COM through pointers, not by name.
#[repr(C)]
struct UnknownVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct FileOpVtbl {
    base: UnknownVtbl,
    advise: usize,
    unadvise: usize,
    set_operation_flags: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    set_progress_message: usize,
    set_progress_dialog: usize,
    set_properties: usize,
    set_owner_window: usize,
    apply_properties_to_item: usize,
    apply_properties_to_items: usize,
    rename_item: usize,
    rename_items: usize,
    move_item: usize,
    move_items: usize,
    copy_item: usize,
    copy_items: usize,
    delete_item: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT,
    delete_items: usize,
    new_item: usize,
    perform_operations: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    get_any_operations_aborted: usize,
}

type P = *mut c_void;
type W = *const u16;

#[repr(C)]
struct SinkVtbl {
    base: UnknownVtbl,
    start_operations: unsafe extern "system" fn(P) -> HRESULT,
    finish_operations: unsafe extern "system" fn(P, HRESULT) -> HRESULT,
    pre_rename_item: unsafe extern "system" fn(P, u32, P, W) -> HRESULT,
    post_rename_item: unsafe extern "system" fn(P, u32, P, W, HRESULT, P) -> HRESULT,
    pre_move_item: unsafe extern "system" fn(P, u32, P, P, W) -> HRESULT,
    post_move_item: unsafe extern "system" fn(P, u32, P, P, W, HRESULT, P) -> HRESULT,
    pre_copy_item: unsafe extern "system" fn(P, u32, P, P, W) -> HRESULT,
    post_copy_item: unsafe extern "system" fn(P, u32, P, P, W, HRESULT, P) -> HRESULT,
    pre_delete_item: unsafe extern "system" fn(P, u32, P) -> HRESULT,
    post_delete_item: unsafe extern "system" fn(P, u32, P, HRESULT, P) -> HRESULT,
    pre_new_item: unsafe extern "system" fn(P, u32, P, W) -> HRESULT,
    post_new_item: unsafe extern "system" fn(P, u32, P, W, W, u32, HRESULT, P) -> HRESULT,
    update_progress: unsafe extern "system" fn(P, u32, u32) -> HRESULT,
    reset_timer: unsafe extern "system" fn(P) -> HRESULT,
    pause_timer: unsafe extern "system" fn(P) -> HRESULT,
    resume_timer: unsafe extern "system" fn(P) -> HRESULT,
}

/// Our `IFileOperationProgressSink`: a heap object whose first field is its vtable, freed
/// when its last reference goes.
#[repr(C)]
struct Sink {
    vtbl: &'static SinkVtbl,
    refs: AtomicU32,
    refused: AtomicBool,
    /// PostDeleteItem came: its HRESULT, and whether the item landed in the bin.
    posted: AtomicBool,
    hr: AtomicI32,
    landed: AtomicBool,
}

unsafe extern "system" fn qi(this: P, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_NOINTERFACE;
    }
    // SAFETY: COM passes a valid IID pointer and out-pointer.
    let id = unsafe { &*riid };
    let eq = |g: &GUID| g.data1 == id.data1 && g.data2 == id.data2 && g.data3 == id.data3 && g.data4 == id.data4;
    if eq(&IID_IUNKNOWN) || eq(&IID_ISINK) {
        // SAFETY: as above; `this` is a live Sink.
        unsafe {
            *out = this;
            add_ref(this);
        }
        S_OK
    } else {
        // SAFETY: as above.
        unsafe { *out = std::ptr::null_mut() };
        E_NOINTERFACE
    }
}
unsafe extern "system" fn add_ref(this: P) -> u32 {
    // SAFETY: `this` is a live Sink.
    unsafe { &*(this as *const Sink) }.refs.fetch_add(1, SeqCst) + 1
}
unsafe extern "system" fn release(this: P) -> u32 {
    // SAFETY: `this` is a live Sink; the last reference frees the Box it came from.
    let left = unsafe { &*(this as *const Sink) }.refs.fetch_sub(1, SeqCst) - 1;
    if left == 0 {
        drop(unsafe { Box::from_raw(this as *mut Sink) });
    }
    left
}
unsafe extern "system" fn ok0(_: P) -> HRESULT {
    S_OK
}
unsafe extern "system" fn finish(_: P, _: HRESULT) -> HRESULT {
    S_OK
}
unsafe extern "system" fn pre_rename(_: P, _: u32, _: P, _: W) -> HRESULT {
    S_OK
}
unsafe extern "system" fn post_rename(_: P, _: u32, _: P, _: W, _: HRESULT, _: P) -> HRESULT {
    S_OK
}
unsafe extern "system" fn pre_move(_: P, _: u32, _: P, _: P, _: W) -> HRESULT {
    S_OK
}
unsafe extern "system" fn post_move(_: P, _: u32, _: P, _: P, _: W, _: HRESULT, _: P) -> HRESULT {
    S_OK
}
/// Refuses, before anything is removed, any delete the shell isn't going to recycle.
unsafe extern "system" fn pre_delete(this: P, flags: u32, _: P) -> HRESULT {
    if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE as u32 == 0 {
        // SAFETY: `this` is a live Sink.
        unsafe { &*(this as *const Sink) }.refused.store(true, SeqCst);
        return E_ABORT;
    }
    S_OK
}
unsafe extern "system" fn post_delete(this: P, _: u32, _: P, hr: HRESULT, new_item: P) -> HRESULT {
    // SAFETY: `this` is a live Sink.
    let s = unsafe { &*(this as *const Sink) };
    s.hr.store(hr, SeqCst);
    s.landed.store(!new_item.is_null(), SeqCst);
    s.posted.store(true, SeqCst);
    S_OK
}
unsafe extern "system" fn pre_new(_: P, _: u32, _: P, _: W) -> HRESULT {
    S_OK
}
unsafe extern "system" fn post_new(_: P, _: u32, _: P, _: W, _: W, _: u32, _: HRESULT, _: P) -> HRESULT {
    S_OK
}
unsafe extern "system" fn progress(_: P, _: u32, _: u32) -> HRESULT {
    S_OK
}

static SINK_VTBL: SinkVtbl = SinkVtbl {
    base: UnknownVtbl { query_interface: qi, add_ref, release },
    start_operations: ok0,
    finish_operations: finish,
    pre_rename_item: pre_rename,
    post_rename_item: post_rename,
    pre_move_item: pre_move,
    post_move_item: post_move,
    pre_copy_item: pre_move,
    post_copy_item: post_move,
    pre_delete_item: pre_delete,
    post_delete_item: post_delete,
    pre_new_item: pre_new,
    post_new_item: post_new,
    update_progress: progress,
    reset_timer: ok0,
    pause_timer: ok0,
    resume_timer: ok0,
};

/// Releases a COM object.
unsafe fn release_obj(p: P) {
    if !p.is_null() {
        // SAFETY: `p` is a live COM object, whose first field points at an IUnknown vtable.
        unsafe { ((**(p as *mut *const UnknownVtbl)).release)(p) };
    }
}

fn fail(what: &str, hr: HRESULT) -> TrashError {
    TrashError::Failed(format!("{what} (code {hr:#010x})"))
}

/// One `IFileOperation` deleting `abs` to the Recycle Bin, with our sink watching. The
/// worker thread is its own single-threaded apartment for the call.
pub fn recycle(abs: &Path) -> Result<Recycled, TrashError> {
    // SAFETY: CoUninitialize balances a successful CoInitializeEx on this thread.
    unsafe {
        let init = CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
        if init < 0 {
            return Err(fail("couldn't start the shell's file operations", init));
        }
        let r = file_operation(abs);
        CoUninitialize();
        r
    }
}

/// SAFETY: COM is initialized on this thread as a single-threaded apartment.
unsafe fn file_operation(abs: &Path) -> Result<Recycled, TrashError> {
    let mut op: P = std::ptr::null_mut();
    // SAFETY: valid GUIDs and out-pointer.
    let hr = unsafe { CoCreateInstance(&FileOperation, std::ptr::null_mut(), CLSCTX_ALL, &IID_IFILEOPERATION, &mut op) };
    if hr < 0 || op.is_null() {
        return Err(fail("couldn't start the shell's file operations", hr));
    }
    let sink = Box::into_raw(Box::new(Sink {
        vtbl: &SINK_VTBL,
        refs: AtomicU32::new(1),
        refused: AtomicBool::new(false),
        posted: AtomicBool::new(false),
        hr: AtomicI32::new(0),
        landed: AtomicBool::new(false),
    })) as P;
    let mut item: P = std::ptr::null_mut();
    // SAFETY: `op` is a live IFileOperation and `sink` a live Sink; each object is released
    // once, here, after the operation is done with it.
    unsafe {
        let r = delete_one(op, abs, sink, &mut item);
        release_obj(item);
        release_obj(op);
        release_obj(sink);
        r
    }
}

/// SAFETY: `op` is a live IFileOperation and `sink` a live Sink. Leaves the shell item it
/// makes in `item` for the caller to release.
unsafe fn delete_one(op: P, abs: &Path, sink: P, item: &mut P) -> Result<Recycled, TrashError> {
    // SAFETY: as the function's contract says; `path` is NUL-terminated and outlives the calls.
    unsafe {
        let vt = &**(op as *mut *const FileOpVtbl);
        let hr = (vt.set_operation_flags)(op, FOF_NO_UI | FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE);
        if hr < 0 {
            return Err(fail("the shell refused the Recycle Bin flags", hr));
        }
        let path = wide(abs.as_os_str());
        let hr = SHCreateItemFromParsingName(path.as_ptr(), std::ptr::null_mut(), &IID_ISHELLITEM, item);
        if hr < 0 || item.is_null() {
            return Err(fail("the shell couldn't find it", hr));
        }
        let hr = (vt.delete_item)(op, *item, sink);
        if hr < 0 {
            return Err(fail("the shell couldn't queue it", hr));
        }
        let performed = (vt.perform_operations)(op);
        let s = &*(sink as *const Sink);
        let deleted = s.posted.load(SeqCst).then(|| (s.hr.load(SeqCst), s.landed.load(SeqCst)));
        Ok(Recycled { refused: s.refused.load(SeqCst), performed, deleted })
    }
}
