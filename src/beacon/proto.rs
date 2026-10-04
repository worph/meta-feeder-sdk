//! Beacon v2 wire types and capability matching.
//!
//! The normative spec is meta-root's `docs/project-architecture/beacon-v2.md`.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const PROTO: &str = "beacon";
pub const VERSION: u8 = 2;
/// Beacon v1's group and port, on purpose — one well-known endpoint for every
/// kind of local resource. v1 and v2 coexist because v2 never sends v1's
/// `discovery` / `announce` types.
pub const DEFAULT_GROUP: &str = "239.255.99.1";
pub const DEFAULT_PORT: u16 = 9099;
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);
/// A node unheard for `interval * this` is dropped (evaluated at read time).
pub const LIVENESS_FACTOR: u32 = 3;
/// Receive buffer. Senders stay under [`MAX_DATAGRAM`].
pub const READ_BUFFER: usize = 8192;
/// Above this a datagram risks IP fragmentation; the sender logs a warning.
pub const MAX_DATAGRAM: usize = 1400;

pub const TYPE_PROBE: &str = "probe";
pub const TYPE_ADVERTISE: &str = "advertise";
pub const TYPE_BYE: &str = "bye";

/// Who is advertising.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeInfo {
    /// What the software is (`meta-sort`, `meta-transport-nzb`).
    pub name: String,
    /// Unique per running copy — the container hostname.
    pub instance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `starting` | `running`. Absent reads as `running`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// One advertised resource.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Resource {
    /// Unique within the node.
    pub id: String,
    pub caps: Vec<String>,
    /// Well-known names: `http`, `ui`, `manifest`, `mcp`. A value starting with
    /// `/` is relative to `http` — see [`Resource::endpoint`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub endpoints: BTreeMap<String, String>,
    /// Opaque revision of the resource's details (e.g. its manifest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// The one consumer instance this resource belongs to. Absent = anyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binds: Option<String>,
    /// Profile-defined, per capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Resource {
    pub fn new(id: impl Into<String>, caps: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            id: id.into(),
            caps: caps.into_iter().map(Into::into).collect(),
            ..Default::default()
        }
    }

    pub fn with_endpoint(mut self, name: impl Into<String>, url: impl Into<String>) -> Self {
        let url = url.into();
        if !url.is_empty() {
            self.endpoints.insert(name.into(), url);
        }
        self
    }

    /// True if any of this resource's caps matches `pattern`.
    pub fn matches(&self, pattern: &str) -> bool {
        self.caps.iter().any(|c| cap_matches(pattern, c))
    }

    /// True if any cap matches any of `patterns`. An empty list matches.
    pub fn matches_any(&self, patterns: &[String]) -> bool {
        patterns.is_empty() || patterns.iter().any(|p| self.matches(p))
    }

    /// `binds` is absent, or names `instance`.
    pub fn usable_by(&self, instance: &str) -> bool {
        self.binds.as_deref().map_or(true, |b| b == instance)
    }

    /// The named endpoint as an absolute URL: a value starting with `/` is
    /// joined onto `endpoints.http`.
    pub fn endpoint(&self, name: &str) -> Option<String> {
        let v = self.endpoints.get(name)?;
        if v.starts_with('/') {
            let base = self.endpoints.get("http")?;
            Some(format!("{}{}", base.trim_end_matches('/'), v))
        } else {
            Some(v.clone())
        }
    }
}

/// Every v2 datagram.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub proto: String,
    pub v: u8,
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<Resource>,
    /// Probe only: informational sender instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Probe only: reply only if a resource matches one of these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub want: Vec<String>,
}

impl Message {
    fn base(msg_type: &str) -> Self {
        Self {
            proto: PROTO.into(),
            v: VERSION,
            msg_type: msg_type.into(),
            node: None,
            resources: Vec::new(),
            from: None,
            want: Vec::new(),
        }
    }

    pub fn probe(from: &str, want: &[String]) -> Self {
        Self {
            from: Some(from.to_string()),
            want: want.to_vec(),
            ..Self::base(TYPE_PROBE)
        }
    }

    pub fn advertise(node: NodeInfo, resources: Vec<Resource>) -> Self {
        Self {
            node: Some(node),
            resources,
            ..Self::base(TYPE_ADVERTISE)
        }
    }

    pub fn bye(node: NodeInfo) -> Self {
        Self {
            node: Some(node),
            ..Self::base(TYPE_BYE)
        }
    }

    /// Decode a datagram, returning `None` for anything that is not a
    /// well-formed beacon v2 message — beacon v1 traffic (no envelope), other
    /// versions, unknown types, or an advertise/bye without a node. The group
    /// is shared, so all of that is silently ignored.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let m: Message = serde_json::from_slice(bytes).ok()?;
        if m.proto != PROTO || m.v != VERSION {
            return None;
        }
        match m.msg_type.as_str() {
            TYPE_PROBE => Some(m),
            TYPE_ADVERTISE | TYPE_BYE => {
                let n = m.node.as_ref()?;
                if n.instance.is_empty() || n.name.is_empty() {
                    return None;
                }
                Some(m)
            }
            _ => None,
        }
    }
}

/// Split `base@contract`. A non-numeric or missing contract is `None`.
fn split_contract(s: &str) -> (&str, Option<&str>) {
    match s.rsplit_once('@') {
        Some((base, c)) => (base, Some(c)),
        None => (s, None),
    }
}

/// Does capability `cap` satisfy `pattern`?
///
/// - `*` matches everything.
/// - `@N` on the pattern must equal the cap's contract; without one the cap's
///   contract is ignored.
/// - `x/*` matches any variant of `x`; otherwise the bases must be equal.
pub fn cap_matches(pattern: &str, cap: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let (pbase, pcontract) = split_contract(pattern);
    let (cbase, ccontract) = split_contract(cap);
    if let Some(pc) = pcontract {
        if ccontract != Some(pc) {
            return false;
        }
    }
    match pbase.strip_suffix("/*") {
        Some(prefix) => cbase
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1),
        None => pbase == cbase,
    }
}

/// A short opaque revision for any serialisable value — what a harness puts in
/// [`Resource::rev`] so consumers re-fetch a manifest only when it changed.
/// Stable for a given build; not cryptographic and not meant to be.
pub fn rev_of<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matches_any_variant_only() {
        assert!(cap_matches("metamesh.transport/*", "metamesh.transport/nzb@1"));
        assert!(cap_matches("metamesh.transport/*", "metamesh.transport/ipfs"));
        assert!(!cap_matches("metamesh.transport/*", "metamesh.transport"));
        assert!(!cap_matches("metamesh.transport/*", "metamesh.transportx/a"));
        assert!(!cap_matches("metamesh.transport/*", "metamesh.transport/"));
        assert!(cap_matches("*", "anything@3"));
    }

    #[test]
    fn contract_rules() {
        assert!(cap_matches("metamesh.transport/nzb", "metamesh.transport/nzb@1"));
        assert!(cap_matches("metamesh.transport/nzb", "metamesh.transport/nzb@2"));
        assert!(cap_matches("metamesh.transport/nzb@2", "metamesh.transport/nzb@2"));
        assert!(!cap_matches("metamesh.transport/nzb@2", "metamesh.transport/nzb@1"));
        assert!(!cap_matches("metamesh.transport/nzb@2", "metamesh.transport/nzb"));
        assert!(cap_matches("metamesh.transport/*@1", "metamesh.transport/nzb@1"));
        assert!(!cap_matches("metamesh.transport/*@1", "metamesh.transport/nzb@2"));
    }

    #[test]
    fn exact_match() {
        assert!(cap_matches("mcp", "mcp"));
        assert!(!cap_matches("mcp", "mcp/x"));
        assert!(!cap_matches("metamesh.core", "metamesh.core2"));
        assert!(!cap_matches("metamesh.transport/nzb", "metamesh.transport/ipfs@1"));
    }

    #[test]
    fn parse_rejects_v1_and_foreign_traffic() {
        // beacon v1 aggregator probe / server announce
        assert!(Message::parse(br#"{"type":"discovery"}"#).is_none());
        assert!(Message::parse(br#"{"type":"announce","name":"x","tools":[]}"#).is_none());
        // meta-discovery v1
        assert!(Message::parse(br#"{"v":1,"type":"announce","name":"meta-sort"}"#).is_none());
        // wrong version / unknown type / not json
        assert!(Message::parse(br#"{"proto":"beacon","v":3,"type":"probe"}"#).is_none());
        assert!(Message::parse(br#"{"proto":"beacon","v":2,"type":"announce"}"#).is_none());
        assert!(Message::parse(b"\x00\x01garbage").is_none());
        // advertise without a node
        assert!(Message::parse(br#"{"proto":"beacon","v":2,"type":"advertise"}"#).is_none());
    }

    #[test]
    fn roundtrip_advertise() {
        let r = Resource::new("nzb", ["metamesh.transport/nzb@1"])
            .with_endpoint("http", "http://metashare-nzb:3000")
            .with_endpoint("manifest", "/manifest");
        let m = Message::advertise(
            NodeInfo { name: "meta-transport-nzb".into(), instance: "metashare-nzb".into(), ..Default::default() },
            vec![r],
        );
        let bytes = serde_json::to_vec(&m).unwrap();
        let back = Message::parse(&bytes).unwrap();
        assert_eq!(back.msg_type, TYPE_ADVERTISE);
        let r = &back.resources[0];
        assert_eq!(r.endpoint("manifest").as_deref(), Some("http://metashare-nzb:3000/manifest"));
        assert!(r.matches("metamesh.transport/*"));
    }

    #[test]
    fn binds() {
        let mut r = Resource::new("nzb", ["metamesh.transport/nzb@1"]);
        assert!(r.usable_by("anyone"));
        r.binds = Some("metashare-app".into());
        assert!(r.usable_by("metashare-app"));
        assert!(!r.usable_by("metawatch-share"));
    }

    #[test]
    fn probe_filter() {
        let r = Resource::new("service", ["metamesh.service/meta-sort"]);
        assert!(r.matches_any(&[]));
        assert!(r.matches_any(&["metamesh.service/*".into()]));
        assert!(!r.matches_any(&["metamesh.core".into()]));
    }
}
