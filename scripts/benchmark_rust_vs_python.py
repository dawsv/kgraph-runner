"""Differential benchmarks. Build release binaries and the bench_core example first.

PYTHONPATH=src python3 scripts/benchmark_rust_vs_python.py [--json-out RESULTS.json]
Cold hashing clears process-local digest caches, not the filesystem page cache.
Only CLI rows include process startup. HTTP latency includes reading the response.
"""

import argparse
import errno
import hashlib
import http.client
import json
import math
import os
import platform
import random
import re
import selectors
import shutil
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time
import zlib
from concurrent.futures import ThreadPoolExecutor
from html.parser import HTMLParser
from pathlib import Path

from kgraph_runner.engine import GraphRunner, TaskNode
from kgraph_runner.helpers import clear_hash_cache, hash_globs, hash_paths
from kgraph_runner.interactions import update_node_progress
from kgraph_runner.verifiers import inspect_png, verify_png_file

ROOT = Path(__file__).resolve().parents[1]
RELEASE = ROOT / "target" / "release"
PYTHON = [sys.executable, "-m", "kgraph_runner.cli"]
RUST = [RELEASE / "kgraph"]
TOWN = [RELEASE / "kgraph-town"]
CORE = [RELEASE / "examples" / "bench_core"]
ENV = {k: v for k, v in os.environ.items() if not k.startswith("KGRAPH_")}
ENV["PYTHONPATH"] = str(ROOT / "src")
CLOCK = time.perf_counter


def same(label, actual, expected):
    if actual != expected:
        # Show the first differing JSON field, including its two values.
        if isinstance(actual, dict) and isinstance(expected, dict):
            same(label + " keys", sorted(actual), sorted(expected))
            for key in sorted(expected):
                same(f"{label}.{key}", actual[key], expected[key])
        if isinstance(actual, list) and isinstance(expected, list):
            same(label + " length", len(actual), len(expected))
            for index, (a, b) in enumerate(zip(actual, expected)):
                same(f"{label}[{index}]", a, b)
        raise AssertionError(f"{label}: got {str(actual)[:300]}, expected {str(expected)[:300]}")


def elapsed(function, *args, **kwargs):
    start = CLOCK()
    value = function(*args, **kwargs)
    return CLOCK() - start, value


def command(argv):
    p = subprocess.run(argv, cwd=ROOT, env=ENV, capture_output=True, timeout=90)
    if p.returncode:
        raise RuntimeError(f"{argv}: exit {p.returncode}: {p.stderr.decode(errors='replace')}")
    return p.stdout


def core(*args):
    return json.loads(command(CORE + list(args)))


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def record(rows, name, py, rs, unit="ms", *, parity, higher=False):
    if unit == "ms":
        py, rs = py * 1000, rs * 1000
    rows.append(dict(metric=name, python=py, rust=rs, unit=unit,
                     factor=rs / py if higher else py / rs, parity=parity))


def medians(commands, repeats, validate):
    samples = [[], []]
    for i in range(repeats):
        # Alternate first implementation across pairs to balance filesystem and CPU effects.
        for side in (i % 2, 1 - i % 2):
            seconds, stdout = elapsed(command, commands[side])
            samples[side].append(seconds)
            validate(side, stdout)
    return [statistics.median(times) for times in samples]


def startup(rows):
    def check(_, stdout):
        for subcommand in [b"plan", b"run", b"render", b"ui"]:
            same("help includes " + subcommand.decode(), subcommand in stdout, True)

    values = medians([PYTHON + ["--help"], RUST + ["--help"]], 20, check)
    record(rows, "Cold CLI help (median of 20)", *values, parity="Both list commands")


def hash_benchmark(temp, rows):
    tree = temp / "tree"
    rng = random.Random(602)
    manifest = {}
    old = int(time.time()) - 60
    for i in range(2000):
        name = f"pkg{i % 10}/module{i // 10 % 5}/assets{i // 50 % 4}/file{i:04d}"
        # A glob selects three quarters of the tree.
        name += ".txt" if i % 4 == 0 else ".bin"
        payload = rng.randbytes(rng.randint(4096, 16384))
        path = tree / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(payload)
        os.utime(path, (old, old))
        manifest[name] = hashlib.sha256(payload).digest()
    for ignored in [".git", "node_modules"]:
        path = tree / "pkg0" / ignored / "ignored.bin"
        path.parent.mkdir()
        path.write_bytes(b"exclude this file")

    def oracle(names):
        digest = hashlib.sha256()
        for name in sorted(names):
            digest.update(os.fsencode(name) + b"\0" + manifest[name])
        return digest.hexdigest()

    native = core("hash", tree)
    for mode, fn, inputs, expected in [
        ("paths", hash_paths, ["."], oracle(manifest)),
        ("globs", hash_globs, ["**/*.bin"], oracle(n for n in manifest if n.endswith(".bin"))),
    ]:
        clear_hash_cache()
        for cache in ["cold", "warm"]:
            seconds, digest = elapsed(fn, inputs, root=tree)
            rs = native[mode][cache]
            same(f"hash_{mode} {cache} Python vs manifest", digest, expected)
            same(f"hash_{mode} {cache} Rust vs Python", rs["digest"], digest)
            record(rows, f"hash_{mode} {cache}", seconds, rs["seconds"],
                   parity="Exact SHA-256 and fixture digest")
    clear_hash_cache()


def make_png(path, idx):
    def chunk(kind, payload):
        return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", zlib.crc32(kind + payload))

    raw = bytearray()
    prev = bytes(512 * 4)
    for y in range(512):
        pixels = bytearray(512 * 4)
        if idx < 15 and 10 + idx * 3 <= y <= 488 - idx * 2:
            for x in range(7 + idx * 4, 500 - idx * 3):
                alpha = (64, 160, 255, 255)[(x // 32 + y // 28 + idx) % 4]
                offset = 4 * x
                pixels[offset:offset + 4] = bytes(((x + idx * 7) % 256, y % 256, (x ^ y) % 256, alpha))
        # Alternate None/Up filtered images with identical RGBA interpretation.
        if idx % 2:
            raw.append(2)
            raw.extend((a - b) & 255 for a, b in zip(pixels, prev))
        else:
            raw.append(0)
            raw.extend(pixels)
        prev = pixels
    header = struct.pack(">IIBBBBB", 512, 512, 8, 6, 0, 0, 0)
    path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) +
                     chunk(b"IDAT", zlib.compress(raw, level=3)) + chunk(b"IEND", b""))


def png_benchmark(temp, rows):
    directory = temp / "images"
    directory.mkdir()
    files = [directory / f"sample-{i:02d}.png" for i in range(16)]
    for i, path in enumerate(files):
        make_png(path, i)
    seconds, checks = elapsed(lambda: [verify_png_file(path) for path in files])
    native = core("png", directory)
    same("PNG count", len(native["images"]), 16)
    for i, (path, check, result) in enumerate(zip(files, checks, native["images"])):
        stats = inspect_png(path)
        for key in ["width", "height", "bbox"]:
            same(f"{path.name}.{key}", result[key], stats[key])
        same("PNG fixture dimensions", (stats["width"], stats["height"]), (512, 512))
        same("PNG name", result["name"], path.name)
        same("PNG decision", (result["passed"], result["signature"]), check[:2])
        same("PNG expected decision", check[0], i < 15)
        same(f"{path.name}.opaque_ratio", abs(result["opaque_ratio"] - stats["opaque_ratio"]) <= 1e-6, True)
    record(rows, "verify_png_file ×16", seconds, native["seconds"], parity="15 pass, 1 blank, matching metrics")


def workflow(temp):
    nodes = []

    def add(name, deps):
        nodes.append({"type": "shell", "id": name, "deps": deps, "command": ["/usr/bin/true"]})
        return name

    root = add("start", [])
    previous = [root] * 4
    for wave in range(4):
        for lane in range(4):
            stem = f"stage-{wave}-lane-{lane}"
            branches = [add(f"{stem}-{side}", [previous[lane]]) for side in ["left", "right"]]
            previous[lane] = add(f"{stem}-join", branches)
    add("finish", previous)
    spec = temp / "workflow.json"
    write_json(spec, {"nodes": nodes})
    py = temp / "workflow.py"
    py.write_text('''import json
from pathlib import Path
from kgraph_runner.engine import sha256_json
from kgraph_runner.helpers import hash_globs, hash_paths, shell_task

def build_graph():
    base = Path(__file__).parent
    specs = json.loads((base / "workflow.json").read_text())["nodes"]
    def task(spec):
        signature = sha256_json(spec)
        def inputs(ctx):
            return sha256_json([signature, hash_globs([], root=base), hash_paths([], root=base)])
        return shell_task(spec["id"], spec["command"], deps=tuple(spec["deps"]),
                          cwd=base, input_hash_fn=inputs)
    return [task(spec) for spec in specs]
''', encoding="utf-8")
    return nodes, spec, py


def plan_status(entries, nodes):
    same("planned node count", len(entries), len(nodes))
    same("planned paths and dependencies", {e["path"]: e["deps"] for e in entries},
         {n["id"]: n["deps"] for n in nodes})
    return {e["path"]: e["status"] for e in entries}


def events_status(events, ids, expected):
    same("trace event count", len(events), len(ids))
    result = {e["node"]: e["status"] for e in events}
    same("trace statuses", result, dict.fromkeys(ids, expected))
    return result


def in_process_dag(temp, nodes, spec, rows):
    ids = [n["id"] for n in nodes]
    native = core("dag", spec, temp / "native-core-state.json")
    graph = [TaskNode(n["id"], "code", lambda ctx, attempt: None, lambda ctx: "input",
                      deps=tuple(n["deps"])) for n in nodes]
    for phase in ["plan", "run", "cached"]:
        runner = GraphRunner(temp / "python-core-state.json", max_parallel_nodes=8, logger=lambda _: None)
        rs = native[phase]
        if phase == "plan":
            seconds, entries = elapsed(runner.plan, graph, {})
            py_status = plan_status(entries, nodes)
            same("native plan", plan_status(rs["plan"], nodes), py_status)
            same("initial core plan", py_status, dict.fromkeys(ids, "will_run"))
        else:
            seconds, _ = elapsed(runner.run, graph, {})
            status = "passed" if phase == "run" else "cached"
            same(phase, events_status(rs["events"], ids, status), events_status(runner.trace, ids, status))
        record(rows, f"DAG core {phase}", seconds, rs["seconds"], parity=f"All 50 {phase} statuses")


def cli_dag(temp, nodes, spec, py, rows):
    ids = {n["id"] for n in nodes}
    times = [[], []]
    results = [[], []]
    for side, (cli, path) in enumerate([(PYTHON, py), (RUST, spec)]):
        state = temp / f"cli-{side}-state.json"
        report = temp / f"cli-{side}-report.json"
        args = [path, "--state", state]
        seconds, stdout = elapsed(command, cli + ["plan"] + args + ["--json"])
        times[side].append(seconds)
        plan = plan_status(json.loads(stdout), nodes)
        same("initial CLI plan", plan, dict.fromkeys(ids, "will_run"))
        results[side].append(plan)
        for phase in ["run", "cached"]:
            seconds, _ = elapsed(command, cli + ["run"] + args + ["--quiet", "--max-parallel", "8", "--report", report])
            times[side].append(seconds)
            telemetry = json.loads(report.read_bytes())
            same("run report", telemetry["status"], "passed")
            same("checkpoint statuses", {k: v["status"] for k, v in json.loads(state.read_bytes())["nodes"].items()},
                 dict.fromkeys(ids, "passed"))
            summary = telemetry["summary"]
            counts = (summary["total_events"], summary["ran_passed"], summary["cached"], summary["failed"])
            same("run and cache counts", counts, (50, 50, 0, 0) if phase == "run" else (50, 0, 50, 0))
            cached = sorted(telemetry["cached_nodes"])
            same("CLI cache hit identities", cached, [] if phase == "run" else sorted(ids))
            results[side].append((cached, counts))
    same("CLI DAG parity", results[0], results[1])
    for i, phase in enumerate(["plan", "run", "cached"]):
        record(rows, f"DAG CLI {phase}", times[0][i], times[1][i], parity=f"All 50 {phase} statuses")


def town_fixture(temp, nodes):
    directory = temp / ".kgraph"
    directory.mkdir()
    state = directory / "diamond_state.json"
    report = directory / "diamond_report.json"
    # Produce real mixed-run telemetry outside the timed CLI rows. Retain the
    # graph dependencies in the state-only fixture for the Town viewer.
    shutil.copyfile(temp / "cli-0-state.json", state)
    retry = [arg for n in nodes[8:] for arg in ("--retry", n["id"])]
    command(PYTHON + ["run", temp / "workflow.py", "--state", state, "--report", report,
                      "--quiet", "--max-parallel", "8", *retry])
    checkpoints = json.loads(state.read_bytes())
    for spec in nodes:
        checkpoints["nodes"][spec["id"]]["deps"] = spec["deps"]
    write_json(state, checkpoints)
    update_node_progress(state, "finish", item_id="package", kind="worker", status="passed",
                         title="Package artifacts", progress=100, agent="builder")
    update_node_progress(state, "finish", item_id="review", deps=["package"], kind="verifier",
                         status="passed", title="Review package", progress=100, agent="qa")
    return ["--state", state, "--report", report]


class ExportedTown(HTMLParser):
    def __init__(self):
        super().__init__()
        self.part = None
        self.doctype = None
        self.tags = set()
        self.closed = set()
        self.content = {}
        self.references = []

    def handle_decl(self, decl):
        self.doctype = decl.lower()

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        self.tags.add(tag)
        if tag in ("script", "style"):
            self.part = attrs.get("id", tag)
        for attr in ("src", "srcset", "href", "xlink:href", "poster"):
            url = attrs.get(attr)
            if url and tag != "a" and not url.startswith(("#", "data:")):
                self.references.append(url)

    def handle_endtag(self, tag):
        self.closed.add(tag)
        if tag in ("script", "style"):
            self.part = None

    def handle_data(self, data):
        if self.part:
            self.content[self.part] = self.content.get(self.part, "") + data


def check_snapshot(snapshot, ids):
    same("snapshot node count", len(snapshot["nodes"]), 50)
    same("snapshot paths", {n["path"] for n in snapshot["nodes"]}, ids)
    same("snapshot cache count", snapshot["counts"]["cached"], 8)
    same("snapshot passed count", snapshot["counts"]["passed"], 42)
    finish = next(n for n in snapshot["nodes"] if n["path"] == "finish")
    same("sidecar subtasks", sorted(n["id"] for n in finish["sub_graph"]["nodes"]), ["package", "review"])
    same("snapshot warnings", snapshot["warning"], None)


def stable_snapshot(snapshot):
    # Only the top-level time of snapshot assembly may differ.
    result = snapshot.copy()
    del result["generated_at"]
    return result


def render_benchmark(temp, town_args, ids, rows):
    files = [temp / "python.html", temp / "rust.html"]
    snapshots = [None, None]

    def validate(side, _):
        page = ExportedTown()
        page.feed(files[side].read_text(encoding="utf-8"))
        page.close()
        same("export HTML declaration", page.doctype, "doctype html")
        structural = {"html", "head", "body", "style", "script"}
        same("complete exported document", structural <= page.tags & page.closed, True)
        same("external export assets", page.references, [])
        same("inline executable script", bool(page.content.get("script", "").strip()), True)
        styles = page.content.get("style", "")
        same("inline stylesheet", bool(styles.strip()), True)
        for url in re.findall(r'url\(\s*["\']?([^)"\']+)', styles, re.I):
            same("embedded CSS URL", url.startswith(("data:", "#")), True)
        options = json.loads(page.content["town-options"])
        same("static export polling URL", options["live"], None)
        snapshot = json.loads(page.content["town-snapshot"])
        check_snapshot(snapshot, ids)
        snapshots[side] = stable_snapshot(snapshot)

    # Embed mode omits the optional live-server manifest and favicon links.
    commands = [cli + ["render", *town_args, "--embed", "--output", output]
                for cli, output in zip([PYTHON, RUST], files)]
    med = medians(commands, 10, validate)
    same("static exported snapshot", snapshots[1], snapshots[0])
    record(rows, "Town render --embed (median of 10)", *med, parity="Self-contained HTML, 50 nodes")
    return snapshots[0]


def rss_kib(pid):
    return int(subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], timeout=2))


def request(port):
    start = CLOCK()
    # Each logical GET is idempotent. Include all attempts in the latency;
    # expose transport failures and never retry HTTP/JSON parity errors.
    resets = 0
    while True:
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
        try:
            # On Darwin a reset between connect() and TCP_NODELAY in
            # HTTPConnection.connect() can surface as EINVAL from setsockopt.
            try:
                conn.connect()
            except OSError as exc:
                if sys.platform == "darwin" and exc.errno == errno.EINVAL:
                    raise ConnectionResetError("connection lost during TCP setup") from exc
                raise
            conn.request("GET", "/api/snapshot")
            response = conn.getresponse()
            body = response.read()
            seconds = CLOCK() - start
            same("HTTP status", response.status, 200)
            return seconds, body, resets
        except (ConnectionResetError, BrokenPipeError, http.client.RemoteDisconnected):
            resets += 1
            if CLOCK() - start >= 10:
                raise
            time.sleep(min(0.001 * resets, 0.05))
        finally:
            conn.close()


def serve_benchmark(cli, town_args, ids, baseline):
    with tempfile.TemporaryFile() as errors:
        process = subprocess.Popen(cli + ["ui", *town_args, "--host", "127.0.0.1", "--port", "0"],
                                   stdout=subprocess.PIPE, stderr=errors, cwd=ROOT, env=ENV)
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                if not selector.select(15):
                    raise RuntimeError("Server did not announce its port within 15 seconds")
            line = process.stdout.readline().decode()
            match = re.search(r"http://127\.0\.0\.1:(\d+)/", line)
            if not match:
                errors.seek(0)
                raise RuntimeError(f"Server startup: {line!r} {errors.read(4096).decode()}")
            port = int(match.group(1))
            # Prime both servers' snapshot caches with the same readiness request.
            reference = json.loads(request(port)[1])
            check_snapshot(reference, ids)
            reference = stable_snapshot(reference)
            same("live vs static snapshot", reference, baseline)
            idle = rss_kib(process.pid)
            samples = []
            stop = threading.Event()

            def sample():
                while not stop.is_set():
                    samples.append(rss_kib(process.pid))
                    stop.wait(0.01)

            with ThreadPoolExecutor(max_workers=16) as pool, ThreadPoolExecutor(max_workers=1) as monitor:
                sampling = monitor.submit(sample)
                try:
                    start = CLOCK()
                    responses = list(pool.map(request, [port] * 300))
                    seconds = CLOCK() - start
                finally:
                    stop.set()
                    sampling.result()
            latencies = sorted(latency for latency, _, _ in responses)
            same("HTTP measured request count", len(responses), 300)
            for index, (_, body, _) in enumerate(responses):
                same(f"HTTP snapshot {index + 1}", stable_snapshot(json.loads(body)), reference)
            return {"rate": len(responses) / seconds, "median": statistics.median(latencies),
                    "p95": latencies[math.ceil(len(latencies) * .95) - 1],
                    "idle": idle / 1024, "active_peak": max(samples) / 1024,
                    "transport_retries": sum(resets for _, _, resets in responses)}
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            process.stdout.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json-out", type=Path)
    args = parser.parse_args()
    for binary in (RUST[0], TOWN[0], CORE[0]):
        if not binary.is_file():
            parser.error(f"missing {binary}; build the release binaries and bench_core first")
    rows = []
    with tempfile.TemporaryDirectory(prefix="kgraph-parity-") as directory:
        temp = Path(directory).resolve()
        startup(rows)
        hash_benchmark(temp, rows)
        png_benchmark(temp, rows)
        nodes, spec, py = workflow(temp)
        in_process_dag(temp, nodes, spec, rows)
        cli_dag(temp, nodes, spec, py, rows)
        town_args = town_fixture(temp, nodes)
        ids = {n["id"] for n in nodes}
        snapshot = render_benchmark(temp, town_args, ids, rows)
        servers = [serve_benchmark(cli, town_args, ids, snapshot) for cli in (PYTHON, TOWN)]
        for key, name, unit in [("rate", "HTTP throughput", "req/s"), ("median", "HTTP median", "ms"),
                               ("p95", "HTTP p95", "ms"), ("idle", "Server idle RSS", "MiB"),
                               ("active_peak", "Server sampled active peak RSS", "MiB")]:
            record(rows, name, servers[0][key], servers[1][key], unit, higher=key == "rate",
                   parity="300/300 snapshots per server" if unit != "MiB" else "Same server workload")
    output = {"platform": platform.platform(), "python_version": platform.python_version(),
              "rustc": command(["rustc", "--version"]).decode().strip() if shutil.which("rustc") else None,
              "http_transport_retries": dict(zip(["python", "rust"], [s["transport_retries"] for s in servers])),
              "parity": "passed", "results": rows}
    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        write_json(args.json_out, output)
    print(f"Transport retries across 300 logical GETs: {output['http_transport_retries']}")
    print(f"{'Metric':38s} {'Python':>10s} {'Rust':>10s} {'Unit':>6s} {'Factor':>8s}  Parity")
    for row in rows:
        print(f"{row['metric']:38s} {row['python']:10.3f} {row['rust']:10.3f} {row['unit']:>6s}"
              f" {row['factor']:7.2f}x  {row['parity']}")


if __name__ == "__main__":
    main()
