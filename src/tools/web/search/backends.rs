//! Search backends with a documented API: SearXNG, Brave, and Firecrawl.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Freshness, MAX_RESULTS, SEARCH_BODY_LIMIT, SearchResult, quality};
use crate::config::resolve_config_value;
use crate::tools::web::{WebTools, read_body, transport_error, validate_http_url};

/// Where each service answers. Tests point these at local servers.
#[derive(Clone)]
pub(in crate::tools::web) struct Endpoints {
    pub(in crate::tools::web) duckduckgo: String,
    pub(in crate::tools::web) brave: String,
    pub(in crate::tools::web) firecrawl: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            duckduckgo: "https://html.duckduckgo.com/html/".into(),
            brave: "https://api.search.brave.com/res/v1/web/search".into(),
            firecrawl: "https://api.firecrawl.dev/v2/search".into(),
        }
    }
}

pub(super) fn resolved_setting(environment: &str, stored: Option<&str>) -> Result<String, String> {
    if let Ok(value) = std::env::var(environment)
        && !value.trim().is_empty()
    {
        return Ok(value);
    }
    let Some(value) = stored else {
        return Err(format!(
            "{environment} is required for this web search provider"
        ));
    };
    let resolved = resolve_config_value(value)
        .map_err(|_| format!("the configured {environment} reference could not be resolved"))?;
    if resolved.trim().is_empty() {
        Err(format!(
            "{environment} is required for this web search provider"
        ))
    } else {
        Ok(resolved)
    }
}

fn require_success(response: &ureq::http::Response<ureq::Body>) -> Result<(), String> {
    match response.status().as_u16() {
        200..=299 => Ok(()),
        status @ (401 | 403) => Err(format!("authentication failed (HTTP {status})")),
        status @ (402 | 429) => Err(format!("rate limit or quota reached (HTTP {status})")),
        status => Err(format!("search failed with HTTP status {status}")),
    }
}

/// Keeps the date part of a timestamp such as `2026-11-02T10:00:00Z`, the only
/// part worth a model's attention.
fn published_date(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    let date = value.get(..10)?;
    let shaped = date.bytes().enumerate().all(|(index, byte)| match index {
        4 | 7 => byte == b'-',
        _ => byte.is_ascii_digit(),
    });
    shaped.then(|| date.to_string())
}

fn usable(raw_count: usize, results: Vec<SearchResult>) -> Result<Vec<SearchResult>, String> {
    let results = quality::clean(results);
    if raw_count > 0 && results.is_empty() {
        Err("the response did not contain usable web results".into())
    } else {
        Ok(results)
    }
}

#[derive(Deserialize)]
struct WireResult {
    title: Option<String>,
    url: Option<String>,
    /// Brave and Firecrawl.
    description: Option<String>,
    /// SearXNG.
    content: Option<String>,
    /// Brave.
    page_age: Option<String>,
    /// SearXNG.
    #[serde(rename = "publishedDate")]
    published_date: Option<String>,
}

fn collect(wire: Vec<WireResult>) -> Result<Vec<SearchResult>, String> {
    let raw_count = wire.len();
    let results = wire
        .into_iter()
        .filter_map(|result| {
            Some(SearchResult {
                title: result.title?,
                url: result.url?,
                snippet: result.description.or(result.content).unwrap_or_default(),
                published: published_date(
                    result
                        .page_age
                        .as_deref()
                        .or(result.published_date.as_deref()),
                ),
            })
        })
        .collect();
    usable(raw_count, results)
}

fn brave_freshness(freshness: Freshness) -> &'static str {
    match freshness {
        Freshness::Day => "pd",
        Freshness::Week => "pw",
        Freshness::Month => "pm",
        Freshness::Year => "py",
    }
}

pub(super) fn brave(
    tools: &WebTools,
    query: &str,
    freshness: Option<Freshness>,
) -> Result<Vec<SearchResult>, String> {
    let key = resolved_setting("BRAVE_API_KEY", tools.brave_api_key.as_deref())?;
    let mut request = tools
        .agent
        .get(&tools.endpoints.brave)
        .query("q", query)
        .query("count", MAX_RESULTS.to_string())
        .header("accept", "application/json")
        .header("x-subscription-token", key);
    if let Some(freshness) = freshness {
        request = request.query("freshness", brave_freshness(freshness));
    }
    let mut response = request
        .call()
        .map_err(|error| transport_error("search", error))?;
    require_success(&response)?;
    parse_brave(&read_body(
        &mut response,
        SEARCH_BODY_LIMIT,
        "Brave response",
    )?)
}

fn parse_brave(body: &str) -> Result<Vec<SearchResult>, String> {
    #[derive(Deserialize)]
    struct BraveResponse {
        web: Option<BraveWeb>,
    }
    #[derive(Deserialize)]
    struct BraveWeb {
        #[serde(default)]
        results: Vec<WireResult>,
    }
    let response: BraveResponse =
        serde_json::from_str(body).map_err(|_| "the response was malformed".to_string())?;
    // Brave omits `web` altogether when a query has no web results.
    collect(response.web.map(|web| web.results).unwrap_or_default())
}

fn firecrawl_body(query: &str, freshness: Option<Freshness>) -> Value {
    let mut body = json!({
        "query": query,
        "limit": MAX_RESULTS,
        "sources": ["web"]
    });
    if let Some(freshness) = freshness {
        body["tbs"] = json!(match freshness {
            Freshness::Day => "qdr:d",
            Freshness::Week => "qdr:w",
            Freshness::Month => "qdr:m",
            Freshness::Year => "qdr:y",
        });
    }
    body
}

pub(super) fn firecrawl(
    tools: &WebTools,
    query: &str,
    freshness: Option<Freshness>,
) -> Result<Vec<SearchResult>, String> {
    let key = resolved_setting("FIRECRAWL_API_KEY", tools.firecrawl_api_key.as_deref())?;
    let mut response = tools
        .agent
        .post(&tools.endpoints.firecrawl)
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .send(firecrawl_body(query, freshness).to_string())
        .map_err(|error| transport_error("search", error))?;
    require_success(&response)?;
    parse_firecrawl(&read_body(
        &mut response,
        SEARCH_BODY_LIMIT,
        "Firecrawl response",
    )?)
}

fn parse_firecrawl(body: &str) -> Result<Vec<SearchResult>, String> {
    #[derive(Deserialize)]
    struct FirecrawlResponse {
        data: Option<FirecrawlData>,
    }
    #[derive(Deserialize)]
    struct FirecrawlData {
        #[serde(default)]
        web: Vec<WireResult>,
    }
    let response: FirecrawlResponse =
        serde_json::from_str(body).map_err(|_| "the response was malformed".to_string())?;
    let Some(data) = response.data else {
        return Err("the response was missing result data".into());
    };
    collect(data.web)
}

/// SearXNG documents only these ranges, so a week uses the nearest wider one.
fn searxng_range(freshness: Freshness) -> &'static str {
    match freshness {
        Freshness::Day => "day",
        Freshness::Week | Freshness::Month => "month",
        Freshness::Year => "year",
    }
}

pub(super) fn searxng(
    tools: &WebTools,
    query: &str,
    freshness: Option<Freshness>,
) -> Result<Vec<SearchResult>, String> {
    let base = resolved_setting("SEARXNG_URL", tools.searxng_url.as_deref())?;
    let url = format!("{}/search", base.trim().trim_end_matches('/'));
    validate_http_url(&url).map_err(|_| "the SearXNG URL must use http or https".to_string())?;
    let mut request = tools
        .agent
        .get(&url)
        .query("q", query)
        .query("format", "json")
        .header("accept", "application/json");
    if let Some(freshness) = freshness {
        request = request.query("time_range", searxng_range(freshness));
    }
    let mut response = request
        .call()
        .map_err(|error| transport_error("search", error))?;
    match response.status().as_u16() {
        200..=299 => {}
        403 => {
            return Err(
                "the instance refused JSON output (HTTP 403); add json under search.formats in its settings.yml"
                    .into(),
            );
        }
        429 => return Err("the instance's rate limit was reached (HTTP 429)".into()),
        status => return Err(format!("search failed with HTTP status {status}")),
    }
    parse_searxng(&read_body(
        &mut response,
        SEARCH_BODY_LIMIT,
        "SearXNG response",
    )?)
}

fn parse_searxng(body: &str) -> Result<Vec<SearchResult>, String> {
    #[derive(Deserialize)]
    struct SearxngResponse {
        results: Option<Vec<WireResult>>,
        #[serde(default)]
        unresponsive_engines: Vec<Value>,
    }
    let response: SearxngResponse =
        serde_json::from_str(body).map_err(|_| "the response was malformed".to_string())?;
    let Some(results) = response.results else {
        return Err("the response was missing results".into());
    };
    // Engines that failed say nothing about whether results exist, so this is
    // a failure to report, not an empty answer to remember.
    if results.is_empty() && !response.unresponsive_engines.is_empty() {
        return Err(format!(
            "no results, and {} of its engines did not respond",
            response.unresponsive_engines.len()
        ));
    }
    collect(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brave_responses_need_valid_json_and_usable_results() {
        let results = parse_brave(
            r#"{"web":{"results":[{"title":"A","url":"https://a.test","description":"S","page_age":"2026-11-02T10:00:00"}]}}"#,
        )
        .expect("brave");
        assert_eq!(results[0].title, "A");
        assert_eq!(results[0].snippet, "S");
        assert_eq!(results[0].published.as_deref(), Some("2026-11-02"));
        assert_eq!(parse_brave("{}").expect("no web results"), Vec::new());
        assert!(parse_brave("not json").is_err());
        assert!(parse_brave(r#"{"web":{"results":[{"title":"missing URL"}]}}"#).is_err());
    }

    #[test]
    fn firecrawl_responses_require_the_data_envelope() {
        assert_eq!(
            parse_firecrawl(
                r#"{"data":{"web":[{"title":"B","url":"https://b.test","description":"T"}]}}"#
            )
            .expect("firecrawl")[0]
                .title,
            "B"
        );
        assert!(parse_firecrawl("not json").is_err());
        assert!(parse_firecrawl("{}").is_err());
        assert!(
            parse_firecrawl(r#"{"data":{"web":[{"url":"https://missing-title.test"}]}}"#).is_err()
        );
    }

    #[test]
    fn searxng_responses_map_content_and_tolerate_missing_dates() {
        let results = parse_searxng(
            r#"{"results":[
                {"title":"C","url":"https://c.test","content":"Body","publishedDate":"2026-10-01T00:00:00"},
                {"title":"D","url":"https://d.test","content":"Body","publishedDate":null}
            ]}"#,
        )
        .expect("searxng");
        assert_eq!(results[0].snippet, "Body");
        assert_eq!(results[0].published.as_deref(), Some("2026-10-01"));
        assert_eq!(results[1].published, None);
        assert_eq!(
            parse_searxng(r#"{"results":[]}"#).expect("empty"),
            Vec::new()
        );
        assert!(
            parse_searxng(r#"{"results":[],"unresponsive_engines":[["google","timeout"]]}"#)
                .expect_err("engines failed")
                .contains("1 of its engines")
        );
        assert_eq!(
            parse_searxng(
                r#"{"results":[{"title":"E","url":"https://e.test"}],"unresponsive_engines":[["google","timeout"]]}"#
            )
            .expect("partial answer")
            .len(),
            1
        );
        assert!(parse_searxng("{}").is_err());
    }

    #[test]
    fn dates_that_are_not_iso_are_dropped() {
        assert_eq!(published_date(Some("2 days ago")), None);
        assert_eq!(published_date(Some("")), None);
        assert_eq!(published_date(None), None);
        assert_eq!(
            published_date(Some("2026-01-05")).as_deref(),
            Some("2026-01-05")
        );
    }

    #[test]
    fn firecrawl_requests_full_pages_and_time_filters_only_when_asked() {
        let plain = firecrawl_body("rust", None);
        assert_eq!(plain["limit"], MAX_RESULTS);
        assert!(plain.get("tbs").is_none());
        assert!(plain.get("scrapeOptions").is_none());
        assert_eq!(
            firecrawl_body("rust", Some(Freshness::Week))["tbs"],
            "qdr:w"
        );
    }

    #[test]
    fn stored_settings_accept_references_and_reject_blanks() {
        assert!(resolved_setting("YAWL_TEST_UNSET_SETTING", None).is_err());
        assert_eq!(
            resolved_setting("YAWL_TEST_UNSET_SETTING", Some("value")).as_deref(),
            Ok("value")
        );
        assert!(resolved_setting("YAWL_TEST_UNSET_SETTING", Some("  ")).is_err());
        assert!(resolved_setting("YAWL_TEST_UNSET_SETTING", Some("$YAWL_TEST_UNSET_REF")).is_err());
    }
}
