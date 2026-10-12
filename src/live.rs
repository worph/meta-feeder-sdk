//! Search-wire-transient fields: facts a feeder sees at query time that are
//! true only *now* — a torrent's seeders, its leechers — and so must never
//! reach the replicated `/file/{cid}` hash (METADATA_KEYS rule #4).
//!
//! They travel under one reserved prefix, [`LIVE_PREFIX`], so every layer that
//! persists a record strips the whole family without knowing each key:
//!
//! ```text
//! live/seeders = "42"          int-string
//! live/leechers = "3"          int-string
//! live/at       = "1791726110" unix seconds the feeder observed them
//! ```
//!
//! A record without them means "unknown", never "zero": a replayed or
//! persisted record has none, and that is the honest answer.
//!
//! The feeder SDK strips them in [`crate::meta_core::build_metadata_body`]
//! (every feeder self-publish); meta-gateway and meta-search strip them on
//! their own write paths (see METADATA_KEYS "Search-wire-transient fields").

use std::collections::BTreeMap;

/// The reserved prefix of the transient family.
pub const LIVE_PREFIX: &str = "live/";

/// Swarm seeders, `int-string`.
pub const LIVE_SEEDERS: &str = "live/seeders";
/// Swarm leechers, `int-string`.
pub const LIVE_LEECHERS: &str = "live/leechers";
/// When the feeder observed the `live/*` values, unix seconds.
pub const LIVE_AT: &str = "live/at";

/// The bare swarm counters some feeders wrote before the `live/*` family
/// existed (tribler). Stripped with it: rule #4 bars them in any spelling.
pub const LEGACY_SWARM_KEYS: [&str; 3] = ["seeders", "leechers", "peers"];

/// Whether a field is search-wire-transient and must not be persisted.
pub fn is_transient(key: &str) -> bool {
    key.starts_with(LIVE_PREFIX) || LEGACY_SWARM_KEYS.contains(&key)
}

/// Remove every transient field. Call on the copy about to be persisted, never
/// on the record still bound for the search wire.
pub fn strip_transient(fields: &mut BTreeMap<String, String>) {
    fields.retain(|k, _| !is_transient(k));
}

/// The `live/at` value for a feeder observing counters right now.
pub fn now_secs() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_family_and_the_legacy_counters_only() {
        let mut f: BTreeMap<String, String> = [
            ("live/seeders", "42"),
            ("live/at", "1"),
            ("seeders", "7"),
            ("peers", "9"),
            ("fileName", "x.mkv"),
            ("livestream", "true"),
            ("source/live", "true"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        strip_transient(&mut f);
        let keys: Vec<&str> = f.keys().map(String::as_str).collect();
        assert_eq!(keys, ["fileName", "livestream", "source/live"]);
    }
}
