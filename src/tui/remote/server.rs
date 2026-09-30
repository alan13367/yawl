//! Blocking HTTP server for the remote-control page: pairing, the frame event
//! stream, and input from the controlling device.

use std::io::{self, BufReader, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::{Duration, Instant};

use super::address::{pairing_code, session_token};
use super::http::{
    Request, read_request, respond, respond_status, start_event_stream, write_event,
};
use super::hub::{AttachError, Hub, Outgoing, PairResult};

pub(super) const DEFAULT_PORT: u16 = 7474;
const PORT_ATTEMPTS: u16 = 10;
const COOKIE: &str = "yawl_remote";
/// Carries the per-connection token sent in the stream's `hello` event.
const CLIENT_HEADER: &str = "x-yawl-client";
const MAX_CONNECTIONS: usize = 32;
/// How often waiting threads recheck whether their session is still current.
const POLL_MILLIS: libc::c_int = 100;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an idle kept-alive connection waits for its next request.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const KEEPALIVE: Duration = Duration::from_secs(15);
const PAGE: &str = include_str!("index.html");
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; \
    script-src 'unsafe-inline' https://cdn.jsdelivr.net; \
    style-src 'unsafe-inline' https://cdn.jsdelivr.net; \
    connect-src 'self'; img-src data:; base-uri 'none'; form-action 'none'; \
    frame-ancestors 'none'";

/// Binds `ip`, opens a hub session, and serves it on a background thread
/// until the session ends.
pub(super) fn start(hub: &Arc<Hub>, ip: IpAddr, port: u16) -> io::Result<SocketAddr> {
    let listener = bind(ip, port)?;
    let address = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let generation = hub.start(address, pairing_code()?, session_token()?);
    let server = Server {
        hub: Arc::clone(hub),
        generation,
        connections: Arc::new(AtomicUsize::new(0)),
    };
    let spawned = thread::Builder::new()
        .name("yawl-remote".into())
        .spawn(move || server.accept_loop(listener));
    if let Err(error) = spawned {
        hub.stop(None);
        return Err(error);
    }
    Ok(address)
}

fn bind(ip: IpAddr, port: u16) -> io::Result<TcpListener> {
    if port == 0 {
        return TcpListener::bind((ip, 0));
    }
    for candidate in port..port.saturating_add(PORT_ATTEMPTS) {
        match TcpListener::bind((ip, candidate)) {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
            Err(error) => return Err(error),
        }
    }
    TcpListener::bind((ip, 0))
}

#[derive(Clone)]
struct Server {
    hub: Arc<Hub>,
    generation: u64,
    connections: Arc<AtomicUsize>,
}

impl Server {
    fn accept_loop(self, listener: TcpListener) {
        while self.hub.is_current(self.generation) {
            // Wait for a connection without sleeping through it, so new
            // connections are accepted at once.
            if !wait_readable(listener.as_raw_fd()) {
                continue;
            }
            if let Ok((stream, peer)) = listener.accept() {
                if self.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                    self.connections.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let server = self.clone();
                let spawned = thread::Builder::new()
                    .name("yawl-remote-client".into())
                    .spawn(move || {
                        let _ = server.handle(stream, peer.ip());
                        server.connections.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    self.connections.fetch_sub(1, Ordering::SeqCst);
                }
            }
        }
    }

    /// Serves requests on one connection until the client closes it, it
    /// idles out, or it becomes an event stream.
    fn handle(&self, mut stream: TcpStream, peer: IpAddr) -> io::Result<()> {
        // Accepted sockets inherit the listener's non-blocking flag on BSDs.
        stream.set_nonblocking(false)?;
        // Frames and keystroke acknowledgements are small; send them now.
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        loop {
            // Idle kept-alive connections close as soon as their session
            // ends, so a browser never reuses one after `/remote` restarts.
            if reader.buffer().is_empty() && !self.wait_for_request(&stream) {
                return Ok(());
            }
            let request = match read_request(&mut reader) {
                Ok(Some(request)) => request,
                Ok(None) => return Ok(()),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(());
                }
                Err(_) => return respond_status(&mut stream, "400 Bad Request"),
            };
            // A request that raced the end of the session must not be
            // answered with its stale state. Closing without a response
            // lets the browser retry on a fresh connection.
            if !self.hub.is_current(self.generation) {
                return Ok(());
            }
            let close = request
                .header("connection")
                .is_some_and(|value| value.eq_ignore_ascii_case("close"));
            if !self.route(&mut stream, &request, peer)? || close {
                return Ok(());
            }
        }
    }

    /// Waits for the next request on an idle connection. Returns false when
    /// the session ends or the connection idles out.
    fn wait_for_request(&self, stream: &TcpStream) -> bool {
        let started = Instant::now();
        while self.hub.is_current(self.generation) {
            if wait_readable(stream.as_raw_fd()) {
                return true;
            }
            if started.elapsed() >= IDLE_TIMEOUT {
                return false;
            }
        }
        false
    }

    /// Answers one request. Returns false when the connection must not be
    /// reused, such as after an event stream ends.
    fn route(&self, stream: &mut TcpStream, request: &Request, peer: IpAddr) -> io::Result<bool> {
        if !request.header("host").is_some_and(host_allowed) {
            respond_status(stream, "421 Misdirected Request")?;
            return Ok(false);
        }
        let authorized = request
            .cookie(COOKIE)
            .is_some_and(|token| self.hub.authorized(self.generation, token));
        let writes = request.method == "POST";
        if writes && !origin_allowed(request) {
            respond_status(stream, "403 Forbidden")?;
            return Ok(false);
        }
        let client = request.header(CLIENT_HEADER).unwrap_or_default();
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/events") if authorized => {
                // Opening the page or pressing "Take control" takes it; a
                // reconnect only resumes the device's own stream.
                let take = request.query_param("take").is_some();
                let resume = request.query_param("resume").unwrap_or_default();
                self.stream_events(stream, peer, take, resume)?;
                return Ok(false);
            }
            _ => {}
        }
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/") => respond(
                stream,
                "200 OK",
                "text/html; charset=utf-8",
                &[("Content-Security-Policy", CONTENT_SECURITY_POLICY)],
                PAGE.as_bytes(),
            ),
            ("GET", "/session") if authorized => {
                if self.hub.busy(self.generation, client) {
                    respond_status(stream, "409 Conflict")
                } else {
                    respond_status(stream, "204 No Content")
                }
            }
            ("POST", "/pair") => self.pair(stream, request),
            ("POST", "/input") if authorized => {
                if self.hub.push_input(self.generation, client, &request.body) {
                    respond_status(stream, "204 No Content")
                } else {
                    respond_status(stream, "409 Conflict")
                }
            }
            ("POST", "/resize") if authorized => match parse_size(&request.body) {
                Some((columns, rows)) => {
                    if self.hub.resize(self.generation, client, columns, rows) {
                        respond_status(stream, "204 No Content")
                    } else {
                        respond_status(stream, "409 Conflict")
                    }
                }
                None => respond_status(stream, "400 Bad Request"),
            },
            (_, "/session" | "/events" | "/input" | "/resize") => {
                respond_status(stream, "401 Unauthorized")
            }
            _ => respond_status(stream, "404 Not Found"),
        }?;
        Ok(true)
    }

    fn pair(&self, stream: &mut TcpStream, request: &Request) -> io::Result<()> {
        let code = String::from_utf8_lossy(&request.body);
        match self.hub.pair(self.generation, &code) {
            PairResult::Paired(token) => {
                let cookie = format!("{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/");
                respond(
                    stream,
                    "204 No Content",
                    "text/plain; charset=utf-8",
                    &[("Set-Cookie", &cookie)],
                    b"",
                )
            }
            PairResult::Rejected { remaining } => respond(
                stream,
                "403 Forbidden",
                "text/plain; charset=utf-8",
                &[],
                remaining.to_string().as_bytes(),
            ),
            PairResult::Locked => respond_status(stream, "410 Gone"),
        }
    }

    fn stream_events(
        &self,
        stream: &mut TcpStream,
        peer: IpAddr,
        take: bool,
        resume: &str,
    ) -> io::Result<()> {
        let client = session_token()?;
        let (id, receiver) =
            match self
                .hub
                .attach(self.generation, peer, client.clone(), take, resume)
            {
                Ok(attached) => attached,
                Err(AttachError::Gone) => return respond_status(stream, "410 Gone"),
                Err(AttachError::Busy) => return respond_status(stream, "409 Conflict"),
            };
        let result = (|| {
            start_event_stream(&mut *stream)?;
            write_event(&mut *stream, "hello", &client)?;
            loop {
                match receiver.recv_timeout(KEEPALIVE) {
                    Ok(Outgoing::Frame(frame)) => write_event(&mut *stream, "frame", &frame)?,
                    Ok(Outgoing::Clipboard(text)) => write_event(&mut *stream, "clipboard", &text)?,
                    Ok(Outgoing::Bell) => write_event(&mut *stream, "bell", "")?,
                    Ok(Outgoing::Replaced) => return write_event(&mut *stream, "replaced", ""),
                    Err(RecvTimeoutError::Timeout) => {
                        stream.write_all(b": ping\n\n")?;
                        stream.flush()?;
                    }
                    // A dropped sender with the session still current means
                    // this client fell behind; ending the stream lets the page
                    // reconnect and receive a full frame.
                    Err(RecvTimeoutError::Disconnected) => {
                        if self.hub.is_current(self.generation) {
                            return Ok(());
                        }
                        return write_event(&mut *stream, "closed", "");
                    }
                }
            }
        })();
        self.hub.detach(self.generation, id);
        result
    }
}

/// Waits up to [`POLL_MILLIS`] for a pending connection or request. A closed
/// peer also counts as readable, so the next read sees the end of stream.
fn wait_readable(fd: RawFd) -> bool {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `descriptor` is one initialized pollfd for a descriptor the
    // caller keeps open for the duration of the call.
    unsafe { libc::poll(&mut descriptor, 1, POLL_MILLIS) > 0 }
}

/// Accepts IP literals, single-label MagicDNS names, and `*.ts.net` names.
/// Rejecting other names blocks DNS-rebinding pages on public domains.
fn host_allowed(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => name,
        _ => host,
    };
    let name = name.trim_start_matches('[').trim_end_matches(']');
    if name.parse::<IpAddr>().is_ok() {
        return true;
    }
    let name = name.to_ascii_lowercase();
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
        && (!name.contains('.') || name.ends_with(".ts.net"))
}

fn origin_allowed(request: &Request) -> bool {
    match (request.header("origin"), request.header("host")) {
        (Some(origin), Some(host)) => origin
            .strip_prefix("http://")
            .is_some_and(|origin| origin.eq_ignore_ascii_case(host)),
        _ => false,
    }
}

fn parse_size(body: &[u8]) -> Option<(u16, u16)> {
    let text = std::str::from_utf8(body).ok()?;
    let mut parts = text.split_whitespace();
    let size = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    parts.next().is_none().then_some(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::Ipv4Addr;

    /// Sends one request on its own connection and reads the response.
    fn request(address: SocketAddr, raw: &str) -> io::Result<String> {
        let mut stream = TcpStream::connect(address)?;
        let raw = raw.replacen("\r\n", "\r\nConnection: close\r\n", 1);
        stream.write_all(raw.as_bytes())?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response)
    }

    fn post(
        address: SocketAddr,
        path: &str,
        cookie: &str,
        client: &str,
        body: &str,
    ) -> io::Result<String> {
        request(
            address,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\n\
                 Cookie: {cookie}\r\nX-Yawl-Client: {client}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    fn read_until(stream: &mut TcpStream, needle: &str) -> io::Result<String> {
        let mut received = String::new();
        let mut buf = [0u8; 512];
        while !received.contains(needle) {
            let read = stream.read(&mut buf)?;
            assert!(read > 0, "stream ended before {needle:?}");
            received.push_str(&String::from_utf8_lossy(&buf[..read]));
        }
        Ok(received)
    }

    /// Opens an event stream (`query` included) and returns it with its
    /// client token.
    fn open_events(
        address: SocketAddr,
        cookie: &str,
        query: &str,
    ) -> io::Result<(TcpStream, String)> {
        let mut events = TcpStream::connect(address)?;
        events.set_read_timeout(Some(Duration::from_secs(5)))?;
        write!(
            events,
            "GET /events{query} HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\n\r\n"
        )?;
        let head = read_until(&mut events, "event: hello\ndata: ")?;
        let head = if head.ends_with("\n\n") {
            head
        } else {
            head + &read_until(&mut events, "\n\n")?
        };
        assert!(head.contains("text/event-stream"));
        let data = head
            .split("event: hello\ndata: ")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .expect("hello data");
        let token = serde_json::from_str::<String>(data).map_err(io::Error::other)?;
        Ok((events, token))
    }

    fn cookie_from(response: &str) -> Option<String> {
        let line = response
            .lines()
            .find_map(|line| line.strip_prefix("Set-Cookie: "))?;
        Some(line.split(';').next()?.to_string())
    }

    #[test]
    fn host_filter_blocks_public_domains() {
        assert!(host_allowed("100.64.0.1:7474"));
        assert!(host_allowed("[fd7a::1]:7474"));
        assert!(host_allowed("macbook:7474"));
        assert!(host_allowed("MacBook.tail1234.ts.NET:7474"));
        assert!(!host_allowed("evil.example.com:7474"));
        assert!(!host_allowed("ts.net.evil.com"));
        assert!(!host_allowed(""));
    }

    #[test]
    fn serves_several_requests_on_one_connection() -> io::Result<()> {
        let hub = Arc::new(Hub::default());
        let address = start(&hub, IpAddr::V4(Ipv4Addr::LOCALHOST), 0)?;
        let mut stream = TcpStream::connect(address)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        for _ in 0..3 {
            write!(stream, "GET /session HTTP/1.1\r\nHost: {address}\r\n\r\n")?;
            let response = read_until(&mut stream, "\r\n\r\n401 Unauthorized")?;
            assert!(response.contains("Connection: keep-alive"));
        }
        hub.stop(None);
        Ok(())
    }

    #[test]
    fn idle_connections_close_when_the_session_ends() -> io::Result<()> {
        let hub = Arc::new(Hub::default());
        let address = start(&hub, IpAddr::V4(Ipv4Addr::LOCALHOST), 0)?;
        let mut stream = TcpStream::connect(address)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        write!(stream, "GET /session HTTP/1.1\r\nHost: {address}\r\n\r\n")?;
        read_until(&mut stream, "401 Unauthorized")?;
        hub.stop(None);
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest)?;
        assert!(rest.is_empty());
        Ok(())
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size(b"48 30"), Some((48, 30)));
        assert_eq!(parse_size(b"48"), None);
        assert_eq!(parse_size(b"48 30 1"), None);
        assert_eq!(parse_size(b"-1 30"), None);
    }

    #[test]
    fn pairs_streams_and_accepts_input_over_http() -> io::Result<()> {
        let hub = Arc::new(Hub::default());
        let address = start(&hub, IpAddr::V4(Ipv4Addr::LOCALHOST), 0)?;
        let (_, code) = hub.pairing().expect("session");

        let page = request(
            address,
            &format!("GET / HTTP/1.1\r\nHost: {address}\r\n\r\n"),
        )?;
        assert!(page.starts_with("HTTP/1.1 200 OK"));
        assert!(page.contains("Content-Security-Policy"));

        let rebinding = request(address, "GET / HTTP/1.1\r\nHost: evil.example.com\r\n\r\n")?;
        assert!(rebinding.starts_with("HTTP/1.1 421"));

        let unpaired = post(address, "/input", "", "", "x")?;
        assert!(unpaired.starts_with("HTTP/1.1 401"));

        let wrong = post(address, "/pair", "", "", "000000")?;
        assert!(wrong.starts_with("HTTP/1.1 403"));

        let paired = post(address, "/pair", "", "", &code)?;
        assert!(paired.starts_with("HTTP/1.1 204"));
        let cookie = cookie_from(&paired).expect("cookie");

        let cross_site = request(
            address,
            &format!(
                "POST /input HTTP/1.1\r\nHost: {address}\r\nOrigin: http://evil.example.com\r\n\
                 Cookie: {cookie}\r\nContent-Length: 1\r\n\r\nx"
            ),
        )?;
        assert!(cross_site.starts_with("HTTP/1.1 403"));

        // The cookie alone is not enough: writes need the stream's token.
        assert!(post(address, "/input", &cookie, "", "x")?.starts_with("HTTP/1.1 409"));
        let (mut events, client) = open_events(address, &cookie, "?take=1")?;
        assert!(hub.snapshot().controlled);
        assert!(post(address, "/resize", &cookie, &client, "50 20")?.starts_with("HTTP/1.1 204"));
        assert_eq!(hub.snapshot().size, Some((50, 20)));
        assert!(post(address, "/input", &cookie, &client, "hi")?.starts_with("HTTP/1.1 204"));
        let mut buf = [0u8; 8];
        assert_eq!(hub.take_input(&mut buf), 2);

        assert!(hub.send(Outgoing::Frame("\x1b[1;1Hframe-text".into())));
        assert!(read_until(&mut events, "frame-text")?.contains("event: frame"));

        // A reconnect that is not the controller's own stream is refused
        // rather than silently taking control.
        let refused = request(
            address,
            &format!(
                "GET /events?resume=stale HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\n\r\n"
            ),
        )?;
        assert!(refused.starts_with("HTTP/1.1 409"));
        let session = |client: &str| {
            request(
                address,
                &format!(
                    "GET /session HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\n\
                     X-Yawl-Client: {client}\r\n\r\n"
                ),
            )
        };
        assert!(session("stale")?.starts_with("HTTP/1.1 409"));
        assert!(session(&client)?.starts_with("HTTP/1.1 204"));

        // A second device takes over explicitly; the first keeps its cookie
        // but loses the ability to type or resize.
        let (mut second, second_client) = open_events(address, &cookie, "?take=1")?;
        assert!(read_until(&mut events, "event: replaced")?.contains("replaced"));
        assert!(post(address, "/input", &cookie, &client, "x")?.starts_with("HTTP/1.1 409"));
        assert!(post(address, "/resize", &cookie, &client, "60 20")?.starts_with("HTTP/1.1 409"));
        assert!(post(address, "/input", &cookie, &second_client, "y")?.starts_with("HTTP/1.1 204"));
        let events = &mut second;

        hub.stop(None);
        let mut rest = String::new();
        events.read_to_string(&mut rest)?;
        assert!(rest.contains("event: closed"));
        Ok(())
    }
}
