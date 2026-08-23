//! Best-effort secret scanner for staged git commits.
//!
//! This is a **guardrail, not a security boundary**. It matches well-known
//! credential prefixes and PEM private-key headers so `git_commit` can refuse
//! to snapshot obvious secrets. It does not replace `.gitignore`, `git-secrets`,
//! or a dedicated pre-commit hook, and it will miss novel or obfuscated values.

use std::sync::OnceLock;

use regex::Regex;

/// A single secret-like match in a staged file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFinding {
    /// Workspace-relative path that contained the match.
    pub path: String,
    /// Human-readable rule name (never the secret itself).
    pub kind: &'static str,
}

struct SecretRule {
    kind: &'static str,
    re: Regex,
}

fn rules() -> &'static [SecretRule] {
    static RULES: OnceLock<Vec<SecretRule>> = OnceLock::new();
    RULES.get_or_init(|| {
        const SPECS: &[(&str, &str)] = &[
            ("AWS access key ID", r"AKIA[0-9A-Z]{16}"),
            ("GitHub personal access token", r"ghp_[A-Za-z0-9]{36}"),
            (
                "GitHub fine-grained personal access token",
                r"github_pat_[A-Za-z0-9_]{20,}",
            ),
            ("GitHub OAuth token", r"gho_[A-Za-z0-9]{36}"),
            ("GitHub App token", r"ghs_[A-Za-z0-9]{36}"),
            ("GitLab personal access token", r"glpat-[A-Za-z0-9\-_]{20,}"),
            ("Slack token", r"xox[baprs]-[A-Za-z0-9-]{10,}"),
            (
                "Slack incoming webhook",
                r"hooks\.slack\.com/services/T[A-Z0-9]+/B[A-Z0-9]+/[A-Za-z0-9]+",
            ),
            ("OpenAI API key", r"sk-proj-[A-Za-z0-9_-]{20,}"),
            ("OpenAI API key", r"sk-[A-Za-z0-9]{48}"),
            ("Anthropic API key", r"sk-ant-[A-Za-z0-9\-_]{20,}"),
            ("OpenRouter API key", r"sk-or-v1-[A-Za-z0-9]{20,}"),
            ("Stripe live secret key", r"sk_live_[A-Za-z0-9]{20,}"),
            ("Stripe live restricted key", r"rk_live_[A-Za-z0-9]{20,}"),
            ("Google API key", r"AIza[0-9A-Za-z\-_]{35}"),
            ("npm access token", r"npm_[A-Za-z0-9]{36}"),
            ("Hugging Face token", r"hf_[A-Za-z0-9]{32,}"),
            (
                "PEM private key",
                r"-----BEGIN (?:RSA |DSA |EC |OPENSSH |ENCRYPTED |PGP )?PRIVATE KEY-----",
            ),
            (
                "JSON Web Token",
                r"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
            ),
        ];
        SPECS
            .iter()
            .map(|(kind, pat)| SecretRule {
                kind,
                re: Regex::new(pat).expect("valid secret regex"),
            })
            .collect()
    })
}

/// Filename suffixes that are almost always generated lockfiles / binaries.
/// Scanning them is noisy and they are a poor place to store credentials.
fn skip_path(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "cargo.lock"
            | "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "bun.lock"
            | "bun.lockb"
            | "go.sum"
            | "composer.lock"
            | "gemfile.lock"
            | "poetry.lock"
            | "flake.lock"
    ) || lower.ends_with(".min.js")
        || lower.ends_with(".min.css")
        || lower.ends_with(".map")
        || lower.ends_with(".wasm")
        || lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
        || lower.ends_with(".ico")
        || lower.ends_with(".pdf")
        || lower.ends_with(".zip")
        || lower.ends_with(".gz")
        || lower.ends_with(".tar")
        || lower.ends_with(".woff")
        || lower.ends_with(".woff2")
        || lower.ends_with(".ttf")
        || lower.ends_with(".exe")
        || lower.ends_with(".dll")
        || lower.ends_with(".so")
        || lower.ends_with(".dylib")
}

fn looks_like_placeholder(matched: &str) -> bool {
    let upper = matched.to_ascii_uppercase();
    upper.contains("EXAMPLE")
        || upper.contains("SAMPLE")
        || upper.contains("PLACEHOLDER")
        || upper.contains("CHANGEME")
        || upper.contains("REDACTED")
        || upper.contains("YOUR_")
        || upper.contains("DUMMY")
        || upper.contains("XXXXX")
        || matched.contains("...")
}

/// Maximum bytes scanned per staged file (head only).
const MAX_SCAN_BYTES: usize = 1_048_576;
const MAX_FINDINGS: usize = 12;

/// Scan file bytes for well-known secret patterns.
///
/// Binary content (NUL in the first 8 KiB) is skipped. Findings never include
/// the matched secret — only the path and rule name.
pub fn scan_bytes(path: &str, bytes: &[u8]) -> Vec<SecretFinding> {
    if skip_path(path) {
        return Vec::new();
    }
    let head = if bytes.len() > MAX_SCAN_BYTES {
        &bytes[..MAX_SCAN_BYTES]
    } else {
        bytes
    };
    let probe = if head.len() > 8192 {
        &head[..8192]
    } else {
        head
    };
    if probe.contains(&0) {
        return Vec::new();
    }
    let Ok(text) = std::str::from_utf8(head) else {
        return Vec::new();
    };
    scan_text(path, text)
}

/// Scan UTF-8 text for well-known secret patterns.
pub fn scan_text(path: &str, text: &str) -> Vec<SecretFinding> {
    if skip_path(path) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for rule in rules() {
        for mat in rule.re.find_iter(text) {
            if looks_like_placeholder(mat.as_str()) {
                continue;
            }
            let finding = SecretFinding {
                path: path.to_string(),
                kind: rule.kind,
            };
            if !out.contains(&finding) {
                out.push(finding);
            }
            if out.len() >= MAX_FINDINGS {
                return out;
            }
        }
    }
    out
}

/// Format findings into the tool-error string returned by `git_commit`.
pub fn format_refusal(findings: &[SecretFinding]) -> String {
    let mut out =
        String::from("Error: git_commit refused — possible secrets detected in staged changes:\n");
    for f in findings.iter().take(MAX_FINDINGS) {
        out.push_str(&format!("  {}: {}\n", f.path, f.kind));
    }
    out.push_str(
        "Remove or redact the secrets, then retry. `.env` / `.env.*` are already \
         excluded from staging. This gate never records the secret value.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_aws_access_key() {
        let hits = scan_text("src/config.rs", "key = \"AKIATESTTESTTEST1234\"");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "AWS access key ID");
        assert_eq!(hits[0].path, "src/config.rs");
    }

    #[test]
    fn detects_github_pat() {
        let tok = format!("ghp_{}", "a".repeat(36));
        let hits = scan_text("auth.toml", &format!("token = \"{tok}\""));
        assert_eq!(hits[0].kind, "GitHub personal access token");
    }

    #[test]
    fn detects_openrouter_key() {
        let tok = format!("sk-or-v1-{}", "b".repeat(32));
        let hits = scan_text("src/main.rs", &tok);
        assert_eq!(hits[0].kind, "OpenRouter API key");
    }

    #[test]
    fn detects_pem_private_key() {
        let hits = scan_text("id_rsa", "-----BEGIN RSA PRIVATE KEY-----\nMIIE");
        assert_eq!(hits[0].kind, "PEM private key");
    }

    #[test]
    fn detects_openssh_private_key() {
        let hits = scan_text("id_ed25519", "-----BEGIN OPENSSH PRIVATE KEY-----");
        assert_eq!(hits[0].kind, "PEM private key");
    }

    #[test]
    fn skips_aws_documentation_example() {
        let hits = scan_text("docs.md", "AKIAIOSFODNN7EXAMPLE");
        assert!(hits.is_empty(), "official AWS example is a placeholder");
    }

    #[test]
    fn skips_ellipsis_examples() {
        let hits = scan_text("README.md", "export RAVEN_API_KEY=sk-or-v1-abc123...");
        assert!(hits.is_empty());
    }

    #[test]
    fn skips_lockfiles() {
        let tok = format!("sk-or-v1-{}", "c".repeat(32));
        assert!(scan_text("Cargo.lock", &tok).is_empty());
        assert!(scan_text("package-lock.json", &tok).is_empty());
    }

    #[test]
    fn skips_binary_nul() {
        let mut bytes = b"AKIAABCDEFGHIJKLMNOP".to_vec();
        bytes.insert(3, 0);
        assert!(scan_bytes("blob.bin", &bytes).is_empty());
    }

    #[test]
    fn ignores_ordinary_source() {
        let src = "pub fn double(n: i32) -> i32 { n * 2 }\nconst MSG: &str = \"hello\";\n";
        assert!(scan_text("src/lib.rs", src).is_empty());
    }

    #[test]
    fn refusal_does_not_echo_secret() {
        let findings = scan_text("a.rs", "AKIATESTTESTTEST1234");
        let msg = format_refusal(&findings);
        assert!(msg.starts_with("Error:"));
        assert!(msg.contains("a.rs"));
        assert!(msg.contains("AWS access key ID"));
        assert!(!msg.contains("AKIATESTTESTTEST1234"));
    }

    #[test]
    fn detects_anthropic_and_stripe() {
        let ant = format!("sk-ant-{}", "aB3_".repeat(8));
        assert_eq!(scan_text("cfg.toml", &ant)[0].kind, "Anthropic API key");
        let stripe = format!("sk_live_{}", "9fQ2".repeat(8));
        assert_eq!(
            scan_text("pay.toml", &stripe)[0].kind,
            "Stripe live secret key"
        );
    }
}
