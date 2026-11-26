use crate::{args::Publish, town};
use serde_json::Value;
use std::{
    sync::{Arc, Condvar, Mutex},
    thread,
    time::Duration,
};

/// A serialized Hub connection with one replaceable pending snapshot. The
/// first snapshot is handed directly to the sender so a fast pause cannot erase it.
#[derive(Default)]
struct Queue {
    pending: Option<Value>,
    started: bool,
    sending: bool,
    closed: bool,
    run_id: Option<String>,
}
pub struct Publisher {
    url: String,
    token: Option<String>,
    queue: Mutex<Queue>,
    changed: Condvar,
}
impl Publisher {
    pub fn new(url: String, token: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            url,
            token,
            queue: Mutex::default(),
            changed: Condvar::new(),
        })
    }
    pub fn initialize(&self, id: &str) {
        self.queue
            .lock()
            .unwrap()
            .run_id
            .get_or_insert_with(|| id.to_owned());
    }
    pub fn submit(self: &Arc<Self>, snapshot: Value) {
        let mut q = self.queue.lock().unwrap();
        if !q.started {
            q.started = true;
            q.sending = true;
            let this = self.clone();
            thread::spawn(move || this.send(snapshot));
        } else {
            q.pending = Some(snapshot);
            self.changed.notify_all();
        }
    }
    fn send(&self, mut snapshot: Value) {
        loop {
            let opts = Publish {
                token: self.token.clone(),
                run_id: self.queue.lock().unwrap().run_id.clone(),
                ..Default::default()
            };
            let result = town::publish(snapshot, &self.url, &opts);
            let mut q = self.queue.lock().unwrap();
            match result {
                Ok(run) => {
                    if let Some(id) = run["id"].as_str() {
                        q.run_id = Some(id.into());
                    }
                }
                Err(error) => eprintln!("kgraph-town: could not report to Hero Town Hub: {error}"),
            }
            q.sending = false;
            self.changed.notify_all();
            q = self
                .changed
                .wait_while(q, |q| q.pending.is_none() && !q.closed)
                .unwrap();
            let Some(next) = q.pending.take() else {
                return;
            };
            q.sending = true;
            snapshot = next;
        }
    }
    fn flush(&self) -> Option<String> {
        let q = self.queue.lock().unwrap();
        let (q, _) = self
            .changed
            .wait_timeout_while(q, Duration::from_millis(250), |q| {
                q.sending || q.pending.is_some()
            })
            .unwrap();
        q.run_id.clone()
    }
    pub fn finish(self: &Arc<Self>, snapshot: Value) -> Option<String> {
        self.flush();
        self.submit(snapshot);
        self.flush()
    }
    pub fn close(&self) {
        self.queue.lock().unwrap().closed = true;
        self.changed.notify_all();
    }
}

#[cfg(test)]
#[path = "../tests/support/ingest.rs"]
mod ingest;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{net::TcpListener, sync::mpsc, time::Instant};

    #[test]
    fn blocked_hub_replaces_pending_pause_with_final_report() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let hub = Publisher::new(format!("http://{}", listener.local_addr().unwrap()), None);
        hub.initialize("stable");
        let (started, sending) = mpsc::sync_channel(1);
        let (release, blocked) = mpsc::sync_channel(1);
        let server = thread::spawn(move || {
            let (stream, first) = ingest::request(&listener);
            started.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
            ingest::ack(stream, &first);
            let (stream, last) = ingest::request(&listener);
            ingest::ack(stream, &last);
            (first, last)
        });
        hub.submit(json!({"status":"running","event":"node_start"}));
        sending.recv_timeout(Duration::from_secs(2)).unwrap();
        hub.submit(json!({"status":"awaiting_subagents","event":"node_awaiting_subagents"}));
        let start = Instant::now();
        hub.finish(json!({"status":"awaiting_subagents","summary":{"total_events":1}}));
        assert!(start.elapsed() < Duration::from_millis(1250));
        hub.close();
        release.send(()).unwrap();
        let (first, last) = server.join().unwrap();
        assert_eq!(first["snapshot"]["event"], "node_start");
        assert_eq!(last["snapshot"]["summary"]["total_events"], 1);
        assert!(last["snapshot"].get("event").is_none());
        assert_eq!(first["run"]["run_id"], last["run"]["run_id"]);
    }
}
