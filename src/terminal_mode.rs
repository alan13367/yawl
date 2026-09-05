//! Shared terminal-mode guards for interactive front ends.

use std::io;

use crate::error::Error;

/// Switches stdin to raw mode while keeping Ctrl+C signal delivery enabled.
pub(crate) struct RawMode {
    original: libc::termios,
}

impl RawMode {
    pub(crate) fn enter() -> Result<Self, Error> {
        // SAFETY: The zeroed value is initialized by `tcgetattr` before any
        // field is read.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: stdin is a valid process descriptor and `original` points
        // to writable termios storage.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        let mut raw = original;
        // SAFETY: `raw` is an initialized termios value.
        unsafe { libc::cfmakeraw(&mut raw) };
        raw.c_lflag |= libc::ISIG;
        raw.c_cc[libc::VQUIT] = 0;
        raw.c_cc[libc::VSUSP] = 0;
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1;
        // SAFETY: stdin is valid and `raw` points to initialized terminal
        // settings.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) } != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(Self { original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: `original` came from a successful `tcgetattr` call for
        // stdin and remains initialized for the lifetime of this guard.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.original);
        }
    }
}
