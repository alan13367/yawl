//! Runs a project's `verify_command`, the check that decides whether a turn's
//! changes are finished. The conversation decides when; this owns how.

use std::path::Path;
use std::time::Duration;

use super::{exec, output};

/// Long enough for a full test suite. A project that needs less can bound its
/// own command.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Changes to these alone do not start a check. `.txt` is not among them, because
/// `requirements.txt` and `CMakeLists.txt` drive builds.
const DOCUMENT_EXTENSIONS: &[&str] = &["md", "markdown", "mdx", "rst", "adoc", "org"];
/// The name a check's run goes by in turn events, though no model called it.
pub(crate) const TOOL_NAME: &str = "verify";

pub(crate) struct VerifyRun {
    pub(crate) passed: bool,
    pub(crate) interrupted: bool,
    /// Bounded for the model; long output is saved under `output_dir`.
    pub(crate) output: String,
}

pub(crate) fn run(command: &str, output_dir: &Path) -> VerifyRun {
    let failed = |output: String| VerifyRun {
        passed: false,
        interrupted: false,
        output,
    };
    let shell = match exec::shell_command(command, None) {
        Ok(shell) => shell,
        Err(error) => return failed(format!("could not start the shell: {error}")),
    };
    match exec::run_with_timeout(shell, None, VERIFY_TIMEOUT) {
        Ok(result) => {
            let (mut content, is_error) = exec::render_result(&result, VERIFY_TIMEOUT);
            output::prepare(output_dir, &mut content);
            VerifyRun {
                passed: !is_error,
                interrupted: result.interrupted,
                output: content,
            }
        }
        Err(error) => failed(format!("could not start the shell: {error}")),
    }
}

pub(crate) fn is_document(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            DOCUMENT_EXTENSIONS
                .iter()
                .any(|document| extension.eq_ignore_ascii_case(document))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_status_decides_and_output_comes_back() {
        let directory = crate::test_support::unique_temp_dir("verify");
        let passed = run("echo all good", &directory);
        assert!(passed.passed);
        assert_eq!(passed.output.trim(), "all good");

        let failed = run("echo broken >&2; exit 3", &directory);
        assert!(!failed.passed);
        assert!(!failed.interrupted);
        assert!(failed.output.contains("broken"), "{}", failed.output);
        assert!(failed.output.contains("exit code: 3"), "{}", failed.output);
    }

    #[test]
    fn documents_are_recognized_by_extension() {
        assert!(is_document(Path::new("README.md")));
        assert!(is_document(Path::new("docs/GUIDE.RST")));
        assert!(!is_document(Path::new("src/main.rs")));
        assert!(!is_document(Path::new("Makefile")));
        assert!(!is_document(Path::new("CMakeLists.txt")));
        assert!(!is_document(Path::new("requirements.txt")));
    }
}
