//! Self-update: download, verify, and atomically replace the running binary.
//!
//! The security model mirrors the installer (`install.sh`):
//!   - **Authenticity** — `checksums.txt` is signed with an Ed25519 key held
//!     offline by the maintainer. The signature is verified in-process against
//!     the pinned public key (`raven-signing-key.pub`) before anything is
//!     trusted.
//!   - **Integrity** — the downloaded binary's SHA-256 must match the entry in
//!     the (authenticated) `checksums.txt`.
//!
//! Both checks fail closed: a missing or invalid checksum/signature refuses the
//! update rather than silently installing an unverified binary.

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Default base URL for release artifacts, matching the installers.
pub const DEFAULT_RELEASE_BASE_URL: &str =
    "https://github.com/raythurman2386/raven/releases/download";

/// Pinned Ed25519 public key (PEM) — the root of trust for release authenticity.
///
/// This is the single source of truth (`raven-signing-key.pub`), embedded at
/// compile time so the update path never depends on a runtime file.
pub const SIGNING_PUBLIC_KEY_PEM: &str = include_str!("../raven-signing-key.pub");

/// Resolve the public key used for signature verification.
///
/// Defaults to the embedded pinned key. `RAVEN_SIGNING_PUBLIC_KEY` overrides it
/// (mirroring the `RAVEN_RELEASE_BASE_URL` override) so self-hosted mirrors and
/// the integration tests can pin their own trust root.
fn signing_public_key() -> String {
    std::env::var("RAVEN_SIGNING_PUBLIC_KEY").unwrap_or_else(|_| SIGNING_PUBLIC_KEY_PEM.to_string())
}

/// Arguments for the `raven self update` subcommand.
#[derive(clap::Args, Clone, Debug)]
pub struct UpdateArgs {
    /// Pin a specific release version (e.g. `0.5.1` or `v0.5.1`).
    #[arg(long)]
    pub version: Option<String>,

    /// Restore the previous binary from the `.old` backup.
    #[arg(long)]
    pub rollback: bool,
}

/// Entry point for `raven self update`.
pub async fn run(args: UpdateArgs) -> Result<()> {
    if args.rollback {
        let target = std::env::current_exe().context("failed to resolve current executable")?;
        rollback(&target)?;
        println!("==> Rolled back raven to the previous version");
        return Ok(());
    }
    update(args.version.as_deref()).await
}

/// Download, verify, and install a release, replacing the running binary.
async fn update(pinned_version: Option<&str>) -> Result<()> {
    let base_url = std::env::var("RAVEN_RELEASE_BASE_URL")
        .unwrap_or_else(|_| DEFAULT_RELEASE_BASE_URL.to_string());
    let base_url = base_url.trim_end_matches('/');

    let triple = detect_triple()?;
    let version_tag = match pinned_version {
        Some(v) => normalize_version_tag(v),
        None => latest_version_tag().await?,
    };
    let version_no_v = version_tag.trim_start_matches('v');
    let artifact = format!("raven-{version_no_v}-{triple}");

    let artifact_url = format!("{base_url}/{version_tag}/{artifact}");
    let checksums_url = format!("{base_url}/{version_tag}/checksums.txt");
    let signature_url = format!("{base_url}/{version_tag}/checksums.txt.sig");

    println!("==> Platform:  {triple}");
    println!("==> Version:   {version_tag}");
    println!("==> Artifact:  {artifact}");

    let binary = download(&artifact_url).await?;
    let checksums = download(&checksums_url).await?;
    let signature = download(&signature_url).await?;

    // Authenticity: the checksums file must verify against the pinned Ed25519
    // public key. This proves the checksums (and therefore the binary) were
    // produced by the raven maintainers, not tampered with in transit.
    if !verify_signature(&checksums, &signature, &signing_public_key()) {
        bail!("release signature verification FAILED for checksums.txt; refusing to update");
    }
    println!("==> Signature OK");

    // Integrity: the binary's SHA-256 must match the signed checksums entry.
    let expected = find_checksum(&checksums, &artifact)?;
    if !verify_checksum(&binary, &expected) {
        bail!("checksum mismatch for {artifact}; refusing to update");
    }
    println!("==> Checksum OK");

    let target = std::env::current_exe().context("failed to resolve current executable")?;
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent).context("failed to create temp file")?;
    tmp.as_file_mut()
        .write_all(&binary)
        .context("failed to write downloaded binary")?;
    make_executable(tmp.path());
    // Close the handle and keep the path, then move the file into place.
    // `NamedTempFile` does not use FILE_FLAG_DELETE_ON_CLOSE, so closing the
    // handle does not delete the file; the TempPath only deletes it on drop if
    // the rename below fails.
    let tmp_path = tmp.into_temp_path();
    atomic_replace(&tmp_path, &target)?;

    println!("==> Updated raven to {version_tag}");
    Ok(())
}

/// Verify `bytes` against an expected SHA-256 hex digest (case-insensitive).
pub fn verify_checksum(bytes: &[u8], expected_sha256: &str) -> bool {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    hex_encode(digest.as_ref()).eq_ignore_ascii_case(expected_sha256.trim())
}

/// Verify an Ed25519 signature over `checksums_bytes` against a PEM public key.
///
/// Returns `false` (never panics) on any malformed input, so callers fail
/// closed.
pub fn verify_signature(checksums_bytes: &[u8], sig_bytes: &[u8], public_key_pem: &str) -> bool {
    let Some(raw_key) = parse_ed25519_public_key_pem(public_key_pem) else {
        return false;
    };
    let public_key = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &raw_key);
    public_key.verify(checksums_bytes, sig_bytes).is_ok()
}

/// Atomically replace `target_path` with `new_path`, keeping a `.old` backup.
///
/// The running binary is renamed aside (not overwritten) before the new one is
/// moved into place, keeping a `.old` backup for rollback.
pub fn atomic_replace(new_path: &Path, target_path: &Path) -> Result<()> {
    let backup = old_backup_path(target_path);
    if backup.exists() {
        std::fs::remove_file(&backup)
            .with_context(|| format!("failed to remove stale backup {}", backup.display()))?;
    }
    if target_path.exists() {
        std::fs::rename(target_path, &backup)
            .with_context(|| format!("failed to back up {}", target_path.display()))?;
    }
    std::fs::rename(new_path, target_path)
        .with_context(|| format!("failed to install {}", target_path.display()))?;
    Ok(())
}

/// Restore the previous binary from the `.old` backup.
pub fn rollback(target_path: &Path) -> Result<()> {
    let backup = old_backup_path(target_path);
    if !backup.exists() {
        bail!("no backup binary found at {}", backup.display());
    }
    if target_path.exists() {
        // Rename the current binary aside first, then restore the backup into
        // its place.
        let stale = target_path.with_extension("stale");
        if stale.exists() {
            std::fs::remove_file(&stale)
                .with_context(|| format!("failed to remove {}", stale.display()))?;
        }
        std::fs::rename(target_path, &stale)
            .with_context(|| format!("failed to move aside {}", target_path.display()))?;
        let _ = std::fs::remove_file(&stale);
    }
    std::fs::rename(&backup, target_path)
        .with_context(|| format!("failed to restore {}", backup.display()))?;
    Ok(())
}

/// The `.old` backup path for a given binary path.
fn old_backup_path(target_path: &Path) -> PathBuf {
    let mut name = target_path.as_os_str().to_os_string();
    name.push(".old");
    PathBuf::from(name)
}

/// Mark a file executable on Unix.
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o755);
        let _ = std::fs::set_permissions(path, perms);
    }
}

/// Extract the raw 32-byte Ed25519 public key from a SubjectPublicKeyInfo PEM.
fn parse_ed25519_public_key_pem(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .ok()?;
    // Ed25519 SPKI DER: 12-byte header (SEQUENCE { SEQUENCE { OID 1.3.101.112 },
    // BIT STRING }) followed by the 32-byte raw public key.
    const PREFIX: &[u8] = &[
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    if der.len() != 44 || &der[..12] != PREFIX {
        return None;
    }
    Some(der[12..].to_vec())
}

/// Find the SHA-256 entry for `artifact` in a `checksums.txt` body.
fn find_checksum(checksums: &[u8], artifact: &str) -> Result<String> {
    let text = std::str::from_utf8(checksums).context("checksums.txt is not valid UTF-8")?;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(hash), Some(name)) = (parts.next(), parts.next()) {
            if name == artifact && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                return Ok(hash.to_string());
            }
        }
    }
    bail!("no checksum entry found for {artifact} in checksums.txt")
}

/// Map the current platform to the release triple used by the installer.
fn detect_triple() -> Result<String> {
    let triple = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "arm") => "armv7-unknown-linux-gnueabihf",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        (os, arch) => bail!("unsupported platform: {os}/{arch}"),
    };
    Ok(triple.to_string())
}

/// Normalize a user-supplied version to a `v`-prefixed release tag.
fn normalize_version_tag(version: &str) -> String {
    if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    }
}

/// Query GitHub for the latest release tag.
///
/// Follows the `/releases/latest` redirect instead of the API endpoint. The
/// API is rate-limited to 60 req/hr per IP for unauthenticated clients, which
/// breaks self-update on shared/NAT'd networks; the redirect is not limited.
async fn latest_version_tag() -> Result<String> {
    let url = "https://github.com/raythurman2386/raven/releases/latest";
    let resp = reqwest::Client::new()
        .get(url)
        .header("User-Agent", "raven-self-update")
        .send()
        .await
        .context("failed to query GitHub for the latest release")?;
    if !resp.status().is_success() {
        bail!("failed to determine latest version: {}", resp.status());
    }
    let final_url = resp.url().as_str();
    let tag = final_url
        .rsplit('/')
        .next()
        .context("missing tag in redirect URL")?;
    Ok(tag.to_string())
}

/// Download a URL to memory. Supports `http(s)://`, `file://`, and local paths
/// (the latter two mirror the installers' `fetch` helper for offline testing).
async fn download(url: &str) -> Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).with_context(|| format!("failed to read {path}"));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let resp = reqwest::get(url)
            .await
            .with_context(|| format!("failed to download {url}"))?;
        if !resp.status().is_success() {
            bail!("download failed: {url} returned {}", resp.status());
        }
        return resp
            .bytes()
            .await
            .map(|b| b.to_vec())
            .with_context(|| format!("failed to read response body from {url}"));
    }
    std::fs::read(url).with_context(|| format!("failed to read {url}"))
}

/// Lowercase hex encoding of a byte slice.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    /// Build a SubjectPublicKeyInfo PEM for a raw 32-byte Ed25519 public key.
    fn spki_pem(public_key: &[u8]) -> String {
        const PREFIX: &[u8] = &[
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        let mut der = PREFIX.to_vec();
        der.extend_from_slice(public_key);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&der);
        format!("-----BEGIN PUBLIC KEY-----\n{b64}\n-----END PUBLIC KEY-----\n")
    }

    #[test]
    fn verify_checksum_matches_and_rejects() {
        let bytes = b"hello world";
        let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
        let expected = hex_encode(digest.as_ref());
        assert!(verify_checksum(bytes, &expected));
        assert!(verify_checksum(bytes, &expected.to_uppercase()));
        assert!(!verify_checksum(bytes, "deadbeef"));
        assert!(!verify_checksum(b"other", &expected));
    }

    #[test]
    fn verify_signature_roundtrip() {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let keypair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let pem = spki_pem(keypair.public_key().as_ref());
        let msg = b"checksums content";
        let sig = keypair.sign(msg);
        assert!(verify_signature(msg, sig.as_ref(), &pem));
        assert!(!verify_signature(b"tampered", sig.as_ref(), &pem));
        assert!(!verify_signature(msg, sig.as_ref(), "not a pem"));
    }

    #[test]
    fn parse_public_key_rejects_malformed() {
        assert!(parse_ed25519_public_key_pem("garbage").is_none());
        assert!(parse_ed25519_public_key_pem("").is_none());
        assert!(parse_ed25519_public_key_pem(
            "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n"
        )
        .is_none());
    }

    #[test]
    fn find_checksum_locates_artifact() {
        let body = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  raven-0.5.1-x86_64-unknown-linux-gnu\nbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  raven-0.5.1-x86_64-unknown-linux-gnu.tar.gz\n";
        let found = find_checksum(body, "raven-0.5.1-x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(found, "a".repeat(64));
        assert!(find_checksum(body, "raven-0.5.1-missing").is_err());
    }

    #[test]
    fn normalize_version_tag_adds_v() {
        assert_eq!(normalize_version_tag("0.5.1"), "v0.5.1");
        assert_eq!(normalize_version_tag("v0.5.1"), "v0.5.1");
    }

    #[test]
    fn detect_triple_matches_host() {
        let triple = detect_triple().unwrap();
        assert!(triple.contains(std::env::consts::ARCH) || triple.contains("armv7"));
    }

    #[test]
    fn atomic_replace_and_rollback_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("raven");
        let new = dir.path().join("new");
        std::fs::write(&new, b"new binary").unwrap();

        // First install: no prior binary, so no backup is created.
        atomic_replace(&new, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new binary");
        assert!(!old_backup_path(&target).exists());

        // Second install: the prior binary is backed up to `.old`.
        let newer = dir.path().join("newer");
        std::fs::write(&newer, b"newer binary").unwrap();
        atomic_replace(&newer, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"newer binary");
        assert_eq!(
            std::fs::read(old_backup_path(&target)).unwrap(),
            b"new binary"
        );

        // Rollback restores the backup.
        rollback(&target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new binary");
        assert!(!old_backup_path(&target).exists());

        // Rollback with no backup fails closed.
        assert!(rollback(&target).is_err());
    }
}
