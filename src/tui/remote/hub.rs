//! State shared between the UI thread and the remote-control server threads.

use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Mutex, MutexGuard};

use super::Snapshot;
use crate::tui::events::{Event, EventReader, Key};

/// Wrong pairing codes tolerated before the session shuts itself down.
pub(super) const MAX_PAIRING_FAILURES: u8 = 5;
/// Outgoing messages buffered per client. A phone that falls this far behind
/// is dropped and receives a full frame when it reconnects.
const CLIENT_BUFFER: usize = 256;
/// Smallest frame accepted from a device. The Git dashboard and its setup
/// screen render at least this size; the page shrinks its font to match.
const MIN_COLUMNS: u16 = 40;
const MIN_ROWS: u16 = 10;
/// Remote input buffered before further requests are rejected.
const MAX_PENDING_INPUT: usize = 1 << 20;

pub(super) enum Outgoing {
    Frame(String),
    Clipboard(String),
    Bell,
    /// The browser tab title changed.
    Title(String),
    /// Another device took control; this client should stop reconnecting.
    Replaced,
}

/// Why an event stream could not take control.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum AttachError {
    /// The session ended or was replaced.
    Gone,
    /// Another device holds control and the request did not ask to take it.
    Busy,
}

pub(super) enum PairResult {
    Paired(String),
    Rejected { remaining: u8 },
    Locked,
}

struct Client {
    id: u64,
    /// Proves a request comes from this connection rather than another
    /// device sharing the pairing cookie.
    token: String,
    sender: SyncSender<Outgoing>,
}

struct Session {
    generation: u64,
    address: SocketAddr,
    code: String,
    token: String,
    failures: u8,
    controlled: bool,
    client: Option<Client>,
    peer: Option<IpAddr>,
    size: Option<(u16, u16)>,
}

#[derive(Default)]
struct State {
    session: Option<Session>,
    generation: u64,
    next_client: u64,
    input: VecDeque<u8>,
    revision: u64,
    notices: Vec<String>,
    /// Browser tab title, kept across sessions so a new stream starts with
    /// it before the next draw.
    title: String,
}

#[derive(Default)]
pub(super) struct Hub {
    state: Mutex<State>,
    wake: Option<Wake>,
}

/// A non-blocking self-pipe that wakes the UI thread's input wait as soon as
/// remote input arrives, instead of after the raw-mode read timeout.
struct Wake {
    read: OwnedFd,
    write: OwnedFd,
}

impl Wake {
    fn new() -> io::Result<Self> {
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

    fn notify(&self) {
        // A full pipe already guarantees a pending wake-up.
        // SAFETY: writes one byte from a valid buffer to an owned descriptor.
        let _ = unsafe { libc::write(self.write.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }

    fn drain(&self) {
        let mut buf = [0u8; 64];
        // SAFETY: reads into a valid local buffer from an owned, non-blocking
        // descriptor until it is empty.
        while unsafe { libc::read(self.read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {
        }
    }
}

impl Hub {
    /// A hub whose remote input can interrupt the UI thread's input wait.
    pub(super) fn with_wake() -> Self {
        Self {
            state: Mutex::default(),
            wake: Wake::new().ok(),
        }
    }

    /// Descriptor that becomes readable when remote input is queued.
    pub(super) fn wake_fd(&self) -> Option<RawFd> {
        self.wake.as_ref().map(|wake| wake.read.as_raw_fd())
    }

    pub(super) fn drain_wake(&self) {
        if let Some(wake) = &self.wake {
            wake.drain();
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn session(state: &mut State, generation: u64) -> Option<&mut Session> {
        state
            .session
            .as_mut()
            .filter(|session| session.generation == generation)
    }

    /// The session, only while `client` is its controlling connection.
    fn controlling<'a>(
        state: &'a mut State,
        generation: u64,
        client: &str,
    ) -> Option<&'a mut Session> {
        Self::session(state, generation).filter(|session| {
            session.controlled
                && session.client.as_ref().is_some_and(|active| {
                    constant_time_eq(active.token.as_bytes(), client.as_bytes())
                })
        })
    }

    /// Opens a new session, replacing any previous one, and returns its
    /// generation.
    pub(super) fn start(&self, address: SocketAddr, code: String, token: String) -> u64 {
        let mut state = self.lock();
        state.generation += 1;
        state.revision += 1;
        state.input.clear();
        state.session = Some(Session {
            generation: state.generation,
            address,
            code,
            token,
            failures: 0,
            controlled: false,
            client: None,
            peer: None,
            size: None,
        });
        state.generation
    }

    /// Ends the session, returning whether one was running. Dropping the
    /// client sender closes its event stream.
    pub(super) fn stop(&self, notice: Option<&str>) -> bool {
        let mut state = self.lock();
        let stopped = state.session.take().is_some();
        if stopped {
            state.revision += 1;
            state.input.clear();
            if let Some(notice) = notice {
                state.notices.push(notice.to_string());
            }
        }
        stopped
    }

    pub(super) fn is_current(&self, generation: u64) -> bool {
        self.lock()
            .session
            .as_ref()
            .is_some_and(|session| session.generation == generation)
    }

    pub(super) fn pairing(&self) -> Option<(SocketAddr, String)> {
        self.lock()
            .session
            .as_ref()
            .map(|session| (session.address, session.code.clone()))
    }

    pub(super) fn snapshot(&self) -> Snapshot {
        let state = self.lock();
        let session = state.session.as_ref();
        Snapshot {
            address: session.map(|session| session.address),
            controlled: session.is_some_and(|session| session.controlled),
            connected: session.is_some_and(|session| session.client.is_some()),
            peer: session.and_then(|session| session.peer),
            size: session.and_then(|session| session.size),
            revision: state.revision,
        }
    }

    pub(super) fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().notices)
    }

    /// Moves queued remote input into `buf`, returning the byte count.
    pub(super) fn take_input(&self, buf: &mut [u8]) -> usize {
        let mut state = self.lock();
        let count = buf.len().min(state.input.len());
        for (slot, byte) in buf.iter_mut().zip(state.input.drain(..count)) {
            *slot = byte;
        }
        count
    }

    /// Filters bytes typed on the host terminal. While a remote device has
    /// control, host input is discarded and Ctrl+C ends the session.
    /// Returns whether the bytes should reach the UI.
    pub(super) fn accept_host_input(&self, bytes: &[u8]) -> bool {
        let state = self.lock();
        if !state
            .session
            .as_ref()
            .is_some_and(|session| session.controlled)
        {
            return true;
        }
        if contains_ctrl_c(bytes) {
            drop(state);
            self.stop(Some(
                "Remote control stopped. This terminal has control again.",
            ));
        }
        false
    }

    /// Sends a message to the attached client, returning whether it was
    /// delivered. A slow or vanished client is detached.
    pub(super) fn send(&self, message: Outgoing) -> bool {
        let mut state = self.lock();
        let Some(session) = state.session.as_mut() else {
            return false;
        };
        let Some(client) = session.client.as_ref() else {
            return false;
        };
        match client.sender.try_send(message) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                session.client = None;
                state.revision += 1;
                false
            }
        }
    }

    /// Records the browser tab title, forwarding a change to the attached
    /// client.
    pub(super) fn set_title(&self, title: &str) {
        let mut state = self.lock();
        if state.title == title {
            return;
        }
        state.title = title.to_string();
        drop(state);
        self.send(Outgoing::Title(title.to_string()));
    }

    pub(super) fn title(&self) -> String {
        self.lock().title.clone()
    }

    pub(super) fn authorized(&self, generation: u64, token: &str) -> bool {
        let mut state = self.lock();
        Self::session(&mut state, generation)
            .is_some_and(|session| constant_time_eq(session.token.as_bytes(), token.as_bytes()))
    }

    pub(super) fn pair(&self, generation: u64, code: &str) -> PairResult {
        let mut state = self.lock();
        let Some(session) = Self::session(&mut state, generation) else {
            return PairResult::Locked;
        };
        if constant_time_eq(session.code.as_bytes(), code.trim().as_bytes()) {
            return PairResult::Paired(session.token.clone());
        }
        session.failures += 1;
        let remaining = MAX_PAIRING_FAILURES.saturating_sub(session.failures);
        if remaining > 0 {
            return PairResult::Rejected { remaining };
        }
        drop(state);
        self.stop(Some(
            "Remote control stopped after too many wrong pairing codes. Run /remote to start again.",
        ));
        PairResult::Locked
    }

    /// Whether a connected device other than `client` holds control.
    pub(super) fn busy(&self, generation: u64, client: &str) -> bool {
        let mut state = self.lock();
        Self::session(&mut state, generation).is_some_and(|session| {
            session
                .client
                .as_ref()
                .is_some_and(|active| !constant_time_eq(active.token.as_bytes(), client.as_bytes()))
        })
    }

    /// Makes `peer` the controlling client. Any previous client is told it
    /// was replaced. Only an explicit `take` may replace another connected
    /// device; a stream that merely reconnects (`resume` names its previous
    /// client token) may replace only its own stale connection, so a device
    /// waking up never silently takes control back.
    pub(super) fn attach(
        &self,
        generation: u64,
        peer: IpAddr,
        token: String,
        take: bool,
        resume: &str,
    ) -> Result<(u64, Receiver<Outgoing>), AttachError> {
        let mut state = self.lock();
        state.next_client += 1;
        let id = state.next_client;
        let session = Self::session(&mut state, generation).ok_or(AttachError::Gone)?;
        let occupied = session
            .client
            .as_ref()
            .is_some_and(|active| !constant_time_eq(active.token.as_bytes(), resume.as_bytes()));
        if occupied && !take {
            return Err(AttachError::Busy);
        }
        let (sender, receiver) = mpsc::sync_channel(CLIENT_BUFFER);
        if let Some(previous) = session.client.replace(Client { id, token, sender }) {
            let _ = previous.sender.try_send(Outgoing::Replaced);
        }
        session.controlled = true;
        session.peer = Some(peer);
        state.revision += 1;
        Ok((id, receiver))
    }

    pub(super) fn detach(&self, generation: u64, id: u64) {
        let mut state = self.lock();
        let Some(session) = Self::session(&mut state, generation) else {
            return;
        };
        if session
            .client
            .as_ref()
            .is_some_and(|client| client.id == id)
        {
            session.client = None;
            state.revision += 1;
        }
    }

    /// Queues input from the controlling connection. Returns false for any
    /// other client, including a device that was replaced, or when the queue
    /// is full.
    pub(super) fn push_input(&self, generation: u64, client: &str, bytes: &[u8]) -> bool {
        let mut state = self.lock();
        let controlling = Self::controlling(&mut state, generation, client).is_some();
        if !controlling || state.input.len() + bytes.len() > MAX_PENDING_INPUT {
            return false;
        }
        state.input.extend(bytes);
        drop(state);
        if let Some(wake) = &self.wake {
            wake.notify();
        }
        true
    }

    /// Sets the frame size from the controlling connection. Returns false
    /// for any other client.
    pub(super) fn resize(&self, generation: u64, client: &str, columns: u16, rows: u16) -> bool {
        let mut state = self.lock();
        let Some(session) = Self::controlling(&mut state, generation, client) else {
            return false;
        };
        let size = Some((columns.clamp(MIN_COLUMNS, 500), rows.clamp(MIN_ROWS, 300)));
        if session.size != size {
            session.size = size;
            state.revision += 1;
        }
        true
    }
}

/// Whether host bytes contain Ctrl+C, either as the 0x03 byte or as the
/// kitty keyboard protocol's `CSI 99;5u`, which terminals send instead once
/// the protocol is enabled.
fn contains_ctrl_c(bytes: &[u8]) -> bool {
    let mut remaining = bytes;
    let mut reader = EventReader::new(&mut remaining);
    loop {
        match reader.read_event() {
            Ok(Event::Key(Key::Ctrl('c'))) => return true,
            Ok(Event::Tick) if !reader.has_pending() => return false,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> (Hub, u64) {
        let hub = Hub::default();
        let generation = hub.start(
            "127.0.0.1:7474".parse().expect("address"),
            "123456".into(),
            "token".into(),
        );
        (hub, generation)
    }

    fn peer() -> IpAddr {
        "100.64.0.2".parse().expect("peer")
    }

    #[test]
    fn host_input_passes_through_until_a_device_attaches() {
        let (hub, generation) = hub();
        assert!(hub.accept_host_input(b"a"));
        let _client = hub
            .attach(generation, peer(), "a".into(), true, "")
            .expect("attach");
        assert!(!hub.accept_host_input(b"a"));
        assert!(hub.snapshot().controlled);
    }

    #[test]
    fn host_ctrl_c_ends_remote_control() {
        let (hub, generation) = hub();
        let _client = hub
            .attach(generation, peer(), "a".into(), true, "")
            .expect("attach");
        assert!(!hub.accept_host_input(&[0x03]));
        assert!(!hub.snapshot().controlled);
        assert!(!hub.is_current(generation));
        assert_eq!(hub.take_notices().len(), 1);
        assert!(hub.accept_host_input(&[0x03]));
    }

    #[test]
    fn kitty_encoded_host_ctrl_c_ends_remote_control() {
        let (hub, generation) = hub();
        let _client = hub
            .attach(generation, peer(), "a".into(), true, "")
            .expect("attach");
        assert!(!hub.accept_host_input(b"\x1b[99;3u"));
        assert!(hub.is_current(generation));
        assert!(!hub.accept_host_input(b"x\x1b[99;5u"));
        assert!(!hub.is_current(generation));
    }

    #[test]
    fn a_reconnecting_device_does_not_take_control_back() {
        let (hub, generation) = hub();
        let (_, _phone) = hub
            .attach(generation, peer(), "phone".into(), true, "")
            .expect("phone");
        let (_, laptop) = hub
            .attach(generation, peer(), "laptop".into(), true, "")
            .expect("laptop");
        // The phone's automatic reconnect names its old stream, which is no
        // longer the controller.
        assert_eq!(
            hub.attach(generation, peer(), "phone-2".into(), false, "phone")
                .err(),
            Some(AttachError::Busy)
        );
        assert!(hub.busy(generation, "phone"));
        assert!(!hub.busy(generation, "laptop"));
        assert!(laptop.try_recv().is_err());
        // The controller itself may resume its own stale stream.
        let (_, _resumed) = hub
            .attach(generation, peer(), "laptop-2".into(), false, "laptop")
            .expect("resume");
        // An explicit take always succeeds.
        assert!(
            hub.attach(generation, peer(), "phone-3".into(), true, "")
                .is_ok()
        );
        assert_eq!(
            hub.attach(generation + 1, peer(), "x".into(), true, "")
                .err(),
            Some(AttachError::Gone)
        );
    }

    #[test]
    fn any_device_attaches_when_none_is_connected() {
        let (hub, generation) = hub();
        let (id, _) = hub
            .attach(generation, peer(), "a".into(), true, "")
            .expect("attach");
        hub.detach(generation, id);
        assert!(!hub.busy(generation, "b"));
        assert!(
            hub.attach(generation, peer(), "b".into(), false, "")
                .is_ok()
        );
    }

    #[test]
    fn remote_input_requires_control() {
        let (hub, generation) = hub();
        assert!(!hub.push_input(generation, "a", b"x"));
        let _client = hub
            .attach(generation, peer(), "a".into(), true, "")
            .expect("attach");
        assert!(hub.push_input(generation, "a", b"xy"));
        assert!(!hub.push_input(generation, "other", b"x"));
        let mut buf = [0u8; 1];
        assert_eq!(hub.take_input(&mut buf), 1);
        assert_eq!(&buf, b"x");
        assert!(!hub.push_input(generation + 1, "a", b"z"));
    }

    #[test]
    fn repeated_wrong_codes_stop_the_session() {
        let (hub, generation) = hub();
        for remaining in (1..MAX_PAIRING_FAILURES).rev() {
            assert!(matches!(
                hub.pair(generation, "000000"),
                PairResult::Rejected { remaining: left } if left == remaining
            ));
        }
        assert!(matches!(
            hub.pair(generation, "123456"),
            PairResult::Paired(_)
        ));
        assert!(matches!(hub.pair(generation, "000000"), PairResult::Locked));
        assert!(!hub.is_current(generation));
        assert!(matches!(hub.pair(generation, "123456"), PairResult::Locked));
    }

    #[test]
    fn a_new_client_replaces_the_previous_one() {
        let (hub, generation) = hub();
        let (first_id, first) = hub
            .attach(generation, peer(), "a".into(), true, "")
            .expect("first");
        let (_, second) = hub
            .attach(generation, peer(), "b".into(), true, "")
            .expect("second");
        assert!(matches!(first.try_recv(), Ok(Outgoing::Replaced)));
        hub.detach(generation, first_id);
        assert!(hub.snapshot().connected);
        // The replaced device keeps its pairing cookie but not control.
        assert!(!hub.push_input(generation, "a", b"x"));
        assert!(!hub.resize(generation, "a", 80, 24));
        assert!(hub.push_input(generation, "b", b"x"));
        assert!(hub.send(Outgoing::Bell));
        assert!(matches!(second.try_recv(), Ok(Outgoing::Bell)));
    }

    #[test]
    fn a_vanished_client_is_detached_on_send() {
        let (hub, generation) = hub();
        let (_, receiver) = hub
            .attach(generation, peer(), "a".into(), true, "")
            .expect("attach");
        drop(receiver);
        let revision = hub.snapshot().revision;
        assert!(!hub.send(Outgoing::Bell));
        let snapshot = hub.snapshot();
        assert!(!snapshot.connected);
        assert!(snapshot.controlled);
        assert!(snapshot.revision > revision);
    }

    #[test]
    fn resize_clamps_and_bumps_revision_only_on_change() {
        let (hub, generation) = hub();
        assert!(!hub.resize(generation, "a", 80, 24));
        let _client = hub
            .attach(generation, peer(), "a".into(), true, "")
            .expect("attach");
        assert!(hub.resize(generation, "a", 5, 1000));
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.size, Some((MIN_COLUMNS, 300)));
        assert!(hub.resize(generation, "a", 5, 1000));
        assert_eq!(hub.snapshot().revision, snapshot.revision);
    }
}
