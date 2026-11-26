use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

pub struct Project(pub TempDir);
impl Project {
    pub fn new() -> Self {
        let p = Self(tempfile::tempdir().unwrap());
        p.write("ok.json", &json!({}));
        p
    }
    pub fn path(&self, path: &str) -> PathBuf {
        kgraph_town::interactions::resolve(self.0.path())
            .unwrap()
            .join(path)
    }
    pub fn write(&self, path: &str, data: &Value) {
        kgraph_core::files::write_json(&self.path(path), data, true).unwrap();
    }
    pub fn read(&self, path: &str) -> Value {
        serde_json::from_slice(&fs::read(self.path(path)).unwrap()).unwrap()
    }
    pub fn workflow(&self, nodes: Value) {
        self.write("flow.json", &json!({"nodes":nodes}));
    }
    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_kgraph"));
        c.current_dir(self.0.path())
            .args(args)
            .env("KGRAPH_HOME", self.path("home"))
            .env_remove("KGRAPH_HUB_URL")
            .env_remove("KGRAPH_HUB_TOKEN");
        c
    }
    pub fn cli(&self, args: &[&str], expected: i32) -> Output {
        let o = self.cmd(args).output().unwrap();
        assert_eq!(
            o.status.code(),
            Some(expected),
            "{args:?}\n{}\n{}",
            stdout(&o),
            stderr(&o)
        );
        o
    }
    pub fn json(&self, args: &[&str], code: i32) -> Value {
        serde_json::from_slice(&self.cli(args, code).stdout).unwrap()
    }
}
pub fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}
pub fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).unwrap()
}
pub fn shell(id: &str) -> Value {
    json!({"id":id,"type":"shell","command":[env!("CARGO_BIN_EXE_kgraph"),"status","ok.json"],"resource":"editor"})
}
pub fn gate(id: &str) -> Value {
    json!({"id":id,"type":"gate","packet_path":format!(".kgraph/{id}_packet.json"),
    "artifact_paths":[format!(".kgraph/{id}.json")],"packet":{"approval_path":format!("{id}.json"),"approved_fields":{"approve_visuals":true}},"verify_json":{"require_approved":true}})
}
pub fn wait_for<T>(mut condition: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = condition() {
            return v;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "condition timed out"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
pub fn read_when(path: &Path, condition: impl Fn(&Value) -> bool) -> Value {
    wait_for(|| {
        let data = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        condition(&data).then_some(data)
    })
}
