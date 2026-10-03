//! BitTorrent-specific half of the contract: the torrent plugin's extra routes,
//! and the magnet helpers both sides use.
//!
//! Extra routes (all under `/torrent/`):
//!
//! - `GET  /torrent/inventory`         → [`Inventory`] — every managed torrent,
//!   with have-bytes, per-file progress and live stats. One call per eviction
//!   pass / dashboard poll instead of one per row.
//! - `GET  /torrent/status/:cid`       → `Option<`[`LiveStats`]`>` — peeks only,
//!   never starts the session (meta-watch polls it while connecting).
//! - `GET  /torrent/peers/:cid?budget_ms=&cap=` → [`PeerCount`] — query-only
//!   mainline-DHT population for the availability probe.
//! - `POST /torrent/confirm`           ← [`ConfirmRequest`] → [`ConfirmResponse`]
//!   — metadata-only reachability probe (no torrent kept, no seed row).
//! - `PUT  /torrent/commits`           ← [`Commits`] — the watch-commit intents.
//!   The hull owns the intent (`seeds.json`); the plugin's supervisor owns the
//!   fill (disk floor, concurrency cap, stall hysteresis).

use serde::{Deserialize, Serialize};

/// Live transfer stats for one torrent. Serialises exactly like meta-share's
/// former `bt::SeedLiveStats`, which `/api/file/:cid/status` and `/api/seeds`
/// still emit.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LiveStats {
    /// librqbit torrent state: `live` | `paused` | `initializing` | `error`.
    pub state: String,
    pub down_bps: u64,
    pub up_bps: u64,
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub ratio: f64,
    pub peers_live: usize,
    pub peers_seen: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct FileProgress {
    pub index: usize,
    /// File length from the torrent metadata.
    pub len: u64,
    /// Bytes of it on disk.
    pub have: u64,
}

impl FileProgress {
    pub fn complete(&self) -> bool {
        self.have >= self.len
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct TorrentInfo {
    /// Lowercase hex v1 infohash.
    pub infohash: String,
    #[serde(default)]
    pub name: Option<String>,
    /// librqbit have-bytes — the eviction size of the unit.
    pub have_bytes: u64,
    pub streaming_only: bool,
    /// Metadata resolved (`files` is empty until it is).
    pub metadata: bool,
    #[serde(default)]
    pub files: Vec<FileProgress>,
    pub stats: LiveStats,
}

impl TorrentInfo {
    /// Is file `index` fully downloaded? `None` when unknowable yet.
    pub fn file_complete(&self, index: usize) -> Option<bool> {
        self.files.iter().find(|f| f.index == index).map(FileProgress::complete)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Inventory {
    /// Is the librqbit session up? When it isn't, `torrents` is empty and the
    /// hull falls back to row lengths (the session is lazy on purpose).
    pub session_up: bool,
    /// Does the session directory hold persisted torrents?
    pub persisted: bool,
    pub torrents: Vec<TorrentInfo>,
}

impl Inventory {
    pub fn get(&self, infohash_hex: &str) -> Option<&TorrentInfo> {
        self.torrents.iter().find(|t| t.infohash == infohash_hex)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerCount {
    /// Distinct DHT peers seen; `None` when the DHT couldn't be queried.
    pub peers: Option<usize>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ConfirmRequest {
    pub magnet: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct ConfirmResponse {
    /// Length of file 0.
    pub len: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitIntent {
    pub cid: String,
    /// The seed row's `added_at` — the supervisor's recency order.
    pub added_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Commits {
    pub intents: Vec<CommitIntent>,
}

// ---------------------------------------------------------------------------
// Magnet helpers (moved verbatim from meta-share `bt.rs`)
// ---------------------------------------------------------------------------

/// A bare-infohash magnet: DHT resolves peers from the infohash alone.
pub fn build_magnet(infohash20: &[u8; 20]) -> String {
    format!("magnet:?xt=urn:btih:{}", super::cid::to_hex(infohash20))
}

/// High-uptime open trackers appended to every magnet before librqbit sees it,
/// so a trackerless indexer magnet doesn't leave peer discovery to DHT alone.
pub const DEFAULT_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
    "udp://exodus.desync.com:6969/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://open.demonii.com:1337/announce",
    "http://nyaa.tracker.wf:7777/announce",
];

fn encode_tr(url: &str) -> String {
    let mut out = String::with_capacity(url.len() * 3);
    for b in url.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Append [`DEFAULT_TRACKERS`] to `magnet`, skipping any already present. A
/// non-`magnet:` string is returned unchanged.
pub fn with_default_trackers(mut magnet: String) -> String {
    if !magnet.starts_with("magnet:") {
        return magnet;
    }
    for tr in DEFAULT_TRACKERS {
        let enc = encode_tr(tr);
        if magnet.contains(&enc) || magnet.contains(tr) {
            continue;
        }
        magnet.push_str("&tr=");
        magnet.push_str(&enc);
    }
    magnet
}

fn decode_tr(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
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

/// The decoded, de-duplicated `tr=` announce URLs of a magnet.
pub fn parse_trackers(magnet: &str) -> Vec<String> {
    if !magnet.starts_with("magnet:") {
        return Vec::new();
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let query = magnet.splitn(2, '?').nth(1).unwrap_or("");
    for pair in query.split('&') {
        if let Some(val) = pair.strip_prefix("tr=") {
            let url = decode_tr(val);
            if !url.is_empty() && seen.insert(url.clone()) {
                out.push(url);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_magnet_includes_infohash() {
        assert_eq!(
            build_magnet(&[0xab; 20]),
            "magnet:?xt=urn:btih:abababababababababababababababababababab"
        );
    }

    #[test]
    fn with_default_trackers_appends_to_trackerless_magnet() {
        let bare = "magnet:?xt=urn:btih:abababababababababababababababababababab&dn=Foo".to_string();
        let out = with_default_trackers(bare);
        assert!(out.contains("&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce"));
        assert!(out.contains("&tr=http%3A%2F%2Fnyaa.tracker.wf%3A7777%2Fannounce"));
        assert!(out.contains("&dn=Foo"));
    }

    #[test]
    fn with_default_trackers_dedups_existing() {
        let pre = "magnet:?xt=urn:btih:abababababababababababababababababababab\
                   &tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce"
            .to_string();
        let out = with_default_trackers(pre);
        assert_eq!(out.matches("tracker.opentrackr.org%3A1337").count(), 1);
    }

    #[test]
    fn with_default_trackers_leaves_non_magnet_untouched() {
        assert_eq!(with_default_trackers("not-a-magnet".to_string()), "not-a-magnet");
    }

    #[test]
    fn parse_trackers_round_trips_the_defaults() {
        let m = with_default_trackers(build_magnet(&[1; 20]));
        assert_eq!(parse_trackers(&m), DEFAULT_TRACKERS.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(parse_trackers("not-a-magnet").is_empty());
    }

    #[test]
    fn live_stats_serialise_with_the_legacy_field_names() {
        let v = serde_json::to_value(LiveStats::default()).unwrap();
        for k in [
            "state", "down_bps", "up_bps", "uploaded_bytes", "downloaded_bytes",
            "total_bytes", "ratio", "peers_live", "peers_seen",
        ] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
    }
}
