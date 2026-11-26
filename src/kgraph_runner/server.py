"""Serve Hero Town and run telemetry over HTTP."""

from contextlib import nullcontext
from copy import deepcopy
from functools import partial
import hmac
from html import escape
from http import HTTPStatus
from http.cookies import CookieError, SimpleCookie
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import logging
from pathlib import Path
import socket
from threading import Event
from time import monotonic
from urllib.parse import parse_qsl, quote, unquote, urlencode, urlsplit
import webbrowser

from ._files import read_object
from .interactions import (
    _apply_remote_response, _reject_constant, read_node_log_tail, read_remote_response, read_run_qa,
    submit_gate_response, submit_run_qa,
)
from .network import CloudflareTunnel, discover_access_urls, generate_token, with_token
from .registry import _entry, get_run, ingest_remote_run, list_runs, remove_run
from .snapshot import _snapshot_from_graph
from .ui import APP_ICON, PWA_META, render_town_html


_LOG = logging.getLogger(__name__)
_MAX_INGEST_BYTES = 2 * 1024 * 1024
_MAX_RESPONSE_BYTES = 256 * 1024
_EVENT_KEEPALIVE_SECONDS = 15
_MANIFEST = {
    "id": "/",
    "name": "Hero Town — KGraph",
    "short_name": "Hero Town",
    "start_url": "/",
    "display": "standalone",
    "theme_color": "#49b7fb",
    "background_color": "#fff8e8",
    "icons": [{"src": "/icon.svg", "type": "image/svg+xml", "sizes": "any", "purpose": "any"}],
}


def _unlock_page(run):
    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
{PWA_META}
<title>Unlock Hero Town</title>
<style>
* {{ box-sizing: border-box; }}
body {{ margin: 0; padding: 64px 16px 24px; font: 16px/1.5 system-ui, sans-serif;
  color: #172033; background: #d8f7ff; }}
main {{ max-width: 390px; margin: auto; border: 3px solid; border-radius: 17px;
  background: #fff8e8; padding: 24px; box-shadow: 5px 5px #172033; }}
img {{ display: block; width: 64px; }}
h1 {{ margin: 8px 0; line-height: 1.1; }}
label {{ display: block; margin: 16px 0 4px; font-weight: 700; }}
input,button {{ width: 100%; min-height: 44px; padding: 8px 12px; font: inherit;
  color: inherit; border: 2px solid; border-radius: 8px; }}
button {{ margin-top: 12px; background: #ffcf54; font-weight: 800; cursor: pointer;
  box-shadow: 2px 2px #172033; }}
</style>
</head>
<body>
<main>
  <img src="/icon.svg" width="64" height="64" alt="">
  <h1>Unlock Hero Town</h1>
  <form method="post" action="/auth">
    <input type="hidden" name="run" value="{escape(run)}">
    <label for="token">Access token</label>
    <input type="password" id="token" name="token" autocomplete="current-password" required>
    <button type="submit">Unlock</button>
  </form>
</main>
</body>
</html>"""


def _single_run(snapshot):
    workflow_ref = snapshot.get("workflow_ref")
    workflow = Path(workflow_ref).resolve() if workflow_ref else None
    state = Path(snapshot.get("state_file") or ".kgraph/town_state.json").resolve()
    project = next((parent.parent for parent in state.parents if parent.name == ".kgraph"), Path.cwd())
    default_report = "report.json" if state.stem == "state" else f"{state.stem.removesuffix('_state')}_report.json"
    report = Path(snapshot.get("report_file") or
                  state.with_name(default_report)).resolve()
    return _entry(workflow, state, report, project, "build_graph", snapshot.get("status", "pending"),
                  snapshot.get("generated_at"), name=snapshot.get("title"))


def _hub_snapshot(run):
    if run is None:
        return {
            "version": 1, "title": "Workflow Hub", "status": "pending",
            "nodes": [], "edges": [], "run_id": None,
        }
    if run.get("source") == "remote":
        snapshot = _apply_remote_response(read_object(run["remote_snapshot_path"]), run["id"])
    else:
        # Read telemetry without importing workflows or running graph factories.
        snapshot = _snapshot_from_graph(None, run["state_path"], run["report_path"], title=run["name"],
                                        status_override=run["status"], project_dir=run["project_dir"])
        snapshot.update(
            workflow_ref=run["workflow_path"], run_id=run["id"],
            project_dir=run["project_dir"], factory=run["factory"],
        )
    return snapshot


def _disable_responses(snapshot):
    def scrub(obj):
        if isinstance(obj, dict):
            if isinstance(obj.get("input_gate"), dict):
                obj["input_gate"] = {**obj["input_gate"], "can_respond": False}
            for value in obj.values():
                scrub(value)
        elif isinstance(obj, list):
            for item in obj:
                scrub(item)
    snapshot = deepcopy(snapshot)
    scrub(snapshot)
    return snapshot


def _create_server(*, host, port, poll_ms, token, snapshot_fn=None, watch_dirs=(), layout_mode="grid",
                   read_only=False):
    if poll_ms < 1:
        raise ValueError("--poll-ms must be at least 1")
    if not 0 <= port <= 65535:
        raise ValueError("--port must be between 0 and 65535")
    mode = "hub" if snapshot_fn is None else "single"
    expected = token.encode("utf-8") if token else None
    responses_enabled = expected is not None and not read_only

    class TownHandler(BaseHTTPRequestHandler):
        def _send(self, body, content_type, status=HTTPStatus.OK, *, run_id=None):
            payload = body.encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", f"{content_type}; charset=utf-8")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Referrer-Policy", "no-referrer")
            self.send_header("Content-Length", str(len(payload)))
            if run_id is not None:
                self.send_header("X-KGraph-Run-ID", run_id)
            self.end_headers()
            try:
                self.wfile.write(payload)
            except (BrokenPipeError, ConnectionResetError):
                # Navigating away can close a polling connection mid-response.
                pass

        def _json(self, data, status=HTTPStatus.OK, *, run_id=None):
            self._send(json.dumps(data), "application/json", status, run_id=run_id)

        def _matches(self, value):
            return value is not None and hmac.compare_digest(expected, value.encode("utf-8"))

        def _authenticated(self, query_token):
            if expected is None or self._matches(query_token):
                return True
            scheme, _, value = self.headers.get("Authorization", "").partition(" ")
            if scheme.lower() == "bearer" and self._matches(value):
                return True
            cookies = SimpleCookie()
            try:
                cookies.load(self.headers.get("Cookie", ""))
            except CookieError:
                return False
            return "kgraph_token" in cookies and self._matches(unquote(cookies["kgraph_token"].value))

        def _redirect(self, fields):
            query = urlencode(fields)
            self.send_response(HTTPStatus.FOUND)
            self.send_header("Location", "/" + ("?" + query if query else ""))
            if expected is not None:
                cookie = f"kgraph_token={quote(token, safe='')}; HttpOnly; SameSite=Lax; Path=/"
                if self.headers.get("X-Forwarded-Proto", "").lower() == "https":
                    cookie += "; Secure"
                self.send_header("Set-Cookie", cookie)
            self.send_header("Cache-Control", "no-store")
            self.send_header("Referrer-Policy", "no-referrer")
            self.send_header("Content-Length", "0")
            self.end_headers()

        def _unauthorized(self, path, run):
            if path.startswith("/api/"):
                self._json({"error": "unauthorized"}, HTTPStatus.UNAUTHORIZED)
            else:
                self._send(_unlock_page(run), "text/html", HTTPStatus.UNAUTHORIZED)

        def _snapshot(self, selected):
            active = None
            if mode == "hub":
                runs = list_runs(watch_dirs)
                active = next((run for run in runs if run["id"] == selected), runs[0] if runs else None)
                snapshot = _hub_snapshot(active)
            else:
                snapshot = snapshot_fn()
            return (snapshot if responses_enabled else _disable_responses(snapshot)), active

        def _event_snapshot(self, selected):
            snapshot, active = self._snapshot(selected)
            if mode == "hub":
                return {**snapshot, "run_id": active["id"] if active else None}
            return snapshot

        def _events(self, selected):
            snapshot = self._event_snapshot(selected)
            stop = self.server._stop_event
            previous = None
            keepalive_at = monotonic() + _EVENT_KEEPALIVE_SECONDS
            try:
                self.connection.settimeout(5)
                self.send_response(HTTPStatus.OK)
                self.send_header("Content-Type", "text/event-stream; charset=utf-8")
                self.send_header("Cache-Control", "no-store")
                self.send_header("Connection", "keep-alive")
                self.send_header("X-Accel-Buffering", "no")
                self.send_header("Referrer-Policy", "no-referrer")
                self.end_headers()
                while not stop.is_set():
                    comparable = json.dumps({key: value for key, value in snapshot.items() if key != "generated_at"},
                                            sort_keys=True)
                    if comparable != previous:
                        self.wfile.write(f"event: snapshot\ndata: {json.dumps(snapshot)}\n\n".encode("utf-8"))
                        self.wfile.flush()
                        previous = comparable
                    now = monotonic()
                    if now >= keepalive_at:
                        self.wfile.write(b": keepalive\n\n")
                        self.wfile.flush()
                        keepalive_at = now + _EVENT_KEEPALIVE_SECONDS
                    if stop.wait(min(poll_ms / 1000, keepalive_at - now)):
                        break
                    snapshot = self._event_snapshot(selected)
            except OSError:
                # Clients can disconnect while an event or heartbeat is in flight.
                pass
            except Exception:
                _LOG.exception("Could not update town event stream")
            finally:
                self.close_connection = True

        def _run_api_target(self, path):
            prefix, _, endpoint = path.rpartition("/")
            if endpoint not in ("logs", "qa"):
                return None, None
            if mode == "single" and prefix == "/api":
                return endpoint, None
            if mode == "hub" and prefix.startswith("/api/runs/"):
                return endpoint, unquote(prefix.removeprefix("/api/runs/"))
            return None, None

        def _run_for_api(self, run_id, *, require_state=True):
            if run_id is not None:
                return get_run(run_id, watch_dirs)
            snapshot = snapshot_fn()
            if require_state and not snapshot.get("state_file"):
                return {}
            return _single_run(snapshot)

        def _run_data(self, endpoint, run_id, query):
            run = self._run_for_api(run_id)
            if run is None:
                return self._json({"error": "Run not found"}, HTTPStatus.NOT_FOUND)
            if endpoint == "logs":
                status, result = read_node_log_tail(run, query.get("path"), max_bytes=query.get("max_bytes", 32768))
            else:
                status, result = read_run_qa(run, can_edit=responses_enabled)
            return self._json(result, status)

        def do_GET(self):
            parsed = urlsplit(self.path)
            path = parsed.path
            if path == "/api/health":
                return self._json({"ok": True, "mode": mode})
            if path == "/manifest.webmanifest":
                return self._send(json.dumps(_MANIFEST, ensure_ascii=False), "application/manifest+json")
            if path == "/icon.svg":
                return self._send(APP_ICON, "image/svg+xml")

            fields = parse_qsl(parsed.query, keep_blank_values=True)
            query = dict(fields)
            query_token, selected = query.get("token"), query.get("run", "")
            if path == "/" and expected is not None and self._matches(query_token):
                return self._redirect([(key, value) for key, value in fields if key != "token"])
            if not self._authenticated(query_token):
                return self._unauthorized(path, selected)
            if mode == "hub" and path.startswith("/api/runs/") and path.endswith("/response"):
                if expected is None:
                    return self._unauthorized(path, selected)
                run_id = unquote(path.removeprefix("/api/runs/").removesuffix("/response"))
                response = read_remote_response(run_id)
                return self._json(response) if response is not None else self._json(
                    {"error": "No queued response for this run"}, HTTPStatus.NOT_FOUND)
            endpoint, run_id = self._run_api_target(path)
            if endpoint is None and path not in ("/", "/api/snapshot", "/api/runs", "/api/events"):
                return self.send_error(HTTPStatus.NOT_FOUND)

            try:
                if endpoint is not None:
                    return self._run_data(endpoint, run_id, query)
                if path == "/api/events":
                    return self._events(selected)
                if path == "/api/runs":
                    runs = list_runs(watch_dirs) if mode == "hub" else [_single_run(snapshot_fn())]
                    return self._json({"runs": runs})
                snapshot, active = self._snapshot(selected)
                if path == "/api/snapshot":
                    # Route remote snapshots by the registry ID.
                    remote_id = active["id"] if active and active.get("source") == "remote" else None
                    self._json(snapshot, run_id=remote_id)
                else:
                    respond_url = ("/api/runs" if mode == "hub" else "/api/respond") if responses_enabled else None
                    html = render_town_html(
                        snapshot, live_url="/api/snapshot", poll_ms=poll_ms,
                        runs_url="/api/runs" if mode == "hub" else None,
                        respond_url=respond_url,
                        events_url="/api/events",
                        logs_url="/api/runs" if mode == "hub" else "/api/logs",
                        qa_url="/api/runs" if mode == "hub" else "/api/qa",
                        active_run_id=active["id"] if active else snapshot.get("run_id"),
                        access_urls=self.server.access_urls,
                        layout_mode=layout_mode,
                        read_only=read_only,
                    )
                    self._send(html, "text/html")
            except Exception:
                _LOG.exception("Could not build town snapshot")
                self.send_error(HTTPStatus.INTERNAL_SERVER_ERROR, "Could not build town snapshot")

        def _request_body(self, max_size, limit_error):
            lengths = self.headers.get_all("Content-Length", [])
            length = lengths[0].strip() if len(lengths) == 1 else ""
            if not length.isascii() or not length.isdecimal() or "Transfer-Encoding" in self.headers:
                return None, (HTTPStatus.BAD_REQUEST, "A valid Content-Length is required")
            length = length.lstrip("0") or "0"
            limit = str(max_size)
            if len(length) > len(limit) or (len(length) == len(limit) and length > limit):
                return None, (HTTPStatus.REQUEST_ENTITY_TOO_LARGE, limit_error)
            size = int(length)
            try:
                self.connection.settimeout(5)
                body = self.rfile.read(size)
                if len(body) != size:
                    return None, (HTTPStatus.BAD_REQUEST, "Incomplete request body")
            except OSError:
                return None, (HTTPStatus.BAD_REQUEST, "Could not read request body")
            return body, None

        def _json_body(self, max_size, limit_error):
            body, error = self._request_body(max_size, limit_error)
            if error:
                return None, error
            try:
                payload = json.loads(body.decode("utf-8"), parse_constant=_reject_constant)
                if not isinstance(payload, dict):
                    raise ValueError("Payload must be a JSON object")
            except (OSError, ValueError, RecursionError):
                return None, (HTTPStatus.BAD_REQUEST, "Invalid JSON request body")
            return payload, None

        def _ingest(self):
            payload, error = self._json_body(_MAX_INGEST_BYTES, "Snapshot payload exceeds 2 MiB")
            if error:
                return self._json({"error": error[1]}, error[0])
            try:
                run = ingest_remote_run(payload.get("run"), payload.get("snapshot"))
            except (ValueError, RecursionError) as exc:
                return self._json({"error": str(exc)}, HTTPStatus.BAD_REQUEST)
            except OSError:
                _LOG.exception("Could not store remote snapshot")
                return self._json({"error": "Could not store remote snapshot"}, HTTPStatus.INTERNAL_SERVER_ERROR)
            return self._json({"ok": True, "run": run})

        def _same_origin_submission(self):
            sites = self.headers.get_all("Sec-Fetch-Site", [])
            origins = self.headers.get_all("Origin", [])
            hosts = self.headers.get_all("Host", [])
            if len(sites) > 1 or len(origins) > 1 or len(hosts) > 1:
                return False
            if sites and sites[0] not in ("same-origin", "none"):
                return False
            if not origins:
                return True
            host = hosts[0].lower() if hosts else ""
            scheme = self.headers.get("X-Forwarded-Proto", "http").lower()
            return bool(host and scheme in ("http", "https") and origins[0].lower() == f"{scheme}://{host}")

        def _submit_interaction(self, parsed, run_id=None, *, qa=False):
            if read_only:
                return self._json({"error": "This shared Hero Town view is read-only"}, HTTPStatus.FORBIDDEN)
            if expected is None or not self._authenticated(dict(parse_qsl(parsed.query)).get("token")):
                return self._unauthorized(parsed.path, "")
            if not self._same_origin_submission():
                return self._json({"error": "Cross-origin submissions are forbidden"}, HTTPStatus.FORBIDDEN)
            subject = "QA" if qa else "Response"
            payload, error = self._json_body(_MAX_RESPONSE_BYTES, f"{subject} payload exceeds 256 KiB")
            if error:
                return self._json({"error": error[1]}, error[0])
            try:
                run = self._run_for_api(run_id, require_state=qa)
                if run is None:
                    return self._json({"error": "Run not found"}, HTTPStatus.NOT_FOUND)
                submit = submit_run_qa if qa else submit_gate_response
                status, result = submit(run, payload)
                return self._json(result, status)
            except Exception:
                message = "Could not record manual QA" if qa else "Could not deliver gate response"
                _LOG.exception(message)
                return self._json({"error": message}, HTTPStatus.INTERNAL_SERVER_ERROR)

        def do_POST(self):
            parsed = urlsplit(self.path)
            path = parsed.path
            if mode == "single" and path == "/api/respond":
                return self._submit_interaction(parsed)
            endpoint, run_id = self._run_api_target(path)
            if endpoint == "qa":
                return self._submit_interaction(parsed, run_id, qa=True)
            if mode == "hub" and path.startswith("/api/runs/") and path.endswith("/respond"):
                run_id = unquote(path.removeprefix("/api/runs/").removesuffix("/respond"))
                return self._submit_interaction(parsed, run_id)
            if mode == "hub" and path.startswith("/api/runs/") and path.endswith("/delete"):
                return self._delete_run(parsed, path.removesuffix("/delete"))
            if path == "/api/ingest" and mode == "hub":
                query_token = dict(parse_qsl(parsed.query)).get("token")
                if expected is None or not self._authenticated(query_token):
                    return self._unauthorized(path, "")
                if not self._same_origin_submission():
                    return self._json({"error": "Cross-origin submissions are forbidden"}, HTTPStatus.FORBIDDEN)
                return self._ingest()
            if path != "/auth":
                if not self._authenticated(None):
                    return self._unauthorized(path, "")
                return self.send_error(HTTPStatus.NOT_FOUND)
            body, error = self._request_body(4096, "Auth payload exceeds 4 KiB")
            if error:
                return self.send_error(*error)
            try:
                form = dict(parse_qsl(body.decode("utf-8")))
            except ValueError:
                return self.send_error(HTTPStatus.BAD_REQUEST)
            run = form.get("run", "")
            if expected is None or self._matches(form.get("token")):
                return self._redirect([("run", run)] if run else [])
            return self._unauthorized("/", run)

        def _delete_run(self, parsed, path):
            if read_only:
                return self._json({"error": "This shared Hero Town view is read-only"}, HTTPStatus.FORBIDDEN)
            if not self._authenticated(dict(parse_qsl(parsed.query)).get("token")):
                return self._unauthorized(parsed.path, "")
            if not self._same_origin_submission():
                return self._json({"error": "Cross-origin submissions are forbidden"}, HTTPStatus.FORBIDDEN)
            run_id = unquote(path.removeprefix("/api/runs/"))
            try:
                if remove_run(run_id, watch_dirs):
                    return self._json({"ok": True, "removed": run_id})
            except OSError:
                _LOG.exception("Could not dismiss run")
                return self._json({"error": "Could not dismiss run"}, HTTPStatus.INTERNAL_SERVER_ERROR)
            return self._json({"error": "Run not found"}, HTTPStatus.NOT_FOUND)

        def do_DELETE(self):
            parsed = urlsplit(self.path)
            if mode == "hub" and parsed.path.startswith("/api/runs/"):
                return self._delete_run(parsed, parsed.path)
            return self.send_error(HTTPStatus.NOT_FOUND)

        def log_request(self, code="-", size="-"):
            _LOG.debug("%s %s %s", self.command, code, size)

        def log_message(self, format, *args):
            _LOG.debug(format, *args)

    class TownServer(ThreadingHTTPServer):
        allow_reuse_address = True
        address_family = socket.AF_INET6 if ":" in host else socket.AF_INET

        def __init__(self, *args, **kwargs):
            self._stop_event = Event()
            super().__init__(*args, **kwargs)

        def shutdown(self):
            self._stop_event.set()
            return super().shutdown()

        def server_close(self):
            self._stop_event.set()
            return super().server_close()

        def server_bind(self):
            if host == "::" and socket.has_dualstack_ipv6():
                self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
            super().server_bind()

    server = TownServer((host, port), TownHandler)
    try:
        server.access_urls = discover_access_urls(server.server_port, host, token=token)
    except BaseException:
        server.server_close()
        raise
    return server


def create_town_server(snapshot_fn, *, host="127.0.0.1", port=8765, poll_ms=1200, token=None, layout_mode="grid",
                       read_only=False):
    """Bind a single-workflow town server. Port 0 selects an available port."""
    return _create_server(snapshot_fn=snapshot_fn, host=host, port=port, poll_ms=poll_ms, token=token,
                          layout_mode=layout_mode, read_only=read_only)


def create_hub_server(watch_dirs=(), *, host="127.0.0.1", port=8765, poll_ms=1200, token=None, read_only=False):
    """Bind a town server that serves the machine's registered workflows."""
    return _create_server(watch_dirs=tuple(watch_dirs), host=host, port=port, poll_ms=poll_ms, token=token,
                          read_only=read_only)


def _serve(create, *, host, port, poll_ms, open_browser, token, tunnel, no_auth):
    if tunnel and host not in ("", "127.0.0.1", "localhost", "0.0.0.0", "::"):
        raise ValueError("--tunnel requires a bind that accepts IPv4 connections on 127.0.0.1.")
    token = None if no_auth else token or (generate_token() if tunnel else None)
    try:
        with create(host=host, port=port, poll_ms=poll_ms, token=token) as server:
            connection = CloudflareTunnel(server.server_port) if tunnel else nullcontext()
            with connection as quick_tunnel:
                if quick_tunnel is not None:
                    server.access_urls.append({
                        "kind": "tunnel", "label": "Public HTTPS Tunnel",
                        "url": with_token(quick_tunnel.url + "/", token),
                    })
                local, *other_urls = server.access_urls
                print(f"Hero Town is live. {local['label']}: {local['url']}", flush=True)
                for entry in other_urls:
                    print(f"  {entry['label']}: {entry['url']}", flush=True)
                if open_browser and not webbrowser.open(local["url"]):
                    _LOG.warning("Could not open a browser.")
                server.serve_forever()
    except KeyboardInterrupt:
        pass


def serve_town(snapshot_fn, *, host="127.0.0.1", port=8765, poll_ms=1200, open_browser=False,
               token=None, tunnel=False, no_auth=False, layout_mode="grid",
               read_only=False):
    """Serve a single workflow until interrupted."""
    _serve(partial(create_town_server, snapshot_fn, layout_mode=layout_mode, read_only=read_only),
           host=host, port=port, poll_ms=poll_ms, open_browser=open_browser,
           token=token, tunnel=tunnel, no_auth=no_auth)


def serve_hub(watch_dirs=(), *, host="127.0.0.1", port=8765, poll_ms=1200, open_browser=False,
              token=None, tunnel=False, no_auth=False):
    """Serve the multi-project hub until interrupted."""
    _serve(partial(create_hub_server, watch_dirs), host=host, port=port, poll_ms=poll_ms,
           open_browser=open_browser, token=token, tunnel=tunnel, no_auth=no_auth)
