//! Short-lived memory of search answers, shared by the main agent and its
//! subagents. Models often repeat a query word for word, and answering from
//! here spends no API credit and adds nothing to DuckDuckGo's request count.
//! Nothing is written to disk.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::{Freshness, SearchResult};
use crate::config::WebSearchProvider;

const TTL: Duration = Duration::from_secs(10 * 60);
const CAPACITY: usize = 32;

static ENTRIES: Mutex<Entries> = Mutex::new(Entries(Vec::new()));

/// `scope` stands for the search settings, so changing the provider, its address
/// or keys, or fallback takes effect immediately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Key {
    scope: u64,
    query: String,
    freshness: Option<Freshness>,
}

impl Key {
    pub(super) fn new(scope: u64, query: &str, freshness: Option<Freshness>) -> Self {
        Key {
            scope,
            query: query
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase(),
            freshness,
        }
    }
}

struct Entry {
    key: Key,
    stored: Instant,
    answered_by: WebSearchProvider,
    results: Vec<SearchResult>,
}

struct Entries(Vec<Entry>);

impl Entries {
    fn get(&self, key: &Key, now: Instant) -> Option<(WebSearchProvider, Vec<SearchResult>)> {
        self.0
            .iter()
            .find(|entry| entry.key == *key && now.duration_since(entry.stored) < TTL)
            .map(|entry| (entry.answered_by, entry.results.clone()))
    }

    fn put(&mut self, entry: Entry) {
        self.0.retain(|existing| existing.key != entry.key);
        if self.0.len() >= CAPACITY {
            self.0.remove(0);
        }
        self.0.push(entry);
    }
}

pub(super) fn get(key: &Key) -> Option<(WebSearchProvider, Vec<SearchResult>)> {
    ENTRIES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(key, Instant::now())
}

pub(super) fn put(key: Key, answered_by: WebSearchProvider, results: Vec<SearchResult>) {
    ENTRIES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .put(Entry {
            key,
            stored: Instant::now(),
            answered_by,
            results,
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(query: &str, stored: Instant) -> Entry {
        Entry {
            key: Key::new(1, query, None),
            stored,
            answered_by: WebSearchProvider::DuckDuckGo,
            results: Vec::new(),
        }
    }

    #[test]
    fn keys_ignore_case_and_spacing_but_not_filters_or_settings() {
        let base = Key::new(1, "Rust  Borrow", None);
        assert_eq!(base, Key::new(1, "rust borrow", None));
        assert_ne!(base, Key::new(1, "rust borrow", Some(Freshness::Week)));
        assert_ne!(base, Key::new(2, "rust borrow", None));
    }

    #[test]
    fn entries_expire_and_the_oldest_is_evicted_at_capacity() {
        let now = Instant::now();
        let mut entries = Entries(Vec::new());
        entries.put(entry("first", now));
        let key = Key::new(1, "first", None);
        assert!(
            entries
                .get(&key, now + TTL - Duration::from_secs(1))
                .is_some()
        );
        assert!(entries.get(&key, now + TTL).is_none());

        for index in 0..CAPACITY {
            entries.put(entry(&format!("query {index}"), now));
        }
        assert_eq!(entries.0.len(), CAPACITY);
        assert!(entries.get(&key, now).is_none());
    }

    #[test]
    fn storing_a_key_again_replaces_it() {
        let now = Instant::now();
        let mut entries = Entries(Vec::new());
        entries.put(entry("same", now));
        entries.put(entry("same", now));
        assert_eq!(entries.0.len(), 1);
    }
}
