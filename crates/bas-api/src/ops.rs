//! Operation table: one arm per entry in `docs/abi.md`.
//!
//! Each op does three things and nothing more: pull typed arguments out of the
//! JSON, call the core, and shape the result. Anything a core function cannot
//! do because it would be an effect (read a file, bind a port, keep a handle)
//! is done here.

use std::path::PathBuf;
use std::time::Duration;

use bas_core::error::{Code, Error};
use bas_core::model::TunnelConfig;
use bas_core::sshclient::SshClient;
use bas_core::{bridge, landscape, sshconfig};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::registry::{self, Tunnel};
use crate::{login, runtime};

/// Dispatch one operation.
pub fn call(op: &str, args: &Value) -> Result<Value, Error> {
    match op {
        // ── pure helpers ────────────────────────────────────────────────────
        "normalize_landscape" => normalize_landscape(args),
        "jwt_describe" => jwt_describe(args),
        "ext_login_url" => ext_login_url(args),
        "ssh_config_block" => ssh_config_block(args),
        "ssh_config_publish" => ssh_config_publish(args),
        "ssh_config_unpublish" => ssh_config_unpublish(args),
        "ssh_config_list" => ssh_config_list(args),
        "port_free" => port_free(args),

        // ── landscape REST ──────────────────────────────────────────────────
        "devspaces" => devspaces(args),
        "devspace" => devspace(args),
        "devspace_start" => devspace_start(args),
        "devspace_stop" => devspace_stop(args),
        "devspace_key" => devspace_key(args),

        // ── dev channel ─────────────────────────────────────────────────────
        "tunnel_open" => tunnel_open(args),
        "tunnel_status" => tunnel_status(args),
        "tunnel_list" => tunnel_list(args),
        "tunnel_close" => tunnel_close(args),
        "forward_add" => forward_add(args),
        "forward_remove" => forward_remove(args),

        // ── SSH inside the dev space ────────────────────────────────────────
        "ssh_exec" => ssh_exec(args),
        "ssh_stat" => ssh_stat(args),

        // ── SFTP bridge ─────────────────────────────────────────────────────
        "bridge_ensure" => bridge_ensure(args),
        "bridge_state" => bridge_state(args),
        "bridge_stop" => bridge_stop(args),

        // ── browser sign-in ─────────────────────────────────────────────────
        "login_begin" => login_begin(args),
        "login_poll" => login_poll(args),
        "login_cancel" => login_cancel(args),

        other => Err(Error::new(
            Code::Invalid,
            format!("unknown operation \"{other}\""),
        )),
    }
}

// ── argument helpers ────────────────────────────────────────────────────────

/// Deserialize the argument object into a typed struct.
fn typed<T: DeserializeOwned>(args: &Value) -> Result<T, Error> {
    serde_json::from_value(args.clone())
        .map_err(|error| Error::invalid(format!("unusable arguments: {error}")))
}

fn required_str(args: &Value, key: &str) -> Result<String, Error> {
    match args.get(key).and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => Ok(value.to_string()),
        Some(_) => Err(Error::invalid(format!("\"{key}\" is empty"))),
        None => Err(Error::invalid(format!("\"{key}\" is required"))),
    }
}

fn optional_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

fn optional_u16(args: &Value, key: &str) -> Result<Option<u16>, Error> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(number) if number <= u64::from(u16::MAX) => Ok(Some(number as u16)),
            _ => Err(Error::invalid(format!("\"{key}\" must be 0-65535"))),
        },
    }
}

fn required_u16(args: &Value, key: &str) -> Result<u16, Error> {
    optional_u16(args, key)?.ok_or_else(|| Error::invalid(format!("\"{key}\" is required")))
}

fn optional_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn optional_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

/// Milliseconds, defaulted.
fn millis(args: &Value, key: &str, default: u64) -> Duration {
    Duration::from_millis(optional_u64(args, key).unwrap_or(default))
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    runtime().block_on(future)
}

// ── pure helpers ────────────────────────────────────────────────────────────

fn normalize_landscape(args: &Value) -> Result<Value, Error> {
    let landscape = landscape::normalize(&required_str(args, "landscape")?);
    Ok(json!({
        "landscape": landscape,
        "host": landscape::host_of(&landscape),
        "label": landscape::label_of(&landscape),
        "credentialId": landscape::credential_id(&landscape),
    }))
}

fn jwt_describe(args: &Value) -> Result<Value, Error> {
    let jwt = required_str(args, "jwt")?;
    let now = args
        .get("nowMs")
        .and_then(Value::as_i64)
        .unwrap_or_else(registry::now_ms);
    let skew = args.get("skewMs").and_then(Value::as_i64).unwrap_or(60_000);
    Ok(json!(bas_core::jwt::describe(&jwt, now, skew)))
}

fn ext_login_url(args: &Value) -> Result<Value, Error> {
    let landscape = landscape::normalize(&required_str(args, "landscape")?);
    let cache_buster = optional_u64(args, "cacheBuster").unwrap_or(0) as u32;
    Ok(json!({ "url": landscape::ext_login_url(&landscape, cache_buster) }))
}

fn ssh_config_block(args: &Value) -> Result<Value, Error> {
    let section = required_str(args, "section")?;
    let identity = required_str(args, "identityFile")?;
    let port = required_u16(args, "port")?;
    let user = optional_str(args, "user").unwrap_or_else(|| "user".to_string());
    let host = optional_str(args, "hostName").unwrap_or_else(|| "127.0.0.1".to_string());
    Ok(json!({
        "block": sshconfig::block(&section, &identity, port, &user, &host, false),
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublishArgs {
    target: String,
    section: String,
    identity_file: String,
    port: u16,
    #[serde(default = "default_user")]
    user: String,
    #[serde(default)]
    header: String,
}

fn default_user() -> String {
    "user".to_string()
}

fn ssh_config_publish(args: &Value) -> Result<Value, Error> {
    let parsed: PublishArgs = typed(args)?;
    let target = PathBuf::from(&parsed.target);
    let rendered = sshconfig::block(
        &parsed.section,
        &parsed.identity_file,
        parsed.port,
        &parsed.user,
        "127.0.0.1",
        false,
    );
    // The guard is here, not in the core: it inspects the filesystem, which the
    // protocol layer must not do.
    if let Some(reason) = write_guard(&target) {
        return Ok(json!({
            "written": false,
            "section": parsed.section,
            "target": parsed.target,
            "reason": reason,
            "block": rendered,
        }));
    }
    let existed = target.exists();
    let text = if existed {
        std::fs::read_to_string(&target).map_err(|error| {
            Error::transport(format!("cannot read {}: {error}", target.display()))
        })?
    } else {
        String::new()
    };
    let (next, changed) = sshconfig::upsert_block(
        &text,
        existed,
        &sshconfig::Entry {
            section: &parsed.section,
            identity_file: &parsed.identity_file,
            port: parsed.port,
            user: &parsed.user,
            host_name: "127.0.0.1",
            header: &parsed.header,
        },
    );
    if changed {
        write_private(&target, &next)?;
    }
    Ok(json!({
        "written": true,
        "changed": changed,
        "section": parsed.section,
        "target": parsed.target,
        "reason": "",
        "block": rendered,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SectionArgs {
    target: String,
    section: String,
}

fn ssh_config_unpublish(args: &Value) -> Result<Value, Error> {
    let parsed: SectionArgs = typed(args)?;
    let target = PathBuf::from(&parsed.target);
    if let Some(reason) = write_guard(&target) {
        return Ok(json!({ "changed": false, "reason": reason }));
    }
    if !target.exists() {
        return Ok(json!({ "changed": false, "reason": "" }));
    }
    let text = std::fs::read_to_string(&target)
        .map_err(|error| Error::transport(format!("cannot read {}: {error}", target.display())))?;
    let (next, removed) = sshconfig::strip_block(&text, &parsed.section);
    if removed {
        write_private(&target, &sshconfig::collapse_blank_lines(&next))?;
    }
    Ok(json!({ "changed": removed, "reason": "" }))
}

fn ssh_config_list(args: &Value) -> Result<Value, Error> {
    let target = PathBuf::from(required_str(args, "target")?);
    if !target.exists() {
        return Ok(json!({ "sections": [] }));
    }
    let text = std::fs::read_to_string(&target)
        .map_err(|error| Error::transport(format!("cannot read {}: {error}", target.display())))?;
    Ok(json!({ "sections": sshconfig::managed_sections(&text) }))
}

/// Why a file must not be written, if it must not.
///
/// A nix or home-manager managed `~/.ssh/config` is a symlink into the store or
/// an unwritable file, and writing it would either fail or be reverted on the
/// next switch. Refusing with a reason lets the caller report the block to
/// paste instead of failing the connect.
fn write_guard(target: &std::path::Path) -> Option<String> {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Some(format!(
                "{} is a symlink (managed elsewhere)",
                target.display()
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Some(format!("{} is not a regular file", target.display()));
        }
        Ok(_) => {}
        // A missing file is writable by definition: `write_private` creates the
        // parent directory (a fresh `~/.ssh` is the normal first-run case).
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            return Some(format!(
                "{} cannot be inspected ({error})",
                target.display()
            ));
        }
    }
    None
}

/// Write a file with owner-only permissions, atomically.
fn write_private(target: &std::path::Path, text: &str) -> Result<(), Error> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::transport(format!("cannot create {}: {error}", parent.display()))
        })?;
    }
    let temp = target.with_extension("dsh-bas-remote.tmp");
    std::fs::write(&temp, text)
        .map_err(|error| Error::transport(format!("cannot write {}: {error}", temp.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&temp, target).map_err(|error| {
        Error::transport(format!("cannot replace {}: {error}", target.display()))
    })?;
    Ok(())
}

fn port_free(args: &Value) -> Result<Value, Error> {
    let port = required_u16(args, "port")?;
    let free = block_on(bas_core::forward::port_free(port));
    Ok(json!({ "free": free }))
}

// ── landscape REST ──────────────────────────────────────────────────────────

fn client() -> Result<landscape::LandscapeClient, Error> {
    landscape::LandscapeClient::new(Duration::from_secs(120))
}

fn devspaces(args: &Value) -> Result<Value, Error> {
    let where_ = landscape::normalize(&required_str(args, "landscape")?);
    let jwt = required_str(args, "jwt")?;
    let probe = optional_bool(args, "probeSsh").unwrap_or(false);
    block_on(async {
        let client = client()?;
        let mut spaces = client.list_dev_spaces(&where_, &jwt).await?;
        if probe {
            // The key endpoint is the only reliable SSH signal: the
            // `optionalExtensions` annotation produces false negatives.
            let results: Vec<bool> = futures_util::future::join_all(
                spaces
                    .iter()
                    .map(|space| client.probe_ssh(&where_, &jwt, space)),
            )
            .await;
            for (space, available) in spaces.iter_mut().zip(results) {
                space.ssh_enabled = Some(available);
            }
        }
        Ok(json!({ "devSpaces": spaces }))
    })
}

fn devspace(args: &Value) -> Result<Value, Error> {
    let where_ = landscape::normalize(&required_str(args, "landscape")?);
    let jwt = required_str(args, "jwt")?;
    let id = required_str(args, "id")?;
    block_on(async {
        let space = client()?.get_dev_space(&where_, &jwt, &id).await?;
        Ok(json!({ "devSpace": space }))
    })
}

fn devspace_key(args: &Value) -> Result<Value, Error> {
    let where_ = landscape::normalize(&required_str(args, "landscape")?);
    let jwt = required_str(args, "jwt")?;
    let id = required_str(args, "id")?;
    block_on(async {
        let key = client()?.get_key(&where_, &jwt, &id).await?;
        Ok(json!(key))
    })
}

/// Statuses a start/stop waits through.
fn is_final(status: &str, wanted: &str) -> bool {
    status == wanted || status == "ERROR"
}

fn devspace_start(args: &Value) -> Result<Value, Error> {
    let where_ = landscape::normalize(&required_str(args, "landscape")?);
    let jwt = required_str(args, "jwt")?;
    let id = required_str(args, "id")?;
    let wait = millis(args, "waitMs", 240_000);
    let max_active = optional_u64(args, "maxActive").unwrap_or(2) as usize;
    block_on(async {
        let client = client()?;
        let spaces = client.list_dev_spaces(&where_, &jwt).await?;
        let Some(space) = spaces.iter().find(|space| space.id == id).cloned() else {
            return Err(Error::new(
                Code::NotFound,
                format!(
                    "no dev space \"{id}\"; available: {}",
                    spaces
                        .iter()
                        .map(|space| space.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        };
        if space.status == bas_core::DEV_SPACE_RUNNING {
            return Ok(json!({ "devSpace": space, "started": false, "alreadyRunning": true }));
        }
        let active: Vec<&bas_core::DevSpace> = spaces
            .iter()
            .filter(|entry| {
                entry.id != space.id
                    && (entry.status == bas_core::DEV_SPACE_RUNNING || entry.status == "STARTING")
            })
            .collect();
        if space.status == bas_core::DEV_SPACE_STOPPED && active.len() >= max_active {
            return Err(Error::new(
                Code::Status,
                format!(
                    "{} already has {} running or starting dev spaces ({}); BAS allows {} at a time — stop one first",
                    landscape::host_of(&where_),
                    active.len(),
                    active
                        .iter()
                        .map(|entry| entry.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    max_active
                ),
            ));
        }
        if space.status == "STOPPING" {
            // A space on its way down finishes first; a start issued now is lost.
            wait_for(
                &client,
                &where_,
                &jwt,
                &id,
                bas_core::DEV_SPACE_STOPPED,
                wait,
            )
            .await?;
        }
        let started = space.status == bas_core::DEV_SPACE_STOPPED;
        if started {
            client
                .set_suspended(&where_, &jwt, &id, false, &space.label)
                .await?;
        }
        // `waitMs: 0` means "send the request and report what BAS says now" —
        // a UI that polls the listing itself must not be held for minutes.
        if wait.is_zero() {
            let current = client.get_dev_space(&where_, &jwt, &id).await?;
            return Ok(json!({
                "devSpace": current,
                "started": started,
                "alreadyRunning": false,
                "waited": false,
            }));
        }
        let settled = wait_for(
            &client,
            &where_,
            &jwt,
            &id,
            bas_core::DEV_SPACE_RUNNING,
            wait,
        )
        .await?;
        Ok(json!({
            "devSpace": settled,
            "started": started,
            "alreadyRunning": false,
            "waited": true,
        }))
    })
}

fn devspace_stop(args: &Value) -> Result<Value, Error> {
    let where_ = landscape::normalize(&required_str(args, "landscape")?);
    let jwt = required_str(args, "jwt")?;
    let id = required_str(args, "id")?;
    let wait = millis(args, "waitMs", 240_000);
    block_on(async {
        let client = client()?;
        let spaces = client.list_dev_spaces(&where_, &jwt).await?;
        let Some(space) = spaces.iter().find(|space| space.id == id).cloned() else {
            return Err(Error::new(Code::NotFound, format!("no dev space \"{id}\"")));
        };
        if space.status == bas_core::DEV_SPACE_STOPPED {
            return Ok(json!({ "devSpace": space, "stopped": false, "alreadyStopped": true }));
        }
        if space.status != "STOPPING" {
            client
                .set_suspended(&where_, &jwt, &id, true, &space.label)
                .await?;
        }
        if wait.is_zero() {
            let current = client.get_dev_space(&where_, &jwt, &id).await?;
            return Ok(json!({
                "devSpace": current,
                "stopped": true,
                "alreadyStopped": false,
                "waited": false,
            }));
        }
        let settled = wait_for(
            &client,
            &where_,
            &jwt,
            &id,
            bas_core::DEV_SPACE_STOPPED,
            wait,
        )
        .await?;
        Ok(json!({ "devSpace": settled, "stopped": true, "alreadyStopped": false, "waited": true }))
    })
}

/// Poll a dev space until it reaches `wanted` (or `ERROR`), up to `timeout`.
async fn wait_for(
    client: &landscape::LandscapeClient,
    where_: &str,
    jwt: &str,
    id: &str,
    wanted: &str,
    timeout: Duration,
) -> Result<bas_core::DevSpace, Error> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let space = client.get_dev_space(where_, jwt, id).await?;
        if is_final(&space.status, wanted) {
            if space.status == wanted {
                return Ok(space);
            }
            return Err(Error::new(
                Code::Status,
                format!(
                    "dev space {} is {} (wanted {wanted}); check it in BAS",
                    if space.label.is_empty() {
                        &space.id
                    } else {
                        &space.label
                    },
                    space.status
                ),
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::new(
                Code::Status,
                format!(
                    "dev space {} is still {} after {}s (wanted {wanted})",
                    if space.label.is_empty() {
                        &space.id
                    } else {
                        &space.label
                    },
                    space.status,
                    timeout.as_secs()
                ),
            ));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

// ── dev channel ─────────────────────────────────────────────────────────────

fn tunnel_open(args: &Value) -> Result<Value, Error> {
    let config = TunnelConfig {
        landscape: landscape::normalize(&required_str(args, "landscape")?),
        jwt: required_str(args, "jwt")?,
        ws_url: required_str(args, "wsUrl")?,
        local_port: optional_u16(args, "localPort")?.unwrap_or(0),
        username: optional_str(args, "username").unwrap_or_else(|| "user".to_string()),
        connect_attempts: optional_u64(args, "connectAttempts")
            .unwrap_or(3)
            .clamp(1, 5) as u8,
        connect_timeout_ms: optional_u64(args, "timeoutMs").unwrap_or(30_000),
    };
    block_on(async {
        let channel = std::sync::Arc::new(
            bas_core::devchannel::DevChannel::connect(
                &config.jwt,
                &config.ws_url,
                &config.username,
                config.connect_attempts,
                Duration::from_millis(config.connect_timeout_ms),
            )
            .await?,
        );
        let forward = bas_core::forward::Forward::open(
            std::sync::Arc::clone(&channel),
            bas_core::DEV_SPACE_SSHD_PORT,
            config.local_port,
        )
        .await?;
        let id = registry::new_handle();
        let server_uri = bas_core::devchannel::server_uri(&config.ws_url).unwrap_or_default();
        let tunnel = Tunnel {
            id: id.clone(),
            server_uri,
            local_port: forward.local_port,
            username: config.username,
            started_at: registry::now_ms(),
            channel,
            forwards: vec![forward],
            closed_reason: String::new(),
        };
        let status = tunnel.status();
        registry::insert(tunnel);
        Ok(json!({ "tunnelId": id, "status": status }))
    })
}

fn tunnel_status(args: &Value) -> Result<Value, Error> {
    let id = required_str(args, "tunnelId")?;
    registry::with(&id, |tunnel| json!({ "status": tunnel.status() }))
}

fn tunnel_list(_args: &Value) -> Result<Value, Error> {
    Ok(json!({ "tunnels": registry::list() }))
}

fn tunnel_close(args: &Value) -> Result<Value, Error> {
    let id = required_str(args, "tunnelId")?;
    let Some(tunnel) = registry::take(&id) else {
        return Ok(json!({ "closed": false }));
    };
    block_on(async move {
        for forward in &tunnel.forwards {
            forward.close();
        }
        tunnel.channel.close().await;
        Ok(json!({ "closed": true }))
    })
}

fn forward_add(args: &Value) -> Result<Value, Error> {
    let id = required_str(args, "tunnelId")?;
    let remote_port = required_u16(args, "remotePort")?;
    let local_port = optional_u16(args, "localPort")?.unwrap_or(0);
    let channel = registry::with(&id, |tunnel| std::sync::Arc::clone(&tunnel.channel))?;
    block_on(async move {
        let forward = bas_core::forward::Forward::open(channel, remote_port, local_port).await?;
        let bound = forward.local_port;
        registry::with(&id, |tunnel| {
            tunnel
                .forwards
                .retain(|existing| existing.remote_port != remote_port);
            tunnel.forwards.push(forward);
        })?;
        Ok(json!({ "localPort": bound }))
    })
}

fn forward_remove(args: &Value) -> Result<Value, Error> {
    let id = required_str(args, "tunnelId")?;
    let remote_port = required_u16(args, "remotePort")?;
    registry::with(&id, |tunnel| {
        let before = tunnel.forwards.len();
        tunnel
            .forwards
            .retain(|forward| forward.remote_port != remote_port);
        json!({ "removed": tunnel.forwards.len() != before })
    })
}

// ── SSH inside the dev space ────────────────────────────────────────────────

fn ssh_username(args: &Value) -> String {
    optional_str(args, "username").unwrap_or_else(|| "user".to_string())
}

fn ssh_exec(args: &Value) -> Result<Value, Error> {
    let port = required_u16(args, "port")?;
    let key = required_str(args, "key")?;
    let username = ssh_username(args);
    let command = required_str(args, "command")?;
    let timeout = millis(args, "timeoutMs", 120_000);
    block_on(async move {
        let session = SshClient::connect(port, &key, &username, Duration::from_secs(30)).await?;
        let output = session.exec(&command, timeout).await;
        session.close().await;
        let output = output?;
        Ok(json!(output))
    })
}

fn ssh_stat(args: &Value) -> Result<Value, Error> {
    let port = required_u16(args, "port")?;
    let key = required_str(args, "key")?;
    let username = ssh_username(args);
    let path = required_str(args, "path")?;
    block_on(async move {
        let session = SshClient::connect(port, &key, &username, Duration::from_secs(30)).await?;
        let stat = session.stat(&path).await;
        session.close().await;
        Ok(json!(stat?))
    })
}

// ── SFTP bridge ─────────────────────────────────────────────────────────────

fn bridge_options(args: &Value) -> Result<bridge::BridgeOptions, Error> {
    Ok(bridge::BridgeOptions {
        port: required_u16(args, "port")?,
        key: required_str(args, "key")?,
        username: ssh_username(args),
        remote_port: optional_u16(args, "remotePort")?.unwrap_or(bridge::DEFAULT_BRIDGE_PORT),
        dir: optional_str(args, "dir").unwrap_or_else(|| bridge::DEFAULT_BRIDGE_DIR.to_string()),
        install: optional_bool(args, "install").unwrap_or(true),
        timeout: millis(args, "timeoutMs", 300_000),
    })
}

fn bridge_ensure(args: &Value) -> Result<Value, Error> {
    let options = bridge_options(args)?;
    block_on(async move {
        let info = bridge::ensure(&options).await?;
        Ok(json!(info))
    })
}

fn bridge_state(args: &Value) -> Result<Value, Error> {
    let options = bridge_options(args)?;
    block_on(async move {
        let state = bridge::state(&options).await?;
        Ok(json!(state))
    })
}

fn bridge_stop(args: &Value) -> Result<Value, Error> {
    let options = bridge_options(args)?;
    block_on(async move {
        let stopped = bridge::stop(&options).await?;
        Ok(json!({ "stopped": stopped }))
    })
}

// ── browser sign-in ─────────────────────────────────────────────────────────

fn login_begin(args: &Value) -> Result<Value, Error> {
    let landscape = landscape::normalize(&required_str(args, "landscape")?);
    let host = optional_str(args, "host").unwrap_or_else(|| "127.0.0.1".to_string());
    let port = optional_u16(args, "port")?.unwrap_or(bas_core::DEFAULT_LOGIN_PORT);
    block_on(async move {
        let (id, url, bound_port) = login::begin(&landscape, &host, port).await?;
        Ok(json!({
            "loginId": id,
            "url": url,
            "boundPort": bound_port,
            "landscape": landscape,
            "host": host,
        }))
    })
}

fn login_poll(args: &Value) -> Result<Value, Error> {
    let id = required_str(args, "loginId")?;
    login::poll(&id)
}

fn login_cancel(args: &Value) -> Result<Value, Error> {
    let id = required_str(args, "loginId")?;
    Ok(json!({ "cancelled": login::cancel(&id)? }))
}

/// Everything a host must release on teardown.
pub fn shutdown() {
    let tunnels = registry::list();
    for status in tunnels {
        if let Some(tunnel) = registry::take(&status.id) {
            for forward in &tunnel.forwards {
                forward.close();
            }
        }
    }
    login::cancel_all();
}
