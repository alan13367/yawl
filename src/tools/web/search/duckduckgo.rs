//! DuckDuckGo's keyless HTML results page: the request, its bot-check
//! detection, and result parsing.

use scraper::Html;

use super::pace::Pace;
use super::{BackendError, Freshness, SEARCH_BODY_LIMIT, SearchResult, quality};
use crate::tools::web::{WebTools, normalized_text, read_body, selector, transport_error};

fn freshness_code(freshness: Freshness) -> &'static str {
    match freshness {
        Freshness::Day => "d",
        Freshness::Week => "w",
        Freshness::Month => "m",
        Freshness::Year => "y",
    }
}

pub(super) fn search(
    tools: &WebTools,
    query: &str,
    freshness: Option<Freshness>,
) -> Result<Vec<SearchResult>, BackendError> {
    let pace = Pace::new(&tools.state_dir);
    pace.wait_for_slot()?;
    let mut form = vec![("q", query)];
    if let Some(freshness) = freshness {
        form.push(("df", freshness_code(freshness)));
    }
    let mut response = tools
        .agent
        .post(&tools.endpoints.duckduckgo)
        .header("accept", "text/html")
        .send_form(form)
        .map_err(|error| BackendError::Failed(transport_error("search", error)))?;
    // DuckDuckGo takes no key, so 403 and 429 mean it is refusing automated
    // searches, like the 202 bot check.
    let status = response.status().as_u16();
    let results = match status {
        202 | 403 | 429 => None,
        200..=299 => {
            let body = read_body(&mut response, SEARCH_BODY_LIMIT, "DuckDuckGo response")
                .map_err(BackendError::Failed)?;
            parse(&body).map_err(BackendError::Failed)?
        }
        _ => {
            return Err(BackendError::Failed(format!(
                "search failed with HTTP status {status}"
            )));
        }
    };
    match results {
        Some(results) => {
            pace.record_success();
            Ok(results)
        }
        None => Err(BackendError::Paused(pace.record_block())),
    }
}

/// Returns `None` when DuckDuckGo served its bot-check puzzle instead of
/// results.
fn parse(body: &str) -> Result<Option<Vec<SearchResult>>, String> {
    let document = Html::parse_document(body);
    let result_selector = selector(".result")?;
    let title_selector = selector(".result__a")?;
    let snippet_selector = selector(".result__snippet")?;
    let mut results = Vec::new();
    let mut containers = 0;
    for result in document.select(&result_selector) {
        containers += 1;
        let classes = result.value().attr("class").unwrap_or_default();
        if classes
            .split_whitespace()
            .any(|class| class.starts_with("result--ad"))
        {
            continue;
        }
        let Some(link) = result.select(&title_selector).next() else {
            continue;
        };
        let Some(url) = link.value().attr("href").and_then(unwrap_url) else {
            continue;
        };
        results.push(SearchResult {
            title: normalized_text(link.text()),
            url,
            snippet: result
                .select(&snippet_selector)
                .next()
                .map(|element| normalized_text(element.text()))
                .unwrap_or_default(),
            published: None,
        });
    }
    let results = quality::clean(results);
    if !results.is_empty() {
        Ok(Some(results))
    } else if document
        .select(&selector(".anomaly-modal__modal, .anomaly-modal__puzzle")?)
        .next()
        .is_some()
    {
        Ok(None)
    } else if document
        .select(&selector(".no-results, .no-results__message")?)
        .next()
        .is_some()
    {
        Ok(Some(Vec::new()))
    } else if containers > 0 {
        Err("results could not be parsed; the page markup may have changed".into())
    } else {
        Err("the response did not contain recognizable search markup".into())
    }
}

fn unwrap_url(href: &str) -> Option<String> {
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    let query = href.split_once('?')?.1;
    let encoded = query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "uddg").then_some(value)
    })?;
    let decoded = percent_decode(encoded)?;
    (decoded.starts_with("http://") || decoded.starts_with("https://")).then_some(decoded)
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = hex(bytes[index + 1])?;
                let low = hex(bytes[index + 2])?;
                output.push(high * 16 + low);
                index += 3;
            }
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(output).ok()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_skips_ads_unwraps_and_deduplicates() {
        let html = r#"
            <div class="result result--ad"><a class="result__a" href="https://ads.test">Ad</a></div>
            <div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa">First</a><span class="result__snippet">Useful  result</span></div>
            <div class="result"><a class="result__a" href="https://example.com/a">Duplicate</a></div>
            <div class="result"><a class="result__a" href="https://example.com/b">Second</a></div>
        "#;
        let results = parse(html).expect("parse").expect("results");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://example.com/a");
        assert_eq!(results[0].snippet, "Useful result");
    }

    #[test]
    fn parser_keeps_a_full_page_of_results() {
        let html: String = (0..12)
            .map(|index| {
                format!(r#"<div class="result"><a class="result__a" href="https://site{index}.test/">Title {index}</a></div>"#)
            })
            .collect();
        assert_eq!(parse(&html).expect("parse").expect("results").len(), 12);
    }

    #[test]
    fn parser_distinguishes_no_results_and_markup_changes() {
        assert_eq!(
            parse("<div class='no-results'>None</div>").expect("empty"),
            Some(Vec::new())
        );
        assert!(parse("<html><body>changed</body></html>").is_err());
        assert!(parse(r#"<div class="result">no link here</div>"#).is_err());
    }

    #[test]
    fn parser_recognizes_the_bot_check_page() {
        let page = r#"<html><body><div class="anomaly-modal__mask"><div class="anomaly-modal__modal">
            <div class="anomaly-modal__title">Unfortunately, bots use DuckDuckGo too.</div>
            <div class="anomaly-modal__puzzle"></div></div></div></body></html>"#;
        assert_eq!(parse(page).expect("bot check page"), None);
    }
}
