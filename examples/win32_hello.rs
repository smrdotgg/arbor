//! Cold-start probe: a bare Win32 window (RegisterClass, CreateWindowEx, "hello" painted with
//! GDI in WM_PAINT), no GPU: the floor any Windows app starts from. Appends `win32-hello <ms> …`
//! to `SATCHEL_TIMING_FILE` at the end of its first WM_PAINT, then closes (see
//! `src/coldstart.rs` for what `ms` counts). Windows only; elsewhere it says so.
//!
//! ```text
//! cargo build --profile dist --example win32_hello
//! ```

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[path = "../src/coldstart.rs"]
mod coldstart;

#[cfg(not(windows))]
fn main() {
    eprintln!("win32_hello is a Windows probe");
}

#[cfg(windows)]
fn main() {
    win::run();
}

#[cfg(windows)]
mod win {
    use std::cell::Cell;
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT, TextOutW};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, IDC_ARROW,
        LoadCursorW, MSG, PostQuitMessage, RegisterClassW, SW_SHOWDEFAULT, ShowWindow, TranslateMessage, WM_DESTROY, WM_PAINT, WNDCLASSW,
        WS_OVERLAPPEDWINDOW,
    };

    use super::coldstart;

    thread_local! {
        static MAIN: Cell<Option<Instant>> = const { Cell::new(None) };
        static PAINTED: Cell<bool> = const { Cell::new(false) };
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_PAINT => {
                // SAFETY: a zeroed PAINTSTRUCT is valid input; BeginPaint fills it for this window.
                let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
                let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
                let hello: Vec<u16> = "hello".encode_utf16().collect();
                unsafe { TextOutW(hdc, 20, 20, hello.as_ptr(), hello.len() as i32) };
                unsafe { EndPaint(hwnd, &ps) };
                if !PAINTED.replace(true) {
                    let main = MAIN.get().unwrap_or_else(Instant::now);
                    let line = coldstart::line("win32-hello", main);
                    match std::env::var_os("SATCHEL_TIMING_FILE") {
                        Some(path) => coldstart::append(&path, &line),
                        None => eprintln!("{line}"),
                    }
                    unsafe { DestroyWindow(hwnd) };
                }
                0
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    pub fn run() {
        MAIN.set(Some(Instant::now()));
        let class = wide("win32-hello");
        let title = wide("win32-hello");
        // SAFETY: plain Win32 calls with live, NUL-terminated strings; the class outlives the window.
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(proc),
                hInstance: instance,
                hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                980,
                620,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            ShowWindow(hwnd, SW_SHOWDEFAULT);
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
