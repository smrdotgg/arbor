//! Paths and names for the OS's C APIs: NUL-terminated bytes on Unix, NUL-terminated UTF-16 on
//! Windows. Every module calling `libc` or `windows-sys` with a path takes them from here.

#[cfg(unix)]
use std::ffi::{CString, OsStr};

/// `s` as a C string, for `libc`. A NUL inside it (which no path from the OS has) is
/// `InvalidInput`.
#[cfg(unix)]
pub fn cstr(s: impl AsRef<OsStr>) -> std::io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(s.as_ref().as_bytes()).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "name contains a NUL byte"))
}

/// `s` as NUL-terminated UTF-16, for the `W` functions of the Windows API.
#[cfg(windows)]
pub fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.as_ref().encode_wide().chain([0]).collect()
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn a_nul_inside_is_refused() {
        assert_eq!(super::cstr("/a/b").unwrap().as_bytes(), b"/a/b");
        assert_eq!(super::cstr("a\0b").unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    }
}
