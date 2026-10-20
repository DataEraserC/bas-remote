//! `bas` — a command-line host for `bas-core`.
//!
//! The CLI is the third part of the split and the proof that the protocol does
//! not need a harness: it drives the same operations the C ABI exposes, with
//! argv and a terminal in place of JSON.
//!
//! It is deliberately thin. Anything that is policy — where a token lives, which
//! dev space is "the" one, which ports to pin — belongs to the caller, so the
//! commands take their inputs explicitly and print one JSON object with `--json`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use bas_core::model::DevSpace;
use bas_core::{bridge, landscape, sshclient, sshconfig};
use clap::{Parser, Subcommand};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "bas",
    version,
    about = "SAP Business Application Studio dev spaces from the command line",
    after_help = "Every command accepts --json for one machine-readable object."
)]
struct Cli {
    /// Print one JSON object instead of human text
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Dev spaces of a landscape
    Devspaces {
        /// Landscape URL or host
        landscape: String,
        /// Landscape JWT (or set BAS_JWT)
        #[arg(long, env = "BAS_JWT")]
        jwt: String,
        /// Probe each running dev space for its SSH key
        #[arg(long)]
        probe: bool,
    },
    /// Start a dev space and wait for it to run
    Start {
        landscape: String,
        #[arg(long, env = "BAS_JWT")]
        jwt: String,
        /// Dev-space id
        id: String,
        /// Seconds to wait for RUNNING
        #[arg(long, default_value_t = 240)]
        wait: u64,
    },
    /// Stop a dev space and wait for it to be stopped
    Stop {
        landscape: String,
        #[arg(long, env = "BAS_JWT")]
        jwt: String,
        id: String,
        #[arg(long, default_value_t = 240)]
        wait: u64,
    },
    /// Print the SSH key of a dev space
    Key {
        landscape: String,
        #[arg(long, env = "BAS_JWT")]
        jwt: String,
        id: String,
    },
    /// Open a dev-channel tunnel and keep it up until interrupted
    Tunnel {
        /// Dev-space runtime base URL
        ws_url: String,
        #[arg(long, env = "BAS_JWT")]
        jwt: String,
        /// Loopback port (0 picks a free one)
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// SSH user presented to the dev channel
        #[arg(long, default_value_t = String::from("user"))]
        user: String,
        /// Forward this dev-space port too (repeatable)
        #[arg(long = "forward")]
        forwards: Vec<u16>,
    },
    /// Run a command inside a connected dev space
    Exec {
        /// Loopback port of a tunnel into the dev space
        #[arg(long)]
        port: u16,
        /// Private key file of the dev space
        #[arg(long)]
        key: PathBuf,
        #[arg(long, default_value_t = String::from("user"))]
        user: String,
        /// Command to run
        command: String,
    },
    /// Manage the SFTP bridge inside a dev space
    Bridge {
        #[command(subcommand)]
        action: BridgeAction,
    },
    /// Render or publish an SSH config entry
    SshConfig {
        #[command(subcommand)]
        action: SshConfigAction,
    },
    /// Describe a landscape JWT
    Jwt {
        #[arg(env = "BAS_JWT")]
        jwt: String,
    },
}

#[derive(Subcommand)]
enum BridgeAction {
    /// Install and start the bridge (needs a tunnel into the dev space)
    Ensure {
        #[arg(long)]
        port: u16,
        #[arg(long)]
        key: PathBuf,
        #[arg(long, default_value_t = String::from("user"))]
        user: String,
        #[arg(long, default_value_t = bridge::DEFAULT_BRIDGE_PORT)]
        remote_port: u16,
    },
    /// Report the bridge state
    Status {
        #[arg(long)]
        port: u16,
        #[arg(long)]
        key: PathBuf,
        #[arg(long, default_value_t = String::from("user"))]
        user: String,
        #[arg(long, default_value_t = bridge::DEFAULT_BRIDGE_PORT)]
        remote_port: u16,
    },
    /// Stop the bridge
    Stop {
        #[arg(long)]
        port: u16,
        #[arg(long)]
        key: PathBuf,
        #[arg(long, default_value_t = String::from("user"))]
        user: String,
        #[arg(long, default_value_t = bridge::DEFAULT_BRIDGE_PORT)]
        remote_port: u16,
    },
}

#[derive(Subcommand)]
enum SshConfigAction {
    /// Print the Host block for a dev space
    Block {
        #[arg(long)]
        section: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        port: u16,
        #[arg(long, default_value_t = String::from("user"))]
        user: String,
    },
    /// Write the block into a config file
    Publish {
        #[arg(long)]
        target: PathBuf,
        #[arg(long)]
        section: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        port: u16,
        #[arg(long, default_value_t = String::from("user"))]
        user: String,
    },
    /// Remove the block from a config file
    Unpublish {
        #[arg(long)]
        target: PathBuf,
        #[arg(long)]
        section: String,
    },
    /// List the blocks this tool manages
    List {
        #[arg(long)]
        target: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the runtime")?;
    let value = runtime.block_on(run(cli.command))?;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print_human(&value);
    }
    Ok(())
}

async fn run(command: Command) -> Result<serde_json::Value> {
    match command {
        Command::Devspaces {
            landscape,
            jwt,
            probe,
        } => {
            let where_ = landscape::normalize(&landscape);
            let client = landscape::LandscapeClient::new(std::time::Duration::from_secs(120))?;
            let mut spaces = client.list_dev_spaces(&where_, &jwt).await?;
            if probe {
                for space in spaces.iter_mut() {
                    space.ssh_enabled = Some(client.probe_ssh(&where_, &jwt, space).await);
                }
            }
            Ok(json!({ "landscape": where_, "devSpaces": spaces }))
        }
        Command::Start {
            landscape,
            jwt,
            id,
            wait,
        } => {
            let where_ = landscape::normalize(&landscape);
            let client = landscape::LandscapeClient::new(std::time::Duration::from_secs(120))?;
            let spaces = client.list_dev_spaces(&where_, &jwt).await?;
            let space = find(&spaces, &id)?;
            if space.status != bas_core::DEV_SPACE_RUNNING {
                client
                    .set_suspended(&where_, &jwt, &id, false, &space.label)
                    .await?;
                let settled = wait_for(
                    &client,
                    &where_,
                    &jwt,
                    &id,
                    bas_core::DEV_SPACE_RUNNING,
                    wait,
                )
                .await?;
                return Ok(json!({ "devSpace": settled, "started": true }));
            }
            Ok(json!({ "devSpace": space, "started": false }))
        }
        Command::Stop {
            landscape,
            jwt,
            id,
            wait,
        } => {
            let where_ = landscape::normalize(&landscape);
            let client = landscape::LandscapeClient::new(std::time::Duration::from_secs(120))?;
            let spaces = client.list_dev_spaces(&where_, &jwt).await?;
            let space = find(&spaces, &id)?;
            if space.status != bas_core::DEV_SPACE_STOPPED {
                client
                    .set_suspended(&where_, &jwt, &id, true, &space.label)
                    .await?;
                let settled = wait_for(
                    &client,
                    &where_,
                    &jwt,
                    &id,
                    bas_core::DEV_SPACE_STOPPED,
                    wait,
                )
                .await?;
                return Ok(json!({ "devSpace": settled, "stopped": true }));
            }
            Ok(json!({ "devSpace": space, "stopped": false }))
        }
        Command::Key { landscape, jwt, id } => {
            let where_ = landscape::normalize(&landscape);
            let client = landscape::LandscapeClient::new(std::time::Duration::from_secs(120))?;
            let key = client.get_key(&where_, &jwt, &id).await?;
            Ok(json!(key))
        }
        Command::Tunnel {
            ws_url,
            jwt,
            port,
            user,
            forwards,
        } => {
            let channel = std::sync::Arc::new(
                bas_core::devchannel::DevChannel::connect(
                    &jwt,
                    &ws_url,
                    &user,
                    3,
                    std::time::Duration::from_secs(30),
                )
                .await?,
            );
            let forward = bas_core::forward::Forward::open(
                std::sync::Arc::clone(&channel),
                bas_core::DEV_SPACE_SSHD_PORT,
                port,
            )
            .await?;
            let mut extra = Vec::new();
            for remote in forwards {
                extra.push(
                    bas_core::forward::Forward::open(std::sync::Arc::clone(&channel), remote, 0)
                        .await?,
                );
            }
            println!(
                "tunnel ready: 127.0.0.1:{} -> dev space 127.0.0.1:{}",
                forward.local_port,
                bas_core::DEV_SPACE_SSHD_PORT
            );
            for entry in &extra {
                println!(
                    "forward:      127.0.0.1:{} -> dev space 127.0.0.1:{}",
                    entry.local_port, entry.remote_port
                );
            }
            println!("press Ctrl-C to close");
            let _ = tokio::signal::ctrl_c().await;
            for entry in &extra {
                entry.close();
            }
            forward.close();
            channel.close().await;
            Ok(json!({ "closed": true }))
        }
        Command::Exec {
            port,
            key,
            user,
            command,
        } => {
            let pem = std::fs::read_to_string(&key)
                .with_context(|| format!("cannot read {}", key.display()))?;
            let session = sshclient::SshClient::connect(
                port,
                &pem,
                &user,
                std::time::Duration::from_secs(30),
            )
            .await?;
            let output = session
                .exec(&command, std::time::Duration::from_secs(600))
                .await;
            session.close().await;
            let output = output?;
            if output.code != 0 {
                anyhow::bail!(
                    "remote command exited {}: {}",
                    output.code,
                    output.stderr.trim()
                );
            }
            Ok(json!({
                "code": output.code,
                "stdout": output.stdout,
                "stderr": output.stderr,
            }))
        }
        Command::Bridge { action } => {
            let (action, port, key, user, remote_port) = match action {
                BridgeAction::Ensure {
                    port,
                    key,
                    user,
                    remote_port,
                } => ("ensure", port, key, user, remote_port),
                BridgeAction::Status {
                    port,
                    key,
                    user,
                    remote_port,
                } => ("status", port, key, user, remote_port),
                BridgeAction::Stop {
                    port,
                    key,
                    user,
                    remote_port,
                } => ("stop", port, key, user, remote_port),
            };
            let pem = std::fs::read_to_string(&key)
                .with_context(|| format!("cannot read {}", key.display()))?;
            let options = bridge::BridgeOptions {
                port,
                key: pem,
                username: user,
                remote_port,
                ..bridge::BridgeOptions::default()
            };
            Ok(match action {
                "ensure" => json!(bridge::ensure(&options).await?),
                "status" => json!(bridge::state(&options).await?),
                _ => json!({ "stopped": bridge::stop(&options).await? }),
            })
        }
        Command::SshConfig { action } => Ok(match action {
            SshConfigAction::Block {
                section,
                identity,
                port,
                user,
            } => json!({
                "block": sshconfig::block(
                    &section,
                    &identity.display().to_string(),
                    port,
                    &user,
                    "127.0.0.1",
                    false,
                ),
            }),
            SshConfigAction::Publish {
                target,
                section,
                identity,
                port,
                user,
            } => {
                let text = std::fs::read_to_string(&target).unwrap_or_default();
                let existed = target.exists();
                let (next, changed) = sshconfig::upsert_block(
                    &text,
                    existed,
                    &sshconfig::Entry {
                        section: &section,
                        identity_file: &identity.display().to_string(),
                        port,
                        user: &user,
                        host_name: "127.0.0.1",
                        header: "",
                    },
                );
                if changed {
                    std::fs::write(&target, &next)
                        .with_context(|| format!("cannot write {}", target.display()))?;
                }
                json!({ "changed": changed, "target": target.display().to_string() })
            }
            SshConfigAction::Unpublish { target, section } => {
                let text = std::fs::read_to_string(&target).unwrap_or_default();
                let (next, removed) = sshconfig::strip_block(&text, &section);
                if removed {
                    std::fs::write(&target, sshconfig::collapse_blank_lines(&next))
                        .with_context(|| format!("cannot write {}", target.display()))?;
                }
                json!({ "changed": removed })
            }
            SshConfigAction::List { target } => {
                let text = std::fs::read_to_string(&target).unwrap_or_default();
                json!({ "sections": sshconfig::managed_sections(&text) })
            }
        }),
        Command::Jwt { jwt } => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or(0);
            Ok(json!(bas_core::jwt::describe(&jwt, now, 60_000)))
        }
    }
}

fn find<'a>(spaces: &'a [DevSpace], id: &str) -> Result<&'a DevSpace> {
    spaces
        .iter()
        .find(|space| space.id == id || space.label.eq_ignore_ascii_case(id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no dev space \"{id}\"; available: {}",
                spaces
                    .iter()
                    .map(|space| format!("{} [{}]", space.label, space.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

async fn wait_for(
    client: &landscape::LandscapeClient,
    where_: &str,
    jwt: &str,
    id: &str,
    wanted: &str,
    seconds: u64,
) -> Result<DevSpace> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        let space = client.get_dev_space(where_, jwt, id).await?;
        if space.status == wanted {
            return Ok(space);
        }
        if space.status == "ERROR" {
            anyhow::bail!("dev space {id} is ERROR; check it in BAS");
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("dev space {id} is still {} after {seconds}s", space.status);
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

/// Print a JSON value as readable text.
fn print_human(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, entry) in map {
                match entry {
                    serde_json::Value::Array(items) => {
                        println!("{key}:");
                        for item in items {
                            println!("  {}", one_line(item));
                        }
                    }
                    other => println!("{key}: {}", one_line(other)),
                }
            }
        }
        other => println!("{}", one_line(other)),
    }
}

fn one_line(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, entry)| format!("{key}={}", short(entry)))
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    }
}

fn short(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}
