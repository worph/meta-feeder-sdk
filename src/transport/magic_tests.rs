//! Tests for [`super`] — content sniffing.
//!
//! Every fixture below is **real**: the leading bytes of files materialised on
//! watch.nsl.sh, captured 2026-09-16. Synthetic headers would have agreed with
//! whatever this module happened to implement; these are the bytes that actually
//! defeated the name-based classifiers.

use super::*;

/// Decode a hex fixture. Panics on bad input — these are consts, not user data.
fn hx(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "odd-length hex fixture");
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("hex"))
        .collect()
}

/// Volume 1 of 35 of the trigger posting — `[smol] Tensei Oujo … S01E05v2`,
/// `bagcsaaa5brqxa2jonz5geltmnftglnrqztt2n23topk2bsnglvhjmty`. Extension-less
/// name, RAR5, method 0 (store). The EBML header of the contained MKV is
/// visible inside these same 200 bytes, at the volume's data offset.
const RAR5_VOL1: &str = "526172211a0701002409274c1001050c010b0101bac0d7af80808080004ce244be8b0102130b94bfd7af80800004eefaa1818d8000b683021a946591800001635b736d6f6c5d2054656e736569204f756a6f20746f2054656e736169205265696a6f75206e6f204d61686f75204b616b756d6569202d205330314530357632202842442031303830702048455643204f70757329205b39394434344436465d2e6d6b760a0313184c91644af1d2231a45dfa3a34286810142f7810142f2810442f381084282886d61";

/// Volume 2 — a *continuation* volume. Byte-identical signature to volume 1,
/// which is why "does it start with `Rar!`" cannot tell you which volume you
/// hold, and why electing "the largest file" picks one at random.
const RAR5_VOL2: &str = "526172211a0701000d6002471101050c03010b0101bac0d7af808080800030554b778b01021b0b93bfd7af80800004eefaa1818d8000b6830236ead743800001635b736d6f6c5d2054656e736569204f756a6f20746f2054656e736169205265696a6f75206e6f204d61686f75204b616b756d6569202d205330314530357632202842442031303830702048455643204f70757329205b39394434344436465d2e6d6b760a0313184c91644af1d223da5b7aaacbc910a46f8b3a8886bc4bfaff0518a54e4a2eb791";

/// RAR4 head, from `…gkigqufwwvhpkbhijvufrdcshogy` (claims `video`/`movie`).
const RAR4: &str = "526172211a0700197a7311000d00000000000000d9427423916f0069ff3f067a";

/// par2 head, from `…gk2iwj5iy3cyzafghckcab7j2fuy` — the posting that
/// materialised to a *single* par2 file and published a dag-pb root over it.
const PAR2: &str = "5041523200504b54083b1a00000000000226e38aa627ac7ee833f4fc92637a69";

/// A genuine EPUB — `Brandon Sanderson - Mistborn`, `…gkdthedplbm5qwmtedo2ymyct2si`.
/// Stored `mimetype` member first, exactly as OCF requires.
const EPUB: &str = "504b0304140000000000ee523a486f61ab2c1400000014000000080000006d696d65747970656170706c69636174696f6e2f657075622b7a6970504b0304140002080800ee523a4827f0a79ab4000000";

/// A MobiPocket Palm database — the `CR!…`-named ebook from `…gle4fanocf5txcdocnto6dh4au4a`.
/// Type `BOOK` at offset 60, creator `MOBI` at 64.
const PALM: &str = "43522158593841474857463731314e4642394a44314443314d374b4548344d000000000057d30d1e57d3113200000000000000000000000000000000424f4f4b4d4f4249000001df";

/// A Matroska head from a posting that resolved correctly.
const MKV: &str = "1a45dfa3a34286810142f7810142f281";

// ---- the six shapes that mattered -------------------------------------------

#[test]
fn the_real_headers_are_each_recognised() {
    assert_eq!(sniff(&hx(RAR5_VOL1)), Magic::Rar5);
    assert_eq!(sniff(&hx(RAR5_VOL2)), Magic::Rar5);
    assert_eq!(sniff(&hx(RAR4)), Magic::Rar4);
    assert_eq!(sniff(&hx(PAR2)), Magic::Par2);
    assert_eq!(sniff(&hx(EPUB)), Magic::Epub);
    assert_eq!(sniff(&hx(PALM)), Magic::PalmDoc);
    assert_eq!(sniff(&hx(MKV)), Magic::Matroska);
}

/// The regression that started this: three correct ebook postings on the box are
/// zips, and a "zip ⇒ archive" rule condemns all of them. An EPUB is the
/// document, not a wrapper around one.
#[test]
fn an_epub_is_a_document_not_an_archive() {
    let m = sniff(&hx(EPUB));
    assert_eq!(m.file_type(), Some("document"));
    assert!(!m.is_archive(), "an epub must not route into the unwrapper");
}

/// A `.cbz` and a scene `.zip` share the EPUB signature for four bytes and
/// diverge at the first member. `METADATA_KEYS.md` §`fileType` puts `cbz` in
/// `archive`, so plain-zip ⇒ archive is the correct fallback.
#[test]
fn a_zip_without_the_ocf_mimetype_member_is_an_archive() {
    let mut z = hx(EPUB);
    // Corrupt just the mimetype *value*; everything else stays a valid zip.
    let at = 38;
    z[at..at + 3].copy_from_slice(b"XXX");
    assert_eq!(sniff(&z), Magic::Zip);
    assert_eq!(sniff(&z).file_type(), Some("archive"));
}

/// OCF requires the `mimetype` member to be *stored*. A deflated one is not an
/// OCF container even if the bytes happen to spell the string.
#[test]
fn a_deflated_mimetype_member_is_not_an_epub() {
    let mut z = hx(EPUB);
    z[8] = 8; // method = deflate
    assert_eq!(sniff(&z), Magic::Zip);
}

#[test]
fn parity_is_not_a_file_type_at_all() {
    let m = sniff(&hx(PAR2));
    assert!(m.is_parity());
    assert_eq!(m.file_type(), None, "par2 must never present as payload");
}

#[test]
fn every_rar_generation_reads_as_an_archive() {
    for f in [RAR4, RAR5_VOL1, RAR5_VOL2] {
        let m = sniff(&hx(f));
        assert!(m.is_archive());
        assert_eq!(m.file_type(), Some("archive"));
        assert!(!m.is_parity());
    }
}

// ---- Unknown is an answer, not an error -------------------------------------

#[test]
fn unrecognised_and_truncated_input_is_unknown_not_a_panic() {
    assert_eq!(sniff(b""), Magic::Unknown);
    assert_eq!(sniff(b"Rar"), Magic::Unknown);
    assert_eq!(sniff(b"PK\x03"), Magic::Unknown);
    assert_eq!(sniff(&[0u8; 8]), Magic::Unknown);
    assert_eq!(sniff(b"not a container at all"), Magic::Unknown);
    assert_eq!(sniff(&hx(MKV)[..2]), Magic::Unknown);
}

/// The holed posting on the box (`…gkisayoy…`, 837 MB of zeros) sniffs Unknown.
/// It is already refused upstream by `disk::first_hole`; this only pins that
/// Unknown carries no `fileType` claim either way.
#[test]
fn a_hole_claims_nothing() {
    let zeros = vec![0u8; SNIFF_LEN];
    assert_eq!(sniff(&zeros), Magic::Unknown);
    assert_eq!(sniff(&zeros).file_type(), None);
}

/// RIFF is AVI *and* WAV — a video type and an audio type. Guessing between
/// them is precisely the disagreement case the registry legislates away.
#[test]
fn riff_is_recognised_but_declines_to_name_a_type() {
    let m = sniff(b"RIFF\x00\x00\x00\x00AVI LIST");
    assert_eq!(m, Magic::Riff);
    assert_eq!(m.file_type(), None);
}

// ---- name + bytes together ---------------------------------------------------

#[test]
fn content_decides_when_the_name_says_nothing() {
    // The obfuscated case: no extension anywhere.
    assert_eq!(
        resolve_file_type(&hx(RAR5_VOL1), "IuYf3jBKJOukfUesqNCw0LoUc1Gqxxmilo0ICJJMeddjB6"),
        Some("archive")
    );
    assert_eq!(resolve_file_type(&hx(MKV), "56GqgfEUMp7RAqF94AGp"), Some("video"));
}

#[test]
fn agreement_between_name_and_bytes_wins() {
    assert_eq!(resolve_file_type(&hx(MKV), "The.Movie.2019.1080p.mkv"), Some("video"));
    assert_eq!(resolve_file_type(&hx(EPUB), "book.epub"), Some("document"));
}

/// `METADATA_KEYS.md` §`fileType`: *"disagreement returns `undefined`"*. A file
/// named `.mkv` whose bytes are a RAR is not a video and is not reliably an
/// archive either — it is a thing we refuse to route on.
#[test]
fn disagreement_returns_undefined_rather_than_guessing() {
    assert_eq!(resolve_file_type(&hx(RAR5_VOL1), "episode.mkv"), None);
    assert_eq!(resolve_file_type(&hx(MKV), "release.rar"), None);
}

#[test]
fn an_unreadable_header_falls_back_to_the_extension() {
    assert_eq!(resolve_file_type(b"\x00\x01\x02\x03", "movie.mkv"), Some("video"));
    assert_eq!(resolve_file_type(b"", "chapter.cbz"), Some("archive"));
}

#[test]
fn neither_source_has_an_opinion() {
    assert_eq!(resolve_file_type(b"\x00\x01\x02\x03", "blob"), None);
    // par2 is deliberately absent from the extension table: it is parity, and
    // `Magic::is_parity` is how callers route it.
    assert_eq!(file_type_from_extension("recovery.par2"), None);
}

#[test]
fn the_extension_table_matches_the_registry_for_the_shapes_we_see() {
    assert_eq!(file_type_from_extension("a.mkv"), Some("video"));
    assert_eq!(file_type_from_extension("a.MKV"), Some("video"));
    assert_eq!(file_type_from_extension("a.epub"), Some("document"));
    assert_eq!(file_type_from_extension("a.cbz"), Some("archive"));
    assert_eq!(file_type_from_extension("a.rar"), Some("archive"));
    assert_eq!(file_type_from_extension("a.srt"), Some("subtitle"));
    assert_eq!(file_type_from_extension("no-extension-here"), None);
}

/// The MKV inside the trigger posting is *stored*, so its EBML header sits in
/// plaintext a few hundred bytes into volume 1. That is what makes the
/// offset-map unwrap possible at all, and it is worth pinning independently of
/// the RAR parser.
#[test]
fn the_stored_mkv_header_is_visible_inside_volume_one() {
    let v1 = hx(RAR5_VOL1);
    let ebml = v1
        .windows(4)
        .position(|w| w == [0x1a, 0x45, 0xdf, 0xa3])
        .expect("EBML header should be plainly visible inside a stored RAR volume");
    assert_eq!(ebml, 174, "volume 1's data offset, per the RAR5 header chain");
    assert_eq!(sniff(&v1[ebml..]), Magic::Matroska);
}
