//! **Pointer cids** — cids that name something to fetch rather than hash it.
//!
//! A pointer's multihash is `identity`: the cid *contains* the address (an
//! indexer host + release id, a provider source + file id), not a digest of the
//! bytes. Bitswap cannot answer one, because there is no hash to verify against.
//! It is resolved instead: by a manifest this peer already stored (the record's
//! pointer field names its content cid), or by a **redeem** at a gateway whose
//! feeder holds the credential for `key`.
//!
//! This module is the single place that knows which codecs are pointers, so the
//! hull ("resolve it first") and the ipfs plugin ("here is how") agree.
//!
//! | codec | what it points at | redeem key | record field |
//! |---|---|---|---|
//! | `0x1005` nzb-release | a `.nzb` at an indexer | indexer host | `manifest` |
//! | `0x1003` nzb-posting | a self-scanned `.nzb` | — (record only) | `manifest` |
//! | `0x100A` provider-file | a file a provider holds | provider source | `file` |

use anyhow::{Context, Result};

use super::cid::{parse_record_cid, read_uvarint};
use super::nzb::{
    decode_nzb_release_cid, normalize_indexer_host, NZB_POSTING_CODEC, NZB_RELEASE_CODEC,
};

/// MetaMesh-private multicodec for a provider-held file (`docs/cid-formats.md` §8).
pub const PROVIDER_FILE_CODEC: u64 = 0x100A;

/// Redeem-claim codec names, as gateways advertise them in
/// `GET /api/gateway/plugins` (`redeems[].codec`).
pub const REDEEM_NZB_RELEASE: &str = "nzb-release";
pub const REDEEM_PROVIDER_FILE: &str = "provider-file";

/// Record fields a redeem merges onto the pointer's record: the content cid of
/// what it resolved to.
pub const FIELD_MANIFEST: &str = "manifest";
pub const FIELD_FILE: &str = "file";

/// Identity multihash code: the digest bytes are the payload verbatim.
const IDENTITY_MULTIHASH: u64 = 0x00;

/// A decoded pointer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pointer {
    /// The redeem-claim codec, or `None` when no gateway can redeem it (a
    /// self-scanned posting resolves from the record alone).
    pub redeem: Option<RedeemKey>,
    /// The record field naming the resolved content cid.
    pub field: &'static str,
    /// What the resolved bytes are: a manifest a byte plugin consumes, or the
    /// file itself.
    pub kind: PointerKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedeemKey {
    /// [`REDEEM_NZB_RELEASE`] or [`REDEEM_PROVIDER_FILE`].
    pub codec: &'static str,
    /// The indexer host / provider source a gateway's claim must cover.
    pub key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerKind {
    /// A `.nzb`: handed to the nzb plugin, which fetches the bytes.
    NzbManifest,
    /// The file itself (a subtitle): served as is.
    File,
}

/// Decode `cid` as a pointer.
///
/// - `None` — not a pointer: an ordinary content cid, or unparseable.
/// - `Some(Err(_))` — the codec says pointer but the payload is malformed; the
///   caller answers `400`.
pub fn decode(cid: &str) -> Option<Result<Pointer>> {
    match codec_of(cid)? {
        NZB_RELEASE_CODEC => Some(decode_nzb_release(cid)),
        NZB_POSTING_CODEC => Some(Ok(Pointer {
            redeem: None,
            field: FIELD_MANIFEST,
            kind: PointerKind::NzbManifest,
        })),
        PROVIDER_FILE_CODEC => Some(decode_provider_file(cid).map(|(source, _id)| Pointer {
            redeem: Some(RedeemKey { codec: REDEEM_PROVIDER_FILE, key: source }),
            field: FIELD_FILE,
            kind: PointerKind::File,
        })),
        _ => None,
    }
}

/// Is `cid` a pointer (well-formed or not)?
pub fn is_pointer(cid: &str) -> bool {
    decode(cid).is_some()
}

fn decode_nzb_release(cid: &str) -> Result<Pointer> {
    let mscid = parse_record_cid(cid)?;
    let locator = decode_nzb_release_cid(&mscid)?;
    Ok(Pointer {
        redeem: Some(RedeemKey {
            codec: REDEEM_NZB_RELEASE,
            key: normalize_indexer_host(locator.authority()),
        }),
        field: FIELD_MANIFEST,
        kind: PointerKind::NzbManifest,
    })
}

/// Decode a `provider-file` cid into `(source, id)`. Errors on a different
/// codec or a malformed payload. Works on the raw multibase string because a
/// long provider id can exceed the 64-byte multihash a typed cid parse allows.
pub fn decode_provider_file(cid: &str) -> Result<(String, String)> {
    let bytes = raw_cid_bytes(cid).context("provider-file: not a base32 CIDv1")?;
    let mut at = 0usize;
    let next = |at: &mut usize, what: &str| -> Result<u64> {
        let (v, n) = read_uvarint(&bytes[*at..])
            .with_context(|| format!("provider-file: {what} varint"))?;
        *at += n;
        Ok(v)
    };
    anyhow::ensure!(next(&mut at, "version")? == 1, "provider-file: not a CIDv1");
    anyhow::ensure!(next(&mut at, "codec")? == PROVIDER_FILE_CODEC, "not a provider-file cid");
    let mh = next(&mut at, "multihash-code")?;
    anyhow::ensure!(
        mh == IDENTITY_MULTIHASH,
        "provider-file locator must use the identity multihash (0x00), got 0x{mh:x}"
    );
    let len = next(&mut at, "digest-length")?;
    anyhow::ensure!(
        (bytes.len() - at) as u64 == len,
        "provider-file digest length {len} != {} trailing byte(s)",
        bytes.len() - at
    );
    let source_len = next(&mut at, "source-length")?;
    let rest = &bytes[at..];
    let source_len = usize::try_from(source_len)
        .ok()
        .filter(|n| *n <= rest.len())
        .context("provider-file: source length overruns the digest")?;
    let (source, id) = rest.split_at(source_len);
    let source = std::str::from_utf8(source).context("provider-file source is not utf-8")?;
    let id = std::str::from_utf8(id).context("provider-file id is not utf-8")?;
    anyhow::ensure!(
        !source.is_empty() && !id.is_empty(),
        "provider-file locator needs both a source and an id"
    );
    Ok((source.to_string(), id.to_string()))
}

/// The codec of a base32 CIDv1 string, without the 64-byte multihash limit.
fn codec_of(cid: &str) -> Option<u64> {
    let bytes = raw_cid_bytes(cid)?;
    let (version, n) = read_uvarint(&bytes).ok()?;
    if version != 1 {
        return None;
    }
    read_uvarint(&bytes[n..]).ok().map(|(codec, _)| codec)
}

/// Decode a `b`-prefixed lowercase base32 (RFC 4648, no padding) CID string.
fn raw_cid_bytes(cid: &str) -> Option<Vec<u8>> {
    let body = cid.strip_prefix('b')?;
    let mut out = Vec::with_capacity(body.len() * 5 / 8);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in body.bytes() {
        let v = match c {
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        } as u32;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Ragna Crimson release from watch.nsl.sh (2026-10-04).
    const NZBGEEK: &str = "bagcsaabfcrqxa2jonz5gez3fmvvs42lomzxs6ylqngxuch22nhfhl4v4vmyhiegah2ka";

    fn b32(bytes: &[u8]) -> String {
        const A: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
        let (mut acc, mut bits, mut s) = (0u32, 0u32, String::from("b"));
        for &b in bytes {
            acc = (acc << 8) | b as u32;
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                s.push(A[((acc >> bits) & 31) as usize] as char);
            }
            acc &= (1 << bits) - 1;
        }
        if bits > 0 {
            s.push(A[((acc << (5 - bits)) & 31) as usize] as char);
        }
        s
    }

    fn provider_file(source: &str, id: &str) -> String {
        let mut digest = vec![source.len() as u8];
        digest.extend_from_slice(source.as_bytes());
        digest.extend_from_slice(id.as_bytes());
        // version 1, codec 0x100A (varint 0x8a 0x20), identity, length
        let mut wire = vec![0x01, 0x8a, 0x20, 0x00, digest.len() as u8];
        wire.extend_from_slice(&digest);
        b32(&wire)
    }

    #[test]
    fn an_nzb_release_is_a_pointer_keyed_by_its_indexer_host() {
        let p = decode(NZBGEEK).unwrap().unwrap();
        assert_eq!(
            p.redeem,
            Some(RedeemKey { codec: REDEEM_NZB_RELEASE, key: "api.nzbgeek.info".into() })
        );
        assert_eq!(p.field, FIELD_MANIFEST);
        assert_eq!(p.kind, PointerKind::NzbManifest);
    }

    #[test]
    fn a_provider_file_is_a_pointer_keyed_by_its_source() {
        let cid = provider_file("opensubtitles", "file:7061834");
        assert_eq!(
            decode_provider_file(&cid).unwrap(),
            ("opensubtitles".into(), "file:7061834".into())
        );
        let p = decode(&cid).unwrap().unwrap();
        assert_eq!(p.redeem.unwrap().key, "opensubtitles");
        assert_eq!(p.field, FIELD_FILE);
        assert_eq!(p.kind, PointerKind::File);
    }

    #[test]
    fn a_content_cid_is_not_a_pointer() {
        assert!(!is_pointer("bafkreigh2akiscaildcqabsyg3dfr6chu3fgpregiymsck7e7aqa4s52zy"));
        assert!(!is_pointer("not a cid"));
    }
}
