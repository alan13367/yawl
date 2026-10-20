//! Windows cancellation: a console control handler for Ctrl+C. Windows has no
//! way to interrupt another thread's blocking socket read, so there is no
//! wake-up; blocking HTTP reads poll for cancellation instead (see
//! `provider::http`).

use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{FALSE, TRUE};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

use super::INTERRUPTED;

/// Guards against registering the routine twice: Windows would run it twice
/// per event, and the handler list has no use for duplicates.
static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Console control routine, called by the system on a thread it creates for
/// each event. It must therefore touch nothing but atomics.
///
/// Returns TRUE (handled) for Ctrl+C and Ctrl+Break so the default handler,
/// which would terminate the process, does not run. Close, logoff, and
/// shutdown return FALSE so default handling proceeds and the process exits.
unsafe extern "system" fn handle_ctrl_event(ctrl_type: u32) -> i32 {
    match ctrl_type {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            INTERRUPTED.store(true, Ordering::Relaxed);
            TRUE
        }
        _ => FALSE,
    }
}

/// Installs the process-wide Ctrl+C handler used by both front ends. Calling
/// it again after success does nothing.
///
/// # Errors
///
/// Returns the operating-system error if the handler cannot be registered.
pub(crate) fn install_interrupt_handler() -> std::io::Result<()> {
    if HANDLER_INSTALLED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    // SAFETY: `handle_ctrl_event` has the system calling convention the API
    // requires and only performs an atomic store. Passing a routine with
    // `add = TRUE` registers it; a zero return reports failure, with the
    // reason available from `GetLastError`.
    let added = unsafe { SetConsoleCtrlHandler(Some(handle_ctrl_event), TRUE) };
    if added == 0 {
        HANDLER_INSTALLED.store(false, Ordering::Release);
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// An identifier for the calling thread, kept for symmetry with Unix, where
/// [`wake_thread`] uses it.
pub(crate) fn native_thread_id() -> usize {
    // SAFETY: `GetCurrentThreadId` takes no arguments, cannot fail, and
    // returns the calling thread's identifier.
    unsafe { GetCurrentThreadId() as usize }
}

/// Does nothing: Windows cannot interrupt another thread's blocking socket
/// call. Blocked HTTP reads notice cancellation within one polling slice
/// instead.
pub(crate) fn wake_thread(_thread: usize) {}
