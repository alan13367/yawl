//! Background full-text search over transcript entries.

use std::sync::{Arc, Mutex, mpsc};

#[derive(Default)]
struct Snapshot {
    generation: u64,
    matches: Vec<usize>,
}

struct Request {
    generation: u64,
    query: String,
}

/// Search state lives only while the search bar is open. The worker owns an
/// immutable lowercase corpus and coalesces bursts of query edits before it
/// scans, keeping input handling independent of transcript length.
pub(super) struct TranscriptSearch {
    query: String,
    generation: u64,
    applied_generation: u64,
    matches: Vec<usize>,
    current: usize,
    tx: mpsc::Sender<Request>,
    snapshot: Arc<Mutex<Snapshot>>,
}

impl TranscriptSearch {
    pub(super) fn open(corpus: Vec<(usize, String)>) -> Self {
        let corpus = corpus
            .into_iter()
            .map(|(index, text)| (index, text.to_lowercase()))
            .collect::<Vec<_>>();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let output = Arc::clone(&snapshot);
        let (tx, rx) = mpsc::channel::<Request>();
        let _ = std::thread::Builder::new()
            .name("yawl-transcript-search".into())
            .spawn(move || {
                while let Ok(mut request) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        request = newer;
                    }
                    let query = request.query.to_lowercase();
                    let matches = if query.is_empty() {
                        Vec::new()
                    } else {
                        corpus
                            .iter()
                            .filter_map(|(index, text)| text.contains(&query).then_some(*index))
                            .collect()
                    };
                    if let Ok(mut snapshot) = output.lock() {
                        *snapshot = Snapshot {
                            generation: request.generation,
                            matches,
                        };
                    }
                }
            });
        Self {
            query: String::new(),
            generation: 0,
            applied_generation: 0,
            matches: Vec::new(),
            current: 0,
            tx,
            snapshot,
        }
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn push(&mut self, character: char) {
        if !character.is_control() {
            self.query.push(character);
            self.request();
        }
    }

    pub(super) fn paste(&mut self, text: &str) {
        let old_len = self.query.len();
        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        if self.query.len() != old_len {
            self.request();
        }
    }

    pub(super) fn backspace(&mut self) {
        if self.query.pop().is_some() {
            self.request();
        }
    }

    fn request(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.matches.clear();
        self.current = 0;
        let _ = self.tx.send(Request {
            generation: self.generation,
            query: self.query.clone(),
        });
    }

    /// Picks up a completed scan. Returns the newly selected entry when the
    /// visible result set changed.
    pub(super) fn poll(&mut self) -> Option<Option<usize>> {
        let snapshot = self.snapshot.lock().ok()?;
        if snapshot.generation <= self.applied_generation || snapshot.generation != self.generation
        {
            return None;
        }
        self.applied_generation = snapshot.generation;
        self.matches.clone_from(&snapshot.matches);
        self.current = 0;
        Some(self.selected())
    }

    pub(super) fn next(&mut self, reverse: bool) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        self.current = if reverse {
            if self.current == 0 {
                self.matches.len() - 1
            } else {
                self.current - 1
            }
        } else {
            (self.current + 1) % self.matches.len()
        };
        self.selected()
    }

    pub(super) fn selected(&self) -> Option<usize> {
        self.matches.get(self.current).copied()
    }

    pub(super) fn position(&self) -> Option<(usize, usize)> {
        (!self.matches.is_empty()).then_some((self.current + 1, self.matches.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_search_is_case_insensitive_and_wraps() {
        let mut search = TranscriptSearch::open(vec![
            (0, "First message".into()),
            (1, "another FIRST".into()),
            (2, "unrelated".into()),
        ]);
        for character in "first".chars() {
            search.push(character);
        }
        for _ in 0..100 {
            if let Some(selected) = search.poll() {
                assert_eq!(selected, Some(0));
                assert_eq!(search.position(), Some((1, 2)));
                assert_eq!(search.next(false), Some(1));
                assert_eq!(search.next(false), Some(0));
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("search worker did not publish a result");
    }
}
