//! Windows console raw mode, window size, and key input.
//!
//! The console is switched to virtual-terminal input and output, so the TUI
//! sees the same escape sequences as on Unix. Standard handles are borrowed
//! from the process and never closed here.

use std::io::{self, Read};

use windows_sys::Win32::Foundation::{
    HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT,
    ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_PROCESSED_OUTPUT, ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT, FlushConsoleInputBuffer, GetConsoleCP,
    GetConsoleMode, GetConsoleOutputCP, GetConsoleScreenBufferInfo, GetNumberOfConsoleInputEvents,
    GetStdHandle, INPUT_RECORD, KEY_EVENT, ReadConsoleInputW, STD_HANDLE, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, SetConsoleCP, SetConsoleMode, SetConsoleOutputCP,
};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

use super::key_text::KeyText;
use crate::error::Error;

const UTF8_CODE_PAGE: u32 = 65001;
/// How long an idle read waits. Matches the Unix raw-mode `VTIME` of 100 ms,
/// which paces UI ticks.
const TICK_MILLIS: u32 = 100;
/// Records taken per read, so one burst of pasted text is a few calls.
const RECORDS: usize = 128;

/// The process's standard handle for `which`, which is borrowed, not owned.
fn std_handle(which: STD_HANDLE) -> io::Result<HANDLE> {
    // SAFETY: GetStdHandle takes a constant and returns a handle value or
    // INVALID_HANDLE_VALUE; it dereferences nothing.
    let handle = unsafe { GetStdHandle(which) };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else if handle == 0 {
        Err(io::Error::other("the process has no such standard handle"))
    } else {
        Ok(handle)
    }
}

/// The console mode of `handle`; fails when it is not a console buffer.
fn console_mode(handle: HANDLE) -> io::Result<u32> {
    let mut mode = 0u32;
    // SAFETY: `mode` is writable storage for one u32; an invalid or
    // non-console handle makes the call fail instead of misbehave.
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(mode)
}

fn set_console_mode(handle: HANDLE, mode: u32) -> io::Result<()> {
    // SAFETY: SetConsoleMode takes a handle and flag bits by value; an
    // invalid handle makes the call fail.
    if unsafe { SetConsoleMode(handle, mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Mode bits that undo what raw mode changed on stdin. `GetConsoleMode` only
/// reports the quick-edit state when the extended flags are present, and
/// `SetConsoleMode` ignores it without them, so a mode lacking the extended
/// flags is restored with quick edit explicitly on, the console default.
fn restorable_input_mode(original: u32) -> u32 {
    if original & ENABLE_EXTENDED_FLAGS == 0 {
        original | ENABLE_EXTENDED_FLAGS | ENABLE_QUICK_EDIT_MODE
    } else {
        original
    }
}

/// The input mode for raw operation: no line editing or echo, no
/// console-side mouse selection or resize records, and VT sequences for
/// special keys and mouse reports. `ENABLE_PROCESSED_INPUT` is kept (set) so
/// Ctrl+C raises the console control handler, as ISIG does on Unix.
fn raw_input_mode(original: u32) -> u32 {
    (original
        & !(ENABLE_LINE_INPUT
            | ENABLE_ECHO_INPUT
            | ENABLE_MOUSE_INPUT
            | ENABLE_QUICK_EDIT_MODE
            | ENABLE_WINDOW_INPUT))
        | ENABLE_PROCESSED_INPUT
        | ENABLE_VIRTUAL_TERMINAL_INPUT
        | ENABLE_EXTENDED_FLAGS
}

/// Switches the console to raw mode while keeping Ctrl+C signal delivery
/// enabled, and restores the previous modes and code pages on drop.
pub(crate) struct RawMode {
    stdin: HANDLE,
    stdin_mode: u32,
    /// Absent when stdout is not a console, as when output is redirected.
    stdout: Option<(HANDLE, u32)>,
    /// Zero when the code page could not be read, which skips restoring it.
    input_code_page: u32,
    output_code_page: u32,
}

impl RawMode {
    pub(crate) fn enter() -> Result<Self, Error> {
        let stdin = std_handle(STD_INPUT_HANDLE)?;
        let stdin_mode = console_mode(stdin)?;
        let stdout = std_handle(STD_OUTPUT_HANDLE)
            .ok()
            .and_then(|handle| Some((handle, console_mode(handle).ok()?)));
        // SAFETY: The code page getters take no arguments and return 0 on
        // failure.
        let (input_code_page, output_code_page) = unsafe { (GetConsoleCP(), GetConsoleOutputCP()) };
        // Built before anything changes: if a step below fails, dropping the
        // guard puts back whatever was already switched.
        let guard = Self {
            stdin,
            stdin_mode,
            stdout,
            input_code_page,
            output_code_page,
        };
        guard.apply()?;
        Ok(guard)
    }

    fn apply(&self) -> Result<(), Error> {
        // Code pages are best effort: Rust's own console output is UTF-16, so
        // a refusal only affects child processes that inherit the console.
        // SAFETY: The setters take a code page number by value.
        unsafe {
            SetConsoleCP(UTF8_CODE_PAGE);
            SetConsoleOutputCP(UTF8_CODE_PAGE);
        }
        if let Some((handle, mode)) = self.stdout {
            // Like OPOST off on Unix: a bare line feed does not return the
            // carriage. Wrapping and other bits are left as the user had them.
            let raw = mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
            // Consoles older than Windows 10 1709 reject the newline flag.
            set_console_mode(handle, raw | DISABLE_NEWLINE_AUTO_RETURN)
                .or_else(|_| set_console_mode(handle, raw))?;
        }
        set_console_mode(self.stdin, raw_input_mode(self.stdin_mode))?;
        // Like TCSAFLUSH: drop anything typed before raw mode began.
        // SAFETY: Takes the stdin handle by value; failure only means
        // typeahead survives.
        unsafe { FlushConsoleInputBuffer(self.stdin) };
        Ok(())
    }

    /// Toggles whether Ctrl+C raises the console control handler. With
    /// signals off, Ctrl+C arrives as a 0x03 input character instead.
    pub(crate) fn set_signals(&self, enabled: bool) -> Result<(), Error> {
        let mode = console_mode(self.stdin)?;
        let mode = if enabled {
            mode | ENABLE_PROCESSED_INPUT
        } else {
            mode & !ENABLE_PROCESSED_INPUT
        };
        // Extended flags keep quick edit off, as `enter` left it.
        set_console_mode(self.stdin, mode | ENABLE_EXTENDED_FLAGS)?;
        Ok(())
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Restoration is best effort; there is nobody left to report to.
        if let Some((handle, mode)) = self.stdout {
            let _ = set_console_mode(handle, mode);
        }
        let _ = set_console_mode(self.stdin, restorable_input_mode(self.stdin_mode));
        // SAFETY: The setters take a code page number by value, and the
        // flush takes the stdin handle by value.
        unsafe {
            if self.input_code_page != 0 {
                SetConsoleCP(self.input_code_page);
            }
            if self.output_code_page != 0 {
                SetConsoleOutputCP(self.output_code_page);
            }
            FlushConsoleInputBuffer(self.stdin);
        }
    }
}

/// Size of the visible console window on stdout as `(columns, rows)`, not
/// the scrollback buffer behind it.
pub(super) fn window_size() -> Option<(u16, u16)> {
    let handle = std_handle(STD_OUTPUT_HANDLE).ok()?;
    // SAFETY: The struct is plain integers, so all-zero is a valid value that
    // the call overwrites on success.
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is writable storage; a non-console handle makes the call
    // fail.
    if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } == 0 {
        return None;
    }
    let window = info.srWindow;
    let columns = i32::from(window.Right) - i32::from(window.Left) + 1;
    let rows = i32::from(window.Bottom) - i32::from(window.Top) + 1;
    Some((u16::try_from(columns).ok()?, u16::try_from(rows).ok()?))
}

/// Unbuffered reads of console key input as UTF-8, with the contract of the
/// Unix raw-mode `VMIN=0, VTIME=1` reads: a read returns the bytes available,
/// or `Ok(0)` after about 100 ms without input. `Ok(0)` is a tick, not end of
/// input.
pub(crate) struct ConsoleInput {
    handle: HANDLE,
    text: KeyText,
}

impl ConsoleInput {
    /// Reads the process's standard input. An unusable handle shows up as an
    /// error from `read`, so this cannot fail.
    pub(crate) fn stdin() -> Self {
        Self {
            handle: std_handle(STD_INPUT_HANDLE).unwrap_or(INVALID_HANDLE_VALUE),
            text: KeyText::default(),
        }
    }

    /// The handle that is signaled while input records are queued. It is
    /// signaled by records that carry no text too, so a ready handle does not
    /// promise that `read` returns bytes.
    pub(crate) fn handle(&self) -> HANDLE {
        self.handle
    }

    /// Whether translated bytes are queued that the handle does not show.
    pub(crate) fn has_pending(&self) -> bool {
        self.text.has_pending()
    }

    /// Moves queued input records into the text queue. Only key-down records
    /// with a character matter; focus, menu, size, and mouse records are
    /// discarded.
    fn read_records(&mut self) -> io::Result<()> {
        let mut queued = 0u32;
        // SAFETY: `queued` is writable storage for one u32.
        if unsafe { GetNumberOfConsoleInputEvents(self.handle, &mut queued) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // The read below would block on an empty queue, so never call it
        // without a record to take. Nobody else reads this handle.
        if queued == 0 {
            return Ok(());
        }
        let count = (queued as usize).min(RECORDS);
        // SAFETY: INPUT_RECORD is plain integers and unions of them, so an
        // all-zero value is valid.
        let mut records: [INPUT_RECORD; RECORDS] = unsafe { std::mem::zeroed() };
        let mut read = 0u32;
        // SAFETY: `records` has room for `count` entries and `read` is
        // writable storage; at least one record is queued, so the call
        // returns without blocking.
        if unsafe { ReadConsoleInputW(self.handle, records.as_mut_ptr(), count as u32, &mut read) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        for record in &records[..(read as usize).min(count)] {
            if u32::from(record.EventType) != KEY_EVENT {
                continue;
            }
            // SAFETY: `EventType` says the KeyEvent member is the active one.
            let key = unsafe { record.Event.KeyEvent };
            // SAFETY: Every bit pattern of the character union is a valid
            // u16, and the wide member is the one ReadConsoleInputW fills.
            let unit = unsafe { key.uChar.UnicodeChar };
            self.text.push(key.bKeyDown != 0, key.wRepeatCount, unit);
        }
        Ok(())
    }
}

impl Read for ConsoleInput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if !self.text.has_pending() {
            // Wait on the handle, not a blocking read: the handle is signaled
            // for any record, and `read_records` never blocks.
            // SAFETY: Takes the handle and a timeout by value; an invalid
            // handle makes the call fail.
            match unsafe { WaitForSingleObject(self.handle, TICK_MILLIS) } {
                WAIT_OBJECT_0 => self.read_records()?,
                WAIT_TIMEOUT => return Ok(0),
                WAIT_FAILED => return Err(io::Error::last_os_error()),
                _ => return Ok(0),
            }
        }
        // Zero bytes here means only non-text records arrived: a tick.
        Ok(self.text.drain(buf))
    }
}

/// Reads a line from stdin in cooked mode, giving up with `Interrupted` once
/// Ctrl+C has set the interrupt flag. A console read cannot be interrupted
/// from outside, so the read runs on a helper thread and this thread polls
/// the flag every tick. After an interrupt the helper stays blocked until the
/// console delivers a line or the process exits, which is what happens next.
pub(super) fn read_line(line: &mut String) -> io::Result<usize> {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::time::Duration;

    let (sender, receiver) = channel();
    std::thread::Builder::new()
        .name("yawl-stdin".into())
        .spawn(move || {
            let mut text = String::new();
            let result = io::stdin().read_line(&mut text).map(|count| (count, text));
            // The receiver is gone after an interrupt; nobody wants the line.
            let _ = sender.send(result);
        })?;
    loop {
        match receiver.recv_timeout(Duration::from_millis(u64::from(TICK_MILLIS))) {
            Ok(result) => {
                let (count, text) = result?;
                line.push_str(&text);
                return Ok(count);
            }
            Err(RecvTimeoutError::Timeout) => {
                if crate::interrupted() {
                    return Err(io::ErrorKind::Interrupted.into());
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("the stdin reader stopped"));
            }
        }
    }
}
