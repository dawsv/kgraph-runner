"""Packet, verification and repair state shared by the workflow builders."""

from .._files import read_object, write_json
from ..engine import GraphFailure, TaskNode, sha256_file, sha256_json
from ..helpers import subagent_gate


def file_hash(path):
    return sha256_file(path) if path is not None and path.is_file() else None


def fingerprint(*values):
    return sha256_json(values)


def files_hash(paths):
    return fingerprint([(str(path), file_hash(path)) for path in paths])


def read_optional(path):
    try:
        return read_object(path)
    except FileNotFoundError:
        return {}


def revision(path):
    try:
        number = read_optional(path).get("revision", 0)
    except (OSError, ValueError):
        return 0
    return number if type(number) is int and number >= 0 else 0


def with_retry_route(loop, source, target, label="if fails → repair"):
    """Attach the display route without adding a runner dependency."""
    loop.retry_from = source
    loop.retry_to = target
    loop.retry_label = f"{label} (max {loop.max_retries})"
    return loop


class Repairs:
    """Remember rejected output and repair history across CLI processes."""

    def __init__(self, path, input_hash, artifacts, metadata=None, *, artifact_hash=None, minimum_revision=0):
        self.path = path
        self.input_hash = input_hash
        self.artifact_hash = artifact_hash if artifact_hash is not None else lambda: files_hash(artifacts)
        self.metadata = metadata
        self.minimum_revision = minimum_revision

    def state(self):
        state = read_optional(self.path)
        return state if state.get("input_hash") == self.input_hash() else {}

    def instructions(self):
        state = self.state()
        return {"revision": max(state.get("revision", 0), self.minimum_revision),
                "repair_feedback": state.get("feedback", [])}

    def waiting_reason(self):
        state = self.state()
        expected = max(state.get("revision", 0), self.minimum_revision)
        if self.metadata is not None and expected > revision(self.metadata):
            return f"{self.metadata.name} must declare revision {expected} or newer"
        if state.get("rejected_hash") == self.artifact_hash():
            return "The rejected artifacts still have the same contents"
        return None

    def repair(self, ctx, signature, detail, dirty, max_retries):
        state = self.state()
        feedback = state.get("feedback", [])
        unresolved = state.get("consecutive_repairs", 0)
        if unresolved and feedback[-1]["error_signature"] == signature:
            # The loop rechecks the same result in this process so the runner
            # records its native two-consecutive-failure circuit breaker.
            return []
        if unresolved >= max_retries:
            raise GraphFailure(self.path.parent.name, f"Exhausted {max_retries} repairs ({signature}): {detail}")
        number = max(state.get("revision", 0), self.minimum_revision)
        if self.metadata is not None:
            number = max(number, revision(self.metadata))
        number += 1
        feedback = [*feedback, {"revision": number, "error_signature": signature, "detail": detail}]
        ctx.setdefault("repair_feedback", {})[str(self.path)] = feedback
        write_json(self.path, {
            "input_hash": self.input_hash(), "revision": number, "feedback": feedback,
            "rejected_hash": self.artifact_hash(), "consecutive_repairs": unresolved + 1,
        })
        return dirty

    def resolved(self):
        state = self.state()
        if state.get("consecutive_repairs"):
            state.update(rejected_hash=None, consecutive_repairs=0)
            write_json(self.path, state)


def packet_worker(node_id, *, packet_path, artifacts, input_hash, packet, deps=(),
                  repairs=None, enabled=lambda: True, verify=None, resource=None):
    """Bind a completed worker delivery to the inputs it was generated for."""
    receipt = packet_path.with_suffix(".receipt.json")
    artifact_paths = artifacts if callable(artifacts) else lambda _ctx: artifacts

    def verify_delivery(ctx, paths):
        reason = repairs.waiting_reason() if repairs else None
        previous = read_optional(receipt)
        if previous and previous["input_hash"] != input_hash() and previous["artifact_hash"] == files_hash(paths):
            reason = reason or "The source inputs changed. Update the existing artifacts for the current inputs."
        if not reason and verify is not None:
            return verify(ctx, paths)
        return not reason, "", reason or "Artifacts delivered"

    def write_packet(ctx):
        return {
            **packet(ctx), "input_hash": input_hash(), "outputs": [str(path) for path in artifact_paths(ctx)],
            "delivery_note": "If a source input changed but your revalidated output has the same content, "
                             "include this packet's input_hash in a delivered JSON file.",
            **(repairs.instructions() if repairs else {}),
        }

    gate = subagent_gate(
        node_id, packet_path=packet_path, artifact_paths=artifact_paths, packet_fn=write_packet,
        verify_fn=verify_delivery, input_hash_fn=lambda _ctx: input_hash(), deps=deps, resource=resource,
    )
    run_gate = gate.run_fn

    def run(ctx, attempt):
        if not enabled():
            return "Worker disabled"
        detail = run_gate(ctx, attempt)
        write_json(receipt, {"input_hash": input_hash(), "artifact_hash": files_hash(artifact_paths(ctx))})
        return detail

    gate.kind = "worker"
    gate.run_fn = run
    gate.input_hash_fn = lambda _ctx: fingerprint(input_hash(), repairs.instructions() if repairs else None, enabled())
    gate.outputs_fn = lambda ctx: (*artifact_paths(ctx), receipt) if enabled() else ()
    return gate


def record_verification(node_id, *, path, input_hash, check, deps=(), resource=None, cacheable=True):
    def run(ctx, _attempt):
        result = check(ctx)
        ctx.setdefault("verification_results", {})[str(path)] = result
        ok, signature, detail = result
        write_json(path, {"ok": ok, "error_signature": signature, "detail": detail})
        return f"{signature}: {detail}" if signature else detail

    def describe(_ctx, detail):
        # A verifier delivers a result to the loop. Only the loop can apply
        # targeted repair across multiple child nodes.
        return True, "", detail

    return TaskNode(
        id=node_id, kind="verifier", deps=deps, resource=resource, cacheable=cacheable,
        input_hash_fn=lambda _ctx: input_hash(), outputs_fn=lambda _ctx: (path,), run_fn=run, verify_fn=describe,
    )


def verification(path):
    record = read_object(path)
    return record["ok"], record["error_signature"], record["detail"]
