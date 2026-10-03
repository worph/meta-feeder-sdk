//! Usenet cid shapes, shared by the hull and the usenet transport plugin, plus
//! the plugin's extra-route DTOs and its settings.
//!
//! The codec helpers are moved verbatim from meta-share `nzb/mod.rs`; the hull
//! dispatches and grades with them, the plugin decodes with them, so they must
//! be one copy.
//!
//! Extra routes of the usenet plugin (all under `/nzb/`):
//!
//! - `GET    /nzb/status/:cid`      → the `kind:"nzb"` status JSON (never starts a job)
//! - `GET    /nzb/jobs`             → `{"jobs":[…]}` in-flight jobs (`NzbActivity` JSON)
//! - `DELETE /nzb/jobs/:cid`        → `{"cancelled":bool}`
//! - `GET    /nzb/capability`       → [`Capability`]
//! - `GET    /nzb/verdict/:cid`     → `Option<String>` remembered verdict
//! - `POST   /nzb/verify/:cid`      → `Option<String>` verdict, verified if free
//! - `POST   /nzb/test-nntp`        ← [`NntpTest`] → `{"ok","message"}`
//! - `POST   /nzb/restart`          → `204`, then the process exits (settings changed)
//! - `POST   /nzb/seal/:cid/claim`  → [`SealClaim`] / `409` (not `Ready`, or already claimed)
//! - `POST   /nzb/seal/:cid/promote`→ [`SealPrepared`] (publish into `cache/` + the hole/magic gates)
//! - `POST   /nzb/seal/:cid/done`   ← [`SealDone`]
//! - `POST   /nzb/seal/:cid/abort`  → `204`
//!
//! `/raw/:cid` may answer `409` with header [`HDR_REDIRECT_CID`] — "these bytes
//! now live under that cid (an IPFS root) and my copy is gone": the hull
//! re-dispatches to it, exactly where the monolith fell back to the blockstore.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::cid::MsCid;

/// `409` + this header on `/raw`: re-dispatch the request to this cid.
pub const HDR_REDIRECT_CID: &str = "x-metamesh-redirect-cid";

/// `GET /nzb/capability`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Capability {
    /// The plugin holds an NNTP pool (host+user+pass configured).
    pub nntp: bool,
    /// The NNTP provider host (never the account), when credentialed.
    #[serde(default)]
    pub provider: Option<String>,
}

/// `POST /nzb/test-nntp` — a candidate connection, already merged with the
/// stored secret by the hull (which owns `settings.json`).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct NntpTest {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    pub tls: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SealClaim {
    pub path: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SealPrepared {
    /// The published file, under `cache/<cid>/`.
    pub path: String,
    /// Its name inside the container.
    pub rel: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SealDone {
    pub root: String,
    pub size: u64,
    pub path: String,
}

/// Usenet (NZB) retrieval settings — the `usenet` section of the hull's
/// `settings.json`. The plugin reads the file read-only at boot (the hull owns
/// it and restarts the plugin when it changes).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UsenetSettings {
    pub nntp_host: Option<String>,
    pub nntp_port: Option<u16>,
    pub nntp_user: Option<String>,
    /// Secret.
    pub nntp_pass: Option<String>,
    pub nntp_tls: bool,
    pub nntp_connections: u32,
}

impl Default for UsenetSettings {
    fn default() -> Self {
        Self {
            nntp_host: None,
            nntp_port: None,
            nntp_user: None,
            nntp_pass: None,
            nntp_tls: true,
            nntp_connections: 20,
        }
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

impl UsenetSettings {
    /// DEPRECATED seed path: the legacy `META_SHARE_NNTP_*` env vars. Only
    /// consulted when `settings.json` doesn't exist yet.
    pub fn from_env() -> Self {
        let mut s = Self::default();
        if let Some(v) = env_nonempty("META_SHARE_NNTP_HOST") {
            s.nntp_host = Some(v);
        }
        if let Some(v) = env_nonempty("META_SHARE_NNTP_PORT").and_then(|v| v.parse().ok()) {
            s.nntp_port = Some(v);
        }
        if let Some(v) = env_nonempty("META_SHARE_NNTP_USER") {
            s.nntp_user = Some(v);
        }
        if let Some(v) = env_nonempty("META_SHARE_NNTP_PASS") {
            s.nntp_pass = Some(v);
        }
        if let Ok(v) = std::env::var("META_SHARE_NNTP_TLS") {
            s.nntp_tls = !matches!(v.trim(), "0" | "false" | "no");
        }
        if let Some(v) = env_nonempty("META_SHARE_NNTP_CONNECTIONS").and_then(|v| v.parse().ok()) {
            s.nntp_connections = v;
        }
        s.nntp_connections = s.nntp_connections.clamp(1, 100);
        s
    }

    /// The `usenet` section of `<config_dir>/settings.json`, or the legacy env
    /// seed when the file is absent or unparseable.
    pub fn load(config_dir: &std::path::Path) -> Self {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct File {
            usenet: UsenetSettings,
        }
        match std::fs::read(config_dir.join("settings.json")) {
            Ok(bytes) => match serde_json::from_slice::<File>(&bytes) {
                Ok(f) => f.usenet,
                Err(_) => Self::from_env(),
            },
            Err(_) => Self::from_env(),
        }
    }
}

/// MetaMesh-private multicodec for a self-describing Newznab **release** locator
/// (`nzb-release`). Mirrors `meta-feeder-sdk`'s `hash::NZB_RELEASE_CODEC`. The
/// cid's identity multihash embeds `{host, id}` directly, so the feeder emits it
/// with **no `.nzb` download** and the credentialed peer decodes the locator
/// straight from the cid at playback — no KV side-table.
pub const NZB_RELEASE_CODEC: u64 = 0x1005;

/// MetaMesh-private multicodec for a **self-scanned Usenet posting**
/// (`nzb-posting`). Mirrors `meta-feeder-sdk`'s `hash::NZB_POSTING_CODEC`.
///
/// The crucial difference from [`NZB_RELEASE_CODEC`]: this is a **digest over
/// the article Message-ID set**, not a locator. It embeds no indexer host, so
/// **any peer with a plain NNTP provider can redeem it — no indexer credential**.
/// That portability is the entire point of scanning Usenet ourselves.
///
/// Two consequences the code must respect:
/// - It is **not reversible**. There is no `decode_*` for it. The `.nzb`
///   manifest arrives out of band, as an ordinary sha2-256 cid named by the
///   record's `manifest` field (see the hull's manifest resolution).
/// - It is **not bitswap-fetchable under itself**. Bitswap derives a block's
///   cid by hashing the block; this digest is over the id set, so a want for it
///   could never be satisfied. Never offer it to the swarm.
///
/// See `meta-gateway/docs/others/self-hosted-usenet-indexer-study.md` §5.
pub const NZB_POSTING_CODEC: u64 = 0x1003;

/// Does this cid address a Usenet **release locator** (codec `0x1005`) — the
/// self-describing `{host, id}` form that must be redeemed against a specific
/// indexer with a matching credential?
///
/// Narrower than [`is_nzb_cid`] on purpose: only the locator carries a host, so
/// only the locator gates on an indexer key (held by a gateway's feeder). Use this where
/// the *host* matters; use [`is_nzb_cid`] where "should this go to the Usenet
/// transport" matters.
pub fn is_nzb_locator(mscid: &MsCid) -> bool {
    mscid.codec() == NZB_RELEASE_CODEC
}

/// Does this cid address a **self-scanned Usenet posting** (codec `0x1003`)?
pub fn is_nzb_posting(mscid: &MsCid) -> bool {
    mscid.codec() == NZB_POSTING_CODEC
}

/// Does this cid go to the **Usenet transport** at all — either form?
///
/// This is the `/raw` dispatch predicate: both shapes end at
/// the usenet plugin's `/raw`, which resolves a manifest (differently per
/// form — see the hull's manifest resolution), NNTP-fetches the articles, and
/// materialises/streams the media. Everything downstream of manifest
/// resolution is identical for the two.
pub fn is_nzb_cid(mscid: &MsCid) -> bool {
    is_nzb_locator(mscid) || is_nzb_posting(mscid)
}

/// String convenience over [`is_nzb_cid`] — either Usenet form. Used by the
/// playback-focus endpoints, which see the bare CID string. There is no
/// locator-only counterpart: every string-typed call site that used to ask
/// "is this specifically a `0x1005` locator" now needs "is this Usenet at
/// all" — see the six dispatch sites widened alongside `nzb-posting`.
pub fn cid_is_nzb(cid: &str) -> bool {
    super::cid::parse_record_cid(cid)
        .map(|c| is_nzb_cid(&c))
        .unwrap_or(false)
}

/// Re-derive an `nzb-posting` digest from a manifest's Message-IDs.
///
/// ⚠ **MIRRORED LOGIC** — byte-for-byte equivalent to
/// `meta_feeder_sdk::hash::compute_nzb_posting_cid` (the gateway/feeder side
/// mints with it; we verify with it). The two crates cannot share a path-dep,
/// so the normalisation rule is duplicated and must be changed on both sides
/// together. A drift here does not error — it silently makes every self-scanned
/// posting fail verification and become unplayable.
///
/// The rule: trim, strip a surrounding `<…>`, drop empties, dedupe, sort
/// lexicographically, join with `\n`, SHA-256. Case is preserved (a Message-ID
/// local part is case-sensitive per RFC 5322).
pub fn nzb_posting_digest<S: AsRef<str>>(message_ids: &[S]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let mut ids: Vec<&str> = message_ids
        .iter()
        .map(|s| {
            let t = s.as_ref().trim();
            t.strip_prefix('<')
                .and_then(|r| r.strip_suffix('>'))
                .unwrap_or(t)
        })
        .filter(|s| !s.is_empty())
        .collect();
    ids.sort_unstable();
    ids.dedup();

    let mut hasher = Sha256::new();
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            hasher.update(b"\n");
        }
        hasher.update(id.as_bytes());
    }
    hasher.finalize().into()
}

/// A decoded self-describing `nzb-release` locator: the Newznab **API base**,
/// recovered from the cid's identity-multihash digest. The release id stays in
/// the cid — only the feeder that redeems it reads it.
pub struct NzbLocator {
    /// The indexer's Newznab API base, **scheme-less, authority + optional
    /// path** (`api.nzb.life`, `api.nzbgeek.info/api`). Stamped by the feeder
    /// from the indexer aggregator's own configured `baseUrl` — i.e. the URL
    /// that aggregator itself queries, so it is redeemable by construction.
    ///
    /// ⚠ This used to be a bare `host` taken from the release's `<guid>`
    /// **permalink**, which is only the API host by coincidence. nzb.life
    /// serves both from `api.nzb.life` so it worked; nzbgeek's permalinks are on
    /// `nzbgeek.info` while its API is on `api.nzbgeek.info/api`, so every
    /// nzbgeek release was unredeemable — `https://nzbgeek.info/api?t=get`
    /// answers 302→HTML and the grab died in `nzb xml parse`.
    ///
    /// Because it may carry a path it is **not** a matching key: use
    /// [`authority()`](Self::authority) for that.
    pub api_base: String,
}

impl NzbLocator {
    /// The host to match against the gateways' redeem claims — the authority
    /// alone, path stripped.
    ///
    /// The base may carry a path (`api.nzbgeek.info/api`) that a claim never
    /// does: operators configure, and feeders advertise, plain hosts.
    pub fn authority(&self) -> &str {
        match self.api_base.split_once('/') {
            Some((authority, _path)) => authority,
            None => &self.api_base,
        }
    }
}

/// Normalise an indexer host for matching: drop any `http(s)://` scheme, trim a
/// trailing slash and surrounding whitespace, lowercase. A locator's
/// [`NzbLocator::authority`] passes through this before it is compared with the
/// hosts a gateway advertises, so a claim for `https://api.example.com/` and one
/// for `api.example.com` match the same releases.
pub fn normalize_indexer_host(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

/// Decode a self-describing `nzb-release` cid (codec `0x1005`) into its
/// API base. The identity-multihash digest is
/// `varint(base_len) ‖ api_base ‖ id_bytes` — the exact inverse of
/// meta-feeder-sdk's `hash::compute_nzb_release_cid`; `id_bytes` is the
/// hex-decoded release id, which only the redeeming feeder reads. Errors
/// (mapped to 4xx by the caller) on a wrong codec or a malformed digest.
///
/// The wire shape is unchanged from when this field held a bare host — only the
/// *meaning* of those bytes widened to "authority + optional path", so old cids
/// still decode (they just name an unredeemable host; see [`NzbLocator`]).
pub fn decode_nzb_release_cid(mscid: &MsCid) -> Result<NzbLocator> {
    anyhow::ensure!(
        mscid.codec() == NZB_RELEASE_CODEC,
        "not an nzb-release cid (codec 0x{:x}, want 0x{:x})",
        mscid.codec(),
        NZB_RELEASE_CODEC
    );
    let digest = mscid.hash().digest();
    let (base_len, consumed) =
        super::cid::read_uvarint(digest).context("nzb-release digest: base-length varint")?;
    let base_len = usize::try_from(base_len)
        .map_err(|_| anyhow::anyhow!("nzb-release base length {base_len} overflows usize"))?;
    let rest = &digest[consumed..];
    anyhow::ensure!(
        rest.len() > base_len,
        "nzb-release digest truncated: base_len={base_len}, only {} byte(s) left (need base + id)",
        rest.len()
    );
    let base_bytes = &rest[..base_len];
    let api_base = std::str::from_utf8(base_bytes)
        .context("nzb-release api base is not valid utf-8")?
        .to_string();
    Ok(NzbLocator { api_base })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::cid::parse_record_cid;

    #[test]
    fn settings_load_reads_the_usenet_section_and_ignores_the_rest() {
        let d = std::env::temp_dir().join(format!("nzb-settings-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("settings.json"),
            r#"{"network":{"meta_core_url":"x"},"usenet":{"nntp_host":"news.example","nntp_user":"u","nntp_pass":"p","nntp_tls":true,"nntp_connections":7,"indexers":[1]}}"#,
        )
        .unwrap();
        let s = UsenetSettings::load(&d);
        assert_eq!(s.nntp_host.as_deref(), Some("news.example"));
        assert_eq!(s.nntp_connections, 7);
        let _ = std::fs::remove_dir_all(&d);
    }


    /// RFC 4648 base32 lowercase, no padding (multibase `b` body).
    fn base32_lower(input: &[u8]) -> String {
        const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
        let mut out = String::new();
        let (mut buffer, mut bits): (u64, u32) = (0, 0);
        for &b in input {
            buffer = (buffer << 8) | b as u64;
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(ALPHABET[((buffer >> bits) & 0x1F) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(ALPHABET[((buffer << (5 - bits)) & 0x1F) as usize] as char);
        }
        out
    }

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    /// Build a self-describing nzb-release cid exactly as meta-feeder-sdk's
    /// `compute_nzb_release_cid` does: identity multihash over
    /// `varint(host_len) ‖ host ‖ id_bytes`.
    fn make_nzb_cid(host: &str, id_bytes: &[u8]) -> String {
        let mut digest = Vec::new();
        varint(host.len() as u64, &mut digest);
        digest.extend_from_slice(host.as_bytes());
        digest.extend_from_slice(id_bytes);
        let mut wire = vec![0x01u8];
        varint(NZB_RELEASE_CODEC, &mut wire);
        wire.push(0x00); // identity multihash
        varint(digest.len() as u64, &mut wire);
        wire.extend_from_slice(&digest);
        format!("b{}", base32_lower(&wire))
    }

    #[test]
    fn decode_round_trips_the_base() {
        let cid = make_nzb_cid("api.example.com", &[0x48, 0x5c, 0x52, 0xf0]);
        let mscid = parse_record_cid(&cid).expect("parse");
        assert!(is_nzb_locator(&mscid));
        let loc = decode_nzb_release_cid(&mscid).expect("decode");
        assert_eq!(loc.api_base, "api.example.com");
        // No path ⇒ authority is the whole thing.
        assert_eq!(loc.authority(), "api.example.com");
    }

    /// The nzbgeek shape, and the whole reason `authority()` exists: its API
    /// lives at `https://api.nzbgeek.info/api` while a feeder claims the plain
    /// host. The base keeps the path; every match must drop it.
    #[test]
    fn a_base_with_a_path_matches_on_its_authority() {
        let cid = make_nzb_cid("api.nzbgeek.info/api", &[0x19, 0xfe, 0x38, 0xe4]);
        let mscid = parse_record_cid(&cid).expect("parse");
        let loc = decode_nzb_release_cid(&mscid).expect("decode");
        assert_eq!(loc.api_base, "api.nzbgeek.info/api");
        assert_eq!(
            loc.authority(),
            "api.nzbgeek.info",
            "can_redeem / probe / redeem claims all compare on the authority"
        );
    }

    /// A port is part of the authority, not a path — it must survive the split.
    #[test]
    fn a_port_stays_in_the_authority() {
        let cid = make_nzb_cid("indexer.local:5076/api", &[0x01, 0x02]);
        let mscid = parse_record_cid(&cid).expect("parse");
        let loc = decode_nzb_release_cid(&mscid).expect("decode");
        assert_eq!(loc.authority(), "indexer.local:5076");
    }

    #[test]
    fn decode_rejects_wrong_codec() {
        // codec 0x1001 (btih-v1-file), not an nzb-release locator.
        let mut wire = vec![0x01u8];
        varint(0x1001, &mut wire);
        wire.extend_from_slice(&[0x00, 0x02, 0xAB, 0xCD]); // identity mh, 2-byte digest
        let cid = format!("b{}", base32_lower(&wire));
        let mscid = parse_record_cid(&cid).expect("parse");
        assert!(!is_nzb_locator(&mscid));
        assert!(decode_nzb_release_cid(&mscid).is_err());
    }

    #[test]
    fn decode_rejects_truncated_digest() {
        // host_len claims the whole digest, leaving no id bytes.
        let cid = make_nzb_cid("api.example.com", &[]);
        let mscid = parse_record_cid(&cid).expect("parse");
        assert!(decode_nzb_release_cid(&mscid).is_err());
    }

    /// Build an nzb-posting cid exactly as meta-feeder-sdk's
    /// `compute_nzb_posting_cid` does: codec 0x1003 + sha2-256 over the
    /// normalised, sorted, newline-joined Message-ID set.
    fn make_posting_cid(message_ids: &[&str]) -> String {
        let digest = nzb_posting_digest(message_ids);
        let mut wire = vec![0x01u8];
        varint(NZB_POSTING_CODEC, &mut wire);
        wire.push(0x12); // sha2-256
        wire.push(0x20); // 32 bytes
        wire.extend_from_slice(&digest);
        format!("b{}", base32_lower(&wire))
    }

    /// ⚠ THE MIRROR TEST. `nzb_posting_digest` here must agree byte-for-byte
    /// with `meta_feeder_sdk::hash::compute_nzb_posting_cid`'s digest — the
    /// gateway mints with one, we verify with the other, and a drift silently
    /// makes every self-scanned posting unplayable rather than erroring.
    ///
    /// This vector is the same one pinned on the SDK side
    /// (`hash::tests::nzb_posting_cid_wire_shape`): a single id `a@x.com`
    /// digests to plain `sha256("a@x.com")`.
    #[test]
    fn posting_digest_matches_the_sdk_mint() {
        use sha2::{Digest, Sha256};
        let expect: [u8; 32] = Sha256::digest(b"a@x.com").into();
        assert_eq!(nzb_posting_digest(&["a@x.com"]), expect);
    }

    /// The normalisation rule, mirrored from the SDK: order-independent,
    /// bracket/blank/dupe-insensitive, case-SENSITIVE.
    #[test]
    fn posting_digest_normalisation_matches_the_mint() {
        // Order does not matter — this is why we hash a sorted set and never
        // the raw `.nzb` bytes (the retired 0x1004's mistake).
        assert_eq!(
            nzb_posting_digest(&["c@x.com", "a@x.com", "b@x.com"]),
            nzb_posting_digest(&["a@x.com", "b@x.com", "c@x.com"])
        );
        // Angle brackets, whitespace, empties and duplicates all normalise away.
        assert_eq!(
            nzb_posting_digest(&["a@x.com", "b@x.com"]),
            nzb_posting_digest(&[" <a@x.com> ", "b@x.com", "", "  ", "a@x.com"])
        );
        // Case is preserved (RFC 5322: the local part is case-sensitive).
        assert_ne!(
            nzb_posting_digest(&["Abc@x.com"]),
            nzb_posting_digest(&["abc@x.com"])
        );
        // A different article set is a different posting.
        assert_ne!(
            nzb_posting_digest(&["a@x.com", "b@x.com"]),
            nzb_posting_digest(&["a@x.com", "c@x.com"])
        );
    }

    /// Both Usenet forms route to the Usenet transport, but only the `0x1005`
    /// locator is host-bound. Keeping the two predicates distinct is what lets
    /// `can_redeem` skip the credential check for a posting.
    #[test]
    fn posting_and_locator_are_both_nzb_but_only_one_is_a_locator() {
        let posting = parse_record_cid(&make_posting_cid(&["a@x.com"])).expect("parse posting");
        assert!(is_nzb_posting(&posting));
        assert!(is_nzb_cid(&posting), "postings go to the Usenet transport");
        assert!(
            !is_nzb_locator(&posting),
            "a posting names no indexer host, so it must NOT be treated as a locator              (that would send it down the credential-gated grab path)"
        );

        let locator = parse_record_cid(&make_nzb_cid("api.example.com", &[0xab, 0xcd]))
            .expect("parse locator");
        assert!(is_nzb_locator(&locator));
        assert!(is_nzb_cid(&locator));
        assert!(!is_nzb_posting(&locator));
    }

    /// A posting cid must never decode as a release locator — the digest is not
    /// a `{host, id}` payload, and misreading it would produce a garbage host.
    #[test]
    fn posting_cid_does_not_decode_as_a_locator() {
        let posting = parse_record_cid(&make_posting_cid(&["a@x.com", "b@x.com"]))
            .expect("parse posting");
        assert!(decode_nzb_release_cid(&posting).is_err());
    }

    /// The string convenience wrappers agree with the parsed forms, including
    /// on garbage input (which must be `false`, never a panic).
    #[test]
    fn cid_string_helpers_agree() {
        let posting = make_posting_cid(&["a@x.com"]);
        assert!(cid_is_nzb(&posting));

        let locator = make_nzb_cid("api.example.com", &[0x01]);
        assert!(cid_is_nzb(&locator));

        assert!(!cid_is_nzb("not-a-cid"));
    }
}
