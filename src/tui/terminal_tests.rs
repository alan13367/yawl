//! Focused tests for the corresponding TUI responsibility.

use super::render::FrameImage;
use super::terminal::{ImageProtocol, MouseMode, supports_pointer_shapes, write_inline_images};
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

#[test]
fn ring_bell_only_emits_when_unfocused() {
    let mut output = Vec::new();
    Terminal::ring_bell_to(&mut output, true);
    assert!(output.is_empty());

    Terminal::ring_bell_to(&mut output, false);
    assert_eq!(output, b"\x07");
}

#[test]
fn git_mouse_mode_restores_drag_reporting_and_pointer_on_exit() -> std::io::Result<()> {
    let mut mode = MouseMode::new(true);
    let mut output = Vec::new();
    mode.update(&mut output, true, false)?;
    assert_eq!(output, b"\x1b[?1002l\x1b[?1003h");
    output.clear();
    mode.update(&mut output, true, true)?;
    assert_eq!(output, b"\x1b]22;pointer\x1b\\");
    output.clear();
    mode.update(&mut output, true, true)?;
    assert!(
        output.is_empty(),
        "unchanged hover must not resend terminal modes"
    );
    mode.update(&mut output, true, false)?;
    assert_eq!(output, b"\x1b]22;\x1b\\");
    mode.update(&mut output, true, true)?;
    output.clear();
    mode.update(&mut output, false, true)?;
    assert_eq!(output, b"\x1b[?1003l\x1b[?1002h\x1b]22;\x1b\\");
    Ok(())
}

#[test]
fn unknown_terminals_get_hover_tracking_without_cursor_shape_commands() -> std::io::Result<()> {
    for (program, term, kitty, supported) in [
        (Some("Ghostty"), Some("xterm-256color"), false, true),
        (None, Some("xterm-kitty"), false, true),
        (None, Some("screen-256color"), true, true),
        (None, Some("foot-extra"), false, true),
        (Some("Apple_Terminal"), Some("xterm-256color"), false, false),
        (None, Some("xterm-256color"), false, false),
    ] {
        assert_eq!(supports_pointer_shapes(program, term, kitty), supported);
    }
    let mut mode = MouseMode::new(false);
    let mut output = Vec::new();
    mode.update(&mut output, true, true)?;
    mode.update(&mut output, false, false)?;
    assert_eq!(output, b"\x1b[?1002l\x1b[?1003h\x1b[?1003l\x1b[?1002h");
    Ok(())
}
