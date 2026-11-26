//! Credential filtering shared by telemetry and bounded log reads.
use once_cell::sync::Lazy;
use regex::Regex;

static CREDENTIAL: Lazy<Regex> = Lazy::new(|| {
    let value = r#"(?:"[^"\r\n]*"|'[^'\r\n]*'|(?:https?://)?[^\s"'&,;]+)"#;
    Regex::new(&format!(concat!(
        r"(?i)(?:\b(?:bearer|basic)\s+[\w.+/=-]+|",
        r#"\b[\w-]*(?:token|password|passwd|secret|api[_ -]?key|credential|private[_ -]?key)[\w-]*["']?\s*(?:=|:)\s*{0}|"#,
        r"--(?:[\w-]+-)?(?:token|password|secret|api-key)\s+{0}|",
        r"https?://[^\s/]+:[^\s/@]+@|",
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)|",
        r"\b(?:gh[pousr]_|github_pat_|xox[baprs]-|sk-(?:proj-)?|npm_|AIza)[\w-]{{16,}}|",
        r"\b(?:AKIA|ASIA)[A-Z0-9]{{16}}\b)"
    ), value)).expect("credential pattern")
});

// Python treats letters, numbers and underscore as word characters. Rust's
// Unicode word class also contains combining marks and join controls. Project
// just the differing characters before matching, retaining original offsets.
static CLASS_DIFFERENCES: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[\x1c-\x1fİı]|[\w&&[^\p{L}\p{N}_]]").unwrap());

fn contains_ascii_ci(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle))
}

// Fast conservative prefilter: avoids compiling/running the heavy Unicode regexes
// on ordinary ASCII text that cannot match any branch of CREDENTIAL.
fn may_contain_credential(input: &str) -> bool {
    let bytes = input.as_bytes();
    if bytes.iter().any(|&b| !b.is_ascii()) {
        return true;
    }
    let has_eq_or_colon = bytes.iter().any(|&b| b == b'=' || b == b':');
    if has_eq_or_colon
        && (bytes.contains(&b'@')
            || contains_ascii_ci(bytes, b"token")
            || contains_ascii_ci(bytes, b"pass")
            || contains_ascii_ci(bytes, b"secret")
            || contains_ascii_ci(bytes, b"key")
            || contains_ascii_ci(bytes, b"credential"))
    {
        return true;
    }
    bytes.windows(2).any(|w| w == b"--")
        || contains_ascii_ci(bytes, b"bearer")
        || contains_ascii_ci(bytes, b"basic")
        || contains_ascii_ci(bytes, b"aiza")
        || contains_ascii_ci(bytes, b"akia")
        || contains_ascii_ci(bytes, b"asia")
        || contains_ascii_ci(bytes, b"sk-")
        || contains_ascii_ci(bytes, b"npm_")
        || contains_ascii_ci(bytes, b"xox")
        || contains_ascii_ci(bytes, b"ghp_")
        || contains_ascii_ci(bytes, b"gho_")
        || contains_ascii_ci(bytes, b"ghu_")
        || contains_ascii_ci(bytes, b"ghs_")
        || contains_ascii_ci(bytes, b"ghr_")
        || contains_ascii_ci(bytes, b"github_pat_")
}

struct Matcher<'a> {
    text: std::borrow::Cow<'a, str>,
    offsets: Vec<usize>,
}

impl<'a> Matcher<'a> {
    fn new(input: &'a str) -> Self {
        let needs_projection = input
            .bytes()
            .any(|b| !b.is_ascii() || (0x1c..=0x1f).contains(&b));
        if !needs_projection || !CLASS_DIFFERENCES.is_match(input) {
            return Self {
                text: input.into(),
                offsets: Vec::new(),
            };
        }
        let mut text = String::with_capacity(input.len());
        let mut offsets = Vec::with_capacity(input.len() + 1);
        let mut cursor = 0;
        for m in CLASS_DIFFERENCES.find_iter(input) {
            text.push_str(&input[cursor..m.start()]);
            offsets.extend(cursor..m.start());
            text.push(match m.as_str() {
                "İ" | "ı" => 'i',
                v if v.as_bytes()[0] < 32 => ' ',
                _ => '!',
            });
            offsets.push(m.start());
            cursor = m.end();
        }
        text.push_str(&input[cursor..]);
        offsets.extend(cursor..=input.len());
        Self {
            text: text.into(),
            offsets,
        }
    }
    fn spans(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        CREDENTIAL.find_iter(&self.text).map(|m| {
            if self.offsets.is_empty() {
                (m.start(), m.end())
            } else {
                (self.offsets[m.start()], self.offsets[m.end()])
            }
        })
    }
}

fn redact_from(input: &str, cutoff: usize) -> String {
    if !may_contain_credential(input) {
        return input[cutoff..].to_owned();
    }
    let matcher = Matcher::new(input);
    let mut result = String::new();
    let mut cursor = cutoff;
    for (start, end) in matcher.spans() {
        if end <= cursor {
            continue;
        }
        if start > cursor {
            result.push_str(&input[cursor..start]);
        }
        result.push_str("[redacted]");
        cursor = end;
    }
    result.push_str(&input[cursor..]);
    result
}
pub fn redact_credentials(text: &str) -> String {
    redact_from(text, 0)
}

pub fn has_credentials(text: &str) -> bool {
    may_contain_credential(text) && CREDENTIAL.is_match(&Matcher::new(text).text)
}

/// Cut at a UTF-8 byte offset in the original window. When a match crosses
/// the cutoff, keep the replacement in the visible part of the tail.
pub fn redact_tail_with_lookback(window: &str, cutoff: usize, max_bytes: usize) -> String {
    let mut start = cutoff.min(window.len());
    while !window.is_char_boundary(start) {
        start -= 1;
    }
    let result = redact_from(window, start);
    let skip = result.chars().count().saturating_sub(max_bytes);
    result.chars().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_python_word_boundaries_and_whitespace() {
        assert_eq!(
            redact_credentials("e\u{301}Bearer secret"),
            "e\u{301}[redacted]"
        );
        assert_eq!(redact_credentials("Bearer\u{1c}secret"), "[redacted]");
        assert_eq!(redact_credentials("apİ_key=x"), "[redacted]");
        assert_eq!(redact_credentials("Bearer a\u{301}b"), "[redacted]\u{301}b");
    }
    #[test]
    fn redacts_each_credential_across_every_interior_cutoff() {
        for secret in [
            "Bearer abc.def",
            "Basic MTIz==",
            "api_key='a b'",
            "--db-password a:b",
            "https://user:secret@",
            "-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----",
            "-----BEGIN PRIVATE KEY-----\nunclosed",
            "github_pat_abcdefghijklmnop",
            "xoxb-abcdefghijklmnop",
            "sk-proj-abcdefghijklmnop",
            "npm_abcdefghijklmnop",
            "AIzaabcdefghijklmnop",
            "AKIA1234567890123456",
            "ASIA1234567890123456",
        ] {
            assert_eq!(redact_credentials(secret), "[redacted]");
            for cut in 0..secret.len() {
                assert_eq!(
                    redact_tail_with_lookback(secret, cut, 256),
                    "[redacted]",
                    "{secret} at {cut}"
                );
            }
        }
        assert_eq!(
            redact_tail_with_lookback("é token='hidden' after", 18, 256),
            "after"
        );
    }
}
