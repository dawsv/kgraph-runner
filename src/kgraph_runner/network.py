"""Discover local access addresses and manage a Cloudflare quick tunnel."""

from contextlib import suppress
from http.client import HTTPException
from ipaddress import ip_address, ip_network
import json
import platform
from queue import Empty, Queue
import re
from secrets import token_urlsafe
import shutil
import socket
import subprocess
from threading import Thread
from time import monotonic
from typing import Dict, List, Optional
from urllib.error import HTTPError
from urllib.parse import urlencode, urlsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener


_TAILSCALE_CGNAT = ip_network("100.64.0.0/10")


def with_token(url, token):
    return url + "?" + urlencode({"token": token}) if token else url


def _is_local_host(host):
    try:
        address = ip_address(host)
        return (address.is_private or address.is_loopback or address.is_link_local
                or address in _TAILSCALE_CGNAT)
    except ValueError:
        host = host.rstrip(".")
        return "." not in host or host.endswith((".localhost", ".local", ".lan", ".home.arpa", ".ts.net"))


class _IngestRedirectHandler(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        # Only send the snapshot and its credentials to the configured hub.
        return None


def push_snapshot(hub_url, token, run, snapshot):
    """Send one workflow snapshot to the hub and return its registry entry."""
    hub = urlsplit(hub_url)
    if hub.scheme not in ("http", "https") or not hub.hostname or hub.username is not None:
        raise ValueError("Hub URL must be an HTTP or HTTPS address")
    endpoint = hub._replace(path=hub.path.rstrip("/") + "/api/ingest", fragment="").geturl()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    payload = json.dumps({"run": run, "snapshot": snapshot}, allow_nan=False).encode("utf-8")
    request = Request(endpoint, data=payload, headers=headers, method="POST")
    handlers = [_IngestRedirectHandler()]
    if _is_local_host(hub.hostname):
        handlers.append(ProxyHandler({}))
    try:
        with build_opener(*handlers).open(request, timeout=3) as response:
            result = json.load(response)
    except HTTPError as exc:
        exc.close()
        raise ValueError(f"Hero Town Hub returned HTTP {exc.code} {exc.reason}") from exc
    except HTTPException as exc:
        raise OSError(f"Invalid Hero Town Hub response: {exc}") from exc
    if not isinstance(result, dict) or result.get("ok") is not True or not isinstance(result.get("run"), dict):
        raise ValueError("Hero Town Hub did not acknowledge the snapshot")
    return result["run"]


def _command(*args):
    try:
        return subprocess.check_output(args, text=True, stderr=subprocess.DEVNULL, timeout=2).strip()
    except (OSError, subprocess.SubprocessError):
        return ""


def _lan_address():
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as route:
            route.connect(("8.8.8.8", 80))
            address = route.getsockname()[0]
            if _is_ipv4(address):
                return address
    except OSError:
        pass
    return _command("ipconfig", "getifaddr", "en0") if platform.system() == "Darwin" else None


def _is_ipv4(value):
    try:
        address = ip_address(value)
        return address.version == 4 and not (address.is_loopback or address.is_unspecified)
    except ValueError:
        return False


def _tailscale_host():
    binary = shutil.which("tailscale")
    if not binary:
        return None
    try:
        status = json.loads(_command(binary, "status", "--json"))
        if status.get("BackendState") == "Running":
            peer = status.get("Self") or {}
            name = (peer.get("DNSName") or "").rstrip(".")
            return name or next((ip for ip in peer.get("TailscaleIPs") or () if _is_ipv4(ip)), None)
    except (ValueError, AttributeError, TypeError):
        pass
    return None


def _host_url(host, port):
    return f"http://[{host}]:{port}/" if ":" in host else f"http://{host}:{port}/"


def discover_access_urls(port: int, bind_host: str, *, token: Optional[str] = None) -> List[Dict[str, str]]:
    try:
        loopback = ip_address(bind_host).is_loopback
    except ValueError:
        loopback = bind_host.lower().rstrip(".") == "localhost"
    local = bind_host if bind_host not in ("", "0.0.0.0", "::", "localhost") else "127.0.0.1"
    entries = [{"kind": "local", "label": "Local", "url": with_token(_host_url(local, port), token)}]
    if loopback:
        return entries

    lan = _lan_address()
    if _is_ipv4(lan):
        entries.append({"kind": "lan", "label": "Wi-Fi / Mobile", "url": with_token(_host_url(lan, port), token)})
    if platform.system() == "Darwin":
        name = (_command("scutil", "--get", "LocalHostName") or socket.gethostname()).split(".")[0]
        if name:
            entries.append({"kind": "bonjour", "label": "Bonjour",
                            "url": with_token(_host_url(f"{name}.local", port), token)})
    tailnet = _tailscale_host()
    if tailnet:
        entries.append({"kind": "tailscale", "label": "Tailscale", "url": with_token(_host_url(tailnet, port), token)})
    return entries


def generate_token() -> str:
    return token_urlsafe(18)


_TUNNEL_URL = re.compile(r"https://[a-z0-9-]+\.trycloudflare\.com\b")


class CloudflareTunnel:
    """Start a quick tunnel and stop its process on exit or startup failure."""

    def __init__(self, port, *, timeout=20):
        self.port = port
        self.timeout = timeout
        self.url = None
        self._process = None
        self._reader = None

    def __enter__(self):
        binary = shutil.which("cloudflared")
        if not binary:
            raise ValueError("cloudflared is required for --tunnel. Install it with brew install cloudflared on macOS.")
        self._process = subprocess.Popen(
            [binary, "tunnel", "--no-autoupdate", "--url", f"http://127.0.0.1:{self.port}"],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace",
        )
        results = Queue()

        def drain():
            try:
                for line in self._process.stdout:
                    match = _TUNNEL_URL.search(line)
                    if match:
                        results.put(match.group())
            finally:
                results.put(None)

        self._reader = Thread(target=drain, daemon=True)
        self._reader.start()
        try:
            self.url = results.get(timeout=self.timeout)
            if not self.url:
                raise ValueError("cloudflared exited before reporting a quick tunnel URL. Check cloudflared and network access.")
        except BaseException as exc:
            self.__exit__(None, None, None)
            if isinstance(exc, Empty):
                raise ValueError(f"cloudflared did not report a quick tunnel URL within {self.timeout}s.") from exc
            raise
        return self

    def __exit__(self, *_):
        with suppress(ProcessLookupError):
            self._process.terminate()
        try:
            self._process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            with suppress(ProcessLookupError):
                self._process.kill()
            self._process.wait()
        self._reader.join(timeout=1)
        self._process.stdout.close()
