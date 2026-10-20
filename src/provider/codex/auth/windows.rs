//! Opening the login page in the default browser on Windows.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr::null;

use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Asks the shell to open `url` with the user's default browser.
///
/// Only https URLs that Yawl builds itself reach this function. The shell
/// would run anything it is given, including local programs, so other
/// schemes are refused.
pub(super) fn open_url(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err(format!("refusing to open {url}: not an https URL"));
    }
    let wide = |text: &str| {
        OsStr::new(text)
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<u16>>()
    };
    let operation = wide("open");
    let file = wide(url);
    // SAFETY: `operation` and `file` are NUL-terminated UTF-16 buffers that
    // outlive the call. The window, parameters, and directory arguments are
    // allowed to be null, and `ShellExecuteW` keeps no pointer after
    // returning.
    let result = unsafe {
        ShellExecuteW(
            0,
            operation.as_ptr(),
            file.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    // The returned value is an HINSTANCE kept for 16-bit compatibility: more
    // than 32 means success, and anything lower is a `SE_ERR_*` code.
    if result > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecuteW failed with code {result}"))
    }
}
