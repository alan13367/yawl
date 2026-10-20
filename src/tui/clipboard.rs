//! Clipboard image extraction and private temporary-file staging.

use std::fs::{self, File, OpenOptions};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::{Child, Command, ExitStatus, Stdio};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(any(target_os = "macos", target_os = "linux"))]
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);
/// Starting PowerShell and loading Windows Forms takes noticeably longer
/// than the macOS and Linux helpers.
#[cfg(any(windows, test))]
const POWERSHELL_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StagedImage {
    pub(super) id: usize,
    pub(super) path: PathBuf,
    pub(super) media_type: String,
}

impl StagedImage {
    pub(super) fn marker(&self) -> String {
        format!("[Image #{}]", self.id)
    }
}

#[derive(Default)]
pub(super) struct ClipboardStore {
    process_dir: Option<PathBuf>,
    next_file: usize,
}

impl ClipboardStore {
    pub(super) fn paste_image(&mut self, id: usize) -> Result<StagedImage, String> {
        let process_dir = self
            .ensure_process_dir()
            .map_err(|error| error.to_string())?
            .to_path_buf();
        self.next_file = self.next_file.saturating_add(1);
        let part = process_dir.join(format!("clipboard-{}.part", self.next_file));
        create_private_file(&part)
            .map_err(|error| format!("cannot stage clipboard image: {error}"))?;

        let result = platform_read(&part).and_then(|()| validate_staged(&part));
        let media_type = match result {
            Ok(media_type) => media_type,
            Err(error) => {
                let _ = fs::remove_file(&part);
                return Err(error);
            }
        };
        let final_path = part.with_extension(crate::image::extension(media_type));
        fs::rename(&part, &final_path)
            .map_err(|error| format!("cannot finish clipboard image: {error}"))?;
        Ok(StagedImage {
            id,
            path: final_path,
            media_type: media_type.to_string(),
        })
    }

    fn ensure_process_dir(&mut self) -> io::Result<&Path> {
        if self.process_dir.is_none() {
            let root = staging_root();
            ensure_private_directory(&root)?;
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let directory = root.join(format!("{}-{stamp}", std::process::id()));
            create_private_directory(&directory)?;
            self.process_dir = Some(directory);
        }
        self.process_dir
            .as_deref()
            .ok_or_else(|| io::Error::other("clipboard directory was not created"))
    }
}

impl Drop for ClipboardStore {
    fn drop(&mut self) {
        if let Some(directory) = self.process_dir.take() {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

/// Where per-process staging directories live.
#[cfg(unix)]
fn staging_root() -> PathBuf {
    PathBuf::from("/tmp/yawl-clipboard")
}

/// The per-user temporary directory (`%LOCALAPPDATA%\Temp`) is private to the
/// user by its default ACL, unlike a shared `/tmp`.
#[cfg(windows)]
fn staging_root() -> PathBuf {
    std::env::temp_dir().join("yawl-clipboard")
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

/// New directories inherit the private ACL of the user's temporary directory,
/// so no mode is set.
#[cfg(windows)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> io::Result<()> {
    // SAFETY: `geteuid` has no preconditions and does not dereference pointers.
    let effective_uid = unsafe { libc::geteuid() };
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != effective_uid
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{} is not a private directory owned by this user",
                        path.display()
                    ),
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_private_directory(path),
        Err(error) => Err(error),
    }
}

/// The staging root sits in the user's private temporary directory, which no
/// other user can write to, so there is no owner or mode to verify; only a
/// reparse point or plain file squatting on the name is refused.
#[cfg(windows)]
fn ensure_private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a plain directory", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_private_directory(path),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// The file inherits the private ACL of its staging directory.
#[cfg(windows)]
fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn validate_staged(path: &Path) -> Result<&'static str, String> {
    let bytes = fs::read(path).map_err(|error| format!("cannot read clipboard image: {error}"))?;
    if bytes.is_empty() {
        return Err("clipboard does not contain a supported image".into());
    }
    if bytes.len() > crate::image::MAX_IMAGE_BYTES {
        return Err(format!(
            "clipboard image exceeds the {}-byte limit",
            crate::image::MAX_IMAGE_BYTES
        ));
    }
    crate::image::media_type(&bytes)
        .ok_or_else(|| "clipboard image must be PNG, JPEG, GIF, or WebP".into())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn wait_for_child(child: &mut Child, deadline: Instant) -> Result<ExitStatus, String> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if crate::interrupted() => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("clipboard image paste was interrupted".into());
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("clipboard image paste timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(format!("cannot wait for clipboard helper: {error}")),
        }
    }
}

#[cfg(target_os = "macos")]
fn platform_read(path: &Path) -> Result<(), String> {
    let path = path.to_string_lossy();
    let script = format!(
        "try\nset imgData to the clipboard as \u{00AB}class PNGf\u{00BB}\n\
         set fRef to open for access POSIX file \"{path}\" with write permission\n\
         set eof of fRef to 0\nwrite imgData to fRef\nclose access fRef\n\
         on error message\ntry\nclose access POSIX file \"{path}\"\nend try\nerror message\nend try"
    );
    let mut child = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run osascript: {error}"))?;
    if wait_for_child(&mut child, Instant::now() + CLIPBOARD_TIMEOUT)?.success() {
        Ok(())
    } else {
        Err("clipboard does not contain an image macOS can convert to PNG".into())
    }
}

#[cfg(target_os = "linux")]
fn platform_read(path: &Path) -> Result<(), String> {
    const ATTEMPTS: &[(&str, &[&str])] = &[
        ("wl-paste", &["--type", "image/png"]),
        ("wl-paste", &["--type", "image/jpeg"]),
        ("wl-paste", &["--type", "image/webp"]),
        ("wl-paste", &["--type", "image/gif"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/jpeg", "-o"],
        ),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/webp", "-o"],
        ),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/gif", "-o"],
        ),
    ];
    let mut found_helper = false;
    let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
    for (program, args) in ATTEMPTS {
        if Instant::now() >= deadline {
            return Err("clipboard image paste timed out".into());
        }
        let output = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .map_err(|error| format!("cannot prepare clipboard image file: {error}"))?;
        let mut child = match Command::new(program)
            .args(*args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(output))
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("cannot run {program}: {error}")),
        };
        found_helper = true;
        if wait_for_child(&mut child, deadline)?.success()
            && fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0)
        {
            return Ok(());
        }
    }
    if found_helper {
        Err("clipboard does not contain a supported image".into())
    } else {
        Err("image paste on Linux requires wl-paste or xclip".into())
    }
}

/// Variable the script reads the target path from, so the path never passes
/// through string interpolation into code.
#[cfg(any(windows, test))]
const POWERSHELL_PATH_VARIABLE: &str = "YAWL_CLIPBOARD_IMAGE_PATH";

/// Saves the clipboard bitmap as PNG. Exits 0 on success, 2 when the clipboard
/// holds no image, and 1 with a message on stderr for anything else.
#[cfg(any(windows, test))]
const POWERSHELL_SCRIPT: &str = concat!(
    "$ErrorActionPreference = 'Stop'; ",
    "try { ",
    "Add-Type -AssemblyName System.Windows.Forms, System.Drawing; ",
    "$image = [System.Windows.Forms.Clipboard]::GetImage(); ",
    "if ($null -eq $image) { exit 2 }; ",
    "try { $image.Save($env:YAWL_CLIPBOARD_IMAGE_PATH, [System.Drawing.Imaging.ImageFormat]::Png) } ",
    "finally { $image.Dispose() } ",
    "} catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }",
);

#[cfg(windows)]
fn platform_read(path: &Path) -> Result<(), String> {
    use crate::tools::exec::{Keep, RunOptions, run};

    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-Command",
            POWERSHELL_SCRIPT,
        ])
        .env(POWERSHELL_PATH_VARIABLE, path);
    let result = run(
        command,
        RunOptions {
            stdin: None,
            timeout: Some(POWERSHELL_TIMEOUT),
            stdout: Keep::Head(1024),
            stderr: Keep::Head(2048),
        },
    );
    match result {
        Ok(result) => powershell_outcome(
            result.status,
            result.timed_out,
            result.interrupted,
            &result.stderr,
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err("image paste on Windows requires powershell.exe".into())
        }
        Err(error) => Err(format!("cannot run powershell.exe: {error}")),
    }
}

/// Maps how the PowerShell helper ended to the paste result.
#[cfg(any(windows, test))]
fn powershell_outcome(
    status: Option<i32>,
    timed_out: bool,
    interrupted: bool,
    stderr: &str,
) -> Result<(), String> {
    if interrupted {
        return Err("clipboard image paste was interrupted".into());
    }
    if timed_out {
        return Err("clipboard image paste timed out".into());
    }
    match status {
        Some(0) => Ok(()),
        Some(2) => Err("clipboard does not contain a supported image".into()),
        status => {
            let message = stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(|line| crate::text::sanitize_line(line, crate::text::Tabs::Space));
            Err(match (message, status) {
                (Some(message), _) => format!("cannot read clipboard image: {message}"),
                (None, Some(code)) => format!("clipboard helper exited with status {code}"),
                (None, None) => "clipboard helper ended without a status".into(),
            })
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn platform_read(_: &Path) -> Result<(), String> {
    Err("clipboard image paste is supported only on macOS, Linux, and Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("yawl-clipboard-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn staged_marker_uses_stable_id() {
        let image = StagedImage {
            id: 3,
            path: PathBuf::from("/tmp/image.png"),
            media_type: "image/png".into(),
        };
        assert_eq!(image.marker(), "[Image #3]");
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_has_owner_only_permissions() -> io::Result<()> {
        let path = temp_path("private");
        let _ = fs::remove_dir_all(&path);
        ensure_private_directory(&path)?;
        let metadata = fs::metadata(&path)?;
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        fs::remove_dir(path)
    }

    #[test]
    fn store_drop_removes_only_its_process_directory() -> io::Result<()> {
        let path = temp_path("drop");
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path)?;
        fs::write(path.join("image.png"), b"image")?;
        let store = ClipboardStore {
            process_dir: Some(path.clone()),
            next_file: 1,
        };
        drop(store);
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn powershell_results_map_to_paste_errors() {
        assert_eq!(powershell_outcome(Some(0), false, false, ""), Ok(()));
        assert_eq!(
            powershell_outcome(Some(2), false, false, ""),
            Err("clipboard does not contain a supported image".into())
        );
        assert_eq!(
            powershell_outcome(Some(1), false, false, "\n  Out of memory \x1b[2J\n"),
            Err("cannot read clipboard image: Out of memory \u{fffd}[2J".into())
        );
        assert_eq!(
            powershell_outcome(Some(7), false, false, ""),
            Err("clipboard helper exited with status 7".into())
        );
        assert_eq!(
            powershell_outcome(None, false, false, ""),
            Err("clipboard helper ended without a status".into())
        );
        assert!(
            powershell_outcome(None, true, false, "")
                .is_err_and(|error| error.contains("timed out"))
        );
        assert!(
            powershell_outcome(Some(0), false, true, "")
                .is_err_and(|error| error.contains("interrupted"))
        );
    }

    #[test]
    fn powershell_script_reads_its_path_from_the_environment() {
        assert!(POWERSHELL_SCRIPT.contains(&format!("$env:{POWERSHELL_PATH_VARIABLE}")));
        // No double quotes, so the argument needs no escaping on a command line.
        assert!(!POWERSHELL_SCRIPT.contains('"'));
        assert!(POWERSHELL_TIMEOUT >= Duration::from_secs(5));
    }

    #[test]
    fn staged_validation_checks_magic_and_size() -> io::Result<()> {
        let valid = temp_path("valid.png");
        fs::write(&valid, b"\x89PNG\r\n\x1a\nrest")?;
        assert_eq!(validate_staged(&valid).as_deref(), Ok("image/png"));
        fs::remove_file(valid)?;

        let invalid = temp_path("invalid.bin");
        fs::write(&invalid, b"plain text")?;
        assert!(validate_staged(&invalid).is_err());
        fs::remove_file(invalid)
    }
}
