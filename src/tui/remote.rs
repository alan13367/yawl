//! `/remote`: hands this session to a browser on another device in the same
//! Tailscale network. The browser mirrors the terminal frames at its own size
//! and sends input bytes back; the host terminal is locked until Ctrl+C.

mod address;
mod http;
mod hub;
mod qr;
mod server;

use std::cell::RefCell;
use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;

use self::hub::{Hub, Outgoing};

use super::ViewState;
use super::terminal::Terminal;
use super::transcript::Entry;

/// Size used until the controlling device reports its own.
pub(super) const DEFAULT_SIZE: (u16, u16) = (48, 32);
/// Characters of the first prompt kept in the browser tab title.
const TITLE_PROMPT_CHARS: usize = 48;

/// A consistent view of the remote session for one draw.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Snapshot {
    pub(super) address: Option<SocketAddr>,
    /// A paired device has connected; the host terminal is locked.
    pub(super) controlled: bool,
    /// The controlling device's event stream is currently open.
    pub(super) connected: bool,
    pub(super) peer: Option<IpAddr>,
    pub(super) size: Option<(u16, u16)>,
    /// Changes whenever the remote side needs a full redraw.
    pub(super) revision: u64,
}

/// The remote-control session owned by the terminal. Dropping it closes any
/// connected browser.
pub(super) struct Remote {
    hub: Arc<Hub>,
    /// The pairing notice's transcript index and text, hidden once a device
    /// connects or the session ends. Only the UI thread touches it.
    pairing_notice: RefCell<Option<(usize, String)>>,
    /// Working directory name, which leads the browser tab title.
    directory: String,
}

impl Remote {
    pub(super) fn new() -> Self {
        Self {
            hub: Arc::new(Hub::with_wake()),
            pairing_notice: RefCell::new(None),
            directory: std::env::current_dir()
                .ok()
                .and_then(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_default(),
        }
    }

    /// Wraps the host terminal so remote bytes join the same event stream
    /// and host bytes are discarded while a device has control. Waiting
    /// polls both the terminal and remote input, so remote keystrokes are
    /// handled immediately rather than after the raw-mode read timeout.
    ///
    /// `host` must be unbuffered, such as [`FdInput`]: bytes held in a
    /// reader's buffer are invisible to `poll` and would stall.
    pub(super) fn terminal_input<R: Read + AsRawFd>(&self, host: R) -> RemoteInput<R> {
        RemoteInput {
            poll_fd: Some(host.as_raw_fd()),
            host,
            hub: Arc::clone(&self.hub),
        }
    }

    #[cfg(test)]
    fn input<R: Read>(&self, host: R) -> RemoteInput<R> {
        RemoteInput {
            host,
            hub: Arc::clone(&self.hub),
            poll_fd: None,
        }
    }

    pub(super) fn snapshot(&self) -> Snapshot {
        self.hub.snapshot()
    }

    pub(super) fn send_frame(&self, frame: String) -> bool {
        self.hub.send(Outgoing::Frame(frame))
    }

    pub(super) fn send_clipboard(&self, text: &str) -> bool {
        self.hub.send(Outgoing::Clipboard(text.to_string()))
    }

    pub(super) fn send_bell(&self) -> bool {
        self.hub.send(Outgoing::Bell)
    }

    /// Names the browser tab after this session so several remote sessions
    /// can be told apart. Unchanged titles are not resent.
    pub(super) fn update_title(&self, state: &ViewState) {
        self.hub
            .set_title(&session_title(&self.directory, state.transcript.entries()));
    }

    /// Moves session notices (such as a host Ctrl+C or a pairing lockout)
    /// into the transcript, and hides the pairing notice once it is no longer
    /// needed. Returns whether anything changed.
    pub(super) fn poll(&self, state: &mut ViewState) -> bool {
        let mut changed = false;
        let snapshot = self.hub.snapshot();
        if snapshot.controlled || snapshot.address.is_none() {
            changed |= self.hide_pairing_notice(state);
        }
        for notice in self.hub.take_notices() {
            state.notice(notice);
            changed = true;
        }
        changed
    }

    fn show_pairing_notice(&self, state: &mut ViewState, text: String) {
        self.hide_pairing_notice(state);
        state.notice(text.clone());
        let index = state.transcript.entries().len() - 1;
        self.pairing_notice.replace(Some((index, text)));
    }

    fn hide_pairing_notice(&self, state: &mut ViewState) -> bool {
        let Some((index, text)) = self.pairing_notice.take() else {
            return false;
        };
        if state.transcript.hide_notice(index, &text) {
            state.render_cache.invalidate();
        }
        true
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.hub.stop(None);
    }
}

/// Unbuffered reads from a terminal descriptor.
pub(super) struct FdInput(RawFd);

impl FdInput {
    pub(super) fn stdin() -> Self {
        Self(libc::STDIN_FILENO)
    }
}

impl Read for FdInput {
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

impl AsRawFd for FdInput {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

pub(super) struct RemoteInput<R> {
    host: R,
    hub: Arc<Hub>,
    /// Host descriptor to wait on together with the hub's wake pipe.
    poll_fd: Option<RawFd>,
}

/// Matches the raw-mode read timeout, which paces UI ticks.
const TICK_MILLIS: libc::c_int = 100;

impl<R: Read> RemoteInput<R> {
    /// Waits until host input is readable, remote input is queued, or a tick
    /// elapses. Returns whether the host has input.
    fn wait(&self, host: RawFd, wake: RawFd) -> bool {
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
        if fds[1].revents != 0 {
            self.hub.drain_wake();
        }
        // An interrupted or timed-out wait is a tick.
        ready > 0 && fds[0].revents != 0
    }
}

impl<R: Read> Read for RemoteInput<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let queued = self.hub.take_input(buf);
        if queued > 0 {
            return Ok(queued);
        }
        if let (Some(host), Some(wake)) = (self.poll_fd, self.hub.wake_fd())
            && !self.wait(host, wake)
        {
            return Ok(self.hub.take_input(buf));
        }
        // Without a wake pipe, raw mode returns from this read after at most
        // 100 ms, which bounds remote input latency.
        let read = self.host.read(buf)?;
        if read > 0 && self.hub.accept_host_input(&buf[..read]) {
            return Ok(read);
        }
        Ok(self.hub.take_input(buf))
    }
}

/// Handles `/remote` and `/remote off`.
pub(super) fn command(terminal: &mut Terminal, state: &mut ViewState, argument: &str) {
    match argument {
        "" => {
            if let Some((address, code)) = terminal.remote().hub.pairing() {
                let notice = listening_notice(address, &code, false);
                terminal.remote().show_pairing_notice(state, notice);
                return;
            }
            if let Err(error) = start(terminal.remote()) {
                state.notice(error);
                return;
            }
            if let Err(error) = terminal.sync_host_signals(true) {
                terminal.remote().hub.stop(None);
                state.notice(format!("Could not start remote control: {error}"));
                return;
            }
            let Some((address, code)) = terminal.remote().hub.pairing() else {
                return;
            };
            // With Universal Clipboard (or a clipboard manager), the link is
            // ready to paste on the other device.
            let copied = terminal
                .copy_text(&pairing_link(address, &code))
                .unwrap_or(false);
            let notice = listening_notice(address, &code, copied);
            terminal.remote().show_pairing_notice(state, notice);
        }
        "off" | "stop" => {
            if terminal.remote().hub.stop(None) {
                state.notice("Remote control stopped.");
            } else {
                state.notice("Remote control is not running.");
            }
        }
        _ => state.notice("Usage: /remote [off]"),
    }
}

fn start(remote: &Remote) -> Result<(), String> {
    let ip = match address::tailscale_ipv4() {
        Ok(Some(ip)) => ip,
        Ok(None) => {
            return Err(
                "Remote control needs Tailscale. Connect this machine to your tailnet \
                        and run /remote again."
                    .into(),
            );
        }
        Err(error) => return Err(format!("Could not list network interfaces: {error}")),
    };
    server::start(&remote.hub, IpAddr::V4(ip), server::DEFAULT_PORT)
        .map(|_| ())
        .map_err(|error| format!("Could not start remote control: {error}"))
}

/// The working directory name and the first line of the session's first
/// prompt, as the resume picker identifies sessions.
fn session_title(directory: &str, entries: &[Entry]) -> String {
    let prompt = entries.iter().find_map(|entry| match entry {
        Entry::User(text) => text.lines().map(str::trim).find(|line| !line.is_empty()),
        _ => None,
    });
    let prompt = prompt.map(|prompt| {
        let mut chars = prompt.chars();
        let mut short: String = chars.by_ref().take(TITLE_PROMPT_CHARS).collect();
        if chars.next().is_some() {
            short.push('…');
        }
        short
    });
    match (directory.is_empty(), prompt) {
        (false, Some(prompt)) => format!("{directory} · {prompt}"),
        (true, Some(prompt)) => prompt,
        (false, None) => directory.to_string(),
        (true, None) => String::new(),
    }
}

/// The page URL with the pairing code in its fragment. Browsers never send
/// the fragment to the server; the page reads it and pairs automatically.
fn pairing_link(address: SocketAddr, code: &str) -> String {
    format!("http://{address}/#{code}")
}

fn listening_notice(address: SocketAddr, code: &str, copied: bool) -> String {
    let link = pairing_link(address, code);
    let qr = qr::QrCode::encode(link.as_bytes())
        .map(|code| format!("```\n{}```\n\n", code.to_text()))
        .unwrap_or_default();
    let clipboard = if copied {
        " The link is on your clipboard."
    } else {
        ""
    };
    format!(
        "Remote control is listening at http://{address}\n\n\
         Scan the code to open it and pair automatically, or open the address on a device in \
         your tailnet and enter pairing code **{code}**.{clipboard}\n\n{qr}\
         The device takes control when it connects; press `Ctrl+C` in this terminal to take it \
         back. `/remote off` stops it."
    )
}

/// Rows shown on the host terminal while a device has control.
pub(super) fn lock_screen(snapshot: &Snapshot, columns: u16, rows: u16) -> Vec<String> {
    let width = usize::from(columns);
    let status = match (snapshot.connected, snapshot.peer) {
        (true, Some(peer)) => format!("Controlled from {peer}"),
        (true, None) => "Controlled remotely".to_string(),
        (false, _) => "Device disconnected, waiting for it to reconnect".to_string(),
    };
    let address = snapshot
        .address
        .map(|address| format!("http://{address}"))
        .unwrap_or_default();
    let lines = [
        ("\x1b[1m", "yawl · remote control".to_string()),
        ("", String::new()),
        ("", status),
        ("\x1b[2m", address),
        ("", String::new()),
        (
            "\x1b[2m",
            "Press Ctrl+C to stop remote control and take back this terminal.".to_string(),
        ),
    ];
    let top = usize::from(rows).saturating_sub(lines.len()) / 2;
    let mut frame = vec![String::new(); usize::from(rows)];
    for (offset, (style, text)) in lines.into_iter().enumerate() {
        let Some(row) = frame.get_mut(top + offset) else {
            break;
        };
        let text = text.chars().take(width).collect::<String>();
        let padding = width.saturating_sub(text.chars().count()) / 2;
        *row = format!("{}{style}{text}\x1b[0m", " ".repeat(padding));
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::markdown::strip_ansi;

    #[test]
    fn lock_screen_centers_status_and_fits_width() {
        let snapshot = Snapshot {
            address: Some("100.64.0.1:7474".parse().expect("address")),
            controlled: true,
            connected: true,
            peer: Some("100.64.0.9".parse().expect("peer")),
            size: None,
            revision: 1,
        };
        let frame = lock_screen(&snapshot, 30, 10);
        assert_eq!(frame.len(), 10);
        let text = frame.iter().map(|row| strip_ansi(row)).collect::<Vec<_>>();
        assert!(text.iter().any(|row| row.contains("100.64.0.9")));
        assert!(text.iter().all(|row| row.chars().count() <= 30));
        let disconnected = Snapshot {
            connected: false,
            ..snapshot
        };
        let text = lock_screen(&disconnected, 80, 10)
            .iter()
            .map(|row| strip_ansi(row))
            .collect::<String>();
        assert!(text.contains("waiting for it to reconnect"));
        assert!(text.contains("Ctrl+C"));
    }

    #[test]
    fn session_title_names_the_directory_and_first_prompt() {
        let entries = [
            Entry::Assistant("hello".into()),
            Entry::User("\n  fix the remote pairing flow  \nmore detail".into()),
            Entry::User("second".into()),
        ];
        assert_eq!(
            session_title("yawl", &entries),
            "yawl · fix the remote pairing flow"
        );
        assert_eq!(session_title("yawl", &[]), "yawl");
        assert_eq!(
            session_title("", &entries[1..]),
            "fix the remote pairing flow"
        );

        let long = [Entry::User("x".repeat(TITLE_PROMPT_CHARS + 5))];
        assert_eq!(
            session_title("yawl", &long),
            format!("yawl · {}…", "x".repeat(TITLE_PROMPT_CHARS))
        );
    }

    #[test]
    fn listening_notice_shares_a_scannable_pairing_link() {
        let address = "100.64.0.1:7474".parse().expect("address");
        let notice = listening_notice(address, "482913", true);
        assert!(notice.contains("pairing code **482913**"));
        assert!(notice.contains("on your clipboard"));
        assert!(notice.contains("```\n█"));
        assert_eq!(
            pairing_link(address, "482913"),
            "http://100.64.0.1:7474/#482913"
        );
        assert!(!listening_notice(address, "482913", false).contains("clipboard"));
    }

    #[test]
    fn bursts_larger_than_one_read_arrive_without_stalling() -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        use std::time::{Duration, Instant};

        let remote = Remote::new();
        let (host, mut terminal) = UnixStream::pair()?;
        let mut input = remote.terminal_input(FdInput(host.as_raw_fd()));
        terminal.write_all(&[b'p'; 1024])?;
        let started = Instant::now();
        let mut buf = [0u8; 512];
        let mut total = 0;
        while total < 1024 {
            let read = input.read(&mut buf)?;
            assert!(read > 0, "input stalled after {total} bytes");
            total += read;
        }
        assert!(started.elapsed() < Duration::from_millis(50));
        Ok(())
    }

    #[test]
    fn remote_input_interrupts_the_terminal_wait() -> io::Result<()> {
        use std::os::unix::net::UnixStream;
        use std::time::{Duration, Instant};

        let remote = Remote::new();
        let generation = remote.hub.start(
            "127.0.0.1:1".parse().expect("address"),
            "123456".into(),
            "token".into(),
        );
        let _client = remote
            .hub
            .attach(
                generation,
                "100.64.0.2".parse().expect("peer"),
                "c".into(),
                true,
                "",
            )
            .expect("attach");
        // An idle host terminal: nothing ever becomes readable on it.
        let (host, _peer) = UnixStream::pair()?;
        let mut input = remote.terminal_input(host);
        let hub = Arc::clone(&remote.hub);
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            hub.push_input(generation, "c", b"k")
        });
        let started = Instant::now();
        let mut buf = [0u8; 4];
        let mut read = input.read(&mut buf)?;
        // A tick may elapse first on a slow machine; the byte must follow it
        // without waiting for another full timeout.
        if read == 0 {
            read = input.read(&mut buf)?;
        }
        assert!(sender.join().expect("sender"));
        assert_eq!(&buf[..read], b"k");
        assert!(started.elapsed() < Duration::from_millis(150));
        Ok(())
    }

    #[test]
    fn remote_input_is_merged_and_host_input_is_locked() -> io::Result<()> {
        let remote = Remote::new();
        let generation = remote.hub.start(
            "127.0.0.1:1".parse().expect("address"),
            "123456".into(),
            "token".into(),
        );
        let mut input = remote.input(&b"host"[..]);
        let mut buf = [0u8; 16];
        assert_eq!(input.read(&mut buf)?, 4);
        assert_eq!(&buf[..4], b"host");

        remote
            .hub
            .attach(
                generation,
                "100.64.0.2".parse().expect("peer"),
                "c".into(),
                true,
                "",
            )
            .expect("attach");
        assert!(remote.hub.push_input(generation, "c", b"phone"));
        let mut input = remote.input(&b"ignored"[..]);
        assert_eq!(input.read(&mut buf)?, 5);
        assert_eq!(&buf[..5], b"phone");
        assert_eq!(input.read(&mut buf)?, 0);
        Ok(())
    }
}
