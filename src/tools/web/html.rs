//! HTML-to-Markdown extraction for fetched pages.
//!
//! Request execution and search-provider parsing stay in the web facade;
//! this child converts response bodies to readable text with bounded
//! concurrency and cancellation.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use scraper::{ElementRef, Html, Node};

use super::{interrupted, normalized_text, selector};

const HTML_PROCESSING_TIMEOUT: Duration = Duration::from_secs(10);
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// Nesting depth ceiling for Markdown rendering. Browsers cap DOM depth for
/// the same reason: an adversarial page can otherwise overflow the worker
/// thread stack and take the whole process down.
const MAX_DOM_DEPTH: usize = 256;

static HTML_EXTRACT_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

fn visible_text_chars(element: ElementRef<'_>) -> usize {
    element
        .text()
        .flat_map(str::split_whitespace)
        .map(|word| word.chars().count())
        .sum()
}

/// Bounds concurrent HTML extraction by waiting for the single slot. The
/// worker retains the slot until it exits, even if its caller stops waiting.
fn acquire_html_extract_slot(started: Instant) -> Result<HtmlExtractSlot, String> {
    loop {
        interrupted()?;
        if HTML_EXTRACT_IN_FLIGHT
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(HtmlExtractSlot);
        }
        let remaining = HTML_PROCESSING_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err("HTML extraction timed out".into());
        }
        std::thread::sleep(remaining.min(CANCELLATION_POLL_INTERVAL));
    }
}

struct HtmlExtractSlot;

impl Drop for HtmlExtractSlot {
    fn drop(&mut self) {
        HTML_EXTRACT_IN_FLIGHT.store(0, Ordering::Release);
    }
}

pub(super) fn extract_html(body: String) -> Result<(Option<String>, String), String> {
    let started = Instant::now();
    let slot = acquire_html_extract_slot(started)?;
    let result_rx = spawn_html_extract(slot, move || extract_html_inner(&body))?;
    loop {
        interrupted()?;
        let remaining = HTML_PROCESSING_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err("HTML extraction timed out".into());
        }
        match result_rx.recv_timeout(remaining.min(CANCELLATION_POLL_INTERVAL)) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("HTML extraction worker stopped unexpectedly".into());
            }
        }
    }
}

type HtmlExtractResult = Result<(Option<String>, String), String>;

fn spawn_html_extract(
    slot: HtmlExtractSlot,
    extract: impl FnOnce() -> HtmlExtractResult + Send + 'static,
) -> Result<mpsc::Receiver<HtmlExtractResult>, String> {
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("yawl-web-html".into())
        .spawn(move || {
            // A timeout or cancellation only stops the caller from waiting.
            // Retain admission until parsing and sending the result finish.
            let _slot = slot;
            let _ = result_tx.send(extract());
        })
        .map_err(|error| format!("could not start HTML extraction: {error}"))?;
    Ok(result_rx)
}

fn extract_html_inner(body: &str) -> Result<(Option<String>, String), String> {
    let document = Html::parse_document(body);
    let title = document
        .select(&selector("title")?)
        .next()
        .map(|element| normalized_text(element.text()))
        .filter(|title| !title.is_empty());
    let candidates = selector(
        "article, main, [role='main'], #content, #main-content, .content, .article, .post",
    )?;
    let body_selector = selector("body")?;
    let root = document
        .select(&candidates)
        .max_by_key(|element| visible_text_chars(*element))
        .or_else(|| document.select(&body_selector).next())
        .ok_or_else(|| "the fetched HTML page did not contain a readable body".to_string())?;
    let mut output = String::new();
    render_element(root, &mut output, 0);
    let output = cleanup_markdown(&output);
    if output.is_empty() {
        Err("the fetched HTML page did not contain readable content".into())
    } else {
        Ok((title, output))
    }
}

fn render_element(element: ElementRef<'_>, output: &mut String, depth: usize) {
    if depth >= MAX_DOM_DEPTH {
        return;
    }
    let tag = element.value().name();
    if matches!(
        tag,
        "script"
            | "style"
            | "nav"
            | "header"
            | "footer"
            | "aside"
            | "form"
            | "button"
            | "svg"
            | "noscript"
            | "template"
    ) {
        return;
    }
    match tag {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            block_break(output);
            let level = tag[1..].parse::<usize>().unwrap_or(1);
            output.push_str(&"#".repeat(level));
            output.push(' ');
            render_children(element, output, depth);
            block_break(output);
        }
        "p" | "section" | "article" | "main" | "div" => {
            block_break(output);
            render_children(element, output, depth);
            block_break(output);
        }
        "li" => {
            line_break(output);
            output.push_str("- ");
            render_children(element, output, depth);
            line_break(output);
        }
        "blockquote" => {
            block_break(output);
            output.push_str("> ");
            render_children(element, output, depth);
            block_break(output);
        }
        "pre" => {
            block_break(output);
            output.push_str("```\n");
            output.push_str(&element.text().collect::<String>());
            output.push_str("\n```\n\n");
        }
        "code" => {
            output.push('`');
            output.push_str(&element.text().collect::<String>());
            output.push('`');
        }
        "a" => {
            let text = normalized_text(element.text());
            if text.is_empty() {
                return;
            }
            if let Some(href) = element.value().attr("href")
                && (href.starts_with("http://") || href.starts_with("https://"))
            {
                output.push_str(&format!("[{text}]({href})"));
            } else {
                output.push_str(&text);
            }
        }
        "br" => line_break(output),
        _ => render_children(element, output, depth),
    }
}

fn render_children(element: ElementRef<'_>, output: &mut String, depth: usize) {
    for child in element.children() {
        match child.value() {
            Node::Text(text) => append_text(output, text),
            Node::Element(_) => {
                if let Some(child) = ElementRef::wrap(child) {
                    render_element(child, output, depth.saturating_add(1));
                }
            }
            _ => {}
        }
    }
}

fn append_text(output: &mut String, text: &str) {
    for word in text.split_whitespace() {
        if !output.is_empty()
            && !output.ends_with([' ', '\n', '`'])
            && !word.starts_with([',', '.', ';', ':', '!', '?', ')', ']'])
        {
            output.push(' ');
        }
        output.push_str(word);
    }
}

fn line_break(output: &mut String) {
    while output.ends_with(' ') {
        output.pop();
    }
    if !output.ends_with('\n') {
        output.push('\n');
    }
}

fn block_break(output: &mut String) {
    line_break(output);
    if !output.ends_with("\n\n") {
        output.push('\n');
    }
}

fn cleanup_markdown(value: &str) -> String {
    let mut output = String::new();
    let mut blank = false;
    for line in value.lines() {
        let line = line.trim();
        if line.is_empty() {
            if !blank && !output.is_empty() {
                output.push('\n');
            }
            blank = true;
        } else {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(line);
            blank = false;
        }
    }
    output.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoned_extraction_retains_slot_until_worker_exits() {
        let slot = acquire_html_extract_slot(Instant::now()).expect("first slot");
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let result_rx = spawn_html_extract(slot, move || {
            started_tx.send(()).expect("signal worker started");
            release_rx.recv().expect("release worker");
            Ok((None, "finished".into()))
        })
        .expect("worker");
        started_rx.recv().expect("worker started");
        // Timeout and cancellation both abandon this receiver without
        // stopping the parser. Another caller must not start a worker yet.
        drop(result_rx);
        assert!(acquire_html_extract_slot(Instant::now() - HTML_PROCESSING_TIMEOUT).is_err());
        release_tx.send(()).expect("finish worker");
        let _second =
            acquire_html_extract_slot(Instant::now()).expect("slot is free after worker exits");
    }

    #[test]
    fn deeply_nested_markup_is_bounded_instead_of_overflowing_the_stack() {
        let depth = 1_000;
        let nested = format!(
            "<p>shallow</p>{divs}buried{closes}",
            divs = "<div>".repeat(depth),
            closes = "</div>".repeat(depth),
        );
        let (_title, content) = extract_html(nested).expect("content");
        assert!(content.contains("shallow"));
        // Text past the depth ceiling is dropped rather than recursed into.
        assert!(!content.contains("buried"));
        assert!(content.len() < 64 * 1024);
    }

    #[test]
    fn html_extraction_chooses_main_content_and_renders_markdown() {
        let html = r#"
          <html><head><title> Example &amp; Test </title></head><body>
          <nav>Noise</nav><aside>More noise</aside>
          <main><h1>Hello</h1><p>Unicode café &amp; tea.</p><ul><li>One</li><li><a href="https://example.com">Two</a></li></ul><pre>let x = 1;</pre></main>
          </body></html>
        "#;
        let (title, content) = extract_html(html.to_string()).expect("content");
        assert_eq!(title.as_deref(), Some("Example & Test"));
        assert!(content.contains("# Hello"));
        assert!(content.contains("café & tea."));
        assert!(content.contains("- One"));
        assert!(content.contains("[Two](https://example.com)"));
        assert!(content.contains("```\nlet x = 1;\n```"));
        assert!(!content.contains("Noise"));
    }
}
