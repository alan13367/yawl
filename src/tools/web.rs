//! Web browsing tools: `web_search` and `web_fetch`.
//!
//! The facade owns the shared HTTP agent, bounded body reads, cancellation,
//! and untrusted-content delimiters; children own each tool.

mod fetch;
mod html;
mod search;
#[cfg(test)]
mod test_support;

use std::io::Read;
use std::time::Duration;

use scraper::Selector;
use serde_json::Value;

use crate::config::{Config, WebSearchProvider};

use super::{ToolEntry, ToolOutcome, str_arg};

const UNTRUSTED_BEGIN_PREFIX: &str = "[BEGIN UNTRUSTED";
const UNTRUSTED_END_PREFIX: &str = "[END UNTRUSTED";
const UNTRUSTED_SEARCH_BEGIN: &str = "[BEGIN UNTRUSTED WEB SEARCH RESULTS]";
const UNTRUSTED_SEARCH_END: &str = "[END UNTRUSTED WEB SEARCH RESULTS]";
const UNTRUSTED_CONTENT_BEGIN: &str = "[BEGIN UNTRUSTED WEB CONTENT]";
const UNTRUSTED_CONTENT_END: &str = "[END UNTRUSTED WEB CONTENT]";

pub(super) struct WebTools {
    agent: ureq::Agent,
    provider: WebSearchProvider,
    fetch_max_chars: usize,
    /// Where pages longer than `fetch_max_chars` are saved in full.
    artifact_dir: std::path::PathBuf,
    brave_api_key: Option<String>,
    firecrawl_api_key: Option<String>,
}

impl WebTools {
    pub(super) fn new(config: &Config) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .max_redirects(5)
            .max_redirects_will_error(true)
            .user_agent("Yawl/0.1 web tools")
            .build()
            .into();
        WebTools {
            agent,
            provider: config.web_search_provider,
            fetch_max_chars: config.web_fetch_max_chars,
            artifact_dir: config.home_dir.join("artifacts/tool-output"),
            brave_api_key: config.brave_api_key.clone(),
            firecrawl_api_key: config.firecrawl_api_key.clone(),
        }
    }

    pub(super) fn entries(provider: WebSearchProvider) -> Vec<ToolEntry> {
        vec![search::entry(provider), fetch::entry()]
    }
}

fn validate_http_url(url: &str) -> Result<(), String> {
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|_| "'url' must be a valid http or https URL".to_string())?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.authority().is_none() {
        return Err("'url' must be a valid http or https URL".into());
    }
    Ok(())
}

fn transport_error(operation: &str, error: ureq::Error) -> String {
    if crate::cancellation::interrupted() {
        format!("{operation} interrupted")
    } else if error.to_string().to_ascii_lowercase().contains("timeout") {
        format!("{operation} timed out")
    } else {
        format!("{operation} request failed: {error}")
    }
}

pub(super) fn interrupted() -> Result<(), String> {
    if crate::cancellation::interrupted() {
        Err("web operation interrupted".into())
    } else {
        Ok(())
    }
}

fn read_body(
    response: &mut ureq::http::Response<ureq::Body>,
    limit: usize,
    label: &str,
) -> Result<String, String> {
    read_body_while(response, limit, label, crate::cancellation::interrupted)
}

fn read_body_while(
    response: &mut ureq::http::Response<ureq::Body>,
    limit: usize,
    label: &str,
    mut is_interrupted: impl FnMut() -> bool,
) -> Result<String, String> {
    let wire_limit = limit.saturating_add(1) as u64;
    let mut reader = response.body_mut().with_config().limit(wire_limit).reader();
    let mut output = Vec::with_capacity(limit.min(16 * 1024));
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        if is_interrupted() {
            return Err("web operation interrupted".into());
        }
        let read = match reader.read(&mut chunk) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                if is_interrupted() {
                    return Err("web operation interrupted".into());
                }
                continue;
            }
            Err(error) => {
                if error.to_string().to_ascii_lowercase().contains("limit") {
                    return Err(format!("{label} exceeded the {limit}-byte response limit"));
                }
                return Err(format!("could not read {label}: {error}"));
            }
        };
        if read == 0 {
            break;
        }
        if output.len().saturating_add(read) > limit {
            return Err(format!(
                "{label} exceeded the {limit}-byte decoded response limit"
            ));
        }
        output.extend_from_slice(&chunk[..read]);
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

fn compact_field(value: &str, max_chars: usize) -> String {
    let mut value = neutralize_untrusted_delimiters(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if truncate_chars(&mut value, max_chars) {
        value.push('…');
    }
    value
}

fn neutralize_untrusted_delimiters(value: &str) -> String {
    value
        .replace(UNTRUSTED_BEGIN_PREFIX, "(BEGIN UNTRUSTED")
        .replace(UNTRUSTED_END_PREFIX, "(END UNTRUSTED")
}

pub(super) fn selector(value: &str) -> Result<Selector, String> {
    Selector::parse(value).map_err(|_| "internal HTML selector is invalid".to_string())
}

pub(super) fn normalized_text<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_chars(value: &mut String, max_chars: usize) -> bool {
    if let Some((index, _)) = value.char_indices().nth(max_chars) {
        value.truncate(index);
        true
    } else {
        false
    }
}

pub(super) fn execute(web: Option<&WebTools>, args: &Value, search: bool) -> ToolOutcome {
    let Some(web) = web else {
        return ToolOutcome::error("web browsing is disabled");
    };
    let result = if search {
        str_arg(args, "query").and_then(|query| web.search(query).map_err(ToolOutcome::error))
    } else {
        str_arg(args, "url").and_then(|url| web.fetch(url).map_err(ToolOutcome::error))
    };
    match result {
        Ok(content) => ToolOutcome::ok(content),
        Err(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{serve, serve_bytes, test_tools};
    use super::*;

    #[test]
    fn http_url_validation_rejects_non_http_schemes() {
        assert!(validate_http_url("https://example.com/a").is_ok());
        assert!(validate_http_url("http://127.0.0.1/").is_ok());
        assert!(validate_http_url("file:///etc/passwd").is_err());
        assert!(validate_http_url("javascript:alert(1)").is_err());
        assert!(validate_http_url("data:text/html,hi").is_err());
        assert!(validate_http_url("ftp://example.com/file").is_err());
        assert!(validate_http_url("/relative").is_err());
        assert!(validate_http_url("").is_err());
        assert!(test_tools(20_000).fetch("file:///etc/passwd").is_err());
        assert!(test_tools(20_000).fetch("javascript:alert(1)").is_err());
        assert!(test_tools(20_000).fetch("data:text/html,hi").is_err());
    }

    #[test]
    fn truncation_is_character_safe() {
        let mut text = "aé日z".to_string();
        assert!(truncate_chars(&mut text, 3));
        assert_eq!(text, "aé日");
    }

    #[test]
    fn decoded_response_limit_stops_compressed_expansion() {
        const GZIP_1024_AS: &[u8] = &[
            31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 75, 76, 28, 5, 163, 96, 20, 140, 84, 0, 0, 185, 151,
            85, 124, 0, 4, 0, 0,
        ];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            GZIP_1024_AS.len()
        )
        .into_bytes();
        response.extend_from_slice(GZIP_1024_AS);
        let (base, server) = serve_bytes(vec![response]);
        let tools = test_tools(20_000);
        let mut response = tools.agent.get(&base).call().expect("compressed response");
        let error = read_body(&mut response, 64, "compressed page").expect_err("decoded limit");
        server.join().expect("compressed server");
        assert!(error.contains("decoded response limit"), "{error}");
    }

    #[test]
    fn body_reader_checks_cancellation_between_chunks() {
        let (base, server) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into(),
        ]);
        let tools = test_tools(20_000);
        let mut response = tools.agent.get(&base).call().expect("text response");
        let mut checks = 0;
        let error = read_body_while(&mut response, 64, "page", || {
            checks += 1;
            checks > 1
        })
        .expect_err("interrupted read");
        server.join().expect("text server");
        assert_eq!(error, "web operation interrupted");
    }
}
