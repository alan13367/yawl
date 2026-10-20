//! Windows side of the remote input wait: the console input handle, an
//! auto-reset event that wakes the wait, and `WaitForMultipleObjects` over
//! both.

use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};

use super::HostInput;

pub(in crate::tui) use crate::terminal_mode::ConsoleInput as TerminalInput;

/// What the input wait blocks on: a waitable kernel object handle.
pub(super) type Handle = HANDLE;

/// Matches the console reader's idle timeout, which paces UI ticks.
const TICK_MILLIS: u32 = 100;

impl HostInput for TerminalInput {
    fn wait_handle(&self) -> Handle {
        self.handle()
    }

    fn has_buffered(&self) -> bool {
        self.has_pending()
    }
}

/// An auto-reset event that wakes the UI thread's input wait as soon as
/// remote input arrives, instead of after the next tick.
pub(super) struct Wake {
    event: HANDLE,
}

impl Wake {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: Null attributes and name request an unnamed event that is
        // not inheritable. Auto-reset (manual-reset FALSE) and initially
        // unsignaled (FALSE). Returns a new handle, or 0 on failure.
        let event = unsafe { CreateEventW(std::ptr::null(), FALSE, FALSE, std::ptr::null()) };
        if event == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { event })
    }

    pub(super) fn notify(&self) {
        // A signaled event already guarantees a pending wake-up, and failure
        // leaves the next tick to pick the input up.
        // SAFETY: `event` is a valid event handle owned by this value.
        let _ = unsafe { SetEvent(self.event) };
    }

    /// A successful wait on an auto-reset event already clears it; this also
    /// clears a signal that nobody waited on.
    pub(super) fn drain(&self) {
        // SAFETY: `event` is a valid event handle owned by this value.
        let _ = unsafe { ResetEvent(self.event) };
    }

    /// Signaled when [`notify`](Self::notify) was called.
    pub(super) fn handle(&self) -> Handle {
        self.event
    }
}

impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: `event` came from a successful CreateEventW and is closed
        // exactly once, here.
        unsafe { CloseHandle(self.event) };
    }
}

/// What an input wait found.
pub(super) struct Ready {
    /// The host terminal has input to read.
    pub(super) host: bool,
    /// The wake handle was signaled.
    pub(super) wake: bool,
}

/// Waits until `host` is signaled, `wake` is signaled, or a tick elapses.
/// The console handle is signaled by any input record, so a ready host may
/// still read zero bytes; the reader treats that as a tick.
pub(super) fn wait(host: Handle, wake: Handle) -> Ready {
    let handles = [host, wake];
    // SAFETY: `handles` is a valid array of two handles that stay open for
    // the duration of the call; wait-any (bWaitAll FALSE) with a timeout.
    let result = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), FALSE, TICK_MILLIS) };
    match result {
        WAIT_OBJECT_0 => Ready {
            host: true,
            // The wait consumed only the host signal. Take a pending wake
            // too, as the Unix wait drains both.
            // SAFETY: `wake` is a valid event handle; a zero timeout polls.
            wake: unsafe { WaitForSingleObject(wake, 0) } == WAIT_OBJECT_0,
        },
        result if result == WAIT_OBJECT_0 + 1 => Ready {
            host: false,
            wake: true,
        },
        // A failed wait is reported as ready so the read that follows
        // surfaces the error instead of this loop spinning silently.
        WAIT_FAILED => Ready {
            host: true,
            wake: false,
        },
        // WAIT_TIMEOUT, and anything unexpected, is a tick.
        _ => Ready {
            host: false,
            wake: false,
        },
    }
}
