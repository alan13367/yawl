//! Dark and light variants of the fixed interface colors.
//!
//! The theme is chosen when the terminal opens, from the `theme` setting or
//! the background color the terminal reports, and read wherever a fixed
//! color is painted. Accent colors come from the user, so on light
//! backgrounds [`ink`] darkens them just enough to stay readable.

use std::cell::RefCell;
#[cfg(not(test))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::config::{Theme, UiColor};

/// Truecolor escapes for one background. Truecolor reads the same in every
/// terminal palette; basic ANSI blue, for example, renders near-black on
/// many dark themes.
pub(super) struct Palette {
    /// Cool panel behind user prompts.
    pub(super) user_background: &'static str,
    pub(super) user_text: &'static str,
    /// Tool-card backgrounds by outcome or kind.
    pub(super) card_success: &'static str,
    pub(super) card_error: &'static str,
    pub(super) card_skill: &'static str,
    pub(super) card_toolsmith: &'static str,
    pub(super) card_question: &'static str,
    pub(super) card_running: &'static str,
    /// Tool-card text tones.
    pub(super) tool_header: &'static str,
    pub(super) tool_output: &'static str,
    pub(super) tool_muted: &'static str,
    pub(super) tool_added: &'static str,
    pub(super) tool_removed: &'static str,
    pub(super) tool_error: &'static str,
    pub(super) inline_code: &'static str,
    /// Warm gray behind fenced code, distinct from the user panel and the
    /// tool cards. Background escapes never survive `strip_ansi`, so
    /// drag-select copies stay clean.
    pub(super) code_background: &'static str,
    pub(super) heading_major: &'static str,
    pub(super) heading_minor: &'static str,
    /// List bullets, quote bars, and other Markdown structure.
    pub(super) marker: &'static str,
    /// Rules, table borders, and quoted text.
    pub(super) muted: &'static str,
    /// Syntax colors, tuned for `code_background`.
    pub(super) keyword: &'static str,
    pub(super) string: &'static str,
    pub(super) number: &'static str,
    pub(super) comment: &'static str,
    pub(super) literal: &'static str,
    /// Full-row Git diff tints with their text and `+`/`-` prefixes.
    pub(super) diff_add_background: &'static str,
    pub(super) diff_add_text: &'static str,
    pub(super) diff_add_prefix: &'static str,
    pub(super) diff_del_background: &'static str,
    pub(super) diff_del_text: &'static str,
    pub(super) diff_del_prefix: &'static str,
    /// Git dashboard stage and unstage actions, independent of the accent,
    /// which is also the default selection background.
    pub(super) stage_action: &'static str,
    pub(super) unstage_action: &'static str,
    /// Validation errors in picker fields.
    pub(super) field_error: &'static str,
}

const DARK: Palette = Palette {
    user_background: "\x1b[48;2;52;53;64m",
    user_text: "\x1b[38;2;208;208;214m",
    card_success: "\x1b[48;2;42;50;41m",
    card_error: "\x1b[48;2;50;42;42m",
    card_skill: "\x1b[48;2;54;44;82m",
    card_toolsmith: "\x1b[48;2;28;52;60m",
    card_question: "\x1b[48;2;40;44;52m",
    card_running: "\x1b[48;5;58m",
    tool_header: "\x1b[1;97m",
    tool_output: "\x1b[38;5;245m",
    tool_muted: "\x1b[2;37m",
    tool_added: "\x1b[38;5;114m",
    tool_removed: "\x1b[38;5;203m",
    tool_error: "\x1b[38;5;210m",
    inline_code: "\x1b[38;2;155;188;198m",
    code_background: "\x1b[48;2;36;34;30m",
    heading_major: "\x1b[1;38;2;137;180;250m",
    heading_minor: "\x1b[1;38;2;180;205;250m",
    marker: "\x1b[38;2;116;199;213m",
    muted: "\x1b[38;2;140;146;160m",
    keyword: "\x1b[1;38;2;198;160;246m",
    string: "\x1b[38;2;166;209;137m",
    number: "\x1b[38;2;239;159;118m",
    comment: "\x1b[3;38;2;140;146;160m",
    literal: "\x1b[38;2;133;193;220m",
    diff_add_background: "\x1b[48;2;30;64;39m",
    diff_add_text: "\x1b[38;2;215;236;217m",
    diff_add_prefix: "\x1b[1;38;5;114m",
    diff_del_background: "\x1b[48;2;68;34;34m",
    diff_del_text: "\x1b[38;2;242;216;216m",
    diff_del_prefix: "\x1b[1;38;5;203m",
    stage_action: "\x1b[38;2;139;213;162m",
    unstage_action: "\x1b[38;2;232;202;118m",
    field_error: "\x1b[38;5;210m",
};

const LIGHT: Palette = Palette {
    user_background: "\x1b[48;2;230;232;240m",
    user_text: "\x1b[38;2;36;38;48m",
    card_success: "\x1b[48;2;228;241;226m",
    card_error: "\x1b[48;2;250;229;229m",
    card_skill: "\x1b[48;2;236;230;250m",
    card_toolsmith: "\x1b[48;2;224;239;244m",
    card_question: "\x1b[48;2;231;234;241m",
    card_running: "\x1b[48;2;246;240;212m",
    tool_header: "\x1b[1;38;2;28;28;32m",
    tool_output: "\x1b[38;2;88;88;92m",
    tool_muted: "\x1b[38;2;112;112;118m",
    tool_added: "\x1b[38;2;24;120;48m",
    tool_removed: "\x1b[38;2;184;36;36m",
    tool_error: "\x1b[38;2;168;32;32m",
    inline_code: "\x1b[38;2;16;104;128m",
    code_background: "\x1b[48;2;243;240;233m",
    heading_major: "\x1b[1;38;2;28;84;188m",
    heading_minor: "\x1b[1;38;2;52;100;170m",
    marker: "\x1b[38;2;0;118;140m",
    muted: "\x1b[38;2;96;102;116m",
    keyword: "\x1b[1;38;2;124;58;196m",
    string: "\x1b[38;2;44;114;28m",
    number: "\x1b[38;2;170;80;14m",
    comment: "\x1b[3;38;2;98;104;116m",
    literal: "\x1b[38;2;16;104;150m",
    diff_add_background: "\x1b[48;2;218;242;222m",
    diff_add_text: "\x1b[38;2;20;72;32m",
    diff_add_prefix: "\x1b[1;38;2;24;120;48m",
    diff_del_background: "\x1b[48;2;252;225;225m",
    diff_del_text: "\x1b[38;2;112;24;24m",
    diff_del_prefix: "\x1b[1;38;2;184;36;36m",
    stage_action: "\x1b[38;2;24;120;60m",
    unstage_action: "\x1b[38;2;150;100;0m",
    field_error: "\x1b[38;2;168;32;32m",
};

#[cfg(not(test))]
static LIGHT_ACTIVE: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    /// Tests switch themes per thread so parallel tests keep the dark default.
    static TEST_LIGHT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What the terminal reported at startup: 0 unknown, 1 dark, 2 light.
static DETECTED: AtomicU8 = AtomicU8::new(0);

pub(super) fn is_light() -> bool {
    #[cfg(test)]
    return TEST_LIGHT.with(std::cell::Cell::get);
    #[cfg(not(test))]
    LIGHT_ACTIVE.load(Ordering::Relaxed)
}

fn set_light(light: bool) {
    #[cfg(test)]
    TEST_LIGHT.with(|cell| cell.set(light));
    #[cfg(not(test))]
    LIGHT_ACTIVE.store(light, Ordering::Relaxed);
}

#[cfg(test)]
pub(super) fn use_light(light: bool) {
    set_light(light);
}

pub(super) fn palette() -> &'static Palette {
    if is_light() { &LIGHT } else { &DARK }
}

/// Records the background the terminal reported and applies `setting`.
pub(super) fn init(setting: Theme, detected_light: Option<bool>) {
    let code = match detected_light {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    DETECTED.store(code, Ordering::Relaxed);
    apply(setting);
}

/// Applies a theme setting; `auto` reuses the startup detection.
pub(super) fn apply(setting: Theme) {
    let detected = match DETECTED.load(Ordering::Relaxed) {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    };
    let colorfgbg = std::env::var("COLORFGBG").ok();
    set_light(resolve(setting, detected, colorfgbg.as_deref()));
}

/// `auto` trusts the terminal's reported color first, then `COLORFGBG`,
/// and otherwise assumes the more common dark background.
fn resolve(setting: Theme, detected_light: Option<bool>, colorfgbg: Option<&str>) -> bool {
    match setting {
        Theme::Dark => false,
        Theme::Light => true,
        Theme::Auto => detected_light
            .or_else(|| colorfgbg.and_then(colorfgbg_is_light))
            .unwrap_or(false),
    }
}

/// `COLORFGBG` is `fg;bg` (or `fg;default;bg`) in the 16-color palette, where
/// backgrounds 7 and 9–15 are light.
fn colorfgbg_is_light(value: &str) -> Option<bool> {
    let background = value.rsplit(';').next()?.trim().parse::<u8>().ok()?;
    Some(matches!(background, 7 | 9..=15))
}

/// The background-color query (OSC 11) followed by a primary device
/// attributes request, which every terminal answers, so a reader can stop
/// at that answer instead of waiting out a timeout.
pub(super) const BACKGROUND_QUERY: &[u8] = b"\x1b]11;?\x1b\\\x1b[c";

/// Whether `reply` already holds the device-attributes answer
/// (`ESC [ ? digits/semicolons c`) that ends a [`BACKGROUND_QUERY`] reply.
pub(super) fn has_device_attributes(reply: &[u8]) -> bool {
    reply.windows(3).enumerate().any(|(start, window)| {
        window == b"\x1b[?"
            && reply[start + 3..]
                .iter()
                .find(|byte| !byte.is_ascii_digit() && **byte != b';')
                == Some(&b'c')
    })
}

/// Whether the terminal's OSC 11 reply (`ESC ] 11 ; rgb:R/G/B`, with one to
/// four hex digits per channel) names a light background.
pub(super) fn reported_background_is_light(reply: &[u8]) -> Option<bool> {
    let text = String::from_utf8_lossy(reply);
    let body = &text[text.find("]11;")? + 4..];
    let body = &body[..body.find(['\x07', '\x1b']).unwrap_or(body.len())];
    let rgb = body
        .strip_prefix("rgb:")
        .or_else(|| body.strip_prefix("rgba:"))?;
    let mut channels = rgb.split('/').map(|hex| {
        if hex.is_empty() || hex.len() > 4 {
            return None;
        }
        let value = u32::from_str_radix(hex, 16).ok()?;
        let max = (1_u32 << (4 * hex.len())) - 1;
        Some(f64::from(value) / f64::from(max))
    });
    let (red, green, blue) = (channels.next()??, channels.next()??, channels.next()??);
    Some(0.299 * red + 0.587 * green + 0.114 * blue > 0.5)
}

/// A foreground color readable on the terminal background: unchanged on a
/// dark background, darkened on a light one until it has 3:1 contrast with
/// white, the minimum for interface text and glyphs.
pub(super) fn ink(color: UiColor) -> UiColor {
    if !is_light() {
        return color;
    }
    thread_local! {
        /// Recently darkened colors. A frame asks for the same accent and
        /// selection colors many times, and each darkening repeats the
        /// contrast math.
        static INKED: RefCell<Vec<(UiColor, UiColor)>> = const { RefCell::new(Vec::new()) };
    }
    INKED.with_borrow_mut(|inked| {
        if let Some(&(_, darkened)) = inked.iter().find(|(original, _)| *original == color) {
            return darkened;
        }
        let mut darkened = color;
        while contrast(darkened, UiColor::new(255, 255, 255)) < 3.0 {
            let darker = |channel: u8| (f32::from(channel) * 0.85) as u8;
            darkened = UiColor::new(
                darker(darkened.red),
                darker(darkened.green),
                darker(darkened.blue),
            );
        }
        if inked.len() == 8 {
            inked.remove(0);
        }
        inked.push((color, darkened));
        darkened
    })
}

/// WCAG contrast ratio between two colors.
fn contrast(first: UiColor, second: UiColor) -> f64 {
    let (first, second) = (luminance(first), luminance(second));
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

/// WCAG relative luminance.
fn luminance(color: UiColor) -> f64 {
    let linear = |channel: u8| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.red) + 0.7152 * linear(color.green) + 0.0722 * linear(color.blue)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The truecolor RGB in an escape such as `\x1b[1;38;2;1;2;3m`.
    fn rgb(escape: &str, kind: &str) -> UiColor {
        let start = escape
            .find(&format!("{kind};2;"))
            .unwrap_or_else(|| panic!("{escape:?} has no {kind} truecolor"));
        let channels = escape[start + kind.len() + 3..]
            .trim_end_matches('m')
            .split(';')
            .map(|channel| channel.parse::<u8>().expect("channel"))
            .collect::<Vec<_>>();
        UiColor::new(channels[0], channels[1], channels[2])
    }

    #[test]
    fn light_palette_text_is_readable_on_its_backgrounds() {
        let white = UiColor::new(255, 255, 255);
        let background = |escape| rgb(escape, "48");
        let text = |escape| rgb(escape, "38");
        let pairs = [
            (LIGHT.user_text, background(LIGHT.user_background), 4.5),
            (LIGHT.inline_code, white, 4.5),
            (LIGHT.heading_major, white, 4.5),
            (LIGHT.heading_minor, white, 4.5),
            (LIGHT.marker, white, 4.5),
            (LIGHT.muted, white, 4.5),
            (LIGHT.stage_action, white, 4.5),
            (LIGHT.unstage_action, white, 4.5),
            (LIGHT.field_error, white, 4.5),
            (
                LIGHT.diff_add_text,
                background(LIGHT.diff_add_background),
                4.5,
            ),
            (
                LIGHT.diff_add_prefix,
                background(LIGHT.diff_add_background),
                4.5,
            ),
            (
                LIGHT.diff_del_text,
                background(LIGHT.diff_del_background),
                4.5,
            ),
            (
                LIGHT.diff_del_prefix,
                background(LIGHT.diff_del_background),
                4.5,
            ),
        ];
        let code = background(LIGHT.code_background);
        let syntax = [
            LIGHT.keyword,
            LIGHT.string,
            LIGHT.number,
            LIGHT.comment,
            LIGHT.literal,
        ]
        .map(|escape| (escape, code, 4.5));
        let cards = [
            LIGHT.card_success,
            LIGHT.card_error,
            LIGHT.card_skill,
            LIGHT.card_toolsmith,
            LIGHT.card_question,
            LIGHT.card_running,
        ];
        let tones = [
            (LIGHT.tool_header, 4.5),
            (LIGHT.tool_output, 4.5),
            (LIGHT.tool_added, 4.5),
            (LIGHT.tool_removed, 4.5),
            (LIGHT.tool_error, 4.5),
            (LIGHT.tool_muted, 3.0),
        ];
        let card_pairs = cards.iter().flat_map(|card| {
            tones
                .iter()
                .map(move |(tone, minimum)| (*tone, background(card), *minimum))
        });
        for (escape, against, minimum) in pairs.into_iter().chain(syntax).chain(card_pairs) {
            let ratio = contrast(text(escape), against);
            assert!(
                ratio >= minimum,
                "{escape:?} on {against:?} is {ratio:.2}:1"
            );
        }
    }

    #[test]
    fn ink_darkens_pale_accents_only_on_light_backgrounds() {
        let white = UiColor::new(255, 255, 255);
        use_light(false);
        assert_eq!(ink(UiColor::WHITE), UiColor::WHITE);
        use_light(true);
        assert!(contrast(ink(UiColor::WHITE), white) >= 3.0);
        assert_eq!(ink(UiColor::WHITE), ink(UiColor::WHITE), "a cached answer");
        let navy = UiColor::new(20, 30, 120);
        assert_eq!(ink(navy), navy);
        use_light(false);
    }

    #[test]
    fn auto_follows_the_terminal_report_then_colorfgbg() {
        assert!(!resolve(Theme::Dark, Some(true), Some("0;15")));
        assert!(resolve(Theme::Light, Some(false), None));
        assert!(resolve(Theme::Auto, Some(true), Some("15;0")));
        assert!(!resolve(Theme::Auto, Some(false), Some("0;15")));
        assert!(resolve(Theme::Auto, None, Some("0;default;15")));
        assert!(!resolve(Theme::Auto, None, Some("15;0")));
        assert!(!resolve(Theme::Auto, None, Some("garbage")));
        assert!(!resolve(Theme::Auto, None, None));
    }

    #[test]
    fn background_reports_parse_any_channel_width_and_terminator() {
        let light = b"\x1b]11;rgb:ffff/fefe/f0f0\x1b\\\x1b[?62;22c";
        assert_eq!(reported_background_is_light(light), Some(true));
        assert!(has_device_attributes(light));
        let dark = b"\x1b]11;rgb:1e/1e/2e\x07";
        assert_eq!(reported_background_is_light(dark), Some(false));
        assert!(!has_device_attributes(dark));
        assert_eq!(reported_background_is_light(b"\x1b[?1;2c"), None);
        assert!(has_device_attributes(b"\x1b[?1;2c"));
        assert_eq!(
            reported_background_is_light(b"\x1b]11;rgb:zz/00/00\x07"),
            None
        );
        assert!(!has_device_attributes(b"\x1b]11;rgb:cccc/cccc/cccc\x07"));
    }
}
