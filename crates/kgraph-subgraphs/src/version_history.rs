//! Author, validate, and package a Portyl Version History release.

use crate::common::{
    file_hash, fingerprint, packet_worker, require_bug_id, verification, with_retry_route,
    PacketWorkerOptions, Repairs, StringFn,
};
use chrono::{Datelike, Local, Timelike};
use kgraph_core::files::{read_object, require_id, write_json, write_text};
use kgraph_core::hash::{sha256_bytes, sha256_file};
use kgraph_core::types::{
    Context, LoopSubgraphNode, Node, TaskKind, TaskNode, TaskRunError, VerifyOutcome,
};
use kgraph_core::verifiers::{check_jpeg, check_webp, inspect_png};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use url::Url;
use zip::{write::SimpleFileOptions, CompressionMethod, DateTime, ZipArchive, ZipWriter};

const BUILDER: &str = ".agents/skills/portyl-version-history/scripts/build_package.py";
const MIB: u64 = 1024 * 1024;
const RELEASE_ID_ERROR: &str =
    "release_id must start with a lowercase letter or digit and use up to 80 slug characters";

pub fn require_release_id(value: &str) -> Result<String, String> {
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if value.len() > 80
        || !value.bytes().next().is_some_and(alnum)
        || !value.bytes().all(|b| alnum(b) || b"._-".contains(&b))
    {
        return Err(RELEASE_ID_ERROR.into());
    }
    require_bug_id(value.split('.').next().unwrap_or_default())?;
    Ok(value.into())
}

pub fn find_version_history_builder(builder_script: Option<&Path>) -> Option<PathBuf> {
    [
        builder_script.map(Path::to_owned),
        env::home_dir().map(|p| p.join(BUILDER)),
        env::current_dir().ok().map(|p| p.join(BUILDER)),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_file())
    .and_then(|p| p.canonicalize().ok())
}

pub fn check_image_file(path: &Path, extension: &str) -> Result<(), String> {
    match extension {
        ".png" => inspect_png(path).map(|_| ()),
        ".jpg" | ".jpeg" => check_jpeg(&fs::read(path).map_err(err)?),
        ".webp" => check_webp(&fs::read(path).map_err(err)?),
        _ => Err(format!("Unsupported image format: {extension}")),
    }
}

#[derive(Clone, Debug)]
pub struct VersionHistoryConfig {
    pub work_dir: PathBuf,
    pub product: String,
    pub release_id: Option<String>,
    pub changes_manifest_path: Option<PathBuf>,
    pub media_dir: Option<PathBuf>,
    pub builder_script: Option<PathBuf>,
    pub max_retries: usize,
    pub deps: Vec<String>,
    pub node_id: String,
    pub source_paths: Vec<PathBuf>,
}

impl Default for VersionHistoryConfig {
    fn default() -> Self {
        Self {
            work_dir: ".".into(),
            product: "desktop".into(),
            release_id: None,
            changes_manifest_path: None,
            media_dir: None,
            builder_script: None,
            max_retries: 3,
            deps: Vec::new(),
            node_id: "version_history_package".into(),
            source_paths: Vec::new(),
        }
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// Path.resolve(strict=False) also follows symlinks in partially delivered packages.
fn resolve(path: &Path) -> Result<PathBuf, String> {
    fn walk(path: &Path, result: &mut PathBuf, links: &mut HashSet<PathBuf>) -> io::Result<()> {
        for component in path.components() {
            match component {
                Component::CurDir => (),
                Component::ParentDir => {
                    result.pop();
                }
                Component::Normal(name) => {
                    result.push(name);
                    match fs::symlink_metadata(&*result) {
                        Ok(meta) if meta.is_symlink() => {
                            let link = result.clone();
                            if !links.insert(link.clone()) {
                                return Err(io::Error::other(format!(
                                    "Symlink loop at {}",
                                    link.display()
                                )));
                            }
                            let target = fs::read_link(&link)?;
                            result.pop();
                            walk(&target, result, links)?;
                            links.remove(&link);
                        }
                        Ok(_) => (),
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                            ) => {}
                        Err(e) => return Err(e),
                    }
                }
                _ => result.push(component.as_os_str()),
            }
        }
        Ok(())
    }
    let mut result = PathBuf::new();
    walk(
        &env::current_dir().map_err(err)?.join(path),
        &mut result,
        &mut HashSet::new(),
    )
    .map_err(err)?;
    Ok(result)
}

// Python's casefold equivalence expands ligatures and sharp S, and distinguishes dotless i.
fn folded_path(name: &str) -> String {
    let mut folded = String::new();
    for ch in name.chars() {
        match ch {
            'ı' => folded.push(ch),
            'ẞ' => folded.push_str("ss"),
            _ => folded.extend(ch.to_uppercase().flat_map(char::to_lowercase)),
        }
    }
    folded
}

fn local_media(pkg: &Path, name: &str) -> Result<PathBuf, String> {
    if name.split('/').any(|part| matches!(part, "" | "." | ".."))
        || name.chars().any(|c| c < ' ' || r#"\:*?"<>|#"#.contains(c))
        || folded_path(name) == "release.json"
    {
        return Err(format!("Unsafe media path: '{name}'"));
    }
    let path = resolve(&pkg.join(name))?;
    if !path.starts_with(pkg) {
        return Err(format!("Media is outside the package: '{name}'"));
    }
    Ok(path)
}

type Media = BTreeMap<String, PathBuf>;

fn local_references(doc: &Value, pkg: &Path) -> Media {
    let mut media = Media::new();
    for change in doc["changes"].as_array().into_iter().flatten() {
        for item in change["evidence"].as_array().into_iter().flatten() {
            if let Some(name) = item["url"].as_str() {
                if let Ok(path) = local_media(pkg, name) {
                    media.insert(name.to_owned(), path);
                }
            }
        }
    }
    media
}

fn repr(value: &Value) -> String {
    match value {
        Value::String(s) => format!("'{s}'"),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

fn check_evidence(
    item: &Value,
    pkg: &Path,
    media: &mut Media,
    before: &mut HashSet<String>,
    after: &mut HashSet<String>,
) -> Result<(), String> {
    let name = item["url"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or("Each evidence entry needs a URL")?;
    let trimmed = name.trim_start_matches(|c| c <= ' ');
    if let Some((scheme, rest)) = trimmed.split_once(':') {
        if scheme.eq_ignore_ascii_case("https") {
            let authority = rest
                .strip_prefix("//")
                .unwrap_or("")
                .split(['/', '?', '#'])
                .next()
                .unwrap_or("");
            if authority.is_empty()
                || authority.contains(['@', '\\'])
                || !Url::parse(trimmed).is_ok_and(|u| u.host().is_some())
            {
                return Err("Invalid HTTPS evidence URL".into());
            }
            return Ok(());
        }
    }
    let path = local_media(pkg, name)?;
    let extension = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{}", s.to_ascii_lowercase()))
        .unwrap_or_default();
    let image = match (item["kind"].as_str(), extension.as_str()) {
        (Some("image"), ".png" | ".jpg" | ".jpeg" | ".webp") => true,
        (Some("video"), ".mp4" | ".mov" | ".webm") => false,
        _ => {
            return Err(format!(
                "Unsupported media format for {}: {name}",
                repr(&item["kind"])
            ))
        }
    };
    if !path
        .metadata()
        .is_ok_and(|m| m.is_file() && (1..=25 * MIB).contains(&m.len()))
    {
        return Err(format!("Missing or oversized media: {name}"));
    }
    if !media.contains_key(name) {
        if image {
            check_image_file(&path, &extension)?;
        }
        media.insert(name.into(), path.clone());
    }
    if image {
        match item["stage"].as_str() {
            Some("before") => {
                before.insert(sha256_file(&path)?);
            }
            Some("after") => {
                after.insert(sha256_file(&path)?);
            }
            _ => (),
        }
    }
    Ok(())
}

fn preflight(doc: &Value, pkg: &Path, product: &str, release_id: Option<&str>) -> VerifyOutcome {
    let fail = VerifyOutcome::fail;
    if doc.get("platforms").is_some() {
        return fail(
            "forbidden_platforms_field",
            "Remove platforms from release.json. The service supplies the product group.",
        );
    }
    if doc.get("revision").is_some() {
        return fail(
            "unsupported_release_field",
            "Write the authoring revision in author_meta.json.",
        );
    }
    if doc["product"].as_str() != Some(product) {
        return VerifyOutcome::fail(
            "product_mismatch",
            format!("Expected product '{product}' in release.json."),
        );
    }
    let id = match require_release_id(doc["release_id"].as_str().unwrap_or("")) {
        Ok(id) => id,
        Err(e) => return VerifyOutcome::fail("invalid_release_id", e),
    };
    if let Some(expected) = release_id.filter(|s| *s != id) {
        return VerifyOutcome::fail(
            "release_id_mismatch",
            format!("Expected release_id '{expected}' in release.json."),
        );
    }
    let Some(changes) = doc["changes"].as_array().filter(|a| !a.is_empty()) else {
        return fail("missing_changes", "Provide a non-empty changes list.");
    };
    let mut ids = HashSet::new();
    let mut media = Media::new();
    for change in changes {
        let Some(id) = change["id"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && ids.insert(*s))
        else {
            return VerifyOutcome::fail(
                "invalid_change_id",
                format!(
                    "Each change needs a unique non-empty id. Got {}.",
                    repr(&change["id"])
                ),
            );
        };
        let (mut before, mut after) = (HashSet::new(), HashSet::new());
        if let Some(evidence) = change.get("evidence") {
            let Some(items) = evidence.as_array() else {
                return VerifyOutcome::fail(
                    "invalid_evidence",
                    format!("Evidence for {id} must be a list."),
                );
            };
            for item in items {
                if let Err(e) = check_evidence(item, pkg, &mut media, &mut before, &mut after) {
                    return VerifyOutcome::fail("invalid_media", format!("Change {id}: {e}"));
                }
            }
        }
        if before.is_empty() || after.is_empty() {
            return VerifyOutcome::fail(
                "missing_before_after_media",
                format!("Change {id} needs local before and after images."),
            );
        }
        if !before.is_disjoint(&after) {
            return VerifyOutcome::fail(
                "duplicate_before_after_media",
                format!("Change {id} uses identical before and after media."),
            );
        }
    }
    let size = |p: &Path| p.metadata().map(|m| m.len()).map_err(err);
    let manifest_size = match size(&pkg.join("release.json")) {
        Ok(n) => n,
        Err(e) => return VerifyOutcome::fail("invalid_media", e),
    };
    let expanded = media.values().try_fold(manifest_size, |sum, p| {
        size(p).map(|n| sum.saturating_add(n))
    });
    let expanded = match expanded {
        Ok(n) => n,
        Err(e) => return VerifyOutcome::fail("invalid_media", e),
    };
    if manifest_size > 256 * 1024 || media.len() >= 150 || expanded > 100 * MIB {
        return fail(
            "package_too_large",
            "The package exceeds the entry count or expanded ZIP size limit.",
        );
    }
    if media
        .keys()
        .map(|s| folded_path(s))
        .collect::<HashSet<_>>()
        .len()
        != media.len()
    {
        return fail(
            "duplicate_media_path",
            "Media paths must be unique on case-insensitive filesystems.",
        );
    }
    VerifyOutcome::pass(format!(
        "Validated {} changes with {} local media files.",
        changes.len(),
        media.len()
    ))
}

fn list_source_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(err)? {
        let entry = entry.map_err(err)?;
        let path = entry.path();
        if entry.file_type().map_err(err)?.is_dir() {
            list_source_files(&path, files)?;
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

// Python files_hash calls fingerprint with the list as its sole positional argument.
fn path_list_hash(paths: &[PathBuf]) -> String {
    let entries: Vec<_> = paths
        .iter()
        .map(|p| json!([p, file_hash(Some(p))]))
        .collect();
    fingerprint(json!([entries]))
}

struct Package {
    cfg: VersionHistoryConfig,
    dir: PathBuf,
    pkg: PathBuf,
    release: PathBuf,
    meta: PathBuf,
    check_log: PathBuf,
    checked: PathBuf,
    build_log: PathBuf,
    manifest: PathBuf,
}

impl Package {
    fn new(mut cfg: VersionHistoryConfig, builder: Option<PathBuf>) -> Result<Self, String> {
        let dir = resolve(&cfg.work_dir)?.join("version_history");
        let pkg = dir.join("package");
        cfg.builder_script = builder;
        cfg.media_dir = cfg.media_dir.as_deref().map(resolve).transpose()?;
        cfg.changes_manifest_path = cfg
            .changes_manifest_path
            .as_deref()
            .map(resolve)
            .transpose()?;
        cfg.source_paths = cfg
            .source_paths
            .iter()
            .map(|p| resolve(p))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            release: pkg.join("release.json"),
            meta: pkg.join("author_meta.json"),
            check_log: dir.join("check.log"),
            checked: dir.join("check_result.json"),
            build_log: dir.join("build.log"),
            manifest: dir.join("package_manifest.json"),
            dir,
            pkg,
            cfg,
        })
    }

    fn source_hash(&self) -> Result<String, String> {
        let mut files = Vec::new();
        if let Some(dir) = &self.cfg.media_dir {
            list_source_files(dir, &mut files)?;
        }
        files.sort();
        let extra: Vec<_> = self
            .cfg
            .source_paths
            .iter()
            .filter(|p| p.is_file())
            .cloned()
            .collect();
        let builder = self.cfg.builder_script.as_deref();
        let schema = builder
            .and_then(Path::parent)
            .and_then(Path::parent)
            .map(|p| p.join("references/schema.json"));
        let changes = self.cfg.changes_manifest_path.as_deref();
        let path_string =
            |p: Option<&Path>| p.map_or_else(|| "None".into(), |p| p.display().to_string());
        let mut inputs = vec![
            json!(self.cfg.product),
            json!(self.cfg.release_id),
            json!(path_string(self.cfg.media_dir.as_deref())),
            json!(path_string(builder)),
            json!(file_hash(builder)),
            json!(file_hash(schema.as_deref())),
            json!(path_string(changes)),
            json!(file_hash(changes)),
            json!(path_list_hash(&files)),
        ];
        if !extra.is_empty() {
            inputs.push(json!(path_list_hash(&extra)));
        }
        Ok(fingerprint(Value::Array(inputs)))
    }

    fn doc(&self) -> Value {
        Value::Object(read_object(&self.release).unwrap_or_default())
    }

    fn delivery_hash(&self) -> String {
        let media: BTreeSet<_> = local_references(&self.doc(), &self.pkg)
            .into_values()
            .collect();
        let mut paths = vec![self.release.clone(), self.meta.clone()];
        paths.extend(media);
        path_list_hash(&paths)
    }

    fn inputs(&self, repairs: &Repairs) -> Result<String, String> {
        Ok(fingerprint(json!([
            self.source_hash()?,
            self.delivery_hash(),
            repairs.instructions()?["revision"]
        ])))
    }

    fn packet(&self) -> Result<Value, String> {
        fs::create_dir_all(&self.pkg).map_err(err)?;
        Ok(json!({
            "stage": "author_release", "product": self.cfg.product, "release_id": self.cfg.release_id,
            "pkg_dir": self.pkg, "release_json": self.release, "author_meta": self.meta,
            "changes_manifest_path": self.cfg.changes_manifest_path, "media_dir": self.cfg.media_dir,
            "builder_script": self.cfg.builder_script, "check_log": self.check_log, "build_log": self.build_log,
            "required_release_fields": ["schema_version", "product", "release_id", "version", "title", "date", "stage", "summary", "changes"],
            "required_change_fields": ["id", "type", "title", "description", "verification", "evidence"],
            "instructions": [
                "Write release.json with schema_version 2. Omit platforms and other unsupported fields.",
                "Use the requested release_id when supplied and a unique release_id for each publication. Use stable, unique change IDs.",
                "Choose source, staging, internal, or production for the stage actually reached. State what passed and what remains untested in verification.",
                "Copy genuine matched before and after images into the package for every change. Reference them by relative URL in evidence entries with kind image and stage before or after.",
                "Keep secrets and personal information out of the manifest and media.",
                "Write author_meta.json beside release.json with the packet revision (integer) and matching release_id."
            ]
        }))
    }

    fn check(&self, repairs: &Repairs) -> Result<VerifyOutcome, String> {
        let doc = self.doc();
        let output = if let Some(builder) = &self.cfg.builder_script {
            match run_builder(builder, "check", &self.release, None) {
                Ok(output) => Some(output),
                Err(e) => {
                    write_text(&self.check_log, &format!("{e}\n"))?;
                    return Ok(VerifyOutcome::fail("vh_check_launch_error", e));
                }
            }
        } else {
            None
        };
        let mut result = preflight(
            &doc,
            &self.pkg,
            &self.cfg.product,
            self.cfg.release_id.as_deref(),
        );
        let mut log = String::new();
        if let Some((status, text)) = output {
            log = text;
            if !status.success() {
                let normalized = log.replace(self.pkg.to_string_lossy().as_ref(), "<package>");
                let sig = fingerprint(json!([normalized]));
                let detail = if log.trim().is_empty() {
                    format!("Builder check exited with code {}", exit_code(status))
                } else {
                    log.trim().to_owned()
                };
                result = VerifyOutcome::fail(format!("vh_check_failed:{}", &sig[..8]), detail);
            }
        }
        if result.ok {
            let meta = Value::Object(read_object(&self.meta).unwrap_or_default());
            let requested = repairs.instructions()?["revision"]
                .as_u64()
                .unwrap_or_default();
            if !meta["revision"].as_u64().is_some_and(|n| n >= requested)
                || meta["release_id"] != doc["release_id"]
            {
                result = VerifyOutcome::fail("invalid_author_meta", "Write author_meta.json with the requested revision and the release_id from release.json.");
            }
        }
        if log.is_empty() || !result.error_signature.starts_with("vh_check_failed:") {
            if !log.is_empty() && !log.ends_with('\n') {
                log.push('\n');
            }
            log.push_str(&description(&result));
            log.push('\n');
        }
        write_text(&self.check_log, &log)?;
        Ok(result)
    }

    fn validate(&self, ctx: &Context, repairs: &Repairs) -> Result<Value, String> {
        let inputs = self.inputs(repairs)?;
        let result = self.check(repairs)?;
        write_json(
            &self.checked,
            &json!({
                "ok": result.ok, "error_signature": result.error_signature, "detail": result.detail, "input_hash": inputs,
            }),
            true,
        )?;
        let mut context = ctx.0.write().unwrap_or_else(|e| e.into_inner());
        let results = context
            .entry("verification_results")
            .or_insert_with(|| json!({}));
        results[self.checked.to_string_lossy().as_ref()] =
            json!([result.ok, result.error_signature, result.detail]);
        Ok(json!(description(&result)))
    }

    fn zip_path(&self) -> Option<PathBuf> {
        require_release_id(self.doc()["release_id"].as_str()?)
            .ok()
            .map(|s| self.dir.join(format!("{s}.zip")))
    }

    fn build(&self, repairs: &Repairs) -> Result<Value, String> {
        remove_if_present(&self.manifest)?;
        remove_if_present(&self.build_log)?;
        let checked = read_object(&self.checked)?;
        if checked.get("ok") != Some(&Value::Bool(true)) {
            return Ok(json!("Waiting for package validation"));
        }
        let inputs = self.inputs(repairs)?;
        if checked.get("input_hash").and_then(Value::as_str) != Some(&inputs) {
            write_text(
                &self.build_log,
                "Package inputs changed after validation.\n",
            )?;
            return Ok(json!("Package needs revalidation"));
        }
        let doc = Value::Object(read_object(&self.release)?);
        let id = require_release_id(doc["release_id"].as_str().unwrap_or(""))?;
        let path = self.dir.join(format!("{id}.zip"));
        let media = local_references(&doc, &self.pkg);
        let mut entries = vec![("release.json", &self.release)];
        entries.extend(media.iter().map(|(n, p)| (n.as_str(), p)));
        let build = || -> Result<(), String> {
            remove_if_present(&path)?;
            let expected: BTreeMap<_, _> = entries
                .iter()
                .map(|(name, file)| {
                    Ok((
                        *name,
                        (file.metadata().map_err(err)?.len(), sha256_file(file)?),
                    ))
                })
                .collect::<Result<_, String>>()?;
            if let Some(builder) = &self.cfg.builder_script {
                let (status, log) = run_builder(builder, "build", &self.release, Some(&path))?;
                write_text(&self.build_log, &log)?;
                if !status.success() {
                    return Err(format!("Builder exited with code {}", exit_code(status)));
                }
            } else {
                write_archive(&path, &entries)?;
            }
            verify_archive(&path, &expected)?;
            if self.inputs(repairs)? != inputs {
                return Err("Package inputs changed during ZIP creation".into());
            }
            Ok(())
        };
        if let Err(e) = build() {
            let _ = remove_if_present(&path);
            let mut log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.build_log)
                .map_err(err)?;
            writeln!(log, "\n{e}").map_err(err)?;
            return Ok(json!("ZIP build failed"));
        }
        let change_ids: Vec<_> = doc["changes"]
            .as_array()
            .ok_or("Missing changes")?
            .iter()
            .map(|c| &c["id"])
            .collect();
        write_json(
            &self.manifest,
            &json!({
                "release_id": id, "product": self.cfg.product, "zip_path": path,
                "zip_sha256": sha256_file(&path)?, "change_ids": change_ids, "media_files": media.keys().collect::<Vec<_>>(),
            }),
            true,
        )?;
        Ok(json!(format!("Built {id}.zip")))
    }

    fn build_outputs(&self) -> Vec<PathBuf> {
        let mut outputs = Vec::new();
        if let Some(zip) = self.zip_path().filter(|_| self.manifest.is_file()) {
            outputs.extend([self.manifest.clone(), zip]);
        }
        if self.build_log.is_file() {
            outputs.push(self.build_log.clone());
        }
        outputs
    }

    fn verify(&self, repairs: &Repairs) -> Result<VerifyOutcome, String> {
        let mut result = verification(&self.checked)?;
        if !result.ok {
            let log = fs::read_to_string(&self.check_log).map_err(err)?;
            if !log.is_empty() {
                result.detail = log;
            }
            return Ok(result);
        }
        if let Some(zip) = self
            .zip_path()
            .filter(|p| self.manifest.is_file() && p.is_file())
        {
            repairs.resolved()?;
            return Ok(VerifyOutcome::pass(format!(
                "Verified Version History ZIP {}",
                zip.file_name().unwrap_or_default().to_string_lossy()
            )));
        }
        let mut detail = fs::read_to_string(&self.check_log).map_err(err)?;
        if self.build_log.is_file() {
            detail.push_str(&fs::read_to_string(&self.build_log).map_err(err)?);
        }
        Ok(VerifyOutcome::fail("vh_zip_failed", detail))
    }
}

fn description(outcome: &VerifyOutcome) -> String {
    if outcome.error_signature.is_empty() {
        outcome.detail.clone()
    } else {
        format!("{}: {}", outcome.error_signature, outcome.detail)
    }
}

fn remove_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(err(e)),
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}

fn run_builder(
    builder: &Path,
    action: &str,
    release: &Path,
    output: Option<&Path>,
) -> Result<(ExitStatus, String), String> {
    let (mut reader, writer) = io::pipe().map_err(err)?;
    let mut cmd = Command::new(if cfg!(windows) { "python" } else { "python3" });
    cmd.arg(builder)
        .arg(action)
        .arg(release)
        .env("PYTHONHASHSEED", "0");
    if let Some(output) = output {
        cmd.arg("--output").arg(output);
    }
    cmd.stdout(writer.try_clone().map_err(err)?).stderr(writer);
    let mut child = cmd.spawn().map_err(err)?;
    drop(cmd);
    let mut bytes = Vec::new();
    let read = reader.read_to_end(&mut bytes);
    let status = child.wait().map_err(err)?;
    read.map_err(err)?;
    Ok((
        status,
        String::from_utf8_lossy(&bytes)
            .replace("\r\n", "\n")
            .replace('\r', "\n"),
    ))
}

fn write_archive(path: &Path, entries: &[(&str, &PathBuf)]) -> Result<(), String> {
    static DRAFT_ID: AtomicU64 = AtomicU64::new(0);
    let (draft, file) = loop {
        let candidate = path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            DRAFT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        match File::create_new(&candidate) {
            Ok(file) => break (candidate, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(err(e)),
        }
    };
    let write = || -> Result<(), String> {
        let mut archive = ZipWriter::new(file);
        for (name, source) in entries {
            let mut file = File::open(source).map_err(err)?;
            let meta = file.metadata().map_err(err)?;
            let mtime: chrono::DateTime<Local> = meta.modified().map_err(err)?.into();
            let stamp = DateTime::from_date_and_time(
                mtime.year() as u16,
                mtime.month() as u8,
                mtime.day() as u8,
                mtime.hour() as u8,
                mtime.minute() as u8,
                mtime.second() as u8,
            )
            .map_err(err)?;
            let options = SimpleFileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .compression_level(Some(6))
                .last_modified_time(stamp);
            #[cfg(unix)]
            let options = {
                use std::os::unix::fs::PermissionsExt;
                options.unix_permissions(meta.permissions().mode())
            };
            archive.start_file(*name, options).map_err(err)?;
            io::copy(&mut file, &mut archive).map_err(err)?;
        }
        archive.finish().map_err(err)?.sync_all().map_err(err)?;
        fs::rename(&draft, path).map_err(err)
    };
    let result = write();
    if result.is_err() {
        let _ = remove_if_present(&draft);
    }
    result
}

const WRONG_ENTRIES: &str =
    "ZIP entries must contain only root release.json and the referenced local media";

// ZipArchive indexes entries by name, collapsing duplicates. Count the physical central-directory
// records as well, so a builder cannot deliver two versions of the same file.
fn verify_entry_count(file: &mut File, start: u64, expected: usize) -> Result<(), String> {
    file.seek(SeekFrom::Start(start)).map_err(err)?;
    for _ in 0..expected {
        let mut head = [0u8; 46];
        file.read_exact(&mut head).map_err(err)?;
        if head[..4] != *b"PK\x01\x02" {
            return Err(WRONG_ENTRIES.into());
        }
        let variable: i64 = head[28..34]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|s| i64::from(u16::from_le_bytes(*s)))
            .sum();
        file.seek(SeekFrom::Current(variable)).map_err(err)?;
    }
    let mut signature = [0u8; 4];
    file.read_exact(&mut signature).map_err(err)?;
    if signature == *b"PK\x01\x02" {
        return Err(WRONG_ENTRIES.into());
    }
    Ok(())
}

fn verify_archive(path: &Path, expected: &BTreeMap<&str, (u64, String)>) -> Result<(), String> {
    let mut file = File::open(path).map_err(err)?;
    if file.metadata().map_err(err)?.len() > 50 * MIB {
        return Err("Compressed ZIP exceeds 50 MiB".into());
    }
    let mut archive = ZipArchive::new(file.try_clone().map_err(err)?).map_err(err)?;
    if archive.len() != expected.len() || archive.file_names().any(|s| !expected.contains_key(s)) {
        return Err(WRONG_ENTRIES.into());
    }
    verify_entry_count(&mut file, archive.central_directory_start(), expected.len())?;
    for (name, (size, digest)) in expected {
        let entry = archive.by_name(name).map_err(err)?;
        if entry.size() == *size {
            let mut bytes = Vec::new();
            entry.take(size + 1).read_to_end(&mut bytes).map_err(err)?;
            if bytes.len() as u64 == *size && sha256_bytes(&bytes) == *digest {
                continue;
            }
        }
        return Err(format!(
            "ZIP entry differs from its validated source: {name}"
        ));
    }
    Ok(())
}

pub fn create_version_history_subgraph(
    cfg: VersionHistoryConfig,
) -> Result<LoopSubgraphNode, String> {
    let builder = find_version_history_builder(cfg.builder_script.as_deref());
    create_subgraph(cfg, builder)
}

fn create_subgraph(
    cfg: VersionHistoryConfig,
    builder: Option<PathBuf>,
) -> Result<LoopSubgraphNode, String> {
    if !matches!(cfg.product.as_str(), "desktop" | "app") {
        return Err("product must be desktop or app".into());
    }
    if let Some(id) = &cfg.release_id {
        require_release_id(id)?;
    }
    let node_id = require_id(&cfg.node_id)?;
    let package = Arc::new(Package::new(cfg, builder)?);
    let source_hash: StringFn = {
        let p = package.clone();
        Arc::new(move || p.source_hash())
    };
    let artifacts = vec![package.release.clone(), package.meta.clone()];
    let repairs = Repairs::new(
        package.dir.join("repair_state.json"),
        source_hash.clone(),
        artifacts.clone(),
        Some(package.meta.clone()),
        Some({
            let p = package.clone();
            Arc::new(move || Ok(p.delivery_hash()))
        }),
        0,
    );
    let p = package.clone();
    let worker = packet_worker(PacketWorkerOptions {
        node_id: "assemble_package".into(),
        packet_path: p.dir.join("release_packet.json"),
        artifacts: Arc::new(move |_| Ok(artifacts.clone())),
        input_hash: source_hash,
        packet: Arc::new(move |_| p.packet()),
        deps: vec![],
        repairs: Some(repairs.clone()),
        enabled: None,
        verify: None,
        resource: None,
    });

    let (p, r) = (package.clone(), repairs.clone());
    let (hash_package, hash_repairs) = (package.clone(), repairs.clone());
    let mut verifier = TaskNode::new(
        "verify_package",
        TaskKind::Verifier,
        Arc::new(move |ctx, _| p.validate(ctx, &r).map_err(TaskRunError::other)),
        Arc::new(move |_| hash_package.inputs(&hash_repairs)),
    );
    verifier.deps.push(worker.id.clone());
    let outputs = vec![package.checked.clone(), package.check_log.clone()];
    verifier.outputs_fn = Arc::new(move |_| Ok(outputs.clone()));
    verifier.verify_fn = Some(Arc::new(|_, detail| {
        VerifyOutcome::pass(detail.as_str().unwrap_or(""))
    }));

    let (p, r) = (package.clone(), repairs.clone());
    let (hash_package, hash_repairs) = (package.clone(), repairs.clone());
    let mut build = TaskNode::new(
        "build_zip",
        TaskKind::Code,
        Arc::new(move |_, _| p.build(&r).map_err(TaskRunError::other)),
        Arc::new(move |_| {
            Ok(fingerprint(json!([
                hash_package.inputs(&hash_repairs)?,
                file_hash(Some(&hash_package.checked))
            ])))
        }),
    );
    build.deps.push(verifier.id.clone());
    let p = package.clone();
    build.outputs_fn = Arc::new(move |_| Ok(p.build_outputs()));
    let max = package.cfg.max_retries;
    let deps = package.cfg.deps.clone();
    let loop_repairs = repairs.clone();
    let mut node = LoopSubgraphNode::new(
        node_id,
        vec![Node::Task(worker), Node::Task(verifier), Node::Task(build)],
        Arc::new(move |_| package.verify(&loop_repairs)),
    );
    node.deps = deps;
    node.max_retries = max;
    // LoopRepairFn fixes the shared GraphFailure return type.
    #[allow(clippy::result_large_err)]
    let repair_fn: kgraph_core::types::LoopRepairFn = Arc::new(move |ctx, _, sig, detail| {
        repairs.repair(
            ctx,
            sig,
            detail,
            &["assemble_package", "verify_package", "build_zip"],
            max,
        )
    });
    node.repair_fn = Some(repair_fn);
    Ok(with_retry_route(
        node,
        "verify_package",
        "assemble_package",
        "if fails → repair",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraph_core::engine::{GraphRunner, RunnerConfig};
    use tempfile::TempDir;

    // Two 1x1 RGB PNGs with different pixel values, including valid IDAT CRCs.
    fn png(after: bool) -> Vec<u8> {
        let idat = if after {
            "6338615e0d000345017b7dd3f70e"
        } else {
            "634831af060002190117e9869b71"
        };
        let hex = format!("89504e470d0a1a0a0000000d4948445200000001000000010802000000907753de0000000c49444154789c{idat}0000000049454e44ae426082");
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    fn author(pkg: &Path, revision: u64) -> Value {
        let doc = json!({
            "schema_version": 2, "product": "desktop", "release_id": "desktop-42", "version": "1.0.42",
            "title": "Desktop maintenance", "date": "2026-11-23", "stage": "source", "summary": "Fixed saved settings.",
            "changes": [{"id": "app-42", "type": "fix", "title": "Restore desktop settings",
                "description": "Reopening settings uses the saved value.", "verification": "The local regression test passed on macOS.",
                "evidence": [
                    {"label": "Before", "kind": "image", "stage": "before", "url": "media/before.png"},
                    {"label": "After", "kind": "image", "stage": "after", "url": "media/after.png"},
                    {"label": "Video", "kind": "video", "stage": "after", "url": "https://example.org/demo.mp4"}
                ]
            }]
        });
        fs::create_dir_all(pkg.join("media")).unwrap();
        fs::write(pkg.join("media/before.png"), png(false)).unwrap();
        fs::write(pkg.join("media/after.png"), png(true)).unwrap();
        fs::write(pkg.join("unreferenced.txt"), "Exclude this file").unwrap();
        write_json(&pkg.join("release.json"), &doc, true).unwrap();
        write_json(
            &pkg.join("author_meta.json"),
            &json!({"revision": revision, "release_id": "desktop-42"}),
            true,
        )
        .unwrap();
        doc
    }

    fn sig(doc: &Value, pkg: &Path) -> String {
        preflight(doc, pkg, "desktop", Some("desktop-42")).error_signature
    }

    fn config(root: &Path) -> VersionHistoryConfig {
        VersionHistoryConfig {
            work_dir: root.into(),
            release_id: Some("desktop-42".into()),
            ..Default::default()
        }
    }

    #[test]
    fn release_ids_and_config() {
        for invalid in [
            "",
            "_ok",
            "Upper",
            "ok/ok",
            "con.2",
            "lpt1",
            "com5.zip",
            &"a".repeat(81),
        ] {
            assert!(require_release_id(invalid).is_err(), "{invalid}");
        }
        for valid in ["v1.2-rc_2", "com10.2", &"9".repeat(80)] {
            assert_eq!(require_release_id(valid).unwrap(), valid);
        }
        let root = TempDir::new().unwrap();
        assert!(create_version_history_subgraph(VersionHistoryConfig {
            product: "mobile".into(),
            ..config(root.path())
        })
        .is_err());
        assert!(create_version_history_subgraph(VersionHistoryConfig {
            node_id: "not/valid".into(),
            ..config(root.path())
        })
        .is_err());
    }

    #[test]
    fn rejects_manifest_fields_and_invalid_changes() {
        let root = TempDir::new().unwrap();
        let pkg = resolve(root.path()).unwrap();
        let good = author(&pkg, 0);
        assert_eq!(sig(&good, &pkg), "");
        for (field, value, signature) in [
            ("platforms", Value::Null, "forbidden_platforms_field"),
            ("revision", json!(0), "unsupported_release_field"),
            ("product", json!("app"), "product_mismatch"),
            ("release_id", json!("nul.1"), "invalid_release_id"),
            ("release_id", json!("v1"), "release_id_mismatch"),
            ("changes", json!([]), "missing_changes"),
            ("changes", json!([{"id": " "}]), "invalid_change_id"),
            (
                "changes",
                json!([good["changes"][0], good["changes"][0]]),
                "invalid_change_id",
            ),
            (
                "changes",
                json!([{"id": "a", "evidence": null}]),
                "invalid_evidence",
            ),
            (
                "changes",
                json!([{"id": "a"}]),
                "missing_before_after_media",
            ),
        ] {
            let mut doc = good.clone();
            doc[field] = value;
            assert_eq!(sig(&doc, &pkg), signature);
        }
    }

    #[test]
    fn checks_paths_urls_extensions_and_image_contents() {
        let root = TempDir::new().unwrap();
        let pkg = resolve(root.path()).unwrap().join("package");
        let mut doc = author(&pkg, 0);
        for url in [
            "",
            "../outside.png",
            "/absolute.png",
            "./media/before.png",
            "media//before.png",
            "media/before.png/",
            "media\\before.png",
            "unsafe\n.mov",
            "a:b.mov",
            "RELEASE.json",
            "photo.gif",
            "https:/no-host",
            "https:///empty",
            "https://user@host/path",
            "https://user:secret@host",
            "http://host/video.mp4",
        ] {
            doc["changes"][0]["evidence"][2]["url"] = json!(url);
            assert_eq!(sig(&doc, &pkg), "invalid_media", "{url}");
        }
        #[cfg(unix)]
        {
            fs::write(root.path().join("outside.mov"), [1]).unwrap();
            std::os::unix::fs::symlink(root.path().join("outside.mov"), pkg.join("link.mov"))
                .unwrap();
            doc["changes"][0]["evidence"][2]["url"] = json!("link.mov");
            assert_eq!(sig(&doc, &pkg), "invalid_media");
        }
        doc["changes"][0]["evidence"][2]["url"] = json!("https://example.org/a");
        doc["changes"][0]["evidence"][0]["url"] = json!("https://example.org/before.png");
        assert_eq!(sig(&doc, &pkg), "missing_before_after_media");
        doc["changes"][0]["evidence"][0]["url"] = json!("media/before.png");
        let after = pkg.join("media/after.png");
        fs::write(&after, png(false)).unwrap();
        assert_eq!(sig(&doc, &pkg), "duplicate_before_after_media");
        fs::write(&after, b"fake PNG").unwrap();
        assert_eq!(sig(&doc, &pkg), "invalid_media");
        File::create(&after).unwrap().set_len(25 * MIB + 1).unwrap();
        assert_eq!(sig(&doc, &pkg), "invalid_media");
        fs::write(&after, []).unwrap();
        assert_eq!(sig(&doc, &pkg), "invalid_media");
    }

    #[test]
    fn enforces_casefold_uniqueness_and_package_limits() {
        let root = TempDir::new().unwrap();
        let pkg = resolve(root.path()).unwrap();
        let mut doc = author(&pkg, 0);
        let evidence = |name: &str| json!({"kind": "video", "url": name});
        for name in ["straße.mov", "STRASSE.MOV"] {
            fs::write(pkg.join(name), [0]).unwrap();
            doc["changes"][0]["evidence"]
                .as_array_mut()
                .unwrap()
                .push(evidence(name));
        }
        assert_eq!(sig(&doc, &pkg), "duplicate_media_path");
        let mut doc = author(&pkg, 0);
        let items = doc["changes"][0]["evidence"].as_array_mut().unwrap();
        for i in 0..148 {
            let name = format!("{i}.mov");
            fs::write(pkg.join(&name), [0]).unwrap();
            items.push(evidence(&name));
        }
        assert_eq!(sig(&doc, &pkg), "package_too_large");
        doc["changes"][0]["evidence"].as_array_mut().unwrap().pop();
        assert_eq!(sig(&doc, &pkg), "");
        for i in 0..4 {
            File::create(pkg.join(format!("{i}.mov")))
                .unwrap()
                .set_len(25 * MIB)
                .unwrap();
        }
        assert_eq!(sig(&doc, &pkg), "package_too_large");
        let doc = author(&pkg, 0);
        File::create(pkg.join("release.json"))
            .unwrap()
            .set_len(256 * 1024 + 1)
            .unwrap();
        assert_eq!(sig(&doc, &pkg), "package_too_large");
    }

    fn assert_archive(dir: &Path) -> String {
        let path = dir.join("desktop-42.zip");
        let digest = sha256_bytes(&fs::read(&path).unwrap());
        assert_eq!(
            Value::Object(read_object(&dir.join("package_manifest.json")).unwrap()),
            json!({
                "release_id": "desktop-42", "product": "desktop", "zip_path": path, "zip_sha256": digest,
                "change_ids": ["app-42"], "media_files": ["media/after.png","media/before.png"]
            })
        );
        let mut zip = ZipArchive::new(File::open(path).unwrap()).unwrap();
        assert_eq!(zip.len(), 3);
        for name in ["media/after.png", "media/before.png", "release.json"] {
            let mut entry = zip.by_name(name).unwrap();
            assert_eq!(entry.compression(), CompressionMethod::Deflated);
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            assert_eq!(data, fs::read(dir.join("package").join(name)).unwrap());
        }
        digest
    }

    #[test]
    fn runner_pauses_repairs_builds_and_caches() {
        let root = TempDir::new().unwrap();
        let dir = resolve(root.path()).unwrap().join("version_history");
        let pkg = dir.join("package");
        let run = |budget| {
            let cfg = RunnerConfig {
                max_worker_calls: budget,
                ..RunnerConfig::new(root.path().join("state.json"))
            };
            let runner = GraphRunner::new(cfg).unwrap();
            let node = Node::LoopSubgraph(create_subgraph(config(root.path()), None).unwrap());
            runner
                .run(&[node], &Context::new(), "")
                .map(|_| runner)
                .map_err(Box::new)
        };
        let failure = run(None).err().unwrap();
        assert!(failure.awaiting_subagents);
        let packet_path = PathBuf::from(failure.packet_path.unwrap());
        let packet = read_object(&packet_path).unwrap();
        assert_eq!(packet["stage"], "author_release");
        assert_eq!(packet["revision"], 0);
        assert_eq!(packet["builder_script"], Value::Null);

        author(&pkg, 0);
        // A boolean revision is not an authoring integer.
        write_json(
            &pkg.join("author_meta.json"),
            &json!({"release_id": "desktop-42", "revision": true}),
            true,
        )
        .unwrap();
        assert!(run(None).err().unwrap().awaiting_subagents);
        let repair = read_object(&packet_path).unwrap();
        assert_eq!(repair["revision"], 1);
        assert_eq!(
            repair["repair_feedback"][0]["error_signature"],
            "invalid_author_meta"
        );
        assert_eq!(
            repair["repair_feedback"][0]["detail"],
            fs::read_to_string(dir.join("check.log")).unwrap()
        );
        assert!(!dir.join("package_manifest.json").exists());
        author(&pkg, 0);
        assert!(run(None).err().unwrap().reason.contains("revision 1"));
        author(&pkg, 1);
        fs::write(dir.join("desktop-42.zip"), b"stale output").unwrap();
        run(None).unwrap();
        let digest = assert_archive(&dir);
        let cached = run(Some(0)).unwrap();
        assert_eq!(
            cached
                .trace()
                .iter()
                .filter(|e| e["status"] == "cached")
                .count(),
            3
        );
        assert_eq!(cached.worker_calls_used(), 0);
        assert_eq!(sha256_file(&dir.join("desktop-42.zip")).unwrap(), digest);
        // Replacing media alone invalidates both verification and the ZIP.
        fs::write(pkg.join("media/before.png"), png(true)).unwrap();
        fs::write(pkg.join("media/after.png"), png(false)).unwrap();
        run(Some(0)).unwrap();
        assert_ne!(assert_archive(&dir), digest);
    }

    #[test]
    fn revalidates_inputs_changed_after_check_and_caps_compressed_zip() {
        let root = TempDir::new().unwrap();
        let dir = resolve(root.path()).unwrap().join("version_history");
        let pkg = dir.join("package");
        author(&pkg, 0);
        let node = create_subgraph(config(root.path()), None).unwrap();
        let ctx = Context::new();
        let Node::Task(check) = &node.nodes[1] else {
            panic!("expected verifier")
        };
        let Node::Task(build) = &node.nodes[2] else {
            panic!("expected build stage")
        };
        (check.run_fn)(&ctx, 1).unwrap();
        assert!(read_object(&dir.join("check_result.json")).unwrap()["ok"]
            .as_bool()
            .unwrap());
        fs::write(pkg.join("media/after.png"), png(false)).unwrap();
        assert_eq!(
            (build.run_fn)(&ctx, 1).unwrap(),
            "Package needs revalidation"
        );
        assert_eq!(
            fs::read_to_string(dir.join("build.log")).unwrap(),
            "Package inputs changed after validation.\n"
        );
        assert!(!dir.join("package_manifest.json").exists());
        let path = dir.join("oversized.zip");
        File::create(&path).unwrap().set_len(50 * MIB + 1).unwrap();
        assert_eq!(
            verify_archive(&path, &BTreeMap::new()).unwrap_err(),
            "Compressed ZIP exceeds 50 MiB"
        );
    }

    #[test]
    fn external_builder_and_post_build_checks() {
        let root = TempDir::new().unwrap();
        let dir = resolve(root.path()).unwrap().join("version_history");
        let pkg = dir.join("package");
        author(&pkg, 0);
        let builder = root.path().join("builder.py");
        let run = |mode: &str| {
            let script = format!(
                r#"
from pathlib import Path
from zipfile import ZipFile, ZIP_DEFLATED
import os, sys
assert os.environ['PYTHONHASHSEED'] == '0'
action, release = sys.argv[1], Path(sys.argv[2])
mode = '{mode}'
if action == 'check':
    print('Validated ' + str(release))
    sys.exit(1 if mode == 'check_failure' else 0)
assert action == 'build' and sys.argv[3] == '--output'
with ZipFile(sys.argv[4], 'w', ZIP_DEFLATED) as archive:
    for name in ['release.json','media/before.png','media/after.png']:
        if mode == 'bad_bytes' and name == 'media/after.png':
            archive.writestr(name, (release.parent / 'media/before.png').read_bytes())
        else:
            archive.write(release.parent / name, name)
    if mode == 'extra': archive.writestr('extra', b'x')
    if mode == 'duplicate': archive.write(release, 'release.json')
if mode == 'changed': (release.parent / 'author_meta.json').write_text('{{}}')
print('Built release', file=sys.stderr)
"#
            );
            fs::write(&builder, script).unwrap();
            let cfg = VersionHistoryConfig {
                builder_script: Some(builder.clone()),
                max_retries: 0,
                ..config(root.path())
            };
            let node = Node::LoopSubgraph(create_version_history_subgraph(cfg).unwrap());
            let runner =
                GraphRunner::new(RunnerConfig::new(root.path().join("state.json"))).unwrap();
            runner.run(&[node], &Context::new(), "").map_err(Box::new)
        };
        run("ok").unwrap();
        assert_archive(&dir);
        assert!(fs::read_to_string(dir.join("build.log"))
            .unwrap()
            .contains("Built release"));
        // Every script revision is a fresh source input. Acknowledge it in the delivery.
        for (mode, message) in [
            ("extra", WRONG_ENTRIES),
            ("duplicate", WRONG_ENTRIES),
            ("bad_bytes", "ZIP entry differs"),
            ("changed", "Package inputs changed during ZIP creation"),
            ("check_failure", "Validated "),
        ] {
            write_json(
                &pkg.join("author_meta.json"),
                &json!({"revision": 1, "release_id": "desktop-42", "note": mode}),
                true,
            )
            .unwrap();
            let failure = run(mode).unwrap_err();
            assert!(failure.reason.contains(message), "{}", failure.reason);
            assert!(!dir.join("package_manifest.json").exists());
            assert!(!dir.join("desktop-42.zip").exists());
            if mode == "check_failure" {
                let checked = read_object(&dir.join("check_result.json")).unwrap();
                let expected = format!(
                    "Validated <package>{}release.json\n",
                    std::path::MAIN_SEPARATOR
                );
                let digest = fingerprint(json!([expected]));
                assert_eq!(
                    checked["error_signature"],
                    format!("vh_check_failed:{}", &digest[..8])
                );
            }
        }
    }

    #[test]
    fn matches_python_reference_package() {
        let reference = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src");
        if !reference
            .join("kgraph_runner/subgraphs/version_history.py")
            .is_file()
        {
            return;
        }
        // Exercise the fallback everywhere, plus the installed skill builder when available.
        for builder in std::iter::once(None).chain(find_version_history_builder(None).map(Some)) {
            let root = TempDir::new().unwrap();
            let root_path = {
                let canon = root.path().canonicalize().unwrap();
                #[cfg(windows)]
                {
                    canon
                        .to_str()
                        .and_then(|s| s.strip_prefix(r"\\?\"))
                        .map(PathBuf::from)
                        .unwrap_or(canon)
                }
                #[cfg(not(windows))]
                {
                    canon
                }
            };
            let dir = resolve(&root_path).unwrap().join("version_history");
            let cfg = config(&root_path);
            let node = Node::LoopSubgraph(create_subgraph(cfg, builder.clone()).unwrap());
            let runner =
                GraphRunner::new(RunnerConfig::new(root_path.join("rust_state.json"))).unwrap();
            assert!(
                runner
                    .run(std::slice::from_ref(&node), &Context::new(), "")
                    .unwrap_err()
                    .awaiting_subagents
            );
            author(&dir.join("package"), 0);
            runner.run(&[node], &Context::new(), "").unwrap();
            assert_archive(&dir);
            let script = r#"
import hashlib, json, sys
from pathlib import Path
from unittest.mock import patch
from zipfile import ZipFile
from kgraph_runner.subgraphs.version_history import create_version_history_subgraph
from kgraph_runner.engine import GraphRunner, GraphFailure
root = Path(sys.argv[1])
directory = root / 'version_history'
builder = Path(sys.argv[2]) if sys.argv[2] else None
def read(name):
    return json.loads((directory / name).read_bytes())
def members():
    with ZipFile(directory / 'desktop-42.zip') as archive:
        # The skill writes its manifest with the current wall-clock time.
        return [(e.filename, e.date_time if builder is None else None, e.compress_type,
                 e.CRC, archive.read(e)) for e in archive.infolist()]
packet = read('release_packet.json')
checked = read('check_result.json')
manifest = read('package_manifest.json')
archive = members()
with patch('kgraph_runner.subgraphs.version_history.find_version_history_builder', return_value=builder):
    loop = create_version_history_subgraph(work_dir=root, release_id='desktop-42', builder_script=builder)
runner = GraphRunner(root / 'python_state.json', logger=lambda _: None)
meta_file = directory / 'package/author_meta.json'
meta = meta_file.read_bytes()
meta_file.unlink()
try:
    runner.run([loop], {})
except GraphFailure as error:
    assert error.awaiting_subagents
else:
    raise AssertionError('expected author packet')
assert packet == read('release_packet.json')
meta_file.write_bytes(meta)
runner.run([loop], {})
assert checked == read('check_result.json')
assert archive == members()
built = read('package_manifest.json')
assert built.pop('zip_sha256') == hashlib.sha256((directory / 'desktop-42.zip').read_bytes()).hexdigest()
manifest.pop('zip_sha256')
assert manifest == built
"#;
            let output = Command::new(if cfg!(windows) { "python" } else { "python3" })
                .arg("-c")
                .arg(script)
                .arg(&root_path)
                .arg(builder.as_deref().unwrap_or(Path::new("")))
                .env("PYTHONDONTWRITEBYTECODE", "1")
                .env("PYTHONPATH", &reference)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
