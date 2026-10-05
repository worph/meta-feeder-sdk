//! Wire types of the transport-plugin contract (hull ⇄ plugin, JSON over HTTP).
//!
//! Only what is common to every plugin lives here. A plugin's own extra routes
//! (ipfs `/share`, torrent `/commits`, …) carry their DTOs in the per-protocol
//! modules next to this one, so the hull and the plugin still compile against
//! one definition.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped on any breaking change to the common routes below. The hull refuses
/// a plugin whose `/manifest` reports a contract outside
/// [`MIN_CONTRACT`]`..=`[`CONTRACT_VERSION`].
///
/// - **2**: a plugin that consumes a manifest (nzb) no longer pulls it from the
///   hull. It answers `/raw` (and its verify route) with `428` +
///   [`HDR_NEEDS_MANIFEST`] until the hull has pushed one with
///   `PUT /manifests/:cid`.
pub const CONTRACT_VERSION: u32 = 2;
/// The oldest contract the hull still drives (contract-1 nzb plugins pull their
/// manifest from `/internal/nzb/manifest/:cid`, kept for one release).
pub const MIN_CONTRACT: u32 = 1;

/// `428` marker: this plugin needs the cid's manifest pushed before it can
/// answer. The hull resolves the pointer, `PUT`s `/manifests/:cid`, and retries.
pub const HDR_NEEDS_MANIFEST: &str = "x-metamesh-needs-manifest";

/// Why a pointer could not be resolved, on a non-2xx `/ipfs-tier/resolve`
/// answer: [`RESOLVE_NO_GATEWAY`], [`RESOLVE_UNCLAIMED`], [`RESOLVE_QUOTA`],
/// [`RESOLVE_NOT_FOUND`] or [`RESOLVE_UPSTREAM`].
pub const HDR_RESOLVE_ERROR: &str = "x-metamesh-resolve-error";
/// No gateway is known at all yet (discovery is eventual): retry.
pub const RESOLVE_NO_GATEWAY: &str = "no-gateway";
/// Gateways are known, but none claims this pointer's key.
pub const RESOLVE_UNCLAIMED: &str = "unclaimed";
/// The provider's quota is spent: retry after `Retry-After`.
pub const RESOLVE_QUOTA: &str = "quota";
/// Every claiming gateway answered not found (or nothing is stored and the
/// pointer cannot be redeemed).
pub const RESOLVE_NOT_FOUND: &str = "not-found";
/// A gateway failed, or answered bytes that did not verify.
pub const RESOLVE_UPSTREAM: &str = "upstream";

/// `X-MetaMesh-Lane`: the hull's lane decision for a `/raw` request.
pub const HDR_LANE: &str = "x-metamesh-lane";
/// `X-MetaMesh-Player`: forwarded verbatim from the client — marks a genuine
/// play (commits, provisional focus).
pub const HDR_PLAYER: &str = "x-metamesh-player";
/// `X-MetaMesh-Magnet`: the magnet the hull resolved for a torrent cid.
pub const HDR_MAGNET: &str = "x-metamesh-magnet";
/// Seed facts a plugin attaches to a successful `/raw` response, so the hull
/// records the seed row *before* it relays the body (the monolith wrote the
/// row inside the fetch, ahead of the first byte). Internal: the hull strips
/// every `x-metamesh-seed-*` header before relaying.
pub const HDR_SEED_NAME: &str = "x-metamesh-seed-name";
pub const HDR_SEED_SIZE: &str = "x-metamesh-seed-size";
/// Prefix of the seed headers the hull strips from a relayed response.
pub const INTERNAL_HEADER_PREFIX: &str = "x-metamesh-seed-";
/// `x-metamesh-meter: egress` on a `/raw` response: the hull floors this body
/// against the playback focus when it relays it. Set exactly where the
/// monolith metered (`focus::gated_stream`) — torrent ranges, Usenet file
/// reads, IPFS dag ranges — and *not* on single-block IPFS answers (posters),
/// which the monolith never throttled. Internal; stripped by the hull.
pub const HDR_METER: &str = "x-metamesh-meter";
pub const METER_EGRESS: &str = "egress";

/// Encode free text (a file name) for an internal header value: every byte
/// outside a conservative visible-ASCII set becomes `%XX`. Inverse:
/// [`header_text_decode`].
pub fn header_text_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b" ._-()[]+,".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Inverse of [`header_text_encode`]. Invalid escapes pass through literally.
pub fn header_text_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `GET /manifest`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// Protocol id: `torrent` | `nzb` | `ipfs` (a competing implementation of a
    /// protocol reports the same id — that's what makes it a drop-in).
    pub id: String,
    /// Implementation name + version, for the dashboard.
    pub implementation: String,
    pub version: String,
    pub contract: u32,
    pub capabilities: Capabilities,
    /// Serves a config plane (`/config`, `/config/schema`, `/config/values`).
    /// Set by the harness from [`TransportPlugin::config`](super::plugin::TransportPlugin::config);
    /// absent in older manifests.
    #[serde(default)]
    pub config: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Can fetch bytes for a cid (`/raw`).
    pub fetch: bool,
    /// Serves what it fetched to other peers (seeds).
    pub share: bool,
}

/// `GET /health`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Health {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

impl Health {
    pub fn ok() -> Self {
        Self { ok: true, detail: None }
    }
}

/// One unit of bytes a plugin holds — the eviction unit as the plugin knows it
/// (a torrent, an NZB job). `GET /jobs`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Job {
    /// Stable unit key (a torrent's infohash hex, an nzb job's cid).
    pub id: String,
    /// The cids this unit answers for, as far as the plugin can name them.
    #[serde(default)]
    pub cids: Vec<String>,
    /// Bytes on disk right now.
    pub bytes_on_disk: u64,
    /// Every selected byte is present.
    pub complete: bool,
    /// Downloading in the background on its own (exempt from eviction).
    pub filling: bool,
    /// Plugin-defined state label (`live`, `paused`, `streaming`, …).
    #[serde(default)]
    pub state: String,
    /// Plugin-specific detail the hull renders opaquely (live transfer stats).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub extra: Value,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Jobs {
    pub jobs: Vec<Job>,
}

/// `DELETE /jobs/:cid` query: the *other* cids of the same unit the hull still
/// wants (sibling files of one torrent). The plugin keeps those.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DeleteQuery {
    /// Comma-separated cids.
    #[serde(default)]
    pub keep: String,
}

impl DeleteQuery {
    pub fn keep_list(&self) -> Vec<String> {
        self.keep
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// `DELETE /jobs/:cid` → the bytes actually unlinked, which is what the hull's
/// eviction budget credits.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Deleted {
    pub bytes_freed: u64,
}

/// `POST /reconcile`: delete every unit no wanted cid names, and any on-disk
/// leftover no unit owns older than `grace_secs`.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ReconcileRequest {
    pub wanted: Vec<String>,
    pub grace_secs: u64,
    /// Only meaningful for plugins whose state lives in a lazily-started
    /// session (torrent): bring it up even if nothing asked for bytes yet,
    /// because persisted units are on disk and only the session can say whose.
    #[serde(default)]
    pub start_session: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Units deleted because nothing wanted them.
    pub units_deleted: u64,
    pub unit_bytes_freed: u64,
    /// On-disk leftovers removed.
    pub orphans_deleted: u64,
    pub orphan_bytes_freed: u64,
    /// The plugin couldn't tell what it owns (session down): nothing deleted.
    #[serde(default)]
    pub skipped: bool,
}

/// A file inside a promoted cache container.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PromotedFile {
    /// Path relative to the container directory.
    pub rel: String,
    /// Absolute path on the shared `/data` volume.
    pub path: String,
    pub size: u64,
}

/// Plugin → hull notifications, `POST {hull}/internal/events` (see
/// [`super::hull::HullClient`]). The hull acknowledges once it has acted, so a
/// plugin that must not proceed before the hull indexed something can wait.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The plugin now holds bytes under `cid`: the hull upserts a seed row.
    Seed {
        cid: String,
        /// `torrent` | `ipfs` (the hull's `SeedKind`).
        kind: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        size_bytes: u64,
        /// The container the bytes live in, when there is one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        container: Option<String>,
    },
    /// A completed fetch was published as a cache container (`rename(2)` into
    /// `cache/`). The hull indexes it in the material index.
    Promoted {
        container: String,
        cids: Vec<String>,
        files: Vec<PromotedFile>,
    },
    /// The content behind `cid` turned out to be an archive: the hull marks
    /// the meta-core record `fileType=archive`.
    Archive { cid: String },
    /// Bytes behind these cids are gone: the hull drops their seed rows.
    Vanished { cids: Vec<String> },
    /// What the plugin knows about `cid` changed (a new availability verdict):
    /// the hull drops its cached probe grade.
    ProbeStale { cid: String },
    /// A fetch finished and its file is complete at `path` (still under
    /// `tmp/`). The hull indexes it for local-first reads and decides what to
    /// do next (the Usenet → IPFS re-seed).
    Ready {
        cid: String,
        path: String,
        size: u64,
    },
    /// Any event type this hull doesn't know yet. Ignored.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_text_round_trips_unicode_and_reserved() {
        for s in ["Plain Name.mkv", "Été 100% \"quoted\"/x.mkv", "", "%", "a%2"] {
            assert_eq!(header_text_decode(&header_text_encode(s)), s);
        }
        assert!(header_text_encode("é/").is_ascii());
    }

    #[test]
    fn delete_query_splits_and_trims() {
        let q = DeleteQuery { keep: " a, ,b,".into() };
        assert_eq!(q.keep_list(), vec!["a".to_string(), "b".to_string()]);
        assert!(DeleteQuery::default().keep_list().is_empty());
    }

    #[test]
    fn events_are_tagged_and_unknown_tolerated() {
        let e = Event::Archive { cid: "b".into() };
        let j = serde_json::to_string(&e).unwrap();
        assert_eq!(j, r#"{"type":"archive","cid":"b"}"#);
        let u: Event = serde_json::from_str(r#"{"type":"from_the_future","x":1}"#).unwrap();
        assert_eq!(u, Event::Unknown);
    }
}
