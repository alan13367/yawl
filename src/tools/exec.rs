//! The one child-process runner: `shell`, custom tools, Git inspection, the
//! Git dashboard, and `/undo` checkpoints all run commands through it.
//!
//! Commands run as their own process tree (a process group on Unix, a Job
//! Object on Windows) with piped output, a deadline, and live cancellation.
//! Results follow one contract: stdout first, then any stderr under a
//! `[stderr]` marker; a nonzero exit marks an error.
//!
//! This module holds what is shared: the result and capture types and the
//! output rules. `unix` and `windows` own the process mechanics, and
//! `git_bash` holds the platform-independent logic that finds and quotes for
//! Git for Windows' bash.

use std::io::Read;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(any(windows, test))]
mod git_bash;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

// `nonblocking(pipe)` makes reads on a child's output pipe return
// `WouldBlock` instead of waiting, so a reader can keep checking whether it
// should stop.
#[cfg(unix)]
pub(crate) use platform::isolate_from_terminal;
pub(crate) use platform::nonblocking;
#[cfg(windows)]
pub(crate) use platform::{Job, spawn_in_job};

/// Bytes kept from the start and from the end of each output stream. Build
/// and test tools print their failures and summaries last, so the end is
/// kept as well as the beginning.
const CAPTURE_HEAD_BYTES: usize = 32 * 1024;
const CAPTURE_TAIL_BYTES: usize = 32 * 1024;

/// How tool descriptions name the shell invocation.
pub(crate) const SHELL_INVOCATION: &str = if cfg!(windows) { "bash -c" } else { "sh -c" };

/// The command that runs `script` in the shell: `sh -c` on Unix and Git for
/// Windows' `bash -c` on Windows. `path`, when given, replaces `PATH`.
///
/// # Errors
///
/// On Windows, fails with a message that says how to install Git for Windows
/// when bash cannot be found, and when the script exceeds the Windows
/// command-line limit.
pub(crate) fn shell_command(
    script: &str,
    path: Option<&std::ffi::OsStr>,
) -> std::io::Result<Command> {
    platform::shell_command(script, path)
}

/// A path in the form the shell expects: unchanged on Unix, with forward
/// slashes on Windows.
pub(crate) fn shell_path(path: &std::path::Path) -> std::ffi::OsString {
    platform::shell_path(path)
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

/// Runs `cmd` as its own process tree with piped output.
///
/// The run ends when the child has exited and both output pipes reached
/// end of file, when the deadline passes, or when the current job is
/// interrupted; the last two kill the whole process tree. Completion waits
/// for end of file because a shell can exit while a background descendant
/// still writes, and that descendant cannot hold the run open past the
/// deadline or an interrupt either.
pub(crate) fn run(cmd: Command, options: RunOptions<'_>) -> std::io::Result<ExecResult> {
    platform::run(cmd, options)
}

/// Reads `reader` into a bounded capture until end of file or until `stop`
/// is set. `idle` waits briefly when the reader has nothing to give.
fn drain_with<R: Read>(mut reader: R, keep: Keep, stop: &AtomicBool, idle: impl Fn(&R)) -> String {
    let mut capture = Capture::new(keep);
    let mut chunk = [0u8; 8 * 1024];
    while !stop.load(Ordering::Acquire) {
        let read = match reader.read(&mut chunk) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                idle(&reader);
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
    use std::time::Instant;

    /// `sh -c script` on Unix, Git Bash on Windows.
    fn sh(script: &str) -> Command {
        shell_command(script, None).expect("a shell is available")
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
            run_with_timeout(sh("sleep 2 & printf ready"), None, Duration::from_secs(5)).unwrap()
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
        let result = run_with_timeout(
            sh("sleep 2 & printf ready"),
            None,
            Duration::from_millis(150),
        )
        .unwrap();
        assert!(
            result.timed_out,
            "timeout was ignored after the shell exited"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn timeout_stops_a_child_that_keeps_stdin_open_without_reading() {
        let data = vec![b'x'; 4 * 1024 * 1024];
        let started = Instant::now();
        let result =
            run_with_timeout(sh("sleep 2"), Some(&data), Duration::from_millis(150)).unwrap();
        assert!(result.timed_out);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn successful_shell_collects_late_descendant_output() {
        let result = run_with_timeout(
            sh("(sleep 0.1; printf late) & printf early"),
            None,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(result.status, Some(0));
        assert_eq!(result.stdout, "earlylate");
        assert!(!result.timed_out);
        assert!(!result.interrupted);
    }

    #[test]
    fn run_with_timeout_captures_output_and_exit_code() -> std::io::Result<()> {
        let cmd = sh("echo out; echo err >&2; exit 3");
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
    fn successful_commands_keep_stderr_warnings() -> std::io::Result<()> {
        let cmd = sh("echo built; echo 'warning: unused variable' >&2");
        let result = run_with_timeout(cmd, None, Duration::from_secs(5))?;
        let (text, is_error) = render_result(&result, Duration::from_secs(5));
        assert!(!is_error);
        assert_eq!(text, "built\n[stderr]\nwarning: unused variable");

        let result = run_with_timeout(sh("printf ok"), None, Duration::from_secs(5))?;
        assert_eq!(render_result(&result, Duration::from_secs(5)).0, "ok");
        Ok(())
    }

    #[test]
    fn run_with_timeout_kills_hung_process() -> std::io::Result<()> {
        let start = Instant::now();
        let r = run_with_timeout(sh("sleep 30 & wait"), None, Duration::from_millis(200))?;
        assert!(r.timed_out);
        assert!(start.elapsed() < Duration::from_secs(5));
        Ok(())
    }

    #[test]
    fn stdin_is_delivered() -> std::io::Result<()> {
        let r = run_with_timeout(sh("cat"), Some(b"{\"x\":1}"), Duration::from_secs(5))?;
        assert_eq!(r.stdout, "{\"x\":1}");
        assert_eq!(r.status, Some(0));
        Ok(())
    }

    #[test]
    fn scripts_reach_the_shell_unmangled() -> std::io::Result<()> {
        let cmd = sh(r#"printf '%s\n' 'ls*' "two words" 'back\slash' "quo\"te""#);
        let r = run_with_timeout(cmd, None, Duration::from_secs(5))?;
        assert_eq!(r.stdout, "ls*\ntwo words\nback\\slash\nquo\"te\n");
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
        let result = run_with_timeout(sh("yes x | head -c 100000"), None, Duration::from_secs(5))?;
        assert!(result.stdout.len() < 70_000);
        assert!(result.stdout.contains("output truncated:"));
        Ok(())
    }

    #[test]
    fn long_output_keeps_its_final_lines() -> std::io::Result<()> {
        let cmd = sh("seq 1 30000; echo 'test result: FAILED. 1 failed'");
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
        let result = run(
            sh("yes x | head -c 300000"),
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
