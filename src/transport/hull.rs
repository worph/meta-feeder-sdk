//! The plugin → hull direction: callbacks a plugin makes into meta-share.
//!
//! The hull serves these on a **separate internal listener**
//! (`META_SHARE_INTERNAL_LISTEN`, default `0.0.0.0:3001`) that no deployment
//! publishes: they reach meta-core's records and write to them, so they must
//! never sit behind the public perimeter. A plugin finds it at
//! `META_SHARE_HULL_URL` (default `http://metashare-app:3001`).
//!
//! Routes:
//!
//! - `POST /internal/events`            ← [`Event`] → `204` once the hull acted.
//! - `GET  /internal/records/:cid[?cached=1]` → [`RecordInfo`] (`404` = no
//!   record). `cached=1` answers from the hull's record cache only (no
//!   meta-core round trip) — for display names on a hot path.
//! - `GET  /internal/nzb/manifest/:cid` → the `.nzb` bytes the cid resolves to,
//!   with [`HDR_MANIFEST_SOURCE`] (`record` | `redeem`) and, for `record`,
//!   [`HDR_MANIFEST_CID`]. `404` = not a Usenet cid; any other error carries
//!   `{"error"}` — the manifest could not be redeemed or fetched. Parsing and
//!   the `nzb-posting` digest check stay in the plugin.
//! - `GET  /internal/network` → [`NetworkInfo`]: the hull's own endpoints a
//!   plugin needs to know (the ipfs plugin announces `peer_url` in identify).
//! - `GET  /internal/legacy/usenet` → the NNTP block meta-share's `settings.json`
//!   held before the nzb plugin owned its settings (`404` = none). One-shot
//!   migration source for the nzb plugin's config plane.
//! - `GET  /health` → `200` once the hull has finished its boot steps (the
//!   ipfs plugin waits on it before opening `ipfs/blocks.redb`).
//!
//! Why callbacks and not plugin-side logic: these need what only the hull has —
//! the record cache and meta-core, the gateway redeem, the IPFS tier. Plugins
//! never call each other; the hull routes every such edge.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::dto::Event;

/// Default hull callback base, matching the dev and store compose service name.
pub const DEFAULT_HULL_URL: &str = "http://metashare-app:3001";

/// Where a pointer's bytes came from: `store` (already held here, named by the
/// record's pointer field), `redeem` (a gateway), or `record` (contract-1
/// callback naming for `store`).
pub const HDR_MANIFEST_SOURCE: &str = "x-metamesh-manifest-source";
/// The content cid the pointer resolved to.
pub const HDR_MANIFEST_CID: &str = "x-metamesh-manifest-cid";

/// `.nzb` bytes as the hull resolved them.
#[derive(Clone, Debug)]
pub struct ManifestBytes {
    pub bytes: bytes::Bytes,
    /// `true` when redeemed through a gateway, `false` for the record pointer.
    pub redeemed: bool,
    pub manifest_cid: Option<String>,
}

/// What a plugin may need to know about a record.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordInfo {
    /// meta-core's hash id for the record (write-back key).
    #[serde(default)]
    pub hash_id: Option<String>,
    /// The record's `fileName`.
    #[serde(default)]
    pub file_name: Option<String>,
    /// The record's display title.
    #[serde(default)]
    pub title: Option<String>,
}

/// The hull's endpoints (`GET /internal/network`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkInfo {
    /// meta-core base URL the hull talks to.
    #[serde(default)]
    pub meta_core_url: Option<String>,
    /// The URL other peers reach this meta-share at (announced in identify).
    #[serde(default)]
    pub peer_url: Option<String>,
}

/// Plugin-side client for the hull's internal API. A client with no base URL
/// (`META_SHARE_HULL_URL=` empty, or [`HullClient::disabled`]) answers every call
/// with "not available", which is what unit tests and a standalone plugin get.
#[derive(Clone, Debug)]
pub struct HullClient {
    base: Option<String>,
    http: reqwest::Client,
}

impl HullClient {
    pub fn new(base: Option<String>, http: reqwest::Client) -> Self {
        let base = base
            .map(|b| b.trim().trim_end_matches('/').to_string())
            .filter(|b| !b.is_empty());
        Self { base, http }
    }

    /// From `META_SHARE_HULL_URL`, defaulting to [`DEFAULT_HULL_URL`]; an
    /// explicitly empty value disables callbacks.
    pub fn from_env(http: reqwest::Client) -> Self {
        let base = match std::env::var("META_SHARE_HULL_URL") {
            Ok(v) => Some(v),
            Err(_) => Some(DEFAULT_HULL_URL.to_string()),
        };
        Self::new(base, http)
    }

    pub fn disabled() -> Self {
        Self::new(None, reqwest::Client::new())
    }

    pub fn is_enabled(&self) -> bool {
        self.base.is_some()
    }

    fn url(&self, path: &str) -> Result<String, String> {
        self.base
            .as_ref()
            .map(|b| format!("{b}{path}"))
            .ok_or_else(|| "hull callbacks disabled (META_SHARE_HULL_URL unset)".to_string())
    }

    /// Notify the hull; returns once it has acted.
    pub async fn emit(&self, ev: &Event) -> Result<(), String> {
        let r = self
            .http
            .post(self.url("/internal/events")?)
            .json(ev)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("hull unreachable: {e}"))?;
        if r.status().is_success() {
            Ok(())
        } else {
            Err(format!("hull refused event: {}", r.status()))
        }
    }

    /// `GET` a JSON resource; `404` → `Ok(None)`.
    pub async fn get_json<T: DeserializeOwned>(&self, path: &str, timeout: Duration) -> Result<Option<T>, String> {
        let r = self
            .http
            .get(self.url(path)?)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("hull unreachable: {e}"))?;
        if r.status().as_u16() == 404 {
            return Ok(None);
        }
        if !r.status().is_success() {
            return Err(error_message(r).await);
        }
        r.json::<T>().await.map(Some).map_err(|e| format!("hull sent malformed JSON: {e}"))
    }

    /// `GET` raw bytes; `404` → `Ok(None)`, any other failure → the hull's
    /// `{"error"}` message.
    pub async fn get_bytes(&self, path: &str, timeout: Duration) -> Result<Option<bytes::Bytes>, String> {
        let r = self
            .http
            .get(self.url(path)?)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("hull unreachable: {e}"))?;
        if r.status().as_u16() == 404 {
            return Ok(None);
        }
        if !r.status().is_success() {
            return Err(error_message(r).await);
        }
        r.bytes().await.map(Some).map_err(|e| format!("hull body: {e}"))
    }

    /// The `.nzb` behind a Usenet cid. `Ok(None)` = not a Usenet cid.
    pub async fn nzb_manifest(&self, cid: &str, timeout: Duration) -> Result<Option<ManifestBytes>, String> {
        let r = self
            .http
            .get(self.url(&format!("/internal/nzb/manifest/{cid}"))?)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("hull unreachable: {e}"))?;
        if r.status().as_u16() == 404 {
            return Ok(None);
        }
        if !r.status().is_success() {
            return Err(error_message(r).await);
        }
        let h = r.headers();
        let redeemed = h
            .get(HDR_MANIFEST_SOURCE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == "redeem");
        let manifest_cid = h
            .get(HDR_MANIFEST_CID)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = r.bytes().await.map_err(|e| format!("hull body: {e}"))?;
        Ok(Some(ManifestBytes { bytes, redeemed, manifest_cid }))
    }

    /// [`RecordInfo`] for `cid` (record cache, else meta-core), or `None` when
    /// the hull has no record.
    pub async fn record(&self, cid: &str) -> Result<Option<RecordInfo>, String> {
        self.get_json(&format!("/internal/records/{cid}"), Duration::from_secs(15)).await
    }

    /// The hull's endpoints.
    pub async fn network(&self) -> Result<Option<NetworkInfo>, String> {
        self.get_json("/internal/network", Duration::from_secs(5)).await
    }

    /// The NNTP block meta-share's own settings held before the nzb plugin had
    /// a config plane; `Ok(None)` = there is none.
    pub async fn legacy_usenet(&self) -> Result<Option<super::nzb::UsenetSettings>, String> {
        self.get_json("/internal/legacy/usenet", Duration::from_secs(5)).await
    }

    /// Wait until the hull's internal listener answers `GET /health` with 2xx,
    /// or `max` elapses. Returns whether it came up.
    pub async fn wait_ready(&self, max: Duration) -> bool {
        let Ok(url) = self.url("/health") else {
            return false;
        };
        let deadline = tokio::time::Instant::now() + max;
        loop {
            if let Ok(r) = self.http.get(&url).timeout(Duration::from_secs(3)).send().await {
                if r.status().is_success() {
                    return true;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// [`RecordInfo`] from the hull's record cache only.
    pub async fn cached_record(&self, cid: &str) -> Result<Option<RecordInfo>, String> {
        self.get_json(&format!("/internal/records/{cid}?cached=1"), Duration::from_secs(5)).await
    }
}

async fn error_message(r: reqwest::Response) -> String {
    let status = r.status();
    r.json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| format!("hull error {status}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_disabled_client_refuses_without_network() {
        let h = HullClient::disabled();
        assert!(!h.is_enabled());
        assert!(h.emit(&Event::Archive { cid: "x".into() }).await.is_err());
        assert!(h.record("x").await.is_err());
    }

    #[test]
    fn blank_base_disables() {
        assert!(!HullClient::new(Some("  ".into()), reqwest::Client::new()).is_enabled());
        assert!(HullClient::new(Some("http://h:3001/".into()), reqwest::Client::new()).is_enabled());
    }
}
