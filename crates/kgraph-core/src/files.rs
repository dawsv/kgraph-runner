//! Atomic file helpers, safe identifier checks, and cross-process file locking.

use fs2::FileExt;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use tempfile::NamedTempFile;

static ID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z0-9_][A-Za-z0-9_-]*$").expect("valid ID regex"));

/// Guard holding an exclusive advisory lock on a sibling lock file.
pub struct FileLock {
    file: File,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn valid_id(value: &str) -> bool {
    ID_RE.is_match(value)
}

pub fn require_id(value: &str) -> Result<String, String> {
    if !valid_id(value) {
        return Err(format!(
            "Invalid ID {:?}: use letters, digits, underscores, or hyphens",
            value
        ));
    }
    Ok(value.to_string())
}

pub fn read_object(path: &Path) -> Result<Map<String, Value>, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let val: Value =
        serde_json::from_str(&raw).map_err(|e| format!("Invalid JSON in {}: {}", path.display(), e))?;
    match val {
        Value::Object(map) => Ok(map),
        _ => Err(format!("{} must contain a JSON object", path.display())),
    }
}

pub fn read_optional_object(path: &Path) -> Result<Map<String, Value>, String> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let val: Value = serde_json::from_str(&raw)
                .map_err(|e| format!("Invalid JSON in {}: {}", path.display(), e))?;
            match val {
                Value::Object(map) => Ok(map),
                _ => Err(format!("{} must contain a JSON object", path.display())),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("{}: {}", path.display(), e)),
    }
}

// Resolve symlinks before processing "..", including symlinks to future output files.
pub(crate) fn resolve_path(path: &Path) -> Result<PathBuf, String> {
    fn resolve(base: &mut PathBuf, path: &Path, depth: usize) -> Result<(), String> {
        if depth > 40 {
            return Err(format!(
                "Too many symbolic links resolving {}",
                path.display()
            ));
        }
        for part in path.components() {
            match part {
                Component::ParentDir => {
                    base.pop();
                }
                Component::CurDir => {}
                Component::Normal(name) => {
                    base.push(name);
                    match fs::symlink_metadata(&base) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            let target = fs::read_link(&base).map_err(|e| e.to_string())?;
                            base.pop();
                            resolve(base, &target, depth + 1)?;
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(format!("{}: {e}", base.display())),
                    }
                }
                _ => base.push(part.as_os_str()),
            }
        }
        Ok(())
    }
    let mut base = std::env::current_dir().map_err(|e| e.to_string())?;
    resolve(&mut base, path, 0)?;
    Ok(base)
}

fn sync_file(file: &File) -> std::io::Result<()> {
    // Rust's File::sync_all() and File::sync_data() both issue F_FULLFSYNC on
    // Apple platforms (~4.4 ms NVMe barrier per checkpoint). Call POSIX fsync
    // directly on Apple targets to match Python's os.fsync() durability and speed.
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        let rc = unsafe { libc::fsync(file.as_raw_fd()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        file.sync_data()
    }
}

/// Atomically write UTF-8 text via a temporary file in the same directory + fsync + rename.
pub fn write_text(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Could not create directory {}: {}", parent.display(), e))?;
        }
    }
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(dir)
        .map_err(|e| format!("Could not create temp file in {}: {}", dir.display(), e))?;
    tmp.write_all(content.as_bytes())
        .map_err(|e| format!("Could not write temp file for {}: {}", path.display(), e))?;
    sync_file(tmp.as_file())
        .map_err(|e| format!("Could not sync temp file for {}: {}", path.display(), e))?;
    tmp.persist(path)
        .map_err(|e| format!("Could not persist {}: {}", path.display(), e.error))?;
    Ok(())
}

/// Atomically serialize a value to JSON (pretty or compact) with a trailing newline.
pub fn write_json<T: Serialize>(path: &Path, data: &T, pretty: bool) -> Result<(), String> {
    let mut text = if pretty {
        serde_json::to_string_pretty(data)
    } else {
        serde_json::to_string(data)
    }
    .map_err(|e| format!("JSON serialization error for {}: {}", path.display(), e))?;
    text.push('\n');
    write_text(path, &text)
}

/// Acquire an exclusive cross-process file lock on `path`.
pub fn file_lock(path: &Path) -> Result<FileLock, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Could not create lock dir {}: {}", parent.display(), e))?;
        }
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("Could not open lock file {}: {}", path.display(), e))?;
    file.lock_exclusive()
        .map_err(|e| format!("Could not lock {}: {}", path.display(), e))?;
    Ok(FileLock { file })
}
