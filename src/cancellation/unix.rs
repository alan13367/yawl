//! POSIX cancellation: a SIGINT handler for Ctrl+C and a SIGUSR1 wake-up that
//! interrupts a worker thread's blocking system call.

use std::sync::atomic::{AtomicBool, Ordering};

use super::INTERRUPTED;

/// Set once the no-op `SIGUSR1` handler is installed. Until then a wake signal
/// could terminate the process (the default action), so none is sent.
static WAKE_HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

extern "C" fn handle_wake(_: libc::c_int) {}

/// Installs the process-wide Ctrl+C handler used by both front ends.
///
/// # Errors
///
/// Returns the operating-system error if the signal handler cannot be set.
pub(crate) fn install_interrupt_handler() -> std::io::Result<()> {
    // SAFETY: The zeroed sigaction is initialized before installation.
    // `handle_sigint` has the required C ABI and only performs an atomic
    // store. Leaving SA_RESTART unset lets Ctrl+C interrupt a blocking
    // provider read.
    let result = unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle_sigint as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut())
    };
    if result != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: `handle_wake` has the required C ABI and performs no work.
        // Leaving SA_RESTART unset makes SIGUSR1 wake a blocked provider read
        // without changing the process-wide interrupt flag.
        let wake_result = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handle_wake as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = 0;
            libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut())
        };
        if wake_result != 0 {
            Err(std::io::Error::last_os_error())
        } else {
            WAKE_HANDLER_INSTALLED.store(true, Ordering::Release);
            Ok(())
        }
    }
}

/// An identifier [`wake_thread`] can later use to signal the calling thread.
pub(crate) fn native_thread_id() -> usize {
    // SAFETY: `pthread_self` takes no arguments and returns the calling
    // thread's stable pthread identifier.
    unsafe { libc::pthread_self() as usize }
}

/// Interrupts the blocking system call of the thread identified by `thread`
/// so it can observe cancellation immediately. A no-op until
/// [`install_interrupt_handler`] has installed the `SIGUSR1` handler. Blocking
/// HTTP reads also poll for cancellation, so this only shortens the delay.
pub(crate) fn wake_thread(thread: usize) {
    if !WAKE_HANDLER_INSTALLED.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: callers retain the worker until cancellation has settled, so
    // this pthread identifier still refers to that worker. SIGUSR1 has a
    // no-op handler and exists only to interrupt a blocking system call.
    unsafe {
        libc::pthread_kill(thread as libc::pthread_t, libc::SIGUSR1);
    }
}
