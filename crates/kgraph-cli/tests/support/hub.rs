use super::support::*;
use kgraph_town::server::{start_hub_server, ServerConfig};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::{process::Stdio, thread, time::Duration};

#[test]
fn authenticated_hub_publishing_queued_decisions_and_watch_resume() {
    let hub = Project::new();
    let origin = Project::new();
    // The only in-process registry user in this test binary is the Hub.
    // Each CLI subprocess has a separate registry, as on a remote origin.
    std::env::set_var("KGRAPH_HOME", hub.path("home"));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let server = rt
        .block_on(start_hub_server(
            vec![],
            ServerConfig {
                port: 0,
                token: Some("secret".into()),
                ..Default::default()
            },
        ))
        .unwrap();
    let url = format!("http://127.0.0.1:{}", server.port);
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let get = |path: &str| -> Value {
        client
            .get(format!("{url}{path}"))
            .bearer_auth("secret")
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap()
    };
    let queue = |id: &str, gate: &Value, mut decision: Value| {
        decision["node_path"] = gate["node_path"].clone();
        decision["packet_hash"] = gate["packet_hash"].clone();
        let r: Value = client
            .post(format!("{url}/api/runs/{id}/respond"))
            .bearer_auth("secret")
            .json(&decision)
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(r["delivery"], "remote_queue", "{r}");
    };
    let p = &origin;
    p.workflow(json!([gate("review")]));
    p.cli(&["run", "flow.json", "--quiet"], 4);
    let state = ".kgraph/flow_state.json";
    let pull = [
        "pull-response",
        state,
        "--run-id",
        "remote-review",
        "--hub-url",
        &url,
        "--token",
        "secret",
    ];
    let call = |base: &[&str], extra: &[&str], code| {
        let mut a = base.to_vec();
        a.extend_from_slice(extra);
        p.cli(&a, code)
    };
    let o = p.cli(
        &[
            "push",
            "flow.json",
            "--run-id",
            "remote-review",
            "--hub-url",
            &url,
            "--token",
            "secret",
            "--project-name",
            "team",
            "--host-label",
            "elsewhere",
        ],
        0,
    );
    assert!(stdout(&o).contains("Pushed remote-review | awaiting_subagents"));
    let g = &get("/api/snapshot?run=remote-review")["attention"]["input_gate"];
    assert_eq!(g["node_path"], "review");
    assert_eq!(
        stdout(&call(
            &pull,
            &["--wait", "--poll-ms", "10", "--timeout", "0.03", "--json"],
            4
        )),
        "null\n"
    );
    queue(
        "remote-review",
        g,
        json!({"approved":true,"approved_fields":{"approve_visuals":false}}),
    );
    assert!(stdout(&call(&pull, &["--no-apply", "--json"], 0)).contains("packet_hash"));
    assert!(!p.path(".kgraph/review.json").exists());
    let packet = p.read(".kgraph/review_packet.json");
    let mut stale = packet.clone();
    stale["revision"] = json!("edited");
    p.write(".kgraph/review_packet.json", &stale);
    assert!(stderr(&call(&pull, &[], 1)).contains("packet hash"));
    p.write(".kgraph/review_packet.json", &packet);
    assert!(stdout(&call(&pull, &["--apply"], 0)).contains("Approved"));
    let d = p.read(".kgraph/review.json");
    assert_eq!(d["approve_visuals"], false);
    p.cli(&["run", "flow.json", "--quiet"], 0);
    call(&pull, &[], 1);
    assert_eq!(p.read(".kgraph/review.json"), d);

    let s = ".kgraph/state.json";
    p.write(
        s,
        &json!({"nodes":{"release":{"kind":"worker","status":"running"}}}),
    );
    let ask = [
        "ask",
        s,
        "--node",
        "release",
        "--item",
        "confirm",
        "--option",
        "Yes",
        "--hub-url",
        &url,
        "--token",
        "secret",
        "--run-id",
        "remote-subtask",
        "--host-label",
        "elsewhere",
    ];
    call(&ask, &["--question", "Review old packet?"], 0);
    let g = get("/api/snapshot?run=remote-subtask")["attention"]["input_gate"].clone();
    queue("remote-subtask", &g, json!({"decisions":{"q1":"Yes"}}));
    call(&ask, &["--question", "Review updated packet?"], 0);
    let new = get("/api/snapshot?run=remote-subtask")["attention"]["input_gate"].clone();
    assert_ne!(g["packet_hash"], new["packet_hash"]);
    let answer = [
        "answer",
        s,
        "--node",
        "release",
        "--item",
        "confirm",
        "--run-id",
        "remote-subtask",
        "--hub-url",
        &url,
        "--token",
        "secret",
        "--json",
    ];
    assert_eq!(stdout(&call(&answer, &[], 4)), "null\n");
    thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(40));
            queue("remote-subtask", &new, json!({"decisions":{"q1":"Yes"}}));
        });
        let decision: Value = serde_json::from_slice(
            &call(&answer, &["--wait", "--poll-ms", "10", "--timeout", "3"], 0).stdout,
        )
        .unwrap();
        assert_eq!(decision["node_path"], "release::confirm");
        assert_eq!(decision, p.read(".kgraph/artifacts/confirm_decision.json"));
    });
    p.cli(
        &[
            "answer",
            s,
            "--node",
            "release",
            "--item",
            "confirm",
            "--run-id",
            "remote-subtask",
            "--hub-url",
            &url,
            "--token",
            "wrong",
        ],
        0,
    );

    let watch = Project::new();
    watch.workflow(json!([gate("approve")]));
    let child = watch
        .cmd(&[
            "run",
            "flow.json",
            "--watch",
            "--watch-interval",
            "0.01",
            "--watch-timeout",
            "5",
            "--quiet",
        ])
        .env("KGRAPH_HUB_URL", &url)
        .env("KGRAPH_HUB_TOKEN", "secret")
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (id, g) = wait_for(|| {
        let runs = get("/api/runs");
        let r = runs["runs"].as_array()?.iter().find(|r| {
            r["status"] == "awaiting_subagents"
                && r["id"] != "remote-review"
                && r["id"] != "remote-subtask"
        })?;
        let id = r["id"].as_str()?;
        let gate = get(&format!("/api/snapshot?run={id}"))["attention"]["input_gate"].clone();
        (gate["node_path"] == "approve").then(|| (id.to_owned(), gate))
    });
    queue(&id, &g, json!({"approved":true}));
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(watch.read(".kgraph/flow_report.json")["status"], "passed");
    assert_eq!(get(&format!("/api/snapshot?run={id}"))["status"], "passed");
    watch.cli(
        &[
            "run",
            "flow.json",
            "--hub-url",
            &url,
            "--hub-token",
            "wrong",
            "--quiet",
        ],
        0,
    );
    server.stop();
    rt.block_on(server.wait()).unwrap();
}
