//! Local HTTP fixtures shared by the web tool tests.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use super::WebTools;
use crate::config::WebSearchProvider;

pub(super) fn serve(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    serve_bytes(responses.into_iter().map(String::into_bytes).collect())
}

/// Reads one whole request, since a client may send a form body after its
/// headers in a separate packet.
fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).expect("read request");
        request.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&request);
        let Some((headers, body)) = text.split_once("\r\n\r\n") else {
            if read == 0 {
                return text.into_owned();
            }
            continue;
        };
        let expected = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        if read == 0 || body.len() >= expected {
            return text.into_owned();
        }
    }
}

pub(super) fn serve_bytes(
    responses: Vec<Vec<u8>>,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let (mut stream, _) = listener.accept().expect("accept");
            requests.push(read_request(&mut stream));
            stream.write_all(&response).expect("write response");
        }
        requests
    });
    (format!("http://{address}"), handle)
}

pub(super) fn test_tools(max_chars: usize) -> WebTools {
    WebTools {
        agent: crate::provider::cancellable_agent(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(2)))
                .max_redirects(5)
                .build(),
        ),
        provider: WebSearchProvider::DuckDuckGo,
        fallback: false,
        fetch_max_chars: max_chars,
        artifact_dir: std::env::temp_dir()
            .join(format!("yawl-web-artifacts-{}", std::process::id())),
        state_dir: crate::test_support::unique_temp_dir("web-state"),
        brave_api_key: None,
        firecrawl_api_key: None,
        searxng_url: None,
        endpoints: super::search::Endpoints::default(),
        blocks_read: |_| false,
    }
}
