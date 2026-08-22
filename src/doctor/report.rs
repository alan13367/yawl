//! Finding rendering and the doctor exit code.

use std::io::IsTerminal;

use super::{Finding, Severity};

/// Prints findings grouped by severity. Colors are used only on a terminal.
pub(super) fn print(findings: &[Finding]) {
    let colored = std::io::stdout().is_terminal();
    if findings.is_empty() {
        println!("\nConfiguration looks healthy.");
        return;
    }
    println!();
    for severity in [Severity::Error, Severity::Warning, Severity::Info] {
        for finding in findings
            .iter()
            .filter(|finding| finding.severity == severity)
        {
            println!("{} {}", mark(severity, colored), finding);
        }
    }
    let errors = count(findings, Severity::Error);
    let warnings = count(findings, Severity::Warning);
    let notes = count(findings, Severity::Info);
    println!("\n{errors} error(s), {warnings} warning(s), {notes} note(s).");
}

/// 0 when no error findings remain, 1 otherwise.
pub(super) fn exit_code(findings: &[Finding]) -> i32 {
    findings
        .iter()
        .any(|finding| finding.severity == Severity::Error)
        .into()
}

fn count(findings: &[Finding], severity: Severity) -> usize {
    findings
        .iter()
        .filter(|finding| finding.severity == severity)
        .count()
}

fn mark(severity: Severity, colored: bool) -> String {
    let (symbol, code) = match severity {
        Severity::Error => ("✗", "31"),
        Severity::Warning => ("!", "33"),
        Severity::Info => ("·", "36"),
    };
    if colored {
        format!("\x1b[{code}m{symbol}\x1b[0m")
    } else {
        symbol.to_string()
    }
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.fix {
            Some(fix) => write!(
                f,
                "{}: {} [fix: {}]",
                self.area,
                self.message,
                fix.describe()
            ),
            None => write!(f, "{}: {}", self.area, self.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Fix;

    fn finding(severity: Severity) -> Finding {
        Finding {
            severity,
            area: "global config".into(),
            message: "malformed JSON: EOF".into(),
            fix: Some(Fix::QuarantineFile {
                path: std::path::PathBuf::from("/tmp/config.json"),
            }),
        }
    }

    #[test]
    fn exit_code_reflects_remaining_errors() {
        assert_eq!(exit_code(&[]), 0);
        assert_eq!(exit_code(&[finding(Severity::Warning)]), 0);
        assert_eq!(exit_code(&[finding(Severity::Error)]), 1);
    }

    #[test]
    fn display_names_the_area_and_the_fix() {
        let line = finding(Severity::Error).to_string();
        assert!(line.contains("global config: malformed JSON"));
        assert!(line.contains("[fix: rename /tmp/config.json"));
    }
}
