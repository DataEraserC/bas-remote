//! Landscape REST client: the four calls the SAP `app-studio-toolkit` makes
//! through `@sap/bas-sdk`, re-implemented against the wire directly.
//!
//! | Call | Source in the SAP toolkit |
//! |---|---|
//! | `GET <landscape>/ws-manager/api/v1/workspace` | `get-devspace.ts` |
//! | `GET <landscape>/ws-manager/api/v1/workspace/<id>` | `get-devspace.ts` |
//! | `PUT <landscape>/ws-manager/api/v1/workspace/<id>` | `devspace/update.ts` |
//! | `GET <startup url>/key` | `devspace-utils.ts` |
//!
//! Stateless by construction: the client owns no token and no cache, every
//! call takes the JWT it should present.

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde_json::json;

use crate::error::{Code, Error, Result};
use crate::model::{DevSpace, KeyMaterial};
use crate::{DEV_SPACE_RUNNING, WS_MANAGER_PATH};

/// User agent BAS expects; the landscape rejects unknown ones on some routes.
pub const USER_AGENT: &str = "Bas";

/// One landscape client. Cheap to clone (shared connection pool).
#[derive(Clone, Debug)]
pub struct LandscapeClient {
    http: Client,
}

impl LandscapeClient {
    /// Build a client with an explicit timeout.
    pub fn new(timeout: Duration) -> Result<Self> {
        let http = Client::builder()
            .timeout(timeout)
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self { http })
    }

    /// List the dev spaces of a landscape.
    pub async fn list_dev_spaces(&self, landscape: &str, jwt: &str) -> Result<Vec<DevSpace>> {
        let url = format!("{}{WS_MANAGER_PATH}/workspace", normalize(landscape));
        let body = self.get_json(&url, jwt).await?;
        let entries = match body {
            serde_json::Value::Array(items) => items,
            serde_json::Value::Object(mut map) => match map.remove("workspaces") {
                Some(serde_json::Value::Array(items)) => items,
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        Ok(entries.iter().map(flatten_dev_space).collect())
    }

    /// Read one dev space record, flattened.
    pub async fn get_dev_space(&self, landscape: &str, jwt: &str, ws_id: &str) -> Result<DevSpace> {
        let url = format!(
            "{}{WS_MANAGER_PATH}/workspace/{}",
            normalize(landscape),
            encode(ws_id)
        );
        let body = self.get_json(&url, jwt).await?;
        Ok(flatten_dev_space(&body))
    }

    /// Start (`suspended = false`) or stop (`suspended = true`) one dev space.
    pub async fn set_suspended(
        &self,
        landscape: &str,
        jwt: &str,
        ws_id: &str,
        suspended: bool,
        display_name: &str,
    ) -> Result<()> {
        let url = format!(
            "{}{WS_MANAGER_PATH}/workspace/{}",
            normalize(landscape),
            encode(ws_id)
        );
        let response = self
            .http
            .put(&url)
            .headers(headers(jwt)?)
            .json(&json!({
                "Suspended": suspended,
                "WorkspaceDisplayName": display_name,
            }))
            .send()
            .await?;
        expect_success(response, "update dev space").await?;
        Ok(())
    }

    /// Fetch the SSH key a running dev space serves from its runtime.
    pub async fn get_key(&self, landscape: &str, jwt: &str, ws_id: &str) -> Result<KeyMaterial> {
        let space = self.get_dev_space(landscape, jwt, ws_id).await?;
        let url = key_url(&space)?;
        let response = self.http.get(&url).headers(headers(jwt)?).send().await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(Error::upstream(format!(
                "{} responded HTTP {}: {}",
                url,
                status.as_u16(),
                summarize(&text)
            )));
        }
        if !text.contains("PRIVATE KEY") {
            return Err(Error::new(
                Code::BadBody,
                format!("{} did not return a private key", url),
            ));
        }
        Ok(KeyMaterial {
            key: text,
            ws_url: space.url.clone(),
            startup_url: space.startup_url.clone(),
        })
    }

    /// Probe whether a running dev space serves its key right now.
    ///
    /// The `optionalExtensions` annotation is not a reliable signal: a dev
    /// space with no remote-access entry still answers `/key` while RUNNING.
    pub async fn probe_ssh(&self, _landscape: &str, jwt: &str, space: &DevSpace) -> bool {
        if space.status != DEV_SPACE_RUNNING {
            return false;
        }
        let Ok(url) = key_url(space) else {
            return false;
        };
        let Ok(Ok(response)) = tokio::time::timeout(
            Duration::from_secs(20),
            self.http
                .get(&url)
                .headers(match headers(jwt) {
                    Ok(h) => h,
                    Err(_) => return false,
                })
                .send(),
        )
        .await
        else {
            return false;
        };
        if !response.status().is_success() {
            return false;
        }
        match response.text().await {
            Ok(body) => body.contains("PRIVATE KEY"),
            Err(_) => false,
        }
    }

    async fn get_json(&self, url: &str, jwt: &str) -> Result<serde_json::Value> {
        let response = self.http.get(url).headers(headers(jwt)?).send().await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(http_error(url, status, &text));
        }
        serde_json::from_str(&text).map_err(|error| {
            Error::new(
                Code::BadBody,
                format!("{url} did not return JSON: {}", summarize(&text)),
            )
            .with_source(error)
        })
    }
}

/// Flatten one `ws-manager` workspace record.
///
/// `sshEnabled` stays `None`: SSH availability is probed, never inferred from
/// the record, because the annotation and reality disagree.
#[must_use]
pub fn flatten_dev_space(entry: &serde_json::Value) -> DevSpace {
    let config = entry.get("config");
    let labels = config.and_then(|c| c.get("labels"));
    let annotations = config.and_then(|c| c.get("annotations"));
    let runtime = entry.get("runtime");
    let string = |value: Option<&serde_json::Value>| {
        value
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    DevSpace {
        id: string(config.and_then(|c| c.get("id"))),
        label: string(labels.and_then(|l| l.get("ws-manager.devx.sap.com/displayname"))),
        status: {
            let status = string(runtime.and_then(|r| r.get("status")));
            if status.is_empty() {
                "UNKNOWN".to_string()
            } else {
                status
            }
        },
        url: string(runtime.and_then(|r| r.get("baseUrl"))),
        startup_url: string(
            runtime
                .and_then(|r| r.get("url"))
                .and_then(|u| u.get("startup")),
        ),
        ssh_enabled: None,
        pack: string(annotations.and_then(|a| a.get("pack"))),
        pack_display_name: string(annotations.and_then(|a| a.get("packTagline"))),
        origin: {
            let origin = string(entry.get("devSpaceOriginLabel"));
            if origin.is_empty() {
                string(labels.and_then(|l| l.get("ws-manager.devx.sap.com/LCAP")))
            } else {
                origin
            }
        },
    }
}

/// The `/key` URL of a dev space, derived from its startup URL.
pub fn key_url(space: &DevSpace) -> Result<String> {
    if space.startup_url.is_empty() {
        return Err(Error::new(
            Code::Status,
            format!(
                "dev space {} is {} and has no startup URL; start it first",
                if space.id.is_empty() {
                    "(unknown)"
                } else {
                    &space.id
                },
                space.status
            ),
        ));
    }
    let mut url = url::Url::parse(&space.startup_url)?;
    url.set_path("key");
    url.set_query(None);
    Ok(url.to_string())
}

/// Accept `host`, `https://host`, `https://host/` or a full BAS view URL and
/// return `https://host` without a trailing slash.
pub fn normalize(input: &str) -> String {
    let raw = input.trim();
    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    match url::Url::parse(&candidate) {
        Ok(mut url) => {
            url.set_path("/");
            url.set_query(None);
            url.set_fragment(None);
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.to_string().trim_end_matches('/').to_string()
        }
        Err(_) => raw.trim_end_matches('/').to_string(),
    }
}

/// The `host[:port]` of a landscape.
#[must_use]
pub fn host_of(landscape: &str) -> String {
    url::Url::parse(&normalize(landscape))
        .ok()
        .and_then(|url| {
            url.host_str().map(|host| match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_string(),
            })
        })
        .unwrap_or_default()
}

/// The short display name of a landscape (`eu10` from `x.eu10...`).
#[must_use]
pub fn label_of(landscape: &str) -> String {
    url::Url::parse(&normalize(landscape))
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .and_then(|host| host.split('.').next().map(str::to_string))
        .unwrap_or_default()
}

/// A credential-store-safe id for one landscape.
///
/// Credential keys are `<scope>/<id>` and both halves must match
/// `^[a-z][a-z0-9-]*$`, which a BAS host never does: hosts are dotted and
/// often start with a digit. The host is slugged into that alphabet and
/// suffixed with a short digest, so distinct hosts stay distinct even when
/// they slug alike (`a.b` vs `a-b`) and the id is stable across runs.
#[must_use]
pub fn credential_id(landscape: &str) -> String {
    use sha2::{Digest, Sha256};
    let host = host_of(landscape).to_lowercase();
    let slug: String = host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(32)
        .collect();
    let slug = slug.trim_end_matches('-').to_string();
    let digest = hex::encode(Sha256::digest(host.as_bytes()));
    let digest = &digest[..8];
    let base = if slug.is_empty() {
        digest.to_string()
    } else {
        format!("{slug}-{digest}")
    };
    if base.starts_with(|c: char| c.is_ascii_lowercase()) {
        base
    } else {
        format!("ls-{base}")
    }
}

/// The external-login URL the browser is sent to (`cb` is a cache buster).
#[must_use]
pub fn ext_login_url(landscape: &str, cache_buster: u32) -> String {
    let base = normalize(landscape);
    format!("{base}/ext-login.html?cb={cache_buster}")
}

fn headers(jwt: &str) -> Result<reqwest::header::HeaderMap> {
    use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
    let mut map = HeaderMap::new();
    let value = HeaderValue::from_str(&format!("bearer {jwt}"))
        .map_err(|error| Error::invalid(format!("unusable token: {error}")))?;
    map.insert(AUTHORIZATION, value.clone());
    map.insert("x-approuter-authorization", value);
    map.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/plain, */*"),
    );
    Ok(map)
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn summarize(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(400).collect()
}

fn http_error(url: &str, status: StatusCode, body: &str) -> Error {
    let message = format!(
        "{url} responded HTTP {}: {}",
        status.as_u16(),
        summarize(body)
    );
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        Error::new(Code::TokenExpired, message)
    } else {
        Error::new(Code::Upstream, message)
    }
}

async fn expect_success(response: reqwest::Response, what: &str) -> Result<()> {
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(Error::upstream(format!(
        "{what} failed (HTTP {}): {}",
        status.as_u16(),
        summarize(&body)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_landscape_forms() {
        assert_eq!(normalize("host.example"), "https://host.example");
        assert_eq!(normalize("https://host.example/"), "https://host.example");
        assert_eq!(
            normalize("https://host.example/deep/path?x=1"),
            "https://host.example"
        );
        assert_eq!(
            normalize("http://host.example"),
            "http://host.example",
            "the scheme is preserved; callers upgrade explicitly"
        );
    }

    #[test]
    fn credential_ids_are_valid_and_stable() {
        let id = credential_id("https://3fb23b87trial.ap21cf.trial.applicationstudio.cloud.sap");
        assert!(id.starts_with(|c: char| c.is_ascii_lowercase()));
        assert!(
            id.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
        assert_eq!(
            id,
            credential_id("3fb23b87trial.ap21cf.trial.applicationstudio.cloud.sap")
        );
        assert_ne!(credential_id("a.b"), credential_id("a-b"));
    }

    #[test]
    fn flattens_a_workspace_record() {
        let record = serde_json::json!({
            "config": {
                "id": "ws-1",
                "labels": { "ws-manager.devx.sap.com/displayname": "My Space" },
                "annotations": { "pack": "FULL_STACK", "packTagline": "Full Stack" }
            },
            "runtime": {
                "status": "RUNNING",
                "baseUrl": "https://ws-1.example",
                "url": { "startup": "https://ws-1.example/startup" }
            }
        });
        let space = flatten_dev_space(&record);
        assert_eq!(space.id, "ws-1");
        assert_eq!(space.label, "My Space");
        assert_eq!(space.status, "RUNNING");
        assert_eq!(space.pack_display_name, "Full Stack");
        assert_eq!(space.ssh_enabled, None);
    }

    #[test]
    fn derives_key_url() {
        let space = DevSpace {
            startup_url: "https://ws-1.example/startup?token=1".into(),
            ..DevSpace::default()
        };
        assert_eq!(key_url(&space).unwrap(), "https://ws-1.example/key");
        assert!(key_url(&DevSpace::default()).is_err());
    }
}
