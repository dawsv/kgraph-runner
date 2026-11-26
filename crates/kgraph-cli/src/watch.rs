use crate::{remaining, resolve, response, Result};
use kgraph_town::{interactions as live, network};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};
type Signature = (SystemTime, u64);
pub type Outputs = HashMap<PathBuf, Option<Signature>>;
pub type Wakeups = HashMap<PathBuf, Signature>;

fn signature(path: &Path) -> Option<Signature> {
    let m = path.metadata().ok()?;
    if !m.is_file() {
        return None;
    }
    Some((m.modified().ok()?, m.len()))
}
pub fn collect(
    outputs: &mut Outputs,
    packets: impl IntoIterator<Item = impl AsRef<str>>,
    state: &Path,
    report: &Path,
    project: &Path,
    wakeups: &Wakeups,
) {
    let (Ok(state), Ok(report)) = (resolve(state), resolve(report)) else {
        return;
    };
    let (root, packet_state) = match live::allowed_kgraph_root(&state) {
        Ok(root) => (root, state.clone()),
        Err(_) => {
            let Ok(root) = resolve(&project.join(".kgraph")) else {
                return;
            };
            let proxy = root.join("state.json");
            (root, proxy)
        }
    };
    let progress = resolve(&live::progress_path_for_state(&state)).ok();
    for raw in packets {
        let Ok(path) = live::resolve_packet(
            raw.as_ref(),
            &packet_state,
            &report,
            Some(project),
            &mut |_| {},
        ) else {
            continue;
        };
        let Ok(path) = resolve(&path) else {
            continue;
        };
        let Ok((packet, meta, _)) = live::json_file(&path, Some(live::PACKET_LIMIT)) else {
            continue;
        };
        let declared = &packet["outputs"];
        let mut candidates: Vec<&Value> = match declared {
            Value::Object(o) => o.values().collect(),
            Value::Array(a) => a.iter().collect(),
            other => vec![other],
        };
        candidates.extend([&packet["output_path"], &packet["approval_path"]]);
        for raw in candidates
            .into_iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.trim().is_empty() && !s.contains('\0'))
        {
            let target = Path::new(raw);
            if live::reject_path_traversal(target).is_err() {
                continue;
            }
            let base = if target.starts_with(".kgraph") {
                root.parent()
            } else {
                path.parent()
            };
            let Some(base) = base else {
                continue;
            };
            let Ok(target) = live::ensure_inside(&base.join(target), &root) else {
                continue;
            };
            if target == state
                || target == report
                || target == path
                || Some(&target) == progress.as_ref()
                || outputs.contains_key(&target)
            {
                continue;
            }
            let mut current = signature(&target);
            if current.is_some_and(|s| {
                meta.modified().is_ok_and(|packet_time| s.0 > packet_time)
                    && wakeups.get(&target) != Some(&s)
            }) {
                current = None;
            }
            outputs.insert(target, current);
        }
    }
}

pub enum Wait {
    Ready,
    TimedOut,
    Interrupted,
}
pub struct Watch<'a> {
    pub state: &'a Path,
    pub report: &'a Path,
    pub interval: Duration,
    pub limit: Option<Duration>,
    pub url: Option<&'a str>,
    pub token: Option<&'a str>,
    pub run_id: Option<&'a str>,
}
impl Watch<'_> {
    pub fn wait(
        &self,
        outputs: &Outputs,
        wakeups: &mut Wakeups,
        applied: &mut Option<Value>,
        start: Instant,
    ) -> Result<Wait> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let result=runtime.block_on(async {
            tokio::select!{
                sig=tokio::signal::ctrl_c()=>sig.map(|_|Wait::Interrupted).map_err(|e|e.to_string()),
                result=self.poll(outputs,wakeups,applied,start)=>result,
            }
        });
        // A slow in-flight HTTP read must not hold the foreground on interrupt.
        runtime.shutdown_background();
        result
    }
    async fn poll(
        &self,
        outputs: &Outputs,
        wakeups: &mut Wakeups,
        applied: &mut Option<Value>,
        start: Instant,
    ) -> Result<Wait> {
        let mut last_error = None;
        loop {
            let mut changed = false;
            for (path, old) in outputs {
                if let Some(current) = signature(path).filter(|s| Some(s) != old.as_ref()) {
                    wakeups.insert(path.clone(), current);
                    changed = true;
                }
            }
            if changed {
                return Ok(Wait::Ready);
            }
            let left = remaining(start, self.limit);
            if left.is_zero() {
                return Ok(Wait::TimedOut);
            }
            if let (Some(url), Some(id)) = (self.url, self.run_id) {
                let (url, token, id) =
                    (url.to_owned(), self.token.map(str::to_owned), id.to_owned());
                let task = tokio::task::spawn_blocking(move || {
                    network::fetch_remote_response(&url, token.as_deref(), &id)
                });
                let received = if self.limit.is_some() {
                    match tokio::time::timeout(left, task).await {
                        Ok(r) => r,
                        Err(_) => return Ok(Wait::TimedOut),
                    }
                } else {
                    task.await
                };
                let result = received.map_err(|e| e.to_string())?.and_then(|r| {
                    if let Some(r) = r.map(Value::Object).filter(|r| Some(r) != applied.as_ref()) {
                        if !remaining(start, self.limit).is_zero()
                            && response::apply(&r, self.state, Some(self.report), true)?.is_some()
                        {
                            *applied = Some(r);
                            return Ok(true);
                        }
                    }
                    Ok(false)
                });
                match result {
                    Ok(true) => return Ok(Wait::Ready),
                    Ok(false) => last_error = None,
                    Err(e) => {
                        if last_error.as_ref() != Some(&e) {
                            eprintln!("kgraph-town: could not pull from Hero Town Hub: {e}");
                        }
                        last_error = Some(e);
                    }
                }
            }
            tokio::time::sleep(self.interval.min(remaining(start, self.limit))).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraph_core::files::write_json;
    use serde_json::json;

    #[test]
    fn packet_paths_are_confined_and_protected_with_a_custom_state_location() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().canonicalize().unwrap();
        let root = project.join(".kgraph");
        let state = project.join("custom_state.json");
        let report = root.join("report.json");
        let packet = root.join("packets/gate.json");
        write_json(&packet,&json!({"outputs":{"normal":"image.png","nested":".kgraph/nested/output","protected":report,"packet":packet,"parent":"../outside","unsafe_absolute":project.join("external"),"empty":"","null":"a\0b"}}),true).unwrap();
        let mut outputs = Outputs::new();
        collect(
            &mut outputs,
            [".kgraph/packets/gate.json"],
            &state,
            &report,
            &project,
            &Wakeups::new(),
        );
        assert_eq!(outputs.len(), 2);
        assert!(outputs.contains_key(&root.join("packets/image.png")));
        assert!(outputs.contains_key(&root.join("nested/output")));
    }
    #[test]
    fn oversized_packet_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join(".kgraph");
        let packet = root.join("gate.json");
        let state = root.join("state.json");
        write_json(
            &packet,
            &json!({"approval_path":"ok.json","pad":"x".repeat(live::PACKET_LIMIT)}),
            false,
        )
        .unwrap();
        let mut outputs = Outputs::new();
        collect(
            &mut outputs,
            [packet.to_str().unwrap()],
            &state,
            &root.join("report.json"),
            temp.path(),
            &Wakeups::new(),
        );
        assert!(outputs.is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn symlink_to_an_external_output_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().canonicalize().unwrap();
        let root = project.join(".kgraph");
        let packet = root.join("gate.json");
        write_json(&packet, &json!({"output_path":"link/output.json"}), true).unwrap();
        std::os::unix::fs::symlink(&project, root.join("link")).unwrap();
        let mut out = Outputs::new();
        collect(
            &mut out,
            [packet.to_str().unwrap()],
            &root.join("state.json"),
            &root.join("report.json"),
            &project,
            &Wakeups::new(),
        );
        assert!(out.is_empty());
    }
}
