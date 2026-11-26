//! Deterministic input hashes compatible with the Python workflow runner.

use globset::{GlobBuilder, GlobSetBuilder};
use once_cell::sync::Lazy;
use rayon::prelude::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::{DirEntry, WalkDir};

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

#[derive(Debug, PartialEq, Eq, Hash)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: i128,
    ctime_ns: i128,
}

type CacheKey = (PathBuf, FileIdentity);
type DigestBytes = [u8; 32];
static FILE_CACHE: Lazy<Mutex<HashMap<CacheKey, DigestBytes>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn clear_hash_cache() {
    FILE_CACHE.lock().unwrap().clear();
}

fn unix_ns(time: SystemTime) -> i128 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128,
        Err(error) => -(error.duration().as_nanos() as i128),
    }
}

#[cfg(unix)]
fn file_identity(meta: &Metadata) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Ok(FileIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.len(),
        mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        ctime_ns: i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
    })
}

#[cfg(not(unix))]
fn file_identity(meta: &Metadata) -> io::Result<FileIdentity> {
    Ok(FileIdentity {
        dev: 0,
        ino: 0,
        size: meta.len(),
        mtime_ns: unix_ns(meta.modified()?),
        ctime_ns: 0,
    })
}

fn file_digest(path: &Path, use_cache: bool) -> io::Result<DigestBytes> {
    let mut key = None;
    if use_cache {
        let stat = file_identity(&fs::metadata(path)?)?;
        if unix_ns(SystemTime::now()) - stat.mtime_ns >= 2_000_000_000 {
            let candidate = (path.canonicalize()?, stat);
            if let Some(&digest) = FILE_CACHE.lock().unwrap().get(&candidate) {
                return Ok(digest);
            }
            key = Some(candidate);
        }
    }

    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let digest = hash.finalize().into();
    if let Some(key) = key {
        // Compare the open descriptor, even if a rename replaced the pathname.
        if file_identity(&file.metadata()?)? == key.1 {
            let mut cache = FILE_CACHE.lock().unwrap();
            if cache.len() >= 16_384 {
                cache.clear();
            }
            cache.insert(key, digest);
        }
    }
    Ok(digest)
}

pub fn sha256_bytes(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    file_digest(path, false)
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

fn relative_path(path: &Path, root: &Path) -> PathBuf {
    let mut base = root.components().peekable();
    let mut target = path.components().peekable();
    if base.peek() != target.peek() {
        return path.to_owned();
    }
    while base.peek().is_some() && base.peek() == target.peek() {
        base.next();
        target.next();
    }
    let mut relative: PathBuf = base.map(|_| "..").collect();
    relative.extend(target);
    relative
}

fn posix_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
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

fn walk_entry(entry: &DirEntry, ignore: &HashSet<String>) -> bool {
    entry.depth() == 0
        || !entry.file_type().is_dir()
        || !entry
            .file_name()
            .to_str()
            .is_some_and(|name| ignore.contains(name))
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
    let ignore = ignore_dirs.unwrap_or(&defaults);
    let excluded: HashSet<_> = exclude_paths
        .iter()
        .map(|p| normalize(&root.join(p)))
        .collect();
    let mut files = HashSet::new();
    for input in paths {
        let path = normalize(&root.join(input));
        if path.parent().is_some_and(|parent| {
            relative_path(parent, &root)
                .iter()
                .any(|part| part.to_str().is_some_and(|s| ignore.contains(s)))
        }) {
            continue;
        }
        if path.is_file() {
            files.insert(path);
        } else if path.is_dir() && !path.is_symlink() {
            if path != root
                && path
                    .file_name()
                    .and_then(|p| p.to_str())
                    .is_some_and(|s| ignore.contains(s))
            {
                continue;
            }
            for entry in WalkDir::new(path)
                .into_iter()
                .filter_entry(|e| walk_entry(e, ignore))
                .flatten()
            {
                if entry.file_type().is_file()
                    || (entry.file_type().is_symlink() && entry.path().is_file())
                {
                    files.insert(entry.into_path());
                }
            }
        }
    }

    let mut sorted: Vec<_> = files
        .difference(&excluded)
        .map(|p| (posix_bytes(&relative_path(p, &root)), p))
        .collect();
    if sorted
        .iter()
        .all(|(name, _)| std::str::from_utf8(name).is_ok())
    {
        sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    } else {
        sorted.sort_by_cached_key(|(name, _)| filename_codepoints(name));
    }
    let digests: Result<Vec<_>, _> = sorted
        .par_iter()
        .map(|(_, p)| file_digest(p, use_cache).map_err(|e| format!("{}: {e}", p.display())))
        .collect();
    let mut hash = Sha256::new();
    for ((name, _), digest) in sorted.into_iter().zip(digests?) {
        hash.update(name);
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
    let mut builder = GlobSetBuilder::new();
    let mut directories_only = Vec::new();
    let mut matches_root = false;
    let mut max_depth = 0;
    for pattern in patterns {
        if pattern.is_empty() || Path::new(pattern).is_absolute() {
            return Err(format!("Expected a relative glob pattern: {pattern:?}"));
        }
        // pathlib has no brace alternation and treats backslashes literally on Unix.
        let normalized = Path::new(pattern)
            .components()
            .filter(|c| *c != Component::CurDir)
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if normalized.is_empty() || normalized == "**" {
            matches_root = true;
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
        directories_only
            .push(pattern.ends_with('/') || pattern.trim_end_matches('/').ends_with("/**"));
    }
    if matches_root {
        return hash_paths(
            &[&root],
            Some(&root),
            Some(ignore),
            exclude_paths,
            use_cache,
        );
    }
    let matcher = builder.build().map_err(|e| e.to_string())?;
    let mut matches = Vec::new();
    // Once a directory matches, hash_paths walks it, so prune overlapping matches here.
    let mut walk = WalkDir::new(&root).max_depth(max_depth).into_iter();
    while let Some(entry) = walk.next() {
        let Ok(entry) = entry else {
            continue;
        };
        if entry.depth() == 0 {
            continue;
        }
        if !walk_entry(&entry, ignore) {
            walk.skip_current_dir();
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(&root)
            .expect("walk stays under root");
        if matcher
            .matches(relative)
            .iter()
            .any(|&i| !directories_only[i] || entry.file_type().is_dir())
        {
            if entry.file_type().is_dir() {
                walk.skip_current_dir();
            }
            matches.push(entry.into_path());
        }
    }
    hash_paths(
        &matches,
        Some(&root),
        Some(ignore),
        exclude_paths,
        use_cache,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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

    #[test]
    fn old_cache_hit_and_recent_bypass() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("input");
        fs::write(&file, b"first").unwrap();
        let original: DigestBytes = Sha256::digest(b"first").into();
        let old = SystemTime::now() - Duration::from_secs(10);
        let handle = File::options().write(true).open(&file).unwrap();
        handle.set_modified(old).unwrap();
        let key = || {
            (
                file.canonicalize().unwrap(),
                file_identity(&file.metadata().unwrap()).unwrap(),
            )
        };
        assert_eq!(file_digest(&file, true).unwrap(), original);
        // A sentinel distinguishes an actual cache hit from an identical recomputation.
        assert_eq!(
            FILE_CACHE.lock().unwrap().insert(key(), [42; 32]),
            Some(original)
        );
        assert_eq!(file_digest(&file, true).unwrap(), [42; 32]);
        assert_eq!(file_digest(&file, false).unwrap(), original);

        handle.set_modified(SystemTime::now()).unwrap();
        assert_eq!(file_digest(&file, true).unwrap(), original);
        assert!(FILE_CACHE.lock().unwrap().insert(key(), [99; 32]).is_none());
        assert_eq!(file_digest(&file, true).unwrap(), original);
        fs::write(&file, b"other").unwrap();
        handle.set_modified(old + Duration::from_secs(1)).unwrap();
        assert_eq!(
            hex::encode(file_digest(&file, true).unwrap()),
            sha256_bytes(b"other")
        );
        clear_hash_cache();
        assert_eq!(
            file_digest(&file, true).unwrap(),
            file_digest(&file, false).unwrap()
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
