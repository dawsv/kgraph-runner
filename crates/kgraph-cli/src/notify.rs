use kgraph_core::repair::redact_secrets;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};

pub struct NotifyConfig<'a> {
    pub webhook_url: Option<&'a str>,
    pub webhook_secret: Option<&'a str>,
    pub desktop: bool,
}

/// Pure-Rust HMAC-SHA256 hex digest (RFC 2104) with zero extra dependencies.
pub fn hmac_sha256_hex(secret: &[u8], payload: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if secret.len() > BLOCK {
        let digest = Sha256::digest(secret);
        key_block[..32].copy_from_slice(&digest);
    } else {
        key_block[..secret.len()].copy_from_slice(secret);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(payload);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    hex::encode(outer.finalize())
}

pub fn send(
    cfg: &NotifyConfig<'_>,
    status: &str,
    workflow: &Path,
    report: &Path,
    detail: &str,
    packets: &[String],
) {
    let clean_detail = redact_secrets(detail);
    if let Some(url) = cfg.webhook_url.filter(|u| !u.is_empty()) {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            eprintln!("kgraph-town: ignoring non-http(s) --notify-webhook URL");
        } else {
            let body = json!({
                "event": format!("run_{status}"),
                "status": status,
                "workflow": workflow,
                "report": report,
                "detail": clean_detail,
                "waiting_packets": packets,
            });
            if let Ok(bytes) = serde_json::to_vec(&body) {
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build();
                if let Ok(client) = client {
                    let mut req = client
                        .post(url)
                        .header("Content-Type", "application/json")
                        .header("X-KGraph-Event", format!("run_{status}"))
                        .body(bytes.clone());
                    if let Some(secret) = cfg.webhook_secret.filter(|s| !s.is_empty()) {
                        let sig = hmac_sha256_hex(secret.as_bytes(), &bytes);
                        req = req.header("X-KGraph-Signature", format!("sha256={sig}"));
                    }
                    if let Err(e) = req.send() {
                        eprintln!("kgraph-town: webhook notification failed: {e}");
                    }
                }
            }
        }
    }
    if cfg.desktop {
        send_desktop(status, workflow, &clean_detail);
    }
}

fn send_desktop(status: &str, workflow: &Path, detail: &str) {
    let name = workflow
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("workflow");
    let title = format!("KGraph: {status}");
    let summary = if detail.is_empty() {
        format!("{name} finished ({status})")
    } else {
        let short: String = detail.chars().take(160).collect();
        format!("{name}: {short}")
    };
    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            esc(&summary),
            esc(&title)
        );
        let _ = Command::new("osascript")
            .args(["-e", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        use std::process::{Command, Stdio};
        let _ = Command::new("notify-send")
            .args([&title, &summary])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (title, summary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_matches_rfc_4231_test_vector() {
        let sig = hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            sig,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
