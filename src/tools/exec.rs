//! The one child-process runner: `shell`, custom tools, Git inspection, the
//! Git dashboard, and `/undo` checkpoints all run commands through it.
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

/// Bytes kept from the start and from the end of each output stream. Build
/// and test tools print their failures and summaries last, so the end is
/// kept as well as the beginning.
const CAPTURE_HEAD_BYTES: usize = 32 * 1024;
const CAPTURE_TAIL_BYTES: usize = 32 * 1024;

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

/// How much of one output stream a run keeps. Whatever is not kept is
/// still read, so a chatty child never blocks on a full pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Keep {
    All,
    /// The first `n` bytes.
    Head(usize),
    /// The first and last 32 KiB, with a marker for what was dropped between.
    HeadAndTail,
}

pub(crate) struct RunOptions<'a> {
    /// Written to the child's stdin, which then closes. `None` gives the
    /// child an empty stdin.
    pub(crate) stdin: Option<&'a [u8]>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) stdout: Keep,
    pub(crate) stderr: Keep,
}

/// Runs a tool command with a deadline, keeping the start and end of its
/// output. See [`run`].
pub fn run_with_timeout(
    cmd: Command,
    stdin_data: Option<&[u8]>,
    timeout: Duration,
) -> std::io::Result<ExecResult> {
    run(
        cmd,
        RunOptions {
            stdin: stdin_data,
            timeout: Some(timeout),
            stdout: Keep::HeadAndTail,
            stderr: Keep::HeadAndTail,
        },
    )
}

/// Runs `cmd` in its own session and process group with piped output.
///
/// The run ends when the child has exited and both output pipes reached
/// end of file, when the deadline passes, or when the current job is
/// interrupted; the last two kill the whole process group. Completion waits
/// for end of file because a shell can exit while a background descendant
/// still writes, and that descendant cannot hold the run open past the
/// deadline or an interrupt either.
pub(crate) fn run(mut cmd: Command, options: RunOptions<'_>) -> std::io::Result<ExecResult> {
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

pub(crate) fn set_nonblocking(pipe: &impl AsRawFd) -> std::io::Result<()> {
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

fn drain(mut reader: impl Read + AsRawFd, keep: Keep, stop: &AtomicBool) -> String {
    let mut capture = Capture::new(keep);
    let mut chunk = [0u8; 8 * 1024];
    while !stop.load(Ordering::Acquire) {
        let read = match reader.read(&mut chunk) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_readable(&reader);
                continue;
            }
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        capture.push(&chunk[..read]);
    }
    capture.finish()
}

/// What a run keeps of one output stream, with a count of what it dropped.
struct Capture {
    keep: Keep,
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    omitted: u64,
}

impl Capture {
    fn new(keep: Keep) -> Self {
        Self {
            keep,
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            omitted: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let head_limit = match self.keep {
            Keep::All => usize::MAX,
            Keep::Head(limit) => limit,
            Keep::HeadAndTail => CAPTURE_HEAD_BYTES,
        };
        let room = head_limit.saturating_sub(self.head.len());
        let (head, rest) = bytes.split_at(room.min(bytes.len()));
        self.head.extend_from_slice(head);
        if self.keep != Keep::HeadAndTail {
            self.omitted = self.omitted.saturating_add(rest.len() as u64);
            return;
        }
        self.tail.extend(rest);
        let excess = self.tail.len().saturating_sub(CAPTURE_TAIL_BYTES);
        if excess > 0 {
            self.tail.drain(..excess);
            self.omitted = self.omitted.saturating_add(excess as u64);
        }
    }

    fn finish(self) -> String {
        let tail = Vec::from(self.tail);
        // A bounded head keeps its exact bytes: callers detect a partial
        // capture from its length.
        if self.omitted == 0 || self.keep != Keep::HeadAndTail {
            let mut bytes = self.head;
            bytes.extend_from_slice(&tail);
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        // Cut on character boundaries so the gap leaves no replacement
        // characters behind.
        let head_end = match std::str::from_utf8(&self.head) {
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            _ => self.head.len(),
        };
        let head = String::from_utf8_lossy(&self.head[..head_end]);
        let tail_start = tail
            .iter()
            .take(3)
            .take_while(|byte| (**byte & 0b1100_0000) == 0b1000_0000)
            .count();
        format!(
            "{head}\n[... output truncated: {} bytes omitted ...]\n{}",
            self.omitted + (self.head.len() - head_end + tail_start) as u64,
            String::from_utf8_lossy(&tail[tail_start..]),
        )
    }
}

#[cfg(test)]
mod tests {
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
        assert!(result.stdout.contains("output truncated:"));
        Ok(())
    }

    #[test]
    fn long_output_keeps_its_final_lines() -> std::io::Result<()> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("seq 1 30000; echo 'test result: FAILED. 1 failed'");
        let result = run_with_timeout(cmd, None, Duration::from_secs(30))?;
        assert!(result.stdout.starts_with("1\n2\n3\n"));
        assert!(
            result
                .stdout
                .ends_with("30000\ntest result: FAILED. 1 failed\n"),
            "{}",
            &result.stdout[result.stdout.len().saturating_sub(80)..]
        );
        Ok(())
    }

    #[test]
    fn bounded_capture_still_drains_the_pipe() -> std::io::Result<()> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("yes x | head -c 300000");
        let result = run(
            cmd,
            RunOptions {
                stdin: None,
                timeout: Some(Duration::from_secs(10)),
                stdout: Keep::Head(256),
                stderr: Keep::All,
            },
        )?;
        assert_eq!(result.status, Some(0), "the writer must not block");
        assert_eq!(result.stdout, "x\n".repeat(128));
        Ok(())
    }

    #[test]
    fn capture_cuts_multibyte_characters_cleanly() {
        let mut capture = Capture::new(Keep::HeadAndTail);
        let text = "é".repeat(CAPTURE_HEAD_BYTES);
        capture.push(text.as_bytes());
        capture.push(b"x");
        let output = capture.finish();
        assert!(!output.contains('\u{fffd}'));
        assert!(output.contains("output truncated:"));
        assert!(output.ends_with('x'));
    }
}
