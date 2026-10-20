//! Shared terminal-mode guards for interactive front ends. Raw mode and the
//! window size come from the platform child module; the rest is shared.

#[cfg(any(windows, test))]
mod key_text;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use self::unix::RawMode;
#[cfg(windows)]
pub(crate) use self::windows::{ConsoleInput, RawMode};

/// Size of the terminal on stdout as `(columns, rows)`, or `None` when it
/// cannot be read. Either value may be zero if the terminal does not say.
pub(crate) fn window_size() -> Option<(u16, u16)> {
    #[cfg(unix)]
    return unix::window_size();
    #[cfg(windows)]
    return windows::window_size();
}

/// Width of the terminal on stdout, or 80 when it cannot be read.
pub(crate) fn terminal_columns() -> usize {
    match window_size() {
        Some((columns, _)) if columns > 0 => usize::from(columns),
        _ => 80,
    }
}

/// Reads one line from stdin, appending it to `line`, and returns the byte
/// count (0 at end of input). On Windows the read returns an `Interrupted`
/// error soon after Ctrl+C, because a console read cannot be interrupted
/// otherwise; on Unix Ctrl+C interrupts the underlying read as before.
pub(crate) fn read_line(line: &mut String) -> std::io::Result<usize> {
    #[cfg(unix)]
    return std::io::stdin().read_line(line);
    #[cfg(windows)]
    return windows::read_line(line);
}
