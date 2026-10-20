//! Cancellation behavior shared by every connector chain. Each scenario takes
//! the function that builds the agent under test, so the same expectations
//! run against the default chain and the Windows chain.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use ureq::Timeout;
use ureq::config::{Config, ConfigBuilder};
use ureq::typestate::AgentScope;

use super::{CURRENT_WATCH, StallWatch, WatchGuard};
use crate::cancellation::{CancellationToken, scope};
use crate::test_support::{read_http_request, write_chunked_head};

pub(super) type BuildAgent = fn(Config) -> ureq::Agent;

fn config() -> ConfigBuilder<AgentScope> {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(None)
}

/// A keep-alive HTTP/1.1 server. Every request is answered with a chunked
/// 200 holding `chunk` after `delay`. A `complete` response ends there;
/// otherwise the body stays open until the server is stopped.
pub(super) struct Server {
    pub(super) url: String,
    accepted: Arc<AtomicUsize>,
    released: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl Server {
    pub(super) fn start(delay: Duration, chunk: &'static str, complete: bool) -> Server {
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
    pub(super) fn stop(self) -> usize {
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

pub(super) fn untouched_request_completes(build: BuildAgent) {
    let server = Server::start(Duration::from_millis(250), "hello", true);
    let agent = build(config().build());
    let body = drain(&server.url, &agent).expect("request");
    assert_eq!(body, "hello");
    server.stop();
}

pub(super) fn token_cancellation_interrupts_a_stalled_body(build: BuildAgent) {
    let server = Server::start(Duration::ZERO, "partial", false);
    let agent = build(config().build());
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

pub(super) fn std_read_helpers_that_retry_interrupted_terminate(build: BuildAgent) {
    let server = Server::start(Duration::ZERO, "partial", false);
    let agent = build(config().build());
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

pub(super) fn stall_flag_interrupts_a_stalled_body(build: BuildAgent) {
    let server = Server::start(Duration::ZERO, "partial", false);
    let agent = build(config().build());
    let watch = Arc::new(StallWatch::default());
    let flag = Arc::clone(&watch);
    let staller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        flag.stalled.store(true, Ordering::Release);
    });
    let started = Instant::now();
    let guard = {
        let previous = CURRENT_WATCH.with_borrow_mut(|current| current.replace(Arc::clone(&watch)));
        WatchGuard(previous)
    };
    let error = drain(&server.url, &agent).expect_err("stalled read must fail");
    drop(guard);
    staller.join().expect("staller");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
    server.stop();
}

pub(super) fn configured_timeouts_keep_their_reasons(build: BuildAgent) {
    let server = Server::start(Duration::from_millis(1500), "late", true);
    let agent = build(
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

    let global = build(
        config()
            .timeout_global(Some(Duration::from_millis(300)))
            .build(),
    );
    let error = global.get(&server.url).call().expect_err("must time out");
    assert!(
        matches!(error, ureq::Error::Timeout(Timeout::Global)),
        "{error:?}"
    );
    server.stop();
}

pub(super) fn pooled_connections_survive_slow_requests(build: BuildAgent) {
    let server = Server::start(Duration::from_millis(450), "ok", true);
    let agent = build(config().build());
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
