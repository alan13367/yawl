//! Console key records to UTF-8 bytes. The Windows console delivers typed
//! text as UTF-16 units in key-down records; with virtual-terminal input
//! enabled, arrows, function keys, and pastes arrive as escape sequences in
//! the same units. This module holds the platform-independent translation so
//! it can be tested anywhere.

use std::collections::VecDeque;

/// Held keys can be coalesced into one record with a repeat count. Bounded so
/// a corrupt count cannot queue megabytes.
const MAX_REPEAT: u16 = 256;

/// Accumulates UTF-16 units from key records and hands them out as UTF-8.
#[derive(Default)]
pub(super) struct KeyText {
    /// A high surrogate waiting for its low half, possibly from an earlier
    /// batch of records.
    high: Option<u16>,
    bytes: VecDeque<u8>,
}

impl KeyText {
    /// Adds one key record. Key-up records and records without a character
    /// (modifier keys, for example) are ignored.
    pub(super) fn push(&mut self, key_down: bool, repeat: u16, unit: u16) {
        if !key_down || unit == 0 {
            return;
        }
        for _ in 0..repeat.clamp(1, MAX_REPEAT) {
            self.push_unit(unit);
        }
    }

    fn push_unit(&mut self, unit: u16) {
        let high = self.high.take();
        match unit {
            0xD800..=0xDBFF => {
                if high.is_some() {
                    self.push_char(char::REPLACEMENT_CHARACTER);
                }
                self.high = Some(unit);
            }
            0xDC00..=0xDFFF => {
                let character = high.and_then(|high| {
                    char::from_u32(
                        0x1_0000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00),
                    )
                });
                self.push_char(character.unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            _ => {
                if high.is_some() {
                    self.push_char(char::REPLACEMENT_CHARACTER);
                }
                self.push_char(
                    char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER),
                );
            }
        }
    }

    fn push_char(&mut self, character: char) {
        let mut encoded = [0u8; 4];
        self.bytes
            .extend(character.encode_utf8(&mut encoded).as_bytes());
    }

    /// Whether translated bytes are waiting for a caller with room for them.
    pub(super) fn has_pending(&self) -> bool {
        !self.bytes.is_empty()
    }

    /// Moves as many waiting bytes as fit into `out` and returns the count.
    /// The rest stay queued for the next call.
    pub(super) fn drain(&mut self, out: &mut [u8]) -> usize {
        let count = out.len().min(self.bytes.len());
        for (slot, byte) in out.iter_mut().zip(self.bytes.drain(..count)) {
            *slot = byte;
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(records: &[(bool, u16)]) -> Vec<u8> {
        let mut keys = KeyText::default();
        for &(down, unit) in records {
            keys.push(down, 1, unit);
        }
        let mut out = vec![0u8; 64];
        let count = keys.drain(&mut out);
        out.truncate(count);
        out
    }

    fn units(text: &str) -> Vec<(bool, u16)> {
        text.encode_utf16().map(|unit| (true, unit)).collect()
    }

    #[test]
    fn ascii_and_escape_sequences_pass_through() {
        assert_eq!(text(&units("a\u{1b}[A\r")), b"a\x1b[A\r");
    }

    #[test]
    fn bmp_and_astral_characters_become_utf8() {
        let input = units("é€😀");
        assert_eq!(text(&input), "é€😀".as_bytes());
    }

    #[test]
    fn key_up_and_empty_records_are_ignored() {
        let records = [(true, 0x61), (false, 0x61), (true, 0), (false, 0x62)];
        assert_eq!(text(&records), b"a");
    }

    #[test]
    fn surrogates_pair_across_separate_pushes() {
        let mut keys = KeyText::default();
        let pair: Vec<u16> = "😀".encode_utf16().collect();
        keys.push(true, 1, pair[0]);
        let mut out = [0u8; 8];
        // The high half alone yields nothing yet.
        assert_eq!(keys.drain(&mut out), 0);
        assert!(!keys.has_pending());
        keys.push(true, 1, pair[1]);
        assert_eq!(keys.drain(&mut out), 4);
        assert_eq!(&out[..4], "😀".as_bytes());
    }

    #[test]
    fn key_up_between_surrogate_halves_keeps_the_pair() {
        let pair: Vec<u16> = "😀".encode_utf16().collect();
        let records = [(true, pair[0]), (false, pair[0]), (true, pair[1])];
        assert_eq!(text(&records), "😀".as_bytes());
    }

    #[test]
    fn lone_surrogates_become_replacement_characters() {
        let replacement = "\u{fffd}".as_bytes();
        // Lone low surrogate.
        assert_eq!(text(&[(true, 0xDC00)]), replacement);
        // High surrogate followed by a normal character.
        let mut expected = replacement.to_vec();
        expected.push(b'x');
        assert_eq!(text(&[(true, 0xD800), (true, u16::from(b'x'))]), expected);
        // Two high surrogates in a row: the first is lone.
        let mut keys = KeyText::default();
        keys.push(true, 1, 0xD800);
        keys.push(true, 1, 0xD83D);
        keys.push(true, 1, 0xDE00);
        let mut out = [0u8; 16];
        let count = keys.drain(&mut out);
        let mut expected = replacement.to_vec();
        expected.extend_from_slice("😀".as_bytes());
        assert_eq!(&out[..count], expected);
    }

    #[test]
    fn small_output_buffers_keep_the_rest_queued() {
        let mut keys = KeyText::default();
        for (down, unit) in units("a€b") {
            keys.push(down, 1, unit);
        }
        let mut one = [0u8; 1];
        assert_eq!(keys.drain(&mut one), 1);
        assert_eq!(&one, b"a");
        let mut two = [0u8; 2];
        assert_eq!(keys.drain(&mut two), 2);
        assert_eq!(&two, &"€".as_bytes()[..2]);
        assert!(keys.has_pending());
        let mut rest = [0u8; 8];
        assert_eq!(keys.drain(&mut rest), 2);
        assert_eq!(&rest[..2], &[0xAC, b'b']);
        assert!(!keys.has_pending());
        assert_eq!(keys.drain(&mut rest), 0);
        assert_eq!(keys.drain(&mut []), 0);
    }

    #[test]
    fn repeat_counts_repeat_the_character_within_a_bound() {
        let mut keys = KeyText::default();
        keys.push(true, 3, u16::from(b'z'));
        keys.push(true, 0, u16::from(b'y'));
        keys.push(true, u16::MAX, u16::from(b'x'));
        let mut out = vec![0u8; 1024];
        let count = keys.drain(&mut out);
        assert_eq!(&out[..4], b"zzzy");
        assert_eq!(count, 4 + usize::from(MAX_REPEAT));
    }
}
