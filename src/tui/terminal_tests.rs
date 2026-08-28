//! Focused tests for the corresponding TUI responsibility.

use super::render::FrameImage;
use super::terminal::{ImageProtocol, write_inline_images};
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

#[test]
fn views_without_an_editor_keep_the_terminal_cursor_hidden() {
    assert_eq!(cursor_control(HIDDEN_CURSOR, false), "\x1b[?25l");
    assert_eq!(cursor_control((4, 7), true), "\x1b[?25l");
    assert_eq!(cursor_control((4, 7), false), "\x1b[4;7H\x1b[?25h");
}

#[test]
fn terminal_environment_selects_an_inline_image_protocol() {
    assert_eq!(
        ImageProtocol::from_terminal(Some("Ghostty"), false, Some("xterm-256color")),
        ImageProtocol::Kitty
    );
    assert_eq!(
        ImageProtocol::from_terminal(Some("iTerm.app"), false, Some("xterm-256color")),
        ImageProtocol::Iterm2
    );
    assert_eq!(
        ImageProtocol::from_terminal(Some("Apple_Terminal"), false, Some("xterm-256color")),
        ImageProtocol::None
    );
}

#[test]
fn kitty_images_are_chunked_and_do_not_move_the_cursor() {
    let image = FrameImage {
        key: 1,
        row: 3,
        column: 2,
        columns: 40,
        rows: 8,
        content: std::sync::Arc::new(crate::provider::ImageContent {
            media_type: "image/png".into(),
            data: "a".repeat(5000),
        }),
    };
    let mut output = Vec::new();

    write_inline_images(&mut output, ImageProtocol::Kitty, &[image]).expect("render image");

    let output = String::from_utf8(output).expect("escape output");
    assert!(output.starts_with("\x1b[3;2H\x1b_Ga=T,f=100,t=d,q=2,C=1,i=1,c=40,r=8,m=1;"));
    assert!(output.contains("\x1b\\\x1b_Gm=0;"));
    assert_eq!(output.matches("\x1b_G").count(), 2);
}
