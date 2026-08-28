//! Clipboard image extraction and private temporary-file staging.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);

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
            let root = PathBuf::from("/tmp/yawl-clipboard");
            ensure_private_directory(&root)?;
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let directory = root.join(format!("{}-{stamp}", std::process::id()));
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700).create(&directory)?;
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
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700).create(path)
        }
        Err(error) => Err(error),
    }
}

fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_read(_: &Path) -> Result<(), String> {
    Err("clipboard image paste is supported only on macOS and Linux".into())
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
