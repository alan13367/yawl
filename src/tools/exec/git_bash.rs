//! Finding Git for Windows' bash and passing it a script.
//!
//! Everything here is plain string logic over injected environment values and
//! an existence check, so it is unit-tested on every platform even though only
//! Windows runs it.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Names the executable that overrides discovery.
pub(super) const OVERRIDE_VAR: &str = "YAWL_GIT_BASH";

/// `CreateProcessW` accepts command lines of at most this many UTF-16 units,
/// counting the terminating NUL.
const MAX_COMMAND_LINE_UTF16: usize = 32_767;

const SEPARATORS: [char; 2] = ['\\', '/'];

/// Folders next to `bin` that hold the 64-bit and 32-bit Git tools.
const MINGW_DIRS: [&str; 3] = ["mingw64", "mingw32", "clangarm64"];

/// Where Git for Windows keeps its POSIX tools (`ls`, `grep`, `sed`, ...)
/// relative to the `bin` folder that holds `bash.exe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GitBash {
    pub(super) exe: PathBuf,
    /// `<root>\usr\bin`, which a non-login bash does not always put on
    /// `PATH` when started from a native process.
    pub(super) tools_dir: Option<String>,
}

pub(super) fn missing_message() -> String {
    format!(
        "Git Bash was not found. Yawl runs shell commands with the bash that comes with Git for Windows. \
         Install Git for Windows from https://git-scm.com/download/win, or set {OVERRIDE_VAR} to the full path of bash.exe."
    )
}

/// Resolves bash from the override, then from `git.exe` on `PATH`, then from
/// the standard install folders. A bare `bash` is never used: on Windows that
/// name is usually the WSL launcher.
///
/// `var` reads an environment variable; `exists` tells whether a file exists.
pub(super) fn resolve(
    var: impl Fn(&str) -> Option<OsString>,
    exists: impl Fn(&Path) -> bool,
) -> Result<GitBash, String> {
    let found = |exe: PathBuf| {
        let tools_dir = tools_dir(&exe, &exists);
        GitBash { exe, tools_dir }
    };
    if let Some(configured) = var(OVERRIDE_VAR).filter(|value| !value.is_empty()) {
        let exe = PathBuf::from(configured);
        return if exists(&exe) {
            Ok(found(exe))
        } else {
            Err(format!(
                "{OVERRIDE_VAR} is set to {}, which does not exist. {}",
                exe.display(),
                missing_message()
            ))
        };
    }
    if let Some(path) = var("PATH") {
        for directory in split_path(&path.to_string_lossy()) {
            if !exists(Path::new(&format!("{directory}\\git.exe"))) {
                continue;
            }
            let Some(candidate) = bash_for_git_dir(&directory).map(PathBuf::from) else {
                continue;
            };
            if exists(&candidate) {
                return Ok(found(candidate));
            }
        }
    }
    for (name, rest) in [
        ("ProgramFiles", "Git\\bin\\bash.exe"),
        ("ProgramW6432", "Git\\bin\\bash.exe"),
        ("ProgramFiles(x86)", "Git\\bin\\bash.exe"),
        ("LOCALAPPDATA", "Programs\\Git\\bin\\bash.exe"),
    ] {
        let Some(base) = var(name).filter(|value| !value.is_empty()) else {
            continue;
        };
        let base = base.to_string_lossy();
        let candidate = PathBuf::from(format!("{}\\{rest}", base.trim_end_matches(SEPARATORS)));
        if exists(&candidate) {
            return Ok(found(candidate));
        }
    }
    Err(missing_message())
}

/// Splits a Windows `PATH` on `;`, honoring double quotes around entries.
fn split_path(path: &str) -> Vec<String> {
    path.split(';')
        .map(|entry| entry.trim().trim_matches('"'))
        .filter(|entry| !entry.is_empty())
        .map(|entry| entry.trim_end_matches(SEPARATORS).to_string())
        .collect()
}

fn split_last(path: &str) -> Option<(&str, &str)> {
    path.rsplit_once(SEPARATORS)
        .filter(|(_, name)| !name.is_empty())
}

/// `git.exe` in `<root>\cmd`, `<root>\bin` or `<root>\mingw64\bin` belongs to
/// the Git for Windows install at `<root>`, whose bash is `<root>\bin\bash.exe`.
fn bash_for_git_dir(directory: &str) -> Option<String> {
    let (parent, name) = split_last(directory)?;
    let root = match name.to_ascii_lowercase().as_str() {
        "cmd" => parent,
        "bin" => match split_last(parent) {
            Some((grandparent, mingw))
                if MINGW_DIRS.contains(&mingw.to_ascii_lowercase().as_str()) =>
            {
                grandparent
            }
            _ => parent,
        },
        _ => return None,
    };
    Some(format!(
        "{}\\bin\\bash.exe",
        root.trim_end_matches(SEPARATORS)
    ))
}

/// The folder with the POSIX tools that belong to `bash`: its own folder when
/// that is `...\usr\bin`, else `<root>\usr\bin` when it exists.
fn tools_dir(bash: &Path, exists: &impl Fn(&Path) -> bool) -> Option<String> {
    let bash = bash.to_str()?;
    let (directory, _) = split_last(bash)?;
    let lower = directory.to_ascii_lowercase().replace('/', "\\");
    if lower.ends_with("\\usr\\bin") {
        return Some(directory.to_string());
    }
    let (root, _) = split_last(directory)?;
    let candidate = format!("{root}\\usr\\bin");
    exists(Path::new(&candidate)).then_some(candidate)
}

/// `path` with `tools_dir` first, so Git's `ls`, `find`, and `sort` win over
/// the Windows programs of the same names, as they do in Git Bash itself.
pub(super) fn path_with_tools(tools_dir: Option<&str>, path: &OsStr) -> OsString {
    let Some(tools_dir) = tools_dir else {
        return path.to_owned();
    };
    let mut joined = OsString::from(tools_dir);
    if !path.is_empty() {
        joined.push(";");
        joined.push(path);
    }
    joined
}

/// Quotes `arg` for a Windows command line the way the MSVC runtime and the
/// MSYS2 runtime both parse it, so bash receives the script byte for byte and
/// never glob-expands it: wrapped in quotes, with backslashes doubled before a
/// quote and at the end, and quotes escaped.
pub(super) fn quote_arg(arg: &str) -> String {
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(ch);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

/// Checks that `"<exe>" -c <quoted>` fits in a Windows command line.
pub(super) fn check_command_line(exe: &Path, quoted_script: &str) -> Result<(), String> {
    // `"exe"`, a space, `-c`, a space, the script, and the terminating NUL.
    let length = exe.to_string_lossy().encode_utf16().count()
        + 2
        + 1
        + 2
        + 1
        + quoted_script.encode_utf16().count()
        + 1;
    if length > MAX_COMMAND_LINE_UTF16 {
        return Err(format!(
            "the command is too long for Windows ({length} characters after quoting; the limit is {MAX_COMMAND_LINE_UTF16}). \
             Write the script to a file and run that file instead."
        ));
    }
    Ok(())
}

/// Windows programs accept `/` as a separator, and bash needs it.
pub(super) fn forward_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| OsString::from(*value))
        }
    }

    fn files<'a>(present: &'a [&'a str]) -> impl Fn(&Path) -> bool + 'a {
        let present: HashSet<String> = present.iter().map(|path| path.to_lowercase()).collect();
        move |path| present.contains(&path.to_string_lossy().to_lowercase())
    }

    #[test]
    fn quoting_wraps_and_escapes_for_the_msvc_runtime() {
        assert_eq!(quote_arg(""), r#""""#);
        assert_eq!(quote_arg("ls*"), r#""ls*""#);
        assert_eq!(quote_arg("echo hi there"), r#""echo hi there""#);
        assert_eq!(quote_arg(r#"echo "hi""#), r#""echo \"hi\"""#);
        assert_eq!(quote_arg(r"C:\dir\file"), r#""C:\dir\file""#);
        assert_eq!(quote_arg(r"ends with\"), r#""ends with\\""#);
        assert_eq!(quote_arg(r"\\server\"), r#""\\server\\""#);
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quote_arg(r#"a\\"b"#), r#""a\\\\\"b""#);
        assert_eq!(quote_arg("one\ntwo"), "\"one\ntwo\"");
        assert_eq!(quote_arg("'single' $HOME"), "\"'single' $HOME\"");
    }

    /// The inverse of the quoting rules (`CommandLineToArgvW` for one argument).
    fn unquote(quoted: &str) -> String {
        let mut chars = quoted
            .strip_prefix('"')
            .expect("opening quote")
            .chars()
            .peekable();
        let mut out = String::new();
        loop {
            match chars.next().expect("closing quote") {
                '"' => {
                    assert!(chars.next().is_none(), "text after the closing quote");
                    return out;
                }
                '\\' => {
                    let mut count = 1;
                    while chars.peek() == Some(&'\\') {
                        chars.next();
                        count += 1;
                    }
                    if chars.peek() == Some(&'"') {
                        out.extend(std::iter::repeat_n('\\', count / 2));
                        if count % 2 == 1 {
                            chars.next();
                            out.push('"');
                        }
                    } else {
                        out.extend(std::iter::repeat_n('\\', count));
                    }
                }
                ch => out.push(ch),
            }
        }
    }

    #[test]
    fn quoting_round_trips_awkward_scripts() {
        for script in [
            "",
            "ls*",
            r#"printf '%s\n' "a b" \"#,
            r"\\",
            r#"\"\"\\"#,
            "echo \"x\"\\\\",
            "tab\there\r\nline",
            r"C:\Program Files\Git\bin\bash.exe -c 'x'",
        ] {
            assert_eq!(unquote(&quote_arg(script)), script, "script {script:?}");
        }
    }

    #[test]
    fn command_lines_over_the_limit_are_refused() {
        let exe = Path::new(r"C:\Program Files\Git\bin\bash.exe");
        assert!(check_command_line(exe, &quote_arg("echo hi")).is_ok());
        let limit = MAX_COMMAND_LINE_UTF16;
        let overhead = exe.to_string_lossy().len() + 2 + 1 + 2 + 1 + 1;
        // The quoted form of `n` plain characters is `n + 2` long.
        let fits = "x".repeat(limit - overhead - 2);
        assert!(check_command_line(exe, &quote_arg(&fits)).is_ok());
        let too_long = "x".repeat(limit - overhead - 1);
        let error = check_command_line(exe, &quote_arg(&too_long)).unwrap_err();
        assert!(error.contains("too long"), "{error}");
        assert!(error.contains("32767"), "{error}");
    }

    #[test]
    fn command_line_length_counts_utf16_units() {
        let exe = Path::new(r"C:\bash.exe");
        // Each emoji is two UTF-16 units but one char and four bytes.
        let script = "😀".repeat(MAX_COMMAND_LINE_UTF16 / 2);
        assert!(check_command_line(exe, &quote_arg(&script)).is_err());
    }

    #[test]
    fn git_directories_map_to_the_install_root() {
        for (directory, expected) in [
            (
                r"C:\Program Files\Git\cmd",
                Some(r"C:\Program Files\Git\bin\bash.exe"),
            ),
            (
                r"C:\Program Files\Git\bin",
                Some(r"C:\Program Files\Git\bin\bash.exe"),
            ),
            (
                r"C:\Program Files\Git\mingw64\bin",
                Some(r"C:\Program Files\Git\bin\bash.exe"),
            ),
            (r"D:\tools\git\CMD", Some(r"D:\tools\git\bin\bash.exe")),
            (r"D:/tools/git/cmd", Some(r"D:/tools/git\bin\bash.exe")),
            (r"C:\Users\me\scoop\shims", None),
            (r"C:\Windows\System32", None),
        ] {
            assert_eq!(
                bash_for_git_dir(directory).as_deref(),
                expected,
                "{directory}"
            );
        }
    }

    #[test]
    fn override_wins_and_must_exist() {
        let vars = env(&[
            (OVERRIDE_VAR, r"D:\msys64\usr\bin\bash.exe"),
            ("PATH", r"C:\Program Files\Git\cmd"),
        ]);
        let present = files(&[
            r"D:\msys64\usr\bin\bash.exe",
            r"C:\Program Files\Git\cmd\git.exe",
            r"C:\Program Files\Git\bin\bash.exe",
        ]);
        let found = resolve(vars, present).unwrap();
        assert_eq!(found.exe, PathBuf::from(r"D:\msys64\usr\bin\bash.exe"));
        assert_eq!(found.tools_dir.as_deref(), Some(r"D:\msys64\usr\bin"));

        let vars = env(&[(OVERRIDE_VAR, r"D:\nowhere\bash.exe")]);
        let error = resolve(vars, files(&[])).unwrap_err();
        assert!(error.contains("YAWL_GIT_BASH is set to"), "{error}");
        assert!(error.contains("git-scm.com"), "{error}");
    }

    #[test]
    fn bash_is_derived_from_git_on_path() {
        let vars = env(&[(
            "PATH",
            r#"C:\Windows\System32;"D:\dev tools\Git\cmd";C:\Python"#,
        )]);
        let present = files(&[
            r"D:\dev tools\Git\cmd\git.exe",
            r"D:\dev tools\Git\bin\bash.exe",
            r"D:\dev tools\Git\usr\bin",
        ]);
        let found = resolve(vars, present).unwrap();
        assert_eq!(found.exe, PathBuf::from(r"D:\dev tools\Git\bin\bash.exe"));
        assert_eq!(
            found.tools_dir.as_deref(),
            Some(r"D:\dev tools\Git\usr\bin")
        );
    }

    #[test]
    fn git_without_a_bash_next_to_it_is_skipped() {
        let vars = env(&[
            (
                "PATH",
                r"C:\Scoop\apps\git\current\cmd;C:\Program Files\Git\cmd",
            ),
            ("ProgramFiles", r"C:\Program Files"),
        ]);
        let present = files(&[
            r"C:\Scoop\apps\git\current\cmd\git.exe",
            r"C:\Program Files\Git\cmd\git.exe",
            r"C:\Program Files\Git\bin\bash.exe",
        ]);
        let found = resolve(vars, present).unwrap();
        assert_eq!(
            found.exe,
            PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")
        );
        assert_eq!(found.tools_dir, None);
    }

    #[test]
    fn standard_locations_are_tried_in_order() {
        let present = files(&[
            r"C:\Users\me\AppData\Local\Programs\Git\bin\bash.exe",
            r"C:\Program Files (x86)\Git\bin\bash.exe",
        ]);
        let vars = env(&[
            ("PATH", r"C:\Windows\System32"),
            ("ProgramFiles", r"C:\Program Files"),
            ("ProgramW6432", r"C:\Program Files"),
            ("ProgramFiles(x86)", r"C:\Program Files (x86)"),
            ("LOCALAPPDATA", r"C:\Users\me\AppData\Local"),
        ]);
        assert_eq!(
            resolve(vars, present).unwrap().exe,
            PathBuf::from(r"C:\Program Files (x86)\Git\bin\bash.exe")
        );

        let present = files(&[r"C:\Users\me\AppData\Local\Programs\Git\bin\bash.exe"]);
        let vars = env(&[("LOCALAPPDATA", r"C:\Users\me\AppData\Local\")]);
        assert_eq!(
            resolve(vars, present).unwrap().exe,
            PathBuf::from(r"C:\Users\me\AppData\Local\Programs\Git\bin\bash.exe")
        );
    }

    #[test]
    fn the_wsl_launcher_is_never_chosen() {
        let vars = env(&[("PATH", r"C:\Windows\System32")]);
        let present = files(&[r"C:\Windows\System32\bash.exe"]);
        let error = resolve(vars, present).unwrap_err();
        assert!(error.contains("Git Bash was not found"), "{error}");
        assert!(error.contains(OVERRIDE_VAR), "{error}");
    }

    #[test]
    fn tools_come_first_on_path_only_when_known() {
        assert_eq!(
            path_with_tools(Some(r"C:\Git\usr\bin"), OsStr::new(r"C:\Windows")),
            OsString::from(r"C:\Git\usr\bin;C:\Windows")
        );
        assert_eq!(
            path_with_tools(Some(r"C:\Git\usr\bin"), OsStr::new("")),
            OsString::from(r"C:\Git\usr\bin")
        );
        assert_eq!(
            path_with_tools(None, OsStr::new(r"C:\Windows")),
            OsString::from(r"C:\Windows")
        );
    }

    #[test]
    fn paths_for_bash_use_forward_slashes() {
        assert_eq!(
            forward_slashes(r"C:\Users\me\.yawl\tools"),
            "C:/Users/me/.yawl/tools"
        );
        assert_eq!(forward_slashes("already/fine"), "already/fine");
    }
}
