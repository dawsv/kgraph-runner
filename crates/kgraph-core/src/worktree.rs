//! Isolated Git working directories with explicit patch and fast-forward integration.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::{tempdir, NamedTempFile};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergePolicy {
    None,
    Patch,
    FastForward,
}

impl MergePolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "patch" => Some(Self::Patch),
            "fast_forward" | "fast-forward" => Some(Self::FastForward),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorktreeSpec {
    pub repo_root: PathBuf,
    /// Relative paths are resolved against repo_root.
    pub worktree_base: PathBuf,
    pub node_path: String,
    pub base_ref: Option<String>,
    pub branch_prefix: Option<String>,
    pub keep_on_failure: bool,
    pub merge_policy: MergePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeResult {
    pub worktree_path: PathBuf,
    pub branch: Option<String>,
    pub base_commit: String,
    pub changed_files: Vec<String>,
    pub patch_path: Option<PathBuf>,
    pub merged: bool,
}

// Git for Windows rejects verbatim `\\?\C:\...` prefixes from `Path::canonicalize()`.
fn git_arg(path: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(s) = path.to_str() {
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            let b = rest.as_bytes();
            if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
                return PathBuf::from(rest);
            }
        }
    }
    path.to_owned()
}

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(git_arg(dir))
        .stdin(Stdio::null())
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    cmd
}

fn run(cmd: &mut Command) -> Result<Vec<u8>, String> {
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            cmd.get_args().next().unwrap_or_default().to_string_lossy(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

fn os_path(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
    }
}

fn git_path(dir: &Path, args: &[&str]) -> Result<PathBuf, String> {
    let bytes = run(git(dir).args(args))?;
    Ok(os_path(bytes.strip_suffix(b"\n").unwrap_or(&bytes)))
}

fn commit(dir: &Path, reference: &str) -> Result<String, String> {
    let bytes = run(git(dir).args([
        "rev-parse",
        "--verify",
        "--end-of-options",
        &format!("{reference}^{{commit}}"),
    ]))?;
    Ok(String::from_utf8(bytes)
        .map_err(|e| e.to_string())?
        .trim_end()
        .into())
}

fn slug(node: &str) -> String {
    let mut stem: String = node
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    stem = stem.trim_matches('-').to_owned();
    if stem == node && !stem.is_empty() {
        return stem;
    }
    if stem.is_empty() {
        stem.push_str("node");
    }
    format!(
        "{stem}-{}",
        &hex::encode(Sha256::digest(node.as_bytes()))[..16]
    )
}

// Keep lock files in Git metadata. Unlinking a lock file on release can give two
// processes separate inodes for the same path while one still holds the lock.
fn lock_file(common: &Path, role: &str, path: &Path) -> Result<File, String> {
    let locks = common.join("kgraph-locks");
    fs::create_dir_all(&locks).map_err(|e| e.to_string())?;
    let key = hex::encode(Sha256::digest(path.as_os_str().as_encoded_bytes()));
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(locks.join(format!("{role}-{key}")))
        .map_err(|e| e.to_string())
}

// Git deletes this private directory with the registration. A manually recreated
// worktree at the same checkout path therefore cannot inherit an old marker.
fn marker_path(path: &Path, common: &Path) -> Result<PathBuf, String> {
    let admin = git_path(path, &["rev-parse", "--absolute-git-dir"])?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if admin.parent() != Some(common.join("worktrees").as_path())
        || admin
            .join(git_path_from_file(&admin.join("gitdir"))?)
            .canonicalize()
            .map_err(|e| e.to_string())?
            != path.join(".git")
    {
        return Err(format!(
            "unexpected Git registration for {}",
            path.display()
        ));
    }
    Ok(admin.join("kgraph-owner"))
}

fn git_path_from_file(file: &Path) -> Result<PathBuf, String> {
    let bytes = fs::read(file).map_err(|e| e.to_string())?;
    Ok(os_path(bytes.strip_suffix(b"\n").unwrap_or(&bytes)))
}

fn owner(root: &Path, path: &Path, node: &str) -> Vec<u8> {
    [
        root.as_os_str().as_encoded_bytes(),
        path.as_os_str().as_encoded_bytes(),
        node.as_bytes(),
    ]
    .join(&0)
}

fn require_owner(root: &Path, path: &Path, common: &Path, node: &str) -> Result<(), String> {
    let marker = marker_path(path, common)
        .ok()
        .and_then(|file| fs::read(file).ok());
    if marker.as_deref() != Some(owner(root, path, node).as_slice()) {
        return Err(format!(
            "refusing to remove worktree without a matching KGraph ownership marker: {}",
            path.display()
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub struct IsolatedWorktree {
    spec: WorktreeSpec,
    path: PathBuf,
    common: PathBuf,
    branch: Option<String>,
    base_commit: String,
    _lease: File,
    cleanup: bool,
}

impl IsolatedWorktree {
    pub fn create(mut spec: WorktreeSpec) -> Result<Self, String> {
        let root = spec.repo_root.canonicalize().map_err(|e| e.to_string())?;
        if git_path(&root, &["rev-parse", "--show-toplevel"])?
            .canonicalize()
            .map_err(|e| e.to_string())?
            != root
        {
            return Err("repo_root must be the Git worktree root".into());
        }
        let base_commit = commit(&root, spec.base_ref.as_deref().unwrap_or("HEAD"))?;
        let common = git_path(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?
        .canonicalize()
        .map_err(|e| e.to_string())?;
        let base = root.join(&spec.worktree_base);
        fs::create_dir_all(&base).map_err(|e| e.to_string())?;
        let name = slug(&spec.node_path);
        let path = base.canonicalize().map_err(|e| e.to_string())?.join(&name);
        if root.starts_with(&path) || path.starts_with(&common) || path.is_symlink() {
            return Err(format!("unsafe worktree path {}", path.display()));
        }
        let lease = lock_file(&common, "worker", &path)?;
        FileExt::try_lock_exclusive(&lease)
            .map_err(|e| format!("worktree is busy: {}: {e}", path.display()))?;
        let branch = spec
            .branch_prefix
            .as_ref()
            .map(|prefix| format!("{prefix}/{name}"));
        if let Some(b) = &branch {
            if b.starts_with('-') {
                return Err("invalid branch prefix".into());
            }
            run(git(&root).args(["check-ref-format", &format!("refs/heads/{b}")]))?;
        }
        let mut add = git(&root);
        let worktree_arg = git_arg(&path);
        add.args(["worktree", "add"]);
        if let Some(b) = &branch {
            // Named branches are retained so exported commits remain reachable.
            let reference = format!("refs/heads/{b}");
            if let Ok(tip) = commit(&root, &reference) {
                if tip != base_commit {
                    return Err(format!("existing branch {b} differs from requested base"));
                }
                add.arg("--").arg(&worktree_arg).arg(b);
            } else {
                add.args(["-b", b, "--"]).arg(&worktree_arg).arg(&base_commit);
            }
        } else {
            add.args(["--detach", "--"])
                .arg(&worktree_arg)
                .arg(&base_commit);
        }
        let listing = run(git(&root).args(["worktree", "list", "--porcelain", "-z"]))?;
        let registered = listing
            .split(|b| *b == 0)
            .filter_map(|line| line.strip_prefix(b"worktree "))
            .any(|p| {
                let registered = os_path(p);
                registered == path || registered.canonicalize().is_ok_and(|p| p == path)
            });
        if registered {
            require_owner(&root, &path, &common, &spec.node_path)?;
            run(git(&root)
                .args(["worktree", "remove", "--force"])
                .arg(&worktree_arg))?;
        } else if path.try_exists().map_err(|e| e.to_string())? {
            return Err(format!(
                "path is not an owned Git worktree: {}",
                path.display()
            ));
        }
        run(&mut add)?;
        let marker = marker_path(&path, &common)?;
        let owner = owner(&root, &path, &spec.node_path);
        File::create_new(&marker)
            .and_then(|mut file| file.write_all(&owner))
            .map_err(|e| format!("writing ownership marker {}: {e}", marker.display()))?;
        spec.repo_root = root;
        Ok(Self {
            spec,
            path,
            common,
            branch,
            base_commit,
            _lease: lease,
            cleanup: true,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Snapshot the final contents (including non-ignored untracked files) relative
    /// to the starting commit. The worker's index is never staged or reset.
    pub fn capture_changes(&self, patch_out: Option<&Path>) -> Result<WorktreeResult, String> {
        Ok(self.snapshot(patch_out, false)?.0)
    }

    fn snapshot(
        &self,
        patch_out: Option<&Path>,
        for_patch_merge: bool,
    ) -> Result<(WorktreeResult, Option<File>), String> {
        let scratch = tempdir().map_err(|e| e.to_string())?;
        let index = scratch.path().join("index");
        let current = git_path(
            &self.path,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?;
        if current.try_exists().map_err(|e| e.to_string())? {
            fs::copy(current, &index).map_err(|e| e.to_string())?;
        }
        let private_git = || {
            let mut cmd = git(&self.path);
            cmd.env("GIT_INDEX_FILE", &index);
            cmd
        };
        run(private_git().args(["add", "--all", "--", "."]))?;
        let diff = || {
            let mut cmd = private_git();
            cmd.args([
                "diff",
                "--cached",
                "--no-renames",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
            ]);
            cmd
        };
        let files = run(diff().args(["--name-only", "-z", &self.base_commit, "--"]))?;
        let changed_files: Vec<_> = files
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        let mut patch_path = None;
        let mut captured = None;
        if !changed_files.is_empty() && (patch_out.is_some() || for_patch_merge) {
            let tmp = if let Some(out) = patch_out {
                let parent = out
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                let parent = parent.canonicalize().map_err(|e| e.to_string())?;
                if parent.starts_with(&self.path) {
                    return Err("export patches outside the disposable worktree".into());
                }
                NamedTempFile::new_in(parent)
            } else {
                NamedTempFile::new()
            }
            .map_err(|e| e.to_string())?;
            let mut patch = diff();
            patch
                .args([
                    "--binary",
                    "--full-index",
                    "--src-prefix=a/",
                    "--dst-prefix=b/",
                    &self.base_commit,
                    "--",
                ])
                .stdout(Stdio::from(tmp.reopen().map_err(|e| e.to_string())?));
            run(&mut patch)?;
            captured = Some(if let Some(out) = patch_out {
                patch_path = Some(out.into());
                tmp.persist(out).map_err(|e| e.to_string())?
            } else {
                tmp.into_file()
            });
        }
        Ok((
            WorktreeResult {
                worktree_path: self.path.clone(),
                branch: self.branch.clone(),
                base_commit: self.base_commit.clone(),
                changed_files,
                patch_path,
                merged: false,
            },
            captured,
        ))
    }

    fn integrate(&self, result: &mut WorktreeResult, captured: Option<File>) -> Result<(), String> {
        let root = &self.spec.repo_root;
        let writer = lock_file(&self.common, "merge", root)?;
        FileExt::lock_exclusive(&writer).map_err(|e| e.to_string())?;
        match self.spec.merge_policy {
            MergePolicy::Patch => {
                // Another worker can atomically replace the same export path before
                // integration. Apply this snapshot using its original open file.
                if let Some(patch) = captured {
                    run(git(root)
                        .args(["apply", "--binary", "--whitespace=nowarn", "-"])
                        .stdin(Stdio::from(patch)))?;
                    result.merged = true;
                }
            }
            MergePolicy::FastForward => {
                if !run(git(&self.path).args([
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--untracked-files=all",
                ]))?
                .is_empty()
                {
                    return Err("fast-forward requires committed worker changes".into());
                }
                let tip = commit(&self.path, "HEAD")?;
                run(git(root).args(["merge-base", "--is-ancestor", "HEAD", &tip])).map_err(
                    |_| "destination HEAD cannot fast-forward to the worker commit".to_string(),
                )?;
                run(git(root).args(["merge", "--ff-only", "--no-edit", &tip]))?;
                result.merged = true;
            }
            MergePolicy::None => (),
        }
        Ok(())
    }

    pub fn finish(
        mut self,
        succeeded: bool,
        patch_out: Option<&Path>,
    ) -> Result<WorktreeResult, String> {
        let operation = (|| {
            let integrate = succeeded && self.spec.merge_policy != MergePolicy::None;
            let (mut result, captured) = self.snapshot(
                patch_out,
                integrate && self.spec.merge_policy == MergePolicy::Patch,
            )?;
            if integrate {
                self.integrate(&mut result, captured)?;
            }
            Ok(result)
        })();
        if (!succeeded || operation.is_err()) && self.spec.keep_on_failure {
            self.cleanup = false;
        } else {
            self.remove()?;
        }
        operation
    }

    fn remove(&mut self) -> Result<(), String> {
        require_owner(
            &self.spec.repo_root,
            &self.path,
            &self.common,
            &self.spec.node_path,
        )?;
        run(git(&self.spec.repo_root)
            .args(["worktree", "remove", "--force"])
            .arg(git_arg(&self.path)))?;
        self.cleanup = false;
        Ok(())
    }
}

impl Drop for IsolatedWorktree {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = self.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    struct Fixture {
        dir: TempDir,
        repo: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempdir().unwrap();
            let repo = dir.path().join("repo");
            fs::create_dir(&repo).unwrap();
            run(git(&repo).args(["init", "-q"])).unwrap();
            for (key, value) in [
                ("user.name", "Isolation Test"),
                ("user.email", "test@example.invalid"),
                ("commit.gpgSign", "false"),
                ("core.hooksPath", "/dev/null"),
                ("core.autocrlf", "false"),
            ] {
                run(git(&repo).args(["config", "--local", key, value])).unwrap();
            }
            fs::write(repo.join("shared.txt"), "original\n").unwrap();
            run(git(&repo).args(["add", "shared.txt"])).unwrap();
            run(git(&repo).args(["commit", "-qm", "fixture"])).unwrap();
            Self { dir, repo }
        }

        fn spec(&self, node: &str, policy: MergePolicy) -> WorktreeSpec {
            WorktreeSpec {
                repo_root: self.repo.clone(),
                worktree_base: self.dir.path().join("workers"),
                node_path: node.into(),
                base_ref: None,
                branch_prefix: None,
                keep_on_failure: false,
                merge_policy: policy,
            }
        }

        fn count(&self) -> usize {
            run(git(&self.repo).args(["worktree", "list", "--porcelain", "-z"]))
                .unwrap()
                .split(|b| *b == 0)
                .filter(|b| b.starts_with(b"worktree "))
                .count()
        }
    }

    #[test]
    fn in_memory_run_captures_git_head_and_dirty_status() {
        use crate::engine::{GraphRunner, RunnerConfig};
        const EXPECTED_HEAD: &str = "KGRAPH_TEST_PROVENANCE_HEAD";
        if let Ok(head) = std::env::var(EXPECTED_HEAD) {
            // The subprocess cwd is a throwaway repo. Keep runner state outside it.
            let state = tempdir().unwrap();
            let runner =
                GraphRunner::new(RunnerConfig::new(state.path().join("state.json"))).unwrap();
            for dirty in [false, true] {
                if dirty {
                    fs::write("untracked", "new").unwrap();
                }
                runner.run(&[], &crate::Context::new(), "").unwrap();
                let run = runner.build_run_telemetry("passed", 0, None);
                assert_eq!(run.git_head.as_deref(), Some(head.as_str()));
                assert_eq!(run.git_dirty, Some(dirty));
            }
            return;
        }
        let f = Fixture::new();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worktree::tests::in_memory_run_captures_git_head_and_dirty_status",
            ])
            .current_dir(&f.repo)
            .env(EXPECTED_HEAD, commit(&f.repo, "HEAD").unwrap())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
    }

    #[test]
    fn registered_foreign_worktree_keeps_its_uncommitted_work() {
        let f = Fixture::new();
        let spec = f.spec("collision", MergePolicy::None);
        let path = spec.worktree_base.join("collision");
        fs::create_dir_all(&spec.worktree_base).unwrap();
        run(git(&f.repo)
            .args(["worktree", "add", "--detach"])
            .arg(&path))
        .unwrap();
        fs::write(path.join("shared.txt"), "user edits").unwrap();
        assert!(IsolatedWorktree::create(spec)
            .unwrap_err()
            .contains("ownership marker"));
        assert_eq!(
            fs::read_to_string(path.join("shared.txt")).unwrap(),
            "user edits"
        );
        assert_eq!(f.count(), 2);
    }

    #[test]
    fn parallel_workers_edit_same_filename_without_collision() {
        let f = Fixture::new();
        let barrier = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for name in ["left", "right"] {
                let tree = IsolatedWorktree::create(f.spec(name, MergePolicy::None)).unwrap();
                let barrier = barrier.clone();
                handles.push(scope.spawn(move || {
                    fs::write(tree.path().join("shared.txt"), name).unwrap();
                    barrier.wait();
                    assert_eq!(
                        fs::read_to_string(tree.path().join("shared.txt")).unwrap(),
                        name
                    );
                    let result = tree.finish(true, None).unwrap();
                    assert_eq!(result.changed_files, ["shared.txt"]);
                    assert!(!result.merged && !result.worktree_path.exists());
                }));
            }
            for handle in handles {
                handle.join().unwrap();
            }
        });
        assert_eq!(
            fs::read_to_string(f.repo.join("shared.txt")).unwrap(),
            "original\n"
        );
        assert_eq!(f.count(), 1);
    }

    #[test]
    fn patch_export_preserves_worker_index_and_includes_binary_untracked_data() {
        let f = Fixture::new();
        let tree = IsolatedWorktree::create(f.spec("split/a", MergePolicy::None)).unwrap();
        fs::write(tree.path().join("shared.txt"), "intermediate\n").unwrap();
        run(git(tree.path()).args(["add", "shared.txt"])).unwrap();
        fs::write(tree.path().join("shared.txt"), "final\n").unwrap();
        fs::write(tree.path().join("new binary"), [0, 255, 7]).unwrap();
        let before = run(git(tree.path()).args(["status", "--porcelain=v1", "-z"])).unwrap();
        let patch = f.dir.path().join("artifacts/result.patch");
        let result = tree.capture_changes(Some(&patch)).unwrap();
        assert_eq!(
            before,
            run(git(tree.path()).args(["status", "--porcelain=v1", "-z"])).unwrap()
        );
        assert_eq!(result.changed_files, ["new binary", "shared.txt"]);
        assert_eq!(result.patch_path.as_deref(), Some(patch.as_path()));
        drop(tree);
        run(git(&f.repo).args(["apply", "--"]).arg(patch)).unwrap();
        assert_eq!(fs::read(f.repo.join("new binary")).unwrap(), [0, 255, 7]);
        assert_eq!(
            fs::read_to_string(f.repo.join("shared.txt")).unwrap(),
            "final\n"
        );
        assert_eq!(f.count(), 1);
    }

    #[test]
    fn patch_policy_applies_without_committing_and_preserves_unrelated_changes() {
        let f = Fixture::new();
        let base = commit(&f.repo, "HEAD").unwrap();
        let tree = IsolatedWorktree::create(f.spec("patch", MergePolicy::Patch)).unwrap();
        fs::remove_file(tree.path().join("shared.txt")).unwrap();
        fs::write(tree.path().join("replacement"), "new\n").unwrap();
        fs::write(f.repo.join("local edits"), "keep").unwrap();
        let result = tree.finish(true, None).unwrap();
        assert!(result.merged && result.patch_path.is_none());
        assert!(!f.repo.join("shared.txt").exists());
        assert_eq!(
            fs::read_to_string(f.repo.join("local edits")).unwrap(),
            "keep"
        );
        assert_eq!(
            fs::read_to_string(f.repo.join("replacement")).unwrap(),
            "new\n"
        );
        assert_eq!(commit(&f.repo, "HEAD").unwrap(), base);
        assert_eq!(f.count(), 1);
    }

    #[test]
    fn merge_uses_own_snapshot_when_export_is_replaced() {
        let f = Fixture::new();
        let first = IsolatedWorktree::create(f.spec("first", MergePolicy::Patch)).unwrap();
        let second = IsolatedWorktree::create(f.spec("second", MergePolicy::Patch)).unwrap();
        let shared_export = f.dir.path().join("changes.patch");
        fs::write(first.path().join("shared.txt"), "first").unwrap();
        fs::write(second.path().join("shared.txt"), "second").unwrap();
        let (mut result, snapshot) = first.snapshot(Some(&shared_export), true).unwrap();
        second.capture_changes(Some(&shared_export)).unwrap();
        first.integrate(&mut result, snapshot).unwrap();
        assert!(result.merged);
        assert_eq!(
            fs::read_to_string(f.repo.join("shared.txt")).unwrap(),
            "first"
        );
    }

    #[test]
    fn conflicting_patch_preserves_destination_and_keeps_worker_on_failure() {
        let f = Fixture::new();
        let mut spec = f.spec("conflict", MergePolicy::Patch);
        spec.keep_on_failure = true;
        let tree = IsolatedWorktree::create(spec.clone()).unwrap();
        let path = tree.path().to_owned();
        fs::write(path.join("shared.txt"), "worker\n").unwrap();
        fs::write(path.join("extra"), "added").unwrap();
        fs::write(f.repo.join("shared.txt"), "destination\n").unwrap();
        assert!(tree.finish(true, None).is_err());
        assert_eq!(
            fs::read_to_string(f.repo.join("shared.txt")).unwrap(),
            "destination\n"
        );
        assert!(!f.repo.join("extra").exists());
        assert_eq!(
            fs::read_to_string(path.join("shared.txt")).unwrap(),
            "worker\n"
        );
        drop(IsolatedWorktree::create(spec).unwrap());
        assert_eq!(f.count(), 1);
    }

    #[test]
    fn failed_worker_can_be_kept_then_recreated_without_applying_changes() {
        let f = Fixture::new();
        let mut spec = f.spec("../../escape/..", MergePolicy::Patch);
        spec.keep_on_failure = true;
        spec.branch_prefix = Some("worker".into());
        let tree = IsolatedWorktree::create(spec.clone()).unwrap();
        assert_eq!(
            tree.path().parent().unwrap(),
            spec.worktree_base.canonicalize().unwrap()
        );
        assert!(IsolatedWorktree::create(spec.clone()).is_err());
        fs::write(tree.path().join("shared.txt"), "failure\n").unwrap();
        let kept = tree.finish(false, None).unwrap();
        assert!(!kept.merged && kept.worktree_path.exists());
        assert_eq!(f.count(), 2);
        let retry = IsolatedWorktree::create(spec).unwrap();
        assert_eq!(
            fs::read_to_string(retry.path().join("shared.txt")).unwrap(),
            "original\n"
        );
        assert!(kept.branch.unwrap().starts_with("worker/"));
        drop(retry);
        assert_eq!(f.count(), 1);
    }

    #[test]
    fn fast_forward_advances_only_to_committed_descendant() {
        let f = Fixture::new();
        let mut spec = f.spec("ff", MergePolicy::FastForward);
        spec.branch_prefix = Some("worker".into());
        let tree = IsolatedWorktree::create(spec).unwrap();
        fs::write(tree.path().join("shared.txt"), "committed\n").unwrap();
        run(git(tree.path()).args(["commit", "-qam", "worker change"])).unwrap();
        let head = commit(tree.path(), "HEAD").unwrap();
        let result = tree.finish(true, None).unwrap();
        assert!(result.merged);
        assert_eq!(result.changed_files, ["shared.txt"]);
        assert_eq!(commit(&f.repo, "HEAD").unwrap(), head);
        assert_eq!(
            fs::read_to_string(f.repo.join("shared.txt")).unwrap(),
            "committed\n"
        );
        assert_eq!(f.count(), 1);
    }

    #[test]
    fn fast_forward_rejects_dirty_worker_and_divergent_history() {
        let f = Fixture::new();
        let tree = IsolatedWorktree::create(f.spec("dirty", MergePolicy::FastForward)).unwrap();
        fs::write(tree.path().join("new"), "pending").unwrap();
        assert!(tree.finish(true, None).unwrap_err().contains("committed"));
        let tree = IsolatedWorktree::create(f.spec("diverged", MergePolicy::FastForward)).unwrap();
        fs::write(tree.path().join("shared.txt"), "worker").unwrap();
        run(git(tree.path()).args(["commit", "-qam", "worker"])).unwrap();
        fs::write(f.repo.join("shared.txt"), "main").unwrap();
        run(git(&f.repo).args(["commit", "-qam", "destination"])).unwrap();
        let head = commit(&f.repo, "HEAD").unwrap();
        assert!(tree
            .finish(true, None)
            .unwrap_err()
            .contains("cannot fast-forward"));
        assert_eq!(commit(&f.repo, "HEAD").unwrap(), head);
        assert_eq!(f.count(), 1);
    }

    #[test]
    fn base_ref_policy_parsing_and_unregistered_paths() {
        let f = Fixture::new();
        assert_eq!(MergePolicy::parse("none"), Some(MergePolicy::None));
        assert_eq!(
            MergePolicy::parse("fast_forward"),
            Some(MergePolicy::FastForward)
        );
        assert_eq!(MergePolicy::parse("patch"), Some(MergePolicy::Patch));
        assert_eq!(MergePolicy::parse("force"), None);
        let old = commit(&f.repo, "HEAD").unwrap();
        fs::write(f.repo.join("shared.txt"), "later").unwrap();
        run(git(&f.repo).args(["commit", "-qam", "next"])).unwrap();
        let mut spec = f.spec("older", MergePolicy::None);
        spec.base_ref = Some("HEAD~1".into());
        spec.worktree_base = ".kgraph/worktrees".into();
        let tree = IsolatedWorktree::create(spec.clone()).unwrap();
        let path = tree.path().to_owned();
        let patch = f.dir.path().join("empty.patch");
        let unchanged = tree.finish(false, Some(&patch)).unwrap();
        assert_eq!(unchanged.base_commit, old);
        assert!(unchanged.changed_files.is_empty() && !patch.exists());
        fs::create_dir(&path).unwrap();
        fs::write(path.join("user-file"), "safe").unwrap();
        assert!(IsolatedWorktree::create(spec).is_err());
        assert_eq!(fs::read_to_string(path.join("user-file")).unwrap(), "safe");
        assert_eq!(f.count(), 1);
    }
}
