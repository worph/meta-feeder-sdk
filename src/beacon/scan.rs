//! On-demand discovery scan — what a consumer's "Scan" button runs.
//!
//! Probe everyone, wait a moment for the replies, then classify every resource
//! matching the consumer's capability pattern against what the consumer has
//! already configured. The report is the same JSON in every consumer
//! (meta-share, meta-gateway, meta-sort), rendered by the shared
//! `<meta-beacon-scan>` element.

use std::time::{Duration, Instant};

use serde::Serialize;

use super::node::{BeaconNode, SeenResource};

/// How a discovered resource relates to this consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CandidateState {
    /// Its URL is already in the consumer's list.
    Configured,
    /// Can be added.
    Addable,
    /// `binds` names another consumer; shown but not addable.
    BoundElsewhere,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub instance: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub resource_id: String,
    pub caps: Vec<String>,
    /// `endpoints.http` — what the consumer would register.
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binds: Option<String>,
    pub state: CandidateState,
    /// The list entry that already holds this URL (state `configured`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured_as: Option<String>,
    /// A list-safe name to register it under.
    pub suggested_name: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSummary {
    /// Every node heard on the segment, whatever it advertises.
    pub nodes: usize,
    /// Resources matching the consumer's pattern.
    pub capable: usize,
    pub configured: usize,
    pub bound_elsewhere: usize,
    pub addable: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanReport {
    pub cap: String,
    /// This consumer's instance (what `binds` is compared against).
    #[serde(rename = "self")]
    pub self_instance: String,
    pub duration_ms: u64,
    pub summary: ScanSummary,
    pub candidates: Vec<Candidate>,
}

fn norm(u: &str) -> String {
    u.trim().trim_end_matches('/').to_string()
}

/// Lower-case, `[a-z0-9._-]` only — accepted by every consumer's name rules.
pub fn suggested_name(instance: &str) -> String {
    let s: String = instance
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    if s.is_empty() { "plugin".into() } else { s }
}

/// Pure classification, for tests and for callers that already hold a view.
/// `configured` is `(name, url)` of every entry the consumer has.
pub fn classify(
    pattern: &str,
    self_instance: &str,
    nodes: usize,
    resources: Vec<SeenResource>,
    configured: &[(String, String)],
) -> ScanReport {
    let mut summary = ScanSummary { nodes, ..Default::default() };
    let mut candidates = Vec::new();
    for r in resources {
        // A resource without an http endpoint cannot be registered anywhere.
        let Some(url) = r.resource.endpoint("http") else { continue };
        let url = norm(&url);
        summary.capable += 1;
        let configured_as = configured.iter().find(|(_, u)| norm(u) == url).map(|(n, _)| n.clone());
        let state = if configured_as.is_some() {
            summary.configured += 1;
            CandidateState::Configured
        } else if !r.resource.usable_by(self_instance) {
            summary.bound_elsewhere += 1;
            CandidateState::BoundElsewhere
        } else {
            summary.addable += 1;
            CandidateState::Addable
        };
        candidates.push(Candidate {
            suggested_name: suggested_name(&r.node.instance),
            instance: r.node.instance,
            name: r.node.name,
            version: r.node.version,
            resource_id: r.resource.id,
            caps: r.resource.caps,
            url,
            binds: r.resource.binds,
            state,
            configured_as,
        });
    }
    // Addable first (what the operator came for), then configured, then the rest.
    let rank = |s: CandidateState| match s {
        CandidateState::Addable => 0,
        CandidateState::Configured => 1,
        CandidateState::BoundElsewhere => 2,
    };
    candidates.sort_by(|a, b| rank(a.state).cmp(&rank(b.state)).then(a.instance.cmp(&b.instance)));
    ScanReport {
        cap: pattern.to_string(),
        self_instance: self_instance.to_string(),
        duration_ms: 0,
        summary,
        candidates,
    }
}

/// Probe every node, wait `wait` for the replies, then classify what matches
/// `pattern`. A node that stays silent through the wait but is still inside
/// its liveness window is counted too — the view is "who is here", not "who
/// answered this probe".
pub async fn scan(
    node: &BeaconNode,
    pattern: &str,
    configured: &[(String, String)],
    wait: Duration,
) -> ScanReport {
    let started = Instant::now();
    node.probe(&[]).await;
    tokio::time::sleep(wait).await;
    let mut report = classify(
        pattern,
        &node.instance(),
        node.nodes().len(),
        node.resources(pattern),
        configured,
    );
    report.duration_ms = started.elapsed().as_millis() as u64;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beacon::{NodeInfo, Resource};

    fn seen(instance: &str, cap: &str, url: Option<&str>, binds: Option<&str>) -> SeenResource {
        let mut r = Resource::new("x", [cap]);
        if let Some(u) = url {
            r = r.with_endpoint("http", u);
        }
        r.binds = binds.map(Into::into);
        SeenResource {
            node: NodeInfo { name: "plugin".into(), instance: instance.into(), ..Default::default() },
            resource: r,
            addr: "10.0.0.1".into(),
            last_seen: 0.0,
        }
    }

    #[test]
    fn classifies_configured_addable_and_bound_elsewhere() {
        let resources = vec![
            seen("metashare-nzb", "metamesh.transport/nzb@1", Some("http://metashare-nzb:3000/"), Some("metashare-app")),
            seen("qbit", "metamesh.transport/torrent@1", Some("http://qbit:3000"), None),
            seen("other-ipfs", "metamesh.transport/ipfs@1", Some("http://other-ipfs:3000"), Some("metawatch-share")),
            seen("no-endpoint", "metamesh.transport/ipfs@1", None, None),
        ];
        let configured = vec![("nzb".to_string(), "http://metashare-nzb:3000".to_string())];
        let r = classify("metamesh.transport/*", "metashare-app", 10, resources, &configured);
        assert_eq!(r.summary.nodes, 10);
        assert_eq!(r.summary.capable, 3, "a resource without an http endpoint is not a candidate");
        assert_eq!((r.summary.configured, r.summary.addable, r.summary.bound_elsewhere), (1, 1, 1));
        assert_eq!(r.candidates[0].instance, "qbit", "addable rows sort first");
        assert_eq!(r.candidates[1].configured_as.as_deref(), Some("nzb"));
        assert_eq!(r.candidates[2].state, CandidateState::BoundElsewhere);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["self"], "metashare-app");
        assert_eq!(json["summary"]["boundElsewhere"], 1);
        assert_eq!(json["candidates"][2]["state"], "bound-elsewhere");
    }

    #[test]
    fn suggested_names_are_list_safe() {
        assert_eq!(suggested_name("MetaShare NZB"), "metashare-nzb");
        assert_eq!(suggested_name(""), "plugin");
    }

    #[tokio::test]
    async fn scan_reports_what_the_node_heard() {
        use crate::beacon::{BeaconNode, Config, Message};
        let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut cfg = Config::new("consumer").instance("me");
        cfg.port = port;
        let node = BeaconNode::spawn(cfg).unwrap();
        let ad = Message::advertise(
            NodeInfo { name: "meta-feeder-tmdb".into(), instance: "tmdb-feeder".into(), ..Default::default() },
            vec![Resource::new("feeder", ["metamesh.feeder/tmdb"]).with_endpoint("http", "http://tmdb-feeder:8080")],
        );
        let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        s.send_to(&serde_json::to_vec(&ad).unwrap(), ("127.0.0.1", port)).await.unwrap();
        let r = scan(&node, "metamesh.feeder/*", &[], Duration::from_millis(200)).await;
        assert_eq!(r.summary.nodes, 1);
        assert_eq!(r.summary.addable, 1);
        assert_eq!(r.candidates[0].url, "http://tmdb-feeder:8080");
        assert_eq!(r.candidates[0].suggested_name, "tmdb-feeder");
        assert!(r.duration_ms >= 200);
    }
}
