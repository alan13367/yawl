//! Yawl — a minimal, self-extending AI agent harness.
//!
//! Single crate, blocking I/O, four direct dependencies. The model extends
//! Yawl at runtime by writing executable tools; the harness never needs
//! recompiling to gain capabilities.

pub mod agent;
pub mod compaction;
pub mod config;
pub mod error;
pub mod onboarding;
pub mod prompt;
pub mod provider;
pub mod session;
pub mod tools;
pub mod tui;

use std::sync::atomic::{AtomicBool, Ordering};

/// Global turn-abort flag. Set by Ctrl+C (signal handler in print mode,
/// keyboard byte in the TUI); checked between stream events and during tool
/// execution. Aborts the in-flight turn, never the process.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

pub fn set_interrupted(value: bool) {
    INTERRUPTED.store(value, Ordering::Relaxed);
}

extern "C" fn handle_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// Installs the process-wide Ctrl+C handler used by both front ends.
///
/// # Errors
///
/// Returns the operating-system error if the signal handler cannot be set.
pub fn install_interrupt_handler() -> std::io::Result<()> {
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
        Ok(())
    }
}
