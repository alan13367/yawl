//! Picks the passages of a page most relevant to a search query, so reading a
//! result costs a few thousand characters instead of the whole page.
//!
//! Pages become chunks at paragraph boundaries, ranked with BM25 and a bonus
//! for matching more of the query's terms, then shown in page order.

use std::collections::{HashMap, HashSet};

use crate::tools::web::truncate_chars;

const CHUNK_CHARS: usize = 700;
const SHORT_CHUNK_CHARS: usize = 160;
const GAP: &str = "[…]";
const K1: f64 = 1.2;
const B: f64 = 0.75;
/// Words a chunk needs of its own to be a passage. Reference pages are full of
/// stubs that match only through their heading, and BM25 favors short text.
const MIN_PASSAGE_WORDS: usize = 6;
/// Chunks scoring below this share of the best one would only pad the budget.
const MIN_RELATIVE_SCORE: f64 = 0.25;
/// Longest line treated as a heading. A heading is repeated with each passage
/// under it, so a page must not be able to make that repetition large.
const MAX_HEADING_CHARS: usize = 200;
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "do", "doe", "for", "from", "how",
    "in", "is", "it", "of", "on", "or", "that", "the", "thi", "to", "was", "were", "what", "when",
    "where", "which", "who", "why", "with",
];

#[derive(Debug)]
struct Chunk {
    heading: Option<String>,
    text: String,
}

impl Chunk {
    fn chars(&self) -> usize {
        self.text.chars().count() + self.heading.as_ref().map_or(0, |h| h.chars().count() + 2)
    }
}

/// Returns at most about `budget` characters of `content`, in page order, with
/// `[…]` where text was skipped. A page matching nothing keeps its opening.
pub(super) fn select(content: &str, query: &str, budget: usize) -> String {
    let chunks = chunks(content);
    let scores = scores(&chunks, &terms(query));
    let cutoff = scores.iter().copied().fold(0.0, f64::max) * MIN_RELATIVE_SCORE;
    let mut ranked: Vec<usize> = (0..chunks.len())
        .filter(|i| scores[*i] > 0.0 && scores[*i] >= cutoff)
        .collect();
    ranked.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]));
    let mut chosen = Vec::new();
    let mut used = 0;
    for index in ranked {
        let size = chunks[index].chars();
        if used + size <= budget {
            used += size;
            chosen.push(index);
        }
    }
    if chosen.is_empty() {
        let mut opening = content.trim().to_string();
        if truncate_chars(&mut opening, budget) {
            opening.push_str(&format!("\n{GAP}"));
        }
        return opening;
    }
    chosen.sort_unstable();
    render(&chunks, &chosen)
}

fn render(chunks: &[Chunk], chosen: &[usize]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let mut last_heading = None;
    let mut previous = None;
    for &index in chosen {
        if previous.map_or(index != 0, |previous: usize| index != previous + 1) {
            parts.push(GAP);
        }
        let chunk = &chunks[index];
        if let Some(heading) = chunk.heading.as_deref()
            && last_heading != Some(heading)
        {
            parts.push(heading);
            last_heading = Some(heading);
        }
        parts.push(&chunk.text);
        previous = Some(index);
    }
    if previous.is_some_and(|previous| previous + 1 != chunks.len()) {
        parts.push(GAP);
    }
    parts.join("\n\n")
}

fn heading_line(line: &str) -> bool {
    let level = line.bytes().take_while(|byte| *byte == b'#').count();
    (1..=6).contains(&level)
        && line[level..].starts_with(' ')
        && line.chars().count() <= MAX_HEADING_CHARS
}

fn chunks(content: &str) -> Vec<Chunk> {
    let mut output = Vec::new();
    let mut heading: Option<String> = None;
    let mut block = String::new();
    let mut in_fence = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && (trimmed.is_empty() || heading_line(trimmed)) {
            flush(&mut output, &mut block, heading.as_deref());
            if !trimmed.is_empty() {
                heading = Some(trimmed.to_string());
            }
            continue;
        }
        block.push_str(line);
        block.push('\n');
    }
    flush(&mut output, &mut block, heading.as_deref());
    output
}

/// Moves the block into chunks, joining short neighbours so a list split by
/// blank lines still ranks as one passage.
fn flush(output: &mut Vec<Chunk>, block: &mut String, heading: Option<&str>) {
    for piece in pieces(block) {
        if let Some(last) = output.last_mut()
            && last.heading.as_deref() == heading
            && last.text.chars().count().min(piece.chars().count()) < SHORT_CHUNK_CHARS
            && last.text.chars().count() + piece.chars().count() < CHUNK_CHARS
        {
            last.text.push_str("\n\n");
            last.text.push_str(piece);
            continue;
        }
        output.push(Chunk {
            heading: heading.map(str::to_string),
            text: piece.to_string(),
        });
    }
    block.clear();
}

fn pieces(block: &str) -> Vec<&str> {
    let mut output = Vec::new();
    let mut rest = block.trim();
    while let Some((limit, _)) = rest.char_indices().nth(CHUNK_CHARS) {
        let window = &rest[..limit];
        let late = |cut: &usize| *cut >= limit / 2;
        let cut = window
            .rfind('\n')
            .filter(late)
            .or_else(|| window.rfind(". ").map(|cut| cut + 1).filter(late))
            .or_else(|| window.rfind(' ').filter(late))
            .unwrap_or(limit);
        output.push(rest[..cut].trim());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        output.push(rest);
    }
    output
}

fn is_cjk(character: char) -> bool {
    matches!(character,
        '\u{3040}'..='\u{30ff}' | '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{ac00}'..='\u{d7af}')
}

/// Lowercase words with a plural `s` removed. Text in scripts written without
/// spaces counts each character as a word.
fn tokens(text: &str) -> Vec<String> {
    fn finish(word: &mut String, output: &mut Vec<String>) {
        if word.is_empty() {
            return;
        }
        if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") {
            word.pop();
        }
        output.push(std::mem::take(word));
    }
    let mut output = Vec::new();
    let mut word = String::new();
    for character in text.chars() {
        if is_cjk(character) {
            finish(&mut word, &mut output);
            output.push(character.to_string());
        } else if character.is_alphanumeric() {
            word.extend(character.to_lowercase());
        } else {
            finish(&mut word, &mut output);
        }
    }
    finish(&mut word, &mut output);
    output
}

fn terms(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let all: Vec<String> = tokens(query)
        .into_iter()
        .filter(|token| seen.insert(token.clone()))
        .collect();
    let useful: Vec<String> = all
        .iter()
        .filter(|token| !STOP_WORDS.contains(&token.as_str()))
        .cloned()
        .collect();
    if useful.is_empty() { all } else { useful }
}

/// Markdown text without link addresses, whose URLs repeat a page's own words
/// without saying anything.
fn without_link_targets(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("](") {
        output.push_str(&rest[..=start]);
        let target = &rest[start + 2..];
        rest = target.find(')').map_or(target, |end| &target[end + 1..]);
    }
    output.push_str(rest);
    output
}

fn scores(chunks: &[Chunk], terms: &[String]) -> Vec<f64> {
    let positions: HashMap<&str, usize> = terms
        .iter()
        .enumerate()
        .map(|(position, term)| (term.as_str(), position))
        .collect();
    // Only each chunk's length and query-term counts are kept, so a large page
    // costs memory in proportion to its chunks, not its words.
    let mut lengths = Vec::with_capacity(chunks.len());
    let mut frequencies = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let mut counts = vec![0_u32; terms.len()];
        let words = tokens(&without_link_targets(&chunk.text));
        if words.len() < MIN_PASSAGE_WORDS {
            lengths.push(0);
            frequencies.push(counts);
            continue;
        }
        let heading = chunk
            .heading
            .as_deref()
            .map(|heading| tokens(&without_link_targets(heading)))
            .unwrap_or_default();
        lengths.push(words.len() + heading.len());
        for word in words.iter().chain(&heading) {
            if let Some(position) = positions.get(word.as_str()) {
                counts[*position] += 1;
            }
        }
        frequencies.push(counts);
    }
    let count = chunks.len().max(1) as f64;
    let average = lengths.iter().sum::<usize>() as f64 / count;
    let weights: Vec<f64> = (0..terms.len())
        .map(|position| {
            let matching = frequencies
                .iter()
                .filter(|counts| counts[position] > 0)
                .count() as f64;
            (1.0 + (count - matching + 0.5) / (matching + 0.5)).ln()
        })
        .collect();
    frequencies
        .iter()
        .zip(&lengths)
        .map(|(counts, length)| {
            let length = *length as f64 / average.max(1.0);
            let mut score = 0.0;
            let mut matched = 0;
            for (frequency, weight) in counts.iter().zip(&weights) {
                if *frequency > 0 {
                    let frequency = f64::from(*frequency);
                    matched += 1;
                    score +=
                        weight * frequency * (K1 + 1.0) / (frequency + K1 * (1.0 - B + B * length));
                }
            }
            score * (1.0 + matched as f64 / terms.len().max(1) as f64)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filler(word: &str, repeat: usize) -> String {
        std::iter::repeat_n(word, repeat)
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn relevant_passages_win_and_keep_page_order_and_their_heading() {
        let page = format!(
            "{}\n\n# Pricing\n\n{}\n\n{}\n\n# Timeouts\n\nThe ureq agent sets a global timeout for every request.\n\n{}",
            filler("intro", 120),
            filler("cost", 120),
            filler("plans", 120),
            filler("closing", 120),
        );
        let excerpt = select(&page, "ureq global timeout", 300);
        assert!(excerpt.contains("# Timeouts"), "{excerpt}");
        assert!(excerpt.contains("sets a global timeout"), "{excerpt}");
        assert!(!excerpt.contains("cost"), "{excerpt}");
        assert!(excerpt.starts_with(GAP), "{excerpt}");
        assert!(excerpt.ends_with(GAP), "{excerpt}");
    }

    #[test]
    fn the_budget_holds_and_passages_matching_more_terms_rank_first() {
        let page = "# A\n\nsqlite sqlite text here and some more words.\n\n# B\n\nsqlite wal checkpoint behaviour is explained here.\n\n# C\n\ncheckpoint checkpoint text here and some more words.";
        let excerpt = select(page, "sqlite wal checkpoint", 60);
        assert!(excerpt.contains("sqlite wal checkpoint"), "{excerpt}");
        assert!(!excerpt.contains("sqlite sqlite"), "{excerpt}");
        assert!(!excerpt.contains("checkpoint checkpoint"), "{excerpt}");
    }

    #[test]
    fn stubs_and_link_addresses_do_not_count_as_matches() {
        let page = "# Struct Timeouts\n\nThe timeouts struct configures the global request budget for an agent.\n\n## impl Freeze for Timeouts\n\n§\n\n## impl Send for [Timeouts](https://docs.rs/struct.Timeouts.html)\n\n[Source](https://docs.rs/timeouts/timeouts.html) §\n\n## Related\n\nSee [the guide](https://docs.rs/timeout/timeout/timeout.html) about other unrelated things in this crate.";
        let excerpt = select(page, "timeout", 400);
        assert!(excerpt.contains("configures the global"), "{excerpt}");
        assert!(!excerpt.contains("Freeze"), "{excerpt}");
        assert!(!excerpt.contains("Source"), "{excerpt}");
        assert!(!excerpt.contains("the guide"), "{excerpt}");
    }

    #[test]
    fn weak_matches_do_not_pad_the_budget() {
        let strong = "sqlite wal checkpoint sqlite wal checkpoint behaviour explained in detail.";
        let weak = "A long aside that happens to mention sqlite once among many other unrelated words and topics, which should not be worth a reader's time when something far better sits right beside it on the same page.";
        let excerpt = select(
            &format!("# A\n\n{strong}\n\n# B\n\n{weak}"),
            "sqlite wal checkpoint",
            2_000,
        );
        assert!(excerpt.contains(strong), "{excerpt}");
        assert!(!excerpt.contains("long aside"), "{excerpt}");
    }

    #[test]
    fn link_targets_are_removed_even_when_malformed() {
        assert_eq!(
            without_link_targets("[a](https://x.test/a) and [b](https://x.test/b)!"),
            "[a] and [b]!"
        );
        assert_eq!(without_link_targets("[a](unclosed"), "[a]unclosed");
        assert_eq!(without_link_targets("plain"), "plain");
    }

    #[test]
    fn a_page_that_matches_nothing_keeps_its_opening() {
        let excerpt = select("First line.\n\nSecond line.", "unrelated", 10);
        assert_eq!(excerpt, format!("First line\n{GAP}"));
    }

    #[test]
    fn plurals_and_case_match_and_stop_words_do_not() {
        assert_eq!(terms("What are the Timeouts?"), ["timeout"]);
        assert_eq!(terms("the"), ["the"]);
        assert_eq!(tokens("Glass Cars"), ["glass", "car"]);
    }

    #[test]
    fn scripts_without_spaces_match_by_character() {
        let excerpt = select(
            &format!(
                "{}\n\n東京の天気は晴れです。\n\n{}",
                filler("a", 400),
                filler("b", 400)
            ),
            "東京",
            200,
        );
        assert!(excerpt.contains("東京の天気"), "{excerpt}");
    }

    #[test]
    fn long_blocks_split_on_character_boundaries_and_code_stays_together() {
        let long = "日本語の文章。".repeat(400);
        let parts = pieces(&long);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|part| part.chars().count() <= CHUNK_CHARS));

        let fenced = chunks("```\nlet a = 1;\n\nlet b = 2;\n```");
        assert_eq!(fenced.len(), 1);
        assert!(fenced[0].text.contains("let a = 1;\n\nlet b = 2;"));
    }

    #[test]
    fn a_long_heading_is_only_text() {
        let long = format!("# {}", filler("a", 150));
        assert!(!heading_line(&long));
        assert!(heading_line("# Short heading"));
        let parsed = chunks(&format!("{long}\n\nBody text."));
        assert!(parsed.iter().all(|chunk| chunk.heading.is_none()));
    }

    #[test]
    fn a_hostile_page_cannot_multiply_memory_through_its_headings() {
        // 2 MB of body under one heading is thousands of chunks. Before the cap
        // each chunk held and retokenized this whole heading.
        let page = format!(
            "# {}\n\n{}",
            filler("a", 10_000),
            filler("word timeout", 170_000)
        );
        let started = std::time::Instant::now();
        let excerpt = select(&page, "timeout", 500);
        assert!(
            excerpt.contains("timeout"),
            "{}",
            &excerpt[..80.min(excerpt.len())]
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    #[test]
    fn empty_pages_and_queries_do_not_panic() {
        assert_eq!(select("", "query", 100), "");
        assert_eq!(select("text", "", 100), "text");
        assert_eq!(select("   \n\n ", "query", 100), "");
    }
}
