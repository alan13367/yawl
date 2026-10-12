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

    /// Toggles whether Ctrl+C raises SIGINT. With signals off, Ctrl+C arrives
    /// as a 0x03 input byte instead.
    pub(crate) fn set_signals(&self, enabled: bool) -> Result<(), Error> {
        // SAFETY: The zeroed value is initialized by `tcgetattr` before any
        // field is read.
        let mut current: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: stdin is a valid process descriptor and `current` points
        // to writable termios storage.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut current) } != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        if enabled {
            current.c_lflag |= libc::ISIG;
        } else {
            current.c_lflag &= !libc::ISIG;
        }
        // SAFETY: stdin is valid and `current` points to initialized terminal
        // settings.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &current) } != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(())
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

/// Width of the terminal on stdout, or 80 when it cannot be read.
pub(crate) fn terminal_columns() -> usize {
    // SAFETY: A zeroed winsize has a valid all-integer representation and is
    // passed to ioctl as writable storage for the stdout descriptor.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: STDOUT_FILENO is a valid descriptor and `size` points to
    // writable winsize storage.
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
        && size.ws_col > 0
    {
        usize::from(size.ws_col)
    } else {
        80
    }
}
