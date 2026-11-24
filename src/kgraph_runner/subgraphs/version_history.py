"""Author, validate, and build a Portyl Version History ZIP."""

from __future__ import annotations

from contextlib import suppress
import os
from pathlib import Path, PurePosixPath
import re
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory
from typing import Optional, Sequence, Tuple
from urllib.parse import urlsplit
from zipfile import BadZipFile, ZIP_DEFLATED, ZipFile
import zlib

from .._files import read_object, require_id, write_json
from ..engine import LoopSubgraphNode, TaskNode, sha256_bytes
from ..verifiers import inspect_png
from ._common import Repairs, file_hash, files_hash, fingerprint, packet_worker, verification, with_retry_route
from .bug_fix import require_bug_id

_BUILDER_PATH = Path(".agents/skills/portyl-version-history/scripts/build_package.py")
_IMAGE_EXTENSIONS = {".png", ".jpg", ".jpeg", ".webp"}
_VIDEO_EXTENSIONS = {".mp4", ".webm", ".mov"}
_IMAGE_ERRORS = (OSError, ValueError, struct.error, zlib.error, RuntimeError)
_MIB = 1024 * 1024


def find_version_history_builder(builder_script: Optional[Path] = None) -> Optional[Path]:
    """Find the optional Portyl skill builder in order of precedence."""
    candidates = (builder_script, Path.home() / _BUILDER_PATH, Path.cwd() / _BUILDER_PATH)
    return next((Path(path).resolve() for path in candidates if path is not None and Path(path).is_file()), None)


def _require_release_id(value):
    if not isinstance(value, str) or not re.fullmatch(r"[a-z0-9][a-z0-9._-]{0,79}", value):
        raise ValueError("release_id must start with a lowercase letter or digit and use up to 80 slug characters")
    # The identifier becomes a ZIP filename on both macOS and Windows.
    require_bug_id(value.split(".", 1)[0])
    return value


def _local_media(pkg_dir, url):
    media = PurePosixPath(url)
    if (not media.parts or media.is_absolute() or ".." in media.parts or str(media) != url
            or re.search(r'[\\:*?"<>|#\x00-\x1f]', url) or url.casefold() == "release.json"):
        raise ValueError(f"Unsafe media path: {url!r}")
    path = (pkg_dir / url).resolve()
    if not path.is_relative_to(pkg_dir):
        raise ValueError(f"Media is outside the package: {url!r}")
    return path


def _local_references(doc, pkg_dir):
    """Find safe local references even when the rest of the manifest needs repair."""
    references = {}
    changes = doc.get("changes")
    for change in changes if isinstance(changes, list) else ():
        evidence = change.get("evidence") if isinstance(change, dict) else None
        for item in evidence if isinstance(evidence, list) else ():
            url = item.get("url") if isinstance(item, dict) else None
            if isinstance(url, str):
                try:
                    references[url] = _local_media(pkg_dir, url)
                except (OSError, ValueError, RuntimeError):
                    continue
    return references


def _check_jpeg(data):
    if not data.startswith(b"\xff\xd8"):
        raise ValueError("Missing JPEG start marker")
    offset = 2
    frame = scan = False
    marker_pattern = re.compile(rb"\xff+([^\xff])")
    while marker := marker_pattern.match(data, offset):
        code = marker[1][0]
        offset = marker.end()
        if code == 0xD9 and frame and scan:
            return
        length = int.from_bytes(data[offset:offset + 2], "big")
        if length < 2 or offset + length > len(data):
            break
        if code in (0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF):
            if length < 11 or not all(struct.unpack_from(">HH", data, offset + 3)):
                break
            frame = True
        offset += length
        if code == 0xDA:
            # Skip entropy-coded data up to the next segment marker. Stuffed FF
            # bytes and restart markers are part of the scan.
            next_segment = next((item for item in marker_pattern.finditer(data, offset)
                                 if item[1][0] not in (0, *range(0xD0, 0xD8))), None)
            if length < 8 or next_segment is None or next_segment.start() == offset:
                break
            scan = True
            offset = next_segment.start()
    raise ValueError("JPEG is missing a valid frame, scan, or end marker")


def _webp_image_chunks(data, *, animated=True):
    images = 0
    offset = 0
    while offset < len(data):
        if offset + 8 > len(data):
            raise ValueError("Truncated WebP chunk header")
        kind, size = struct.unpack_from("<4sI", data, offset)
        offset += 8
        if offset + size + (size % 2) > len(data):
            raise ValueError("Truncated WebP chunk")
        payload = data[offset:offset + size]
        if kind == b"VP8 ":
            width, height = struct.unpack_from("<HH", payload, 6)
            if size <= 10 or payload[0] & 1 or payload[3:6] != b"\x9d\x01\x2a" or not (width & 0x3fff and height & 0x3fff):
                raise ValueError("Invalid lossy WebP frame")
            images += 1
        elif kind == b"VP8L":
            if size <= 5 or payload[0] != 0x2f or payload[4] >> 5:
                raise ValueError("Invalid lossless WebP frame")
            images += 1
        elif kind == b"ANMF":
            if not animated or size <= 16 or _webp_image_chunks(payload[16:], animated=False) != 1:
                raise ValueError("Invalid animated WebP frame")
            images += 1
        offset += size + (size % 2)
    return images


def _check_image(path, extension):
    if extension == ".png":
        inspect_png(path)
        return
    data = path.read_bytes()
    if extension in (".jpg", ".jpeg"):
        _check_jpeg(data)
    elif (len(data) < 12 or data[:4] != b"RIFF" or data[8:12] != b"WEBP"
          or int.from_bytes(data[4:8], "little") != len(data) - 8
          or not _webp_image_chunks(data[12:])):
        raise ValueError("Missing or invalid WebP container or frame")


def _preflight(doc, pkg_dir, product, release_id):
    if "platforms" in doc:
        return False, "forbidden_platforms_field", "Remove platforms from release.json. The service supplies the product group."
    if "revision" in doc:
        return False, "unsupported_release_field", "Write the authoring revision in author_meta.json."
    if doc.get("product") != product:
        return False, "product_mismatch", f"Expected product {product!r} in release.json."
    try:
        actual_id = _require_release_id(doc.get("release_id"))
    except ValueError as exc:
        return False, "invalid_release_id", str(exc)
    if release_id is not None and actual_id != release_id:
        return False, "release_id_mismatch", f"Expected release_id {release_id!r} in release.json."

    changes = doc.get("changes")
    if not isinstance(changes, list) or not changes:
        return False, "missing_changes", "Provide a non-empty changes list."
    seen_ids, media = set(), {}
    for change in changes:
        change_id = change.get("id") if isinstance(change, dict) else None
        if not isinstance(change_id, str) or not change_id.strip() or change_id in seen_ids:
            return False, "invalid_change_id", f"Each change needs a unique non-empty id. Got {change_id!r}."
        seen_ids.add(change_id)
        stages = {"before": set(), "after": set()}
        evidence = change.get("evidence", [])
        if not isinstance(evidence, list):
            return False, "invalid_evidence", f"Evidence for {change_id} must be a list."
        for item in evidence:
            try:
                url = item.get("url") if isinstance(item, dict) else None
                if not isinstance(url, str) or not url.strip():
                    raise ValueError("Each evidence entry needs a URL")
                parsed = urlsplit(url)
                if parsed.scheme == "https":
                    if not parsed.hostname or parsed.username or parsed.password:
                        raise ValueError("Invalid HTTPS evidence URL")
                    continue
                path = _local_media(pkg_dir, url)
                extension = PurePosixPath(url).suffix.lower()
                kind = item.get("kind")
                if not ((kind == "image" and extension in _IMAGE_EXTENSIONS)
                        or (kind == "video" and extension in _VIDEO_EXTENSIONS)):
                    raise ValueError(f"Unsupported media format for {kind!r}: {url}")
                if not path.is_file() or not 0 < path.stat().st_size <= 25 * _MIB:
                    raise ValueError(f"Missing or oversized media: {url}")
                if url not in media:
                    if kind == "image":
                        _check_image(path, extension)
                    media[url] = path
                if kind == "image" and item.get("stage") in ("before", "after"):
                    stages[item["stage"]].add(file_hash(path))
            except _IMAGE_ERRORS as exc:
                return False, "invalid_media", f"Change {change_id}: {exc}"
        if not all(stages.values()):
            return False, "missing_before_after_media", f"Change {change_id} needs local before and after images."
        if stages["before"] & stages["after"]:
            return False, "duplicate_before_after_media", f"Change {change_id} uses identical before and after media."
    manifest_size = (pkg_dir / "release.json").stat().st_size
    if (manifest_size > 256 * 1024 or len(media) >= 150
            or manifest_size + sum(path.stat().st_size for path in media.values()) > 100 * _MIB):
        return False, "package_too_large", "The package exceeds the entry count or expanded ZIP size limit."
    if len({name.casefold() for name in media}) != len(media):
        return False, "duplicate_media_path", "Media paths must be unique on case-insensitive filesystems."
    return True, "", f"Validated {len(changes)} changes with {len(media)} local media files."


def _run_builder(builder, action, release_json, *args):
    return subprocess.run(
        [sys.executable, str(builder), action, str(release_json), *map(str, args)],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, encoding="utf-8", errors="replace",
        env={**os.environ, "PYTHONHASHSEED": "0"},
    )


def create_version_history_subgraph(
    *, work_dir: Path, product: str = "desktop", release_id: Optional[str] = None,
    changes_manifest_path: Optional[Path] = None, media_dir: Optional[Path] = None,
    builder_script: Optional[Path] = None, max_retries: int = 3, deps: Tuple[str, ...] = (),
    node_id: str = "version_history_package",
    source_paths: Sequence[Path] = (),
) -> LoopSubgraphNode:
    """Request a v2 manifest and build a ZIP after screenshot evidence passes."""
    if product not in ("desktop", "app"):
        raise ValueError("product must be desktop or app")
    if release_id is not None:
        _require_release_id(release_id)
    if type(max_retries) is not int or max_retries < 0:
        raise ValueError("max_retries must be a nonnegative integer")
    vh_dir = Path(work_dir).resolve() / "version_history"
    pkg_dir = vh_dir / "package"
    release_json, author_meta = pkg_dir / "release.json", pkg_dir / "author_meta.json"
    check_log, check_result = vh_dir / "check.log", vh_dir / "check_result.json"
    build_log, manifest = vh_dir / "build.log", vh_dir / "package_manifest.json"
    builder = find_version_history_builder(builder_script)
    builder_schema = builder.parent.parent / "references" / "schema.json" if builder else None
    changes_path = Path(changes_manifest_path).resolve() if changes_manifest_path is not None else None
    source_media = Path(media_dir).resolve() if media_dir is not None else None
    source_paths = tuple(Path(path).resolve() for path in source_paths)

    def source_hash():
        source_files = sorted(path for path in source_media.rglob("*") if path.is_file()) if source_media else ()
        extra = [path for path in source_paths if path.is_file()]
        extra_hashes = (files_hash(extra),) if extra else ()
        return fingerprint(product, release_id, str(source_media), str(builder), file_hash(builder),
                           file_hash(builder_schema), str(changes_path), file_hash(changes_path), files_hash(source_files),
                           *extra_hashes)

    def release_doc():
        try:
            return read_object(release_json)
        except (OSError, ValueError):
            return {}

    def delivery_hash():
        paths = sorted(set(_local_references(release_doc(), pkg_dir).values()))
        return files_hash((release_json, author_meta, *paths))

    repairs = Repairs(vh_dir / "repair_state.json", source_hash, (release_json, author_meta),
                      metadata=author_meta, artifact_hash=delivery_hash)

    def author_packet(_ctx):
        pkg_dir.mkdir(parents=True, exist_ok=True)
        return {
            "stage": "author_release", "product": product, "release_id": release_id,
            "pkg_dir": str(pkg_dir), "release_json": str(release_json), "author_meta": str(author_meta),
            "changes_manifest_path": str(changes_path) if changes_path else None,
            "media_dir": str(source_media) if source_media else None,
            "builder_script": str(builder) if builder else None,
            "check_log": str(check_log), "build_log": str(build_log),
            "required_release_fields": [
                "schema_version", "product", "release_id", "version", "title", "date", "stage", "summary", "changes",
            ],
            "required_change_fields": ["id", "type", "title", "description", "verification", "evidence"],
            "instructions": [
                "Write release.json with schema_version 2. Omit platforms and other unsupported fields.",
                "Use the requested release_id when supplied and a unique release_id for each publication. "
                "Use stable, unique change IDs.",
                "Choose source, staging, internal, or production for the stage actually reached. "
                "State what passed and what remains untested in verification.",
                "Copy genuine matched before and after images into the package for every change. "
                "Reference them by relative URL in evidence entries with kind image and stage before or after.",
                "Keep secrets and personal information out of the manifest and media.",
                "Write author_meta.json beside release.json with the packet revision (integer) and matching release_id.",
            ],
        }

    def check(_ctx):
        doc = release_doc()
        try:
            output = _run_builder(builder, "check", release_json) if builder else None
        except OSError as exc:
            check_log.write_text(f"{exc}\n", encoding="utf-8")
            return False, "vh_check_launch_error", str(exc)

        ok, signature, detail = _preflight(doc, pkg_dir, product, release_id)
        if output is not None and output.returncode:
            signature = "vh_check_failed:" + fingerprint(output.stdout.replace(str(pkg_dir), "<package>"))[:8]
            detail = output.stdout.strip() or f"Builder check exited with code {output.returncode}"
            ok = False
        if ok:
            try:
                meta = read_object(author_meta)
            except (OSError, ValueError):
                meta = {}
            number = meta.get("revision")
            if type(number) is not int or number < repairs.instructions()["revision"] or meta.get("release_id") != doc["release_id"]:
                ok, signature = False, "invalid_author_meta"
                detail = "Write author_meta.json with the requested revision and the release_id from release.json."
        log = output.stdout if output is not None else ""
        if not log or ok or not signature.startswith("vh_check_failed:"):
            log += ("\n" if log and not log.endswith("\n") else "") + f"{signature + ': ' if signature else ''}{detail}\n"
        check_log.write_text(log, encoding="utf-8")
        return ok, signature, detail

    def package_inputs():
        return fingerprint(source_hash(), delivery_hash(), repairs.instructions()["revision"])

    def validate(ctx, _attempt):
        inputs = package_inputs()
        ok, signature, detail = check(ctx)
        write_json(check_result, {"ok": ok, "error_signature": signature, "detail": detail, "input_hash": inputs})
        ctx.setdefault("verification_results", {})[str(check_result)] = ok, signature, detail
        return f"{signature}: {detail}" if signature else detail

    def zip_path():
        try:
            return vh_dir / f"{_require_release_id(release_doc().get('release_id'))}.zip"
        except ValueError:
            return None

    def build(_ctx, _attempt):
        manifest.unlink(missing_ok=True)
        build_log.unlink(missing_ok=True)
        checked = read_object(check_result)
        if not checked["ok"]:
            return "Waiting for package validation"
        inputs = package_inputs()
        if inputs != checked["input_hash"]:
            build_log.write_text("Package inputs changed after validation.\n", encoding="utf-8")
            return "Package needs revalidation"
        doc = read_object(release_json)
        path = zip_path()
        media = _local_references(doc, pkg_dir)
        try:
            path.unlink(missing_ok=True)
            if builder is not None:
                output = _run_builder(builder, "build", release_json, "--output", path)
                build_log.write_text(output.stdout, encoding="utf-8")
                if output.returncode:
                    raise ValueError(f"Builder exited with code {output.returncode}")
            else:
                with TemporaryDirectory(dir=vh_dir) as temp:
                    draft = Path(temp) / path.name
                    with ZipFile(draft, "w", ZIP_DEFLATED) as archive:
                        archive.write(release_json, "release.json")
                        for name, file in sorted(media.items()):
                            archive.write(file, name)
                    draft.replace(path)
            if path.stat().st_size > 50 * _MIB:
                raise ValueError("Compressed ZIP exceeds 50 MiB")
            expected = {name: file_hash(file) for name, file in media.items()}
            expected["release.json"] = file_hash(release_json)
            with ZipFile(path) as archive:
                if sorted(archive.namelist()) != sorted(expected):
                    raise ValueError("ZIP entries must contain only root release.json and the referenced local media")
                for name, digest in expected.items():
                    if sha256_bytes(archive.read(name)) != digest:
                        raise ValueError(f"ZIP entry differs from its validated source: {name}")
            if inputs != package_inputs():
                raise ValueError("Package inputs changed during ZIP creation")
        except (OSError, ValueError, BadZipFile, RuntimeError, zlib.error) as exc:
            with suppress(OSError):
                path.unlink(missing_ok=True)
            with build_log.open("a", encoding="utf-8") as log:
                log.write(f"\n{exc}\n")
            return "ZIP build failed"
        write_json(manifest, {
            "release_id": doc["release_id"], "product": product, "zip_path": str(path),
            "zip_sha256": file_hash(path), "change_ids": [change["id"] for change in doc["changes"]],
            "media_files": sorted(media),
        })
        return f"Built {path.name}"

    def build_outputs(_ctx):
        path = zip_path()
        if path is not None and manifest.is_file():
            return (manifest, path, *((build_log,) if build_log.is_file() else ()))
        return (build_log,) if build_log.is_file() else ()

    def verify(_ctx):
        ok, signature, detail = verification(check_result)
        if not ok:
            return False, signature, check_log.read_text(encoding="utf-8") or detail
        path = zip_path()
        if path is None or not manifest.is_file() or not path.is_file():
            detail = check_log.read_text(encoding="utf-8")
            if build_log.is_file():
                detail += build_log.read_text(encoding="utf-8")
            return False, "vh_zip_failed", detail
        repairs.resolved()
        return True, "", f"Verified Version History ZIP {path.name}"

    loop = LoopSubgraphNode(
        id=require_id(node_id), deps=deps, max_retries=max_retries, verify_fn=verify,
        repair_fn=lambda ctx, _attempt, sig, detail: repairs.repair(
            ctx, sig, detail, ["assemble_package", "verify_package", "build_zip"], max_retries,
        ),
        nodes=[
            packet_worker("assemble_package", packet_path=vh_dir / "release_packet.json",
                          artifacts=(release_json, author_meta), input_hash=source_hash,
                          packet=author_packet, repairs=repairs),
            TaskNode(id="verify_package", kind="verifier", deps=("assemble_package",), run_fn=validate,
                     input_hash_fn=lambda _ctx: package_inputs(), outputs_fn=lambda _ctx: (check_result, check_log),
                     verify_fn=lambda _ctx, detail: (True, "", detail)),
            TaskNode(id="build_zip", kind="code", deps=("verify_package",), run_fn=build,
                     input_hash_fn=lambda _ctx: fingerprint(package_inputs(), file_hash(check_result)),
                     outputs_fn=build_outputs),
        ],
    )
    return with_retry_route(loop, "verify_package", "assemble_package")
