//! The self-extension contract and process runner.
//!
//! Any executable in `~/.yawl/tools/` or `./.yawl/tools/` is a tool:
//! - `tool --describe` prints JSON `{name, description, input_schema, timeout_secs?}`
//! - invocation: JSON arguments on stdin; stdout becomes the tool result
//! - non-zero exit = error result with stderr appended
//! - default timeout 120s, overridable via `timeout_secs` in the describe output
//! - the process gets `YAWL_SESSION_ID` in its environment and inherits cwd

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use std::os::unix::process::CommandExt;

use serde::Deserialize;

use crate::provider::ToolSpec;

pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
const DESCRIBE_TIMEOUT_SECS: u64 = 10;
const MAX_CAPTURE_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
struct DescribeOutput {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default = "default_schema")]
    input_schema: serde_json::Value,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

fn default_schema() -> serde_json::Value {
    serde_json::json!({"type": "object", "properties": {}})
}

#[derive(Clone)]
pub struct ExecTool {
    pub path: PathBuf,
    pub spec: ToolSpec,
    pub timeout: Duration,
}

/// Caches `--describe` results by (path, mtime) so the per-turn registry
/// rescan does not respawn every tool. Failures are cached too; editing the
/// tool (new mtime) triggers a fresh describe.
#[derive(Default)]
pub struct DescribeCache {
    entries: HashMap<PathBuf, (SystemTime, Option<ExecTool>)>,
}

/// Scans one directory for executables and describes them.
/// Returns discovered tools plus human-readable warnings for broken ones.
pub fn scan_dir(dir: &Path, cache: &mut DescribeCache) -> (Vec<ExecTool>, Vec<String>) {
    let mut tools = Vec::new();
    let mut warnings = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            cache.entries.retain(|path, _| path.parent() != Some(dir));
            return (tools, warnings);
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_executable_file(p))
        .collect();
    paths.sort();
    cache
        .entries
        .retain(|path, _| path.parent() != Some(dir) || paths.binary_search(path).is_ok());
    for path in paths {
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if let Some((cached_mtime, cached)) = cache.entries.get(&path)
            && *cached_mtime == mtime
        {
            match cached {
                Some(tool) => tools.push(tool.clone()),
                None => warnings.push(format!("{}: --describe failed (cached)", path.display())),
            }
            continue;
        }
        match describe(&path) {
            Ok(tool) => {
                cache.entries.insert(path, (mtime, Some(tool.clone())));
                tools.push(tool);
            }
            Err(reason) => {
                warnings.push(format!("{}: {reason}", path.display()));
                cache.entries.insert(path, (mtime, None));
            }
        }
    }
    (tools, warnings)
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn describe(path: &Path) -> Result<ExecTool, String> {
    let mut cmd = Command::new(path);
    cmd.arg("--describe");
    let result = run_with_timeout(cmd, None, Duration::from_secs(DESCRIBE_TIMEOUT_SECS))
        .map_err(|e| format!("spawn failed: {e}"))?;
    if result.timed_out {
        return Err("--describe timed out".to_string());
    }
    if result.status != Some(0) {
        return Err(format!(
            "--describe exited with {:?}: {}",
            result.status,
            crate::error::truncate(result.stderr.trim(), 200)
        ));
    }
    let parsed: DescribeOutput = serde_json::from_str(result.stdout.trim())
        .map_err(|e| format!("bad --describe json: {e}"))?;
    if !valid_tool_name(&parsed.name) {
        return Err("--describe name must be 1-64 ASCII letters, digits, '_' or '-'".to_string());
    }
    Ok(ExecTool {
        path: path.to_path_buf(),
        spec: ToolSpec {
            name: parsed.name,
            description: parsed.description,
            input_schema: parsed.input_schema,
        },
        timeout: Duration::from_secs(parsed.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1)),
    })
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Runs an exec tool: JSON args on stdin, stdout is the result.
pub fn invoke(tool: &ExecTool, args_json: &str, session_id: &str) -> (String, bool) {
    let mut cmd = Command::new(&tool.path);
    cmd.env("YAWL_SESSION_ID", session_id);
    let result = match run_with_timeout(cmd, Some(args_json.as_bytes()), tool.timeout) {
        Ok(r) => r,
        Err(e) => {
            return (
                format!("failed to spawn {}: {e}", tool.path.display()),
                true,
            );
        }
    };
    render_result(&result, tool.timeout)
}

/// Formats a finished process per the contract: stdout is the result;
/// non-zero exit is an error with stderr appended.
pub fn render_result(result: &ExecResult, timeout: Duration) -> (String, bool) {
    if result.interrupted {
        return ("[interrupted by user]".to_string(), true);
    }
    if result.timed_out {
        return (format!("tool timed out after {}s", timeout.as_secs()), true);
    }
    let mut out = result.stdout.clone();
    if result.status == Some(0) {
        (out, false)
    } else {
        if !result.stderr.trim().is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("[stderr]\n");
            out.push_str(result.stderr.trim_end());
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        match result.status {
            Some(code) => out.push_str(&format!("[exit code: {code}]")),
            None => out.push_str("[killed by signal]"),
        }
        (out, true)
    }
}

pub struct ExecResult {
    /// Exit code; `None` if killed by a signal.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub interrupted: bool,
}

/// Spawns a command with piped I/O and waits with a timeout, polling
/// `try_wait` (no tokio). Also honors the global interrupt flag: Ctrl+C
/// kills the child. Reader threads drain stdout/stderr to avoid pipe
/// deadlock on chatty children.
pub fn run_with_timeout(
    mut cmd: Command,
    stdin_data: Option<&[u8]>,
    timeout: Duration,
) -> std::io::Result<ExecResult> {
    // Put the command in its own process group. Killing only the immediate
    // shell would leave grandchildren running and could keep our output
    // pipes open forever.
    cmd.process_group(0);
    cmd.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    if let Some(data) = stdin_data {
        let mut stdin = child.stdin.take().expect("stdin was piped");
        let data = data.to_vec();
        // Ignore broken-pipe: the tool may not read stdin at all.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let out_handle = std::thread::spawn(move || drain(stdout));
    let err_handle = std::thread::spawn(move || drain(stderr));

    let deadline = Instant::now().checked_add(timeout);
    let mut timed_out = false;
    let mut interrupted = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if crate::interrupted() {
            interrupted = true;
            kill_and_reap(&mut child);
            break None;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            timed_out = true;
            kill_and_reap(&mut child);
            break None;
        }
        std::thread::sleep(Duration::from_millis(30));
    };

    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok(ExecResult {
        status: status.and_then(|s| s.code()),
        stdout,
        stderr,
        timed_out,
        interrupted,
    })
}

fn kill_and_reap(child: &mut Child) {
    let process_group = i32::try_from(child.id()).ok();
    if let Some(process_group) = process_group {
        // SAFETY: `kill` receives a valid negative process-group id created
        // for this child. SIGKILL requires no Rust-side memory invariants.
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    } else {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn drain(mut reader: impl Read) -> String {
    let mut captured = Vec::with_capacity(MAX_CAPTURE_BYTES);
    let mut chunk = [0u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&chunk[..read.min(remaining)]);
        truncated |= read > remaining;
    }
    let mut output = String::from_utf8_lossy(&captured).into_owned();
    if truncated {
        output.push_str("\n[process output truncated]");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_with_timeout_captures_output_and_exit_code() -> std::io::Result<()> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo out; echo err >&2; exit 3");
        let r = run_with_timeout(cmd, None, Duration::from_secs(5))?;
        assert_eq!(r.status, Some(3));
        assert_eq!(r.stdout.trim(), "out");
        assert_eq!(r.stderr.trim(), "err");
        let (text, is_error) = render_result(&r, Duration::from_secs(5));
        assert!(is_error);
        assert!(text.contains("[stderr]"));
        assert!(text.contains("[exit code: 3]"));
        Ok(())
    }

    #[test]
    fn run_with_timeout_kills_hung_process() -> std::io::Result<()> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & wait");
        let start = Instant::now();
        let r = run_with_timeout(cmd, None, Duration::from_millis(200))?;
        assert!(r.timed_out);
        assert!(start.elapsed() < Duration::from_secs(5));
        Ok(())
    }

    #[test]
    fn stdin_is_delivered() -> std::io::Result<()> {
        let cmd = Command::new("cat");
        let r = run_with_timeout(cmd, Some(b"{\"x\":1}"), Duration::from_secs(5))?;
        assert_eq!(r.stdout, "{\"x\":1}");
        assert_eq!(r.status, Some(0));
        Ok(())
    }

    #[test]
    fn tool_names_are_provider_safe() {
        assert!(valid_tool_name("search_web-2"));
        assert!(!valid_tool_name(""));
        assert!(!valid_tool_name("has spaces"));
        assert!(!valid_tool_name(&"x".repeat(65)));
    }

    #[test]
    fn process_capture_is_bounded() -> std::io::Result<()> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("yes x | head -c 100000");
        let result = run_with_timeout(cmd, None, Duration::from_secs(5))?;
        assert!(result.stdout.len() < 70_000);
        assert!(result.stdout.ends_with("[process output truncated]"));
        Ok(())
    }

    #[test]
    fn describe_cache_forgets_removed_tools() {
        let dir = std::env::temp_dir().join(format!(
            "yawl-describe-cache-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cache = DescribeCache::default();
        cache
            .entries
            .insert(dir.join("removed-tool"), (SystemTime::UNIX_EPOCH, None));

        let (tools, warnings) = scan_dir(&dir, &mut cache);

        assert!(tools.is_empty());
        assert!(warnings.is_empty());
        assert!(cache.entries.is_empty());
    }
}
