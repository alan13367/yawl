//! Focused tests for the corresponding TUI responsibility.

use super::*;

#[test]
fn screen_selection_extracts_styled_text_in_either_direction() {
    let selection = TextSelection {
        anchor: ScreenPoint { row: 1, column: 5 },
        current: ScreenPoint { row: 0, column: 6 },
        frame: vec![
            "\x1b[31mhello world\x1b[0m   ".into(),
            "second line   ".into(),
        ],
    };

    assert_eq!(selected_text(&selection), "world\nsecond");
    let highlighted = highlighted_selection(&selection);
    assert_eq!(markdown::strip_ansi(&highlighted[0]), "hello world   ");
    assert!(highlighted[0].contains("\x1b[7m"));
}

#[test]
fn clipboard_payload_uses_standard_base64() {
    assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    assert_eq!(base64_encode("copy me".as_bytes()), "Y29weSBtZQ==");
}
