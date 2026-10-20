//! A TCP connector that never lets a receive time out.
//!
//! Winsock documents that a socket whose `SO_RCVTIMEO` receive timed out is in
//! an indeterminate state, with a possible loss of data. The polling
//! connector in `interruptible` times out a receive every 100 ms of silence,
//! which on a streaming response happens constantly, and a plain `http://`
//! provider has no TLS record check to catch a lost byte. So on Windows the
//! chain uses [`ReadyTcpConnector`], whose transport first waits for the
//! socket to become readable (`WSAPoll`), without any receive timeout, and
//! only then reads, which then returns at once.
//!
//! The rest of the chain mirrors ureq's `DefaultConnector` minus its
//! `WarnOnNoSocksConnector` (ureq's `socks-proxy` feature is off, so it only
//! warns about, or panics on, a SOCKS proxy that would be ignored anyway) and
//! `WarnOnMissingTlsProvider` (it only panics when the config asks for a TLS
//! provider that is not compiled in; Yawl never does).
//!
//! The module is also built for tests on Unix, where `poll(2)` stands in for
//! `WSAPoll`, so the chain is exercised on every development platform.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use ureq::unversioned::transport::time::Duration as UreqDuration;
use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout,
    RustlsConnector, Transport,
};
use ureq::{Error, Timeout};

use super::interruptible::Interruptible;

/// Smallest connect budget handed to one address, as in ureq's `TcpConnector`.
const MIN_PER_ADDRESS_TIMEOUT: Duration = Duration::from_millis(10);

/// The connector chain for Windows agents: CONNECT-proxy support, ready-wait
/// TCP, rustls, then cancellation polling.
pub(super) fn connector() -> impl Connector {
    ().chain(ConnectProxyConnector::default())
        .chain(ReadyTcpConnector)
        .chain(RustlsConnector::default())
        .chain(Interruptible)
}

/// Opens plain TCP sockets, or passes through a transport an earlier
/// connector (a CONNECT proxy) already produced, as ureq's `TcpConnector`
/// does.
#[derive(Debug)]
struct ReadyTcpConnector;

impl<In: Transport> Connector<In> for ReadyTcpConnector {
    type Out = Either<In, ReadyTcpTransport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, Error> {
        if let Some(transport) = chained {
            return Ok(Some(Either::A(transport)));
        }
        let stream = connect_stream(&details.addrs, details.timeout)?;
        if details.config.no_delay() {
            stream.set_nodelay(true)?;
        }
        let buffers = LazyBuffers::new(
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );
        Ok(Some(Either::B(ReadyTcpTransport {
            stream,
            buffers,
            write_timeout: None,
        })))
    }
}

/// Connects to the first address that answers, sharing what is left of the
/// connect budget between the addresses still to try: an unreachable first
/// address cannot spend the whole budget. A zero budget means one second, as
/// in ureq, and a budget that is not happening means no limit. When every
/// address fails, the last failure is returned.
fn connect_stream(addrs: &[SocketAddr], timeout: NextTimeout) -> Result<TcpStream, Error> {
    let deadline = timeout
        .not_zero()
        .and_then(|after| Instant::now().checked_add(*after));
    let mut last_error = None;
    for (index, addr) in addrs.iter().enumerate() {
        let attempt = match deadline {
            None => TcpStream::connect(addr),
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(Error::Timeout(timeout.reason));
                }
                let left = u32::try_from(addrs.len() - index).unwrap_or(1);
                let share = (remaining / left).max(MIN_PER_ADDRESS_TIMEOUT);
                TcpStream::connect_timeout(addr, share.min(remaining))
            }
        };
        last_error = Some(match attempt {
            Ok(stream) => return Ok(stream),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => Error::Timeout(timeout.reason),
            Err(error) => Error::Io(error),
        });
    }
    Err(last_error.unwrap_or_else(|| {
        Error::Io(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "no addresses to connect to",
        ))
    }))
}

/// A TCP socket whose reads wait with `WSAPoll` instead of a receive timeout.
/// (ureq's own `TcpTransport` is not exported, so this is a small equivalent.)
#[derive(Debug)]
struct ReadyTcpTransport {
    stream: TcpStream,
    buffers: LazyBuffers,
    write_timeout: Option<UreqDuration>,
}

impl Transport for ReadyTcpTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        let wanted = timeout.not_zero();
        if wanted != self.write_timeout {
            self.stream.set_write_timeout(wanted.map(|after| *after))?;
            self.write_timeout = wanted;
        }
        let output = &self.buffers.output()[..amount];
        self.stream
            .write_all(output)
            .map_err(|error| io_error(error, timeout.reason))
    }

    /// Waits for readability for `timeout`, reporting `Timeout(reason)` if the
    /// socket stays quiet. Only a readable (or hung up, or failed) socket is
    /// read, and the read then completes at once, so no receive ever times
    /// out. Callers wanting cancellation hand this short timeouts.
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        if !wait_readable(&self.stream, timeout.not_zero().map(|after| *after))? {
            return Err(Error::Timeout(timeout.reason));
        }
        let input = self.buffers.input_append_buf();
        let amount = self
            .stream
            .read(input)
            .map_err(|error| io_error(error, timeout.reason))?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    /// An idle pooled connection is open when a nonblocking read finds
    /// nothing; any byte or error means the peer closed or misbehaved.
    fn is_open(&mut self) -> bool {
        if self.stream.set_nonblocking(true).is_err() {
            return false;
        }
        let mut byte = [0];
        match self.stream.read(&mut byte) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            _ => return false,
        }
        self.stream.set_nonblocking(false).is_ok()
    }
}

/// Maps the timeout kinds of a socket I/O error (`TimedOut` on Windows,
/// `WouldBlock` on Unix) to ureq's timeout error.
fn io_error(error: io::Error, reason: Timeout) -> Error {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Error::Timeout(reason),
        _ => Error::Io(error),
    }
}

/// Whole milliseconds for `poll`, rounded up so a short wait is not zero, or
/// -1 (wait indefinitely) for no timeout.
fn poll_millis(timeout: Option<Duration>) -> i32 {
    timeout.map_or(-1, |timeout| {
        let millis = timeout.as_micros().div_ceil(1000).max(1);
        i32::try_from(millis).unwrap_or(i32::MAX)
    })
}

/// Waits until `stream` is readable, has hung up, or has an error, returning
/// false if `timeout` passes first. `None` waits indefinitely.
#[cfg(windows)]
fn wait_readable(stream: &TcpStream, timeout: Option<Duration>) -> io::Result<bool> {
    use std::os::windows::io::AsRawSocket;

    use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, WSAPOLLFD, WSAPoll};

    let mut poll = WSAPOLLFD {
        fd: stream.as_raw_socket() as usize,
        events: POLLRDNORM,
        revents: 0,
    };
    // SAFETY: `poll` is one valid, initialized WSAPOLLFD and the count is 1.
    // Its socket belongs to `stream`, which the caller borrows for the whole
    // call, so the handle stays open. Winsock was started by std when the
    // socket was created. WSAPoll writes only `revents`. A negative return
    // means failure, with the reason in the thread's last error, which
    // `last_os_error` reads (Winsock reports through `GetLastError`). POLLHUP,
    // POLLERR and POLLNVAL come back in `revents` without being requested;
    // any nonzero result means a read will not block.
    let ready = unsafe { WSAPoll(&mut poll, 1, poll_millis(timeout)) };
    match ready {
        0 => Ok(false),
        1.. => Ok(true),
        _ => Err(io::Error::last_os_error()),
    }
}

/// `poll(2)` stand-in for tests on Unix. An interrupted poll reports "not
/// ready" so the caller's timeout loop runs again.
#[cfg(unix)]
fn wait_readable(stream: &TcpStream, timeout: Option<Duration>) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    let mut poll = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `poll` is one valid, initialized pollfd and the count is 1. Its
    // descriptor belongs to `stream`, borrowed for the whole call. poll writes
    // only `revents`.
    let ready = unsafe { libc::poll(&mut poll, 1, poll_millis(timeout)) };
    match ready {
        0 => Ok(false),
        1.. => Ok(true),
        _ => {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use ureq::config::Config;
    use ureq::unversioned::resolver::DefaultResolver;

    use super::super::scenarios;
    use super::*;

    fn ready_agent(config: Config) -> ureq::Agent {
        ureq::Agent::with_parts(config, connector(), DefaultResolver::default())
    }

    fn after(millis: u64) -> NextTimeout {
        NextTimeout {
            after: UreqDuration::from_millis(millis),
            reason: Timeout::Connect,
        }
    }

    #[test]
    fn readiness_reports_data_hangup_and_silence() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
        let mut client = TcpStream::connect(listener.local_addr().expect("address")).expect("dial");
        let (mut server, _) = listener.accept().expect("accept");

        let started = Instant::now();
        assert!(!wait_readable(&client, Some(Duration::from_millis(120))).expect("poll"));
        assert!(started.elapsed() >= Duration::from_millis(100));

        server.write_all(b"x").expect("write");
        assert!(wait_readable(&client, Some(Duration::from_secs(5))).expect("poll"));
        let mut byte = [0];
        client.read_exact(&mut byte).expect("read");

        drop(server);
        assert!(wait_readable(&client, Some(Duration::from_secs(5))).expect("poll"));
        assert_eq!(client.read(&mut byte).expect("eof"), 0);
    }

    #[test]
    fn poll_timeouts_round_up_and_unlimited_is_negative() {
        assert_eq!(poll_millis(None), -1);
        assert_eq!(poll_millis(Some(Duration::from_micros(1))), 1);
        assert_eq!(poll_millis(Some(Duration::from_millis(100))), 100);
        assert_eq!(poll_millis(Some(Duration::from_secs(u64::MAX))), i32::MAX);
    }

    #[test]
    fn connecting_skips_dead_addresses_and_reports_the_last_failure() {
        let dead = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
            listener.local_addr().expect("address")
        };
        let live = TcpListener::bind("127.0.0.1:0").expect("listen");
        let live_addr = live.local_addr().expect("address");

        connect_stream(&[dead, live_addr], after(2_000)).expect("second address answers");
        assert!(matches!(
            connect_stream(&[dead], after(2_000)),
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::ConnectionRefused
        ));
        assert!(matches!(
            connect_stream(&[], after(2_000)),
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::ConnectionRefused
        ));
        connect_stream(
            &[live_addr],
            NextTimeout {
                after: UreqDuration::NotHappening,
                reason: Timeout::Connect,
            },
        )
        .expect("no limit");
    }

    #[test]
    fn an_untouched_request_completes() {
        scenarios::untouched_request_completes(ready_agent);
    }

    #[test]
    fn canceling_the_scoped_token_interrupts_a_stalled_body() {
        scenarios::token_cancellation_interrupts_a_stalled_body(ready_agent);
    }

    #[test]
    fn std_read_helpers_that_retry_interrupted_still_terminate() {
        scenarios::std_read_helpers_that_retry_interrupted_terminate(ready_agent);
    }

    #[test]
    fn the_stall_flag_interrupts_a_stalled_body() {
        scenarios::stall_flag_interrupts_a_stalled_body(ready_agent);
    }

    #[test]
    fn configured_timeouts_keep_their_reasons() {
        scenarios::configured_timeouts_keep_their_reasons(ready_agent);
    }

    #[test]
    fn pooled_connections_survive_requests_that_waited_through_slices() {
        scenarios::pooled_connections_survive_slow_requests(ready_agent);
    }
}
