//! Path helpers that hide platform differences: ordinary-looking canonical
//! paths, Windows reserved device names, and file identity.

#[cfg(windows)]
mod windows;

use std::path::{Path, PathBuf};

#[cfg(windows)]
pub(crate) use windows::file_info;

/// Resolves symlinks and relative components like [`std::fs::canonicalize`].
///
/// On Windows the standard library answers with verbatim paths such as
/// `\\?\C:\work` or `\\?\UNC\server\share`. Those would leak into displays,
/// trust keys, session directory names, prompts, and child working
/// directories, so the prefix is dropped whenever the ordinary spelling names
/// the same file.
///
/// # Errors
///
/// Returns the error from the underlying canonicalization.
pub(crate) fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    if let Some(simple) = canonical.to_str().and_then(simplify_verbatim) {
        return Ok(PathBuf::from(simple));
    }
    Ok(canonical)
}

/// Renames `from` over `to`, replacing an existing file.
///
/// Windows refuses the replacement while another program holds `to` open
/// without sharing deletes, which the system reports only as "access is
/// denied" or "used by another process"; the error says what to look for.
///
/// # Errors
///
/// Returns the error from [`std::fs::rename`].
pub(crate) fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    let renamed = std::fs::rename(from, to);
    #[cfg(windows)]
    let renamed = renamed.map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "cannot replace {}: {error} (the file may be read-only or open in another program)",
                to.display()
            ),
        )
    });
    renamed
}

/// Longest ordinary path, in bytes, that still avoids the `MAX_PATH` limit.
/// Counting UTF-8 bytes never undercounts the UTF-16 units Windows limits.
#[cfg(any(windows, test))]
const MAX_ORDINARY_PATH: usize = 260;

/// Rewrites `\\?\C:\dir` as `C:\dir` and `\\?\UNC\server\share\dir` as
/// `\\server\share\dir`. Returns `None` when the path is not verbatim or when
/// the ordinary spelling could name something else: components that are
/// reserved device names, end in a dot or space, or hold characters invalid
/// in ordinary paths, and results that reach `MAX_PATH`. The verbatim form is
/// kept in those cases because it is the only exact one.
#[cfg(any(windows, test))]
fn simplify_verbatim(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\?\")?;
    let (simple, body) = if let Some(unc) = strip_unc(rest) {
        (format!(r"\\{unc}"), unc)
    } else {
        let bytes = rest.as_bytes();
        if bytes.len() < 3
            || !bytes[0].is_ascii_alphabetic()
            || bytes[1] != b':'
            || bytes[2] != b'\\'
        {
            return None;
        }
        (rest.to_owned(), &rest[3..])
    };
    // A drive root (`C:\`) has an empty body; every other component must be
    // an ordinary, non-empty name.
    if !body.is_empty() && !body.split('\\').all(ordinary_component) {
        return None;
    }
    // A UNC path needs both a server and a share.
    if let Some(unc) = simple.strip_prefix(r"\\")
        && unc.split('\\').count() < 2
    {
        return None;
    }
    (simple.len() < MAX_ORDINARY_PATH).then_some(simple)
}

/// The text after `UNC\`, matched case-insensitively as Windows does.
#[cfg(any(windows, test))]
fn strip_unc(rest: &str) -> Option<&str> {
    let prefix = rest.get(..4)?;
    prefix.eq_ignore_ascii_case(r"UNC\").then(|| &rest[4..])
}

/// Whether Win32 would open `name` as a plain directory entry: no characters
/// invalid in file names, no trailing dot or space, and no device name.
#[cfg(any(windows, test))]
fn ordinary_component(name: &str) -> bool {
    !name.is_empty()
        && !name.ends_with(['.', ' '])
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '|' | '?' | '*'))
        && !is_reserved_device_name(name)
}

/// Whether Windows treats `name` as a legacy DOS device instead of a file,
/// with or without an extension (`NUL`, `con.txt`, `COM1`, `LPT9 .log`).
#[cfg(any(windows, test))]
pub(crate) fn is_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    const BASIC: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];
    if BASIC.iter().any(|device| stem.eq_ignore_ascii_case(device)) {
        return true;
    }
    // COM and LPT followed by one digit, including the superscript digits.
    let mut chars = stem.chars();
    let (Some(a), Some(b), Some(c), Some(digit), None) = (
        chars.next(),
        chars.next(),
        chars.next(),
        chars.next(),
        chars.next(),
    ) else {
        return false;
    };
    let family = [a, b, c].iter().collect::<String>();
    (family.eq_ignore_ascii_case("COM") || family.eq_ignore_ascii_case("LPT"))
        && (digit.is_ascii_digit() || matches!(digit, '\u{b9}' | '\u{b2}' | '\u{b3}'))
}

/// Whether Win32 would open `path` as a device rather than a file: a `\\.\`
/// device path, or any component that is a reserved device name. Verbatim
/// `\\?\` paths skip that translation and name real files.
#[cfg(any(windows, test))]
pub(crate) fn names_device(path: &Path) -> bool {
    let text = path.to_string_lossy();
    if text.starts_with(r"\\?\") {
        return false;
    }
    if text.starts_with(r"\\.\") || text.starts_with("//./") {
        return true;
    }
    text.split(['\\', '/']).any(is_reserved_device_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_verbatim_prefixes_from_plain_paths() {
        assert_eq!(
            simplify_verbatim(r"\\?\C:\Users\alan\proj").as_deref(),
            Some(r"C:\Users\alan\proj")
        );
        assert_eq!(simplify_verbatim(r"\\?\d:\").as_deref(), Some(r"d:\"));
        assert_eq!(
            simplify_verbatim(r"\\?\UNC\server\share\dir").as_deref(),
            Some(r"\\server\share\dir")
        );
        assert_eq!(
            simplify_verbatim(r"\\?\unc\server\share").as_deref(),
            Some(r"\\server\share")
        );
    }

    #[test]
    fn keeps_paths_that_are_not_verbatim_or_cannot_be_spelled_plainly() {
        for path in [
            r"C:\plain",
            "/unix/path",
            r"\\server\share",
            r"\\?\C:",
            r"\\?\C:dir",
            r"\\?\Volume{1234}\dir",
            r"\\?\GLOBALROOT\Device\x",
            r"\\?\UNC\server",
            r"\\?\UNC\\share",
            r"\\?\C:\dir\",
            r"\\?\C:\a\\b",
            r"\\?\C:\a\NUL",
            r"\\?\C:\a\com1.txt",
            r"\\?\C:\a\trailing.",
            r"\\?\C:\a\trailing ",
            r"\\?\C:\a\what?",
            r"\\?\C:\a\stream:name",
            r"\\?\C:\a\..",
        ] {
            assert_eq!(simplify_verbatim(path), None, "{path}");
        }
        let long = format!(r"\\?\C:\{}", "a".repeat(260));
        assert_eq!(simplify_verbatim(&long), None);
        let fits = format!(r"\\?\C:\{}", "a".repeat(250));
        assert!(simplify_verbatim(&fits).is_some());
    }

    #[test]
    fn recognizes_reserved_device_names() {
        for name in [
            "CON",
            "con",
            "NUL",
            "aux.txt",
            "Prn",
            "COM1",
            "lpt9.log",
            "COM\u{b9}",
            "NUL .txt",
            "CONIN$",
            "com0",
        ] {
            assert!(is_reserved_device_name(name), "{name}");
        }
        for name in ["console", "COM10", "COM", "null", "LPT", "a.con", ""] {
            assert!(!is_reserved_device_name(name), "{name}");
        }
    }

    #[test]
    fn spots_paths_that_open_devices() {
        for path in [
            "CON",
            r"C:\work\nul",
            "C:/work/aux.txt",
            r"dir\COM3\file",
            r"\\.\pipe\name",
            "//./COM1",
        ] {
            assert!(names_device(Path::new(path)), "{path}");
        }
        for path in [
            r"C:\work\console.txt",
            "src/main.rs",
            "/dev/null",
            r"\\?\C:\work\nul",
            r"\\server\share\file",
        ] {
            assert!(!names_device(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn canonicalize_resolves_existing_paths_and_reports_missing_ones() {
        let directory = crate::test_support::unique_temp_dir("paths");
        std::fs::create_dir_all(&directory).unwrap();
        let canonical = canonicalize(&directory).unwrap();
        assert!(canonical.is_absolute());
        assert!(!canonical.to_string_lossy().starts_with(r"\\?\"));
        #[cfg(unix)]
        assert_eq!(canonical, std::fs::canonicalize(&directory).unwrap());
        assert!(canonicalize(&directory.join("missing")).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
