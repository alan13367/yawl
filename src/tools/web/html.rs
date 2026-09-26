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
/// Keep table padding proportional to the cells actually present in the DOM.
const MAX_TABLE_PADDING_FACTOR: usize = 4;

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

/// Converts `body` to Markdown. `page_url` resolves relative links.
pub(super) fn extract_html(
    body: String,
    page_url: &str,
) -> Result<(Option<String>, String), String> {
    let started = Instant::now();
    let slot = acquire_html_extract_slot(started)?;
    let page_url = page_url.to_string();
    let result_rx = spawn_html_extract(slot, move || extract_html_inner(&body, &page_url))?;
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

struct Render {
    /// Absolute URL that relative links resolve against.
    base: String,
    #[cfg(test)]
    table_visits: std::cell::Cell<usize>,
}

fn extract_html_inner(body: &str, page_url: &str) -> Result<(Option<String>, String), String> {
    let document = Html::parse_document(body);
    let title = document
        .select(&selector("title")?)
        .next()
        .map(|element| normalized_text(element.text()))
        .filter(|title| !title.is_empty());
    let base = document
        .select(&selector("base[href]")?)
        .next()
        .and_then(|element| element.value().attr("href"))
        .and_then(|href| resolve_link(page_url, href))
        .unwrap_or_else(|| page_url.to_string());
    let candidates = selector(
        "article, main, [role='main'], #content, #main-content, .content, .article, .post",
    )?;
    let body_selector = selector("body")?;
    let root = document
        .select(&candidates)
        .max_by_key(|element| visible_text_chars(*element))
        .or_else(|| document.select(&body_selector).next())
        .ok_or_else(|| "the fetched HTML page did not contain a readable body".to_string())?;
    let render = Render {
        base,
        #[cfg(test)]
        table_visits: std::cell::Cell::new(0),
    };
    let mut output = String::new();
    render_element(root, &mut output, 0, &render);
    let output = cleanup_markdown(&output);
    if output.is_empty() {
        Err("the fetched HTML page did not contain readable content".into())
    } else {
        Ok((title, output))
    }
}

fn render_element(element: ElementRef<'_>, output: &mut String, depth: usize, render: &Render) {
    if depth >= MAX_DOM_DEPTH {
        return;
    }
    let tag = element.value().name();
    if matches!(
        tag,
        "script" | "style" | "nav" | "aside" | "form" | "button" | "svg" | "noscript" | "template"
    ) || is_page_chrome(element)
    {
        return;
    }
    match tag {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            block_break(output);
            let level = tag[1..].parse::<usize>().unwrap_or(1);
            output.push_str(&"#".repeat(level));
            output.push(' ');
            render_children(element, output, depth, render);
            block_break(output);
        }
        "p" | "section" | "article" | "main" | "div" | "header" | "footer" => {
            block_break(output);
            render_children(element, output, depth, render);
            block_break(output);
        }
        "ul" | "ol" => render_list(element, output, depth, render),
        "li" => {
            line_break(output);
            output.push_str("- ");
            render_children(element, output, depth, render);
            line_break(output);
        }
        "table" => render_table(element, output, depth, render),
        "blockquote" => {
            block_break(output);
            output.push_str("> ");
            render_children(element, output, depth, render);
            block_break(output);
        }
        "pre" => {
            block_break(output);
            output.push_str("```");
            output.push_str(&code_language(element));
            output.push('\n');
            output.push_str(element.text().collect::<String>().trim_end_matches('\n'));
            output.push_str("\n```\n\n");
        }
        "code" => {
            space_before_inline(output);
            output.push('`');
            output.push_str(&element.text().collect::<String>());
            output.push('`');
        }
        "a" => {
            let text = normalized_text(element.text());
            if text.is_empty() {
                return;
            }
            match element
                .value()
                .attr("href")
                .and_then(|href| resolve_link(&render.base, href))
            {
                Some(url) => {
                    space_before_inline(output);
                    output.push_str(&format!("[{text}]({url})"));
                }
                None => append_text(output, &text),
            }
        }
        "br" => line_break(output),
        _ => render_children(element, output, depth, render),
    }
}

/// Site headers and footers, including title bars inside `main`, carry menus
/// and language lists; inside an `article` they hold its title and byline.
fn is_page_chrome(element: ElementRef<'_>) -> bool {
    let value = element.value();
    if value
        .attr("role")
        .is_some_and(|role| matches!(role, "navigation" | "banner" | "contentinfo" | "search"))
    {
        return true;
    }
    matches!(value.name(), "header" | "footer")
        && !element
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(|ancestor| ancestor.value().name() == "article")
}

fn render_children(element: ElementRef<'_>, output: &mut String, depth: usize, render: &Render) {
    for child in element.children() {
        match child.value() {
            Node::Text(text) => append_text(output, text),
            Node::Element(_) => {
                if let Some(child) = ElementRef::wrap(child) {
                    render_element(child, output, depth.saturating_add(1), render);
                }
            }
            _ => {}
        }
    }
}

fn render_list(list: ElementRef<'_>, output: &mut String, depth: usize, render: &Render) {
    let ordered = list.value().name() == "ol";
    let mut number = list
        .value()
        .attr("start")
        .and_then(|start| start.trim().parse::<u64>().ok())
        .unwrap_or(1);
    let nesting = list
        .ancestors()
        .filter_map(ElementRef::wrap)
        .filter(|ancestor| matches!(ancestor.value().name(), "ul" | "ol"))
        .count();
    let indent = "  ".repeat(nesting);
    line_break(output);
    for child in list.children() {
        match child.value() {
            Node::Text(text) => append_text(output, text),
            Node::Element(_) => {
                let Some(child) = ElementRef::wrap(child) else {
                    continue;
                };
                if child.value().name() != "li" {
                    render_element(child, output, depth.saturating_add(1), render);
                    continue;
                }
                line_break(output);
                output.push_str(&indent);
                if ordered {
                    output.push_str(&format!("{number}. "));
                    number = number.saturating_add(1);
                } else {
                    output.push_str("- ");
                }
                render_children(child, output, depth.saturating_add(1), render);
                line_break(output);
            }
            _ => {}
        }
    }
    line_break(output);
}

/// Renders data tables as Markdown. Single-column tables are usually page
/// layout, so their cells render as ordinary blocks. Sparse tables use the
/// same fallback to avoid expanding a small HTML input into a huge rectangle.
fn render_table(table: ElementRef<'_>, output: &mut String, depth: usize, render: &Render) {
    #[cfg(test)]
    render.table_visits.set(render.table_visits.get() + 1);
    let mut rows = Vec::new();
    for child in table.children().filter_map(ElementRef::wrap) {
        match child.value().name() {
            "tr" => rows.push(child),
            "thead" | "tbody" | "tfoot" => rows.extend(
                child
                    .children()
                    .filter_map(ElementRef::wrap)
                    .filter(|row| row.value().name() == "tr"),
            ),
            _ => {}
        }
    }
    let rows = rows
        .into_iter()
        .map(|row| {
            row.children()
                .filter_map(ElementRef::wrap)
                .filter(|cell| matches!(cell.value().name(), "th" | "td"))
                .collect::<Vec<_>>()
        })
        .filter(|cells| !cells.is_empty())
        .collect::<Vec<_>>();
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let cells = rows.iter().map(Vec::len).sum::<usize>();
    // Decide from DOM shape alone. Rendering cells before this fallback would
    // visit nested layout tables twice per level, causing exponential work.
    if columns < 2
        || columns.saturating_mul(rows.len()) > cells.saturating_mul(MAX_TABLE_PADDING_FACTOR)
    {
        block_break(output);
        render_children(table, output, depth, render);
        block_break(output);
        return;
    }
    block_break(output);
    for (index, row) in rows.iter().enumerate() {
        output.push('|');
        for column in 0..columns {
            output.push(' ');
            if let Some(cell) = row.get(column) {
                let mut text = String::new();
                render_children(*cell, &mut text, depth.saturating_add(2), render);
                output
                    .push_str(&normalized_text(std::iter::once(text.as_str())).replace('|', "\\|"));
            }
            output.push_str(" |");
        }
        output.push('\n');
        if index == 0 {
            output.push('|');
            output.push_str(&" --- |".repeat(columns));
            output.push('\n');
        }
    }
    output.push('\n');
}

/// Reads a `language-*` or `lang-*` class from a `pre` or its `code` child.
fn code_language(pre: ElementRef<'_>) -> String {
    let code = pre
        .children()
        .filter_map(ElementRef::wrap)
        .find(|child| child.value().name() == "code");
    [Some(pre), code]
        .into_iter()
        .flatten()
        .filter_map(|element| element.value().attr("class"))
        .flat_map(str::split_whitespace)
        .find_map(|class| {
            class
                .strip_prefix("language-")
                .or_else(|| class.strip_prefix("lang-"))
        })
        .map(|language| {
            language
                .chars()
                .filter(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '+' | '#' | '-' | '_')
                })
                .take(20)
                .collect()
        })
        .unwrap_or_default()
}

/// Resolves `href` against the absolute `base` URL. Returns `None` for
/// same-page fragments and non-HTTP schemes such as `mailto:`.
fn resolve_link(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let resolved = if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if href
        .split(['/', '?', '#'])
        .next()
        .is_some_and(|first| first.contains(':'))
    {
        return None;
    } else {
        let uri: ureq::http::Uri = base.parse().ok()?;
        let scheme = uri.scheme_str()?;
        let authority = uri.authority()?.as_str();
        if let Some(rest) = href.strip_prefix("//") {
            format!("{scheme}://{rest}")
        } else if href.starts_with('/') {
            format!("{scheme}://{authority}{}", normalize_path(href))
        } else if href.starts_with('?') {
            format!("{scheme}://{authority}{}{href}", uri.path())
        } else {
            let path = uri.path();
            let directory = &path[..path.rfind('/').map_or(0, |index| index + 1)];
            format!(
                "{scheme}://{authority}{}",
                normalize_path(&format!("/{}{href}", directory.trim_start_matches('/')))
            )
        }
    };
    super::validate_http_url(&resolved)
        .is_ok()
        .then_some(resolved)
}

/// Applies `.` and `..` segments in an absolute path, keeping any query or
/// fragment unchanged.
fn normalize_path(path: &str) -> String {
    let split = path.find(['?', '#']).unwrap_or(path.len());
    let (path, suffix) = path.split_at(split);
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/').skip(1) {
        match segment {
            "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    let trailing = path.ends_with("/.") || path.ends_with("/..");
    let mut normalized = format!("/{}", segments.join("/"));
    if trailing && !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized.push_str(suffix);
    normalized
}

fn append_text(output: &mut String, text: &str) {
    for word in text.split_whitespace() {
        if !output.is_empty()
            && !output.ends_with([' ', '\n', '(', '['])
            && !word.starts_with([',', '.', ';', ':', '!', '?', ')', ']'])
        {
            output.push(' ');
        }
        output.push_str(word);
    }
}

/// Separates an inline link or code span from the preceding word.
fn space_before_inline(output: &mut String) {
    if !output.is_empty() && !output.ends_with([' ', '\n', '(', '[', '"']) {
        output.push(' ');
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

/// Collapses blank runs and stray whitespace, but keeps code blocks verbatim
/// and the leading indentation that nests list items.
fn cleanup_markdown(value: &str) -> String {
    let mut output = String::new();
    let mut blank = false;
    let mut in_code = false;
    for line in value.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code = !in_code;
        } else if in_code {
            output.push('\n');
            output.push_str(line.trim_end());
            continue;
        }
        let line = if is_list_item(trimmed) {
            line.trim_end()
        } else {
            trimmed
        };
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

fn is_list_item(line: &str) -> bool {
    line.starts_with("- ")
        || line.split_once(". ").is_some_and(|(number, _)| {
            !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
        })
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
        let (_title, content) = extract_html(nested, "https://example.com/").expect("content");
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
        let (title, content) =
            extract_html(html.to_string(), "https://example.com/").expect("content");
        assert_eq!(title.as_deref(), Some("Example & Test"));
        assert!(content.contains("# Hello"));
        assert!(content.contains("café & tea."));
        assert!(content.contains("- One"));
        assert!(content.contains("[Two](https://example.com)"));
        assert!(content.contains("```\nlet x = 1;\n```"));
        assert!(!content.contains("Noise"));
    }

    #[test]
    fn relative_links_resolve_against_the_page_or_base_url() {
        let page = "https://docs.example.com/guide/start/intro.html?x=1";
        for (href, expected) in [
            ("/api", Some("https://docs.example.com/api")),
            (
                "../api/index.html#top",
                Some("https://docs.example.com/guide/api/index.html#top"),
            ),
            (
                "next.html",
                Some("https://docs.example.com/guide/start/next.html"),
            ),
            ("./a/../b", Some("https://docs.example.com/guide/start/b")),
            ("//cdn.example.com/x", Some("https://cdn.example.com/x")),
            (
                "?page=2",
                Some("https://docs.example.com/guide/start/intro.html?page=2"),
            ),
            ("https://other.test/y", Some("https://other.test/y")),
            ("#section", None),
            ("mailto:a@example.com", None),
            ("javascript:alert(1)", None),
        ] {
            assert_eq!(resolve_link(page, href).as_deref(), expected, "{href}");
        }

        let html = r#"<html><head><base href="https://mirror.example.com/v2/"></head>
            <body><main><p><a href="setup">Setup</a></p></main></body></html>"#;
        let (_, content) = extract_html(html.into(), page).expect("content");
        assert!(
            content.contains("[Setup](https://mirror.example.com/v2/setup)"),
            "{content}"
        );
    }

    #[test]
    fn inline_links_and_code_are_separated_from_surrounding_words() {
        let html = r#"<main><p>Read the <a href="/guide">guide</a>, then add the<code>: u32</code>type (<a href="/api">API</a>).</p></main>"#;
        let (_, content) = extract_html(html.into(), "https://docs.example.com/").expect("content");
        assert_eq!(
            content,
            "Read the [guide](https://docs.example.com/guide), then add the `: u32` type ([API](https://docs.example.com/api))."
        );
    }

    #[test]
    fn tables_lists_and_code_keep_their_structure() {
        let html = r#"<html><body><main>
            <table><thead><tr><th>Name</th><th>Type</th></tr></thead>
            <tbody><tr><td>limit</td><td>integer | null</td></tr><tr><td>path</td></tr></tbody></table>
            <table><tr><td><p>Layout cell</p></td></tr></table>
            <ol start="3"><li>Third<ul><li>Nested</li></ul></li><li>Fourth</li></ol>
            <pre><code class="language-python">def f():
    return 1
</code></pre>
            </main></body></html>"#;
        let (_, content) = extract_html(html.into(), "https://example.com/").expect("content");
        assert!(
            content.contains(
                "| Name | Type |\n| --- | --- |\n| limit | integer \\| null |\n| path |  |"
            ),
            "{content}"
        );
        assert!(content.contains("Layout cell"));
        assert!(!content.contains("| Layout cell |"));
        assert!(
            content.contains("3. Third\n  - Nested\n4. Fourth"),
            "{content}"
        );
        assert!(
            content.contains("```python\ndef f():\n    return 1\n```"),
            "{content}"
        );
    }

    #[test]
    fn sparse_tables_do_not_expand_to_the_widest_row() {
        let size = 2_000;
        let html = format!(
            "<main><table><tr>{}</tr>{}</table><p>After table</p></main>",
            "<td>wide</td>".repeat(size),
            "<tr><td>narrow</td></tr>".repeat(size),
        );
        let (_, content) = extract_html_inner(&html, "https://example.com/").expect("content");
        assert_eq!(content.matches("wide").count(), size);
        assert_eq!(content.matches("narrow").count(), size);
        assert!(content.contains("After table"));
        assert!(!content.contains("| --- |"));
        assert!(
            content.len() < html.len(),
            "padding must not amplify the input"
        );
    }

    #[test]
    fn nested_layout_tables_are_rendered_once_each() {
        let depth = 12;
        let html = format!(
            "<main>{}<p>Nested content</p>{}</main>",
            "<table><tr><td>".repeat(depth),
            "</td></tr></table>".repeat(depth),
        );
        let document = Html::parse_document(&html);
        let root = document
            .select(&selector("main").expect("selector"))
            .next()
            .expect("main");
        let render = Render {
            base: "https://example.com/".into(),
            table_visits: std::cell::Cell::new(0),
        };
        let mut output = String::new();
        render_element(root, &mut output, 0, &render);
        assert_eq!(cleanup_markdown(&output), "Nested content");
        assert_eq!(render.table_visits.get(), depth);
    }

    #[test]
    fn article_headers_stay_while_page_chrome_is_dropped() {
        let article = r#"<html><body><header>Site menu</header>
            <article><header><h1>Release notes</h1><p>By Ana</p></header><p>Body text.</p></article>
            <footer>Copyright</footer></body></html>"#;
        let (_, content) = extract_html(article.into(), "https://example.com/").expect("content");
        assert!(content.contains("# Release notes"), "{content}");
        assert!(content.contains("By Ana"));
        assert!(!content.contains("Site menu"));

        let title_bar = r#"<html><body><main><header><ul><li><a href="/fr">Français</a></li></ul></header>
            <div role="navigation">Jump to</div><p>Main text.</p></main></body></html>"#;
        let (_, content) = extract_html(title_bar.into(), "https://example.com/").expect("content");
        assert!(content.contains("Main text."));
        assert!(!content.contains("Français"), "{content}");
        assert!(!content.contains("Jump to"));

        let plain = r#"<html><body><header>Site menu</header><p>Only body text.</p>
            <footer>Copyright</footer></body></html>"#;
        let (_, content) = extract_html(plain.into(), "https://example.com/").expect("content");
        assert!(content.contains("Only body text."));
        assert!(!content.contains("Site menu"));
        assert!(!content.contains("Copyright"));
    }
}
