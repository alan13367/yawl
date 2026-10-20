//! Signal-free cancellation of blocking HTTP I/O.
//!
//! Every ureq agent in Yawl is built through [`cancellable_agent`]. Its
//! connector chain is ureq's own `DefaultConnector` (proxies, TLS, pooling)
//! followed by [`Interruptible`], which wraps the finished transport. Reads
//! wait in short slices and, between slices, fail with
//! `io::ErrorKind::Interrupted` once the run was canceled or the provider
//! stall watch fired. No signal is involved, so this works on Windows, and on
//! Unix it backs up the `SIGUSR1` wake-up.

use std::io;
use std::time::{Duration, Instant};

use ureq::Error;
use ureq::config::Config;
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::time::Duration as UreqDuration;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, Transport,
};

/// Longest a single read waits before cancellation is rechecked. Bounds the
/// delay between Ctrl+C and a blocked read failing.
const SLICE: Duration = Duration::from_millis(100);

/// Builds an agent from `config` whose blocking reads honor cancellation.
pub(crate) fn cancellable_agent(config: Config) -> ureq::Agent {
    ureq::Agent::with_parts(
        config,
        DefaultConnector::default().chain(Interruptible),
        DefaultResolver::default(),
    )
}

/// Wraps whatever transport the default chain produced.
#[derive(Debug)]
struct Interruptible;

impl Connector<Box<dyn Transport>> for Interruptible {
    type Out = InterruptibleTransport;

    fn connect(
        &self,
        _details: &ConnectionDetails,
        chained: Option<Box<dyn Transport>>,
    ) -> Result<Option<Self::Out>, Error> {
        Ok(chained.map(|inner| InterruptibleTransport {
            inner,
            abort: Abort::default(),
        }))
    }
}

#[derive(Debug)]
struct InterruptibleTransport {
    inner: Box<dyn Transport>,
    abort: Abort,
}

impl Transport for InterruptibleTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    /// Writes are not sliced. A write that times out after sending part of the
    /// buffer cannot report how much went out, so retrying it would corrupt
    /// the request. Cancellation is checked before the write instead.
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        if let Some(error) = self.abort.check() {
            return Err(error);
        }
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        let Self { inner, abort } = self;
        await_in_slices(timeout, abort, |slice| inner.await_input(slice))
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

/// Reports cancellation or a provider stall as an I/O error.
///
/// The first report is `ErrorKind::Interrupted`, which the stream readers and
/// the web tools already treat as "check why". Std helpers such as
/// `read_to_string` retry `Interrupted` forever, so while the condition holds
/// every later report is a plain `Other` error that ends such loops. Ureq
/// treats that kind as a failed read, unlike `ConnectionAborted`, which it
/// would take for a clean close.
#[derive(Debug, Default)]
struct Abort {
    reported: bool,
}

impl Abort {
    fn check(&mut self) -> Option<Error> {
        if !(crate::cancellation::interrupted() || super::stalled()) {
            self.reported = false;
            return None;
        }
        let kind = if self.reported {
            io::ErrorKind::Other
        } else {
            io::ErrorKind::Interrupted
        };
        self.reported = true;
        Some(Error::Io(io::Error::new(kind, "request interrupted")))
    }
}

/// Runs `await_input` in slices of at most [`SLICE`], checking for
/// cancellation between them, while preserving ureq's own deadline: once
/// `timeout.after` has elapsed the result is `Timeout(timeout.reason)`, as if
/// the wait had never been split. A timed-out slice consumes no bytes in
/// either the TCP or the TLS transport, so the next slice resumes cleanly.
fn await_in_slices(
    timeout: NextTimeout,
    abort: &mut Abort,
    mut await_input: impl FnMut(NextTimeout) -> Result<bool, Error>,
) -> Result<bool, Error> {
    // `NotHappening` derefs to a duration too long to add, which `checked_add`
    // turns into "no deadline".
    let deadline = Instant::now().checked_add(*timeout.after);
    loop {
        if let Some(error) = abort.check() {
            return Err(error);
        }
        let slice = match deadline {
            None => SLICE,
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(Error::Timeout(timeout.reason));
                }
                remaining.min(SLICE)
            }
        };
        let slice = NextTimeout {
            after: UreqDuration::Exact(slice),
            reason: timeout.reason,
        };
        match await_input(slice) {
            // The slice elapsed without data; the deadline check above decides
            // whether that was the real timeout.
            Err(Error::Timeout(_)) => {}
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    use ureq::Timeout;

    use super::super::{StallWatch, WatchGuard};
    use super::*;
    use crate::cancellation::{CancellationToken, scope};
    use crate::test_support::{read_http_request, write_chunked_head};

    fn next(after_ms: u64) -> NextTimeout {
        NextTimeout {
            after: UreqDuration::from_millis(after_ms),
            reason: Timeout::RecvBody,
        }
    }

    fn build_agent(config: Config) -> ureq::Agent {
        cancellable_agent(config)
    }

    fn config() -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(None)
    }

    /// A keep-alive HTTP/1.1 server. Every request is answered with a chunked
    /// 200 holding `chunk` after `delay`. A `complete` response ends there;
    /// otherwise the body stays open until the server is stopped.
    struct Server {
        url: String,
        accepted: Arc<AtomicUsize>,
        released: Arc<AtomicBool>,
        handle: std::thread::JoinHandle<()>,
    }

    impl Server {
        fn start(delay: Duration, chunk: &'static str, complete: bool) -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
            listener.set_nonblocking(true).expect("nonblocking");
            let url = format!("http://{}", listener.local_addr().expect("address"));
            let accepted = Arc::new(AtomicUsize::new(0));
            let released = Arc::new(AtomicBool::new(false));
            let handle = std::thread::spawn({
                let (accepted, released) = (Arc::clone(&accepted), Arc::clone(&released));
                move || {
                    let mut connections = Vec::new();
                    while !released.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((socket, _)) => {
                                accepted.fetch_add(1, Ordering::SeqCst);
                                let released = Arc::clone(&released);
                                connections.push(std::thread::spawn(move || {
                                    serve(socket, delay, chunk, complete, &released);
                                }));
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(error) => panic!("accept: {error}"),
                        }
                    }
                    for connection in connections {
                        let _ = connection.join();
                    }
                }
            });
            Server {
                url,
                accepted,
                released,
                handle,
            }
        }

        /// Stops the server and returns how many connections it accepted.
        fn stop(self) -> usize {
            self.released.store(true, Ordering::SeqCst);
            self.handle.join().expect("server thread");
            self.accepted.load(Ordering::SeqCst)
        }
    }

    fn serve(
        mut socket: TcpStream,
        delay: Duration,
        chunk: &str,
        complete: bool,
        released: &AtomicBool,
    ) {
        socket.set_nonblocking(false).expect("blocking socket");
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("read timeout");
        loop {
            // The next request, or the client closing the connection. Requests
            // are small GETs that arrive whole, so a read timeout only ever
            // means "no request yet".
            match read_http_request(&mut socket) {
                Ok(request) if !request.is_empty() => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) && !released.load(Ordering::SeqCst) =>
                {
                    continue;
                }
                _ => return,
            }
            std::thread::sleep(delay);
            if write_chunked_head(&mut socket, chunk).is_err() {
                return;
            }
            if !complete {
                while !released.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                return;
            }
            if socket
                .write_all(b"0\r\n\r\n")
                .and_then(|()| socket.flush())
                .is_err()
            {
                return;
            }
        }
    }

    /// Reads the body to its end with plain `read` calls, which (unlike
    /// `read_to_string`) hand every `Interrupted` error to the caller.
    fn drain(url: &str, agent: &ureq::Agent) -> io::Result<String> {
        let mut response = agent.get(url).call().map_err(ureq::Error::into_io)?;
        let mut reader = response.body_mut().with_config().reader();
        let mut body = Vec::new();
        let mut chunk = [0_u8; 256];
        loop {
            match reader.read(&mut chunk)? {
                0 => return Ok(String::from_utf8_lossy(&body).into_owned()),
                read => body.extend_from_slice(&chunk[..read]),
            }
        }
    }

    #[test]
    fn slices_forward_data_and_other_errors_unchanged() {
        let mut calls = 0;
        let result = await_in_slices(next(1_000), &mut Abort::default(), |slice| {
            calls += 1;
            assert!(*slice.after <= SLICE);
            assert_eq!(slice.reason, Timeout::RecvBody);
            if calls < 3 {
                Err(Error::Timeout(slice.reason))
            } else {
                Ok(true)
            }
        });
        assert!(matches!(result, Ok(true)));
        assert_eq!(calls, 3);

        let failed = await_in_slices(next(1_000), &mut Abort::default(), |_| {
            Err(Error::ConnectionFailed)
        });
        assert!(matches!(failed, Err(Error::ConnectionFailed)));
    }

    #[test]
    fn the_original_deadline_wins_with_its_reason() {
        let started = Instant::now();
        let mut slices = 0;
        let result = await_in_slices(next(250), &mut Abort::default(), |slice| {
            slices += 1;
            std::thread::sleep(*slice.after);
            Err(Error::Timeout(slice.reason))
        });
        assert!(matches!(result, Err(Error::Timeout(Timeout::RecvBody))));
        assert!(slices >= 3, "a 250 ms wait should span several slices");
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_wait_without_a_deadline_still_slices() {
        let token = CancellationToken::default();
        let mut slices = 0;
        let result = scope(&token, || {
            await_in_slices(
                NextTimeout {
                    after: UreqDuration::NotHappening,
                    reason: Timeout::Global,
                },
                &mut Abort::default(),
                |slice| {
                    slices += 1;
                    assert_eq!(*slice.after, SLICE);
                    if slices == 2 {
                        token.cancel();
                    }
                    Err(Error::Timeout(slice.reason))
                },
            )
        });
        assert!(
            matches!(&result, Err(Error::Io(error)) if error.kind() == io::ErrorKind::Interrupted)
        );
        assert_eq!(slices, 2);
    }

    #[test]
    fn an_untouched_request_completes() {
        let server = Server::start(Duration::from_millis(250), "hello", true);
        let agent = build_agent(config().build());
        let body = drain(&server.url, &agent).expect("request");
        assert_eq!(body, "hello");
        server.stop();
    }

    #[test]
    fn canceling_the_scoped_token_interrupts_a_stalled_body_without_a_wake() {
        let server = Server::start(Duration::ZERO, "partial", false);
        let agent = build_agent(config().build());
        let token = CancellationToken::default();
        let (started_tx, started_rx) = mpsc::channel();
        let canceler = {
            let token = token.clone();
            std::thread::spawn(move || {
                started_rx.recv().expect("reader started");
                std::thread::sleep(Duration::from_millis(250));
                token.cancel();
                Instant::now()
            })
        };
        let error = scope(&token, || {
            let mut response = agent.get(&server.url).call().expect("headers");
            let mut reader = response.body_mut().with_config().reader();
            let mut chunk = [0_u8; 64];
            // The first chunk arrives, then the server goes quiet.
            let read = reader.read(&mut chunk).expect("first chunk");
            assert_eq!(&chunk[..read], b"partial");
            started_tx.send(()).expect("signal");
            reader.read(&mut chunk).expect_err("stalled read must fail")
        });
        let failed_at = Instant::now();
        let canceled_at = canceler.join().expect("canceler");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        assert!(
            failed_at.saturating_duration_since(canceled_at) < Duration::from_secs(1),
            "cancellation took too long to surface"
        );
        server.stop();
    }

    #[test]
    fn std_read_helpers_that_retry_interrupted_still_terminate() {
        let server = Server::start(Duration::ZERO, "partial", false);
        let agent = build_agent(config().build());
        let token = CancellationToken::default();
        let canceler = {
            let token = token.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(250));
                token.cancel();
            })
        };
        let started = Instant::now();
        let error = scope(&token, || {
            let mut response = agent.get(&server.url).call().expect("headers");
            response
                .body_mut()
                .with_config()
                .read_to_string()
                .expect_err("a canceled read must fail rather than spin")
        });
        canceler.join().expect("canceler");
        assert!(started.elapsed() < Duration::from_secs(2), "{error:?}");
        server.stop();
    }

    #[test]
    fn the_stall_flag_interrupts_a_stalled_body() {
        let server = Server::start(Duration::ZERO, "partial", false);
        let agent = build_agent(config().build());
        let watch = Arc::new(StallWatch::default());
        let flag = Arc::clone(&watch);
        let staller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.stalled.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let guard = {
            let previous = super::super::CURRENT_WATCH
                .with_borrow_mut(|current| current.replace(Arc::clone(&watch)));
            WatchGuard(previous)
        };
        let error = drain(&server.url, &agent).expect_err("stalled read must fail");
        drop(guard);
        staller.join().expect("staller");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
        server.stop();
    }

    #[test]
    fn a_configured_ureq_timeout_still_fires_with_its_reason() {
        let server = Server::start(Duration::from_millis(1500), "late", true);
        let agent = build_agent(
            config()
                .timeout_recv_response(Some(Duration::from_millis(400)))
                .build(),
        );
        let started = Instant::now();
        let error = agent.get(&server.url).call().expect_err("must time out");
        assert!(
            matches!(error, ureq::Error::Timeout(Timeout::RecvResponse)),
            "{error:?}"
        );
        assert!(started.elapsed() >= Duration::from_millis(400));
        assert!(started.elapsed() < Duration::from_secs(2));

        let global = build_agent(
            config()
                .timeout_global(Some(Duration::from_millis(300)))
                .build(),
        );
        let error = global.get(&server.url).call().expect_err("must time out");
        assert!(
            matches!(error, ureq::Error::Timeout(Timeout::Global)),
            "{error:?}"
        );
        // Dropping the server's release lets its held sockets finish.
        server.stop();
    }

    #[test]
    fn pooled_connections_survive_requests_that_waited_through_slices() {
        let server = Server::start(Duration::from_millis(450), "ok", true);
        let agent = build_agent(config().build());
        for _ in 0..2 {
            let started = Instant::now();
            assert_eq!(drain(&server.url, &agent).expect("request"), "ok");
            assert!(started.elapsed() >= Duration::from_millis(450));
        }
        assert_eq!(
            server.stop(),
            1,
            "the second request should reuse the first connection"
        );
    }
}
