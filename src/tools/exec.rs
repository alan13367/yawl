//! Bounded child-process execution shared by `shell`, custom tools, and Git
//! inspection.
//!
//! Commands run in their own process group with piped output, a deadline,
//! and live cancellation. Results follow one contract: stdout first, then
//! any stderr under a `[stderr]` marker; a nonzero exit marks an error.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const MAX_CAPTURE_BYTES: usize = 64 * 1024;

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

pub(crate) fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Formats a finished process: stdout, then stderr under a `[stderr]`
/// marker. A nonzero exit is an error and ends with its exit status.
///
/// Stderr is kept on success too. Compilers and linters report warnings
/// there while exiting zero, and dropping it would tell the model a noisy
/// `cargo clippy` run was clean.
pub fn render_result(result: &ExecResult, timeout: Duration) -> (String, bool) {
    if result.interrupted {
        return ("[interrupted by user]".to_string(), true);
    }
    if result.timed_out {
        return (format!("tool timed out after {}s", timeout.as_secs()), true);
    }
    let mut out = result.stdout.clone();
    if !result.stderr.trim().is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("[stderr]\n");
        out.push_str(result.stderr.trim_end());
    }
    if result.status == Some(0) {
        return (out, false);
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    match result.status {
        Some(code) => out.push_str(&format!("[exit code: {code}]")),
        None => out.push_str("[killed by signal]"),
    }
    (out, true)
}

pub struct ExecResult {
    /// Exit code; `None` if killed by a signal.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub interrupted: bool,
}

/// Spawns a command with piped I/O and waits with a timeout, polling
/// `try_wait` (no tokio). Also honors the global interrupt flag: Ctrl+C
/// kills the child. Reader threads drain stdout/stderr to avoid pipe
/// deadlock on chatty children.
pub fn run_with_timeout(
    mut cmd: Command,
    stdin_data: Option<&[u8]>,
    timeout: Duration,
) -> std::io::Result<ExecResult> {
    // Put the command in its own session and process group. Killing only
    // the immediate shell would leave grandchildren running and could keep
    // our output pipes open forever.
    isolate_from_terminal(&mut cmd);
    cmd.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    if let Err(error) = set_nonblocking(&stdout)
        .and_then(|()| set_nonblocking(&stderr))
        .and_then(|()| {
            if let Some(stdin) = &stdin {
                set_nonblocking(stdin)?;
            }
            Ok(())
        })
    {
        kill_and_reap(&mut child);
        return Err(error);
    }
    let stop_readers = Arc::new(AtomicBool::new(false));
    let stop_out = Arc::clone(&stop_readers);
    let stop_err = Arc::clone(&stop_readers);
    let out_handle = std::thread::spawn(move || drain(stdout, &stop_out));
    let err_handle = std::thread::spawn(move || drain(stderr, &stop_err));

    let deadline = Instant::now().checked_add(timeout);
    let mut timed_out = false;
    let mut interrupted = false;
    // The shell can exit while a background descendant still owns a pipe.
    // Completion includes EOF on both pipes, so cancellation and the deadline
    // remain active during output collection as well as process execution.
    let mut status = None;
    let mut input_sent = 0;
    let wait_result = (|| -> std::io::Result<()> {
        loop {
            if status.is_none() {
                status = child.try_wait()?;
            }
            if status.is_some() && out_handle.is_finished() && err_handle.is_finished() {
                return Ok(());
            }
            if crate::cancellation::interrupted() {
                interrupted = true;
                return Ok(());
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                timed_out = true;
                return Ok(());
            }
            if let (Some(pipe), Some(data)) = (stdin.as_mut(), stdin_data)
                && write_available(pipe, data, &mut input_sent)
            {
                // EOF tells a reader that it has received the entire JSON input.
                stdin = None;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    })();
    if interrupted || timed_out || wait_result.is_err() {
        kill_and_reap(&mut child);
        // An escaped descendant may be outside our process group. Do not
        // wait indefinitely for it to close an inherited pipe.
        stop_readers.store(true, Ordering::Release);
        status = None;
    }

    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    wait_result?;
    Ok(ExecResult {
        status: status.and_then(|s| s.code()),
        stdout,
        stderr,
        timed_out,
        interrupted,
    })
}

fn kill_and_reap(child: &mut Child) {
    let process_group = i32::try_from(child.id()).ok();
    if let Some(process_group) = process_group {
        // SAFETY: `kill` receives a valid negative process-group id created
        // for this child. SIGKILL requires no Rust-side memory invariants.
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    } else {
        let _ = child.kill();
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

fn drain(mut reader: impl Read, stop: &AtomicBool) -> String {
    let mut captured = Vec::with_capacity(MAX_CAPTURE_BYTES);
    let mut chunk = [0u8; 8 * 1024];
    let mut truncated = false;
    while !stop.load(Ordering::Acquire) {
        let read = match reader.read(&mut chunk) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&chunk[..read.min(remaining)]);
        truncated |= read > remaining;
    }
    let mut output = String::from_utf8_lossy(&captured).into_owned();
    if truncated {
        output.push_str("\n[process output truncated]");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_stops_descendants_holding_pipes_after_shell_exit() {
        let token = crate::cancellation::CancellationToken::default();
        let trigger = token.clone();
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            trigger.cancel();
        });
        let started = Instant::now();
        let result = crate::cancellation::scope(&token, || {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "sleep 2 & printf ready"]);
            run_with_timeout(cmd, None, Duration::from_secs(5)).unwrap()
        });
        cancel.join().unwrap();
        assert!(
            result.interrupted,
            "cancellation was ignored after the shell exited"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn timeout_covers_descendants_holding_pipes_after_shell_exit() {
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 2 & printf ready"]);
        let result = run_with_timeout(cmd, None, Duration::from_millis(150)).unwrap();
        assert!(
            result.timed_out,
            "timeout was ignored after the shell exited"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
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
    fn timeout_stops_a_child_that_keeps_stdin_open_without_reading() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 2"]);
        let data = vec![b'x'; 4 * 1024 * 1024];
        let started = Instant::now();
        let result = run_with_timeout(cmd, Some(&data), Duration::from_millis(150)).unwrap();
        assert!(result.timed_out);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn successful_shell_collects_late_descendant_output() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "(sleep 0.1; printf late) & printf early"]);
        let result = run_with_timeout(cmd, None, Duration::from_secs(2)).unwrap();
        assert_eq!(result.status, Some(0));
        assert_eq!(result.stdout, "earlylate");
        assert!(!result.timed_out);
        assert!(!result.interrupted);
    }

    #[test]
    fn output_reader_can_stop_without_waiting_for_eof() {
        let (reader, _writer_kept_open) = std::os::unix::net::UnixStream::pair().unwrap();
        set_nonblocking(&reader).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || drain(reader, &reader_stop));
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
    fn run_with_timeout_captures_output_and_exit_code() -> std::io::Result<()> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo out; echo err >&2; exit 3");
        let r = run_with_timeout(cmd, None, Duration::from_secs(5))?;
        assert_eq!(r.status, Some(3));
        assert_eq!(r.stdout.trim(), "out");
        assert_eq!(r.stderr.trim(), "err");
        let (text, is_error) = render_result(&r, Duration::from_secs(5));
        assert!(is_error);
        assert!(text.contains("[stderr]"));
        assert!(text.contains("[exit code: 3]"));
        Ok(())
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

    #[test]
    fn successful_commands_keep_stderr_warnings() -> std::io::Result<()> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("echo built; echo 'warning: unused variable' >&2");
        let result = run_with_timeout(cmd, None, Duration::from_secs(5))?;
        let (text, is_error) = render_result(&result, Duration::from_secs(5));
        assert!(!is_error);
        assert_eq!(text, "built\n[stderr]\nwarning: unused variable");

        let mut quiet = Command::new("sh");
        quiet.arg("-c").arg("printf ok");
        let result = run_with_timeout(quiet, None, Duration::from_secs(5))?;
        assert_eq!(render_result(&result, Duration::from_secs(5)).0, "ok");
        Ok(())
    }

    #[test]
    fn run_with_timeout_kills_hung_process() -> std::io::Result<()> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & wait");
        let start = Instant::now();
        let r = run_with_timeout(cmd, None, Duration::from_millis(200))?;
        assert!(r.timed_out);
        assert!(start.elapsed() < Duration::from_secs(5));
        Ok(())
    }

    #[test]
    fn stdin_is_delivered() -> std::io::Result<()> {
        let cmd = Command::new("cat");
        let r = run_with_timeout(cmd, Some(b"{\"x\":1}"), Duration::from_secs(5))?;
        assert_eq!(r.stdout, "{\"x\":1}");
        assert_eq!(r.status, Some(0));
        Ok(())
    }

    #[test]
    fn tool_names_are_provider_safe() {
        assert!(valid_tool_name("search_web-2"));
        assert!(!valid_tool_name(""));
        assert!(!valid_tool_name("has spaces"));
        assert!(!valid_tool_name(&"x".repeat(65)));
    }

    #[test]
    fn process_capture_is_bounded() -> std::io::Result<()> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("yes x | head -c 100000");
        let result = run_with_timeout(cmd, None, Duration::from_secs(5))?;
        assert!(result.stdout.len() < 70_000);
        assert!(result.stdout.ends_with("[process output truncated]"));
        Ok(())
    }
}
