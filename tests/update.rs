//! Integration tests for `raven self update`.
//!
//! These serve a canned release layout over a local `TcpListener` (no network
//! access) and exercise the full update path: download, signature verification,
//! checksum verification, atomic replacement, and rollback. The pinned public
//! key is overridden via `RAVEN_SIGNING_PUBLIC_KEY` (the same override pattern
//! as `RAVEN_RELEASE_BASE_URL`) so the test can sign with a throwaway keypair.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};

/// The compiled `raven` binary, provided by Cargo for integration tests.
fn raven_bin() -> &'static str {
    env!("CARGO_BIN_EXE_raven")
}

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

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// A tiny HTTP server serving a canned release layout from a directory.
///
/// Routes are `/{version}/{file}`; the directory is expected to contain
/// `checksums.txt`, `checksums.txt.sig`, and the artifact binary.
struct ReleaseServer {
    listener: TcpListener,
    root: std::path::PathBuf,
}

impl ReleaseServer {
    fn start(root: &Path) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        Ok(Self {
            listener,
            root: root.to_path_buf(),
        })
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.listener.local_addr().unwrap())
    }

    fn serve(self) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            for stream in self.listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();

                let file = self.root.join(path.trim_start_matches('/'));
                let (status, body) = match std::fs::read(&file) {
                    Ok(bytes) => ("200 OK", bytes),
                    Err(_) => ("404 Not Found", b"not found".to_vec()),
                };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
            }
        })
    }
}

/// Build a canned release directory with a signed checksums.txt.
///
/// Returns the public key PEM (to pass via `RAVEN_SIGNING_PUBLIC_KEY`) and the
/// artifact name.
fn build_release(dir: &Path, version: &str, triple: &str, binary: &[u8]) -> (String, String) {
    let artifact = format!("raven-{version}-{triple}");
    std::fs::create_dir_all(dir.join(format!("v{version}"))).unwrap();
    std::fs::write(dir.join(format!("v{version}/{artifact}")), binary).unwrap();

    let digest = ring::digest::digest(&ring::digest::SHA256, binary);
    let hash = hex_encode(digest.as_ref());
    let checksums = format!("{hash}  {artifact}\n");
    std::fs::write(dir.join(format!("v{version}/checksums.txt")), &checksums).unwrap();

    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let keypair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let sig = keypair.sign(checksums.as_bytes());
    std::fs::write(
        dir.join(format!("v{version}/checksums.txt.sig")),
        sig.as_ref(),
    )
    .unwrap();

    (spki_pem(keypair.public_key().as_ref()), artifact)
}

/// The host triple, matching `update::detect_triple` for the test host.
fn host_triple() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        _ => panic!("unsupported test host"),
    }
}

#[test]
fn self_update_replaces_and_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let release = dir.path().join("release");
    let (pubkey, _) = build_release(
        &release,
        "0.5.1",
        host_triple(),
        b"#!/bin/sh\necho fake raven\n",
    );

    let server = ReleaseServer::start(&release).unwrap();
    let base = server.base_url();
    let handle = server.serve();

    // Copy the real binary to a temp location so we can observe replacement.
    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let target = bin_dir.join("raven");
    std::fs::copy(raven_bin(), &target).unwrap();

    let out = Command::new(&target)
        .args(["self", "update", "--version", "0.5.1"])
        .env("RAVEN_RELEASE_BASE_URL", &base)
        .env("RAVEN_SIGNING_PUBLIC_KEY", &pubkey)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "self update should succeed, stdout: {stdout}, stderr: {stderr}"
    );
    assert!(stdout.contains("Signature OK"), "stdout: {stdout}");
    assert!(stdout.contains("Checksum OK"), "stdout: {stdout}");

    // The binary was replaced with the fake one.
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"#!/bin/sh\necho fake raven\n",
        "binary should be replaced with the downloaded artifact"
    );
    // A `.old` backup of the original binary exists.
    let backup = bin_dir.join("raven.old");
    assert!(backup.exists(), "a .old backup should be kept");
    assert_eq!(
        std::fs::read(&backup).unwrap(),
        std::fs::read(raven_bin()).unwrap(),
        "backup should be the original binary"
    );

    drop(handle);
}

#[test]
fn rollback_restores_backup() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("raven");
    std::fs::copy(raven_bin(), &target).unwrap();
    // A fake "previous" binary stands in for the .old backup.
    let backup = dir.path().join("raven.old");
    std::fs::write(&backup, b"previous binary").unwrap();

    let out = Command::new(&target)
        .args(["self", "update", "--rollback"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "rollback should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"previous binary",
        "rollback should restore the .old backup"
    );
    assert!(!backup.exists(), "backup should be consumed by rollback");
}

#[test]
fn self_update_fails_closed_on_bad_signature() {
    let dir = tempfile::tempdir().unwrap();
    let release = dir.path().join("release");
    let (pubkey, _) = build_release(
        &release,
        "0.5.1",
        host_triple(),
        b"#!/bin/sh\necho fake raven\n",
    );

    // Tamper with checksums.txt after signing so the signature no longer
    // verifies.
    let checksums_path = release.join("v0.5.1/checksums.txt");
    let mut text = std::fs::read_to_string(&checksums_path).unwrap();
    text.push_str("deadbeef  extra\n");
    std::fs::write(&checksums_path, text).unwrap();

    let server = ReleaseServer::start(&release).unwrap();
    let base = server.base_url();
    let handle = server.serve();

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let target = bin_dir.join("raven");
    std::fs::copy(raven_bin(), &target).unwrap();

    let out = Command::new(&target)
        .args(["self", "update", "--version", "0.5.1"])
        .env("RAVEN_RELEASE_BASE_URL", &base)
        .env("RAVEN_SIGNING_PUBLIC_KEY", &pubkey)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "tampered checksums must fail closed, stderr: {stderr}"
    );
    assert!(
        stderr.contains("signature"),
        "should report a signature failure, stderr: {stderr}"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        std::fs::read(raven_bin()).unwrap(),
        "fail-closed update must not modify the binary"
    );

    drop(handle);
}

#[test]
fn rollback_without_backup_fails() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("raven");
    std::fs::copy(raven_bin(), &target).unwrap();

    let out = Command::new(&target)
        .args(["self", "update", "--rollback"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "rollback with no backup must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no backup") || stderr.contains("backup"),
        "should report missing backup, stderr: {stderr}"
    );
}
