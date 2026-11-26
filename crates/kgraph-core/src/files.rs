//! Atomic file helpers, safe identifier checks, and cross-process file locking.

use fs2::FileExt;
use serde::Serialize;
use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphanumeric() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
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
    let val: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("Invalid JSON in {}: {}", path.display(), e))?;
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

pub(crate) fn sync_file(file: &File) -> std::io::Result<()> {
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

struct TempPath(Option<PathBuf>);

impl Drop for TempPath {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Atomically replace a file with synced bytes from a sibling temporary file.
pub fn write_bytes(path: &Path, content: &[u8]) -> Result<(), String> {
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut created_dir = false;
    let (tmp, mut file) = loop {
        let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".kgraph_tmp.{}.{seq}", std::process::id()));
        if tmp.file_name() == path.file_name() {
            continue;
        }
        match options.open(&tmp) {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) if !created_dir && e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(dir)
                    .map_err(|e| format!("Could not create directory {}: {}", dir.display(), e))?;
                created_dir = true;
            }
            Err(e) => {
                return Err(format!(
                    "Could not create temp file in {}: {}",
                    dir.display(),
                    e
                ))
            }
        }
    };
    let mut guard = TempPath(Some(tmp));
    file.write_all(content)
        .map_err(|e| format!("Could not write temp file for {}: {}", path.display(), e))?;
    sync_file(&file)
        .map_err(|e| format!("Could not sync temp file for {}: {}", path.display(), e))?;
    drop(file);
    fs::rename(guard.0.as_ref().unwrap(), path)
        .map_err(|e| format!("Could not persist {}: {}", path.display(), e))?;
    guard.0 = None;
    Ok(())
}

pub fn write_text(path: &Path, content: &str) -> Result<(), String> {
    write_bytes(path, content.as_bytes())
}

/// Atomically serialize a value to JSON (pretty or compact) with a trailing newline.
pub fn write_json<T: Serialize>(path: &Path, data: &T, pretty: bool) -> Result<(), String> {
    let mut bytes = Vec::with_capacity(4096);
    if pretty {
        serde_json::to_writer_pretty(&mut bytes, data)
    } else {
        serde_json::to_writer(&mut bytes, data)
    }
    .map_err(|e| format!("JSON serialization error for {}: {}", path.display(), e))?;
    bytes.push(b'\n');
    write_bytes(path, &bytes)
}

pub(crate) fn open_lock_file(path: &Path) -> Result<File, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Could not create lock dir {}: {}", parent.display(), e))?;
        }
    }
    OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("Could not open lock file {}: {}", path.display(), e))
}

/// Acquire an exclusive cross-process file lock on `path`.
pub fn file_lock(path: &Path) -> Result<FileLock, String> {
    let file = open_lock_file(path)?;
    file.lock_exclusive()
        .map_err(|e| format!("Could not lock {}: {}", path.display(), e))?;
    Ok(FileLock { file })
}
