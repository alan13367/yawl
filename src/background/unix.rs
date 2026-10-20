//! Unix background processes: the shell leads its own process group, which
//! is signalled whole.

use std::io;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const STOP_GRACE: Duration = Duration::from_secs(2);

/// A background shell's process group, identified by the shell's pid.
pub(super) struct Group(u32);

impl Group {
    /// Starts `command` as the leader of a new session and process group.
    pub(super) fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        let child = crate::tools::exec::isolate_from_terminal(command).spawn()?;
        let group = Self(child.id());
        Ok((child, group))
    }

    /// Whether any process in the group still exists.
    pub(super) fn alive(&self) -> bool {
        let Ok(process_group) = i32::try_from(self.0) else {
            return false;
        };
        // SAFETY: Signal zero performs existence and permission checks only.
        // The negative PID addresses the process group created for this
        // command.
        let result = unsafe { libc::kill(-process_group, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// Asks the group to terminate with `SIGTERM`, then kills it with
    /// `SIGKILL` after a grace period, and reaps the shell.
    pub(super) fn stop(&self, child: &mut Child) {
        self.signal(libc::SIGTERM);
        let deadline = Instant::now().checked_add(STOP_GRACE);
        loop {
            let _ = child.try_wait();
            if !self.alive() {
                let _ = child.wait();
                return;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        self.signal(libc::SIGKILL);
        let _ = child.wait();
    }

    /// Kills the group at once and reaps the shell.
    pub(super) fn kill(&self, child: &mut Child) {
        self.signal(libc::SIGKILL);
        let _ = child.wait();
    }

    fn signal(&self, signal: libc::c_int) {
        if let Ok(process_group) = i32::try_from(self.0) {
            // SAFETY: The child was created as the leader of its own process
            // group. A negative PID addresses that group, and `kill` does not
            // impose Rust memory-safety requirements.
            unsafe {
                libc::kill(-process_group, signal);
            }
        }
    }
}
