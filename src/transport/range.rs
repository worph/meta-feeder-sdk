//! HTTP `Range:` request-header parser.
//!
//! Pure helper: takes an inbound `Range:` header value plus the total
//! resource size in bytes, returns a [`RangeOutcome`] describing how the
//! caller should respond.
//!
//! Shared by the meta-share hull and every transport plugin so the
//! `<video>` element can seek and resume downloads identically whichever
//! process serves the bytes. Moved verbatim from meta-share
//! `crates/meta-share/src/api/range.rs`.
//!
//! ## Supported syntax
//!
//! RFC 7233 §3.1 `bytes=`-prefixed single-range only. Multi-range
//! requests (`bytes=0-99,200-299`) are surfaced as
//! [`RangeOutcome::Unsupported`] so the caller can fall back to `200 OK`
//! with the full body — the RFC permits ignoring multi-range, and
//! every common client (browser video element, curl, wget) accepts the
//! degraded response.
//!
//! ### Returned shape
//!
//! - `bytes=N-M`   → `Ok((N, min(M, total-1)))`
//! - `bytes=N-`    → `Ok((N, total-1))`
//! - `bytes=-M`    → `Ok((max(0, total-M), total-1))` (suffix)
//! - `bytes=N-`    with `N >= total` → `Unsatisfiable`
//! - `bytes=N-M`   with `N > M`      → `Unsatisfiable`
//! - missing / blank / malformed     → `Unsatisfiable` for non-`Absent`,
//!   `Absent` only for the literal "no header" case (callers pass
//!   `Option<&HeaderValue>`).
//!
//! The function is total: it never panics, never allocates, and always
//! returns one of the four variants.

use axum::http::HeaderValue;

/// Outcome of parsing an inbound `Range:` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeOutcome {
    /// Header absent on the request — caller serves `200 OK` with the
    /// full body.
    Absent,
    /// Single-range request parsed cleanly. Inclusive `[start, end]`
    /// clamped to `[0, total-1]`.
    Ok { start: u64, end: u64 },
    /// Multi-range or unsupported syntax — caller serves `200 OK` with
    /// the full body (the RFC permits ignoring; browser video and curl
    /// both fall through to a full GET on a follow-up request).
    Unsupported,
    /// Malformed range, reversed range, or `start >= total` — caller
    /// serves `416 Range Not Satisfiable` with
    /// `Content-Range: bytes */{total}`.
    Unsatisfiable,
}

/// Parse a `Range:` header into a [`RangeOutcome`]. `header` is the raw
/// header value (or `None` if the request didn't carry one). `total` is
/// the resource's full size in bytes — used to clamp open / suffix
/// ranges and to detect out-of-bounds requests.
pub fn parse_range_header(header: Option<&HeaderValue>, total: u64) -> RangeOutcome {
    let Some(value) = header else {
        return RangeOutcome::Absent;
    };
    let Ok(s) = value.to_str() else {
        return RangeOutcome::Unsatisfiable;
    };
    let s = s.trim();
    // Strip the `bytes=` unit prefix. Anything else is unsupported
    // (per RFC the unit is extensible, but in practice `bytes` is the
    // only one any browser sends).
    let Some(rest) = s.strip_prefix("bytes=") else {
        return RangeOutcome::Unsatisfiable;
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return RangeOutcome::Unsatisfiable;
    }
    // Multi-range: caller serves 200 with the full body.
    if rest.contains(',') {
        return RangeOutcome::Unsupported;
    }
    // Must contain exactly one '-' separator.
    let (lhs, rhs) = match rest.split_once('-') {
        Some(parts) => parts,
        None => return RangeOutcome::Unsatisfiable,
    };
    let lhs = lhs.trim();
    let rhs = rhs.trim();
    // Three valid shapes:
    //   - "N-M"  bounded range
    //   - "N-"   open-ended (start known, end = total-1)
    //   - "-M"   suffix (last M bytes)
    if lhs.is_empty() && rhs.is_empty() {
        return RangeOutcome::Unsatisfiable;
    }
    // Edge case: file is empty. Any range against a 0-byte resource is
    // unsatisfiable (RFC 7233 §4.4).
    if total == 0 {
        return RangeOutcome::Unsatisfiable;
    }
    if lhs.is_empty() {
        // Suffix range. `rhs` is the suffix length.
        let Ok(suffix_len) = rhs.parse::<u64>() else {
            return RangeOutcome::Unsatisfiable;
        };
        if suffix_len == 0 {
            return RangeOutcome::Unsatisfiable;
        }
        let start = total.saturating_sub(suffix_len);
        return RangeOutcome::Ok {
            start,
            end: total - 1,
        };
    }
    let Ok(start) = lhs.parse::<u64>() else {
        return RangeOutcome::Unsatisfiable;
    };
    if start >= total {
        return RangeOutcome::Unsatisfiable;
    }
    let end = if rhs.is_empty() {
        total - 1
    } else {
        let Ok(parsed_end) = rhs.parse::<u64>() else {
            return RangeOutcome::Unsatisfiable;
        };
        if parsed_end < start {
            return RangeOutcome::Unsatisfiable;
        }
        // Clamp end to total-1 (RFC: "the last byte position is at most
        // total-1").
        parsed_end.min(total - 1)
    };
    RangeOutcome::Ok { start, end }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hv(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).expect("static header value")
    }

    #[test]
    fn absent_when_no_header() {
        assert_eq!(parse_range_header(None, 1000), RangeOutcome::Absent);
    }

    #[test]
    fn closed_range_in_bounds() {
        assert_eq!(
            parse_range_header(Some(&hv("bytes=0-99")), 1000),
            RangeOutcome::Ok { start: 0, end: 99 }
        );
    }

    #[test]
    fn open_end_clamped_to_total_minus_one() {
        assert_eq!(
            parse_range_header(Some(&hv("bytes=100-")), 1000),
            RangeOutcome::Ok { start: 100, end: 999 }
        );
    }

    #[test]
    fn suffix_range() {
        assert_eq!(
            parse_range_header(Some(&hv("bytes=-512")), 1000),
            RangeOutcome::Ok { start: 488, end: 999 }
        );
    }

    #[test]
    fn single_byte_range() {
        assert_eq!(
            parse_range_header(Some(&hv("bytes=0-0")), 1000),
            RangeOutcome::Ok { start: 0, end: 0 }
        );
        assert_eq!(
            parse_range_header(Some(&hv("bytes=999-999")), 1000),
            RangeOutcome::Ok { start: 999, end: 999 }
        );
    }

    #[test]
    fn suffix_clamps_to_whole_file_when_larger_than_total() {
        assert_eq!(
            parse_range_header(Some(&hv("bytes=-99999")), 1000),
            RangeOutcome::Ok { start: 0, end: 999 }
        );
    }

    #[test]
    fn end_clamped_when_above_total() {
        assert_eq!(
            parse_range_header(Some(&hv("bytes=0-1000")), 1000),
            RangeOutcome::Ok { start: 0, end: 999 }
        );
        assert_eq!(
            parse_range_header(Some(&hv("bytes=0-999999")), 1000),
            RangeOutcome::Ok { start: 0, end: 999 }
        );
    }

    #[test]
    fn start_at_or_past_total_is_unsatisfiable() {
        assert_eq!(
            parse_range_header(Some(&hv("bytes=1000-")), 1000),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(
            parse_range_header(Some(&hv("bytes=2000-3000")), 1000),
            RangeOutcome::Unsatisfiable
        );
    }

    #[test]
    fn reversed_range_is_unsatisfiable() {
        assert_eq!(
            parse_range_header(Some(&hv("bytes=10-9")), 1000),
            RangeOutcome::Unsatisfiable
        );
    }

    #[test]
    fn garbage_input_is_unsatisfiable() {
        assert_eq!(
            parse_range_header(Some(&hv("bytes=abc")), 1000),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(
            parse_range_header(Some(&hv("bytes=")), 1000),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(
            parse_range_header(Some(&hv("blocks=0-99")), 1000),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(
            parse_range_header(Some(&hv("0-99")), 1000),
            RangeOutcome::Unsatisfiable
        );
    }

    #[test]
    fn multi_range_is_unsupported() {
        assert_eq!(
            parse_range_header(Some(&hv("bytes=0-50,100-150")), 1000),
            RangeOutcome::Unsupported
        );
        assert_eq!(
            parse_range_header(Some(&hv("bytes=0-50, 100-150")), 1000),
            RangeOutcome::Unsupported
        );
    }

    #[test]
    fn empty_resource_is_always_unsatisfiable() {
        assert_eq!(
            parse_range_header(Some(&hv("bytes=0-99")), 0),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(
            parse_range_header(Some(&hv("bytes=-10")), 0),
            RangeOutcome::Unsatisfiable
        );
    }

    #[test]
    fn zero_length_suffix_is_unsatisfiable() {
        assert_eq!(
            parse_range_header(Some(&hv("bytes=-0")), 1000),
            RangeOutcome::Unsatisfiable
        );
    }
}
