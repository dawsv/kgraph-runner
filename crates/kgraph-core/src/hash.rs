//! Deterministic input hashes compatible with the Python workflow runner.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use once_cell::sync::Lazy;
use rayon::prelude::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_IGNORE_DIRS: &[&str] = &[
    ".git",
    ".kgraph",
    ".venv",
    "venv",
    "node_modules",
    "__pycache__",
    "Library",
    "Temp",
    "Obj",
    "Build",
    "Logs",
    "target",
];

pub fn default_ignore_dirs() -> HashSet<String> {
    DEFAULT_IGNORE_DIRS.iter().map(|s| (*s).into()).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: i128,
    ctime_ns: i128,
}

type DigestBytes = [u8; 32];
type CacheShard = RwLock<HashMap<FileIdentity, DigestBytes>>;
static FILE_CACHE: Lazy<[CacheShard; 64]> =
    Lazy::new(|| std::array::from_fn(|_| RwLock::default()));

thread_local! {
    static READ_BUF: RefCell<Vec<u8>> = RefCell::new(vec![0; 64 * 1024]);
}

fn cache_shard(stat: &FileIdentity) -> &CacheShard {
    &FILE_CACHE[(stat.ino as usize) & 63]
}

pub fn clear_hash_cache() {
    for shard in FILE_CACHE.iter() {
        shard.write().unwrap().clear();
    }
}

fn unix_ns(time: SystemTime) -> i128 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128,
        Err(error) => -(error.duration().as_nanos() as i128),
    }
}

#[cfg(unix)]
fn file_identity(meta: &Metadata) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(FileIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.len(),
        mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        ctime_ns: i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
    })
}

#[cfg(not(unix))]
fn file_identity(_: &Metadata) -> Option<FileIdentity> {
    // Size and mtime cannot distinguish a replacement with identical timestamps.
    None
}

#[derive(Clone, Copy)]
enum CacheMode {
    Disabled,
    Cold(i128),
    Warm(i128),
}

fn cache_key(meta: &Metadata, now_ns: i128) -> Option<FileIdentity> {
    file_identity(meta).filter(|s| now_ns - s.mtime_ns >= 2_000_000_000)
}

fn file_digest(
    path: &Path,
    mode: CacheMode,
    metadata: Option<&Metadata>,
) -> io::Result<DigestBytes> {
    let (mut file, key) = match mode {
        CacheMode::Disabled => (File::open(path)?, None),
        CacheMode::Cold(now_ns) => {
            // With no cached values to probe, open once and stat the descriptor.
            let file = File::open(path)?;
            let key = match metadata {
                Some(meta) => cache_key(meta, now_ns),
                None => cache_key(&file.metadata()?, now_ns),
            };
            (file, key)
        }
        CacheMode::Warm(now_ns) => {
            let key = match metadata {
                Some(meta) => cache_key(meta, now_ns),
                None => cache_key(&fs::metadata(path)?, now_ns),
            };
            if let Some(stat) = key {
                if let Some(&digest) = cache_shard(&stat).read().unwrap().get(&stat) {
                    return Ok(digest);
                }
            }
            (File::open(path)?, key)
        }
    };
    let mut hash = Sha256::new();
    READ_BUF.with_borrow_mut(|buffer| -> io::Result<()> {
        loop {
            let count = file.read(buffer)?;
            if count == 0 {
                return Ok(());
            }
            hash.update(&buffer[..count]);
        }
    })?;
    let digest = hash.finalize().into();
    if let Some(stat) = key {
        // Compare the open descriptor, even if a rename replaced the pathname.
        if file_identity(&file.metadata()?) == Some(stat) {
            let mut cache = cache_shard(&stat).write().unwrap();
            if cache.len() >= 512 {
                cache.clear();
            }
            cache.insert(stat, digest);
        }
    }
    Ok(digest)
}

pub fn sha256_bytes(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    file_digest(path, CacheMode::Disabled, None)
        .map(hex::encode)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn json_string(out: &mut String, value: &str) {
    // serde supplies the JSON control escapes; Python additionally uses ensure_ascii.
    for ch in serde_json::to_string(value)
        .expect("string serialization")
        .chars()
    {
        if ch >= '\u{7f}' {
            for unit in ch.encode_utf16(&mut [0; 2]) {
                write!(out, "\\u{unit:04x}").unwrap();
            }
        } else {
            out.push(ch);
        }
    }
}

fn json_float(number: &serde_json::Number) -> String {
    // Reformat shortest round-trip digits using Python repr's exponent thresholds.
    let raw = number.to_string();
    let sign = if raw.starts_with('-') { "-" } else { "" };
    let unsigned = raw.trim_start_matches('-');
    let (mantissa, exponent) = unsigned.split_once('e').unwrap_or((unsigned, "0"));
    let point = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let digits = mantissa.replace('.', "");
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return format!("{sign}0.0");
    }
    let exp =
        exponent.parse::<i32>().unwrap() + point - (digits.len() - significant.len()) as i32 - 1;
    let digits = significant.trim_end_matches('0');
    if !(-4..16).contains(&exp) {
        let (first, rest) = digits.split_at(1);
        let dot = if rest.is_empty() { "" } else { "." };
        format!("{sign}{first}{dot}{rest}e{exp:+03}")
    } else if exp < 0 {
        format!("{sign}0.{}{digits}", "0".repeat((-exp - 1) as usize))
    } else {
        let point = exp as usize + 1;
        if digits.len() > point {
            format!("{sign}{}.{}", &digits[..point], &digits[point..])
        } else {
            format!("{sign}{digits}{}.0", "0".repeat(point - digits.len()))
        }
    }
}

fn python_json(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                json_string(out, key);
                out.push_str(": ");
                python_json(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                python_json(item, out);
            }
            out.push(']');
        }
        Value::String(s) => json_string(out, s),
        Value::Number(n) if n.is_f64() => out.push_str(&json_float(n)),
        other => out.push_str(&other.to_string()),
    }
}

pub fn sha256_json(value: &Value) -> String {
    let mut out = String::new();
    python_json(value, &mut out);
    sha256_bytes(out.as_bytes())
}

// Match abspath/relpath lexically so file symlink aliases keep their own names.
fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => (),
            _ => result.push(part),
        }
    }
    result
}

fn absolute_root(root: Option<&Path>) -> Result<PathBuf, String> {
    let root = root.unwrap_or(Path::new("."));
    if root.is_absolute() {
        return Ok(normalize(root));
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    Ok(normalize(&cwd.join(root)))
}

fn relative_path<'a>(path: &'a Path, root: &Path) -> Cow<'a, Path> {
    if let Ok(relative) = path.strip_prefix(root) {
        return Cow::Borrowed(relative);
    }
    let mut base = root.components().peekable();
    let mut target = path.components().peekable();
    if base.peek() != target.peek() {
        return Cow::Borrowed(path);
    }
    while base.peek().is_some() && base.peek() == target.peek() {
        base.next();
        target.next();
    }
    let mut relative: PathBuf = base.map(|_| "..").collect();
    relative.extend(target);
    Cow::Owned(relative)
}

fn posix_bytes(path: Cow<'_, Path>) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        match path {
            Cow::Borrowed(path) => path.as_os_str().as_bytes().to_vec(),
            Cow::Owned(path) => path.into_os_string().into_vec(),
        }
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().replace('\\', "/").into_bytes()
    }
}

// Python sorts filenames as Unicode, mapping each undecodable Unix byte to
// U+DC80..U+DCFF. Hash the original bytes, even when their sort order differs.
fn filename_codepoints(mut name: &[u8]) -> Vec<u32> {
    let mut key = Vec::new();
    loop {
        match std::str::from_utf8(name) {
            Ok(text) => {
                key.extend(text.chars().map(u32::from));
                break;
            }
            Err(e) => {
                let (valid, tail) = name.split_at(e.valid_up_to());
                key.extend(std::str::from_utf8(valid).unwrap().chars().map(u32::from));
                key.push(0xdc00 + u32::from(tail[0]));
                name = &tail[1..];
            }
        }
    }
    key
}

struct InputFile {
    name: Vec<u8>,
    path: PathBuf,
    // Only explicit paths and symlinks already needed metadata to identify files.
    metadata: Option<Box<Metadata>>,
}

struct Walker<'a> {
    root: &'a Path,
    ignore: &'a HashSet<String>,
    excluded: HashSet<PathBuf>,
    files: Mutex<Vec<InputFile>>,
}

struct GlobFilter {
    matcher: GlobSet,
    directories_only: Vec<bool>,
    max_depth: usize,
}

impl<'a> Walker<'a> {
    fn new(root: &'a Path, ignore: &'a HashSet<String>, excluded: &[PathBuf]) -> Self {
        Self {
            root,
            ignore,
            excluded: excluded.iter().map(|p| normalize(&root.join(p))).collect(),
            files: Mutex::default(),
        }
    }

    fn ignored(&self, path: &Path) -> bool {
        relative_path(path, self.root)
            .iter()
            .any(|part| part.to_str().is_some_and(|s| self.ignore.contains(s)))
    }

    fn input(&self, path: PathBuf) {
        if path.parent().is_some_and(|parent| self.ignored(parent)) {
            return;
        }
        let Ok(link) = fs::symlink_metadata(&path) else {
            return;
        };
        if link.is_dir() {
            if !self.ignored(&path) {
                self.walk(&path, None);
            }
        } else {
            let meta = if link.is_symlink() {
                fs::metadata(&path).ok()
            } else {
                Some(link)
            };
            if let Some(file) = meta.filter(|m| m.is_file()) {
                if !self.excluded.contains(&path) {
                    self.files.lock().unwrap().push(InputFile {
                        name: posix_bytes(relative_path(&path, self.root)),
                        path,
                        metadata: Some(Box::new(file)),
                    });
                }
            }
        }
    }

    fn walk(&self, base: &Path, filter: Option<&GlobFilter>) {
        rayon::scope(|scope| self.directory(scope, base, base, filter, 1));
    }

    fn directory<'s>(
        &'s self,
        scope: &rayon::Scope<'s>,
        dir: &Path,
        base: &'s Path,
        filter: Option<&'s GlobFilter>,
        depth: usize,
    ) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        // A parent glob can start above root. Apply custom ignored components
        // (including "..") to the matched input relative to the original root.
        let base_is_root = base == self.root;
        let ignored_parent = filter.is_some() && !base_is_root && self.ignored(dir);
        let mut batch = Vec::new();
        let mut matches = Vec::new();
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|s| self.ignore.contains(s))
            {
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(base).expect("walk stays under base");
            let selected = filter.is_none_or(|glob| {
                glob.matcher.matches_into(relative, &mut matches);
                matches
                    .iter()
                    .any(|&i| kind.is_dir() || !glob.directories_only[i])
            });
            if selected && ignored_parent {
                continue;
            }
            if kind.is_dir() {
                if selected || filter.is_some_and(|glob| depth < glob.max_depth) {
                    let next_filter = if selected { None } else { filter };
                    scope.spawn(move |scope| {
                        self.directory(scope, &path, base, next_filter, depth + 1)
                    });
                }
            } else if selected && !self.excluded.contains(&path) {
                let metadata = if kind.is_file() {
                    None
                } else if kind.is_symlink() {
                    let Ok(meta) = fs::metadata(&path) else {
                        continue;
                    };
                    if !meta.is_file() {
                        continue;
                    }
                    Some(Box::new(meta))
                } else {
                    continue;
                };
                let name = if base_is_root {
                    Cow::Borrowed(relative)
                } else {
                    relative_path(&path, self.root)
                };
                batch.push(InputFile {
                    name: posix_bytes(name),
                    path,
                    metadata,
                });
            }
        }
        if !batch.is_empty() {
            self.files.lock().unwrap().append(&mut batch);
        }
    }
}

pub fn hash_paths<P: AsRef<Path>>(
    paths: &[P],
    root: Option<&Path>,
    ignore_dirs: Option<&HashSet<String>>,
    exclude_paths: &[PathBuf],
    use_cache: bool,
) -> Result<String, String> {
    let root = absolute_root(root)?;
    let defaults = default_ignore_dirs();
    let walker = Walker::new(&root, ignore_dirs.unwrap_or(&defaults), exclude_paths);
    for input in paths {
        walker.input(normalize(&root.join(input)));
    }
    hash_files(walker.files.into_inner().unwrap(), use_cache)
}

fn hash_files(mut sorted: Vec<InputFile>, use_cache: bool) -> Result<String, String> {
    if sorted.iter().all(|f| std::str::from_utf8(&f.name).is_ok()) {
        sorted.sort_unstable_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    } else {
        sorted.sort_by_cached_key(|f| filename_codepoints(&f.name));
    }
    sorted.dedup_by(|a, b| a.path == b.path);
    let mode = if use_cache {
        let now_ns = unix_ns(SystemTime::now());
        if sorted.len() >= FILE_CACHE.len()
            && FILE_CACHE
                .iter()
                .all(|shard| shard.read().unwrap().is_empty())
        {
            CacheMode::Cold(now_ns)
        } else {
            CacheMode::Warm(now_ns)
        }
    } else {
        CacheMode::Disabled
    };
    let reads = sorted.par_iter();
    // Excess concurrent opens contend inside the macOS filesystem. Four
    // batches retain parallel SHA-256 while bounding active file readers.
    #[cfg(target_os = "macos")]
    let reads = reads.with_min_len((sorted.len() / 4).max(1));
    let digests: Result<Vec<_>, _> = reads
        .map(|f| {
            file_digest(&f.path, mode, f.metadata.as_deref())
                .map_err(|e| format!("{}: {e}", f.path.display()))
        })
        .collect();
    let mut hash = Sha256::new();
    for (file, digest) in sorted.into_iter().zip(digests?) {
        hash.update(file.name);
        hash.update(b"\0");
        hash.update(digest);
    }
    Ok(hex::encode(hash.finalize()))
}

pub fn hash_globs(
    patterns: &[String],
    root: Option<&Path>,
    ignore_dirs: Option<&HashSet<String>>,
    exclude_paths: &[PathBuf],
    use_cache: bool,
) -> Result<String, String> {
    let root = absolute_root(root)?;
    let defaults = default_ignore_dirs();
    let ignore = ignore_dirs.unwrap_or(&defaults);
    // Group by walk base so ordinary globs stay within root, even in a
    // request which also watches siblings. The walker keys every match
    // relative to the original root.
    let mut groups: HashMap<PathBuf, Vec<(String, bool)>> = HashMap::new();
    for pattern in patterns {
        if pattern.is_empty() || Path::new(pattern).is_absolute() {
            return Err(format!("Expected a relative glob pattern: {pattern:?}"));
        }
        let mut base = root.clone();
        let mut parts = Path::new(pattern)
            .components()
            .filter(|c| *c != Component::CurDir)
            .peekable();
        while parts.next_if_eq(&Component::ParentDir).is_some() {
            base.pop();
        }
        // pathlib has no brace alternation and treats backslashes literally on Unix.
        let normalized = parts
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let directory_only = pattern.ends_with('/') || normalized.ends_with("/**");
        groups
            .entry(base)
            .or_default()
            .push((normalized, directory_only));
    }
    let walker = Walker::new(&root, ignore, exclude_paths);
    for (base, group) in groups {
        collect_glob_matches(&base, group, &walker)?;
    }
    hash_files(walker.files.into_inner().unwrap(), use_cache)
}

fn collect_glob_matches(
    base: &Path,
    patterns: Vec<(String, bool)>,
    walker: &Walker<'_>,
) -> Result<(), String> {
    let mut builder = GlobSetBuilder::new();
    let mut directories_only = Vec::new();
    let mut max_depth = 0;
    for (normalized, directory_only) in patterns {
        if normalized.is_empty() || normalized == "**" {
            walker.input(base.to_owned());
            return Ok(());
        }
        let normalized = normalized.strip_suffix("/**").unwrap_or(&normalized);
        let components = normalized.split('/');
        max_depth = if components.clone().any(|c| c == "**") {
            usize::MAX
        } else {
            max_depth.max(components.count())
        };
        let escaped = normalized
            .replace('\\', "\\\\")
            .replace('{', "\\{")
            .replace('}', "\\}");
        builder.add(
            GlobBuilder::new(&escaped)
                .literal_separator(true)
                .case_insensitive(cfg!(windows))
                .allow_unclosed_class(true)
                .backslash_escape(true)
                .build()
                .map_err(|e| e.to_string())?,
        );
        directories_only.push(directory_only);
    }
    let filter = GlobFilter {
        matcher: builder.build().map_err(|e| e.to_string())?,
        directories_only,
        max_depth,
    };
    walker.walk(base, Some(&filter));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[cfg(unix)]
    use std::time::Duration;
    use tempfile::TempDir;

    fn write(root: &Path, name: &str, data: &[u8]) {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    #[test]
    fn python_json_digest_handles_sorted_unicode_keys_and_float_thresholds() {
        let value = json!({"é": {"🚀": "\u{7f}\t"}, "z": [1e-5, 1e-4, 1e15, 1e16, -0.0]});
        // hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()
        assert_eq!(
            sha256_json(&value),
            "5f2e5f740e888dd573ad0c2b7bf750031cbef9528dea672e03d05cc5f1b7b84f"
        );
        assert_eq!(
            sha256_json(&json!([{"x": false, "a": null},42])),
            sha256_bytes(br#"[{"a": null, "x": false}, 42]"#)
        );
    }

    #[test]
    fn path_and_glob_hashes_match_python_digest() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "a/b.txt", b"\0hello\n");
        write(root, "src/main.py", b"pass\n");
        write(root, "src/skip.py", b"excluded");
        for &ignored in DEFAULT_IGNORE_DIRS {
            write(root, &format!("src/{ignored}/hidden.py"), b"ignored");
        }
        let excluded = [PathBuf::from("src/./skip.py")];
        // Python hash_paths(["a/b.txt", "src/main.py"], root=root)
        let expected = "00a0af42bcc7e8e5063491b7ed313b417dc795d8279cbb6d4fd876f01845ebd7";
        for paths in [
            &["src", "a", "src/main.py"][..],
            &[".", "src/.git/hidden.py"],
            &["a/b.txt", "src"],
        ] {
            assert_eq!(
                hash_paths(paths, Some(root), None, &excluded, false).unwrap(),
                expected
            );
        }
        for patterns in [
            ["a/b.txt", "src/**/*.py"],
            ["**/*.py", "a/b.txt"],
            ["src/", "a"],
        ] {
            assert_eq!(
                hash_globs(
                    &patterns.map(String::from),
                    Some(root),
                    None,
                    &excluded,
                    true
                )
                .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn parent_globs_preserve_python_relative_path_keys() {
        let tmp = TempDir::new().unwrap();
        let subdir = tmp.path().join("subdir");
        write(&subdir, "local.txt", b"local");
        write(tmp.path(), "sibling/file.txt", b"sibling contents\n");
        // kgraph_runner.helpers.hash_globs(["../sibling/file.txt"], root=subdir)
        let expected = "20152ddf2719e1f2993263513d3802f8d9dd0df0f90d90bae53132f1227060b9";
        for pattern in ["../sibling/file.txt", "./../sibling/*.txt", "../sibling/"] {
            assert_eq!(
                hash_globs(&[pattern.into()], Some(&subdir), None, &[], false).unwrap(),
                expected
            );
        }
        assert_eq!(
            hash_globs(
                &["**".into(), "../sibling/*.txt".into()],
                Some(&subdir),
                None,
                &[],
                false
            )
            .unwrap(),
            hash_paths(
                &["local.txt", "../sibling/file.txt"],
                Some(&subdir),
                None,
                &[],
                false
            )
            .unwrap()
        );
    }

    #[test]
    fn parent_globs_deduplicate_against_local_matches_and_apply_parent_ignores() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        write(&root, "dir/keep.bin", b"local");
        write(&root, "dir/skip.bin", b"excluded");
        write(tmp.path(), "other/outside.bin", b"outside");
        let patterns = ["dir/", "../**/*.bin", "**/*.bin"].map(String::from);
        let excluded = [PathBuf::from("dir/skip.bin")];
        for (ignore, files) in [
            (HashSet::new(), vec!["dir/keep.bin", "../other/outside.bin"]),
            (HashSet::from(["..".to_owned()]), vec!["dir/keep.bin"]),
        ] {
            assert_eq!(
                hash_globs(&patterns, Some(&root), Some(&ignore), &excluded, true).unwrap(),
                hash_paths(&files, Some(&root), Some(&ignore), &[], false).unwrap()
            );
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn replacement_with_same_size_and_mtime_is_rehashed() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("file");
        let old = SystemTime::now() - std::time::Duration::from_secs(10);
        for contents in [b"first", b"other"] {
            fs::write(&path, contents).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap();
            assert_eq!(
                hex::encode(
                    file_digest(&path, CacheMode::Warm(unix_ns(SystemTime::now())), None).unwrap()
                ),
                sha256_bytes(contents)
            );
            fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn globs_respect_path_components_and_custom_ignores() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        for file in [
            "package.toml",
            ".hidden.toml",
            "docs/config.toml",
            "a/b.py",
            "top.py",
            "foo{bar}",
            "same",
            "custom/no.py",
        ] {
            write(root, file, file.as_bytes());
        }
        let ignore = HashSet::from(["custom".into(), "same".into()]);
        for (patterns, paths) in [
            (vec!["*.toml"], vec![".hidden.toml", "package.toml"]),
            (vec!["**/*.py", "a/*.py"], vec!["a/b.py", "top.py"]),
            (vec!["a/*.py", "**/*.py"], vec!["a/b.py", "top.py"]),
            (vec!["foo{bar}", "same"], vec!["foo{bar}", "same"]),
            (vec!["*.toml/"], Vec::new()),
            (vec!["a/**"], vec!["a/b.py"]),
        ] {
            let patterns: Vec<_> = patterns.into_iter().map(String::from).collect();
            assert_eq!(
                hash_globs(&patterns, Some(root), Some(&ignore), &[], false).unwrap(),
                hash_paths(&paths, Some(root), Some(&ignore), &[], false).unwrap()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn old_cache_hit_and_recent_bypass() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("input");
        fs::write(&file, b"first").unwrap();
        let original: DigestBytes = Sha256::digest(b"first").into();
        let old = SystemTime::now() - Duration::from_secs(10);
        let handle = File::options().write(true).open(&file).unwrap();
        handle.set_modified(old).unwrap();
        let cached =
            || file_digest(&file, CacheMode::Warm(unix_ns(SystemTime::now())), None).unwrap();
        let key = || file_identity(&file.metadata().unwrap()).unwrap();
        assert_eq!(cached(), original);
        // A sentinel distinguishes an actual cache hit from an identical recomputation.
        assert_eq!(
            cache_shard(&key()).write().unwrap().insert(key(), [42; 32]),
            Some(original)
        );
        assert_eq!(cached(), [42; 32]);
        assert_eq!(
            file_digest(&file, CacheMode::Disabled, None).unwrap(),
            original
        );

        handle.set_modified(SystemTime::now()).unwrap();
        assert_eq!(cached(), original);
        assert!(cache_shard(&key())
            .write()
            .unwrap()
            .insert(key(), [99; 32])
            .is_none());
        assert_eq!(cached(), original);
        fs::write(&file, b"other").unwrap();
        handle.set_modified(old + Duration::from_secs(1)).unwrap();
        assert_eq!(hex::encode(cached()), sha256_bytes(b"other"));
        clear_hash_cache();
        assert_eq!(
            cached(),
            file_digest(&file, CacheMode::Disabled, None).unwrap()
        );
    }

    #[test]
    fn python_surrogateescape_sort_order_preserves_filename_bytes() {
        // Test bytes directly because APFS refuses to create non-UTF-8 filenames.
        let expected: [&[u8]; 4] = ["é".as_bytes(), &[0x80], &[0xff], "🚀".as_bytes()];
        let mut names = expected;
        names.reverse();
        names.sort_by_cached_key(|name| filename_codepoints(name));
        assert_eq!(names, expected);
    }

    #[cfg(unix)]
    #[test]
    fn file_symlink_names_and_paths_outside_root() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        write(&root, "dir/file", b"f");
        write(tmp.path(), "outside", b"o");
        symlink(root.join("dir"), root.join("alias-dir")).unwrap();
        symlink(root.join("dir/file"), root.join("alias-file")).unwrap();
        symlink(root.join("nonexistent"), root.join("broken")).unwrap();
        let mut expected = Sha256::new();
        for (path, content) in [
            ("../outside", b"o"),
            ("alias-file", b"f"),
            ("dir/file", b"f"),
        ] {
            expected.update(path.as_bytes());
            expected.update([0]);
            expected.update(Sha256::digest(content));
        }
        assert_eq!(
            hash_paths(
                &["../outside", ".", "alias-dir"],
                Some(&root),
                None,
                &[],
                true
            )
            .unwrap(),
            hex::encode(expected.finalize())
        );
    }
}
