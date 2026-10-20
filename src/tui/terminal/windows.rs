//! Windows clipboard text through the Win32 clipboard API.

use std::io;
use std::time::Duration;

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

/// Other programs hold the clipboard briefly while they copy or paste, so
/// opening it is retried a few times before giving up.
const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(10);

/// An open clipboard, closed when dropped.
struct Clipboard;

impl Clipboard {
    fn open() -> io::Result<Self> {
        let mut last_error = io::Error::other("the clipboard could not be opened");
        for attempt in 0..OPEN_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(OPEN_RETRY_DELAY);
            }
            // SAFETY: A null window handle associates the open clipboard with
            // the current task, which is all this thread needs.
            if unsafe { OpenClipboard(0) } != 0 {
                return Ok(Self);
            }
            last_error = io::Error::last_os_error();
        }
        Err(last_error)
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: This value exists only while the clipboard is open on this
        // thread, so closing it here is balanced.
        unsafe { CloseClipboard() };
    }
}

/// The UTF-16 text with a terminating NUL, as `CF_UNICODETEXT` requires.
fn clipboard_units(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Replaces the clipboard contents with `text`.
pub(super) fn set_text(text: &str) -> io::Result<()> {
    let units = clipboard_units(text);
    let bytes = units.len() * size_of::<u16>();
    let _clipboard = Clipboard::open()?;
    // SAFETY: The clipboard is open and this thread holds it, as
    // EmptyClipboard requires; it then owns the clipboard.
    if unsafe { EmptyClipboard() } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: Allocates a movable block of `bytes` bytes; null on failure.
    let block = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
    if block.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `block` is a movable global block that this function owns.
    let destination = unsafe { GlobalLock(block) };
    if destination.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: `block` is still owned here and was never locked or handed
        // to the clipboard.
        unsafe { GlobalFree(block) };
        return Err(error);
    }
    // SAFETY: The block is locked and holds `bytes` bytes, which is exactly
    // the size of `units`; the regions cannot overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(units.as_ptr(), destination.cast::<u16>(), units.len())
    };
    // SAFETY: `block` is locked by this function. GlobalUnlock reports zero
    // both when the lock count reaches zero and on error, so the result says
    // nothing useful here.
    unsafe { GlobalUnlock(block) };
    // SAFETY: The clipboard is open and emptied. On success the system owns
    // the block and it must not be freed or used again.
    if unsafe { SetClipboardData(u32::from(CF_UNICODETEXT), block as HANDLE) } == 0 {
        let error = io::Error::last_os_error();
        // SAFETY: SetClipboardData failed, so ownership stayed with us.
        unsafe { GlobalFree(block) };
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_text_is_nul_terminated_utf16() {
        assert_eq!(clipboard_units("a😀"), [0x61, 0xD83D, 0xDE00, 0]);
        assert_eq!(clipboard_units(""), [0]);
    }
}
