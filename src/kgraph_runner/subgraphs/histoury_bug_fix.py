"""Select, verify, and release a Histoury bug-fix batch with human approval."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence

from .._files import read_object, write_json
from ..engine import GraphFailure, LoopSubgraphNode, SplitterNode, SubgraphNode, TaskNode, sha256_bytes
from ..helpers import hash_globs, subagent_gate
from ..qa_metrics import _record_results, init_qa_checklist, write_qa_scorecard
from ..verifiers import inspect_png
from ._common import (
    Repairs, file_hash, files_hash, fingerprint, packet_worker, read_optional, record_verification, revision,
    verification, with_retry_route,
)
from .bug_fix import _bug_qa_case, _command, _nonempty_text, create_desktop_bug_fix_subgraph, require_bug_id
from .version_history import _IMAGE_ERRORS, _require_release_id, create_version_history_subgraph


_PLATFORMS = {"desktop": ("WebGL", "Windows", "macOS"), "app": ("iOS", "Android")}


def _batch_bugs(batch, batch_id, product, repos):
    if batch.get("batch_id") != batch_id or batch.get("product") != product:
        raise ValueError(f"Expected batch_id {batch_id!r} and product {product!r}")
    bugs = batch.get("bugs")
    if not isinstance(bugs, list) or not bugs:
        raise ValueError("Provide a non-empty bugs list")
    result, seen = [], set()
    for bug in bugs:
        if not isinstance(bug, dict):
            raise ValueError("Each bug must be an object")
        bug_id = require_bug_id(bug.get("id"))
        if bug_id.casefold() in seen:
            raise ValueError(f"Duplicate bug ID {bug_id!r}")
        seen.add(bug_id.casefold())
        repo = bug.get("repo")
        if repo is None and len(repos) == 1:
            repo = next(iter(repos))
        if not isinstance(repo, str) or repo not in repos:
            raise ValueError(f"Bug {bug_id}: choose a repo from {', '.join(repos)}")
        for field in ("title", "description", "platform", "expected"):
            if not _nonempty_text(bug.get(field)):
                raise ValueError(f"Bug {bug_id}: provide a non-empty {field}")
        platform = bug["platform"].strip().casefold()
        if platform not in {name.casefold() for name in _PLATFORMS[product]}:
            raise ValueError(f"Bug {bug_id}: platform must be one of {', '.join(_PLATFORMS[product])}")
        steps = bug.get("repro_steps")
        if not (_nonempty_text(steps) or (isinstance(steps, list) and steps and all(map(_nonempty_text, steps)))):
            raise ValueError(f"Bug {bug_id}: provide reproduction steps as text or a list of steps")
        capture = bug.get("requires_ui_capture", True)
        if type(capture) is not bool:
            raise ValueError(f"Bug {bug_id}: requires_ui_capture must be a boolean")
        _command(bug.get("repro_cmd"), f"Bug {bug_id} repro_cmd")
        result.append({**bug, "repo": repo, "requires_ui_capture": capture})
    return result


def _capture_check(provenance, bugs, work_dir, stage, revision_key, repairs):
    try:
        doc = read_object(provenance)
    except (OSError, ValueError) as exc:
        return False, "invalid_provenance", str(exc)
    number = doc.get("revision")
    captures = doc.get("captures")
    if type(number) is not int or number < repairs.instructions()["revision"] or not isinstance(captures, dict):
        return False, "invalid_provenance", f"{provenance} must provide the requested revision and a captures object"
    for bug in bugs:
        bug_id = bug["id"]
        path = work_dir / "media" / f"{bug_id}-{stage}.png"
        capture = captures.get(bug_id)
        fields = ("path", revision_key, "platform", "captured_at", "screen_state")
        if not isinstance(capture, dict) or not all(_nonempty_text(capture.get(field)) for field in fields):
            return False, "invalid_provenance", f"{bug_id} needs {', '.join(fields)} in {provenance}"
        try:
            if (work_dir / capture["path"]).resolve() != path.resolve() or not path.resolve().is_relative_to(work_dir):
                return False, "invalid_provenance", f"{bug_id}: capture path must reference {path}"
            if capture["platform"].strip().casefold() != bug["platform"].strip().casefold():
                return False, "invalid_provenance", f"{bug_id}: capture platform must match {bug['platform']}"
            inspect_png(path)
        except _IMAGE_ERRORS as exc:
            return False, f"invalid_{stage}_png", f"{path}: {exc}"
        if stage == "after" and file_hash(path) == file_hash(path.with_name(f"{bug_id}-before.png")):
            return False, "after_matches_before", f"{bug_id}: capture the verified screen for {path.name}"
    return True, "", f"Validated {len(bugs)} {stage} captures and their provenance"


def _capture_loop(stage, work_dir, batch_path, ui_bugs, dependency, *, feedback_path=None):
    node_id = f"capture_{stage}"
    before = stage == "before"
    worker_id = "capture_baseline" if before else "capture_verified_after"
    verifier_id = f"verify_{stage}"
    provenance = work_dir / "evidence" / f"{stage}_provenance.json"
    result_path = work_dir / "evidence" / f"{stage}_result.json"
    revision_key = "source_revision" if before else "verified_revision"
    max_retries = 3

    def paths(_ctx):
        return (provenance, *(work_dir / "media" / f"{bug['id']}-{stage}.png" for bug in ui_bugs()))

    def source_hash():
        inputs = [batch_path]
        if not before:
            inputs.extend(work_dir / "bugs" / bug["id"] / "final_summary.json" for bug in ui_bugs())
            inputs.extend(work_dir / "media" / f"{bug['id']}-before.png" for bug in ui_bugs())
            inputs.append(work_dir / "evidence" / "before_provenance.json")
        if feedback_path is not None and feedback_path.is_file():
            inputs.append(feedback_path)
        return files_hash(inputs)

    repairs = Repairs(work_dir / "evidence" / f"{stage}_repair_state.json", source_hash, (),
                      metadata=provenance, artifact_hash=lambda: files_hash(paths(None)))

    def packet(_ctx):
        (work_dir / "media").mkdir(parents=True, exist_ok=True)
        instructions = (
            "Capture the affected screen in the current runtime before the first edit. Preserve this baseline."
            if before else
            "Capture the verified fix on the same platform, viewport, and screen state as the before screenshot."
        )
        return {
            "stage": worker_id, "bugs": ui_bugs(), "bug_batch_path": str(batch_path),
            "media_dir": str(work_dir / "media"), "provenance_path": str(provenance),
            "capture_fields": ["path", revision_key, "platform", "captured_at", "screen_state"],
            "instructions": [
                instructions, "Use genuine runtime screenshots and keep secrets and personal information out.",
                f"Save each screenshot as media/<bug_id>-{stage}.png. Write {provenance.name} "
                "with the requested integer revision and a captures object keyed by bug ID.",
            ],
        }

    worker = packet_worker(
        worker_id, packet_path=work_dir / "evidence" / f"{stage}_packet.json",
        artifacts=paths, input_hash=source_hash, packet=packet, repairs=repairs,
    )
    run_worker = worker.run_fn

    def capture(ctx, attempt):
        if not ui_bugs():
            write_json(provenance, {"revision": 0, "captures": {}})
            return "Batch has no UI captures"
        return run_worker(ctx, attempt)

    worker.run_fn = capture
    worker_outputs = worker.outputs_fn
    worker.outputs_fn = lambda ctx: worker_outputs(ctx) if ui_bugs() else (provenance,)

    def verify(_ctx):
        result = verification(result_path)
        if result[0]:
            repairs.resolved()
        return result

    loop = LoopSubgraphNode(
        id=node_id, deps=(dependency,), max_retries=max_retries, verify_fn=verify,
        repair_fn=lambda ctx, _attempt, sig, detail: repairs.repair(
            ctx, sig, detail, [worker_id, verifier_id], max_retries,
        ),
        nodes=[
            worker,
            record_verification(
                verifier_id, path=result_path,
                input_hash=lambda: fingerprint(source_hash(), files_hash(paths(None)), repairs.instructions()["revision"]),
                check=lambda _ctx: _capture_check(provenance, ui_bugs(), work_dir, stage, revision_key, repairs),
                deps=(worker_id,),
            ),
        ],
    )
    return with_retry_route(loop, verifier_id, worker_id, "if fails → recapture")


def _approval_qa_results(value):
    if isinstance(value, dict):
        entries = value.items()
    elif isinstance(value, list):
        if any(not isinstance(item, dict) for item in value):
            raise ValueError("Each manual_qa list item must be an object")
        entries = ((item.get("id") or item.get("case_id"), item) for item in value)
    else:
        raise ValueError("manual_qa must be a mapping or list of QA results")
    results, seen = [], set()
    for case_id, item in entries:
        if not _nonempty_text(case_id) or case_id in seen:
            raise ValueError("manual_qa results must identify unique bug IDs")
        seen.add(case_id)
        if isinstance(item, str):
            item = {"manual_qa": item}
        if not isinstance(item, dict):
            raise ValueError(f"Invalid manual_qa result for {case_id}")
        results.append({
            "case_id": case_id, "manual_qa": item.get("manual_qa", item.get("result")),
            "resolved_on_attempt": item.get("resolved_on_attempt"), "gap_type": item.get("gap_type"),
            "notes": item.get("notes"),
        })
    return results


def _review_gate(review_dir, review_paths, feedback_path, selected_bugs, max_retries, work_dir):
    approval = review_dir / "approval.json"
    packet_path = review_dir / "user_approval_packet.json"
    qa_checklist = review_dir / "manual_qa.json"

    def previous_packet():
        try:
            return read_object(packet_path)
        except (OSError, ValueError):
            return {}

    def stale_approval(current, review_hash):
        previous = previous_packet()
        return bool(current and (previous.get("review_sha256") != review_hash
                                 or previous.get("stale_approval_sha256") == current
                                 or read_optional(feedback_path).get("rejected_approval_sha256") == current))

    def packet(_ctx):
        review_hash, current = files_hash(review_paths), file_hash(approval)
        return {
            "stage": "user_verify_gate", "review_summary": str(review_paths[1]),
            "review_summary_json": str(review_paths[0]), "review_sha256": review_hash,
            "manual_qa_checklist": str(qa_checklist), "qa_scorecard": str(review_dir / "qa_scorecard.md"),
            "stale_approval_sha256": current if stale_approval(current, review_hash) else None,
            "outputs": [str(approval)],
            "instructions": [
                "Daniel must verify the fixes, before and after screenshots, and ZIP in review_summary.md.",
                "After inspecting this review, write approval.json with the decisions below. "
                "Set either action to false to skip it. Include review_sha256 from this packet.",
                "Record your re-test in manual_qa.json or include a manual_qa mapping of bug IDs to results.",
                "To request fixes, set approved to false and optionally include notes and a rejected_bugs list. "
                "If both rejected_bugs and manual failures are absent, the whole batch will return for fixes.",
            ],
            "approval_example": {
                "approved": True, "approved_by": "Daniel", "approve_upload_zip": True,
                "approve_commit_main": True, "notes": "", "review_sha256": review_hash,
            },
        }

    def read_decision(*, sync_qa=False):
        try:
            raw = approval.read_bytes()
            doc = json.loads(raw.decode("utf-8"))
            if not isinstance(doc, dict) or type(doc.get("approved")) is not bool:
                raise ValueError("Provide a JSON object with an approved boolean.")
            approved = doc["approved"]
            reviewer = doc.get("approved_by") if approved else doc.get("rejected_by", doc.get("approved_by", "Daniel"))
            notes = doc.get("notes", "")
            if (not _nonempty_text(reviewer) or not isinstance(notes, str)
                    or any(type(doc.get(key, True)) is not bool for key in ("approve_upload_zip", "approve_commit_main"))):
                raise ValueError("Provide a reviewer name, text notes, and boolean action decisions.")
            rejected_bugs = None
            if not approved:
                bug_ids = [bug["id"] for bug in selected_bugs()]
                rejected_bugs = doc.get("rejected_bugs")
                if "rejected_bugs" in doc and (not isinstance(rejected_bugs, list)
                                               or any(bug_id not in bug_ids for bug_id in rejected_bugs)):
                    raise ValueError("rejected_bugs must list selected bug IDs.")
        except (OSError, ValueError) as exc:
            return {}, (False, "invalid_approval", f"invalid_approval: {exc}"), None

        current, review_hash = sha256_bytes(raw), files_hash(review_paths)
        default_hash = None if feedback_path.is_file() else review_hash
        if doc.get("review_sha256", default_hash) != review_hash or stale_approval(current, review_hash):
            return doc, (False, "stale_approval", "stale_approval: Review the current bundle and write a fresh approval."), None
        qa_failures = {}
        if sync_qa:
            try:
                results = _approval_qa_results(doc["manual_qa"]) if "manual_qa" in doc else []
                checklist = _record_results(qa_checklist, results, reviewer if results else None)
                qa_failures = {case["id"]: case for case in checklist["cases"] if case["manual_qa"] == "fail"}
            except (OSError, ValueError) as exc:
                return doc, (False, "invalid_approval", f"invalid_approval: {exc}"), None
        if approved:
            return doc, (True, "", f"Release review approved by {reviewer.strip()}"), None
        rejected_bugs = list(dict.fromkeys([*(rejected_bugs or []), *qa_failures]))
        feedback = {"notes": notes, "rejected_bugs": rejected_bugs or bug_ids,
                    "manual_qa": qa_failures,
                    "rejected_by": reviewer.strip(), "rejected_approval_sha256": current}
        return doc, (False, "user_rejected", "user_rejected: Daniel requested fixes before release."), feedback

    def verify_delivery(ctx, _paths):
        _, result, feedback = read_decision(sync_qa=True)
        ctx.setdefault("verification_results", {})[str(approval)] = result
        if feedback is not None:
            ctx.setdefault("repair_feedback", {})[str(approval)] = feedback
            # The runner reaches the loop verifier only when every child completes.
            return True, "", result[2]
        return result

    def verify_review(ctx):
        return ctx["verification_results"][str(approval)]

    def repair_review(ctx, _attempt, signature, detail):
        previous = read_optional(feedback_path)
        round_number = previous.get("round", 0) + 1
        if round_number > max_retries:
            raise GraphFailure("fix_and_review_loop", f"Exhausted {max_retries} review retries ({signature}): {detail}")
        feedback = {**ctx["repair_feedback"][str(approval)], "round": round_number}
        fixes = previous.get("fixes", {})
        for bug_id in feedback["rejected_bugs"]:
            report = work_dir / "bugs" / bug_id / "fix_report.json"
            prior_fix = fixes.get(bug_id)
            prior_attempts = prior_fix.get("review_attempts", 1) if prior_fix else 0
            case = feedback["manual_qa"].get(bug_id)
            fixes[bug_id] = {
                **feedback, "fix_revision": revision(report) + 1, "review_attempts": prior_attempts + 1,
                "manual_qa": case,
            }
        write_json(feedback_path, {**feedback, "fixes": fixes})
        return ("fix_bugs",)

    def approved_action(action):
        doc, (ok, _, detail), _ = read_decision()
        if not ok:
            raise ValueError(detail)
        return doc.get(action, True)

    gate = subagent_gate(
        "user_verify_gate", packet_path=packet_path, artifact_paths=(approval,), packet_fn=packet,
        verify_fn=verify_delivery, input_hash_fn=lambda _ctx: files_hash((*review_paths, packet_path, approval)),
        deps=("prepare_review_bundle",), cacheable=False,
    )
    run_gate = gate.run_fn

    def run(ctx, attempt):
        try:
            return run_gate(ctx, attempt)
        finally:
            try:
                write_qa_scorecard(qa_checklist)
            except ValueError:
                # A malformed editable checklist is reported by the gate when a decision is supplied.
                pass

    gate.run_fn = run
    return gate, verify_review, repair_review, approved_action


def _release_worker(node_id, *, review_dir, review_paths, approved, receipt_name, deps, packet, verify,
                    skip_reason, resource=None):
    receipt = review_dir / f"{receipt_name}_receipt.json"
    worker = packet_worker(
        node_id, packet_path=review_dir / f"{receipt_name}_packet.json", artifacts=(receipt,),
        input_hash=lambda: files_hash(review_paths), packet=packet, deps=deps, resource=resource,
        verify=lambda _ctx, _paths: verify(receipt),
    )
    run_worker, worker_hash, worker_outputs = worker.run_fn, worker.input_hash_fn, worker.outputs_fn

    def run(ctx, attempt):
        if approved():
            return run_worker(ctx, attempt)
        write_json(receipt, {"skipped": True, "reason": skip_reason})
        return skip_reason

    worker.run_fn = run
    worker.input_hash_fn = lambda ctx: fingerprint(worker_hash(ctx), approved())
    worker.outputs_fn = lambda ctx: worker_outputs(ctx) if approved() else (receipt,)
    return worker


def create_histoury_bug_fix_graph(
    *, batch_id: str, work_dir: Path, repos: Dict[str, Path], product: str = "desktop",
    bugs: Optional[Sequence[dict]] = None, build_cmds: Optional[Dict[str, Any]] = None,
    test_cmds: Optional[Dict[str, Any]] = None, watch_globs: Sequence[str] = (),
    verify_fns: Optional[Dict[str, Any]] = None, builder_script: Optional[Path] = None,
    bug_reports_url: str = "https://admin.histoury.com/bug-reports",
    version_history_url: str = "https://admin.histoury.com/version-history",
    max_fix_retries: int = 4, max_package_retries: int = 3, max_review_retries: int = 3,
    timeout_s: Optional[float] = 300.0,
) -> List[Any]:
    """Compose baseline capture, fix and review, and an approved release."""
    _require_release_id(require_bug_id(batch_id))
    if product not in ("desktop", "app"):
        raise ValueError("product must be desktop or app")
    if type(max_review_retries) is not int or max_review_retries < 0:
        raise ValueError("max_review_retries must be a nonnegative integer")
    if not isinstance(repos, dict) or not repos or not all(map(_nonempty_text, repos)):
        raise ValueError("repos must be a non-empty mapping from repository names to paths")
    repos = {name: Path(path).resolve() for name, path in repos.items()}
    build_cmds, test_cmds, verify_fns = (dict(value or {}) for value in (build_cmds, test_cmds, verify_fns))
    watch_globs = tuple(watch_globs)
    work_dir = Path(work_dir).resolve()
    batch_path = work_dir / "bug_batch.json"
    media_dir, evidence_dir, review_dir = (work_dir / name for name in ("media", "evidence", "review"))
    package_manifest = work_dir / "version_history" / "package_manifest.json"
    review_paths = (review_dir / "review_summary.json", review_dir / "review_summary.md")
    feedback_path = review_dir / "review_feedback.json"
    batch_config = fingerprint(batch_id, product, {name: str(path) for name, path in repos.items()}, bug_reports_url)

    def selected_bugs():
        return _batch_bugs(read_object(batch_path), batch_id, product, repos)

    user_gate, verify_review, repair_review, approved_action = _review_gate(
        review_dir, review_paths, feedback_path, selected_bugs, max_review_retries, work_dir,
    )

    def ui_bugs():
        return [bug for bug in selected_bugs() if bug["requires_ui_capture"]]

    def select_packet(_ctx):
        return {
            "stage": "fetch_bug_reports", "bug_reports_url": bug_reports_url, "batch_id": batch_id,
            "product": product, "platforms": _PLATFORMS[product], "repos": list(repos),
            "instructions": [
                "Select the assigned reports for this product from the Admin Dashboard and write bug_batch.json.",
                "Supply the requested bug fields. If the workflow has no build or test command or verifier for a bug, "
                "include a repro_cmd that exits zero when the fix works.",
            ],
            "schema": {"batch_id": batch_id, "product": product, "bugs": [{
                "id": "bug-123", "title": "Bug title", "description": "Observed behavior",
                "platform": _PLATFORMS[product][0], "repo": next(iter(repos)),
                "repro_steps": ["Step to reproduce"], "expected": "Expected behavior", "requires_ui_capture": True,
            }]},
        }

    if bugs is None:
        fetch = packet_worker("fetch_bug_reports", packet_path=work_dir / "bug_reports_packet.json",
                              artifacts=(batch_path,), input_hash=lambda: batch_config, packet=select_packet)
    else:
        batch = {"batch_id": batch_id, "product": product, "bugs": list(bugs)}
        fetch = TaskNode(
            id="fetch_bug_reports", kind="code", outputs_fn=lambda _ctx: (batch_path,),
            run_fn=lambda _ctx, _attempt: write_json(batch_path, batch),
            input_hash_fn=lambda _ctx: fingerprint(batch_config, batch),
        )

    def check_batch(_ctx, _attempt):
        try:
            count = len(selected_bugs())
        except (OSError, ValueError) as exc:
            return False, "invalid_bug_batch", f"invalid_bug_batch: {exc}"
        return True, "", f"Selected {count} bug reports for {product}"

    def split_items(_ctx):
        try:
            return selected_bugs()
        except FileNotFoundError:
            return []

    def fix_child(bug, _ctx):
        repo, bug_id = bug["repo"], bug["id"]
        review_feedback = read_optional(feedback_path).get("fixes", {}).get(bug_id)
        return create_desktop_bug_fix_subgraph(
            bug, work_dir=work_dir, repo_dir=repos[repo], repro_cmd=bug.get("repro_cmd"),
            build_cmd=build_cmds.get(bug_id, build_cmds.get(repo)),
            test_cmd=test_cmds.get(bug_id, test_cmds.get(repo)),
            verify_fn=verify_fns.get(bug_id, verify_fns.get(repo)),
            watch_globs=watch_globs, max_retries=max_fix_retries, timeout_s=timeout_s,
            review_feedback=review_feedback,
        )

    def fix_paths(bug):
        directory = work_dir / "bugs" / bug["id"]
        return tuple(directory / name for name in (
            "final_summary.json", "build_result.json", "test_result.json", "verification.json",
        ))

    def fix_evidence(bug, fix):
        repo = repos[bug["repo"]]
        files = {}
        for name in fix["changed_files"]:
            path = (repo / name).resolve()
            if path.is_relative_to(repo):
                files[name] = file_hash(path)
        return {
            "changed_file_sha256": files,
            "watched_source_sha256": hash_globs(watch_globs, root=repo),
            "log_sha256": {name: file_hash(Path(path)) for name, path in fix["log_paths"].items()},
        }

    def review_inputs(_ctx):
        provenance = [evidence_dir / f"{stage}_provenance.json" for stage in ("before", "after")]
        sources = [batch_path, package_manifest, *provenance]
        evidence = []
        for bug in selected_bugs():
            paths = fix_paths(bug)
            sources.extend(paths)
            evidence.append(fix_evidence(bug, read_object(paths[0])))
        sources.extend(media_dir / f"{bug['id']}-{stage}.png" for bug in ui_bugs() for stage in ("before", "after"))
        sources.append(Path(read_object(package_manifest)["zip_path"]))
        if feedback_path.is_file():
            sources.append(feedback_path)
        return fingerprint("qa-v1", batch_config, version_history_url, files_hash(sources), evidence)

    def prepare_review(_ctx, _attempt):
        batch_bugs = selected_bugs()
        package = read_object(package_manifest)
        if file_hash(Path(package["zip_path"])) != package["zip_sha256"]:
            raise ValueError("ZIP bytes do not match package_manifest.json")
        provenance = {stage: read_object(evidence_dir / f"{stage}_provenance.json") for stage in ("before", "after")}
        touched = {name: str(path) for name, path in repos.items() if any(bug["repo"] == name for bug in batch_bugs)}
        round_number = read_optional(feedback_path).get("round", 0)
        fixes = []
        lines = [f"# Histoury bug-fix review for {batch_id}", "", f"Product: {product}"]
        if round_number:
            lines.append(f"Review round: {round_number + 1}")
        lines.extend(["",
                 "## Version History ZIP", "", f"- ZIP: `{package['zip_path']}`",
                 f"- SHA-256: `{package['zip_sha256']}`", f"- Admin page: {version_history_url}",
                 "", "## Repositories", "",
                 *(f"- {name}: `{path}`" for name, path in touched.items())])
        for bug in batch_bugs:
            fix, build, tests, check = map(read_object, fix_paths(bug))
            item = {"bug": bug, "fix": fix, "build": build, "tests": tests,
                    "verification": check, **fix_evidence(bug, fix)}
            fixes.append(item)
            lines.extend(["", f"## {bug['id']} — {bug['title']}", "", fix["summary"], "",
                          f"- Repository: {bug['repo']}", f"- Build: {build['detail']}",
                          f"- Tests: {tests['detail']}", f"- Verification: {check['detail']}",
                          f"- Verified on attempt {fix['attempts']} (revision {fix['revision']})"])
            lines.extend(f"- {name} log: `{path}`" for name, path in fix["log_paths"].items())
            lines.extend(f"- Changed file: `{path}`" for path in fix["changed_files"])
            if bug["requires_ui_capture"]:
                item["screenshots"] = {}
                for stage in ("before", "after"):
                    path = media_dir / f"{bug['id']}-{stage}.png"
                    item["screenshots"][stage] = {"path": str(path), "sha256": file_hash(path)}
                    lines.append(f"- {stage.capitalize()} screenshot: `{path}`")
        qa_checklist = review_dir / "manual_qa.json"
        init_qa_checklist(qa_checklist, batch_id=batch_id, workflow_type="histoury-bug-fix",
                          title=f"Histoury bug-fix review for {batch_id}",
                          cases=[_bug_qa_case(item["bug"], item["fix"]) for item in fixes])
        write_qa_scorecard(qa_checklist)
        lines.extend(["", "## Manual re-test", "", "[QA Scorecard](qa_scorecard.md) · [Checklist](manual_qa.json)"])
        write_json(review_paths[0], {
            "batch_id": batch_id, "product": product, "bugs": fixes, "touched_repos": touched,
            "before_provenance": provenance["before"], "after_provenance": provenance["after"],
            "package": package, "version_history_url": version_history_url,
            **({"review_round": round_number + 1} if round_number else {}),
        })
        review_paths[1].write_text("\n".join(lines) + "\n", encoding="utf-8")

    def review():
        return read_object(review_paths[0])

    def upload_packet(_ctx):
        package = review()["package"]
        return {
            "stage": "upload_version_history", "version_history_url": version_history_url,
            "release_id": package["release_id"], "zip_path": package["zip_path"], "zip_sha256": package["zip_sha256"],
            "instructions": [
                "Use the authorized signed-in Admin account and run import/preview first for this exact ZIP.",
                "Verify the signed-in uploader has a real display name. Refresh the sign-in if it shows User.",
                "Run import/publish and verify the persisted release and uploader attribution.",
                "If resuming after publication, confirm the existing release before retrying publication.",
                "Write upload_receipt.json with previewed and published booleans, release_id, and uploader_name.",
            ],
        }

    def verify_upload(receipt):
        try:
            doc = read_object(receipt)
        except (OSError, ValueError) as exc:
            return False, "upload_incomplete", f"upload_incomplete: {exc}"
        if (doc.get("previewed") is not True or doc.get("published") is not True
                or doc.get("release_id") != review()["package"]["release_id"]):
            return False, "upload_incomplete", "upload_incomplete: Confirm the preview and publication for this release ID."
        name = doc.get("uploader_name")
        if not _nonempty_text(name) or name.strip().casefold() == "user":
            return False, "invalid_uploader_attribution", "invalid_uploader_attribution: Confirm the real Admin uploader name."
        return True, "", f"Published Version History as {name.strip()}"

    def commit_packet(_ctx):
        return {
            "stage": "commit_to_main", "touched_repos": review()["touched_repos"], "branch": "main",
            "instructions": [
                "Run git diff --check and commit only the reviewed fixes on main in each touched repository.",
                "If resuming after a commit, confirm the existing main commit before creating another.",
                "Push only with explicit approval for pushing.",
                "Write commit_receipt.json with committed true and a commits list containing repo, branch, and sha.",
            ],
        }

    def verify_commit(receipt):
        try:
            doc = read_object(receipt)
        except (OSError, ValueError) as exc:
            return False, "commit_incomplete", f"commit_incomplete: {exc}"
        commits = doc.get("commits")
        if (doc.get("committed") is not True or not isinstance(commits, list) or not commits
                or any(not isinstance(commit, dict) or not _nonempty_text(commit.get("repo"))
                       or commit.get("branch") != "main" or not _nonempty_text(commit.get("sha")) for commit in commits)
                or {commit["repo"] for commit in commits} != set(review()["touched_repos"])):
            return False, "commit_incomplete", "commit_incomplete: Provide main commit SHAs for the reviewed repositories."
        return True, "", f"Recorded {len(commits)} reviewed commits to main"

    fix_and_review = LoopSubgraphNode(
        id="fix_and_review_loop", deps=("capture_before",), max_retries=max_review_retries,
        verify_fn=verify_review, repair_fn=repair_review,
        nodes=[
            SplitterNode(id="fix_bugs", items_fn=split_items, child_builder_fn=fix_child,
                         max_workers=1, resource="repo"),
            _capture_loop("after", work_dir, batch_path, ui_bugs, "fix_bugs", feedback_path=feedback_path),
            create_version_history_subgraph(
                work_dir=work_dir, product=product, release_id=batch_id, changes_manifest_path=batch_path,
                media_dir=media_dir, builder_script=builder_script, max_retries=max_package_retries,
                deps=("capture_after",), source_paths=(feedback_path,),
            ),
            TaskNode(id="prepare_review_bundle", kind="code", deps=("version_history_package",),
                     run_fn=prepare_review, input_hash_fn=review_inputs, outputs_fn=lambda _ctx: review_paths),
            user_gate,
        ],
    )
    fix_and_review.qa_checklist_path = review_dir / "manual_qa.json"
    with_retry_route(fix_and_review, "user_verify_gate", "fix_bugs", "if rejected → re-fix")
    return [
        SubgraphNode(id="select_bugs", nodes=[
            fetch,
            TaskNode(id="verify_bug_batch", kind="verifier", deps=("fetch_bug_reports",),
                     run_fn=check_batch, input_hash_fn=lambda _ctx: fingerprint(batch_config, file_hash(batch_path)),
                     verify_fn=lambda _ctx, result: result),
        ]),
        _capture_loop("before", work_dir, batch_path, ui_bugs, "select_bugs"),
        fix_and_review,
        SubgraphNode(id="release", deps=("fix_and_review_loop",), nodes=[
            _release_worker(
                "upload_version_history", review_dir=review_dir, review_paths=review_paths,
                approved=lambda: approved_action("approve_upload_zip"), receipt_name="upload", deps=(),
                packet=upload_packet, verify=verify_upload, skip_reason="Not approved for upload",
            ),
            _release_worker(
                "commit_to_main", review_dir=review_dir, review_paths=review_paths,
                approved=lambda: approved_action("approve_commit_main"), receipt_name="commit",
                deps=("upload_version_history",), resource="repo",
                packet=commit_packet, verify=verify_commit, skip_reason="Not approved for commit to main",
            ),
        ]),
    ]
