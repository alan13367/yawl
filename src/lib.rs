//! Yawl — a minimal, self-extending AI agent harness.
//!
//! Single crate, blocking I/O, four direct dependencies. The model extends
//! Yawl at runtime by writing executable tools; the harness never needs
//! recompiling to gain capabilities.

pub mod agent;
pub(crate) mod cancellation;
pub mod compaction;
pub mod config;
pub mod error;
pub(crate) mod model;
pub mod onboarding;
pub mod prompt;
pub mod provider;
pub mod session;
pub mod skills;
pub(crate) mod subagent;
pub mod tools;
pub mod tui;

use std::sync::atomic::{AtomicBool, Ordering};

/// Process-wide turn-abort flag set by SIGINT and explicit callers. TUI key
/// cancellation uses a narrower conversation token instead. Aborts active
/// work, never the process.
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

extern "C" fn handle_wake(_: libc::c_int) {}

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
            cancellation::mark_wake_handler_installed();
            Ok(())
        }
    }
}
