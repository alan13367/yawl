//! Local HTTP fixtures shared by the web tool tests.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use super::WebTools;
use crate::config::WebSearchProvider;

pub(super) fn serve(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    serve_bytes(responses.into_iter().map(String::into_bytes).collect())
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
            let mut buffer = [0_u8; 4096];
            let read = stream.read(&mut buffer).expect("read request");
            requests.push(String::from_utf8_lossy(&buffer[..read]).into_owned());
            stream.write_all(&response).expect("write response");
        }
        requests
    });
    (format!("http://{address}"), handle)
}

pub(super) fn test_tools(max_chars: usize) -> WebTools {
    WebTools {
        agent: ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(2)))
            .max_redirects(5)
            .build()
            .into(),
        provider: WebSearchProvider::DuckDuckGo,
        fetch_max_chars: max_chars,
        artifact_dir: std::env::temp_dir()
            .join(format!("yawl-web-artifacts-{}", std::process::id())),
        brave_api_key: None,
        firecrawl_api_key: None,
    }
}
