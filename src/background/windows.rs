//! Windows background processes: the shell and its descendants live in a Job
//! Object that this record owns.

use std::io;
use std::process::{Child, Command};

use crate::tools::exec::{Job, spawn_in_job};

/// A background shell's process tree. Dropping it closes the job, which
/// kills anything still running in it.
pub(super) struct Group(Job);

impl Group {
    /// Starts `command` hidden, isolated from Yawl's console, and inside a
    /// new job.
    pub(super) fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        let (child, job) = spawn_in_job(command)?;
        Ok((child, Self(job)))
    }

    /// Whether any process in the job still runs. When the job cannot be
    /// queried the tree is reported as gone; the shell's own exit still gates
    /// settling, and closing the job kills stragglers.
    pub(super) fn alive(&self) -> bool {
        self.0.active_processes().is_ok_and(|active| active > 0)
    }

    /// Terminates the whole tree at once and reaps the shell.
    ///
    /// A shell with a hidden console has no graceful stop to ask for: there
    /// is no `SIGTERM` equivalent, and Ctrl+C cannot be sent without sharing
    /// the console. Programs that need a clean shutdown must be asked through
    /// their own protocol before `shell_stop`.
    pub(super) fn stop(&self, child: &mut Child) {
        self.kill(child);
    }

    /// Terminates the whole tree at once and reaps the shell.
    pub(super) fn kill(&self, child: &mut Child) {
        let _ = self.0.terminate();
        let _ = child.wait();
    }
}
