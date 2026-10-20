//! Unix process runner: a session and process group per child, signals for
//! the whole group, and nonblocking pipes.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use super::{ExecResult, Keep, RunOptions, drain_with};

/// The shell command for `script`: `sh -c`, with `PATH` replaced when given.
pub(super) fn shell_command(script: &str, path: Option<&OsStr>) -> std::io::Result<Command> {
    let mut command = Command::new("sh");
    command.arg("-c").arg(script);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    Ok(command)
}

/// A path as the shell sees it. Unix paths already are.
pub(super) fn shell_path(path: &Path) -> OsString {
    path.as_os_str().to_owned()
}

/// Starts `command` in a new session, which also makes it the leader of its
/// own process group, so callers can still signal the whole group.
///
/// The new session has no controlling terminal. Otherwise a password or
/// passphrase prompt from ssh, sudo, or git opens the user's terminal, is
/// stopped by `SIGTTIN` because it runs in a background group, and hangs
/// until the timeout; the model then sees only "timed out" instead of the
/// real `Permission denied`. Without a terminal those prompts fail at once.
pub(crate) fn isolate_from_terminal(command: &mut Command) -> &mut Command {
    command.env("GIT_TERMINAL_PROMPT", "0");
    // SAFETY: The hook runs in the forked child before exec and only calls
    // `setsid`, which is async-signal-safe and touches no parent memory.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

pub(super) fn run(mut cmd: Command, options: RunOptions<'_>) -> std::io::Result<ExecResult> {
    // Killing only the immediate shell would leave grandchildren running and
    // could keep the output pipes open forever, so the child leads its own
    // process group.
    isolate_from_terminal(&mut cmd);
    cmd.stdin(if options.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    let mut stdin = child.stdin.take();
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        kill_and_reap(&mut child);
        return Err(std::io::Error::other("child output pipes were not created"));
    };
    if let Err(error) = set_nonblocking(&stdout)
        .and_then(|()| set_nonblocking(&stderr))
        .and_then(|()| stdin.as_ref().map_or(Ok(()), set_nonblocking))
    {
        kill_and_reap(&mut child);
        return Err(error);
    }
    let process_group = i32::try_from(child.id()).ok();
    let stop_readers = Arc::new(AtomicBool::new(false));
    let (stop_out, stop_err) = (Arc::clone(&stop_readers), Arc::clone(&stop_readers));
    let (keep_out, keep_err) = (options.stdout, options.stderr);
    let out_handle = std::thread::spawn(move || drain(stdout, keep_out, &stop_out));
    let err_handle = std::thread::spawn(move || drain(stderr, keep_err, &stop_err));
    // A blocking waiter reports the exit at once, so quick commands do not
    // pay a polling interval. It leaves the child a zombie until this thread
    // has finished signalling the group: reaping frees the pid, and a kill
    // sent to a freed process group id could reach whatever group reuses it.
    let (exited_tx, exited_rx) = std::sync::mpsc::channel();
    let (signalled_tx, signalled_rx) = std::sync::mpsc::channel::<()>();
    let waiter = std::thread::spawn(move || {
        wait_for_exit(child.id());
        let _ = exited_tx.send(());
        let _ = signalled_rx.recv();
        child.wait()
    });

    let deadline = options
        .timeout
        .and_then(|timeout| Instant::now().checked_add(timeout));
    let (mut exited, mut timed_out, mut interrupted) = (false, false, false);
    let mut input_sent = 0;
    loop {
        if exited && out_handle.is_finished() && err_handle.is_finished() {
            break;
        }
        if crate::cancellation::interrupted() {
            interrupted = true;
            break;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            timed_out = true;
            break;
        }
        if let (Some(pipe), Some(data)) = (stdin.as_mut(), options.stdin)
            && write_available(pipe, data, &mut input_sent)
        {
            // EOF tells a reader that it has received the entire input.
            stdin = None;
        }
        let wait = if exited || stdin.is_some() {
            Duration::from_millis(5)
        } else {
            Duration::from_millis(25)
        };
        match exited_rx.recv_timeout(wait) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => exited = true,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    drop(stdin);
    if interrupted || timed_out {
        if let Some(process_group) = process_group {
            kill_group(process_group);
        }
        // An escaped descendant may be outside the process group. Do not
        // wait indefinitely for it to close an inherited pipe.
        stop_readers.store(true, Ordering::Release);
    }
    drop(signalled_tx);
    let status = waiter
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("process waiter panicked")));
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    let status = status?;
    Ok(ExecResult {
        status: (!interrupted && !timed_out)
            .then(|| status.code())
            .flatten(),
        stdout,
        stderr,
        timed_out,
        interrupted,
    })
}

fn kill_group(process_group: i32) {
    // SAFETY: `kill` receives the negative id of the process group created
    // for this child. SIGKILL requires no Rust-side memory invariants.
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
}

/// Blocks until the child `pid` exits without reaping it, so its pid and
/// process group id stay reserved until `Child::wait` collects it.
fn wait_for_exit(pid: libc::id_t) {
    loop {
        // SAFETY: `siginfo_t` is plain data, so all zero bytes are a valid
        // value for `waitid` to overwrite.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a live, writable `siginfo_t`. `WNOWAIT` only
        // observes the exit of this process's own child and does not reap it.
        let result =
            unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if result == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
        {
            return;
        }
    }
}

fn kill_and_reap(child: &mut Child) {
    match i32::try_from(child.id()) {
        Ok(process_group) => kill_group(process_group),
        Err(_) => {
            let _ = child.kill();
        }
    }
    let _ = child.wait();
}

/// Attempts all available input without waiting for a reader. A closed or
/// non-reading child must not outlive the command's timeout on a writer thread.
fn write_available(writer: &mut impl Write, data: &[u8], sent: &mut usize) -> bool {
    while *sent < data.len() {
        match writer.write(&data[*sent..]) {
            Ok(0) => return true,
            Ok(count) => *sent += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return false,
            // The tool may exit before it consumes stdin; treat broken pipes
            // like the earlier best-effort writer did.
            Err(_) => return true,
        }
    }
    true
}

fn set_nonblocking(pipe: &impl AsRawFd) -> std::io::Result<()> {
    // SAFETY: the borrowed pipe owns a valid descriptor throughout both calls.
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: F_SETFL updates flags on that same live descriptor, preserving
    // its existing flags and adding only nonblocking reads.
    if unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Makes reads on `pipe` return `WouldBlock` instead of waiting.
pub(crate) fn nonblocking<R: Read + AsRawFd + Send + 'static>(
    pipe: R,
) -> std::io::Result<impl Read + Send + 'static> {
    set_nonblocking(&pipe)?;
    Ok(pipe)
}

/// Waits up to 10 ms for `pipe` to become readable or reach end of file.
fn wait_readable(pipe: &impl AsRawFd) {
    let mut descriptor = libc::pollfd {
        fd: pipe.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `descriptor` is one initialized pollfd for a descriptor the
    // borrowed pipe keeps open for the duration of the call.
    unsafe {
        libc::poll(&mut descriptor, 1, 10);
    }
}

fn drain<R: Read + AsRawFd>(reader: R, keep: Keep, stop: &AtomicBool) -> String {
    drain_with(reader, keep, stop, |reader| wait_readable(reader))
}

#[cfg(test)]
mod tests {
    use super::super::run_with_timeout;
    use super::*;

    #[test]
    fn exit_wait_keeps_the_pid_reserved_until_reaped() -> std::io::Result<()> {
        let mut child = Command::new("true").spawn()?;
        let pid = libc::pid_t::try_from(child.id()).map_err(std::io::Error::other)?;
        wait_for_exit(child.id());
        // SAFETY: signal 0 only checks that the pid exists.
        let exists = unsafe { libc::kill(pid, 0) } == 0;
        assert!(exists, "the exited child was reaped before `wait`");
        assert!(child.wait()?.success());
        Ok(())
    }

    #[test]
    fn nonblocking_stdin_writer_returns_when_peer_stops_reading() {
        let (mut writer, _reader_kept_open) = std::os::unix::net::UnixStream::pair().unwrap();
        set_nonblocking(&writer).unwrap();
        let data = vec![b'x'; 4 * 1024 * 1024];
        let mut sent = 0;
        assert!(!write_available(&mut writer, &data, &mut sent));
        assert!(sent > 0 && sent < data.len());
    }

    #[test]
    fn output_reader_can_stop_without_waiting_for_eof() {
        let (reader, _writer_kept_open) = std::os::unix::net::UnixStream::pair().unwrap();
        set_nonblocking(&reader).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || drain(reader, Keep::HeadAndTail, &reader_stop));
        std::thread::sleep(Duration::from_millis(40));
        stop.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            handle.is_finished(),
            "reader still waits for a surviving descendant"
        );
        assert_eq!(handle.join().unwrap(), "");
    }

    #[test]
    fn commands_run_in_their_own_session_without_a_terminal() -> std::io::Result<()> {
        let mut command = Command::new("sleep");
        command.arg("5");
        isolate_from_terminal(&mut command);
        let mut child = command.spawn()?;
        let pid = i32::try_from(child.id()).expect("pid fits");
        // SAFETY: getsid and getpgid only read process attributes for a
        // live child we own.
        let (session, group) = unsafe { (libc::getsid(pid), libc::getpgid(pid)) };
        child.kill()?;
        child.wait()?;
        assert_eq!(session, pid, "the child leads a new session");
        assert_eq!(group, pid, "the child still leads its process group");

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("if (exec 3</dev/tty) 2>/dev/null; then echo tty; else echo none; fi; printf %s \"$GIT_TERMINAL_PROMPT\"");
        let result = run_with_timeout(cmd, None, Duration::from_secs(5))?;
        assert_eq!(result.stdout, "none\n0");
        Ok(())
    }
}
