//! Hub HTTP client, access URL discovery, and Cloudflare quick tunnels.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use kgraph_core::files::valid_id;
use once_cell::sync::Lazy;
use rand::{rngs::OsRng, RngCore};
use regex::Regex;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::{redirect::Policy, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io::{self, BufRead, BufReader, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};
use url::{Host, Url};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccessUrl {
    pub kind: String,
    pub label: String,
    pub url: String,
}

pub fn with_token(url: &str, token: Option<&str>) -> String {
    match token.filter(|s| !s.is_empty()) {
        Some(token) => format!(
            "{url}?token={}",
            urlencoding::encode(token).replace("%20", "+")
        ),
        None => url.into(),
    }
}

pub fn generate_token() -> String {
    let mut bytes = [0; 18];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, d] = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_documentation()
        || a == 0
        || a >= 240
        || (a == 198 && matches!(b, 18 | 19))
        || (a == 192 && b == 0 && c == 0 && (d < 8 || matches!(d, 170 | 171)))
}

fn private_v6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return private_v4(mapped);
    }
    let [a, b, c, d, ..] = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || (a == 0x100 && [b, c, d] == [0; 3])
        || (a == 0x2001 && (b < 0x200 || b == 0xdb8))
}

pub fn is_local_host(host: &str) -> bool {
    // Python ipaddress accepts an IPv6 interface scope after '%'.
    let ip_host = if host.contains(':') {
        host.split('%').next().unwrap()
    } else {
        host
    };
    match ip_host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            let [a, b, ..] = ip.octets();
            private_v4(ip) || (a == 100 && (64..128).contains(&b))
        }
        Ok(IpAddr::V6(ip)) => private_v6(ip),
        Err(_) => {
            let host = host.trim_end_matches('.');
            !host.contains('.')
                || [".localhost", ".local", ".lan", ".home.arpa", ".ts.net"]
                    .iter()
                    .any(|suffix| host.ends_with(suffix))
        }
    }
}

const INVALID_HUB: &str = "Hub URL must be an HTTP or HTTPS address";

fn hub_endpoint(hub_url: &str, path: &str) -> Result<(Url, Client), String> {
    // Url's browser-compatible parser repairs missing authorities and drops
    // empty userinfo. Reject both before normalization.
    let (scheme, after_scheme) = hub_url.trim_start().split_once("://").ok_or(INVALID_HUB)?;
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
        return Err(INVALID_HUB.into());
    }
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap();
    if authority.is_empty() || authority.contains(['@', '\\']) {
        return Err(INVALID_HUB.into());
    }
    let mut endpoint = Url::parse(hub_url).map_err(|_| INVALID_HUB)?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
    {
        return Err(INVALID_HUB.into());
    }
    let local = match endpoint.host().ok_or(INVALID_HUB)? {
        Host::Domain(host) => is_local_host(host),
        Host::Ipv4(ip) => is_local_host(&ip.to_string()),
        Host::Ipv6(ip) => private_v6(ip),
    };
    endpoint.set_path(&format!("{}{path}", endpoint.path().trim_end_matches('/')));
    endpoint.set_fragment(None);
    let mut builder = Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(3));
    if local {
        builder = builder.no_proxy();
    }
    Ok((endpoint, builder.build().map_err(|e| e.to_string())?))
}

fn send(request: RequestBuilder, token: Option<&str>) -> Result<Response, String> {
    let request = match token.filter(|s| !s.is_empty()) {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    request
        .send()
        .map_err(|e| format!("Invalid Hero Town Hub response: {e}"))
}

fn response_json(response: Response) -> Result<Value, String> {
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "Hero Town Hub returned HTTP {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        ));
    }
    response
        .json()
        .map_err(|e| format!("Invalid Hero Town Hub response: {e}"))
}

pub fn push_snapshot(
    hub_url: &str,
    token: Option<&str>,
    run: &Value,
    snapshot: &Value,
) -> Result<Map<String, Value>, String> {
    let (url, client) = hub_endpoint(hub_url, "/api/ingest")?;
    #[derive(Serialize)]
    struct Ingest<'a> {
        run: &'a Value,
        snapshot: &'a Value,
    }
    let body = crate::ui::python_json(&Ingest { run, snapshot })?;
    let request = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    let mut body = response_json(send(request, token)?)?;
    if body.get("ok") == Some(&Value::Bool(true)) {
        if let Some(Value::Object(run)) = body.get_mut("run").map(Value::take) {
            return Ok(run);
        }
    }
    Err("Hero Town Hub did not acknowledge the snapshot".into())
}

static RESERVED_ID: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^(con|prn|aux|nul|(com|lpt)[0-9])$").unwrap());

pub fn fetch_remote_response(
    hub_url: &str,
    token: Option<&str>,
    run_id: &str,
) -> Result<Option<Map<String, Value>>, String> {
    if run_id.len() > 128 || !valid_id(run_id) || RESERVED_ID.is_match(run_id) {
        return Err("Invalid run ID".into());
    }
    let path = format!("/api/runs/{}/response", urlencoding::encode(run_id));
    let (url, client) = hub_endpoint(hub_url, &path)?;
    let response = send(client.get(url), token)?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    match response_json(response)? {
        Value::Object(map) => Ok(Some(map)),
        _ => Err("Hero Town Hub response must be a JSON object".into()),
    }
}

fn command(program: &str, args: &[&str]) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut output = String::new();
        let result = stdout.read_to_string(&mut output).map(|_| output);
        let _ = tx.send(result);
    });
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .ok()?
                    .ok()
                    .map(|s| s.trim().to_owned());
            }
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn usable_ipv4(host: &str) -> bool {
    host.parse::<Ipv4Addr>()
        .is_ok_and(|ip| !ip.is_loopback() && !ip.is_unspecified())
}

fn lan_address() -> Option<String> {
    let route = UdpSocket::bind("0.0.0.0:0").and_then(|s| {
        s.connect("8.8.8.8:80")?;
        s.local_addr()
    });
    if let Some(host) = route
        .ok()
        .map(|addr| addr.ip().to_string())
        .filter(|s| usable_ipv4(s))
    {
        return Some(host);
    }
    if cfg!(target_os = "macos") {
        return command("ipconfig", &["getifaddr", "en0"]).filter(|s| usable_ipv4(s));
    }
    None
}

fn bonjour_host() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let name = command("scutil", &["--get", "LocalHostName"])
        .filter(|s| !s.is_empty())
        .or_else(|| command("hostname", &[]))?;
    let label = name.split('.').next()?;
    (!label.is_empty()).then(|| format!("{label}.local"))
}

fn tailscale_host(status: &str) -> Option<String> {
    let status: Value = serde_json::from_str(status).ok()?;
    if status.get("BackendState")?.as_str()? != "Running" {
        return None;
    }
    let peer = status.get("Self")?;
    if let Some(host) = peer
        .get("DNSName")
        .and_then(Value::as_str)
        .map(|s| s.trim_end_matches('.'))
        .filter(|s| !s.is_empty())
    {
        return Some(host.to_owned());
    }
    peer.get("TailscaleIPs")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find(|s| usable_ipv4(s))
        .map(str::to_owned)
}

pub fn discover_access_urls(port: u16, bind_host: &str, token: Option<&str>) -> Vec<AccessUrl> {
    let loopback = bind_host
        .parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or_else(|_| {
            bind_host
                .trim_end_matches('.')
                .eq_ignore_ascii_case("localhost")
        });
    let local = match bind_host {
        "" | "0.0.0.0" | "::" | "localhost" => "127.0.0.1",
        host => host,
    };
    let entry = |kind: &str, label: &str, host: &str| {
        let url = if host.contains(':') {
            format!("http://[{host}]:{port}/")
        } else {
            format!("http://{host}:{port}/")
        };
        AccessUrl {
            kind: kind.into(),
            label: label.into(),
            url: with_token(&url, token),
        }
    };
    let mut entries = vec![entry("local", "Local", local)];
    if loopback {
        return entries;
    }
    if let Some(host) = lan_address() {
        entries.push(entry("lan", "Wi-Fi / Mobile", &host));
    }
    if let Some(host) = bonjour_host() {
        entries.push(entry("bonjour", "Bonjour", &host));
    }
    if let Some(host) = command("tailscale", &["status", "--json"]).and_then(|s| tailscale_host(&s))
    {
        entries.push(entry("tailscale", "Tailscale", &host));
    }
    entries
}

static TUNNEL_URL: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"https://[a-z0-9-]+\.trycloudflare\.com\b").unwrap());

fn read_tunnel_output(stream: impl Read + Send + 'static, tx: Sender<String>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        let mut tx = Some(tx);
        while let Ok(n) = reader.read_until(b'\n', &mut line) {
            if n == 0 {
                break;
            }
            if tx.is_some() {
                if let Some(found) = TUNNEL_URL.find(&String::from_utf8_lossy(&line)) {
                    let _ = tx.take().unwrap().send(found.as_str().into());
                }
            }
            // Keep draining after startup so cloudflared cannot block on a full pipe.
            line.clear();
        }
    });
}

#[derive(Debug)]
pub struct CloudflareTunnel {
    pub url: String,
    child: Child,
}

impl CloudflareTunnel {
    pub fn start(port: u16, timeout_s: u64) -> Result<Self, String> {
        let mut command = Command::new("cloudflared");
        command.args([
            "tunnel",
            "--no-autoupdate",
            "--url",
            &format!("http://127.0.0.1:{port}"),
        ]);
        Self::start_command(command, timeout_s)
    }

    fn start_command(mut command: Command, timeout_s: u64) -> Result<Self, String> {
        let child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
            .map_err(|e| if e.kind() == io::ErrorKind::NotFound {
                "cloudflared is required for --tunnel. Install it with brew install cloudflared on macOS.".into()
            } else { e.to_string() })?;
        let mut tunnel = Self {
            url: String::new(),
            child,
        };
        let (tx, rx) = mpsc::channel();
        read_tunnel_output(tunnel.child.stdout.take().unwrap(), tx.clone());
        read_tunnel_output(tunnel.child.stderr.take().unwrap(), tx);
        tunnel.url = rx.recv_timeout(Duration::from_secs(timeout_s)).map_err(|err| match err {
            mpsc::RecvTimeoutError::Timeout => format!("cloudflared did not report a quick tunnel URL within {timeout_s}s."),
            mpsc::RecvTimeoutError::Disconnected => "cloudflared exited before reporting a quick tunnel URL. Check cloudflared and network access.".into(),
        })?;
        Ok(tunnel)
    }
}

impl Drop for CloudflareTunnel {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        #[cfg(unix)]
        {
            // SAFETY: this PID belongs to our unreaped child and SIGTERM is a valid signal.
            if unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) } == 0 {
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline {
                    match self.child.try_wait() {
                        Ok(Some(_)) => return,
                        Ok(None) => thread::sleep(Duration::from_millis(20)),
                        Err(_) => break,
                    }
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;
    use std::io::Write;
    use std::net::TcpListener;

    #[test]
    fn token_encoding_and_entropy() {
        assert_eq!(with_token("http://town/", None), "http://town/");
        assert_eq!(with_token("u", Some("")), "u");
        assert_eq!(
            with_token("u?q=1", Some("a /+*=~é")),
            "u?q=1?token=a+%2F%2B%2A%3D~%C3%A9"
        );
        let tokens: HashSet<_> = (0..64).map(|_| generate_token()).collect();
        assert_eq!(tokens.len(), 64);
        for token in tokens {
            assert_eq!(token.len(), 24);
            assert!(token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')));
            assert_eq!(URL_SAFE_NO_PAD.decode(token).unwrap().len(), 18);
        }
    }

    #[test]
    fn identifies_local_ips_and_hostnames() {
        for host in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.0.2",
            "172.16.0.1",
            "169.254.1.1",
            "100.64.0.0",
            "100.127.255.255",
            "::1",
            "fd12::1",
            "fe80::a%en0",
            "::ffff:192.168.0.1",
            "hub",
            "hub.local.",
            "hub.lan",
            "hub.localhost",
            "hub.home.arpa",
            "hub.mesh.ts.net.",
        ] {
            assert!(is_local_host(host), "{host}");
        }
        for host in [
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.0",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
            "example.com",
            "ts.net.example.org",
        ] {
            assert!(!is_local_host(host), "{host}");
        }
    }

    #[test]
    fn validates_endpoints_and_run_ids() {
        for bad in [
            "",
            "local/path",
            "https:host",
            "ftp://host",
            "http:///path",
            "http://?q=x",
            "http://user@host",
            "http://user:pass@host",
            "https://:pass@host",
            "https://@host",
        ] {
            assert_eq!(
                push_snapshot(bad, None, &Value::Null, &Value::Null).unwrap_err(),
                INVALID_HUB
            );
        }
        for run_id in ["", "../path", "a b", "cOn", "lPt9", &"a".repeat(129)] {
            assert_eq!(
                fetch_remote_response("http://hub", None, run_id).unwrap_err(),
                "Invalid run ID"
            );
        }
        assert_eq!(
            hub_endpoint("http://[::1]:8080/hub/%20///?key=1#here", "/api/ingest")
                .unwrap()
                .0
                .as_str(),
            "http://[::1]:8080/hub/%20/api/ingest?key=1"
        );
    }

    #[test]
    fn discovers_bound_addresses_and_running_tailnet() {
        for (host, expected) in [
            ("localhost", "127.0.0.1"),
            ("127.0.0.2", "127.0.0.2"),
            ("::1", "[::1]"),
            ("LOCALHOST.", "LOCALHOST."),
        ] {
            assert_eq!(
                discover_access_urls(8000, host, Some("my token")),
                [AccessUrl {
                    kind: "local".into(),
                    label: "Local".into(),
                    url: format!("http://{expected}:8000/?token=my+token")
                }]
            );
        }
        let urls = discover_access_urls(8000, "0.0.0.0", Some("key"));
        assert_eq!(urls[0].url, "http://127.0.0.1:8000/?token=key");
        for entry in &urls[1..] {
            assert!(matches!(
                entry.kind.as_str(),
                "lan" | "bonjour" | "tailscale"
            ));
            assert!(entry.url.ends_with(":8000/?token=key"));
        }
        assert_eq!(tailscale_host(r#"{"BackendState":"Running","Self":{"DNSName":"mini.tail.ts.net.","TailscaleIPs":["100.64.0.1"]}}"#).as_deref(), Some("mini.tail.ts.net"));
        assert_eq!(
            tailscale_host(
                r#"{"BackendState":"Running","Self":{"TailscaleIPs":["fd00::1","100.64.0.1"]}}"#
            )
            .as_deref(),
            Some("100.64.0.1")
        );
        assert!(
            tailscale_host(r#"{"BackendState":"Stopped","Self":{"DNSName":"mini"}}"#).is_none()
        );
    }

    struct Received {
        path: String,
        auth: Option<String>,
        body: Vec<u8>,
    }

    fn hub(status: &str, body: &str) -> (String, thread::JoinHandle<Received>) {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", socket.local_addr().unwrap());
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nLocation: {origin}/redirected\r\nConnection: close\r\n\r\n{body}", body.len());
        let reader = thread::spawn(move || {
            let (stream, _) = socket.accept().unwrap();
            let mut stream = BufReader::new(stream);
            let mut path = String::new();
            stream.read_line(&mut path).unwrap();
            let mut auth = None;
            let mut length = 0;
            loop {
                let mut line = String::new();
                stream.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                let (key, value) = line.trim().split_once(':').unwrap();
                match key.to_ascii_lowercase().as_str() {
                    "authorization" => auth = Some(value.trim().into()),
                    "content-length" => length = value.trim().parse().unwrap(),
                    _ => {}
                }
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            stream.get_mut().write_all(response.as_bytes()).unwrap();
            Received { path, auth, body }
        });
        (origin, reader)
    }

    #[test]
    fn sync_posts_snapshot_and_reads_decision() {
        let (origin, received) = hub("200 OK", r#"{"ok":true,"run":{"id":"a"}}"#);
        let run = json!({"id":"a"});
        let snapshot = json!({"nodes":["é", "</script>"]});
        let result = push_snapshot(
            &format!("{origin}/base/?q=1#anchor"),
            Some("secret"),
            &run,
            &snapshot,
        )
        .unwrap();
        assert_eq!(Value::Object(result), run);
        let request = received.join().unwrap();
        assert_eq!(request.path, "POST /base/api/ingest?q=1 HTTP/1.1\r\n");
        assert_eq!(request.auth.as_deref(), Some("Bearer secret"));
        assert_eq!(
            request.body,
            br#"{"run": {"id": "a"}, "snapshot": {"nodes": ["\u00e9", "</script>"]}}"#
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            json!({"run":run,"snapshot":snapshot})
        );

        let (origin, received) = hub("200 OK", r#"{"decision":"continue"}"#);
        let result = fetch_remote_response(&origin, None, "pilot-01")
            .unwrap()
            .unwrap();
        assert_eq!(result["decision"], "continue");
        let request = received.join().unwrap();
        assert_eq!(request.path, "GET /api/runs/pilot-01/response HTTP/1.1\r\n");
        assert_eq!(request.auth, None);
    }

    #[test]
    fn sync_handles_absence_invalid_bodies_and_redirects() {
        let (origin, received) = hub("404 Not Found", "");
        assert_eq!(
            fetch_remote_response(&origin, Some("secret"), "a").unwrap(),
            None
        );
        assert_eq!(
            received.join().unwrap().auth.as_deref(),
            Some("Bearer secret")
        );
        for (status, body, message) in [
            ("302 Found", "", "Hero Town Hub returned HTTP 302 Found"),
            (
                "403 Forbidden",
                "",
                "Hero Town Hub returned HTTP 403 Forbidden",
            ),
            (
                "200 OK",
                "null",
                "Hero Town Hub response must be a JSON object",
            ),
        ] {
            let (origin, received) = hub(status, body);
            assert_eq!(
                fetch_remote_response(&origin, Some("token"), "a").unwrap_err(),
                message
            );
            received.join().unwrap();
        }
        for body in [r#"{"ok":1,"run":{}}"#, r#"{"ok":true,"run":[]}"#, "[]"] {
            let (origin, received) = hub("200 OK", body);
            assert_eq!(
                push_snapshot(&origin, None, &Value::Null, &Value::Null).unwrap_err(),
                "Hero Town Hub did not acknowledge the snapshot"
            );
            received.join().unwrap();
        }
    }

    #[test]
    #[cfg(unix)]
    fn tunnel_reads_either_stream_and_reaps_process() {
        for redirect in ["", ">&2"] {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(format!(
                "echo 'ready https://town-123.trycloudflare.com!' {redirect}; exec sleep 20"
            ));
            let tunnel = CloudflareTunnel::start_command(cmd, 2).unwrap();
            assert_eq!(tunnel.url, "https://town-123.trycloudflare.com");
            let pid = tunnel.child.id();
            drop(tunnel);
            // SAFETY: signal 0 checks process existence.
            assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        }
        let err = CloudflareTunnel::start_command(Command::new("true"), 2).unwrap_err();
        assert!(err.contains("exited before reporting"));
        let mut cmd = Command::new("sleep");
        cmd.arg("20");
        assert!(CloudflareTunnel::start_command(cmd, 0)
            .unwrap_err()
            .contains("within 0s"));
    }
}
