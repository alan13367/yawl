//! Unix side of the remote input wait: the terminal as a descriptor, a
//! self-pipe that wakes the wait, and `poll` over both.

use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use super::HostInput;

/// What the input wait polls: a file descriptor.
pub(super) type Handle = RawFd;

/// Matches the raw-mode read timeout, which paces UI ticks.
const TICK_MILLIS: libc::c_int = 100;

/// Unbuffered reads from the terminal on stdin.
pub(in crate::tui) struct TerminalInput(RawFd);

impl TerminalInput {
    pub(in crate::tui) fn stdin() -> Self {
        Self(libc::STDIN_FILENO)
    }

    #[cfg(test)]
    pub(super) fn from_fd(fd: RawFd) -> Self {
        Self(fd)
    }
}

impl Read for TerminalInput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` is valid writable storage of `buf.len()` bytes and the
        // descriptor stays open for the terminal's lifetime.
        let read = unsafe { libc::read(self.0, buf.as_mut_ptr().cast(), buf.len()) };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(read as usize)
    }
}

impl AsRawFd for TerminalInput {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// Any readable descriptor can be waited on directly.
impl<R: Read + AsRawFd> HostInput for R {
    fn wait_handle(&self) -> Handle {
        self.as_raw_fd()
    }
}

/// A non-blocking self-pipe that wakes the UI thread's input wait as soon as
/// remote input arrives, instead of after the raw-mode read timeout.
pub(super) struct Wake {
    read: OwnedFd,
    write: OwnedFd,
}

impl Wake {
    pub(super) fn new() -> io::Result<Self> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is writable storage for the two descriptors pipe
        // returns on success.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pipe succeeded, so both descriptors are open and owned
        // solely by this value from here on.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for fd in [&read, &write] {
            // SAFETY: `fd` is an open descriptor owned by this value.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` is an open descriptor owned by this value; adding
            // O_NONBLOCK changes no memory.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` is an open descriptor owned by this value; setting
            // FD_CLOEXEC changes no memory.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { read, write })
    }

    pub(super) fn notify(&self) {
        // A full pipe already guarantees a pending wake-up.
        // SAFETY: writes one byte from a valid buffer to an owned descriptor.
        let _ = unsafe { libc::write(self.write.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }

    pub(super) fn drain(&self) {
        let mut buf = [0u8; 64];
        // SAFETY: reads into a valid local buffer from an owned, non-blocking
        // descriptor until it is empty.
        while unsafe { libc::read(self.read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {
        }
    }

    /// Becomes readable when [`notify`](Self::notify) was called.
    pub(super) fn handle(&self) -> Handle {
        self.read.as_raw_fd()
    }
}

/// What an input wait found.
pub(super) struct Ready {
    /// The host terminal has input to read.
    pub(super) host: bool,
    /// The wake handle was signaled.
    pub(super) wake: bool,
}

/// Waits until `host` is readable, `wake` is signaled, or a tick elapses.
pub(super) fn wait(host: Handle, wake: Handle) -> Ready {
    let mut fds = [
        libc::pollfd {
            fd: host,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: `fds` is a valid array of two initialized pollfd entries
    // for descriptors that stay open for the duration of the call.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, TICK_MILLIS) };
    Ready {
        // An interrupted or timed-out wait is a tick.
        host: ready > 0 && fds[0].revents != 0,
        wake: fds[1].revents != 0,
    }
}
