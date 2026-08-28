//! Session-bound background shell process management.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_ACTIVE: usize = 8;
const MAX_TRACKED: usize = 64;
const MAX_LOG_BYTES: usize = 256 * 1024;
const MAX_READ_BYTES: usize = 48 * 1024;
const READ_CHUNK_BYTES: usize = 8 * 1024;
const STOP_GRACE: Duration = Duration::from_secs(2);
const READER_DRAIN_GRACE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BackgroundId(String);

impl BackgroundId {
    pub(crate) fn new(number: u64) -> Self {
        Self(format!("bg-{number}"))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BackgroundId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BackgroundStatus {
    Starting,
    Running,
    Stopping,
    Exited(i32),
    Signaled,
    TimedOut,
    Stopped,
    Failed(String),
}

impl BackgroundStatus {
    pub(crate) fn is_active(&self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }

    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Exited(0) => "done",
            Self::Exited(_) => "failed",
            Self::Signaled => "signaled",
            Self::TimedOut => "timed out",
            Self::Stopped => "stopped",
            Self::Failed(_) => "failed",
        }
    }

    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Exited(code) => format!("exited with code {code}"),
            Self::Failed(error) => format!("failed: {error}"),
            other => other.label().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
}

impl OutputStream {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LogChunk {
    pub(crate) cursor: u64,
    pub(crate) stream: OutputStream,
    pub(crate) text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct BackgroundSnapshot {
    pub(crate) id: BackgroundId,
    pub(crate) pid: Option<u32>,
    pub(crate) command: String,
    pub(crate) name: Option<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) timeout: Option<Duration>,
    pub(crate) status: BackgroundStatus,
    pub(crate) started_at: Instant,
    pub(crate) settled_at: Option<Instant>,
}

impl BackgroundSnapshot {
    pub(crate) fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.command)
    }

    pub(crate) fn elapsed(&self, now: Instant) -> Duration {
        self.settled_at
            .unwrap_or(now)
            .saturating_duration_since(self.started_at)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BackgroundDetail {
    pub(crate) snapshot: BackgroundSnapshot,
    pub(crate) logs: Vec<LogChunk>,
    pub(crate) oldest_cursor: u64,
    pub(crate) next_cursor: u64,
}

#[derive(Debug)]
pub(crate) struct OutputRead {
    pub(crate) snapshot: BackgroundSnapshot,
    pub(crate) chunks: Vec<LogChunk>,
    pub(crate) stale_cursor: bool,
    pub(crate) next_cursor: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct StartSpec {
    pub(crate) command: String,
    pub(crate) name: Option<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) timeout: Option<Duration>,
}

#[derive(Debug)]
pub(crate) struct StartResult {
    pub(crate) id: BackgroundId,
    pub(crate) pid: u32,
}

struct Entry {
    id: BackgroundId,
    pid: Option<u32>,
    spec: StartSpec,
    status: BackgroundStatus,
    started_at: Instant,
    settled_at: Option<Instant>,
    logs: VecDeque<LogChunk>,
    log_bytes: usize,
    next_cursor: u64,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

struct OutputWorkers {
    done: Arc<AtomicBool>,
    stdout: JoinHandle<()>,
    stderr: JoinHandle<()>,
}

impl Entry {
    fn snapshot(&self) -> BackgroundSnapshot {
        BackgroundSnapshot {
            id: self.id.clone(),
            pid: self.pid,
            command: self.spec.command.clone(),
            name: self.spec.name.clone(),
            cwd: self.spec.cwd.clone(),
            timeout: self.spec.timeout,
            status: self.status.clone(),
            started_at: self.started_at,
            settled_at: self.settled_at,
        }
    }
}

#[derive(Default)]
struct State {
    next_id: u64,
    active: usize,
    entries: VecDeque<Entry>,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Clone)]
pub(crate) struct BackgroundProcessManager {
    inner: Arc<Inner>,
}

impl Default for BackgroundProcessManager {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
            }),
        }
    }
}

impl BackgroundProcessManager {
    pub(crate) fn active_count(&self) -> usize {
        self.lock_state().active
    }

    pub(crate) fn start(&self, spec: StartSpec) -> Result<StartResult, String> {
        let (id, stop, reaped) = {
            let mut state = self.lock_state();
            if state.active >= MAX_ACTIVE {
                return Err(format!(
                    "background process capacity is full ({MAX_ACTIVE} running)"
                ));
            }
            let mut reaped = Vec::new();
            while state.entries.len() >= MAX_TRACKED {
                let Some(index) = state
                    .entries
                    .iter()
                    .position(|entry| !entry.status.is_active())
                else {
                    return Err(format!(
                        "background process history is full ({MAX_TRACKED} tracked)"
                    ));
                };
                if let Some(mut entry) = state.entries.remove(index)
                    && let Some(worker) = entry.worker.take()
                {
                    reaped.push(worker);
                }
            }
            state.next_id = state.next_id.saturating_add(1);
            let id = BackgroundId::new(state.next_id);
            let stop = Arc::new(AtomicBool::new(false));
            state.entries.push_back(Entry {
                id: id.clone(),
                pid: None,
                spec: spec.clone(),
                status: BackgroundStatus::Starting,
                started_at: Instant::now(),
                settled_at: None,
                logs: VecDeque::new(),
                log_bytes: 0,
                next_cursor: 0,
                stop: stop.clone(),
                worker: None,
            });
            state.active = state.active.saturating_add(1);
            (id, stop, reaped)
        };
        for worker in reaped {
            let _ = worker.join();
        }

        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&spec.command)
            .current_dir(&spec.cwd)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.remove_failed_start(&id);
                return Err(format!("failed to spawn background shell: {error}"));
            }
        };
        let pid = child.id();
        let Some(stdout) = child.stdout.take() else {
            terminate_now(&mut child);
            self.remove_failed_start(&id);
            return Err("background shell stdout was not piped".into());
        };
        let Some(stderr) = child.stderr.take() else {
            terminate_now(&mut child);
            self.remove_failed_start(&id);
            return Err("background shell stderr was not piped".into());
        };
        if let Err(error) = set_nonblocking(&stdout) {
            terminate_now(&mut child);
            self.remove_failed_start(&id);
            return Err(format!(
                "failed to configure background shell stdout: {error}"
            ));
        }
        if let Err(error) = set_nonblocking(&stderr) {
            terminate_now(&mut child);
            self.remove_failed_start(&id);
            return Err(format!(
                "failed to configure background shell stderr: {error}"
            ));
        }
        {
            let mut state = self.lock_state();
            let Some(entry) = find_entry_mut(&mut state, id.as_str()) else {
                terminate_now(&mut child);
                return Err("background process entry disappeared during startup".into());
            };
            entry.pid = Some(pid);
            entry.status = BackgroundStatus::Running;
            self.inner.changed.notify_all();
        }

        let readers_done = Arc::new(AtomicBool::new(false));
        let stdout_manager = self.clone();
        let stdout_id = id.clone();
        let stdout_done = readers_done.clone();
        let stdout_worker = std::thread::spawn(move || {
            drain_stream(
                stdout,
                OutputStream::Stdout,
                &stdout_manager,
                &stdout_id,
                &stdout_done,
            );
        });
        let stderr_manager = self.clone();
        let stderr_id = id.clone();
        let stderr_done = readers_done.clone();
        let stderr_worker = std::thread::spawn(move || {
            drain_stream(
                stderr,
                OutputStream::Stderr,
                &stderr_manager,
                &stderr_id,
                &stderr_done,
            );
        });
        let monitor = self.clone();
        let monitor_id = id.clone();
        let timeout = spec.timeout;
        let worker = std::thread::spawn(move || {
            monitor.run_process(
                monitor_id,
                child,
                stop,
                timeout,
                OutputWorkers {
                    done: readers_done,
                    stdout: stdout_worker,
                    stderr: stderr_worker,
                },
            );
        });
        let worker = {
            let mut state = self.lock_state();
            if let Some(entry) = find_entry_mut(&mut state, id.as_str()) {
                entry.worker = Some(worker);
                None
            } else {
                Some(worker)
            }
        };
        if let Some(worker) = worker {
            let _ = worker.join();
            return Err("background process entry disappeared after startup".into());
        }
        Ok(StartResult { id, pid })
    }

    pub(crate) fn snapshots(&self) -> Vec<BackgroundSnapshot> {
        self.lock_state()
            .entries
            .iter()
            .map(Entry::snapshot)
            .collect()
    }

    pub(crate) fn detail(&self, id: &str) -> Result<BackgroundDetail, String> {
        let state = self.lock_state();
        let entry = find_entry(&state, id).ok_or_else(|| unknown_id(id))?;
        Ok(BackgroundDetail {
            snapshot: entry.snapshot(),
            logs: entry.logs.iter().cloned().collect(),
            oldest_cursor: entry
                .logs
                .front()
                .map_or(entry.next_cursor, |chunk| chunk.cursor),
            next_cursor: entry.next_cursor,
        })
    }

    pub(crate) fn read_output(
        &self,
        id: &str,
        cursor: u64,
        wait: Duration,
    ) -> Result<OutputRead, String> {
        let deadline = Instant::now().checked_add(wait);
        let mut state = self.lock_state();
        loop {
            let entry = find_entry(&state, id).ok_or_else(|| unknown_id(id))?;
            let oldest_cursor = entry
                .logs
                .front()
                .map_or(entry.next_cursor, |chunk| chunk.cursor);
            if entry.next_cursor > cursor
                || !entry.status.is_active()
                || wait.is_zero()
                || deadline.is_some_and(|deadline| Instant::now() >= deadline)
            {
                let effective_cursor = cursor.max(oldest_cursor);
                let mut bytes = 0usize;
                let mut chunks = Vec::new();
                let mut next_cursor = effective_cursor;
                for chunk in entry
                    .logs
                    .iter()
                    .filter(|chunk| chunk.cursor >= effective_cursor)
                {
                    if !chunks.is_empty() && bytes.saturating_add(chunk.text.len()) > MAX_READ_BYTES
                    {
                        break;
                    }
                    bytes = bytes.saturating_add(chunk.text.len());
                    next_cursor = chunk.cursor.saturating_add(1);
                    chunks.push(chunk.clone());
                }
                return Ok(OutputRead {
                    snapshot: entry.snapshot(),
                    chunks,
                    stale_cursor: cursor < oldest_cursor,
                    next_cursor,
                });
            }
            if crate::cancellation::interrupted() {
                return Err("output wait interrupted by user".into());
            }
            let Some(deadline) = deadline else {
                return Err("invalid output wait deadline".into());
            };
            let now = Instant::now();
            if now >= deadline {
                continue;
            }
            let slice = deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(100));
            state = match self.inner.changed.wait_timeout(state, slice) {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    pub(crate) fn stop(&self, id: &str) -> Result<BackgroundSnapshot, String> {
        let mut state = self.lock_state();
        let entry = find_entry_mut(&mut state, id).ok_or_else(|| unknown_id(id))?;
        if entry.status.is_active() {
            entry.stop.store(true, Ordering::Release);
            if matches!(entry.status, BackgroundStatus::Running) {
                entry.status = BackgroundStatus::Stopping;
            }
            self.inner.changed.notify_all();
        }
        Ok(entry.snapshot())
    }

    pub(crate) fn restart(&self, id: &str) -> Result<StartResult, String> {
        let spec = {
            let state = self.lock_state();
            let entry = find_entry(&state, id).ok_or_else(|| unknown_id(id))?;
            if entry.status.is_active() {
                return Err(format!("{id} is still active; stop it before restarting"));
            }
            entry.spec.clone()
        };
        self.start(spec)
    }

    pub(crate) fn remove(&self, id: &str) -> Result<(), String> {
        let worker = {
            let mut state = self.lock_state();
            let index = state
                .entries
                .iter()
                .position(|entry| entry.id.as_str() == id)
                .ok_or_else(|| unknown_id(id))?;
            if state.entries[index].status.is_active() {
                return Err(format!("{id} is still active; stop it before removing"));
            }
            state
                .entries
                .remove(index)
                .and_then(|mut entry| entry.worker.take())
        };
        if let Some(worker) = worker {
            let _ = worker.join();
        }
        Ok(())
    }

    pub(crate) fn shutdown_and_discard(&self) {
        let workers = {
            let mut state = self.lock_state();
            for entry in &mut state.entries {
                if entry.status.is_active() {
                    entry.stop.store(true, Ordering::Release);
                }
            }
            self.inner.changed.notify_all();
            state
                .entries
                .iter_mut()
                .filter_map(|entry| entry.worker.take())
                .collect::<Vec<_>>()
        };
        for worker in workers {
            let _ = worker.join();
        }
        let mut state = self.lock_state();
        state.entries.clear();
        state.active = 0;
        self.inner.changed.notify_all();
    }

    fn run_process(
        &self,
        id: BackgroundId,
        mut child: Child,
        stop: Arc<AtomicBool>,
        timeout: Option<Duration>,
        output: OutputWorkers,
    ) {
        let started = Instant::now();
        let process_group = child.id();
        let mut leader_status = None;
        let final_status = loop {
            if leader_status.is_none() {
                match child.try_wait() {
                    Ok(Some(status)) => leader_status = Some(status),
                    Ok(None) => {}
                    Err(error) => {
                        terminate_now(&mut child);
                        break BackgroundStatus::Failed(error.to_string());
                    }
                }
            }
            if !process_group_alive(process_group)
                && let Some(status) = leader_status.take()
            {
                break status
                    .code()
                    .map_or(BackgroundStatus::Signaled, BackgroundStatus::Exited);
            }
            if stop.load(Ordering::Acquire) {
                self.mark_stopping(&id);
                terminate_gracefully(&mut child, process_group);
                break BackgroundStatus::Stopped;
            }
            if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
                self.mark_stopping(&id);
                terminate_gracefully(&mut child, process_group);
                break BackgroundStatus::TimedOut;
            }
            std::thread::sleep(Duration::from_millis(30));
        };
        output.done.store(true, Ordering::Release);
        let _ = output.stdout.join();
        let _ = output.stderr.join();
        let mut state = self.lock_state();
        if let Some(entry) = find_entry_mut(&mut state, id.as_str()) {
            entry.status = final_status;
            entry.settled_at = Some(Instant::now());
            state.active = state.active.saturating_sub(1);
        }
        self.inner.changed.notify_all();
    }

    fn append_output(&self, id: &BackgroundId, stream: OutputStream, text: String) {
        let mut state = self.lock_state();
        let Some(entry) = find_entry_mut(&mut state, id.as_str()) else {
            return;
        };
        let cursor = entry.next_cursor;
        entry.next_cursor = entry.next_cursor.saturating_add(1);
        entry.log_bytes = entry.log_bytes.saturating_add(text.len());
        entry.logs.push_back(LogChunk {
            cursor,
            stream,
            text,
        });
        while entry.log_bytes > MAX_LOG_BYTES {
            let Some(dropped) = entry.logs.pop_front() else {
                break;
            };
            entry.log_bytes = entry.log_bytes.saturating_sub(dropped.text.len());
        }
        self.inner.changed.notify_all();
    }

    fn mark_stopping(&self, id: &BackgroundId) {
        let mut state = self.lock_state();
        if let Some(entry) = find_entry_mut(&mut state, id.as_str()) {
            entry.status = BackgroundStatus::Stopping;
        }
        self.inner.changed.notify_all();
    }

    fn remove_failed_start(&self, id: &BackgroundId) {
        let mut state = self.lock_state();
        if let Some(index) = state.entries.iter().position(|entry| &entry.id == id) {
            state.entries.remove(index);
            state.active = state.active.saturating_sub(1);
        }
        self.inner.changed.notify_all();
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn find_entry<'a>(state: &'a State, id: &str) -> Option<&'a Entry> {
    state.entries.iter().find(|entry| entry.id.as_str() == id)
}

fn find_entry_mut<'a>(state: &'a mut State, id: &str) -> Option<&'a mut Entry> {
    state
        .entries
        .iter_mut()
        .find(|entry| entry.id.as_str() == id)
}

fn unknown_id(id: &str) -> String {
    format!("unknown background process '{id}'")
}

fn drain_stream(
    mut reader: impl Read,
    stream: OutputStream,
    manager: &BackgroundProcessManager,
    id: &BackgroundId,
    done: &AtomicBool,
) {
    let mut bytes = [0u8; READ_CHUNK_BYTES];
    let mut pending = Vec::new();
    let mut done_since = None;
    loop {
        if done.load(Ordering::Acquire) {
            let finished = done_since.get_or_insert_with(Instant::now);
            if finished.elapsed() >= READER_DRAIN_GRACE {
                append_decoded(manager, id, stream, &mut pending, true);
                break;
            }
        }
        match reader.read(&mut bytes) {
            Ok(0) => {
                append_decoded(manager, id, stream, &mut pending, true);
                break;
            }
            Ok(read) => {
                pending.extend_from_slice(&bytes[..read]);
                append_decoded(manager, id, stream, &mut pending, false);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if done.load(Ordering::Acquire) {
                    append_decoded(manager, id, stream, &mut pending, true);
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(_) => {
                append_decoded(manager, id, stream, &mut pending, true);
                break;
            }
        }
    }
}

fn append_decoded(
    manager: &BackgroundProcessManager,
    id: &BackgroundId,
    stream: OutputStream,
    pending: &mut Vec<u8>,
    end_of_stream: bool,
) {
    let text = take_decoded_utf8(pending, end_of_stream);
    if !text.is_empty() {
        manager.append_output(id, stream, text);
    }
}

fn take_decoded_utf8(bytes: &mut Vec<u8>, end_of_stream: bool) -> String {
    let mut text = String::new();
    let mut consumed = 0;
    while consumed < bytes.len() {
        match std::str::from_utf8(&bytes[consumed..]) {
            Ok(valid) => {
                text.push_str(valid);
                consumed = bytes.len();
            }
            Err(error) => {
                let valid_end = consumed.saturating_add(error.valid_up_to());
                text.push_str(&String::from_utf8_lossy(&bytes[consumed..valid_end]));
                match error.error_len() {
                    Some(invalid) => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        consumed = valid_end.saturating_add(invalid);
                    }
                    None if end_of_stream => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        consumed = bytes.len();
                    }
                    None => {
                        consumed = valid_end;
                        break;
                    }
                }
            }
        }
    }
    if consumed > 0 {
        bytes.drain(..consumed);
    }
    text
}

fn set_nonblocking(reader: &impl AsRawFd) -> io::Result<()> {
    let file_descriptor = reader.as_raw_fd();
    // SAFETY: `file_descriptor` belongs to the live pipe handle borrowed for
    // this call, and `F_GETFL` does not access Rust-managed memory.
    let flags = unsafe { libc::fcntl(file_descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: The descriptor remains live for this call. `F_SETFL` updates its
    // status flags without taking ownership of the descriptor.
    if unsafe { libc::fcntl(file_descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn terminate_gracefully(child: &mut Child, process_group: u32) {
    signal_group(process_group, libc::SIGTERM);
    let deadline = Instant::now().checked_add(STOP_GRACE);
    loop {
        let _ = child.try_wait();
        if !process_group_alive(process_group) {
            let _ = child.wait();
            return;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    signal_group(process_group, libc::SIGKILL);
    let _ = child.wait();
}

fn terminate_now(child: &mut Child) {
    signal_group(child.id(), libc::SIGKILL);
    let _ = child.wait();
}

fn signal_group(process_group: u32, signal: libc::c_int) {
    if let Ok(process_group) = i32::try_from(process_group) {
        // SAFETY: The child was created as the leader of its own process
        // group. A negative PID addresses that group, and `kill` does not
        // impose Rust memory-safety requirements.
        unsafe {
            libc::kill(-process_group, signal);
        }
    }
}

fn process_group_alive(process_group: u32) -> bool {
    let Ok(process_group) = i32::try_from(process_group) else {
        return false;
    };
    // SAFETY: Signal zero performs existence and permission checks only. The
    // negative PID addresses the process group created for this command.
    let result = unsafe { libc::kill(-process_group, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;

    use super::*;

    fn spec(command: &str) -> StartSpec {
        StartSpec {
            command: command.into(),
            name: None,
            cwd: std::env::current_dir().expect("test working directory"),
            timeout: None,
        }
    }

    fn wait_until_settled(
        manager: &BackgroundProcessManager,
        id: &BackgroundId,
    ) -> BackgroundSnapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = manager
                .snapshots()
                .into_iter()
                .find(|snapshot| snapshot.id == *id)
                .expect("background process snapshot");
            if !snapshot.status.is_active() {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "background process did not settle"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn captures_incremental_stdout_and_stderr() {
        let manager = BackgroundProcessManager::default();
        let started = manager
            .start(spec("printf out; printf err >&2"))
            .expect("start background shell");
        let snapshot = wait_until_settled(&manager, &started.id);
        assert_eq!(snapshot.status, BackgroundStatus::Exited(0));
        let output = manager
            .read_output(started.id.as_str(), 0, Duration::ZERO)
            .expect("read background output");
        assert!(
            output
                .chunks
                .iter()
                .any(|chunk| chunk.stream == OutputStream::Stdout && chunk.text.contains("out"))
        );
        assert!(
            output
                .chunks
                .iter()
                .any(|chunk| chunk.stream == OutputStream::Stderr && chunk.text.contains("err"))
        );
        assert!(output.next_cursor > 0);
        manager.shutdown_and_discard();
    }

    #[test]
    fn stop_terminates_the_process_group() {
        let manager = BackgroundProcessManager::default();
        let started = manager
            .start(spec("sleep 30 & wait"))
            .expect("start background shell");
        manager.stop(started.id.as_str()).expect("request stop");
        let snapshot = wait_until_settled(&manager, &started.id);
        assert_eq!(snapshot.status, BackgroundStatus::Stopped);
        manager.shutdown_and_discard();
    }

    #[test]
    fn restart_keeps_history_and_allocates_a_new_id() {
        let manager = BackgroundProcessManager::default();
        let first = manager.start(spec("true")).expect("start first run");
        wait_until_settled(&manager, &first.id);
        let second = manager
            .restart(first.id.as_str())
            .expect("restart settled run");
        assert_ne!(first.id, second.id);
        wait_until_settled(&manager, &second.id);
        assert_eq!(manager.snapshots().len(), 2);
        manager.shutdown_and_discard();
    }

    #[test]
    fn explicit_timeout_settles_as_timed_out() {
        let manager = BackgroundProcessManager::default();
        let mut timed = spec("sleep 30");
        timed.timeout = Some(Duration::from_millis(100));
        let started = manager.start(timed).expect("start timed run");
        let snapshot = wait_until_settled(&manager, &started.id);
        assert_eq!(snapshot.status, BackgroundStatus::TimedOut);
        manager.shutdown_and_discard();
    }

    #[test]
    fn active_capacity_is_enforced_synchronously() {
        let manager = BackgroundProcessManager::default();
        let mut ids = Vec::new();
        for _ in 0..MAX_ACTIVE {
            ids.push(
                manager
                    .start(spec("trap 'exit 0' TERM; while :; do sleep 1; done"))
                    .expect("start within capacity")
                    .id,
            );
        }
        let error = manager
            .start(spec("sleep 30"))
            .expect_err("ninth active process should be rejected");
        assert!(error.contains("capacity is full"));
        for id in ids {
            manager.stop(id.as_str()).expect("request stop");
        }
        manager.shutdown_and_discard();
    }

    #[test]
    fn bounded_logs_report_a_stale_cursor() {
        let manager = BackgroundProcessManager::default();
        let started = manager
            .start(spec("yes x | head -c 300000"))
            .expect("start noisy process");
        wait_until_settled(&manager, &started.id);
        let detail = manager
            .detail(started.id.as_str())
            .expect("read retained detail");
        assert!(detail.oldest_cursor > 0);
        let output = manager
            .read_output(started.id.as_str(), 0, Duration::ZERO)
            .expect("read from stale cursor");
        assert!(output.stale_cursor);
        assert!(
            output
                .chunks
                .iter()
                .map(|chunk| chunk.text.len())
                .sum::<usize>()
                <= MAX_READ_BYTES + READ_CHUNK_BYTES
        );
        manager.shutdown_and_discard();
    }

    #[test]
    fn output_wait_wakes_for_new_content() {
        let manager = BackgroundProcessManager::default();
        let started = manager
            .start(spec("sleep 0.1; printf ready"))
            .expect("start delayed output");
        let output = manager
            .read_output(started.id.as_str(), 0, Duration::from_secs(2))
            .expect("wait for output");
        assert!(
            output
                .chunks
                .iter()
                .any(|chunk| chunk.text.contains("ready"))
        );
        wait_until_settled(&manager, &started.id);
        manager.shutdown_and_discard();
    }

    #[test]
    fn decoder_carries_incomplete_utf8_between_reads() {
        let encoded = "prefix € suffix".as_bytes();
        let split = "prefix ".len() + 1;
        let mut pending = encoded[..split].to_vec();

        assert_eq!(take_decoded_utf8(&mut pending, false), "prefix ");
        assert_eq!(pending, &encoded["prefix ".len()..split]);

        pending.extend_from_slice(&encoded[split..]);
        assert_eq!(take_decoded_utf8(&mut pending, false), "€ suffix");
        assert!(pending.is_empty());
    }

    #[test]
    fn stream_reader_can_stop_while_a_writer_keeps_the_pipe_open() {
        let (reader, mut writer) = UnixStream::pair().expect("create stream pair");
        set_nonblocking(&reader).expect("set reader nonblocking");
        writer.write_all(b"partial").expect("write stream content");
        let done = Arc::new(AtomicBool::new(false));
        let reader_done = done.clone();
        let manager = BackgroundProcessManager::default();
        let (settled_tx, settled_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            drain_stream(
                reader,
                OutputStream::Stdout,
                &manager,
                &BackgroundId::new(1),
                &reader_done,
            );
            let _ = settled_tx.send(());
        });

        std::thread::sleep(Duration::from_millis(50));
        done.store(true, Ordering::Release);
        settled_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("reader should observe cancellation without pipe EOF");
        worker.join().expect("join stream reader");
    }
}
