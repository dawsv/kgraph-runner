use crate::{args::Template, resolve, Result};
use serde_json::json;
use std::{fs, io::Write, path::Path};
use tempfile::NamedTempFile;

struct Toolchain {
    build: &'static [&'static str],
    lint: &'static [&'static str],
    test: &'static [&'static str],
}
impl Toolchain {
    fn detect(dir: &Path) -> Self {
        if dir.join("Cargo.toml").is_file() {
            Self {
                build: &["cargo", "build"],
                lint: &["cargo", "clippy", "--all-targets"],
                test: &["cargo", "test"],
            }
        } else if dir.join("package.json").is_file() {
            Self {
                build: &["npm", "run", "--if-present", "build"],
                lint: &["npm", "run", "--if-present", "lint"],
                test: &["npm", "test"],
            }
        } else if dir.join("pyproject.toml").is_file() {
            Self {
                build: &["python3", "-m", "compileall", "-q", "."],
                lint: &["python3", "-m", "tabnanny", "."],
                test: &["python3", "-m", "unittest"],
            }
        } else {
            Self {
                build: &["echo", "Add your build command here"],
                lint: &["echo", "Add your lint command here"],
                test: &["echo", "Add your test command here"],
            }
        }
    }
}

// echo is a shell built-in on Windows. Use a fixed shell command for the
// placeholder checks and explicit argv for the detected project tools.
fn command(argv: &[&str]) -> serde_json::Value {
    if argv.first() == Some(&"echo") {
        json!(argv.join(" "))
    } else {
        json!(argv)
    }
}

fn render(template: Template, cwd: &str, tools: Toolchain) -> String {
    let cwd = json!(cwd);
    let test = command(tools.test);
    match template {
        Template::Ci => {
            let mut text = String::from(
                "# Run independent checks with kgraph run --max-parallel 3.\ntitle = \"CI\"\n",
            );
            for (id, cmd) in [
                ("build", tools.build),
                ("lint", tools.lint),
                ("test", tools.test),
            ] {
                text.push_str(&format!("\n[[nodes]]\nid = \"{id}\"\ntype = \"shell\"\ncwd = {cwd}\ncommand = {}\ncacheable = false\n", command(cmd)));
            }
            text
        }
        Template::AgentLoop => format!(
            r#"title = "Agent loop"

[[nodes]]
id = "implement"
type = "agent"
adapter = "codex"
cwd = {cwd}
prompt = "Implement the requested change and run the project tests."
verify_cmd = {test}
max_retries = 2
cacheable = false

# Uncomment to evaluate each verified attempt and request revisions.
# [nodes.evaluator]
# adapter = "codex"
# prompt = 'Return JSON with "approved" (boolean) and "feedback" (string).'
"#
        ),
        Template::SwarmSplitter => {
            let write_result = if cfg!(windows) {
                "echo %PART% > result.json"
            } else {
                r#"echo "$PART" > result.json"#
            };
            format!(
                r#"title = "Swarm splitter"

[[nodes]]
id = "split"
type = "splitter"
items = [1, 2]
max_workers = 2
isolation = "worktree"

[nodes.template]
id = "part-{{{{item}}}}"
type = "shell"
cwd = {cwd}
command = '{write_result}'
outputs = ["result.json"]
env = {{ PART = "{{{{item}}}}" }}

[nodes.reduce]
from_output = "result.json"
op = "collect_list"
to_ctx = "results"

[[nodes]]
id = "approve"
deps = ["split"]
type = "gate"
packet_path = "approval_packet.json"
artifact_paths = ["approval.json"]
packet = {{ question = "Approve the ordered results?", results = "{{{{ctx.results}}}}", approval_path = "approval.json" }}
verify_json = {{ require_approved = true }}
"#
            )
        }
        Template::BugFix | Template::BugBatch => {
            let (preset, spec) = match template {
                Template::BugFix => ("desktop_bug_fix", "[bug_spec]"),
                _ => ("bug_batch", "[[bugs]]"),
            };
            format!("preset = \"{preset}\"\nrepo_dir = {cwd}\nwork_dir = \"bug-work\"\ntest_cmd = {test}\n\n{spec}\nid = \"example\"\ntitle = \"Describe the bug to fix\"\n")
        }
        Template::Release => {
            "preset = \"version_history\"\nrelease_id = \"v1\"\nwork_dir = \"release-work\"\n"
                .into()
        }
    }
}

pub fn scaffold(template: Template, output: &Path, dir: &Path, force: bool) -> Result<i32> {
    let workspace = dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    if !workspace.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let path = workspace.join(output);
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
    {
        return Err("--output must name a .toml workflow".into());
    }
    let parent = path.parent().ok_or("Missing output directory")?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let base = resolve(parent)?;
    let cwd = match base.strip_prefix(&workspace) {
        Ok(relative) if relative.components().count() == 0 => ".".to_owned(),
        Ok(relative) => vec![".."; relative.components().count()].join("/"),
        Err(_) => workspace
            .to_str()
            .ok_or("Workspace path is not UTF-8")?
            .into(),
    };
    let text = render(template, &cwd, Toolchain::detect(&workspace));
    let mut file = NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    file.write_all(text.as_bytes())
        .and_then(|_| file.as_file().sync_all())
        .map_err(|e| e.to_string())?;
    let saved = if force {
        file.persist(&path)
    } else {
        file.persist_noclobber(&path)
    };
    saved.map_err(|e| format!("{}: {}", path.display(), e.error))?;
    println!("{}", path.display());
    Ok(0)
}
