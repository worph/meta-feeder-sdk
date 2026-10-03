//! Content sniffing — what the bytes *are*, independent of what they are called.
//!
//! Every classifier in the usenet plugin's `archive` keys off the filename quoted in an NZB
//! subject. A fully obfuscated posting has no filename to key off, so all of them
//! fail **open**: `subject_is_par2` misses the parity volumes, `subject_is_rar`
//! misses the archive, and `pick_media`'s `best_video.or(best_any)` then elects
//! the largest arbitrary blob as "the movie". That blob is what gets served, and
//! — far worse — what the Usenet → IPFS re-seed hashes and publishes as
//! the record's content identity.
//!
//! Names are advisory. Bytes are not. This module is the byte half.
//!
//! The resolution order mirrors `filename-tool`'s `FileTypeConfigurable`
//! (the source of truth named by `METADATA_KEYS.md` §`fileType`): sniff the
//! content first, compare with the extension when there is one, and on
//! **disagreement return `undefined` rather than guessing** — a wrong `fileType`
//! routes a file to a handler that cannot open it.

/// A container format recognised from its leading bytes.
///
/// Deliberately *not* exhaustive over "every format that exists" — it only needs
/// to separate payload from wrapper from parity well enough to decide whether a
/// posting has resolved. [`Magic::Unknown`] is a first-class answer, and callers
/// must treat it as "cannot prove anything", never as "bad".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Magic {
    /// RAR 1.5–4.x — signature `52 61 72 21 1A 07 00`.
    Rar4,
    /// RAR 5.0+ — signature `52 61 72 21 1A 07 01 00`.
    Rar5,
    /// par2 recovery data. **Never payload**, whatever its size.
    Par2,
    SevenZip,
    /// A zip that is not an EPUB (includes `.cbz`, which `METADATA_KEYS.md`
    /// §`fileType` classifies as `archive`).
    Zip,
    /// A zip whose first entry is the stored `mimetype` = `application/epub+zip`.
    /// Split from [`Magic::Zip`] because an EPUB **is** the document, not a
    /// wrapper around one — three correct ebook postings on watch.nsl.sh are
    /// this shape and a naive "zip ⇒ archive" rule condemns all of them.
    Epub,
    /// Palm database holding an ebook (`BOOK`/`TEXt` type at offset 60) — the
    /// `CR!…`-named MobiPocket/eReader files an indexer hands back for a book.
    PalmDoc,
    Matroska,
    Mp4,
    /// RIFF container (AVI, and also WAV — the extension disambiguates).
    Riff,
    Pdf,
    /// Nothing recognised. Not an error: see the type-level note above.
    Unknown,
}

impl Magic {
    /// The `METADATA_KEYS.md` §`fileType` value these bytes imply, or `None`
    /// when the content says nothing.
    ///
    /// [`Magic::Riff`] is deliberately `None`: RIFF is AVI *and* WAV, i.e. a
    /// video and an audio type, and guessing between them is exactly the
    /// "disagreement returns undefined" case the registry legislates.
    pub fn file_type(self) -> Option<&'static str> {
        match self {
            Magic::Matroska | Magic::Mp4 => Some("video"),
            Magic::Epub | Magic::PalmDoc | Magic::Pdf => Some("document"),
            Magic::Rar4 | Magic::Rar5 | Magic::SevenZip | Magic::Zip => Some("archive"),
            // Parity is not a fileType — it is not payload at all. Callers route
            // on `is_parity()` before they ever ask for a type.
            Magic::Par2 => None,
            Magic::Riff | Magic::Unknown => None,
        }
    }

    /// The extension these bytes imply, for naming a materialised file whose
    /// posted name carried none (or carried an obfuscated one) — see
    /// the usenet plugin's `naming`.
    ///
    /// Abstains wherever the bytes do not pick a single extension, on the same
    /// discipline as [`Magic::file_type`]: [`Magic::Riff`] is AVI *and* WAV, and
    /// [`Magic::PalmDoc`] is `.prc`/`.mobi`/`.pdb`. [`Magic::Par2`] abstains for
    /// a stronger reason — parity is never the payload, so it never gets named.
    pub fn extension(self) -> Option<&'static str> {
        match self {
            Magic::Matroska => Some("mkv"),
            Magic::Mp4 => Some("mp4"),
            Magic::Pdf => Some("pdf"),
            Magic::Epub => Some("epub"),
            Magic::Rar4 | Magic::Rar5 => Some("rar"),
            Magic::SevenZip => Some("7z"),
            Magic::Zip => Some("zip"),
            Magic::Riff | Magic::PalmDoc | Magic::Par2 | Magic::Unknown => None,
        }
    }

    /// par2 recovery data: present to repair the payload, never to *be* it.
    ///
    /// One posting on watch.nsl.sh (`…gk2iwj5i…`, claiming `video`/`movie`)
    /// materialised to a single 122 MB par2 file and published a dag-pb root
    /// over it, because `subject_is_par2` reads the `.par2` extension the
    /// obfuscated subject did not carry.
    pub fn is_parity(self) -> bool {
        matches!(self, Magic::Par2)
    }

    /// A multi-volume wrapper that must be unwrapped before anything inside it
    /// can be addressed. `Epub`/`PalmDoc` are containers in the file-format
    /// sense but **not** in this one — they are the document.
    pub fn is_archive(self) -> bool {
        matches!(self, Magic::Rar4 | Magic::Rar5 | Magic::SevenZip | Magic::Zip)
    }
}

/// Bytes needed for a confident answer. The EPUB probe is the deepest reader:
/// local header (30) + `"mimetype"` (8) + extra field + `"application/epub+zip"`
/// (20). 128 covers any sane extra-field length.
pub const SNIFF_LEN: usize = 128;

/// Classify a file from its leading bytes. Short input is fine — every probe
/// bounds-checks and falls through to [`Magic::Unknown`].
pub fn sniff(head: &[u8]) -> Magic {
    // RAR5 before RAR4: the RAR4 signature is a prefix of neither, but the
    // shared first 6 bytes make ordering worth being explicit about.
    if head.starts_with(b"Rar!\x1a\x07\x01\x00") {
        return Magic::Rar5;
    }
    if head.starts_with(b"Rar!\x1a\x07\x00") {
        return Magic::Rar4;
    }
    if head.starts_with(b"PAR2\x00PKT") {
        return Magic::Par2;
    }
    if head.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return Magic::SevenZip;
    }
    if head.starts_with(b"%PDF-") {
        return Magic::Pdf;
    }
    // EBML — Matroska and WebM share it; both demux the same way here.
    if head.starts_with(b"\x1a\x45\xdf\xa3") {
        return Magic::Matroska;
    }
    if head.len() >= 8 && &head[4..8] == b"ftyp" {
        return Magic::Mp4;
    }
    if head.starts_with(b"RIFF") {
        return Magic::Riff;
    }
    if head.starts_with(b"PK\x03\x04") {
        return if is_epub(head) { Magic::Epub } else { Magic::Zip };
    }
    if is_palm_ebook(head) {
        return Magic::PalmDoc;
    }
    Magic::Unknown
}

/// An EPUB is a zip whose **first** entry is an uncompressed `mimetype` member
/// holding exactly `application/epub+zip` (OCF 3, §4.2 — the rule exists so the
/// type is readable without inflating anything). That is what separates a book
/// from a `.cbz` or a scene `.zip`.
fn is_epub(head: &[u8]) -> bool {
    const NAME_AT: usize = 30;
    const NAME: &[u8] = b"mimetype";
    const VALUE: &[u8] = b"application/epub+zip";

    // Stored (method 0) is mandatory for this member; anything else is not an
    // OCF container even if it happens to hold the string.
    if head.len() < NAME_AT + NAME.len() || u16::from_le_bytes([head[8], head[9]]) != 0 {
        return false;
    }
    if &head[NAME_AT..NAME_AT + NAME.len()] != NAME {
        return false;
    }
    let extra = u16::from_le_bytes([head[28], head[29]]) as usize;
    let at = NAME_AT + NAME.len() + extra;
    head.len() >= at + VALUE.len() && &head[at..at + VALUE.len()] == VALUE
}

/// A Palm database (`name[32]`, then `type` at 60 and `creator` at 64) whose
/// type marks it an ebook. Covers MobiPocket (`BOOK`/`MOBI`), eReader
/// (`BOOK`/`PNRd`) and PalmDOC (`TEXt`/`REAd`).
fn is_palm_ebook(head: &[u8]) -> bool {
    const TYPE_AT: usize = 60;
    head.len() >= TYPE_AT + 4 && matches!(&head[TYPE_AT..TYPE_AT + 4], b"BOOK" | b"TEXt")
}

/// The `fileType` an extension implies, for the agreement check. Mirrors the
/// table in `METADATA_KEYS.md` §`fileType`, whose own source of truth is
/// `filename-tool`'s `ExtentionsMapping.ts`. Only the values reachable from a
/// Usenet posting are listed; anything else answers `None` ("no opinion"),
/// which agrees with everything.
pub fn file_type_from_extension(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "mkv" | "mp4" | "webm" | "avi" | "mov" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg" | "3gp"
        | "ts" | "m2ts" | "mts" | "vob" | "ogm" | "ogv" | "divx" | "xvid" => "video",
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "oga" | "opus" | "m4a" | "m4b" | "wma" | "mka" => {
            "audio"
        }
        "pdf" | "epub" | "mobi" | "azw" | "azw3" | "djvu" | "txt" | "doc" | "docx" | "prc" => {
            "document"
        }
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "cbz" | "cbr" => "archive",
        "srt" | "sub" | "ass" | "ssa" | "vtt" | "sbv" | "smi" => "subtitle",
        "par2" => return None, // parity, not a fileType — see `Magic::is_parity`
        _ => return None,
    })
}

/// Resolve one file's `fileType` from its bytes and its name together.
///
/// The registry's rule, verbatim: *"sniff the MIME from the first bytes; if it
/// maps to a definite value, compare it with the extension's value — agreement
/// wins, **disagreement returns `undefined`**"*. `None` here is that
/// `undefined`: it means "these two sources disagree, do not route on it",
/// which is a different and strictly weaker statement than "wrong".
pub fn resolve_file_type(head: &[u8], name: &str) -> Option<&'static str> {
    let sniffed = sniff(head).file_type();
    let from_name = file_type_from_extension(name);
    match (sniffed, from_name) {
        (Some(a), Some(b)) if a == b => Some(a),
        (Some(_), Some(_)) => None, // disagreement ⇒ undefined
        (Some(a), None) => Some(a), // no usable extension: content decides
        (None, Some(b)) => Some(b), // unrecognised bytes: the name is all we have
        (None, None) => None,
    }
}

#[cfg(test)]
#[path = "magic_tests.rs"]
mod tests;
