//! Windows process runner: a Job Object per child tree, hidden consoles, and
//! pipes polled with `PeekNamedPipe` because anonymous pipes cannot be made
//! nonblocking.
//!
//! The Win32 surface is small on purpose: job creation and termination, one
//! suspended-start resume, and pipe peeking. Every handle is owned by a std
//! `OwnedHandle`, which closes it on drop.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

use super::git_bash::{self, GitBash};
use super::{ExecResult, Keep, RunOptions, drain_with};

/// How long a polled pipe sleeps when it has nothing to read.
const PIPE_POLL: Duration = Duration::from_millis(10);

/// The shell command for `script`: Git for Windows' `bash -c`, with `PATH`
/// replaced when given.
///
/// The script is passed as one raw, quoted argument. The MSYS2 runtime
/// glob-expands unquoted command-line arguments, and std leaves an argument
/// without spaces unquoted, so `ls*` would otherwise be mangled. `PATH` gains
/// Git's `usr\bin` first so `ls`, `grep`, and `sed` resolve even when only
/// `Git\cmd` is on the Windows `PATH`.
pub(super) fn shell_command(script: &str, path: Option<&OsStr>) -> io::Result<Command> {
    let bash = git_bash()?;
    let quoted = git_bash::quote_arg(script);
    git_bash::check_command_line(&bash.exe, &quoted).map_err(io::Error::other)?;
    let mut command = Command::new(&bash.exe);
    command.raw_arg("-c").raw_arg(quoted);
    let base = path
        .map(OsStr::to_owned)
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    command.env(
        "PATH",
        git_bash::path_with_tools(bash.tools_dir.as_deref(), &base),
    );
    Ok(command)
}

/// A path as bash sees it: Windows accepts `/`, and bash needs it.
pub(super) fn shell_path(path: &Path) -> OsString {
    OsString::from(git_bash::forward_slashes(&path.to_string_lossy()))
}

fn git_bash() -> io::Result<&'static GitBash> {
    static BASH: OnceLock<GitBash> = OnceLock::new();
    if let Some(bash) = BASH.get() {
        return Ok(bash);
    }
    // Only success is cached, so installing Git while Yawl runs is noticed.
    let found = git_bash::resolve(|name| std::env::var_os(name), |path| path.exists())
        .map_err(|message| io::Error::new(io::ErrorKind::NotFound, message))?;
    Ok(BASH.get_or_init(|| found))
}

/// Keeps `command` from showing a console window or taking Ctrl+C from
/// Yawl's console, and keeps credential helpers from prompting.
///
/// `CREATE_NO_WINDOW` gives the child a hidden console of its own. That
/// matches what `setsid` does on Unix for Ctrl+C, and no window pops up.
/// Unlike a session without a terminal, the hidden console still exists, so a
/// program that prompts on the console (an ssh password, for example) waits
/// until the deadline instead of failing at once. Git is told not to prompt,
/// and Git Credential Manager is told not to open its sign-in dialogs.
pub(crate) fn isolate_from_terminal(command: &mut Command) -> &mut Command {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never");
    command.creation_flags(CREATE_NO_WINDOW)
}

fn raw(handle: &impl AsRawHandle) -> HANDLE {
    handle.as_raw_handle() as HANDLE
}

/// A Job Object that owns a process tree. Closing the last handle kills every
/// process still in the job, so children die with Yawl even if Yawl is killed.
pub(crate) struct Job(OwnedHandle);

impl Job {
    /// Creates an unnamed job that kills its processes when closed.
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: Null security attributes and a null name request a default,
        // unnamed job; no pointer is retained.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `handle` is a new job handle that nothing else owns.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) });
        job.set_kill_on_close(true)?;
        Ok(job)
    }

    /// Turns the kill-when-closed limit on or off.
    fn set_kill_on_close(&self, enabled: bool) -> io::Result<()> {
        // SAFETY: The structure is plain data for which all zero bytes mean
        // "no limits".
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        if enabled {
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        // SAFETY: `info` is a live structure of the size passed, and the job
        // handle stays open for the call. The kernel copies the data.
        let ok = unsafe {
            SetInformationJobObject(
                raw(&self.0),
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Lets processes that are still in the job outlive it, as a detached
    /// Unix descendant would. Without this, closing the job kills them.
    pub(crate) fn disarm(&self) {
        let _ = self.set_kill_on_close(false);
    }

    /// Puts `process` in the job.
    fn assign(&self, process: &impl AsRawHandle) -> io::Result<()> {
        // SAFETY: Both handles are open for the call: the job through `self`
        // and the process through the borrowed child.
        if unsafe { AssignProcessToJobObject(raw(&self.0), raw(process)) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Kills every process in the job, descendants included. Terminated
    /// processes report exit code 1.
    pub(crate) fn terminate(&self) -> io::Result<()> {
        // SAFETY: The job handle is open for the call.
        if unsafe { TerminateJobObject(raw(&self.0), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// How many processes are still running in the job.
    pub(crate) fn active_processes(&self) -> io::Result<u32> {
        // SAFETY: The structure is plain data that the call overwrites.
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a live, writable structure of the size passed and
        // the job handle is open. The optional returned length is not wanted.
        let ok = unsafe {
            QueryInformationJobObject(
                raw(&self.0),
                JobObjectBasicAccountingInformation,
                std::ptr::from_mut(&mut info).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.ActiveProcesses)
    }
}

/// Starts `command` isolated from the terminal inside a new job.
///
/// The child starts suspended and joins the job before its first instruction,
/// so it cannot create a descendant that escapes the job first. std does not
/// expose the child's main thread, so it is found by walking the system's
/// threads and resumed. (`raw_attribute` with `PROC_THREAD_ATTRIBUTE_JOB_LIST`
/// would avoid this, but it is still unstable in std.) If anything fails after
/// the spawn, the child is killed and the error returned.
pub(crate) fn spawn_in_job(command: &mut Command) -> io::Result<(Child, Job)> {
    isolate_from_terminal(command);
    command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let job = Job::new()?;
    let mut child = command.spawn()?;
    if let Err(error) = job
        .assign(&child)
        .and_then(|()| resume_main_thread(child.id()))
    {
        let _ = job.terminate();
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok((child, job))
}

/// Resumes the one thread of the suspended process `pid`.
fn resume_main_thread(pid: u32) -> io::Result<()> {
    // SAFETY: A thread snapshot of every process; the flags and process id
    // take no pointers.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `snapshot` is a new handle that nothing else owns.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot as RawHandle) };
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        cntUsage: 0,
        th32ThreadID: 0,
        th32OwnerProcessID: 0,
        tpBasePri: 0,
        tpDeltaPri: 0,
        dwFlags: 0,
    };
    // SAFETY: `entry` is a live structure with `dwSize` set, as the call
    // requires, and the snapshot handle is open.
    let mut more = unsafe { Thread32First(raw(&snapshot), &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            return resume_thread(entry.th32ThreadID);
        }
        // SAFETY: Same as above.
        more = unsafe { Thread32Next(raw(&snapshot), &mut entry) } != 0;
    }
    Err(io::Error::other("could not find the new process's thread"))
}

fn resume_thread(thread_id: u32) -> io::Result<()> {
    // SAFETY: Opens a handle with the one right needed; no pointers.
    let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
    if thread == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `thread` is a new handle that nothing else owns.
    let thread = unsafe { OwnedHandle::from_raw_handle(thread as RawHandle) };
    // SAFETY: The thread handle is open and has THREAD_SUSPEND_RESUME access.
    if unsafe { ResumeThread(raw(&thread)) } == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn run(mut cmd: Command, options: RunOptions<'_>) -> io::Result<ExecResult> {
    cmd.stdin(if options.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let (mut child, job) = spawn_in_job(&mut cmd)?;

    let stdin = child.stdin.take();
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = job.terminate();
        let _ = child.wait();
        return Err(io::Error::other("child output pipes were not created"));
    };
    // Blocking writes cannot be made to give up, so the input goes out on its
    // own thread. It ends when the input is written, which also closes the
    // pipe and gives the reader its end of file, or when the pipe breaks:
    // killing the job closes the read end. The run never waits for it.
    if let (Some(mut pipe), Some(data)) = (stdin, options.stdin) {
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
    }
    let stop_readers = Arc::new(AtomicBool::new(false));
    let (stop_out, stop_err) = (Arc::clone(&stop_readers), Arc::clone(&stop_readers));
    let (keep_out, keep_err) = (options.stdout, options.stderr);
    let out_handle = std::thread::spawn(move || drain(PollingPipe(stdout), keep_out, &stop_out));
    let err_handle = std::thread::spawn(move || drain(PollingPipe(stderr), keep_err, &stop_err));
    // Waiting on the process handle is safe here: unlike a Unix pid, it
    // cannot be reused while the handle is open, so there is no signalling
    // hazard to order around.
    let (exited_tx, exited_rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let status = child.wait();
        let _ = exited_tx.send(());
        status
    });

    let deadline = options
        .timeout
        .and_then(|timeout| Instant::now().checked_add(timeout));
    let (mut exited, mut timed_out, mut interrupted) = (false, false, false);
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
        let wait = if exited {
            Duration::from_millis(5)
        } else {
            Duration::from_millis(25)
        };
        match exited_rx.recv_timeout(wait) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => exited = true,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    if interrupted || timed_out {
        let _ = job.terminate();
        // An escaped descendant may be outside the job. Do not wait
        // indefinitely for it to close an inherited pipe.
        stop_readers.store(true, Ordering::Release);
    } else {
        // Like a Unix descendant that detached from the group, a process that
        // closed its pipes and outlived the shell is left running.
        job.disarm();
    }
    let status = waiter
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("process waiter panicked")));
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

/// What `PeekNamedPipe` found.
enum Peek {
    /// This many bytes can be read without waiting.
    Ready(usize),
    Empty,
    /// Every writer has closed the pipe and nothing is left.
    Closed,
}

fn peek(pipe: &impl AsRawHandle) -> io::Result<Peek> {
    let mut available = 0u32;
    // SAFETY: The pipe handle is open for the call, `available` is a live
    // `u32`, and the null pointers say no data is copied or counted.
    let ok = unsafe {
        PeekNamedPipe(
            raw(pipe),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        return Ok(if available == 0 {
            Peek::Empty
        } else {
            Peek::Ready(available as usize)
        });
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
        return Ok(Peek::Closed);
    }
    Err(error)
}

/// A child pipe that reports `WouldBlock` instead of waiting, which is the
/// contract the Unix nonblocking descriptors give the readers.
struct PollingPipe<R>(R);

impl<R: Read + AsRawHandle> Read for PollingPipe<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match peek(&self.0)? {
            Peek::Closed => Ok(0),
            Peek::Empty => Err(io::ErrorKind::WouldBlock.into()),
            // Never ask for more than is there, so the read cannot wait.
            Peek::Ready(available) => {
                let length = buf.len().min(available);
                self.0.read(&mut buf[..length])
            }
        }
    }
}

/// Makes reads on `pipe` return `WouldBlock` instead of waiting.
pub(crate) fn nonblocking<R: Read + AsRawHandle + Send + 'static>(
    pipe: R,
) -> io::Result<impl Read + Send + 'static> {
    Ok(PollingPipe(pipe))
}

fn drain<R: Read + AsRawHandle>(reader: PollingPipe<R>, keep: Keep, stop: &AtomicBool) -> String {
    drain_with(reader, keep, stop, |_| std::thread::sleep(PIPE_POLL))
}

#[cfg(test)]
mod tests {
    use super::super::{run_with_timeout, shell_command};
    use super::*;

    #[test]
    fn a_job_reports_and_kills_its_processes() -> io::Result<()> {
        let mut command = shell_command("sleep 30", None)?;
        let (mut child, job) = spawn_in_job(&mut command)?;
        assert!(job.active_processes()? >= 1);
        job.terminate()?;
        child.wait()?;
        assert_eq!(job.active_processes()?, 0);
        Ok(())
    }

    #[test]
    fn scripts_reach_bash_unmangled() -> io::Result<()> {
        let command = shell_command(
            r#"printf '%s\n' 'ls*' "two words" 'back\slash' "quo\"te""#,
            None,
        )?;
        let result = run_with_timeout(command, None, Duration::from_secs(10))?;
        assert_eq!(result.stdout, "ls*\ntwo words\nback\\slash\nquo\"te\n");
        Ok(())
    }

    #[test]
    fn timeout_kills_the_whole_tree() -> io::Result<()> {
        let command = shell_command("sleep 100 & sleep 100", None)?;
        let started = Instant::now();
        let result = run_with_timeout(command, None, Duration::from_millis(500))?;
        assert!(result.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
        Ok(())
    }

    #[test]
    fn commands_get_no_credential_prompts() -> io::Result<()> {
        let command = shell_command(
            r#"printf '%s %s' "$GIT_TERMINAL_PROMPT" "$GCM_INTERACTIVE""#,
            None,
        )?;
        let result = run_with_timeout(command, None, Duration::from_secs(10))?;
        assert_eq!(result.stdout, "0 never");
        Ok(())
    }
}
