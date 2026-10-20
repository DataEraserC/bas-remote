//! The SFTP bridge: a small SSH server installed *inside* the dev space.
//!
//! A BAS dev space runs dropbear and its image ships no `sftp-server`, so the
//! SFTP subsystem fails with exit code 127 and every SFTP-based tool breaks
//! (`rw_stat`, `rw_read_file`, `rw_write_file`, `rw_sync`, `rw_push`). There is
//! no way to install one: the image has no root and `/usr/lib` is read-only.
//!
//! So this module uploads a self-contained Node SSH server into the dev space's
//! home, starts it on the dev space's loopback as the dev-space user, and the
//! dev channel forwards its port. Both halves are ordinary SSH then: one
//! endpoint speaks exec, the other speaks exec + SFTP.
//!
//! Setup is idempotent: the server source is uploaded only when its digest
//! changes, `ssh2` is installed once, and the host key is generated once. A
//! daemon left behind by a dropped tunnel is replaced on the next run.

use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::error::{Code, Error, Result};
use crate::model::{BridgeInfo, BridgeState};
use crate::sshclient::SshClient;

/// Where the bridge lives inside the dev space, relative to `$HOME`.
pub const DEFAULT_BRIDGE_DIR: &str = ".dsh-bas-remote/sftp-bridge";
/// Port the bridge listens on inside the dev space.
pub const DEFAULT_BRIDGE_PORT: u16 = 2223;
/// Chunk size for the base64 upload: dropbear caps the exec command length.
const UPLOAD_CHUNK: usize = 3000;
/// The bridge server, embedded at build time so the library is self-contained.
pub const SERVER_SOURCE: &str = include_str!("../assets/bridge-server.cjs");

/// Digest of the embedded server source, used as its cache key and version.
#[must_use]
pub fn server_digest() -> String {
    hex::encode(Sha256::digest(SERVER_SOURCE.as_bytes()))
}

/// Options of one bridge operation.
#[derive(Debug, Clone)]
pub struct BridgeOptions {
    /// Loopback port of the tunnel that reaches the dev space's sshd.
    pub port: u16,
    /// Dev-space private key (PEM).
    pub key: String,
    pub username: String,
    /// Port the bridge listens on inside the dev space.
    pub remote_port: u16,
    /// Bridge directory inside the dev space (`~`-relative when relative).
    pub dir: String,
    /// Whether a one-time `npm install ssh2` is allowed.
    pub install: bool,
    /// Ceiling for the first-time setup (upload + install).
    pub timeout: Duration,
}

impl Default for BridgeOptions {
    fn default() -> Self {
        Self {
            port: 0,
            key: String::new(),
            username: "user".to_string(),
            remote_port: DEFAULT_BRIDGE_PORT,
            dir: DEFAULT_BRIDGE_DIR.to_string(),
            install: true,
            timeout: Duration::from_secs(300),
        }
    }
}

/// Bring the bridge up (or confirm it is up) inside a dev space.
pub async fn ensure(options: &BridgeOptions) -> Result<BridgeInfo> {
    let session = SshClient::connect(
        options.port,
        &options.key,
        &options.username,
        Duration::from_secs(30),
    )
    .await?;
    let result = ensure_with(&session, options).await;
    session.close().await;
    result
}

async fn ensure_with(session: &SshClient, options: &BridgeOptions) -> Result<BridgeInfo> {
    let base = resolve_dir(session, &options.dir).await?;
    expect_ok(
        "mkdir bridge dir",
        session
            .exec(&format!("mkdir -p {}", q(&base)), Duration::from_secs(30))
            .await?,
    )?;

    let digest = server_digest();
    let remote = session
        .exec(
            &format!(
                "sha256sum {} 2>/dev/null | cut -d' ' -f1",
                q(&format!("{base}/server.js"))
            ),
            Duration::from_secs(30),
        )
        .await?;
    if remote.stdout.trim() != digest {
        upload_text(session, &base, "server.js", SERVER_SOURCE).await?;
    }

    let installed = ensure_dependencies(session, &base, options).await?;

    let host_key = format!("{base}/host_key");
    let present = session
        .exec(
            &format!("test -f {} && echo yes || echo no", q(&host_key)),
            Duration::from_secs(30),
        )
        .await?;
    if present.stdout.trim() != "yes" {
        // An ed25519 host key generated inside the dev space would need a
        // crypto dependency there; dropbear's own `ssh-keygen` is not
        // guaranteed either, so reuse the key material we already have and
        // derive a stable host key from it instead.
        let derived = derive_host_key(&options.key)?;
        upload_text(session, &base, "host_key", &derived).await?;
        expect_ok(
            "chmod host key",
            session
                .exec(
                    &format!("chmod 600 {}", q(&host_key)),
                    Duration::from_secs(30),
                )
                .await?,
        )?;
    }

    let public_blob = public_blob(&options.key)?;
    let replaced = stop_daemon(session, &base, options.remote_port).await?;

    let script = format!("{base}/server.js");
    let pid_file = format!("{base}/bridge.pid");
    let log_file = format!("{base}/bridge.log");
    let start = format!(
        "cd {dir} && setsid nohup node {script} --port {port} --host-key {host_key} \
         --pid-file {pid_file} --authorized-key {blob} >> {log} 2>&1 < /dev/null & \
         sleep 1; cat {pid_file} 2>/dev/null || true",
        dir = q(&base),
        script = q(&script),
        port = options.remote_port,
        host_key = q(&host_key),
        pid_file = q(&pid_file),
        blob = q(&public_blob),
        log = q(&log_file),
    );
    expect_ok(
        "start bridge",
        session.exec(&start, Duration::from_secs(60)).await?,
    )?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut listening = false;
    while !listening && tokio::time::Instant::now() < deadline {
        let probe = session
            .exec(
                &format!(
                    "node -e \"const n=require('net');const s=n.connect({port},'127.0.0.1');\
                 s.on('connect',()=>{{console.log('UP');process.exit(0)}});\
                 s.on('error',()=>{{console.log('DOWN');process.exit(0)}})\" 2>/dev/null | tail -1",
                    port = options.remote_port
                ),
                Duration::from_secs(20),
            )
            .await?;
        listening = probe.stdout.trim() == "UP";
        if !listening {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    if !listening {
        let tail = session
            .exec(
                &format!("tail -8 {} 2>/dev/null", q(&log_file)),
                Duration::from_secs(20),
            )
            .await?
            .stdout;
        let tail = tail.trim();
        return Err(Error::new(
            Code::Upstream,
            format!(
                "the SFTP bridge did not start listening on {} inside the dev space{}",
                options.remote_port,
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(":\n{tail}")
                }
            ),
        ));
    }

    let pid = session
        .exec(
            &format!("cat {} 2>/dev/null", q(&pid_file)),
            Duration::from_secs(20),
        )
        .await?
        .stdout
        .trim()
        .parse::<u32>()
        .unwrap_or(0);
    let _ = replaced;
    Ok(BridgeInfo {
        dir: base,
        remote_port: options.remote_port,
        pid,
        installed,
        version: digest[..12].to_string(),
    })
}

/// Report the state of a bridge without changing anything.
pub async fn state(options: &BridgeOptions) -> Result<BridgeState> {
    let session = SshClient::connect(
        options.port,
        &options.key,
        &options.username,
        Duration::from_secs(15),
    )
    .await?;
    let result = state_with(&session, options).await;
    session.close().await;
    result
}

async fn state_with(session: &SshClient, options: &BridgeOptions) -> Result<BridgeState> {
    let base = resolve_dir(session, &options.dir).await?;
    let probe = format!(
        "test -d {modules} && echo installed || echo missing; \
         if [ -f {pid} ] && kill -0 \"$(cat {pid})\" 2>/dev/null; then echo running; cat {pid}; else echo stopped; fi; \
         (ss -ltn 2>/dev/null | grep -c \":{port} \" || true)",
        modules = q(&format!("{base}/node_modules/ssh2")),
        pid = q(&format!("{base}/bridge.pid")),
        port = options.remote_port,
    );
    let output = session.exec(&probe, Duration::from_secs(30)).await?;
    let lines: Vec<&str> = output.stdout.trim().split('\n').collect();
    let installed = lines.first().is_some_and(|line| *line == "installed");
    let running = lines.get(1).is_some_and(|line| *line == "running");
    let pid = lines
        .get(2)
        .and_then(|line| line.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let listening = lines
        .get(3)
        .and_then(|line| line.trim().parse::<u32>().ok())
        .unwrap_or(0)
        > 0;
    let log_tail = session
        .exec(
            &format!("tail -5 {} 2>/dev/null", q(&format!("{base}/bridge.log"))),
            Duration::from_secs(20),
        )
        .await?
        .stdout
        .trim()
        .to_string();
    Ok(BridgeState {
        dir: base,
        installed,
        running,
        pid,
        listening,
        log_tail,
    })
}

/// Stop the bridge daemon of a dev space.
pub async fn stop(options: &BridgeOptions) -> Result<bool> {
    let session = SshClient::connect(
        options.port,
        &options.key,
        &options.username,
        Duration::from_secs(15),
    )
    .await?;
    let result = async {
        let base = resolve_dir(&session, &options.dir).await?;
        stop_daemon(&session, &base, options.remote_port).await
    }
    .await;
    session.close().await;
    result
}

async fn resolve_dir(session: &SshClient, dir: &str) -> Result<String> {
    if dir.starts_with('/') {
        return Ok(dir.to_string());
    }
    let home = session
        .exec("printf %s \"$HOME\"", Duration::from_secs(20))
        .await?
        .stdout
        .trim()
        .to_string();
    let home = if home.is_empty() {
        "/home/user".to_string()
    } else {
        home
    };
    Ok(format!("{home}/{dir}"))
}

async fn ensure_dependencies(
    session: &SshClient,
    base: &str,
    options: &BridgeOptions,
) -> Result<bool> {
    let present = session
        .exec(
            &format!(
                "test -d {} && echo yes || echo no",
                q(&format!("{base}/node_modules/ssh2"))
            ),
            Duration::from_secs(30),
        )
        .await?;
    if present.stdout.trim() == "yes" {
        return Ok(false);
    }
    if !options.install {
        return Err(Error::new(
            Code::Invalid,
            "the dev space has no ssh2 for the SFTP bridge and installation is disabled (sftpBridgeInstall: false)",
        ));
    }
    let npm = session
        .exec(
            &format!(
                "cd {} && (command -v npm || echo npm) && npm install --no-audit --no-fund --omit=optional --no-save ssh2@1 2>&1 | tail -5",
                q(base)
            ),
            options.timeout,
        )
        .await?;
    let ok = session
        .exec(
            &format!(
                "test -d {} && echo yes || echo no",
                q(&format!("{base}/node_modules/ssh2"))
            ),
            Duration::from_secs(30),
        )
        .await?;
    if ok.stdout.trim() != "yes" {
        let detail = if npm.stdout.trim().is_empty() {
            npm.stderr
        } else {
            npm.stdout
        };
        return Err(Error::new(
            Code::Upstream,
            format!(
                "npm install ssh2 failed in the dev space: {}",
                detail.trim().chars().take(300).collect::<String>()
            ),
        ));
    }
    Ok(true)
}

/// Upload a text file in base64 chunks, then verify its digest remotely.
async fn upload_text(session: &SshClient, base: &str, name: &str, text: &str) -> Result<()> {
    use base64::Engine;
    let target = format!("{base}/{name}");
    let staging = format!("{target}.upload");
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    expect_ok(
        "truncate staging",
        session
            .exec(&format!(": > {}", q(&staging)), Duration::from_secs(30))
            .await?,
    )?;
    for chunk in encoded.as_bytes().chunks(UPLOAD_CHUNK) {
        let chunk = std::str::from_utf8(chunk).unwrap_or_default();
        expect_ok(
            "append chunk",
            session
                .exec(
                    &format!("printf %s {} >> {}", q(chunk), q(&staging)),
                    Duration::from_secs(30),
                )
                .await?,
        )?;
    }
    let digest = hex::encode(Sha256::digest(text.as_bytes()));
    let command = format!(
        "base64 -d {staging} > {target}.new && mv {target}.new {target} && rm -f {staging} && sha256sum {target} | cut -d' ' -f1",
        staging = q(&staging),
        target = q(&target),
    );
    let output = expect_ok(
        "decode upload",
        session.exec(&command, Duration::from_secs(60)).await?,
    )?;
    let remote = output.stdout.trim();
    if remote != digest {
        return Err(Error::new(
            Code::Transport,
            format!("bridge upload corrupted: {remote} != {digest}"),
        ));
    }
    Ok(())
}

/// Ask any daemon of this bridge directory to stop, then wait for the port.
async fn stop_daemon(session: &SshClient, base: &str, remote_port: u16) -> Result<bool> {
    let listening =
        || async {
            let probe = session
            .exec(&format!(
                "node -e \"const n=require('net');const s=n.connect({remote_port},'127.0.0.1');\
                 s.on('connect',()=>{{console.log('UP');process.exit(0)}});\
                 s.on('error',()=>{{console.log('DOWN');process.exit(0)}})\" 2>/dev/null | tail -1"
            ), Duration::from_secs(20))
            .await;
            matches!(probe, Ok(output) if output.stdout.trim() == "UP")
        };
    // Recorded before the kill so a caller can tell "stopped a live daemon" from
    // "nothing was there"; the return value is about the port, not this flag.
    let was_running = listening().await;
    let pattern = daemon_pattern(base);
    let command = format!(
        "if [ -f {pid} ]; then kill \"$(cat {pid})\" 2>/dev/null; fi; \
         pkill -f {pattern} 2>/dev/null; rm -f {pid}; \
         owner=$(ss -ltnp 2>/dev/null | grep \":{remote_port} \" | grep -o \"pid=[0-9]*\" | head -1 | cut -d= -f2); \
         if [ -n \"$owner\" ]; then kill \"$owner\" 2>/dev/null; fi; true",
        pid = q(&format!("{base}/bridge.pid")),
        pattern = q(&pattern),
    );
    session.exec(&command, Duration::from_secs(30)).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut still_listening = listening().await;
    while still_listening && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(300)).await;
        still_listening = listening().await;
    }
    if still_listening {
        return Err(Error::new(
            Code::Upstream,
            format!(
                "the SFTP bridge on dev-space port {remote_port} is still listening after 15s; \
                 it may belong to another process"
            ),
        ));
    }
    // The caller asked for "not running", and that is now true either way:
    // `was_running` is only interesting for logging, so it does not decide the
    // answer. Reporting `false` because a daemon had already died would read
    // like a failed stop.
    let _ = was_running;
    Ok(true)
}

/// Pattern matching a previous daemon of one bridge directory.
///
/// It matches the absolute script path the daemon was started with, which is
/// the only reliable handle: `$!` after `setsid` is the wrapper, not node.
fn daemon_pattern(base: &str) -> String {
    format!("{base}/server.js")
}

/// The public-key blob of a private key, in OpenSSH wire format (base64).
fn public_blob(key_pem: &str) -> Result<String> {
    use base64::Engine;
    let key = russh::keys::PrivateKey::from_openssh(key_pem.trim()).map_err(|error| {
        Error::invalid(format!("cannot read the dev-space private key: {error}"))
    })?;
    let blob = key
        .public_key()
        .to_bytes()
        .map_err(|error| Error::invalid(format!("cannot encode the public key: {error}")))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(blob))
}

/// Derive a stable ed25519 host key from the dev-space key material.
///
/// The bridge needs *a* host key; generating one inside the dev space would
/// need a crypto dependency there, and `ssh-keygen` is not guaranteed. Deriving
/// it from the key the landscape already issued keeps setup dependency-free and
/// makes the key stable across reconnects.
fn derive_host_key(key_pem: &str) -> Result<String> {
    use russh::keys::ssh_key::private::{Ed25519Keypair, Ed25519PrivateKey, KeypairData};
    let seed = Sha256::digest(format!("dsh-bas-remote bridge host key\n{key_pem}").as_bytes());
    let seed: [u8; 32] = seed.into();
    let pair = Ed25519Keypair::from(Ed25519PrivateKey::from_bytes(&seed));
    let key =
        russh::keys::PrivateKey::new(KeypairData::Ed25519(pair), "dsh-bas-remote-sftp-bridge")
            .map_err(|error| {
                Error::invalid(format!("cannot build the bridge host key: {error}"))
            })?;
    key.to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .map(|pem| pem.to_string())
        .map_err(|error| Error::invalid(format!("cannot encode the bridge host key: {error}")))
}

fn expect_ok(what: &str, output: crate::model::ExecOutput) -> Result<crate::model::ExecOutput> {
    if output.code == 0 {
        return Ok(output);
    }
    Err(Error::new(
        Code::Upstream,
        format!(
            "{what} failed (exit {}): {}",
            output.code,
            output.stderr.trim()
        ),
    ))
}

/// Quote a value for a POSIX shell.
#[must_use]
pub fn q(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_shell_values() {
        assert_eq!(q("plain"), "'plain'");
        assert_eq!(q("it's"), "'it'\\''s'");
    }

    #[test]
    fn embedded_server_is_present() {
        assert!(SERVER_SOURCE.contains("sftp"));
        assert_eq!(server_digest().len(), 64);
    }

    #[test]
    fn daemon_pattern_names_the_script() {
        assert_eq!(daemon_pattern("/home/user/x"), "/home/user/x/server.js");
    }
}
