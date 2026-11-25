"""Shared fixtures for the standalone workflow tests."""

from contextlib import nullcontext
import json
from pathlib import Path
import runpy
import struct
import subprocess
import sys
from unittest.mock import patch
import zlib

from kgraph_runner import GraphFailure, GraphRunner


def png_chunk(name, payload):
    return struct.pack(">I", len(payload)) + name + payload + struct.pack(">I", zlib.crc32(name + payload))


def write_png(path, width, height, color_type, pixels, *, filters=(0,), palette=None, transparency=None):
    channels = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}[color_type]
    previous = bytes(width * channels)
    rows = bytearray()
    for y in range(height):
        row = bytes(sample for x in range(width) for sample in pixels(x, y))
        method = filters[y % len(filters)]
        rows.append(method)
        for index, value in enumerate(row):
            left = row[index - channels] if index >= channels else 0
            up = previous[index]
            upper_left = previous[index - channels] if index >= channels else 0
            estimate = left + up - upper_left
            paeth = min((left, up, upper_left), key=lambda neighbor: abs(estimate - neighbor))
            predictor = (0, left, up, (left + up) // 2, paeth)[method]
            rows.append((value - predictor) % 256)
        previous = row

    path.parent.mkdir(parents=True, exist_ok=True)
    header = struct.pack(">IIBBBBB", width, height, 8, color_type, 0, 0, 0)
    data = b"\x89PNG\r\n\x1a\n" + png_chunk(b"IHDR", header)
    if palette is not None:
        data += png_chunk(b"PLTE", palette)
    if transparency is not None:
        data += png_chunk(b"tRNS", transparency)
    compressed = zlib.compress(rows)
    midpoint = len(compressed) // 2
    data += png_chunk(b"IDAT", compressed[:midpoint]) + png_chunk(b"IDAT", compressed[midpoint:])
    path.write_bytes(data + png_chunk(b"IEND", b""))


def write_json(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data), encoding="utf-8")


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8"))


def run_workflow(test, root, name="workflow.py", *, status="passed", max_worker_calls=None, ctx=None,
                 builder_home=None):
    """Run the public Python API in a fresh process with the same persisted state."""
    options = json.dumps({
        "ctx": ctx or {}, "max_worker_calls": max_worker_calls,
        "builder_home": str(builder_home) if builder_home is not None else None,
    })
    proc = subprocess.run(
        [sys.executable, str(Path(__file__).resolve()), name, options], cwd=root,
        capture_output=True, text=True, encoding="utf-8", timeout=30,
    )
    test.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
    report = read_json(root / ".kgraph" / f"{Path(name).stem}_report.json")
    test.assertEqual(report["status"], status, report["error"])
    latest = {event["node"]: event["status"] for event in json.loads(proc.stdout)}
    return {**report, "cached_nodes": [node for node, result in latest.items() if result == "cached"]}


def _run(name, options):
    directory = Path.cwd() / ".kgraph"
    stem = Path(name).stem
    runner = GraphRunner(directory / f"{stem}_state.json", max_parallel_nodes=4,
                         max_worker_calls=options["max_worker_calls"], logger=lambda _line: None)
    status, error, packet_path = "passed", "", None
    builder_home = options["builder_home"]
    home = patch.object(Path, "home", return_value=Path(builder_home)) if builder_home else nullcontext()
    with home:
        try:
            runner.run(runpy.run_path(name)["build_graph"](), options["ctx"])
        except GraphFailure as exc:
            if exc.awaiting_subagents:
                status = "awaiting_subagents"
            elif exc.circuit_broken:
                status = "circuit_broken"
            elif exc.budget_exceeded:
                status = "budget_exceeded"
            else:
                status = "failed"
            error, packet_path = str(exc), exc.packet_path
    runner.write_run_report(directory / f"{stem}_report.json", status, error, packet_path)
    print(json.dumps(runner.trace))


if __name__ == "__main__":
    _run(sys.argv[1], json.loads(sys.argv[2]))
