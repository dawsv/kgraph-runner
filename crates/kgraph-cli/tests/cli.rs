#[path = "support/hub.rs"]
mod hub;
mod support;
use serde_json::{json, Value};
use std::{
    fs,
    process::{Command, Stdio},
};
use support::*;

#[test]
fn both_binaries_share_parser_and_validation() {
    for bin in [
        env!("CARGO_BIN_EXE_kgraph"),
        env!("CARGO_BIN_EXE_kgraph-town"),
    ] {
        let help = Command::new(bin).arg("--help").output().unwrap();
        assert!(help.status.success());
        assert!(stdout(&help).contains("pull-response"));
        for args in [
            vec!["xxx"],
            vec!["ui", "--auth", "--token", "x"],
            vec!["hub", "--no-auth", "--auth"],
            vec!["hub", "--token", "x", "--no-auth"],
            vec!["ask", "state", "--node", "n", "--item", "i"],
            vec![
                "ask",
                "state",
                "--node",
                "n",
                "--item",
                "i",
                "--question",
                "q",
                "--questions-json",
                "[]",
            ],
            vec![
                "pull-response",
                "state",
                "--run-id",
                "r",
                "--apply",
                "--no-apply",
            ],
        ] {
            let o = Command::new(bin).args(args).output().unwrap();
            assert_eq!(o.status.code(), Some(1));
            assert!(!o.stderr.is_empty());
        }
    }
}
#[test]
fn plan_cache_and_run_scoping() {
    let p = Project::new();
    let mut second = shell("second");
    second["deps"] = json!(["first"]);
    p.write(
        "flow.json",
        &json!({"nodes":[shell("first"),second],"qa_checklist_path":"qa/manual.json"}),
    );
    let table = stdout(&p.cli(&["plan", "flow.json", "--until", "first"], 0));
    let lines = table
        .lines()
        .map(|s| s.split_whitespace().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(
        lines,
        [
            vec!["Path", "Kind", "Status", "Reason", "Resource"],
            vec!["first", "code", "will_run", "no_prior_state", "editor"]
        ]
    );
    assert!(!p.path(".kgraph").exists());
    p.cli(&["run", "flow.json", "--until", "first", "--quiet"], 0);
    let report = p.read(".kgraph/flow_report.json");
    assert_eq!(report["status"], "stopped");
    assert_eq!(report["workflow_ref"], json!(p.path("flow.json")));
    assert_eq!(report["qa_checklist_path"], json!(p.path("qa/manual.json")));
    let path = p.path(".kgraph/flow_state.json");
    let state = fs::read(&path).unwrap();
    for (options, status, reason) in [
        (vec![], "cached", "fresh"),
        (
            vec!["--retry", "first", "--retry", "second"],
            "will_run",
            "retry_requested",
        ),
        (vec!["--force"], "will_run", "forced"),
    ] {
        let mut args = vec!["plan", "flow.json", "--until", "first", "--json"];
        args.extend(options);
        let out = p.json(&args, 0);
        assert_eq!(out[0]["status"], status);
        assert_eq!(out[0]["reason"], reason);
        assert_eq!(fs::read(&path).unwrap(), state);
    }
    p.cli(&["run", "flow.json", "--quiet"], 0);
    assert_eq!(
        p.read(".kgraph/flow_report.json")["cached_nodes"],
        json!(["first"])
    );
    for flags in [
        vec!["--retry", "first", "--retry", "second"],
        vec!["--force"],
    ] {
        let mut a = vec!["run", "flow.json", "--quiet"];
        a.extend(flags);
        p.cli(&a, 0);
        assert_eq!(
            p.read(".kgraph/flow_report.json")["cached_nodes"],
            json!([])
        );
    }
    let runs = p.json(&["hub", "list", "--json"], 0);
    assert_eq!(runs["runs"][0]["status"], "passed");
    assert!(stdout(&p.cli(&["hub", "list"], 0)).contains("| passed |"));
}
#[test]
fn declarative_context_file_then_inline_and_toml() {
    let p = Project::new();
    p.write("context.json", &json!({"file":"absent"}));
    let file = serde_json::to_string(env!("CARGO_BIN_EXE_kgraph")).unwrap();
    fs::write(
        p.path("workflow.toml"),
        format!(
            r#"
        [default_context]
        file = "missing"
        [[nodes]]
        id = "run"
        type = "shell"
        command = [{file}, "status", "{{{{ctx.file}}}}"]
    "#
        ),
    )
    .unwrap();
    p.cli(
        &[
            "run",
            "workflow.toml",
            "--ctx-file",
            "context.json",
            "--ctx",
            r#"{"file":"ok.json"}"#,
            "--quiet",
        ],
        0,
    );
    p.cli(&["plan", "workflow.toml", "--ctx", "[]"], 1);
}
#[test]
fn failures_and_exit_codes() {
    let p = Project::new();
    let mut n = shell("step");
    n["command"][2] = json!("missing.json");
    for (retry, code, status) in [(0, 1, "failed"), (2, 2, "circuit_broken")] {
        n["max_retries"] = json!(retry);
        p.workflow(json!([n]));
        assert!(stderr(&p.cli(&["run", "flow.json", "--quiet"], code)).contains(status));
        assert_eq!(p.read(".kgraph/flow_report.json")["status"], status);
    }
    let mut worker = shell("worker");
    worker["kind"] = json!("worker");
    p.workflow(json!([worker]));
    p.cli(&["run", "flow.json", "--max-workers", "0", "--quiet"], 3);
    assert_eq!(
        p.read(".kgraph/flow_report.json")["status"],
        "budget_exceeded"
    );
    p.workflow(json!([gate("review")]));
    p.cli(&["run", "flow.json", "--quiet"], 4);
    assert_eq!(
        p.read(".kgraph/flow_report.json")["status"],
        "awaiting_subagents"
    );
}
#[test]
fn invalid_inputs_write_failure_report_and_protect_files() {
    let p = Project::new();
    fs::write(p.path("flow.py"), "raise Exception('never execute')").unwrap();
    let error = stderr(&p.cli(&["run", "flow.py", "--quiet"], 1));
    assert!(error.contains("declarative .toml/.json"));
    assert_eq!(p.read(".kgraph/flow_report.json")["status"], "failed");
    p.workflow(json!([shell("step")]));
    for (opt, value) in [
        ("--max-parallel", "0"),
        ("--max-workers", "-1"),
        ("--watch-interval", "0"),
        ("--watch-interval", "nan"),
        ("--watch-timeout", "-1"),
        ("--watch-timeout", "inf"),
    ] {
        assert!(stderr(&p.cli(&["run", "flow.json", opt, value], 1)).contains(opt));
    }
    let before = fs::read(p.path("flow.json")).unwrap();
    p.cli(&["run", "flow.json", "--report", "./flow.json"], 1);
    assert_eq!(fs::read(p.path("flow.json")).unwrap(), before);
    p.cli(
        &[
            "run",
            "flow.json",
            "--state",
            "new.json",
            "--report",
            "./new.json",
        ],
        1,
    );
    assert!(!p.path("new.json").exists());
    fs::write(p.path("state.json"), "{").unwrap();
    p.cli(&["run", "flow.json", "--state", "state.json", "--quiet"], 1);
    let r = p.read(".kgraph/flow_report.json");
    assert_eq!(r["state_file"], "state.json");
    assert_eq!(r["summary"]["total_events"], 0);
}
#[test]
fn render_defaults_embed_layout_and_protected_paths() {
    let p = Project::new();
    p.workflow(json!([shell("stage")]));
    p.cli(
        &[
            "render",
            "flow.json",
            "--title",
            "My Town",
            "--layout",
            "linear",
        ],
        0,
    );
    let html = fs::read_to_string(p.path(".kgraph/flow_town.html")).unwrap();
    assert!(html.contains("My Town"));
    assert!(html.contains(r#"data-layout="strip" aria-pressed="true""#));
    assert!(html.contains("stage"));
    p.write(
        "legacy.json",
        &json!({"nodes":{"old_stage":{"kind":"code","status":"passed"}}}),
    );
    p.cli(
        &[
            "render",
            "--state",
            "legacy.json",
            "--embed",
            "--layout",
            "compact",
        ],
        0,
    );
    let embed = fs::read_to_string(p.path(".kgraph/town.html")).unwrap();
    assert!(embed.contains("old_stage"));
    assert!(embed.contains(r#"data-layout="iso" aria-pressed="true""#));
    assert!(embed.contains(r#"<body class="embed""#));
    p.cli(
        &[
            "render",
            "legacy.py",
            "--state",
            "legacy.json",
            "--output",
            "legacy.html",
        ],
        0,
    );
    assert!(fs::read_to_string(p.path("legacy.html"))
        .unwrap()
        .contains("old_stage"));
    p.write("ctx.json", &json!({}));
    for protected in ["flow.json", "ctx.json", "state.json", "report.json"] {
        let err = stderr(&p.cli(
            &[
                "render",
                "flow.json",
                "--ctx-file",
                "ctx.json",
                "--state",
                "state.json",
                "--report",
                "report.json",
                "--output",
                protected,
            ],
            1,
        ));
        assert!(err.contains("--output"));
    }
    p.cli(&["ui", "missing.toml", "--port", "0"], 1);
    p.cli(&["ui"], 1);
}
#[test]
fn status_and_report_format_existing_python_telemetry() {
    let p = Project::new();
    assert_eq!(
        stdout(&p.cli(&["status", "ok.json"], 0)),
        "No recorded nodes.\n"
    );
    p.write("state.json",&json!({"nodes":{"two":{"kind":"gate","status":"failed","attempts":2,"duration_s":0.25,"detail":"one\ntwo","error_signature":"mismatch"},"one":{"kind":"code","status":"passed"}}}));
    let status = stdout(&p.cli(&["status", "state.json"], 0));
    assert!(status.contains("Detail / error signature"));
    assert!(status.lines().nth(1).unwrap().starts_with("one"));
    assert!(status.contains("one two | mismatch"));
    assert_eq!(
        p.json(&["status", "state.json", "--json"], 0),
        p.read("state.json")
    );
    p.write("report.json",&json!({"status":"failed","error":"oops","packet_path":"p","packet_paths":["p","q"],"reached_path":"b","last_completed_stage":"a","scope":{"requested_until":"b","reached_nodes":["a","b"]},"summary":{"failed":1,"retried_nodes":["b"]},"failure_signatures":[{"node":"b","status":"failed","error_signature":"bad","detail":"message"}],"slowest_nodes":[{"node":"b","duration_s":1.5,"attempts":2}],"evolution_hints":["fix it"]}));
    assert_eq!(stdout(&p.cli(&["report","report.json"],0)),"Status: failed\nError: oops\nPacket: p\nPacket: q\nScope: until=b, reached=b, last completed=a, nodes=2\nSummary: failed=1\nRetried nodes: b\nFailure signatures:\n  b [bad] message\nSlowest nodes:\n  b  1.5s  (2 attempts)\nEvolution hints:\n  fix it\n");
    assert_eq!(
        p.json(&["report", "report.json", "--json"], 0),
        p.read("report.json")
    );
}
#[test]
fn watch_parallel_outputs_and_existing_artifact_edits() {
    let p = Project::new();
    p.workflow(json!([gate("first"), gate("other")]));
    p.write(".kgraph/first.json", &json!({"approved":false}));
    let child = p
        .cmd(&[
            "run",
            "flow.json",
            "--watch",
            "--watch-timeout",
            "6",
            "--watch-interval",
            "0.01",
            "--max-parallel",
            "2",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let report = p.path(".kgraph/flow_report.json");
    read_when(&report, |r| {
        r["packet_paths"].as_array().is_some_and(|a| a.len() == 2)
    });
    p.write(".kgraph/first.json", &json!({"approved":true}));
    read_when(&report, |r| {
        r["packet_paths"]
            .as_array()
            .is_some_and(|a| a.len() == 1 && a[0].as_str().unwrap().ends_with("other_packet.json"))
    });
    p.write(".kgraph/other.json", &json!({"approved":true}));
    let result = child.wait_with_output().unwrap();
    assert_eq!(result.status.code(), Some(0), "{}", stderr(&result));
    assert!(stdout(&result).contains("Waiting packets:"));
    assert_eq!(p.read(".kgraph/flow_report.json")["status"], "passed");
}
#[test]
fn watch_preserves_worker_budget_across_resume() {
    let p = Project::new();
    let mut first = shell("first");
    first["kind"] = json!("worker");
    let mut g = gate("review");
    g["deps"] = json!(["first"]);
    let mut second = shell("second");
    second["kind"] = json!("worker");
    second["deps"] = json!(["review"]);
    p.workflow(json!([first, g, second]));
    let mut child = p
        .cmd(&[
            "run",
            "flow.json",
            "--max-workers",
            "1",
            "--watch",
            "--watch-timeout",
            "5",
            "--watch-interval",
            "0.01",
            "--quiet",
        ])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    read_when(&p.path(".kgraph/flow_report.json"), |r| {
        r["status"] == "awaiting_subagents"
    });
    p.write(".kgraph/review.json", &json!({"approved":true}));
    assert_eq!(child.wait().unwrap().code(), Some(3));
    let r = p.read(".kgraph/flow_report.json");
    assert_eq!(r["summary"]["worker_calls_used"], 1);
    assert_eq!(r["summary"]["max_worker_calls"], 1);
}
#[test]
fn watch_deadline_and_unwatchable_packet() {
    let p = Project::new();
    p.workflow(json!([gate("review")]));
    let a = [
        "run",
        "flow.json",
        "--watch",
        "--watch-timeout",
        "0.04",
        "--quiet",
    ];
    assert!(stderr(&p.cli(&a, 4)).contains("watch timed out"));
    let mut g = gate("review");
    g["packet"] = json!({"outputs":["../outside","flow_state.json","flow_report.json","flow_state_progress.json","review_packet.json"]});
    p.workflow(json!([g]));
    assert!(stderr(&p.cli(&a, 4)).contains("no packet outputs"));
}
#[test]
fn progress_clear_and_question_sources() {
    let p = Project::new();
    let path = ".kgraph/state.json";
    p.write(
        path,
        &json!({"nodes":{"n":{"kind":"worker","status":"running"}}}),
    );
    let mut update = p.cmd(&[
        "progress",
        path,
        "--node",
        "n",
        "--item",
        "i",
        "--kind",
        "worker",
        "--progress",
        "25",
        "--output",
        "a",
        "--output",
        "b",
        "--agent",
        "QA",
        "--json",
    ]);
    // Having a Hub URL in the environment alone must not publish an update.
    let o = update.env("KGRAPH_HUB_URL", "invalid").output().unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    let d: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(d["n"]["items"]["i"]["outputs"], json!(["a", "b"]));
    let d = p.json(
        &[
            "progress", path, "--node", "n", "--item", "i", "--title", "renamed", "--json",
        ],
        0,
    );
    assert_eq!(d["n"]["items"]["i"]["progress"], 25.0);
    p.cli(
        &[
            "progress",
            path,
            "--node",
            "n",
            "--progress",
            "nan",
            "--item",
            "i",
        ],
        1,
    );
    p.write(
        "questions.json",
        &json!([{"question":"Choose","options":["A","B"]}]),
    );
    p.cli(
        &[
            "ask",
            path,
            "--node",
            "n",
            "--item",
            "q",
            "--questions-json",
            "questions.json",
        ],
        0,
    );
    assert_eq!(
        stdout(&p.cli(&["answer", path, "--node", "n", "--item", "q"], 4)),
        "No answer yet.\n"
    );
    let d = p.json(&["progress", path, "--node", "n", "--clear", "--json"], 0);
    assert!(d["n"]["items"]["i"].is_null() && d["n"]["items"]["q"].is_null());
}
#[test]
fn qa_records_scorecards_and_history() {
    let p = Project::new();
    let c = p.path("manual.json");
    kgraph_core::qa_metrics::init_qa_checklist(
        &c,
        "beta",
        &[
            json!({"id":"login","title":"Sign in","platform":"iOS","automated_status":"pass"}),
            json!({"id":"map","title":"Map","platform":"Android","automated_status":"pass"}),
        ],
        "release",
        None,
        None,
        None,
        None,
        false,
    )
    .unwrap();
    let out = p.cli(
        &[
            "qa",
            "record",
            "manual.json",
            "--case",
            "login",
            "--result",
            "pass",
            "--attempt",
            "2",
            "--tester",
            "Andrea",
            "--record-history",
            "--history",
            "history.jsonl",
        ],
        0,
    );
    assert_eq!(stdout(&out), "Recorded pass for login (attempt 2)\n");
    assert!(p.path("qa_scorecard.md").exists());
    let d = p.json(
        &[
            "qa",
            "record",
            "manual.json",
            "--case",
            "map",
            "--result",
            "fail",
            "--gap",
            "ui_regression",
            "--notes",
            "blank",
            "--scorecard",
            "--json",
        ],
        0,
    );
    assert_eq!(d["tester"], "Andrea");
    assert_eq!(d["scorecard"]["counts"]["failed"], 1);
    let md = stdout(&p.cli(
        &[
            "qa",
            "scorecard",
            "manual.json",
            "--md-out",
            "custom.md",
            "--json-out",
            "custom.json",
            "--record-history",
            "--history",
            "history.jsonl",
        ],
        0,
    ));
    assert_eq!(md, fs::read_to_string(p.path("custom.md")).unwrap());
    assert!(md.contains("Manual QA Pass %"));
    assert_eq!(
        p.json(&["qa", "scorecard", "manual.json", "--json"], 0),
        p.read("custom.json")
    );
    let h = p.json(
        &["qa", "history", "--history", "history.jsonl", "--json"],
        0,
    );
    assert_eq!(h["batches"], 1);
    assert_eq!(h["manual_pass_pct"], 50.0);
    assert!(
        stdout(&p.cli(&["qa", "history", "--history", "history.jsonl"], 0))
            .contains("Manual pass  50.0%")
    );
}

#[path = "support/ingest.rs"]
mod ingest;

#[test]
fn live_run_posts_started_paused_and_final_with_one_identity() {
    let project = Project::new();
    project.workflow(json!([gate("review")]));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let capture = std::thread::spawn(move || {
        (0..3)
            .map(|_| {
                let (stream, payload) = ingest::request(&listener);
                ingest::ack(stream, &payload);
                payload
            })
            .collect::<Vec<_>>()
    });
    project.cli(&["run", "flow.json", "--quiet", "--hub-url", &url], 4);
    let requests = capture.join().unwrap();
    let id = requests[0]["run"]["run_id"].as_str().unwrap();
    assert!(requests.iter().all(|p| p["run"]["run_id"] == id));
    assert_eq!(requests[0]["snapshot"]["event"]["event"], "node_start");
    assert_eq!(requests[0]["snapshot"]["nodes"][0]["status"], "running");
    assert_eq!(
        requests[1]["snapshot"]["event"]["event"],
        "node_awaiting_subagents"
    );
    let final_snapshot = &requests[2]["snapshot"];
    assert!(final_snapshot.get("event").is_none());
    assert_eq!(final_snapshot["status"], "awaiting_subagents");
    assert_eq!(
        final_snapshot["attention"]["input_gate"]["node_path"],
        "review"
    );
    assert!(std::path::Path::new(final_snapshot["state_file"].as_str().unwrap()).is_absolute());
}

#[cfg(unix)]
#[test]
fn watch_interrupt_returns_130() {
    let p = Project::new();
    p.workflow(json!([gate("review")]));
    let child = p
        .cmd(&[
            "run",
            "flow.json",
            "--quiet",
            "--watch",
            "--watch-interval",
            "30",
            "--watch-timeout",
            "5",
        ])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    read_when(&p.path(".kgraph/flow_report.json"), |r| {
        r["status"] == "awaiting_subagents"
    });
    std::thread::sleep(std::time::Duration::from_millis(80));
    assert!(Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap()
        .success());
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(130));
}

#[test]
fn ui_and_hub_binaries_serve_authenticated_loopback_requests() {
    use std::{
        io::{BufRead, BufReader},
        process::Child,
        sync::mpsc,
        thread,
        time::Duration,
    };
    struct Server(Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let p = Project::new();
    p.workflow(json!([shell("worker")]));
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    for (args, endpoint) in [
        (
            vec![
                "ui",
                "flow.json",
                "--title",
                "Test town",
                "--layout",
                "linear",
                "--read-only",
            ],
            "api/snapshot",
        ),
        (
            vec!["hub", "--watch-dir", ".", "--watch-dir", "nested"],
            "api/runs",
        ),
    ] {
        let mut cmd = p.cmd(&args);
        let mut server = Server(
            cmd.args(["--port", "0", "--poll-ms", "100", "--token", "test-access"])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdout = server.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let _ = tx.send(line);
            // Drain later access URLs until the server is stopped.
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });
        let line = rx.recv_timeout(Duration::from_secs(8)).unwrap();
        let url = reqwest::Url::parse(line.split_whitespace().last().unwrap()).unwrap();
        let endpoint = url.join(endpoint).unwrap();
        assert_eq!(client.get(endpoint.clone()).send().unwrap().status(), 401);
        let doc: Value = client
            .get(endpoint)
            .bearer_auth("test-access")
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        if args[0] == "ui" {
            assert_eq!(doc["title"], "Test town");
            assert_eq!(doc["nodes"][0]["path"], "worker");
        } else {
            assert!(doc["runs"].is_array());
        }
    }
}
