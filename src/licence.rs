//! Licence-URL → SPDX-style identifier.
//!
//! Generic across upstreams: Internet Archive, Jamendo and Wikimedia Commons
//! all publish a Creative Commons *URL*, while `METADATA_KEYS.md` requires an
//! SPDX-style identifier — a consumer comparing licences should not be doing
//! string surgery on `http://` vs `https://` and a trailing slash.

/// `METADATA_KEYS.md` requires an SPDX-style identifier rather than a URL,
/// because a consumer comparing licences should not be doing string surgery on
/// `http://` vs `https://` and a trailing slash.
pub fn licence_from_url(url: &str) -> Option<String> {
    let u = url.trim().trim_end_matches('/').to_ascii_lowercase();
    if u.is_empty() {
        return None;
    }
    if u.contains("publicdomain/zero") {
        return Some("CC0-1.0".to_string());
    }
    if u.contains("publicdomain/mark") {
        return Some("PublicDomain".to_string());
    }
    // .../licenses/<variant>/<version>[/<jurisdiction>]
    let rest = u.split("/licenses/").nth(1)?;
    let mut parts = rest.split('/');
    let variant = parts.next()?.to_ascii_uppercase();
    let version = parts.next().unwrap_or("4.0");
    if variant.is_empty() {
        return None;
    }
    Some(format!("CC-{variant}-{version}"))
}
