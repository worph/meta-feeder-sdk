//! CID parsing and the CID-*shape* knowledge every transport process shares.
//!
//! Not protocol logic — these are pure decoders over the multihash standard,
//! which is the one thing meta-share's hull stays tied to. Moved verbatim from
//! meta-share (`blockstore.rs` for the parse, `bt.rs` for the torrent shapes) so
//! the hull and the plugins classify a cid identically: a drift here would route
//! a torrent cid to the IPFS walker or vice versa.

use std::str::FromStr;

use anyhow::{Context, Result};
use cid::CidGeneric;

/// Multihash byte ceiling. Must match the gateway and meta-search side — it
/// feeds beetswap's `Behaviour<const MAX_MULTIHASH_SIZE, B>` in the ipfs plugin.
pub const MAX_MULTIHASH_SIZE: usize = 64;

/// The CID type every MetaMesh transport speaks.
pub type MsCid = CidGeneric<MAX_MULTIHASH_SIZE>;

/// Parse a `Record.cid` into an [`MsCid`].
///
/// **Strict**: a bare CIDv1 multibase string and nothing else, matching
/// `cid.Decode` in meta-core's `internal/cid/rank.go`. The `<algo>:<cid>` token
/// form and CIDv0 (`Qm…`) are both rejected — see meta-share's
/// `blockstore::parse_record_cid` history for why each one bit.
pub fn parse_record_cid(cid_str: &str) -> Result<MsCid> {
    if cid_str.contains(':') {
        anyhow::bail!(
            "parse record cid `{cid_str}`: the `<algo>:<cid>` token form was removed; \
             pass the bare CIDv1 (the multicodec already names the algorithm)"
        );
    }
    let parsed =
        MsCid::from_str(cid_str).with_context(|| format!("parse record cid `{cid_str}`"))?;
    if parsed.version() != cid::Version::V1 {
        anyhow::bail!(
            "parse record cid `{cid_str}`: only CIDv1 is supported, got {:?}",
            parsed.version()
        );
    }
    Ok(parsed)
}

// ---------------------------------------------------------------------------
// BitTorrent cid shapes
// ---------------------------------------------------------------------------

/// Multihash code for sha1 — a v1 infohash is the sha1 of the info dict.
pub const MH_SHA1: u64 = 0x11;
const BT_V1_INFOHASH_LEN: usize = 20;
const BT_V2_INFOHASH_LEN: usize = 32;
/// `btih-v1-file`: one file inside a v1 torrent; digest `<infohash> ‖ varint(index)`.
pub const BTIH_V1_FILE_CODEC: u64 = 0x1001;
/// `btih-v2-file`: recognised so the error is precise, not yet fetchable.
pub const BTIH_V2_FILE_CODEC: u64 = 0x1002;

/// Is this a whole-torrent v1 infohash cid (sha1 multihash, 20 bytes)? The raw
/// codec `0x55` is shared with IPFS raw leaves, so key off the multihash.
pub fn is_btih(mscid: &MsCid) -> bool {
    let mh = mscid.hash();
    mh.code() == MH_SHA1 && mh.digest().len() == BT_V1_INFOHASH_LEN
}

/// The raw 20-byte v1 infohash of a `btih:` cid. Errors (a `400` upstream) for
/// anything that isn't sha1/20.
pub fn infohash20_from_mscid(mscid: &MsCid) -> Result<[u8; 20]> {
    let mh = mscid.hash();
    anyhow::ensure!(
        mh.code() == MH_SHA1,
        "not a v1 BitTorrent infohash CID (multihash code 0x{:x}, want 0x11/sha1)",
        mh.code()
    );
    <[u8; 20]>::try_from(mh.digest())
        .map_err(|_| anyhow::anyhow!("infohash digest is {} bytes, want 20", mh.digest().len()))
}

/// A decoded per-file torrent cid.
pub struct BtFileRef {
    pub infohash20: [u8; 20],
    pub file_index: usize,
}

/// Is this a per-file torrent cid (`0x1001` / `0x1002`)?
pub fn is_btih_file(mscid: &MsCid) -> bool {
    matches!(mscid.codec(), BTIH_V1_FILE_CODEC | BTIH_V2_FILE_CODEC)
}

/// Decode a `btih-v1-file` cid into `(infohash, file_index)`; v2 is rejected
/// pending swarm support.
pub fn decode_btih_file(mscid: &MsCid) -> Result<BtFileRef> {
    let infohash_len = match mscid.codec() {
        BTIH_V1_FILE_CODEC => BT_V1_INFOHASH_LEN,
        BTIH_V2_FILE_CODEC => {
            let _ = BT_V2_INFOHASH_LEN; // documents the v2 digest shape
            anyhow::bail!("btih-v2-file CIDs are not yet supported by the fetch tier");
        }
        other => anyhow::bail!("not a btih-file CID (codec 0x{other:x})"),
    };
    let digest = mscid.hash().digest();
    anyhow::ensure!(
        digest.len() > infohash_len,
        "btih-file digest is {} bytes, need > {} (infohash + file-index varint)",
        digest.len(),
        infohash_len
    );
    let (ih, idx_bytes) = digest.split_at(infohash_len);
    let infohash20 = <[u8; 20]>::try_from(ih)
        .map_err(|_| anyhow::anyhow!("btih-file infohash slice is not 20 bytes"))?;
    let (file_index, consumed) = read_uvarint(idx_bytes)?;
    anyhow::ensure!(
        consumed == idx_bytes.len(),
        "btih-file digest has {} trailing byte(s) after the file-index varint",
        idx_bytes.len() - consumed
    );
    let file_index = usize::try_from(file_index)
        .map_err(|_| anyhow::anyhow!("btih-file index {file_index} overflows usize"))?;
    Ok(BtFileRef { infohash20, file_index })
}

/// Minimal unsigned-LEB128 reader → `(value, bytes_consumed)`. Rejects truncated
/// and overflowing encodings.
pub fn read_uvarint(bytes: &[u8]) -> Result<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        anyhow::ensure!(shift < 64, "varint overflows u64");
        result |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok((result, i + 1));
        }
        shift += 7;
    }
    anyhow::bail!("varint is truncated (no terminating byte)")
}

/// Is this cid a torrent of either shape?
pub fn is_torrent_cid(mscid: &MsCid) -> bool {
    is_btih(mscid) || is_btih_file(mscid)
}

/// A torrent cid as `(infohash, file_index)`; a whole-torrent cid is file 0.
/// `None` for anything else.
pub fn torrent_ref_of_cid(cid: &str) -> Option<([u8; 20], usize)> {
    let mscid = parse_record_cid(cid).ok()?;
    if let Ok(r) = decode_btih_file(&mscid) {
        return Some((r.infohash20, r.file_index));
    }
    infohash20_from_mscid(&mscid).ok().map(|ih| (ih, 0))
}

/// A torrent cid's v1 infohash (whole-torrent and per-file share it).
pub fn infohash_of_cid(cid: &str) -> Option<[u8; 20]> {
    torrent_ref_of_cid(cid).map(|(ih, _)| ih)
}

/// Lowercase hex.
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Lowercase hex of a v1 infohash — the magnet-cache and seed-unit key.
pub fn infohash20_hex(infohash20: &[u8; 20]) -> String {
    to_hex(infohash20)
}

/// Parse a 40-char hex infohash.
pub fn infohash20_from_hex(s: &str) -> Option<[u8; 20]> {
    if s.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, pair) in s.as_bytes().chunks(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// Best-effort MIME from a filename extension; `application/octet-stream` when
/// unknown so the browser sniffs.
pub fn content_type_from_name(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let ct = match ext.as_str() {
        "mkv" => "video/x-matroska",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "ts" | "mts" | "m2ts" => "video/mp2t",
        "mpg" | "mpeg" => "video/mpeg",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    };
    ct.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base32_lower(input: &[u8]) -> String {
        const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
        let mut out = String::new();
        let mut buffer: u64 = 0;
        let mut bits: u32 = 0;
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

    fn make_cid(codec: u8, mh_code: u8, digest: &[u8]) -> MsCid {
        let mut wire = vec![0x01u8, codec, mh_code, digest.len() as u8];
        wire.extend_from_slice(digest);
        MsCid::from_str(&format!("b{}", base32_lower(&wire))).expect("cid parse")
    }

    fn write_uvarint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                break;
            }
        }
    }

    fn make_cid_varint(codec: u64, mh_code: u64, digest: &[u8]) -> MsCid {
        let mut wire = vec![0x01u8];
        write_uvarint(codec, &mut wire);
        write_uvarint(mh_code, &mut wire);
        write_uvarint(digest.len() as u64, &mut wire);
        wire.extend_from_slice(digest);
        MsCid::from_str(&format!("b{}", base32_lower(&wire))).expect("cid parse")
    }

    fn bt_file_digest(infohash: &[u8], index: u64) -> Vec<u8> {
        let mut d = infohash.to_vec();
        write_uvarint(index, &mut d);
        d
    }

    fn v1_file_cid(infohash: &[u8; 20], index: u64) -> MsCid {
        make_cid_varint(BTIH_V1_FILE_CODEC, BTIH_V1_FILE_CODEC, &bt_file_digest(infohash, index))
    }

    #[test]
    fn btih_cid_is_recognised_and_decodes() {
        let infohash = [7u8; 20];
        let cid = make_cid(0x55, MH_SHA1 as u8, &infohash);
        assert!(is_btih(&cid));
        assert_eq!(infohash20_from_mscid(&cid).unwrap(), infohash);
    }

    #[test]
    fn ipfs_sha256_cid_is_not_btih() {
        let cid = make_cid(0x55, 0x12, &[0u8; 32]);
        assert!(!is_btih(&cid));
        assert!(infohash20_from_mscid(&cid).is_err());
    }

    #[test]
    fn dagpb_cid_is_not_btih() {
        assert!(!is_btih(&make_cid(0x70, 0x12, &[0u8; 32])));
    }

    #[test]
    fn btih_v1_file_cid_is_recognised_and_decodes() {
        let infohash = [7u8; 20];
        let cid = v1_file_cid(&infohash, 4);
        assert!(is_btih_file(&cid));
        assert!(!is_btih(&cid));
        let r = decode_btih_file(&cid).unwrap();
        assert_eq!(r.infohash20, infohash);
        assert_eq!(r.file_index, 4);
    }

    #[test]
    fn btih_v1_file_index_zero_and_multibyte_roundtrip() {
        assert_eq!(decode_btih_file(&v1_file_cid(&[0xab; 20], 0)).unwrap().file_index, 0);
        assert_eq!(decode_btih_file(&v1_file_cid(&[0x11; 20], 200)).unwrap().file_index, 200);
    }

    #[test]
    fn btih_v2_file_cid_recognised_but_rejected() {
        let cid = make_cid_varint(BTIH_V2_FILE_CODEC, BTIH_V2_FILE_CODEC, &bt_file_digest(&[0x22; 32], 1));
        assert!(is_btih_file(&cid));
        assert!(decode_btih_file(&cid).is_err());
    }

    #[test]
    fn whole_torrent_cid_is_index_zero_and_not_a_file_cid() {
        let infohash = [9u8; 20];
        let cid = make_cid(0x55, MH_SHA1 as u8, &infohash);
        assert!(is_btih(&cid));
        assert!(!is_btih_file(&cid));
        assert!(decode_btih_file(&cid).is_err());
        assert_eq!(torrent_ref_of_cid(&cid.to_string()), Some((infohash, 0)));
    }

    #[test]
    fn decode_btih_file_rejects_missing_index_varint() {
        let cid = make_cid_varint(BTIH_V1_FILE_CODEC, BTIH_V1_FILE_CODEC, &[3u8; 20]);
        assert!(decode_btih_file(&cid).is_err());
    }

    #[test]
    fn read_uvarint_rejects_truncated_and_reads_multibyte() {
        assert!(read_uvarint(&[0x80]).is_err());
        assert_eq!(read_uvarint(&[0xAC, 0x02]).unwrap(), (300, 2));
        assert_eq!(read_uvarint(&[0x05, 0xFF]).unwrap(), (5, 1));
    }

    #[test]
    fn torrent_ref_of_a_file_cid_keeps_its_index() {
        let cid = v1_file_cid(&[5u8; 20], 3).to_string();
        assert_eq!(torrent_ref_of_cid(&cid), Some(([5u8; 20], 3)));
        assert_eq!(infohash_of_cid(&cid), Some([5u8; 20]));
        assert_eq!(torrent_ref_of_cid("not a cid"), None);
    }

    #[test]
    fn hex_round_trips() {
        let ih = [0xabu8; 20];
        assert_eq!(infohash20_from_hex(&infohash20_hex(&ih)), Some(ih));
        assert_eq!(infohash20_from_hex("zz"), None);
    }

    #[test]
    fn parse_rejects_token_form_and_cidv0() {
        assert!(parse_record_cid("midhash256:bafy").is_err());
        assert!(parse_record_cid("QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG").is_err());
    }

    #[test]
    fn content_type_guesses_common_media() {
        assert_eq!(content_type_from_name("movie.mkv"), "video/x-matroska");
        assert_eq!(content_type_from_name("clip.MP4"), "video/mp4");
        assert_eq!(content_type_from_name("noext"), "application/octet-stream");
    }
}
